//! `travsr embed` — RFC-018 embedding plugin management.
//!
//! Manages downloadable embed sidecar binaries and their model files.
//! No compile-time features — the binary never links against ort or sqlite-vec.

use anyhow::{bail, Context as _, Result};
use clap::Subcommand;
use std::path::PathBuf;
use travsr_plugin_host::{lookup_embed_backend, EmbedBackend, EMBED_BACKENDS};

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
        /// Defaults to the first catalog entry (currently nomic-v1.5-int8).
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
                    r#"{{"id":"{}","description":"{}","dim":{},"installed":{},"active":{}}}"#,
                    b.id, b.description, b.dim, installed, is_active
                )
            })
            .collect();
        println!("[{}]", entries.join(",\n"));
        return Ok(());
    }

    println!(
        "{:<22} {:<12} {:<10} DESCRIPTION",
        "BACKEND", "DIM", "STATUS"
    );
    println!("{}", "-".repeat(90));
    for b in EMBED_BACKENDS {
        let installed = bin_dir.join(b.binary_name).exists();
        let is_active = active.as_deref() == Some(b.id);
        let status = if installed && is_active {
            "\u{2713} active".to_string()
        } else if installed {
            "installed".to_string()
        } else {
            "not installed".to_string()
        };
        println!(
            "{:<22} {:<12} {:<10} {}",
            b.id, b.dim, status, b.description
        );
    }
    Ok(())
}

// ── init ──────────────────────────────────────────────────────────────────────

fn cmd_init(backend_id: Option<&str>, reinstall: bool) -> Result<()> {
    let backend = match backend_id {
        Some(id) => lookup_embed_backend(id).ok_or_else(|| {
            anyhow::anyhow!("Unknown backend '{id}'. Run `travsr embed list` to see options.")
        })?,
        None => EMBED_BACKENDS
            .first()
            .ok_or_else(|| anyhow::anyhow!("No embed backends in catalog."))?,
    };

    install_backend(backend, reinstall)?;

    // Activate immediately if this is the first install or explicit choice.
    let mut config = load_config().unwrap_or_default();
    config.active = Some(backend.id.to_string());
    save_config(&config)?;

    println!(
        "\u{2713} '{}' is now the active embedding backend.",
        backend.id
    );
    println!("  Restart the daemon to apply: travsr daemon restart");
    Ok(())
}

