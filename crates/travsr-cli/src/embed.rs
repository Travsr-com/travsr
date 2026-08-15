//! `travsr embed` — RFC-018 embedding plugin management.
//!
//! Manages downloadable embed sidecar binaries and their model files.
//! No compile-time features — the binary never links against ort or sqlite-vec.

use anyhow::{bail, Context as _, Result};
use clap::Subcommand;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use travsr_daemon::regenerate_embed_texts_if_stale;
use travsr_plugin_host::{
    embed_backends, lookup_embed_backend, write_model_descriptor, EmbedBackend,
};

use crate::progress::Palette;

const EMBED_RELEASES_BASE: &str = "https://github.com";
const HF_BASE: &str = "https://huggingface.co";

/// WS3 (B3): process-wide flag set by the Ctrl-C handler during a foreground
/// reindex. Read after the sidecar drains so we skip calibration (E3) and report
/// "cancelled" rather than "complete".
static REINDEX_CANCELLED: AtomicBool = AtomicBool::new(false);

/// Turn Ctrl-C into a graceful reindex cancel: write the cancel sentinel — the
/// sidecar drains its in-flight batch, commits, and exits 0 — and record that we
/// cancelled. The `embed.lock` flock and single-flight guard release on the normal
/// return path once the sidecar exits, so no orphan survives (E5). Best-effort and
/// idempotent: a second install in the same process (init then reindex) is ignored.
fn install_reindex_cancel_handler(db_path: &Path) {
    let sentinel = travsr_plugin_host::cancel_sentinel_path(db_path);
    let _ = ctrlc::set_handler(move || {
        eprintln!("\n^C — cancelling reindex (finishing current batch)...");
        let _ = std::fs::write(&sentinel, b"");
        REINDEX_CANCELLED.store(true, Ordering::SeqCst);
    });
}

/// CLI spelling of `travsr_plugin_host::Priority` (keeps clap out of plugin-host).
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum PriorityArg {
    Normal,
    Low,
    Idle,
}

impl From<PriorityArg> for travsr_plugin_host::Priority {
    fn from(p: PriorityArg) -> Self {
        match p {
            PriorityArg::Normal => travsr_plugin_host::Priority::Normal,
            PriorityArg::Low => travsr_plugin_host::Priority::Low,
            PriorityArg::Idle => travsr_plugin_host::Priority::Idle,
        }
    }
}

/// Clap parser for `--capacity <auto|1-100>`. Accepts the adaptive `auto`
/// sentinel (WS5) or a percent, matching the `embed.capacity` config validator.
fn parse_capacity(s: &str) -> std::result::Result<travsr_plugin_host::Capacity, String> {
    travsr_plugin_host::Capacity::parse(s)
        .ok_or_else(|| format!("expected `auto` or a percent 1-100, got '{s}'"))
}

#[derive(Debug, Subcommand)]
pub enum EmbedCommand {
    /// Show available embedding backends and their install status.
    List {
        /// Output as JSON for programmatic / extension use.
        #[arg(long)]
        json: bool,
    },
    /// Download and install an embedding backend.
    ///
    /// Downloads the sidecar binary from GitHub Releases into ~/.travsr/bin/
    /// and the ONNX model files from HuggingFace into ~/.travsr/models/<backend>/.
    /// Activates the backend on success.
    Init {
        /// Backend ID to install (run `travsr embed list` to see options).
        /// Defaults to the first catalog entry (currently bge-small-en-v1.5).
        #[arg(long)]
        backend: Option<String>,
        /// Re-download even if already installed.
        #[arg(long)]
        reinstall: bool,
        /// Worker budget for the post-install reindex: `auto` or a percent 1-100.
        /// When omitted on an interactive terminal, `init` prompts for a CPU
        /// budget; on a non-interactive shell it uses config/env/default.
        #[arg(long, value_name = "AUTO|PCT", value_parser = parse_capacity)]
        capacity: Option<travsr_plugin_host::Capacity>,
        /// Absolute number of parallel embed workers for the post-install reindex.
        #[arg(long, short = 'j', value_name = "N")]
        jobs: Option<usize>,
        /// OS scheduling priority for the post-install reindex sidecar.
        #[arg(long, value_enum)]
        priority: Option<PriorityArg>,
    },
    /// Embed all un-embedded nodes in the current repo's graph.db.
    ///
    /// Invokes the active embed sidecar in --reindex mode.  Skips nodes that
    /// already have an embedding for the active backend; safe to run repeatedly.
    Reindex {
        /// Path to graph.db to reindex (defaults to .travsr/graph.db in the
        /// nearest git root).
        #[arg(long)]
        db: Option<PathBuf>,
        /// Only embed symbol nodes with shell_number >= N (Phase 1 high-centrality pass).
        /// Omit to embed all pending nodes.
        #[arg(long)]
        phase1: Option<u32>,
        /// Worker budget for this run: `auto` (load-adaptive) or a percent 1-100
        /// of the derived worker count. Leaves cores free for interactive work.
        /// Overrides the `embed.capacity` config for this run only.
        #[arg(long, value_name = "AUTO|PCT", value_parser = parse_capacity)]
        capacity: Option<travsr_plugin_host::Capacity>,
        /// Absolute number of parallel embed workers for this run (overrides
        /// --capacity and the derived count).
        #[arg(long, short = 'j', value_name = "N")]
        jobs: Option<usize>,
        /// OS scheduling priority for the embed sidecar this run.
        #[arg(long, value_enum)]
        priority: Option<PriorityArg>,
    },
    /// Change the reindex resource budget of a running or paused reindex and
    /// apply it immediately (WS4).
    ///
    /// Persists the given knob(s) to config (this repo unless `--global`), then
    /// gracefully cancels any in-flight reindex and respawns it with the new
    /// worker count / priority — resuming from partial work (no node is
    /// re-embedded). At least one of `--capacity` / `-j` / `--priority` is
    /// required.
    #[command(
        about = "Change the reindex resource budget and apply it immediately.",
        long_about = "Change the reindex resource budget and apply it immediately.\n\nPersists the given knob(s) to config (this repo unless `--global`), then gracefully cancels any in-flight reindex and respawns it with the new worker count / priority — resuming from partial work (no node is re-embedded). At least one of `--capacity` / `-j` / `--priority` is required."
    )]
    Reconfigure {
        /// Path to graph.db (defaults to .travsr/graph.db in the nearest git root).
        #[arg(long)]
        db: Option<PathBuf>,
        /// New worker budget: `auto` (load-adaptive) or a percent 1-100.
        #[arg(long, value_name = "AUTO|PCT", value_parser = parse_capacity)]
        capacity: Option<travsr_plugin_host::Capacity>,
        /// New absolute worker count (overrides --capacity).
        #[arg(long, short = 'j', value_name = "N")]
        jobs: Option<usize>,
        /// New OS scheduling priority for the embed sidecar.
        #[arg(long, value_enum)]
        priority: Option<PriorityArg>,
        /// Persist to the machine-global config instead of this repo's config.
        #[arg(long)]
        global: bool,
    },
    /// Show the currently active embedding model and binary status.
    Status,
    /// Switch the active embedding backend (binary must already be installed).
    ///
    /// Inside a repo, writes this repo's config (<repo>/.travsr/embed.toml) —
    /// the only layer that affects an indexed repo's retrieval. Use --global
    /// to set the machine-wide default instead.
    Switch {
        /// Backend ID to make active (run `travsr embed list` to see options).
        backend: String,
        /// Write the machine-global default instead of this repo's config.
        #[arg(long)]
        global: bool,
    },
    /// Reclaim disk space held by inactive embedding models: their vectors in
    /// embed.db and their HNSW index files. Dry-run by default.
    ///
    /// A model switch (`embed switch`) leaves the previous model's vectors in
    /// place rather than deleting them automatically — re-embedding costs
    /// hours, and an eager sweep would silently destroy that work the moment
    /// a user tries a different model. Run this explicitly once you no longer
    /// need the old model's coverage.
    Gc {
        /// Actually delete. Without this, only reports what would be reclaimed.
        #[arg(long)]
        apply: bool,
        /// Retain this model's vectors even though it is inactive. Repeatable.
        #[arg(long = "keep", value_name = "MODEL_ID")]
        keep: Vec<String>,
    },
    /// Re-measure the model-relative semantic floors on the existing index.
    ///
    /// Runs the label-free calibration probe (self-match + nonsense cosines) against
    /// the already-built HNSW and rewrites the `embed_cos_*` graph.db meta anchors —
    /// without re-embedding. Runs automatically after every reindex; use this to
    /// recalibrate after a model or corpus change that didn't go through reindex.
    Calibrate {
        /// Path to graph.db to calibrate (defaults to .travsr/graph.db in the
        /// nearest git root).
        #[arg(long)]
        db: Option<PathBuf>,
    },
}

pub fn run(cmd: EmbedCommand) -> Result<()> {
    match cmd {
        EmbedCommand::List { json } => cmd_list(json),
        EmbedCommand::Init {
            backend,
            reinstall,
            capacity,
            jobs,
            priority,
        } => {
            let overrides = travsr_plugin_host::EmbedOverrides {
                capacity,
                max_workers: jobs,
                priority: priority.map(Into::into),
            };
            cmd_init(backend.as_deref(), reinstall, overrides)
        }
        EmbedCommand::Reindex {
            db,
            phase1,
            capacity,
            jobs,
            priority,
        } => {
            let overrides = travsr_plugin_host::EmbedOverrides {
                capacity,
                max_workers: jobs,
                priority: priority.map(Into::into),
            };
            cmd_reindex(db, phase1, overrides)
        }
        EmbedCommand::Reconfigure {
            db,
            capacity,
            jobs,
            priority,
            global,
        } => {
            let overrides = travsr_plugin_host::EmbedOverrides {
                capacity,
                max_workers: jobs,
                priority: priority.map(Into::into),
            };
            cmd_reconfigure(db, overrides, global)
        }
        EmbedCommand::Status => cmd_status(),
        EmbedCommand::Switch { backend, global } => cmd_switch(&backend, global),
        EmbedCommand::Gc { apply, keep } => cmd_gc(apply, keep),
        EmbedCommand::Calibrate { db } => cmd_calibrate(db),
    }
}

/// Resolve a graph.db path from an optional override, else `.travsr/graph.db` in the
/// nearest git root. Errors when the resolved path does not exist.
fn resolve_graph_db(db_override: Option<PathBuf>) -> Result<PathBuf> {
    match db_override {
        Some(p) => Ok(p),
        None => {
            let cwd = std::env::current_dir().context("getting cwd")?;
            // Callers (reindex/reconfigure/gc/calibrate) mutate this db, so
            // resolve the worktree's own index, never the main worktree (#586).
            let repo_root = crate::repo::find_git_root_for_write(&cwd)?;
            // UX-016: build the path component-by-component so it never mixes
            // separators (`...\.travsr/graph.db`) when displayed on Windows.
            let p = repo_root.join(".travsr").join("graph.db");
            anyhow::ensure!(
                p.exists(),
                "graph.db not found at {}\n  Run `travsr init` first.",
                p.display()
            );
            Ok(p)
        }
    }
}

fn cmd_calibrate(db_override: Option<PathBuf>) -> Result<()> {
    let db_path = resolve_graph_db(db_override)?;
    match calibrate_semantic_floors(&db_path)? {
        Some((lo, hi)) => {
            println!("\u{2713} Calibrated semantic floors (cos_lo={lo:.3}, cos_hi={hi:.3}).")
        }
        None => println!(
            "Calibration skipped (no embedding backend, or too few embedded nodes to sample)."
        ),
    }
    Ok(())
}

// ── list ──────────────────────────────────────────────────────────────────────

