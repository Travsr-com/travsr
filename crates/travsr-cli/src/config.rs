//! `travsr config` — inspect and set layered configuration (WS1, #422).
//!
//! Thin CLI wrapper over the `travsr-config` crate: the registry, validation,
//! layered resolution, and file I/O all live there. This module only parses the
//! subcommand and renders results.

use anyhow::{Context as _, Result};
use clap::Subcommand;
use travsr_config::{Scope, Source};

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Show a single key's active value and which layer set it.
    Get {
        /// Dotted key, e.g. `embed.capacity`.
        key: String,
    },
    /// Set a key. Writes the global config unless `--repo` is given.
    Set {
        /// Dotted key, e.g. `embed.capacity`.
        key: String,
        /// New value (validated against the key's type).
        value: String,
        /// Write to this repository's `.travsr/config.toml` instead of the global one.
        #[arg(long)]
        repo: bool,
        /// Apply immediately: after writing, gracefully cancel any in-flight embed
        /// reindex and respawn it with the new value (WS4). Only meaningful for the
        /// `embed.*` governance keys; a no-op prompt otherwise.
        #[arg(
            long,
            help = "Apply immediately: after writing, gracefully cancel any in-flight embed reindex and respawn it with the new value. Only meaningful for the `embed.*` governance keys; a no-op prompt otherwise."
        )]
        now: bool,
    },
    /// List every known key with its resolved value and source layer.
    List {
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cmd: ConfigCommand) -> Result<()> {
    match cmd {
        ConfigCommand::Get { key } => cmd_get(&key),
        ConfigCommand::Set {
            key,
            value,
            repo,
            now,
        } => cmd_set(&key, &value, repo, now),
        ConfigCommand::List { json } => cmd_list(json),
    }
}

/// Resolve the current repo root for a **read** (`config get`/`list`), if we are
/// inside one (best-effort — config is still usable outside a repo, where only
/// global + env + default apply).
fn current_repo_root() -> Option<std::path::PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    crate::repo::find_git_root(&cwd).ok()
}

/// Write-path variant of [`current_repo_root`]. `config set --repo`/`--now`
/// mutate the worktree's own `.travsr/` (repo-scoped config, or an immediate
/// reindex), so they resolve the worktree we are standing in rather than
/// redirecting to the main worktree (issue #586).
fn current_repo_root_for_write() -> Option<std::path::PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    crate::repo::find_git_root_for_write(&cwd).ok()
}

fn cmd_get(key: &str) -> Result<()> {
    let repo_root = current_repo_root();
    let status = travsr_config::get(key, repo_root.as_deref())?;
    match status.value {
        Some(v) => println!("{v}    ({})", status.source.label()),
        None => println!("{}    (default, unset)", status.default_display),
    }
    Ok(())
}

fn cmd_set(key: &str, value: &str, repo: bool, now: bool) -> Result<()> {
    let scope = if repo {
        let root = current_repo_root_for_write()
            .context("--repo requires being inside a git repository (no .travsr found)")?;
        Scope::Repo(root)
    } else {
        Scope::Global
    };
    travsr_config::set(key, value, scope)?;
    let where_ = if repo { "repo config" } else { "global config" };
    println!("\u{2713} set {key} = {value}  ({where_})");

    // WS4: `--now` applies an embed governance change immediately by cancelling
    // and respawning any in-flight reindex with the freshly-written config (the
    // default overrides resolve to the value we just set).
    if now {
        if !key.starts_with("embed.") {
            eprintln!(
                "note: --now only applies to `embed.*` keys; '{key}' written but not applied"
            );
            return Ok(());
        }
        let root = current_repo_root_for_write()
            .context("--now needs to run inside a git repository (to locate the reindex target)")?;
        let db_path = root.join(".travsr/graph.db");
        anyhow::ensure!(
            db_path.exists(),
            "graph.db not found at {} — run `travsr init` first",
            db_path.display()
        );
        crate::embed::trigger_reindex_now(
            &root,
            &db_path,
            &travsr_plugin_host::EmbedOverrides::default(),
        )?;
    }
    Ok(())
}

fn cmd_list(json: bool) -> Result<()> {
    let repo_root = current_repo_root();
    let statuses = travsr_config::list(repo_root.as_deref());

    if json {
        let entries: Vec<String> = statuses
            .iter()
            .map(|s| {
                format!(
                    r#"{{"key":"{}","value":{},"source":"{}","default":"{}"}}"#,
                    s.key,
                    match &s.value {
                        Some(v) => format!("\"{}\"", v.replace('"', "\\\"")),
                        None => "null".to_string(),
                    },
                    s.source.label(),
                    s.default_display,
                )
            })
            .collect();
        println!("[{}]", entries.join(",\n"));
        return Ok(());
    }

    println!("{:<20} {:<26} {:<14} DESCRIPTION", "KEY", "VALUE", "SOURCE");
    println!("{}", "-".repeat(92));
    for s in &statuses {
        let value = match &s.value {
            Some(v) => v.clone(),
            None => format!("{} (default)", s.default_display),
        };
        let source = if s.source == Source::Default {
            "-".to_string()
        } else {
            s.source.label().to_string()
        };
        println!(
            "{:<20} {:<26} {:<14} {}",
            s.key, value, source, s.description
        );
    }
    Ok(())
}