fn install_backend(backend: &'static EmbedBackend, reinstall: bool) -> Result<()> {
    let bin_dir = embed_bin_dir()?;
    let dest = bin_dir.join(backend.binary_name);

    if dest.exists() && !reinstall {
        println!("\u{2713} {} already installed.", backend.binary_name);
    } else {
        let target = crate::install::current_target().context("determining install target")?;

        let repo = backend.github_repo.to_string();
        let version = crate::lang::run_async(async move {
            crate::install::fetch_latest_version_for_repo(&repo).await
        })
        .unwrap_or_else(|e| {
            eprintln!(
                "warning: could not fetch latest version ({e:#}), using {}",
                backend.version_fallback
            );
            backend.version_fallback.to_string()
        });

        println!("Downloading {} {} ...", backend.binary_name, version);

        let bin_name = backend.binary_name.to_string();
        let repo2 = backend.github_repo.to_string();
        let ver2 = version.clone();
        let tgt = target.to_string();

        let path = crate::lang::run_async(async move {
            download_embed_binary(&repo2, &ver2, &bin_name, &tgt).await
        })
        .context("downloading embed binary")?;

        println!(
            "\u{2713} {} installed to {}",
            backend.binary_name,
            path.display()
        );

        if !crate::install::path_contains_travsr_bin() {
            println!(
                "\nhint: add ~/.travsr/bin to your PATH:\n\n\
                 \texport PATH=\"$HOME/.travsr/bin:$PATH\"\n"
            );
        }
    }

    // Download model files.
    let model_dir = embed_model_dir(backend.id)?;
    let mut any_downloaded = false;
    for mf in backend.model_files {
        let dest = model_dir.join(mf.name);
        if dest.exists() && !reinstall {
            println!("\u{2713} {} already present.", mf.name);
            continue;
        }
        println!("Downloading {} (~{} MB) ...", mf.name, mf.size_hint_mb);
        let hf_repo = mf.hf_repo.to_string();
        let url_path = mf.url_path.to_string();
        let name = mf.name.to_string();
        let dest2 = dest.clone();
        crate::lang::run_async(async move {
            download_model_file(&hf_repo, &url_path, &name, &dest2).await
        })
        .with_context(|| format!("downloading model file {}", mf.name))?;
        any_downloaded = true;
    }
    if !any_downloaded && !reinstall {
        println!("\u{2713} All model files already present.");
    }

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
    let tmp = dest_dir.join(format!("{binary_name}.tmp"));
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

async fn download_model_file(
    hf_repo: &str,
    url_path: &str,
    file_name: &str,
    dest: &std::path::Path,
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
    let bytes = resp.bytes().await.context("reading model file body")?;
    // Atomic write: stage to .tmp then rename so an interrupted download never
    // leaves a truncated file that passes the dest.exists() re-download guard.
    let tmp = dest.with_extension("tmp");
    std::fs::write(&tmp, &bytes).with_context(|| format!("writing model file {file_name}"))?;
    std::fs::rename(&tmp, dest).with_context(|| format!("installing model file {file_name}"))?;
    println!(
        "\u{2713} {} saved ({} MB).",
        file_name,
        bytes.len() / (1024 * 1024)
    );
    Ok(())
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

    let config = load_config().ok_or_else(|| {
        anyhow::anyhow!("No embedding backend active. Run `travsr embed init` first.")
    })?;
    let backend_id = config.active.as_deref().ok_or_else(|| {
        anyhow::anyhow!("No embedding backend active. Run `travsr embed init` first.")
    })?;
    let backend = lookup_embed_backend(backend_id)
        .ok_or_else(|| anyhow::anyhow!("Active backend '{backend_id}' not in catalog."))?;

    let bin_path = embed_bin_dir()?.join(backend.binary_name);
    anyhow::ensure!(
        bin_path.exists(),
        "Sidecar binary not found: {}\n  Run `travsr embed init` to install it.",
        bin_path.display()
    );

    println!(
        "Reindexing {} with backend '{}'...",
        db_path.display(),
        backend_id,
    );

    let mut cmd = std::process::Command::new(&bin_path);
    cmd.arg("--reindex").arg(&db_path);
    if let Some(t) = phase1 {
        cmd.arg("--phase1").arg(t.to_string());
    }
    let status = cmd
        .status()
        .with_context(|| format!("spawning {}", bin_path.display()))?;
    if !status.success() {
        anyhow::bail!("reindex failed (exit code {:?})", status.code());
    }
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

fn query_embed_stats(db_path: &std::path::Path, model_id: &str) -> Result<EmbedStats> {
    let store = travsr_store::SqliteStore::open_read_only(db_path)
        .with_context(|| format!("opening {}", db_path.display()))?;
    let (total_symbols, embedded, phase1_total, phase1_done) =
        store.embed_progress(model_id, 3)?;
    let phase2_total = total_symbols.saturating_sub(phase1_total);
    let phase2_done = embedded.saturating_sub(phase1_done);
    Ok(EmbedStats {
        total_symbols,
        embedded,
        phase1_total,
        phase1_done,
        phase2_total,
        phase2_done,
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
    std::fs::metadata(path).ok().map(|m| m.len() as f64 / 1_048_576.0)
}

fn cmd_status() -> Result<()> {
    let config = load_config();
    let active_id = config.as_ref().and_then(|c| c.active.as_deref());
    let bin_dir = embed_bin_dir()?;

    // ── backend / install state ───────────────────────────────────────────────
    let (backend_ok, backend) = match active_id {
        None => {
            println!("No embedding backend is active.");
            println!("Run `travsr embed init` to install one.");
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
                println!("Backend        : {}", b.id);
                println!("Description    : {}", b.description);
                println!(
                    "Binary         : {}",
                    if installed { "\u{2713} installed" } else { "\u{2717} missing — run `travsr embed init`" }
                );
                println!(
                    "Model files    : {}",
                    if models_ok { "\u{2713} present" } else { "\u{2717} missing — run `travsr embed init`" }
                );
                (ok, b)
            }
        },
    };

    if !backend_ok {
        println!("\nRun `travsr embed init` to complete installation.");
        return Ok(());
    }

    // ── repo progress ─────────────────────────────────────────────────────────
    let db_path = {
        let cwd = std::env::current_dir().context("getting cwd")?;
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
    println!("Repo           : {}", db_path.parent().unwrap_or(&db_path).display());

    let stats = query_embed_stats(&db_path, backend.id)?;

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

    println!(
        "Total symbols  : {}",
        fmt_count(stats.total_symbols)
    );
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
    let p1_eta = fmt_eta(
        stats.phase1_total.saturating_sub(stats.phase1_done),
        400.0,
    );
    println!(
        "Phase 1 (shell \u{2265}3) {} {}/{}  ({:.0}%)  {}",
        p1_bar,
        fmt_count(stats.phase1_done),
        fmt_count(stats.phase1_total),
        p1_pct,
        if p1_eta.is_empty() { "\u{2713} complete".to_string() } else { p1_eta },
    );

    let p2_pct = if stats.phase2_total > 0 {
        stats.phase2_done as f64 / stats.phase2_total as f64 * 100.0
    } else {
        100.0
    };
    let p2_bar = progress_bar(stats.phase2_done, stats.phase2_total, 24);
    let p2_eta = fmt_eta(
        stats.phase2_total.saturating_sub(stats.phase2_done),
        40.0,
    );
    println!(
        "Phase 2 (shell <3) {} {}/{}  ({:.0}%)  {}",
        p2_bar,
        fmt_count(stats.phase2_done),
        fmt_count(stats.phase2_total),
        p2_pct,
        if p2_eta.is_empty() { "\u{2713} complete".to_string() } else { p2_eta },
    );

    // ── HNSW index ────────────────────────────────────────────────────────────
    println!();
    let hnsw_path = travsr_dir()?
        .join("models")
        .join(backend.id)
        .join("hnsw.usearch");
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
        println!("      Watch live: travsr daemon logs --follow");
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