fn cmd_list(json: bool) -> Result<()> {
    // Same resolution `status` uses (repo config wins over the machine
    // default) so the two surfaces can never name different active models
    // (#482/#483).
    let cwd = std::env::current_dir().ok();
    let repo_root = cwd
        .as_ref()
        .and_then(|c| crate::repo::find_git_root(c).ok());
    let repo_active_id = repo_root.and_then(|r| travsr_plugin_host::repo_backend_id(&r));
    let global_id = load_config().and_then(|c| c.active);
    let active = repo_active_id.or(global_id);

    if json {
        let entries: Vec<String> = embed_backends()
            .iter()
            .map(|b| {
                let installed = model_files_installed(b);
                let is_active = active.as_deref() == Some(b.id.as_str());
                format!(
                    r#"{{"id":"{}","description":"{}","dim":{},"params_m":{},"mteb":{:.1},"ram_mb":{},"download_mb":{},"installed":{},"active":{}}}"#,
                    b.id,
                    b.description,
                    b.output_dim(),
                    b.params_m,
                    b.mteb,
                    b.ram_mb,
                    b.model_files.iter().map(|f| f.size_hint_mb).sum::<u32>(),
                    installed,
                    is_active
                )
            })
            .collect();
        println!("[{}]", entries.join(",\n"));
        return Ok(());
    }

    println!(
        "{:<22} {:<5} {:<7} {:<5} {:<10} {:<8} STATUS",
        "BACKEND", "DIM", "PARAMS", "MTEB", "DOWNLOAD", "RAM"
    );
    println!("{}", "-".repeat(92));
    for b in embed_backends() {
        let installed = model_files_installed(b);
        let is_active = active.as_deref() == Some(b.id.as_str());
        let status = if installed && is_active {
            format!("\u{2713} active  {}", b.description)
        } else if installed {
            format!("installed  {}", b.description)
        } else {
            format!("not installed  {}", b.description)
        };
        let download_mb: u32 = b.model_files.iter().map(|f| f.size_hint_mb).sum();
        let ram_str = if b.ram_mb >= 1000 {
            format!("~{:.1}GB", b.ram_mb as f32 / 1024.0)
        } else {
            format!("~{}MB", b.ram_mb)
        };
        let backend_label = if is_active {
            format!("{} *", b.id)
        } else {
            b.id.to_string()
        };
        println!(
            "{:<22} {:<5} {:<7} {:<5} {:<10} {:<8} {}",
            backend_label,
            b.output_dim(),
            format!("{}M", b.params_m),
            format!("{:.1}", b.mteb),
            format!("{download_mb} MB"),
            ram_str,
            status,
        );
    }
    Ok(())
}

// ── init ──────────────────────────────────────────────────────────────────────

fn cmd_init(
    backend_id: Option<&str>,
    reinstall: bool,
    mut overrides: travsr_plugin_host::EmbedOverrides,
) -> Result<()> {
    let pal = Palette::for_stream(std::io::stdout().is_terminal());

    let backend: &'static EmbedBackend = match backend_id {
        Some(id) => lookup_embed_backend(id).ok_or_else(|| {
            anyhow::anyhow!("Unknown backend '{id}'. Run `travsr embed list` to see options.")
        })?,
        None => {
            use std::io::IsTerminal as _;
            if !std::io::stdin().is_terminal() {
                // Non-interactive: list and exit without selecting.
                println!("Available embedding models (run with --backend <id> to install):\n");
                for b in embed_backends() {
                    let dl_mb: u32 = b.model_files.iter().map(|f| f.size_hint_mb).sum();
                    println!("  {}  ({} MB download)", b.id, dl_mb);
                    println!("  {}\n", b.description);
                }
                return Ok(());
            }
            match pick_backend_interactive()? {
                Some(b) => b,
                None => return Ok(()),
            }
        }
    };

    // WS6 (A3): when the user gave no explicit budget flags and we're on an
    // interactive terminal, offer a CPU-budget choice before the (potentially
    // long) reindex — parity with `travsr init`'s prompts. Headless/CI keeps the
    // config/env/default path with no prompt (H3).
    if overrides.capacity.is_none()
        && overrides.max_workers.is_none()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
    {
        if let Some(cap) = prompt_cpu_budget()? {
            overrides.capacity = Some(cap);
        }
    }

    println!();
    install_backend_with_progress(backend, reinstall)?;

    // Record as globally installed/active (used by `travsr embed list` and hints).
    let mut config = load_config().unwrap_or_default();
    config.active = Some(backend.id.to_string());
    save_config(&config)?;

    // Write per-repo config so the daemon only auto-embeds repos the user
    // explicitly opted into. The repo must already be initialised (graph.db exists).
    // Write path: target the worktree we are in, never the main worktree (#586).
    let repo_root = std::env::current_dir()
        .ok()
        .and_then(|c| crate::repo::find_git_root_for_write(&c).ok());
    let db_path = repo_root
        .as_ref()
        .map(|r| r.join(".travsr/graph.db"))
        .filter(|p| p.exists());

    if let Some(ref root) = repo_root {
        if db_path.is_some() {
            if let Err(e) = travsr_plugin_host::write_repo_backend_id(root, &backend.id) {
                tracing::warn!("could not write repo embed config: {e}");
            }
        }
    }

    // RFC-025 Point B, Leg 1: if the on-disk sidecar is below the version floor,
    // `install_backend_with_progress` already printed the WARN + remedy. The post-
    // install reindex would hard-refuse at Point A, so skip it and leave the user
    // in a usable state (prior embeddings stay searchable) rather than aborting
    // `init`. The hard refuse remains where it belongs: `embed reindex`/spawn.
    let below_floor = embed_bin_dir().ok().is_some_and(|d| {
        let p = d.join(backend.binary_filename());
        p.exists() && !travsr_plugin_host::floor_status(backend, &p, None).is_usable()
    });

    match db_path {
        Some(_) if below_floor => {
            println!(
                "\n  {} skipping reindex: the installed sidecar is below the required version.",
                pal.dim("\u{2139}")
            );
            println!(
                "  {} existing embeddings remain searchable; run `travsr embed init --reinstall` to update and reindex.",
                pal.dim("\u{2139}")
            );
        }
        Some(ref p) => reindex_after_init(backend, p, &overrides)?,
        None => {
            println!("\n  {} {} installed", pal.green("\u{25cf}"), backend.id);
            println!(
                "  {} run `travsr embed init` inside a travsr repo to activate for that repo",
                pal.dim("\u{2139}")
            );
        }
    }

    Ok(())
}

/// Interactive CPU-budget prompt shown at `embed init` (WS6 A3). Returns the
/// chosen [`Capacity`], or `None` to fall through to config/env/default (the
/// "Full" default). Only called on an interactive terminal.
fn prompt_cpu_budget() -> Result<Option<travsr_plugin_host::Capacity>> {
    use std::io::Write as _;
    use travsr_plugin_host::Capacity;

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    let cores_note = if cores > 0 {
        format!(" (detected {cores} cores)")
    } else {
        String::new()
    };

    println!("\n  How much CPU should embedding use?{cores_note}");
    println!("  [1] Full     — all available cores (fastest; default)");
    println!("  [2] Half     — 50% of cores");
    println!("  [3] Quarter  — 25% of cores (leaves the machine responsive)");
    println!("  [4] Auto     — adapt to current system load");
    println!("  [5] Custom   — enter a percent 1-100");
    print!("  Choice? (1-5, Enter for Full): ");
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    match input.trim() {
        "" | "1" => Ok(None), // Full → defer to config/default (100%)
        "2" => Ok(Some(Capacity::Percent(50))),
        "3" => Ok(Some(Capacity::Percent(25))),
        "4" => Ok(Some(Capacity::Auto)),
        "5" => {
            print!("  Percent (1-100): ");
            std::io::stdout().flush()?;
            let mut pct = String::new();
            std::io::stdin().read_line(&mut pct)?;
            match Capacity::parse(pct.trim()) {
                Some(c) => Ok(Some(c)),
                None => {
                    println!("  (not a valid percent — using Full)");
                    Ok(None)
                }
            }
        }
        _ => {
            println!("  (unrecognised — using Full)");
            Ok(None)
        }
    }
}

/// Interactive numbered model selector, matching the `travsr lang detect` style.
fn pick_backend_interactive() -> Result<Option<&'static EmbedBackend>> {
    let active = load_config().and_then(|c| c.active);
    let bin_dir = embed_bin_dir()?;

    println!("  Available embedding models:\n");
    for (i, b) in embed_backends().iter().enumerate() {
        let is_active = active.as_deref() == Some(b.id.as_str());
        let installed = bin_dir.join(b.binary_filename()).exists()
            && embed_model_dir(&b.id)
                .map(|d| b.model_files.iter().all(|f| d.join(&f.name).exists()))
                .unwrap_or(false);
        let tag = if is_active {
            "  (active)"
        } else if installed {
            "  (installed)"
        } else {
            ""
        };
        let dl_mb: u32 = b.model_files.iter().map(|f| f.size_hint_mb).sum();
        let ram = if b.ram_mb >= 1000 {
            format!("~{:.1} GB RAM", b.ram_mb as f32 / 1024.0)
        } else {
            format!("~{} MB RAM", b.ram_mb)
        };
        println!(
            "  [{}] {}{}\n      {} dim · {}M params · MTEB {} · {} MB download · {}\n      {}\n",
            i + 1,
            b.id,
            tag,
            b.output_dim(),
            b.params_m,
            b.mteb,
            dl_mb,
            ram,
            b.description,
        );
    }

    use std::io::Write as _;
    print!("  Which model? (1-{}, q to quit): ", embed_backends().len());
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim();

    if input.eq_ignore_ascii_case("q") || input.is_empty() {
        return Ok(None);
    }

    match input.parse::<usize>() {
        Ok(n) if n >= 1 && n <= embed_backends().len() => Ok(Some(&embed_backends()[n - 1])),
        _ => {
            println!("  invalid selection '{input}'");
            Ok(None)
        }
    }
}

fn install_backend_with_progress(backend: &'static EmbedBackend, reinstall: bool) -> Result<()> {
    let pal = Palette::for_stream(std::io::stdout().is_terminal());
    let bin_dir = embed_bin_dir()?;
    let dest = bin_dir.join(backend.binary_filename());

    // Resolved before the presence check, not inside the download, because
    // presence alone does not mean the RIGHT sidecar is installed. Anyone who has
    // run `embed init` once and now sets TRAVSR_EMBED_ACCEL takes the branch
    // below, and without this they would keep the CPU build, see "ready", and get
    // nothing saying the variable did nothing — the same silent-CPU failure this
    // module refuses on a 404, on an unknown value, and on a missing runtime
    // library, arriving through the one door that was not guarded.
    //
    // Resolving here also means an invalid TRAVSR_EMBED_ACCEL is now rejected on
    // this path too, instead of being ignored because the binary happened to exist.
    let target = crate::install::current_target().context("determining install target")?;
    let variant = resolve_accel_variant(target)?;

    // Only `Some(false)` re-downloads: the sidecar answered and said it has no
    // accelerator. `None` means it could not tell us (pre-handshake build), and
    // treating that as "not accelerated" would re-download on every init.
    //
    // `installed` is checked first so a fresh install does not spawn a binary that
    // is not there — `&&` short-circuits, so the probe only runs when there is
    // something to probe.
    let installed = dest.exists();
    let wants_upgrade =
        installed && variant.is_some() && installed_sidecar_is_accelerated(&dest) == Some(false);

    if installed && !reinstall && !wants_upgrade {
        println!("  {} {} ready", pal.green("\u{25cf}"), backend.binary_name);
        // RFC-025 Point B: the binary is present but presence is monotonic and
        // never re-checks the release it was pinned to on install day. Surface a
        // below-floor WARN (offline) and a newer-release advisory (best-effort)
        // here, so a stale sidecar is caught at init instead of at the next
        // reindex. Never fails init.
        crate::install::advise_installed_sidecar(backend, &dest, "travsr embed init --reinstall");
    } else {
        if wants_upgrade {
            println!(
                "  {} {} is installed without an accelerator; TRAVSR_EMBED_ACCEL={} requests one — reinstalling",
                pal.green("\u{25cf}"),
                backend.binary_name,
                variant.map(|v| v.name).unwrap_or_default()
            );
        }
        let repo = backend.github_repo.to_string();
        // RFC-025 G3: resolve the download tag through the same shared resolver
        // the Phase B family uses (embed is never hash-pinned, and `embed init`
        // has no `--version` override), instead of a bespoke inline fetch.
        let version = crate::lang::resolve_install_tag(
            false,
            &backend.version_fallback,
            None,
            &backend.binary_name,
            move || {
                crate::lang::run_async(async move {
                    crate::install::fetch_latest_version_for_repo(&repo).await
                })
            },
        )?;

        let bin_name = backend.binary_name.to_string();
        let repo2 = backend.github_repo.to_string();
        let ver2 = version.clone();
        let tgt = target.to_string();
        let path = crate::lang::run_async(async move {
            download_embed_binary(&repo2, &ver2, &bin_name, &tgt).await
        })
        .context("downloading embed binary")?;

        println!(
            "  {} {} installed  ({})",
            pal.green("\u{25cf}"),
            backend.binary_name,
            path.display()
        );

        if !crate::install::path_contains_travsr_bin() {
            println!("\n{}", crate::install::path_hint());
        }
    }

    // Before the model download, not after: model files run to 1.3 GB, and
    // discovering that the installed sidecar cannot execute this architecture
    // after pulling all of it — or worse, partway through the first background
    // reindex — is the failure travsr-embed #6 set out to remove.
    ensure_sidecar_can_run(&dest, backend)?;

    // Model files — one spinner per file.
    let model_dir = embed_model_dir(&backend.id)?;
    for mf in &backend.model_files {
        let dest = model_dir.join(&mf.name);
        if dest.exists() && !reinstall {
            println!("  {} {} already present", pal.green("\u{25cf}"), mf.name);
            continue;
        }
        let hf_repo = mf.hf_repo.to_string();
        let url_path = mf.url_path.to_string();
        let name = mf.name.to_string();
        let size_mb = mf.size_hint_mb;
        crate::lang::run_async(async move {
            download_model_file_with_progress(&hf_repo, &url_path, &name, &dest, size_mb).await
        })
        .with_context(|| format!("downloading {}", mf.name))?;
    }

    // Bridge to the sidecar: write the per-model descriptor (model.toml) so the
    // sidecar reads dim/pooling/prefix/inputs from config — never hardcoded.
    write_model_descriptor(&model_dir, backend).context("writing model descriptor")?;

    Ok(())
}

