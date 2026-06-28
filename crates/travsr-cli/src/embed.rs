//! `travsr embed` — RFC-018 embedding plugin management.
//!
//! Manages downloadable embed sidecar binaries and their model files.
//! No compile-time features — the binary never links against ort or sqlite-vec.

use anyhow::{bail, Context as _, Result};
use clap::Subcommand;
use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use travsr_daemon::regenerate_embed_texts_if_stale;
use travsr_plugin_host::{lookup_embed_backend, EmbedBackend, EMBED_BACKENDS};

use crate::progress::{fmt_dur, Palette};

const EMBED_RELEASES_BASE: &str = "https://github.com";
const HF_BASE: &str = "https://huggingface.co";

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
    },
    /// Show the currently active embedding model and binary status.
    Status,
    /// Switch the active embedding backend (binary must already be installed).
    Switch {
        /// Backend ID to make active (run `travsr embed list` to see options).
        backend: String,
    },
}

pub fn run(cmd: EmbedCommand) -> Result<()> {
    match cmd {
        EmbedCommand::List { json } => cmd_list(json),
        EmbedCommand::Init { backend, reinstall } => cmd_init(backend.as_deref(), reinstall),
        EmbedCommand::Reindex { db, phase1 } => cmd_reindex(db, phase1),
        EmbedCommand::Status => cmd_status(),
        EmbedCommand::Switch { backend } => cmd_switch(&backend),
    }
}

// ── list ──────────────────────────────────────────────────────────────────────