/// Fail a model download only when the connection STALLS (no bytes for this
/// long), never for merely being slow. The old total-request timeout (300 s)
/// silently imposed a minimum line rate: a 1.3 GB model.onnx needed a
/// sustained ~4.3 MB/s or the client killed a perfectly healthy transfer at
/// the 300 s mark, every retry, on every connection slower than that.
const MODEL_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Size-scaled ceiling on the WHOLE download (#685 review). The stall detector
/// alone lets a server that trickles one byte every <120 s keep `embed init`
/// alive forever; this cap bounds that. Scaled from the expected size at a
/// worst-case sustained 64 KiB/s (a 1.3 GB model gets ~5.7 h) plus a flat
/// grace, so it only fires on transfers no stall-free connection could
/// plausibly still be making progress on.
const MODEL_TOTAL_CAP_FLOOR: std::time::Duration = std::time::Duration::from_secs(600);
const MODEL_TOTAL_CAP_MIN_RATE: u64 = 64 * 1024; // bytes/s

fn model_total_cap(expected_bytes: u64) -> std::time::Duration {
    MODEL_TOTAL_CAP_FLOOR
        + std::time::Duration::from_secs(expected_bytes / MODEL_TOTAL_CAP_MIN_RATE)
}

async fn download_model_file_with_progress(
    hf_repo: &str,
    url_path: &str,
    file_name: &str,
    dest: &std::path::Path,
    size_hint_mb: u32,
) -> Result<()> {
    let url = format!("{HF_BASE}/{hf_repo}/resolve/main/{url_path}");
    // connect_timeout + per-chunk stall detection below — deliberately NO
    // total-request timeout, so transfer duration scales with file size and
    // line speed instead of being capped by a constant.
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .user_agent(format!("travsr-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    let mut resp = client.get(&url).send().await.context("GET model file")?;
    if !resp.status().is_success() {
        bail!("model file download failed ({}): {url}", resp.status());
    }
    let content_length = resp.content_length();
    let total_mb = content_length
        .map(|n| n / 1_048_576)
        .unwrap_or(size_hint_mb as u64);
    // #685 review: the total cap scales with the expected size, so it never
    // reintroduces the old constant ceiling that killed slow-but-healthy
    // transfers; it exists to end transfers that can no longer finish.
    let total_cap = model_total_cap(content_length.unwrap_or(size_hint_mb as u64 * 1_048_576));
    let total_cutoff = std::time::Instant::now() + total_cap;

    let is_tty = std::io::stderr().is_terminal();
    let name = file_name.to_string();

    // Shared progress counter: the chunk loop writes, the spinner reads, so
    // the user sees MB downloaded instead of a bare elapsed-seconds count.
    let downloaded = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Spinner task — aborted once the body is fully streamed.
    let spinner = if is_tty {
        let progress = std::sync::Arc::clone(&downloaded);
        Some(tokio::spawn(async move {
            use std::io::Write as _;
            const FRAMES: [char; 4] = ['◐', '◓', '◑', '◒'];
            let pal = Palette::for_stream(true);
            let start = std::time::Instant::now();
            let mut i = 0usize;
            loop {
                let spin = pal.orange(&FRAMES[i % 4].to_string());
                let elapsed = start.elapsed().as_secs();
                let done_mb = progress.load(std::sync::atomic::Ordering::Relaxed) / 1_048_576;
                eprint!(
                    "\r  {spin} downloading {name} ({done_mb}/{total_mb} MB) ...  {elapsed}s    "
                );
                let _ = std::io::stderr().flush();
                i += 1;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }))
    } else {
        eprintln!("  downloading {file_name} ({total_mb} MB) ...");
        None
    };

    // Stream the body straight to the tmp file: a 1.3 GB model never sits in
    // RAM (the old `bytes()` buffered the whole body), and each chunk resets
    // the stall clock so only a dead connection fails the download.
    // L4 (as in download_embed_binary): UUID suffix so concurrent installs
    // don't clobber each other's partial file.
    let tmp = dest.with_file_name(format!(
        "{file_name}.{}.tmp",
        uuid::Uuid::new_v4().as_simple()
    ));
    let stream_result = async {
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("creating {}", tmp.display()))?;
        loop {
            // #685 review: even a never-stalling trickle must end eventually.
            anyhow::ensure!(
                std::time::Instant::now() < total_cutoff,
                "model download exceeded the {}-minute total cap (got {} of {} MB); \
                 the connection is too slow to finish; check it and re-run \
                 `travsr embed init`",
                total_cap.as_secs() / 60,
                downloaded.load(std::sync::atomic::Ordering::Relaxed) / 1_048_576,
                total_mb
            );
            let chunk = match tokio::time::timeout(MODEL_STALL_TIMEOUT, resp.chunk()).await {
                Err(_) => bail!(
                    "model download stalled: no data received for {}s (got {} of {} MB); \
                     check the connection and re-run `travsr embed init`",
                    MODEL_STALL_TIMEOUT.as_secs(),
                    downloaded.load(std::sync::atomic::Ordering::Relaxed) / 1_048_576,
                    total_mb
                ),
                Ok(next) => next.context("reading model file body")?,
            };
            let Some(chunk) = chunk else { break };
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                .await
                .with_context(|| format!("writing model file {file_name}"))?;
            let written = downloaded
                .fetch_add(chunk.len() as u64, std::sync::atomic::Ordering::Relaxed)
                + chunk.len() as u64;
            // #685 review: a server streaming past its own Content-Length would
            // otherwise grow the .tmp file without bound (disk-fill guard).
            if let Some(expected) = content_length {
                anyhow::ensure!(
                    written <= expected,
                    "server sent more than the advertised {expected} bytes for \
                     {file_name}; aborting instead of filling the disk"
                );
            }
        }
        tokio::io::AsyncWriteExt::flush(&mut file)
            .await
            .with_context(|| format!("flushing model file {file_name}"))?;
        Ok(())
    }
    .await;

    if let Some(h) = spinner {
        h.abort();
        use std::io::Write as _;
        eprint!("\r{}\r", " ".repeat(72));
        let _ = std::io::stderr().flush();
    }
    if let Err(e) = stream_result {
        // Best-effort: don't leave a partial .tmp behind on failure.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    // #506: a running sidecar holds the model file open; displace, not delete.
    crate::install::replace_file(&tmp, dest)
        .with_context(|| format!("installing model file {file_name}"))?;

    let actual_mb = downloaded.load(std::sync::atomic::Ordering::Relaxed) / 1_048_576;
    let pal = Palette::for_stream(std::io::stdout().is_terminal());
    println!(
        "  {} {file_name} ready · {actual_mb} MB",
        pal.green("\u{25cf}")
    );
    Ok(())
}

/// Embed the repo after `embed init`.
///
/// Always runs inline (with a live progress bar) rather than handing off to the
/// daemon. The daemon is single-repo and only ticks its embed catch-up every
/// 60 s, so a hand-off from `embed init` — especially in a repo whose daemon is
/// stale, an older build without this model, or rooted elsewhere — silently does
/// nothing (the bug this replaces). Inline is deterministic and shows progress
/// immediately. Guarded by `embed.lock` so a concurrent `travsr embed reindex`
/// can't double-write embed.db.
fn reindex_after_init(
    backend: &'static EmbedBackend,
    db_path: &Path,
    overrides: &travsr_plugin_host::EmbedOverrides,
) -> Result<()> {
    let pal = Palette::for_stream(std::io::stdout().is_terminal());
    println!();

    // L5: cross-process protection now lives one layer down, inside
    // `run_parallel_reindex` (embed_catalog.rs) — see the comment on the
    // equivalent removal in `run_reindex_locked`. Locking `embed.lock` here
    // too would self-block: `run_reindex_with_progress` below calls into that
    // same lock further down this same call stack.

    if let Err(e) = regenerate_embed_texts_if_stale(db_path) {
        tracing::warn!("embed_text regen check failed (non-fatal): {e}");
    }

    // B3: Ctrl-C during the post-init reindex cancels gracefully, no orphan sidecar.
    install_reindex_cancel_handler(db_path);

    // Show the resolved worker count / budget / priority, same wording as
    // `travsr embed reindex`, so init and reindex read identically (WS6).
    let workers = travsr_plugin_host::derive_num_workers_for_cli(db_path, overrides);
    let gov = travsr_plugin_host::resolve_governance_for_db(db_path, overrides);
    println!("  {}", reindex_banner(workers, &gov));

    run_reindex_with_progress(db_path, None, overrides)?;

    let embedded = query_embed_stats(db_path, &backend.id)
        .map(|s| s.stats.embedded)
        .unwrap_or(0);
    println!(
        "  {} {} — {} nodes embedded",
        pal.green("\u{25cf}"),
        backend.id,
        fmt_count(embedded),
    );

    // A running daemon still holds the previous model in memory; nudge a restart
    // so queries use the newly-embedded model.
    let repo_root = db_path
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(Path::new("."));
    if super::daemon_is_running(repo_root, 1, 0) {
        println!(
            "\n  {} restart the daemon to apply: travsr daemon restart",
            pal.dim("\u{2139}")
        );
    }

    Ok(())
}

/// What the installed sidecar reports it can do (`--capabilities`, travsr-embed
/// #6). Only the fields this guard needs; unknown fields are ignored so a newer
/// sidecar can add some without breaking an older CLI.
#[derive(serde::Deserialize)]
struct SidecarCapabilities {
    /// Model architectures the compiled engines can execute.
    #[serde(default)]
    families: Vec<String>,
    /// True when an engine is compiled in that runs arbitrary ONNX (ORT), in
    /// which case `families` is not exhaustive.
    #[serde(default)]
    universal_onnx: bool,
    /// Whether a hardware execution provider was COMPILED IN — not whether this
    /// host's GPU will be confirmed at run time, which needs a real model load.
    /// Enough to tell a CPU build from a GPU one, which is all the install path
    /// needs.
    #[serde(default)]
    accelerated_compiled: bool,
}

/// Refuse a model the installed sidecar provably cannot execute.
///
/// `tract` runs standard BERT only. Selecting, say, a ModernBERT model against a
/// tract-only sidecar used to fail at ONNX graph load — after the model download,
/// inside a background reindex, with an error pointing at the graph rather than
/// at the choice that caused it.
///
/// Deliberately permissive about not knowing:
///
/// * sidecar too old to accept `--capabilities` (exits non-zero, the same probe
///   contract `--version` uses) — proceed, since that is every sidecar released
///   before this handshake existed;
/// * output unparseable, or the catalog entry carries no `arch` — proceed;
/// * `universal_onnx` — proceed, because ORT runs architectures no list enumerates.
///
/// Only a sidecar that answers clearly, with a finite family list that excludes
/// this model, is treated as a refusal. A false refusal blocks a working install;
/// a false pass costs the error we already had.
/// Ask an installed sidecar what it can do.
///
/// `None` for every "it did not tell us" case — a pre-handshake build rejecting
/// the unknown flag (the same probe contract `--version` uses), a spawn failure,
/// or output that will not parse. Callers must treat `None` as "assume it is
/// fine": refusing on silence would block working installs on older sidecars.
fn read_sidecar_capabilities(sidecar: &Path) -> Option<SidecarCapabilities> {
    let out = std::process::Command::new(sidecar)
        .arg("--capabilities")
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    serde_json::from_slice(&out.stdout).ok()
}

/// Whether an already-installed sidecar was built with an accelerator.
///
/// Only `Some(false)` justifies acting: it means the sidecar answered clearly
/// that it has no GPU engine compiled in. `None` (old build, unparseable) must
/// not trigger a re-download, or every pre-handshake install would re-download on
/// each `embed init` that sets TRAVSR_EMBED_ACCEL.
fn installed_sidecar_is_accelerated(sidecar: &Path) -> Option<bool> {
    read_sidecar_capabilities(sidecar).map(|c| c.accelerated_compiled)
}

fn ensure_sidecar_can_run(sidecar: &Path, backend: &EmbedBackend) -> Result<()> {
    let family = backend.arch.trim();
    if family.is_empty() {
        return Ok(());
    }

    let caps = match read_sidecar_capabilities(sidecar) {
        Some(c) => c,
        None => return Ok(()),
    };

    if caps.universal_onnx || caps.families.iter().any(|f| f == family) {
        return Ok(());
    }

    // The remedy has to be one this platform can actually perform. An ORT-enabled
    // sidecar reaches Linux aarch64 through no published asset at all, so telling
    // a user there to install one is advice with nothing behind it.
    let remedy = match crate::install::current_target().unwrap_or_default() {
        "aarch64-unknown-linux-gnu" => {
            "This platform has no ONNX Runtime sidecar build, so choose a \
             model whose architecture is listed above."
                .to_string()
        }
        t => format!(
            "Choose a model whose architecture is listed above, or install a sidecar \
             with the ONNX Runtime engine: re-run with TRAVSR_EMBED_ACCEL={} (see the \
             travsr-embed README).",
            if t.contains("windows") {
                "directml"
            } else if t.contains("darwin") {
                "auto"
            } else {
                "cuda"
            }
        ),
    };

    bail!(
        "the installed sidecar cannot run '{}' models, which {} needs.\n\
         It supports: {}.\n\
         {remedy}",
        family,
        backend.id,
        if caps.families.is_empty() {
            "(nothing reported)".to_string()
        } else {
            caps.families.join(", ")
        }
    )
}

/// A hardware-accelerated build of the sidecar, published as a separate release
/// asset (travsr-embed #6).
///
/// The default asset is CPU-only on every platform and is what everyone gets
/// unless they ask otherwise. Accelerated builds are separate assets rather than
/// the default for two unrelated reasons: CUDA needs a host CUDA runtime + cuDNN
/// and is useless (or worse) without them, and the Windows build links no ONNX
/// Runtime at all — it loads `onnxruntime.dll` at run time, so the artifact is
/// inert unless that library ships beside it.
///
/// Hence `runtime_files`: for these variants the binary alone is not installable.
#[derive(Debug)]
struct AccelVariant {
    /// Value users pass in `TRAVSR_EMBED_ACCEL`.
    name: &'static str,
    /// Release-asset suffix, e.g. `travsr-embed-<target>-directml.exe`.
    suffix: &'static str,
    /// Must land beside the binary or the variant silently runs on CPU. A 404
    /// here fails the install rather than producing a fake GPU install.
    ///
    /// These names must stay unique ACROSS variants, not just within one.
    /// travsr-embed's release job flattens every artifact into a single directory
    /// (`merge-multiple: true`, `files: dist/**`), so a file is addressed by bare
    /// basename — `{base_url}/{name}` — and two variants shipping the same
    /// basename would overwrite each other in the release, leaving this code to
    /// fetch whichever won. No collision today: `-cuda` ships `.so` files and
    /// `-directml` ships `.dll` files. A second Windows GPU variant with its own
    /// `onnxruntime.dll` is where it would bite.
    runtime_files: &'static [&'static str],
    /// Fetched when present, skipped on 404 — providers that only some builds
    /// of ONNX Runtime carry.
    optional_files: &'static [&'static str],
}

/// Windows: DirectML drives any DX12 adapter (Intel, AMD, NVIDIA). `DirectML.dll`
/// is a Windows system component since 10 1903 and is deliberately not shipped.
const ACCEL_DIRECTML: AccelVariant = AccelVariant {
    name: "directml",
    suffix: "-directml",
    runtime_files: &["onnxruntime.dll"],
    optional_files: &["onnxruntime_providers_shared.dll"],
};

/// Linux x86_64 + NVIDIA. Statically linked ORT core, so only the provider
/// libraries ship; `nv_tensorrt_rtx` is present only in some ORT builds.
const ACCEL_CUDA: AccelVariant = AccelVariant {
    name: "cuda",
    suffix: "-cuda",
    runtime_files: &[
        "libonnxruntime_providers_shared.so",
        "libonnxruntime_providers_cuda.so",
    ],
    optional_files: &[
        "libonnxruntime_providers_tensorrt.so",
        "libonnxruntime_providers_nv_tensorrt_rtx.so",
    ],
};

/// Which accelerated build to install, from `TRAVSR_EMBED_ACCEL`.
///
/// Values: `off` (default), `auto`, `directml`, `cuda`.
///
/// `auto` is deliberately conservative — it selects a variant only where using it
/// cannot make things worse. On Windows x86_64 that is DirectML: no host runtime
/// to install, and if no usable adapter turns up the sidecar declines and falls
/// back to its CPU engine. `auto` never selects CUDA, because that artifact needs
/// a host CUDA runtime + cuDNN this code cannot verify, and picking it for
/// someone without them trades a working CPU install for a broken GPU one.
///
/// Unknown values are rejected rather than ignored: silently installing the CPU
/// build after being asked for a GPU one is the failure users cannot see.
fn resolve_accel_variant(target: &str) -> Result<Option<&'static AccelVariant>> {
    select_accel_variant(
        &std::env::var("TRAVSR_EMBED_ACCEL").unwrap_or_default(),
        target,
    )
}

/// The decision itself, separated from reading the environment so it can be
/// tested per (request, target) pair — process-wide env vars are shared mutable
/// state that parallel tests race on.
fn select_accel_variant(requested: &str, target: &str) -> Result<Option<&'static AccelVariant>> {
    let requested = requested.trim().to_ascii_lowercase();
    let win_x64 = target == "x86_64-pc-windows-msvc";
    let linux_x64 = target == "x86_64-unknown-linux-gnu";

    match requested.as_str() {
        "" | "off" | "none" => Ok(None),
        "auto" => Ok(win_x64.then_some(&ACCEL_DIRECTML)),
        "directml" if win_x64 => Ok(Some(&ACCEL_DIRECTML)),
        "cuda" if linux_x64 => Ok(Some(&ACCEL_CUDA)),
        "directml" | "cuda" => bail!(
            "TRAVSR_EMBED_ACCEL={requested} is not available for {target} \
             (directml: x86_64-pc-windows-msvc, cuda: x86_64-unknown-linux-gnu). \
             Unset it to install the CPU build."
        ),
        other => bail!(
            "unknown TRAVSR_EMBED_ACCEL value '{other}' — expected one of: \
             off, auto, directml, cuda"
        ),
    }
}

/// Download one release asset and check it against its `.sha256` sidecar.
///
/// `Ok(None)` means the asset is absent (404), which callers treat as fatal for
/// the binary and for `runtime_files`, and as fine for `optional_files`. Every
/// other failure — including a checksum mismatch — is an error: these files are
/// executed or loaded into the sidecar's process, so an unverified one is not
/// something to carry on with.
async fn fetch_release_asset(client: &reqwest::Client, url: &str) -> Result<Option<Vec<u8>>> {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;

    let sha_url = format!("{url}.sha256");
    let (resp, sha_resp) = tokio::try_join!(client.get(url).send(), client.get(&sha_url).send())
        .context("sending download requests")?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND
        || sha_resp.status() == reqwest::StatusCode::NOT_FOUND
    {
        // Binary present but sidecar missing is still "no usable asset here":
        // an unverifiable download is not one we will install.
        return Ok(None);
    }
    if !resp.status().is_success() {
        bail!("download failed ({}): {url}", resp.status());
    }
    if !sha_resp.status().is_success() {
        bail!("SHA256 download failed ({}): {sha_url}", sha_resp.status());
    }

    let bytes = resp.bytes().await.context("reading asset body")?.to_vec();
    let sha_text = sha_resp.text().await.context("reading SHA256 body")?;
    let expected = sha_text
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty SHA256 file: {sha_url}"))?;
    let actual = Sha256::digest(&bytes)
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        });
    if actual != expected {
        bail!("SHA256 mismatch for {url}: expected {expected}, got {actual}");
    }
    Ok(Some(bytes))
}

/// Install the ONNX Runtime libraries an accelerated build needs beside it.
///
/// A missing required file aborts the install. The alternative — leaving the GPU
/// binary in place without its runtime — produces a sidecar that declines the
/// accelerated backend and quietly runs on CPU: the user asked for GPU, sees a
/// successful install, and gets CPU speed with nothing to indicate why.
async fn install_runtime_files(
    client: &reqwest::Client,
    base_url: &str,
    variant: &AccelVariant,
    dest_dir: &Path,
) -> Result<()> {
    for (name, required) in variant
        .runtime_files
        .iter()
        .map(|n| (n, true))
        .chain(variant.optional_files.iter().map(|n| (n, false)))
    {
        let url = format!("{base_url}/{name}");
        match fetch_release_asset(client, &url).await? {
            Some(bytes) => {
                let dest = dest_dir.join(name);
                let tmp = dest_dir.join(format!("{name}.{}.tmp", uuid::Uuid::new_v4().as_simple()));
                std::fs::write(&tmp, &bytes)
                    .with_context(|| format!("writing {}", tmp.display()))?;
                crate::install::replace_file(&tmp, &dest)
                    .with_context(|| format!("renaming into {}", dest.display()))?;
            }
            None if required => bail!(
                "the {} sidecar build requires {name}, but it is not in this release. \
                 Installing the binary without it would produce a GPU build that \
                 silently runs on CPU. Unset TRAVSR_EMBED_ACCEL to install the CPU \
                 build instead.",
                variant.name
            ),
            None => {}
        }
    }
    Ok(())
}