fn cmd_list(json: bool) -> Result<()> {
    let active = load_config().and_then(|c| c.active);
    let bin_dir = embed_bin_dir()?;

    if json {
        let entries: Vec<String> = EMBED_BACKENDS
            .iter()
            .map(|b| {
                let installed = bin_dir.join(b.binary_name).exists();
                let is_active = active.as_deref() == Some(b.id);
                format!(
                    r#"{{"id":"{}","description":"{}","dim":{},"params_m":{},"mteb":{:.1},"ram_mb":{},"download_mb":{},"installed":{},"active":{}}}"#,
                    b.id,
                    b.description,
                    b.dim,
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
    for b in EMBED_BACKENDS {
        let installed = bin_dir.join(b.binary_name).exists();
        let is_active = active.as_deref() == Some(b.id);
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
            b.dim,
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

fn cmd_init(backend_id: Option<&str>, reinstall: bool) -> Result<()> {
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
                for b in EMBED_BACKENDS {
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

    println!();
    install_backend_with_progress(backend, reinstall)?;

    // Record as globally installed/active (used by `travsr embed list` and hints).
    let mut config = load_config().unwrap_or_default();
    config.active = Some(backend.id.to_string());
    save_config(&config)?;

    // Write per-repo config so the daemon only auto-embeds repos the user
    // explicitly opted into. The repo must already be initialised (graph.db exists).
    let repo_root = std::env::current_dir()
        .ok()
        .and_then(|c| crate::repo::find_git_root(&c).ok());
    let db_path = repo_root
        .as_ref()
        .map(|r| r.join(".travsr/graph.db"))
        .filter(|p| p.exists());

    if let Some(ref root) = repo_root {
        if db_path.is_some() {
            if let Err(e) = travsr_plugin_host::write_repo_backend_id(root, backend.id) {
                tracing::warn!("could not write repo embed config: {e}");
            }
        }
    }

    match db_path {
        Some(ref p) => reindex_after_init(backend, p)?,
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

/// Interactive numbered model selector, matching the `travsr lang detect` style.
fn pick_backend_interactive() -> Result<Option<&'static EmbedBackend>> {
    let active = load_config().and_then(|c| c.active);
    let bin_dir = embed_bin_dir()?;

    println!("  Available embedding models:\n");
    for (i, b) in EMBED_BACKENDS.iter().enumerate() {
        let is_active = active.as_deref() == Some(b.id);
        let installed = bin_dir.join(b.binary_name).exists()
            && embed_model_dir(b.id)
                .map(|d| b.model_files.iter().all(|f| d.join(f.name).exists()))
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
            b.dim,
            b.params_m,
            b.mteb,
            dl_mb,
            ram,
            b.description,
        );
    }

    use std::io::Write as _;
    print!("  Which model? (1-{}, q to quit): ", EMBED_BACKENDS.len());
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim();

    if input.eq_ignore_ascii_case("q") || input.is_empty() {
        return Ok(None);
    }

    match input.parse::<usize>() {
        Ok(n) if n >= 1 && n <= EMBED_BACKENDS.len() => Ok(Some(&EMBED_BACKENDS[n - 1])),
        _ => {
            println!("  invalid selection '{input}'");
            Ok(None)
        }
    }
}

fn install_backend_with_progress(backend: &'static EmbedBackend, reinstall: bool) -> Result<()> {
    let pal = Palette::for_stream(std::io::stdout().is_terminal());
    let bin_dir = embed_bin_dir()?;
    let dest = bin_dir.join(backend.binary_name);

    if dest.exists() && !reinstall {
        println!("  {} {} ready", pal.green("\u{25cf}"), backend.binary_name);
    } else {
        let target = crate::install::current_target().context("determining install target")?;
        let repo = backend.github_repo.to_string();
        let version = crate::lang::run_async(async move {
            crate::install::fetch_latest_version_for_repo(&repo).await
        })
        .unwrap_or_else(|_| backend.version_fallback.to_string());

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
            println!(
                "\n  hint: add ~/.travsr/bin to your PATH:\n\
                 \n\t  export PATH=\"$HOME/.travsr/bin:$PATH\"\n"
            );
        }
    }

    // Model files — one spinner per file.
    let model_dir = embed_model_dir(backend.id)?;
    for mf in backend.model_files {
        let dest = model_dir.join(mf.name);
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

    Ok(())
}

async fn download_model_file_with_progress(
    hf_repo: &str,
    url_path: &str,
    file_name: &str,
    dest: &std::path::Path,
    size_hint_mb: u32,
) -> Result<()> {
    let url = format!("{HF_BASE}/{hf_repo}/resolve/main/{url_path}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .user_agent(format!("travsr-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    let resp = client.get(&url).send().await.context("GET model file")?;
    if !resp.status().is_success() {
        bail!("model file download failed ({}): {url}", resp.status());
    }
    let total_mb = resp
        .content_length()
        .map(|n| n / 1_048_576)
        .unwrap_or(size_hint_mb as u64);

    let is_tty = std::io::stderr().is_terminal();
    let name = file_name.to_string();

    // Spinner task — aborted once bytes() resolves.
    let spinner = if is_tty {
        Some(tokio::spawn(async move {
            use std::io::Write as _;
            const FRAMES: [char; 4] = ['◐', '◓', '◑', '◒'];
            let pal = Palette::for_stream(true);
            let start = std::time::Instant::now();
            let mut i = 0usize;
            loop {
                let spin = pal.orange(&FRAMES[i % 4].to_string());
                let elapsed = start.elapsed().as_secs();
                eprint!("\r  {spin} downloading {name} ({total_mb} MB) ...  {elapsed}s    ");
                let _ = std::io::stderr().flush();
                i += 1;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }))
    } else {
        eprintln!("  downloading {file_name} ({total_mb} MB) ...");
        None
    };

    let bytes = resp.bytes().await.context("reading model file body")?;

    if let Some(h) = spinner {
        h.abort();
        use std::io::Write as _;
        eprint!("\r{}\r", " ".repeat(72));
        let _ = std::io::stderr().flush();
    }

    let tmp = dest.with_extension("tmp");
    std::fs::write(&tmp, &bytes).with_context(|| format!("writing model file {file_name}"))?;
    std::fs::rename(&tmp, dest).with_context(|| format!("installing model file {file_name}"))?;

    let actual_mb = bytes.len() / 1_048_576;
    let pal = Palette::for_stream(std::io::stdout().is_terminal());
    println!(
        "  {} {file_name} ready · {actual_mb} MB",
        pal.green("\u{25cf}")
    );
    Ok(())
}

/// Reindex after `embed init`. If the daemon is already running we hand off to
/// it rather than spawning a second sidecar that fights for the embed.db lock.
fn reindex_after_init(backend: &'static EmbedBackend, db_path: &std::path::Path) -> Result<()> {
    let is_tty = std::io::stderr().is_terminal();
    let pal = Palette::for_stream(std::io::stdout().is_terminal());

    println!();

    // If the daemon is running it already embeds autonomously in the background.
    // Spawning a second sidecar here would create two concurrent writers on
    // embed.db — SQLite lock contention that throttles throughput to ~5 nodes/s.
    // Also skip embed_text regen — the daemon handles that in maybe_spawn_embed_phase2.
    let repo_root = db_path
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(std::path::Path::new("."));
    if super::daemon_is_running(repo_root, 1, 0) {
        println!("  {} {} is now active", pal.green("\u{25cf}"), backend.id,);
        println!(
            "  {} the daemon is embedding {} nodes in the background",
            pal.dim("\u{2139}"),
            backend.id,
        );
        println!(
            "  {} run `travsr embed status` to monitor progress",
            pal.dim("\u{2139}"),
        );
        return Ok(());
    }

    // Daemon not running: regen embed_text then run the blocking reindex inline.
    if let Err(e) = regenerate_embed_texts_if_stale(db_path) {
        tracing::warn!("embed_text regen check failed (non-fatal): {e}");
    }

    let done = Arc::new(AtomicBool::new(false));
    let done2 = Arc::clone(&done);
    let mid = backend.id.to_string();

    let spinner = std::thread::spawn(move || {
        if !is_tty {
            return;
        }
        use std::io::Write as _;
        const FRAMES: [char; 4] = ['◐', '◓', '◑', '◒'];
        let pal2 = Palette::for_stream(true);
        let start = std::time::Instant::now();
        let mut i = 0usize;
        while !done2.load(Ordering::Relaxed) {
            let spin = pal2.orange(&FRAMES[i % 4].to_string());
            let elapsed = start.elapsed().as_secs();
            eprint!("\r  {spin} reindexing with {mid} ...  {elapsed}s    ");
            let _ = std::io::stderr().flush();
            i += 1;
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });

    let start = std::time::Instant::now();
    let result = travsr_plugin_host::run_parallel_reindex_blocking_quiet(db_path);

    done.store(true, Ordering::Relaxed);
    let _ = spinner.join();

    if is_tty {
        use std::io::Write as _;
        eprint!("\r{}\r", " ".repeat(72));
        let _ = std::io::stderr().flush();
    }

    result?;

    let elapsed = fmt_dur(start.elapsed());
    let embedded = query_embed_stats(db_path, backend.id)
        .map(|s| s.stats.embedded)
        .unwrap_or(0);

    println!(
        "  {} {} — {} nodes embedded · {elapsed}",
        pal.green("\u{25cf}"),
        backend.id,
        fmt_count(embedded),
    );
    println!(
        "\n  {} restart the daemon to apply: travsr daemon restart",
        pal.dim("\u{2139}")
    );

    Ok(())
}

async fn download_embed_binary(
    github_repo: &str,
    version: &str,
    binary_name: &str,
    target: &str,
) -> Result<PathBuf> {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;

    let url = format!(
        "{EMBED_RELEASES_BASE}/{github_repo}/releases/download/{version}/{binary_name}-{target}"
    );
    let sha_url = format!("{url}.sha256");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(format!("travsr-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    let (bin_resp, sha_resp) =
        tokio::try_join!(client.get(&url).send(), client.get(&sha_url).send())
            .context("sending download requests")?;

    if !bin_resp.status().is_success() {
        bail!("download failed ({}): {url}", bin_resp.status());
    }
    if !sha_resp.status().is_success() {
        bail!("SHA256 download failed ({}): {sha_url}", sha_resp.status());
    }

    let bin_bytes = bin_resp.bytes().await.context("reading binary body")?;
    let sha_text = sha_resp.text().await.context("reading SHA256 body")?;

    let expected = sha_text
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty SHA256 file"))?
        .to_string();
    let actual = {
        let hash = Sha256::digest(&bin_bytes);
        hash.iter().fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    };
    if actual != expected {
        bail!("SHA256 mismatch for {binary_name}: expected {expected}, got {actual}");
    }

    let dest_dir = embed_bin_dir()?;
    let dest = dest_dir.join(binary_name);
    // L4: use a UUID suffix so concurrent installs don't clobber each other's tmp file.
    let tmp = dest_dir.join(format!(
        "{binary_name}.{}.tmp",
        uuid::Uuid::new_v4().as_simple()
    ));
    std::fs::write(&tmp, &bin_bytes).with_context(|| format!("writing {}", tmp.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .context("chmod +x embed binary")?;
    }

    std::fs::rename(&tmp, &dest).with_context(|| format!("renaming into {}", dest.display()))?;

    Ok(dest)
}

// ── reindex ───────────────────────────────────────────────────────────────────

fn cmd_reindex(db_override: Option<PathBuf>, phase1: Option<u32>) -> Result<()> {
    let db_path = match db_override {
        Some(p) => p,
        None => {
            let cwd = std::env::current_dir().context("getting cwd")?;
            let repo_root = crate::repo::find_git_root(&cwd)?;
            let p = repo_root.join(".travsr/graph.db");
            anyhow::ensure!(
                p.exists(),
                "graph.db not found at {}\n  Run `travsr init` first.",
                p.display()
            );
            p
        }
    };

    // M6: prevent concurrent `travsr embed reindex` runs from writing to the same
    // embed.db simultaneously (two terminals, CI + local). Flock on embed.lock in
    // the same directory as graph.db. Blocks until the other run finishes.
    let embed_lock_path = db_path
        .parent()
        .map(|p| p.join("embed.lock"))
        .unwrap_or_else(|| std::path::PathBuf::from("embed.lock"));
    let embed_lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&embed_lock_path)
        .context("opening embed.lock")?;
    fs2::FileExt::lock_exclusive(&embed_lock)
        .context("acquiring embed.lock — another `travsr embed reindex` may be running")?;

    let workers = travsr_plugin_host::derive_num_workers_for_cli();
    println!(
        "Reindexing {} ({} parallel worker{})...",
        db_path.display(),
        workers,
        if workers == 1 { "" } else { "s" },
    );

    // Regenerate embed_text with correct richness if the model tier changed.
    if let Err(e) = regenerate_embed_texts_if_stale(&db_path) {
        tracing::warn!("embed_text regen check failed (non-fatal): {e}");
    }

    // RFC-020: delegate to the parallel orchestrator in travsr-plugin-host.
    // It handles worker count derivation, range partitioning, temp-file isolation,
    // merge, and failure recovery per C-01 … C-08.
    travsr_plugin_host::run_parallel_reindex_blocking(&db_path, phase1)
        .context("parallel reindex failed")?;

    println!("\u{2713} Reindex complete.");
    Ok(())
}

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

fn progress_bar(done: u64, total: u64, width: usize) -> String {
    if total == 0 {
        return format!("[{}]", " ".repeat(width));
    }
    let filled = ((done as f64 / total as f64) * width as f64).round() as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    format!("[{}{}]", "=".repeat(filled), " ".repeat(empty))
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
                let installed = bin_dir.join(b.binary_name).exists();
                let model_dir = embed_model_dir(b.id).ok();
                let models_ok = model_dir
                    .as_ref()
                    .map(|d| b.model_files.iter().all(|f| d.join(f.name).exists()))
                    .unwrap_or(false);

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
        Some(id) => println!("Repo model     : {id} (configured for this repo)"),
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
            "stale (new commits since last Phase B)"
        };
        println!("Phase B state  : {state}");
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
    let bar = progress_bar(stats.embedded, stats.total_symbols, 36);

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
    let p1_bar = progress_bar(stats.phase1_done, stats.phase1_total, 24);
    let p1_eta = fmt_eta(stats.phase1_total.saturating_sub(stats.phase1_done), 400.0);
    println!(
        "Phase 1 (shell \u{2265}{threshold}) {} {}/{}  ({:.0}%)  {}",
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
    let p2_bar = progress_bar(stats.phase2_done, stats.phase2_total, 24);
    let p2_eta = fmt_eta(stats.phase2_total.saturating_sub(stats.phase2_done), 40.0);
    println!(
        "Phase 2 (shell <{threshold}) {} {}/{}  ({:.0}%)  {}",
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
        println!("HNSW index     : not built yet (completes after Phase 1 finishes)");
    }

    // ── actionable hints ──────────────────────────────────────────────────────
    if stats.embedded == 0 && stats.total_symbols > 0 {
        println!();
        println!("hint: no nodes embedded yet — the daemon triggers embedding after Phase B.");
        println!("      If the daemon is not running: travsr daemon start");
    } else if remaining > 0 {
        println!();
        println!("hint: embedding is running in the background via the daemon.");
        println!("      Run `travsr embed status` again in a few minutes to see progress.");
    }

    Ok(())
}

// ── switch ────────────────────────────────────────────────────────────────────

fn cmd_switch(backend_id: &str) -> Result<()> {
    let backend = lookup_embed_backend(backend_id).ok_or_else(|| {
        anyhow::anyhow!("Unknown backend '{backend_id}'. Run `travsr embed list`.")
    })?;

    let bin_dir = embed_bin_dir()?;
    if !bin_dir.join(backend.binary_name).exists() {
        bail!(
            "Backend '{backend_id}' is not installed. Run `travsr embed init --backend {backend_id}` first."
        );
    }

    let mut config = load_config().unwrap_or_default();
    config.active = Some(backend_id.to_string());
    save_config(&config)?;

    println!("\u{2713} Switched active backend to '{}'.", backend_id);
    println!("  Restart the daemon to apply: travsr daemon restart");
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