async fn download_embed_binary(
    github_repo: &str,
    version: &str,
    binary_name: &str,
    target: &str,
) -> Result<PathBuf> {
    // Windows release assets carry `.exe` (travsr-embed #12); the sha256
    // sidecar is named after the full asset (`<asset>.exe.sha256`). Same rule as
    // the travsr-lang wrapper lane, so it lives in one place (#588).
    //
    // Order matters: the accelerator suffix is part of the asset's stem, the
    // `.exe` is the extension, so it goes last (#703).
    let asset_ext = crate::install::exe_suffix_for_target(target);
    let variant = resolve_accel_variant(target)?;
    let suffix = variant.map(|v| v.suffix).unwrap_or("");
    let base_url = format!("{EMBED_RELEASES_BASE}/{github_repo}/releases/download/{version}");
    let url = format!("{base_url}/{binary_name}-{target}{suffix}{asset_ext}");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(format!("travsr-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    let bin_bytes = match fetch_release_asset(&client, &url).await? {
        Some(b) => b,
        // Deliberately not falling back to the CPU asset. An accelerated build was
        // asked for explicitly, and quietly installing a different one leaves the
        // user believing they have GPU support — the same invisible failure the
        // runtime-file check above exists to prevent. Name the fix instead.
        None if variant.is_some() => bail!(
            "no accelerated asset at {url}\n\
             {github_repo} {version} does not publish a {} build for {target}. \
             Unset TRAVSR_EMBED_ACCEL to install the CPU build.",
            variant.map(|v| v.name).unwrap_or_default()
        ),
        None => bail!(
            "binary download failed (404 Not Found): {url}\n\
             {github_repo} {version} has no prebuilt binary for {target} — \
             this platform may not be supported by that release yet \
             (see https://github.com/{github_repo}/issues)"
        ),
    };

    let dest_dir = embed_bin_dir()?;
    // On-disk name mirrors the asset extension so Windows gets `travsr-embed.exe`
    // (spawn paths resolve via `EmbedBackend::binary_filename`).
    let file_name = format!("{binary_name}{asset_ext}");
    let dest = dest_dir.join(&file_name);
    // L4: use a UUID suffix so concurrent installs don't clobber each other's tmp file.
    let tmp = dest_dir.join(format!(
        "{file_name}.{}.tmp",
        uuid::Uuid::new_v4().as_simple()
    ));
    std::fs::write(&tmp, &bin_bytes).with_context(|| format!("writing {}", tmp.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .context("chmod +x embed binary")?;
    }

    // #506: upgrading while the daemon runs this sidecar previously failed
    // with Access Denied — replace_file displaces the running image aside.
    crate::install::replace_file(&tmp, &dest)
        .with_context(|| format!("renaming into {}", dest.display()))?;

    // After the binary, so a failure here leaves an installed sidecar rather than
    // a half-written one — and the binary alone still works, on CPU.
    if let Some(v) = variant {
        install_runtime_files(&client, &base_url, v, &dest_dir).await?;
    }

    Ok(dest)
}

// ── reindex ───────────────────────────────────────────────────────────────────

/// One-line "Reindexing (N workers) (capacity X, source) [priority: p]..." banner,
/// shared by `embed reindex`, `embed init`, and `embed reconfigure` so all three
/// report the resolved budget identically (G1/WS6). The capacity note is omitted
/// at the default 100% (nothing surprising to report); priority note omitted at
/// normal.
fn reindex_banner(workers: usize, gov: &travsr_plugin_host::EmbedGovernance) -> String {
    let plural = if workers == 1 { "" } else { "s" };
    let cap_note = match gov.capacity.value {
        travsr_plugin_host::Capacity::Percent(100) => String::new(),
        c => format!(
            "  (capacity {}, {})",
            c.label(),
            gov.capacity.source.label()
        ),
    };
    let prio = gov.priority.value;
    let prio_note = if prio == travsr_plugin_host::Priority::Normal {
        String::new()
    } else {
        format!("  [priority: {}]", prio.as_str())
    };
    format!("Reindexing ({workers} parallel worker{plural}){cap_note}{prio_note}...")
}

fn cmd_reindex(
    db_override: Option<PathBuf>,
    phase1: Option<u32>,
    overrides: travsr_plugin_host::EmbedOverrides,
) -> Result<()> {
    let db_path = resolve_graph_db(db_override)?;
    run_reindex_locked(&db_path, phase1, &overrides)
}

/// The locked reindex core: `embed.lock` guard, embed-text regen, resolved-budget
/// banner, Ctrl-C cancel handler, and the progress-bar run. Shared by `embed
/// reindex` and `embed reconfigure` (WS4) so both serialize on `embed.lock`,
/// report the same banner, and resume incrementally from partial work.
fn run_reindex_locked(
    db_path: &Path,
    phase1: Option<u32>,
    overrides: &travsr_plugin_host::EmbedOverrides,
) -> Result<()> {
    // UX-015: fail fast when there is no backend to reindex with, before any
    // "Preparing…" / "Reindexing (N workers)…" output implies work is underway.
    travsr_plugin_host::ensure_reindex_backend_ready(db_path)?;

    // C2: an absolute -j above the physical core count oversubscribes — warn but
    // honour it (the user asked explicitly). Best-effort core count.
    if let Some(j) = overrides.max_workers {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0);
        if cores > 0 && j > cores {
            eprintln!(
                "warning: -j {j} exceeds {cores} available cores — may oversubscribe the CPU"
            );
        }
    }

    // M6/L5: cross-process protection against a concurrent `travsr embed
    // reindex` (or `embed gc`) now lives one layer down, inside
    // `run_parallel_reindex` (embed_catalog.rs) — the single function every
    // caller (this CLI path, `embed init`'s post-install reindex, and the
    // daemon's three automatic spawn sites) funnels through. Locking
    // `embed.lock` here too would self-block: this function calls into that
    // same lock further down the same call stack, and `flock` does not treat
    // a second `open()` by the same process as compatible with the first.

    let workers = travsr_plugin_host::derive_num_workers_for_cli(db_path, overrides);
    let gov = travsr_plugin_host::resolve_governance_for_db(db_path, overrides);

    // Regenerate embed_text with correct richness if the model tier changed.
    // This is a CPU-heavy SQL pass over every node on large repos, so announce it
    // first — otherwise the command looks hung while it runs (before the bar).
    println!("Preparing embed text for {}...", db_path.display());
    if let Err(e) = regenerate_embed_texts_if_stale(db_path) {
        tracing::warn!("embed_text regen check failed (non-fatal): {e}");
    }

    // Show the resolved worker count, the capacity governor + its source, and the
    // scheduling priority so the user can confirm their config took effect (G1).
    println!("{}", reindex_banner(workers, &gov));

    // B3: Ctrl-C now gracefully cancels this foreground reindex.
    install_reindex_cancel_handler(db_path);

    // RFC-020: delegate to the parallel orchestrator with a live progress bar.
    run_reindex_with_progress(db_path, phase1, overrides)?;

    if REINDEX_CANCELLED.load(Ordering::SeqCst) {
        println!(
            "\u{2717} Reindex cancelled \u{2014} partial embeddings preserved. \
             Run `travsr embed reindex` to resume and make them searchable."
        );
        return Ok(());
    }
    println!("\u{2713} Reindex complete.");
    Ok(())
}

// ── reconfigure (WS4) ─────────────────────────────────────────────────────────

/// `travsr embed reconfigure` — change the reindex resource budget of a running
/// or paused reindex and apply it immediately.
///
/// = persist (config `set`) + graceful cancel (WS3) + respawn (WS2) resuming from
/// partial. The knob change is declarative: it is written to config so a running
/// daemon's future auto-reindexes honour it too (mirrors `systemctl set-property`
/// / `docker update`). Then any in-flight reindex is cancelled and re-launched
/// with the new params. No embedding is lost — rows persist per batch (INV-A); a
/// mid-run cancel keeps them and the respawn resumes via the `NOT EXISTS` filter.
fn cmd_reconfigure(
    db_override: Option<PathBuf>,
    overrides: travsr_plugin_host::EmbedOverrides,
    global: bool,
) -> Result<()> {
    // At least one knob must be given — reconfigure with nothing to change is a
    // user error, not a silent no-op reindex.
    if overrides.capacity.is_none()
        && overrides.max_workers.is_none()
        && overrides.priority.is_none()
    {
        bail!(
            "reconfigure needs at least one of --capacity / -j / --priority\n  \
             e.g. `travsr embed reconfigure --capacity 50`"
        );
    }

    let db_path = resolve_graph_db(db_override)?;
    let repo_root = db_path
        .parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .context("locating repo root from graph.db path")?;

    // 1. Persist the new knob(s) so the change sticks across daemon respawns.
    persist_overrides(&overrides, &repo_root, global)?;

    // 2. Cancel any in-flight reindex, then respawn with the new budget.
    trigger_reindex_now(&repo_root, &db_path, &overrides)
}

/// Write the present knobs of `overrides` to config (repo scope unless `global`),
/// using the same registry/validation as `travsr config set`.
fn persist_overrides(
    overrides: &travsr_plugin_host::EmbedOverrides,
    repo_root: &Path,
    global: bool,
) -> Result<()> {
    let scope = || {
        if global {
            travsr_config::Scope::Global
        } else {
            travsr_config::Scope::Repo(repo_root.to_path_buf())
        }
    };
    let where_ = if global {
        "global config"
    } else {
        "repo config"
    };

    if let Some(cap) = overrides.capacity {
        travsr_config::set("embed.capacity", &cap.to_config_value(), scope())?;
        println!("\u{2713} set embed.capacity = {}  ({where_})", cap.label());
    }
    if let Some(j) = overrides.max_workers {
        travsr_config::set("embed.max_workers", &j.to_string(), scope())?;
        println!("\u{2713} set embed.max_workers = {j}  ({where_})");
    }
    if let Some(p) = overrides.priority {
        travsr_config::set("embed.priority", p.as_str(), scope())?;
        println!("\u{2713} set embed.priority = {}  ({where_})", p.as_str());
    }
    Ok(())
}

/// Cancel any in-flight reindex for `repo_root`, then run a fresh reindex with
/// `overrides`, resuming from partial. Shared by `embed reconfigure` (WS4) and
/// `config set --now`.
///
/// - If a daemon holds the repo, `stop-embed` is sent: it pauses auto-reindex and
///   synchronously drains the daemon's in-flight sidecar (grace-poll then force
///   kill), so no daemon sidecar is still writing when we respawn. We `resume-embed`
///   afterwards to restore normal auto-reindex.
/// - With no daemon, we drop the cancel sentinel so a concurrent foreground
///   reindex in another terminal drains; `embed.lock` then serializes our respawn.
pub(crate) fn trigger_reindex_now(
    repo_root: &Path,
    db_path: &Path,
    overrides: &travsr_plugin_host::EmbedOverrides,
) -> Result<()> {
    let daemon_running = crate::daemon_client::daemon_lock_held(repo_root);
    let mut paused_daemon = false;

    if daemon_running {
        match crate::daemon_client::send_daemon_command(
            repo_root,
            &travsr_ipc::ControlMessage::StopEmbed,
        ) {
            Ok(resp) => {
                paused_daemon = true;
                if let Some(m) = resp.message {
                    println!("  {m}");
                }
            }
            Err(e) => {
                // Best-effort: a daemon that won't take the message shouldn't block
                // a reconfigure — the embed.lock still serializes writers.
                tracing::warn!("stop-embed before reconfigure failed: {e}");
            }
        }
    } else {
        // No daemon: nudge any foreground sidecar in another terminal to drain.
        // run_reindex_locked's spawn removes this stale sentinel before launching
        // its own sidecar, so it never cancels our new run.
        let sentinel = travsr_plugin_host::cancel_sentinel_path(db_path);
        let _ = std::fs::write(&sentinel, b"");
    }

    // A fresh reindex with the new budget. Blocks on embed.lock until the
    // cancelled run releases; resumes incrementally (no re-embed).
    let result = run_reindex_locked(db_path, None, overrides);

    if paused_daemon {
        if let Err(e) = crate::daemon_client::send_daemon_command(
            repo_root,
            &travsr_ipc::ControlMessage::ResumeEmbed,
        ) {
            tracing::warn!("resume-embed after reconfigure failed: {e}");
            eprintln!(
                "warning: could not resume daemon auto-reindex — run `travsr daemon resume-embed`"
            );
        }
    }

    result
}

/// Run the parallel reindex with a live `travsr init`-style progress bar.
///
/// The parallel orchestrator prints nothing incrementally, so a monitor thread
/// polls embed.db for the active model's embedded count (embed.db is WAL — reads
/// during the reindex writes are safe) and drives [`crate::progress::LiveBar`].
fn run_reindex_with_progress(
    db_path: &Path,
    phase1: Option<u32>,
    overrides: &travsr_plugin_host::EmbedOverrides,
) -> Result<()> {
    // Same fallback as `resolve_backend` (embed_catalog.rs): a repo indexed
    // after a global-only `embed init` has no repo config yet. Falling back
    // here only affects the progress bar's label/total, not what actually
    // gets embedded.
    let model_id = db_path
        .parent()
        .and_then(|p| p.parent())
        .and_then(travsr_plugin_host::repo_backend_id)
        .or_else(travsr_plugin_host::active_backend_id);
    let total = model_id
        .as_deref()
        .and_then(|m| query_embed_stats(db_path, m).ok())
        .map(|s| s.stats.total_symbols)
        .unwrap_or(0);

    let done_flag = Arc::new(AtomicBool::new(false));
    let monitor = model_id.map(|mid| {
        let df = Arc::clone(&done_flag);
        let db = db_path.to_path_buf();
        std::thread::spawn(move || {
            let mut bar = crate::progress::LiveBar::new("embedding");
            loop {
                let done = query_embed_stats(&db, &mid)
                    .map(|s| s.stats.embedded)
                    .unwrap_or(0);
                if df.load(Ordering::Relaxed) {
                    bar.finish(done, total);
                    break;
                }
                bar.tick(done, total);
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
        })
    });

    let result = travsr_plugin_host::run_parallel_reindex_blocking(db_path, phase1, overrides);

    done_flag.store(true, Ordering::Relaxed);
    if let Some(m) = monitor {
        let _ = m.join();
    }
    result.context("parallel reindex failed")?;

    // WS3 (E3): on a cancelled reindex the index is a partial set (fast-stop) — skip
    // calibration and keep the prior anchors rather than measuring against a
    // half-built corpus.
    if REINDEX_CANCELLED.load(Ordering::SeqCst) {
        return Ok(());
    }

    // Auto-calibrate the model-relative semantic floors on the freshly-built index.
    // Best-effort: a calibration failure must never fail the reindex.
    match calibrate_semantic_floors(db_path) {
        Ok(Some((lo, hi))) => {
            println!("\u{2713} Calibrated semantic floors (cos_lo={lo:.3}, cos_hi={hi:.3}).")
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("floor calibration failed (non-fatal): {e:#}"),
    }
    Ok(())
}

/// Auto-calibrate the model-relative semantic floors for this repo's active model.
///
/// Label-free: after a reindex, measures the model's own query↔passage cosine scale
/// on THIS corpus and records two anchors in graph.db meta, read by `travsr-mcp`'s
/// `seed::Calibration::load`:
///   • `embed_cos_hi` — p50 of top-1 cosines for a sample of real symbol signatures
///     ("a query matches its answer" scale). Run through the model's real query path,
///     so it captures query/passage-prefix asymmetry and dim-truncation compression —
///     exactly the effects that make bge-small's absolute floors wrong for other models.
///   • `embed_cos_lo` — p95 of top-1 cosines for cross-domain nonsense probes
///     ("confident but unrelated" scale).
///
/// Every embedding model — bundled, future-release, or a user's own catalog entry —
/// self-calibrates here with no hand-tuned constants. Best-effort: on any failure it
/// logs and skips the write, leaving the reader on the safe reference-model identity.
///
/// Returns `Ok(Some((lo, hi)))` when written, `Ok(None)` when skipped.
fn calibrate_semantic_floors(db_path: &Path) -> Result<Option<(f32, f32)>> {
    const SAMPLE: usize = 256;
    const MIN_SIG_SAMPLES: usize = 32;

    // Sample real symbol signatures as "answerable" probes.
    let store = travsr_store::SqliteStore::open_read_only(db_path)
        .with_context(|| format!("opening {} for calibration", db_path.display()))?;
    let nodes = store.all_nodes().context("reading nodes for calibration")?;
    drop(store);

    let mut sigs: Vec<String> = nodes
        .into_iter()
        .filter(|n| {
            matches!(
                n.kind.as_str(),
                "function" | "method" | "class" | "interface" | "struct" | "enum"
            )
        })
        .filter_map(|n| {
            // Strip the leading "kind:" tag; embed the bare symbol path as the query.
            let s = n.vname.signature;
            let bare = s
                .split_once(':')
                .map(|(_, r)| r)
                .unwrap_or(s.as_str())
                .trim();
            (!bare.is_empty()).then(|| bare.to_string())
        })
        .collect();
    if sigs.len() < MIN_SIG_SAMPLES {
        tracing::warn!(
            "floor calibration skipped: only {} eligible signatures (< {MIN_SIG_SAMPLES})",
            sigs.len()
        );
        return Ok(None);
    }
    // Deterministic stride sample down to SAMPLE (spread across the corpus).
    if sigs.len() > SAMPLE {
        let stride = (sigs.len() / SAMPLE).max(1);
        sigs = sigs.into_iter().step_by(stride).take(SAMPLE).collect();
    }

    // One sidecar spawn: probe [signatures.., nonsense..] and split by index.
    let n_sig = sigs.len();
    let mut all = sigs;
    all.extend(NONSENSE_PROBES.iter().map(|s| s.to_string()));
    let Some(cos) = travsr_plugin_host::probe_top1_cosines(db_path, &all) else {
        tracing::warn!("floor calibration skipped: no embedding backend/binary");
        return Ok(None);
    };
    if cos.len() != all.len() {
        tracing::warn!(
            "floor calibration skipped: probe returned {} of {} cosines",
            cos.len(),
            all.len()
        );
        return Ok(None);
    }
    let (sig_cos, non_cos) = cos.split_at(n_sig);

    // hi = p50 of positive self-match cosines; lo = p95 of nonsense cosines.
    let hi = percentile(sig_cos.iter().copied().filter(|c| *c > 0.0).collect(), 0.50);
    let lo = percentile(non_cos.to_vec(), 0.95);
    let (Some(cos_hi), Some(cos_lo)) = (hi, lo) else {
        tracing::warn!("floor calibration skipped: insufficient probe cosines");
        return Ok(None);
    };
    if cos_hi <= cos_lo {
        tracing::warn!(
            "floor calibration skipped: degenerate band (hi {cos_hi:.3} <= lo {cos_lo:.3})"
        );
        return Ok(None);
    }

    // Persist to graph.db meta (read by Calibration::load).
    let mut store = travsr_store::SqliteStore::open(db_path)
        .with_context(|| format!("opening {} to write calibration", db_path.display()))?;
    store
        .set_meta("embed_cos_lo", &format!("{cos_lo}"))
        .context("writing embed_cos_lo")?;
    store
        .set_meta("embed_cos_hi", &format!("{cos_hi}"))
        .context("writing embed_cos_hi")?;
    if let Some(mid) = db_path
        .parent()
        .and_then(|p| p.parent())
        .and_then(travsr_plugin_host::repo_backend_id)
    {
        // Provenance: which model these anchors describe (aids debugging a stale set).
        let _ = store.set_meta("embed_cos_model", &mid);
    }
    Ok(Some((cos_lo, cos_hi)))
}

/// Nearest-rank percentile `p ∈ [0,1]` of an unsorted sample. `None` when empty.
fn percentile(mut xs: Vec<f32>, p: f32) -> Option<f32> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = (((xs.len() - 1) as f32) * p).round() as usize;
    Some(xs[idx.min(xs.len() - 1)])
}

/// Fixed cross-domain probes with no presence in a code repo — the "confident but
/// unrelated" cosine scale for floor calibration. Deliberately spans many domains so
/// the p95 reflects the model's genuine background, not one adversarial near-miss.
const NONSENSE_PROBES: &[&str] = &[
    "quantum blockchain payment gateway subscription billing invoice",
    "react redux frontend css animation component styling theme",
    "audio waveform synthesizer midi signal processing reverb filter",
    "recipe cooking ingredients oven temperature baking sourdough",
    "planetary orbit telescope astronomy nebula galaxy redshift",
    "guitar chord progression melody rhythm tempo verse chorus",
    "medieval castle knight armor sword shield banner heraldry",
    "watercolor landscape painting brush canvas pigment gallery",
    "marathon training nutrition hydration cadence stride recovery",
    "stock dividend portfolio hedge derivative option yield curve",
    "coral reef marine biology plankton tide salinity lagoon",
    "origami paper folding crane geometry crease valley mountain",
];

// ── status ────────────────────────────────────────────────────────────────────

struct EmbedStats {
    total_symbols: u64,
    embedded: u64,
    phase1_total: u64,
    phase1_done: u64,
    phase2_total: u64,
    phase2_done: u64,
}

struct EmbedStatsWithThreshold {
    stats: EmbedStats,
    threshold: u32,
}

fn query_embed_stats(db_path: &std::path::Path, model_id: &str) -> Result<EmbedStatsWithThreshold> {
    // Derive the per-repo threshold the same way the reindex orchestrator does
    // so the phase breakdown labels match what was actually embedded.
    // Falls back to 3 when k-core data isn't available (pre-init or very small repos).
    let threshold = travsr_plugin_host::derive_phase1_threshold_for_status(db_path).unwrap_or(3);
    let store = travsr_store::SqliteStore::open_read_only(db_path)
        .with_context(|| format!("opening {}", db_path.display()))?;
    let (total_symbols, embedded, phase1_total, phase1_done) =
        store.embed_progress(model_id, threshold)?;
    let phase2_total = total_symbols.saturating_sub(phase1_total);
    let phase2_done = embedded.saturating_sub(phase1_done);
    Ok(EmbedStatsWithThreshold {
        stats: EmbedStats {
            total_symbols,
            embedded,
            phase1_total,
            phase1_done,
            phase2_total,
            phase2_done,
        },
        threshold,
    })
}

fn fmt_eta(remaining: u64, nodes_per_sec: f64) -> String {
    if nodes_per_sec < 1.0 || remaining == 0 {
        return String::new();
    }
    let secs = (remaining as f64 / nodes_per_sec).round() as u64;
    if secs < 60 {
        format!("~{secs}s remaining")
    } else if secs < 3600 {
        format!("~{}m remaining", secs / 60)
    } else {
        format!("~{}h {}m remaining", secs / 3600, (secs % 3600) / 60)
    }
}

fn fmt_count(n: u64) -> String {
    // group digits with commas: 126820 → "126,820"
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn file_size_mb(path: &std::path::Path) -> Option<f64> {
    std::fs::metadata(path)
        .ok()
        .map(|m| m.len() as f64 / 1_048_576.0)
}

/// Whether a backend's model weights + descriptor are on disk, independent of
/// the sidecar binary (which is shared across every backend and so cannot
/// answer "is *this* model installed" — #482).
fn model_files_installed(b: &EmbedBackend) -> bool {
    embed_model_dir(&b.id)
        .ok()
        .map(|d| {
            b.model_files.iter().all(|f| d.join(&f.name).exists()) && d.join("model.toml").exists()
        })
        .unwrap_or(false)
}

/// The HNSW index files (code + docs space) that exist on disk for a set of
/// models, keyed off `db_path`'s directory the same way `cmd_status` locates
/// the active model's own index (`{model}.hnsw.usearch`, `embed.rs:~1533`).
fn reclaimable_hnsw_paths(db_path: &Path, models: &[(String, u64, u64)]) -> Vec<PathBuf> {
    let dir = db_path.parent().unwrap_or(db_path);
    models
        .iter()
        .flat_map(|(id, _, _)| {
            [
                dir.join(format!("{id}.hnsw.usearch")),
                dir.join(format!("{id}-docs.hnsw.usearch")),
            ]
        })
        .filter(|p| p.exists())
        .collect()
}

fn cmd_status() -> Result<()> {
    let bin_dir = embed_bin_dir()?;

    // Resolve the repo-local model first. Fall back to global active only for
    // the install-state display (binary/model files check).
    let cwd = std::env::current_dir().context("getting cwd")?;
    let repo_root = crate::repo::find_git_root(&cwd).ok();
    let repo_active_id = repo_root
        .as_ref()
        .and_then(|r| travsr_plugin_host::repo_backend_id(r));

    // For install checks: use whichever model the user has configured globally.
    let global_id = load_config().and_then(|c| c.active);
    let display_id = repo_active_id.as_deref().or(global_id.as_deref());

    // ── backend / install state ───────────────────────────────────────────────
    let (backend_ok, _) = match display_id {
        None => {
            println!("No embedding backend is installed.");
            println!("Run `travsr embed init` to install and activate one for this repo.");
            return Ok(());
        }
        Some(id) => match lookup_embed_backend(id) {
            None => {
                println!("Active backend '{id}' is not in the catalog (stale config?).");
                println!("Run `travsr embed list` to see available backends.");
                return Ok(());
            }
            Some(b) => {
                let installed = bin_dir.join(b.binary_filename()).exists();
                // #391: the sidecar requires a `model.toml` descriptor and exits code 1
                // without it, so a present-but-descriptor-less install is NOT ready.
                // Check it alongside the model weights to avoid the old false positive.
                let models_ok = model_files_installed(b);

                let ok = installed && models_ok;
                println!("Backend        : {} (installed)", b.id);
                println!("Description    : {}", b.description);
                println!(
                    "Binary         : {}",
                    if installed {
                        "\u{2713} installed"
                    } else {
                        "\u{2717} missing — run `travsr embed init`"
                    }
                );
                println!(
                    "Model files    : {}",
                    if models_ok {
                        "\u{2713} present"
                    } else {
                        "\u{2717} missing — run `travsr embed init`"
                    }
                );
                (ok, ())
            }
        },
    };

    if !backend_ok {
        println!("\nRun `travsr embed init` to complete installation.");
        return Ok(());
    }

    // ── per-repo activation state ─────────────────────────────────────────────
    match &repo_active_id {
        Some(id) => {
            println!("Repo model     : {id} (configured for this repo)");
            // #482/#483: `status` and `list` used to name different models with
            // no explanation. Surface the divergence rather than silently
            // picking one — the repo layer always wins for retrieval (#481).
            if let Some(g) = global_id.as_deref() {
                if g != id.as_str() {
                    println!(
                        "                 (machine default is '{g}' — this repo's setting wins)"
                    );
                }
            }
        }
        None => {
            println!("Repo model     : not configured — run `travsr embed init` to activate for this repo");
        }
    }

    // ── repo progress ─────────────────────────────────────────────────────────
    let db_path = {
        match crate::repo::find_git_root(&cwd) {
            Ok(root) => root.join(".travsr/graph.db"),
            Err(_) => {
                println!("\n(not inside a travsr repo — run `travsr init` first to see progress)");
                return Ok(());
            }
        }
    };

    if !db_path.exists() {
        println!("\n(graph.db not found — run `travsr init` first)");
        return Ok(());
    }

    println!();
    println!(
        "Repo           : {}",
        db_path.parent().unwrap_or(&db_path).display()
    );

    // L2: what `embed gc` would reclaim, shown only when non-zero so the
    // common case (nothing to reclaim) is unchanged. Uses the exact same
    // helper `embed gc` reads, so the two surfaces can never disagree.
    if let Some(id) = repo_active_id.as_deref() {
        let embed_db_path = db_path.with_file_name("embed.db");
        if let Ok(rows) = travsr_plugin_host::embeddings_by_model(&embed_db_path) {
            let reclaimable: Vec<_> = rows.into_iter().filter(|(m, _, _)| m != id).collect();
            if !reclaimable.is_empty() {
                let vec_count: u64 = reclaimable.iter().map(|(_, r, _)| r).sum();
                let vec_bytes: u64 = reclaimable.iter().map(|(_, _, b)| b).sum();
                let hnsw_paths = reclaimable_hnsw_paths(&db_path, &reclaimable);
                let hnsw_bytes_mb: f64 = hnsw_paths.iter().filter_map(|p| file_size_mb(p)).sum();
                let total_mb = vec_bytes as f64 / 1_048_576.0 + hnsw_bytes_mb;
                println!(
                    "Reclaimable    : {} vectors + {} index file{} from {} inactive model{} ({total_mb:.1} MB) \u{2014} travsr embed gc",
                    fmt_count(vec_count),
                    hnsw_paths.len(),
                    if hnsw_paths.len() == 1 { "" } else { "s" },
                    reclaimable.len(),
                    if reclaimable.len() == 1 { "" } else { "s" },
                );
            }
        }
    }

    // Phase B state: compare phase_b_commit vs last_commit.
    {
        let store = travsr_store::SqliteStore::open_read_only(&db_path)
            .with_context(|| format!("opening {}", db_path.display()))?;
        let last = store.get_meta("last_commit")?.unwrap_or_default();
        let phase_b = store.get_meta("phase_b_commit")?.unwrap_or_default();
        let state = if last.is_empty() {
            "not run (no commits yet)"
        } else if phase_b.is_empty() {
            "pending (run `travsr daemon start` to trigger)"
        } else if phase_b == last {
            "complete"
        } else {
            "stale (new commits since the last semantic index)"
        };
        println!("Semantic index : {state}");
    }

    // If this repo hasn't been configured, skip the per-repo progress section.
    let repo_model = match repo_active_id.as_deref() {
        Some(id) => id,
        None => return Ok(()),
    };
    let EmbedStatsWithThreshold { stats, threshold } = query_embed_stats(&db_path, repo_model)?;

    if stats.total_symbols == 0 {
        println!("No symbol nodes found — run `travsr init` to index the repo.");
        return Ok(());
    }

    let pct = stats.embedded as f64 / stats.total_symbols as f64 * 100.0;
    // Phase 1 throughput ~400 nodes/sec (k8s: 109k nodes / 4.5 min).
    // Phase 2 is background and ~10× slower on a loaded machine.
    let nodes_per_sec: f64 = if stats.phase1_done < stats.phase1_total {
        400.0
    } else {
        40.0
    };
    let remaining = stats.total_symbols.saturating_sub(stats.embedded);
    let eta = fmt_eta(remaining, nodes_per_sec);
    let pal = Palette::for_stream(std::io::stdout().is_terminal());
    let bar = crate::progress::bar_of_width(pal, stats.embedded, stats.total_symbols, 36);

    println!("Total symbols  : {}", fmt_count(stats.total_symbols));
    println!(
        "Embedded       : {}  ({:.0}%)",
        fmt_count(stats.embedded),
        pct
    );
    if eta.is_empty() {
        println!("{bar}  done");
    } else {
        println!("{bar}  {eta}");
    }

    // ── per-phase breakdown ───────────────────────────────────────────────────
    println!();
    let p1_pct = if stats.phase1_total > 0 {
        stats.phase1_done as f64 / stats.phase1_total as f64 * 100.0
    } else {
        100.0
    };
    let p1_bar = crate::progress::bar_of_width(pal, stats.phase1_done, stats.phase1_total, 24);
    let p1_eta = fmt_eta(stats.phase1_total.saturating_sub(stats.phase1_done), 400.0);
    println!(
        "core symbols (centrality \u{2265}{threshold}) {} {}/{}  ({:.0}%)  {}",
        p1_bar,
        fmt_count(stats.phase1_done),
        fmt_count(stats.phase1_total),
        p1_pct,
        if p1_eta.is_empty() {
            "\u{2713} complete".to_string()
        } else {
            p1_eta
        },
    );

    let p2_pct = if stats.phase2_total > 0 {
        stats.phase2_done as f64 / stats.phase2_total as f64 * 100.0
    } else {
        100.0
    };
    let p2_bar = crate::progress::bar_of_width(pal, stats.phase2_done, stats.phase2_total, 24);
    let p2_eta = fmt_eta(stats.phase2_total.saturating_sub(stats.phase2_done), 40.0);
    println!(
        "other symbols (centrality <{threshold}) {} {}/{}  ({:.0}%)  {}",
        p2_bar,
        fmt_count(stats.phase2_done),
        fmt_count(stats.phase2_total),
        p2_pct,
        if p2_eta.is_empty() {
            "\u{2713} complete".to_string()
        } else {
            p2_eta
        },
    );

    // ── HNSW index ────────────────────────────────────────────────────────────
    // Index is repo-local (node IDs are per-db SQLite rowids).
    // Sidecar places it at <repo>/.travsr/<backend-id>.hnsw.usearch.
    println!();
    let hnsw_path = db_path
        .parent()
        .unwrap_or(&db_path)
        .join(format!("{}.hnsw.usearch", repo_model));
    if let Some(mb) = file_size_mb(&hnsw_path) {
        println!(
            "HNSW index     : {mb:.0} MB  ({} vectors)",
            fmt_count(stats.embedded),
        );
    } else {
        println!("Vector index   : not built yet (completes after the core pass finishes)");
    }

    // ── actionable hints ──────────────────────────────────────────────────────
    if stats.embedded == 0 && stats.total_symbols > 0 {
        println!();
        println!(
            "hint: no symbols embedded yet, the daemon starts embedding after semantic indexing."
        );
        println!("      If the daemon is not running: travsr daemon start");
    } else if remaining > 0 {
        println!();
        println!("hint: embedding is running in the background via the daemon.");
        println!("      Run `travsr embed status` again in a few minutes to see progress.");
    }

    // RFC-025 §8: sidecar version health (installed vs required vs latest).
    println!();
    crate::sidecar_health::print_block();

    Ok(())
}

// ── switch ────────────────────────────────────────────────────────────────────

fn cmd_switch(backend_id: &str, global: bool) -> Result<()> {
    let backend = lookup_embed_backend(backend_id).ok_or_else(|| {
        anyhow::anyhow!("Unknown backend '{backend_id}'. Run `travsr embed list`.")
    })?;

    // #482: the sidecar binary is shared by every backend, so its presence only
    // proves *some* model was installed once. Switching to a model whose weights
    // are absent used to succeed here and then contradict itself — `embed list`
    // reporting `installed=false` and `embed status` `✗ missing` for the model
    // it had just made active. Check the model's own files, as list/status do.
    let bin_dir = embed_bin_dir()?;
    if !bin_dir.join(backend.binary_filename()).exists() || !model_files_installed(backend) {
        bail!(
            "Backend '{backend_id}' is not installed. Run `travsr embed init --backend {backend_id}` first."
        );
    }

    // Inside an indexed repo, "switch this repo's model" is the only intent
    // that has any effect: per-repo embedding reads <repo>/.travsr/embed.toml,
    // not the machine config (#481). --global or "not in a repo" keeps the
    // pre-#481 behavior of writing the machine config.
    let repo_root = if global {
        None
    } else {
        std::env::current_dir()
            .ok()
            // Write path: switch this worktree's model, never the main worktree's (#586).
            .and_then(|c| crate::repo::find_git_root_for_write(&c).ok())
            .filter(|r| r.join(".travsr/graph.db").exists())
    };

    let Some(root) = repo_root else {
        let mut config = load_config().unwrap_or_default();
        config.active = Some(backend_id.to_string());
        save_config(&config)?;
        println!("\u{2713} Switched active backend to '{}'.", backend_id);
        println!("  Restart the daemon to apply: travsr daemon restart");
        return Ok(());
    };

    let previous = travsr_plugin_host::repo_backend_id(&root);
    travsr_plugin_host::write_repo_backend_id(&root, backend_id)?;

    println!("\u{2713} Switched this repo to '{}'.", backend_id);

    // A model change makes every existing vector for the previous model
    // unusable (different space, often a different dimension). Tell the user
    // what just became inactive so `embed gc` (W3) has a real trigger.
    if let Some(prev_id) = previous.filter(|p| p != backend_id) {
        let embed_db = root.join(".travsr/embed.db");
        if let Ok(rows) = travsr_plugin_host::embeddings_by_model(&embed_db) {
            if let Some((_, count, bytes)) = rows.into_iter().find(|(m, _, _)| m == &prev_id) {
                if count > 0 {
                    let mb = bytes as f64 / 1_048_576.0;
                    println!(
                        "  {} vectors for '{prev_id}' are now inactive ({mb:.1} MB).",
                        fmt_count(count)
                    );
                }
            }
        }
    }
    println!("  Re-embed:  travsr embed reindex");
    println!("  Reclaim:   travsr embed gc          (after the re-embed succeeds)");
    Ok(())
}

// ── gc ────────────────────────────────────────────────────────────────────────

fn cmd_gc(apply: bool, keep: Vec<String>) -> Result<()> {
    let db_path = resolve_graph_db(None)?;
    let repo_root = db_path
        .parent()
        .and_then(|p| p.parent())
        .context("resolving repo root from graph.db path")?;

    // L5: refuse to run against an in-flight reindex — reclamation deletes
    // rows a concurrent reindex may be actively writing to.
    let _lock = travsr_plugin_host::EmbedOpLock::try_acquire(repo_root, "gc")?
        .context("could not open .travsr/embed.lock")?;

    let active = travsr_plugin_host::repo_backend_id(repo_root)
        .or_else(travsr_plugin_host::active_backend_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No embedding backend configured for this repo. Run `travsr embed init` first."
            )
        })?;

    // Default keeps the repo's active model; --keep adds exceptions (the A/B
    // use case). Assert non-empty and containing the active model — a bug
    // that emptied this set would delete every vector in the repo.
    let mut keep_set = keep;
    if !keep_set.iter().any(|k| k == &active) {
        keep_set.push(active.clone());
    }
    anyhow::ensure!(
        !keep_set.is_empty() && keep_set.contains(&active),
        "internal error: keep-set must always contain the active model"
    );

    let embed_db_path = db_path.with_file_name("embed.db");
    let all = travsr_plugin_host::embeddings_by_model(&embed_db_path)?;
    let embedded_models = all.len();
    let reclaimable: Vec<(String, u64, u64)> = all
        .into_iter()
        .filter(|(m, _, _)| !keep_set.contains(m))
        .collect();

    if reclaimable.is_empty() {
        println!("Nothing to reclaim \u{2014} every embedded model is in the keep-set.");
        return Ok(());
    }

    // The keep-set is resolved from *config*, but what gets deleted is decided
    // by what is in *embed.db* — and those diverge exactly when it is most
    // expensive. `embed switch` writes the new model to config and prints
    // `Re-embed` and `Reclaim` two lines apart, so a user who runs the second
    // without the first has an active model with zero vectors and a keep-set
    // that protects nothing. The same holds for a repo with no repo config
    // whose global active model was switched from some other directory.
    //
    // Refuse whenever the sweep would take every vector the repo has. `--keep`
    // is the escape hatch: naming a model that actually has rows is an explicit
    // statement about what to preserve, and it clears this guard.
    anyhow::ensure!(
        reclaimable.len() < embedded_models,
        "Refusing to reclaim: '{active}' is this repo's active model but has no embeddings \
         yet, so this would delete every vector in the repo ({} across {embedded_models} \
         model{}) and leave it with none.\n\
         Re-embed first:  travsr embed reindex\n\
         Or name what to keep:  travsr embed gc --keep <model>",
        fmt_count(reclaimable.iter().map(|(_, r, _)| r).sum::<u64>()),
        if embedded_models == 1 { "" } else { "s" },
    );

    let hnsw_paths = reclaimable_hnsw_paths(&db_path, &reclaimable);
    let vec_bytes: u64 = reclaimable.iter().map(|(_, _, b)| b).sum();
    let vec_count: u64 = reclaimable.iter().map(|(_, r, _)| r).sum();
    let hnsw_mb: f64 = hnsw_paths.iter().filter_map(|p| file_size_mb(p)).sum();

    if !apply {
        println!("Would reclaim:");
        for (model, rows, bytes) in &reclaimable {
            println!(
                "  {model}: {} vectors ({:.1} MB)",
                fmt_count(*rows),
                *bytes as f64 / 1_048_576.0
            );
        }
        println!(
            "  {} HNSW index file{} ({hnsw_mb:.1} MB)",
            hnsw_paths.len(),
            if hnsw_paths.len() == 1 { "" } else { "s" }
        );
        println!(
            "Total: {:.1} MB. Run with --apply to reclaim.",
            vec_bytes as f64 / 1_048_576.0 + hnsw_mb
        );
        return Ok(());
    }

    // Files first, rows second. The HNSW files belong to inactive models, so
    // nothing reads them and deleting them early is safe — whereas the reverse
    // order strands them permanently if the process dies in between: the next
    // `gc` derives its file list from `embeddings_by_model`, which by then
    // returns nothing for the reclaimed model, so it reports "nothing to
    // reclaim" while hundreds of MB of orphaned indexes stay on disk.
    let mut removed_files = 0usize;
    for path in &hnsw_paths {
        match std::fs::remove_file(path) {
            Ok(()) => removed_files += 1,
            Err(e) => tracing::warn!("could not remove {}: {e}", path.display()),
        }
    }

    let (deleted, vacuum_skipped) = travsr_plugin_host::gc_embeddings(&embed_db_path, &keep_set)?;
    let deleted_models: Vec<&str> = deleted.iter().map(|(m, _, _)| m.as_str()).collect();

    println!(
        "\u{2713} Reclaimed {} vectors ({:.1} MB) from {}: {}",
        fmt_count(vec_count),
        vec_bytes as f64 / 1_048_576.0,
        if deleted_models.len() == 1 {
            "model"
        } else {
            "models"
        },
        deleted_models.join(", ")
    );
    println!(
        "  Removed {removed_files} of {} HNSW index file{}.",
        hnsw_paths.len(),
        if hnsw_paths.len() == 1 { "" } else { "s" }
    );
    if let Some(reason) = vacuum_skipped {
        println!(
            "  warning: VACUUM skipped ({reason}) \u{2014} rows were deleted but embed.db's size \
             on disk is unchanged. Re-run `travsr embed gc --apply` later to shrink it (it is \
             idempotent \u{2014} nothing left to delete will report zero). A running daemon or \
             MCP server holding embed.db open is the usual cause: travsr daemon stop"
        );
    }
    Ok(())
}

// ── paths ─────────────────────────────────────────────────────────────────────

fn travsr_dir() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("HOME not set"))?
        .join(".travsr"))
}

fn embed_bin_dir() -> Result<PathBuf> {
    let dir = travsr_dir()?.join("bin");
    std::fs::create_dir_all(&dir).context("creating ~/.travsr/bin")?;
    Ok(dir)
}

fn embed_model_dir(backend_id: &str) -> Result<PathBuf> {
    let dir = travsr_dir()?.join("models").join(backend_id);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating ~/.travsr/models/{backend_id}"))?;
    Ok(dir)
}

/// #712: whether the `travsr-embed` sidecar binary is present in `~/.travsr/bin`.
/// Read-only file existence check — no probe.
fn embed_binary_installed() -> bool {
    let Some(bin_dir) = dirs::home_dir().map(|h| h.join(".travsr").join("bin")) else {
        return false;
    };
    travsr_plugin_host::embed_backends()
        .into_iter()
        .any(|b| bin_dir.join(b.binary_filename()).exists())
}

/// #712 F: prompt the user to turn on embeddings when the sidecar is installed
/// but no backend is active. Without this the semantic path (`ask`/`get_context`)
/// silently runs without embeddings and `travsr status` gave no hint that a
/// one-command fix (`travsr embed init`) was waiting. Silent when embeddings are
/// already active, or when the binary is not installed (init's install tip and
/// `travsr embed init` cover that case).
pub fn hint_activate_if_installed() {
    if travsr_plugin_host::active_backend_id().is_some() {
        return;
    }
    if embed_binary_installed() {
        println!(
            "tip: embeddings are installed but not enabled — run `travsr embed init` to turn on semantic search"
        );
    }
}

// ── config ────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct EmbedConfig {
    /// Stable ID of the active backend (matches `EmbedBackend::id`).
    active: Option<String>,
}

fn config_path() -> Result<PathBuf> {
    Ok(travsr_dir()?.join("embed.toml"))
}

fn load_config() -> Option<EmbedConfig> {
    let path = config_path().ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    toml::from_str(&content).ok()
}

fn save_config(config: &EmbedConfig) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating ~/.travsr")?;
    }
    let content = toml::to_string_pretty(config).context("serialising embed config")?;
    std::fs::write(&path, content).context("writing embed.toml")?;
    Ok(())
}

#[cfg(test)]
mod accel_tests {
    use super::*;

    const WIN: &str = "x86_64-pc-windows-msvc";
    const LINUX: &str = "x86_64-unknown-linux-gnu";
    const MAC: &str = "aarch64-apple-darwin";

    #[test]
    fn default_and_off_install_the_cpu_build() {
        for req in ["", "off", "none", "  OFF  "] {
            for tgt in [WIN, LINUX, MAC] {
                assert!(select_accel_variant(req, tgt).unwrap().is_none());
            }
        }
    }

    /// `auto` may only pick a variant that cannot leave the user worse off.
    /// DirectML qualifies: no host runtime to install, and the sidecar declines
    /// to its CPU engine if no usable adapter appears.
    #[test]
    fn auto_selects_directml_on_windows_only() {
        assert_eq!(
            select_accel_variant("auto", WIN).unwrap().map(|v| v.name),
            Some("directml")
        );
        // Never CUDA: that artifact needs a host CUDA runtime + cuDNN this code
        // cannot check, so auto-selecting it would trade a working CPU install
        // for a broken GPU one.
        assert!(select_accel_variant("auto", LINUX).unwrap().is_none());
        assert!(select_accel_variant("auto", MAC).unwrap().is_none());
    }

    #[test]
    fn explicit_requests_resolve_on_their_own_platform() {
        assert_eq!(
            select_accel_variant("directml", WIN)
                .unwrap()
                .map(|v| v.name),
            Some("directml")
        );
        assert_eq!(
            select_accel_variant("CUDA", LINUX).unwrap().map(|v| v.name),
            Some("cuda")
        );
    }

    /// Wrong-platform and unknown values must fail loudly. Falling back to the
    /// CPU build would hand back a successful install that is not what was asked
    /// for — the failure a user cannot see.
    #[test]
    fn wrong_platform_or_unknown_value_is_an_error() {
        let e = select_accel_variant("cuda", WIN).unwrap_err().to_string();
        assert!(e.contains("not available"), "{e}");
        assert!(select_accel_variant("directml", LINUX).is_err());
        assert!(select_accel_variant("directml", MAC).is_err());

        let e = select_accel_variant("rocm", LINUX).unwrap_err().to_string();
        assert!(e.contains("unknown"), "{e}");
        // Names the valid values, so the message is actionable on its own.
        assert!(e.contains("directml") && e.contains("off"), "{e}");
    }

    /// Every accelerated variant must declare at least one runtime file. A
    /// variant with none would install a GPU binary with nothing beside it and
    /// silently run on CPU — the case `install_runtime_files` exists to prevent.
    #[test]
    fn every_variant_declares_its_runtime_files() {
        for v in [&ACCEL_DIRECTML, &ACCEL_CUDA] {
            assert!(!v.runtime_files.is_empty(), "{} has none", v.name);
            assert!(v.suffix.starts_with('-'), "{} suffix", v.name);
        }
    }

    /// The upgrade decision, isolated from the install flow that uses it.
    ///
    /// Guards the gap this logic exists to close: `TRAVSR_EMBED_ACCEL` used to be
    /// read only inside the download path, which an already-installed sidecar
    /// never reaches — so the variable did nothing, and the user was told the
    /// sidecar was "ready".
    fn wants_upgrade(variant_requested: bool, installed_accelerated: Option<bool>) -> bool {
        variant_requested && installed_accelerated == Some(false)
    }

    #[test]
    fn cpu_install_is_upgraded_when_an_accelerator_is_requested() {
        assert!(wants_upgrade(true, Some(false)));
    }

    #[test]
    fn an_already_accelerated_install_is_left_alone() {
        assert!(!wants_upgrade(true, Some(true)));
    }

    /// No request means no reinstall, even on a CPU build — `embed init` must stay
    /// idempotent for everyone who never sets the variable.
    #[test]
    fn no_request_never_reinstalls() {
        assert!(!wants_upgrade(false, Some(false)));
        assert!(!wants_upgrade(false, Some(true)));
        assert!(!wants_upgrade(false, None));
    }

    /// `None` is "the sidecar could not tell us" — a pre-handshake build that
    /// exits non-zero on `--capabilities`. Treating that as "not accelerated"
    /// would re-download on every single init for those users.
    #[test]
    fn a_sidecar_that_cannot_answer_is_not_reinstalled() {
        assert!(!wants_upgrade(true, None));
    }
}
