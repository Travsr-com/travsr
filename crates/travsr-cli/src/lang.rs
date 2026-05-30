//! `travsr lang` subcommands — Phase B language package management.
//!
//! Reads the Phase B catalog from travsr-plugin-host to show what tools
//! exist, then manages registration in ~/.travsr/lang.toml.
//!
//! Catalog is provided by travsr-plugin-host::phase_b::catalog.

use std::path::PathBuf;
use anyhow::{Context as _, Result};
use clap::Subcommand;
use travsr_plugin_host::phase_b::catalog::{lookup, SandboxRequirement, CATALOG};

#[derive(Debug, Subcommand)]
pub enum LangCommand {
    /// Show all known Phase B language tools and their status.
    List,
    /// Register a Phase B tool for a language.
    Add {
        /// Canonical language name (e.g. rust, java, php).
        language: String,
    },
    /// Unregister a Phase B tool for a language.
    Remove {
        /// Canonical language name.
        language: String,
    },
    /// Record PSE approval for a RequiresElevated language.
    /// Must be run before `travsr lang add` for Java, Kotlin, C#, Scala.
    Approve {
        /// Canonical language name (e.g. java, csharp).
        language: String,
        /// GitHub handle of the Principal Security Engineer approving this.
        #[arg(long)]
        approved_by: String,
        /// One-sentence justification (recorded in config).
        #[arg(long)]
        reason: String,
    },
}

pub fn run(cmd: LangCommand) -> Result<()> {
    match cmd {
        LangCommand::List => cmd_list(),
        LangCommand::Add { language } => cmd_add(&language),
        LangCommand::Remove { language } => cmd_remove(&language),
        LangCommand::Approve {
            language,
            approved_by,
            reason,
        } => cmd_approve(&language, &approved_by, &reason),
    }
}

// ── list ──────────────────────────────────────────────────────────────────────

fn cmd_list() -> Result<()> {
    let config = load_config();

    println!(
        "{:<12} {:<22} {:<10} {:<10} {}",
        "LANGUAGE", "COMMAND", "SANDBOX", "STATUS", ""
    );
    println!("{}", "-".repeat(72));

    for entry in CATALOG {
        let sandbox_label = match entry.sandbox {
            SandboxRequirement::Standard => "Standard",
            SandboxRequirement::RequiresElevated => "Elevated",
        };

        let on_path = which(entry.command);
        let registered = config
            .as_ref()
            .map(|c| c.is_registered(entry.language))
            .unwrap_or(false);
        let approved = config
            .as_ref()
            .map(|c| c.is_approved(entry.language))
            .unwrap_or(false);

        let status = if entry.sandbox == SandboxRequirement::RequiresElevated && !approved {
            "needs approval (travsr lang approve)".to_string()
        } else if registered && on_path {
            "\u{2713} active".to_string()
        } else if registered && !on_path {
            format!("registered but {} not on PATH", entry.command)
        } else if on_path {
            format!(
                "on PATH, not registered (run: travsr lang add {})",
                entry.language
            )
        } else {
            format!("not installed  hint: {}", entry.install_hint)
        };

        println!(
            "{:<12} {:<22} {:<10} {}",
            entry.language, entry.command, sandbox_label, status
        );
    }
    Ok(())
}

// ── add ───────────────────────────────────────────────────────────────────────

fn cmd_add(language: &str) -> Result<()> {
    let entry = lookup(language).ok_or_else(|| {
        anyhow::anyhow!(
            "Unknown language '{language}'. Run `travsr lang list` to see available languages."
        )
    })?;

    // RequiresElevated: must be approved first
    if entry.sandbox == SandboxRequirement::RequiresElevated {
        let config = load_config();
        let approved = config
            .as_ref()
            .map(|c| c.is_approved(language))
            .unwrap_or(false);
        if !approved {
            anyhow::bail!(
                "'{language}' Phase B requires SandboxPolicy::Elevated (ADR-017 Rule 1).\n\
                 {}\n\
                 \n\
                 Record PSE approval first:\n\
                 \n\
                 travsr lang approve {language} \\\n\
                 \t--approved-by <pse-github-handle> \\\n\
                 \t--reason \"<one-sentence justification>\"\n\
                 \n\
                 Then re-run: travsr lang add {language}",
                entry.install_hint
            );
        }
    }

    // Check tool is on PATH
    if !which(entry.command) {
        println!(
            "Warning: '{}' is not on PATH. Phase B for '{language}' will be\n\
             registered but disabled until the tool is installed.\n\
             Install: {}",
            entry.command, entry.install_hint
        );
    }

    // Register in config
    let mut config = load_config().unwrap_or_default();
    config.register(language);
    save_config(&config)?;

    println!(
        "\u{2713} '{language}' Phase B registered.\n\
         To activate for a repository:\n\
         \n\
         travsr config set plugins.trust.<corpus> true"
    );
    Ok(())
}

// ── remove ────────────────────────────────────────────────────────────────────

fn cmd_remove(language: &str) -> Result<()> {
    if lookup(language).is_none() {
        anyhow::bail!("Unknown language '{language}'.");
    }
    let mut config = load_config().unwrap_or_default();
    if config.unregister(language) {
        save_config(&config)?;
        println!("\u{2713} '{language}' Phase B unregistered.");
    } else {
        println!("'{language}' was not registered.");
    }
    Ok(())
}

// ── approve ───────────────────────────────────────────────────────────────────

fn cmd_approve(language: &str, approved_by: &str, reason: &str) -> Result<()> {
    let entry = lookup(language)
        .ok_or_else(|| anyhow::anyhow!("Unknown language '{language}'."))?;

    if entry.sandbox != SandboxRequirement::RequiresElevated {
        anyhow::bail!(
            "'{language}' uses Standard sandbox — no approval needed. \
             Run `travsr lang add {language}` directly."
        );
    }

    anyhow::ensure!(!approved_by.is_empty(), "--approved-by must not be empty");
    anyhow::ensure!(!reason.is_empty(), "--reason must not be empty");

    let mut config = load_config().unwrap_or_default();
    config.approve(language, approved_by, reason);
    save_config(&config)?;

    println!(
        "\u{2713} PSE approval recorded for '{language}'.\n\
         Run `travsr lang add {language}` to complete registration."
    );
    Ok(())
}

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct LangConfig {
    #[serde(default)]
    registered: Vec<String>,
    #[serde(default)]
    elevated_approvals: Vec<ElevatedApproval>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ElevatedApproval {
    language: String,
    approved_by: String,
    reason: String,
    approved_date: String,
}

impl LangConfig {
    fn is_registered(&self, language: &str) -> bool {
        self.registered.iter().any(|l| l == language)
    }

    fn is_approved(&self, language: &str) -> bool {
        self.elevated_approvals.iter().any(|a| a.language == language)
    }

    fn register(&mut self, language: &str) {
        if !self.is_registered(language) {
            self.registered.push(language.to_string());
        }
    }

    fn unregister(&mut self, language: &str) -> bool {
        let before = self.registered.len();
        self.registered.retain(|l| l != language);
        self.registered.len() < before
    }

    fn approve(&mut self, language: &str, approved_by: &str, reason: &str) {
        self.elevated_approvals.retain(|a| a.language != language);
        self.elevated_approvals.push(ElevatedApproval {
            language: language.to_string(),
            approved_by: approved_by.to_string(),
            reason: reason.to_string(),
            approved_date: chrono::Local::now().format("%Y-%m-%d").to_string(),
        });
    }
}

fn config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".travsr")
        .join("lang.toml")
}

fn load_config() -> Option<LangConfig> {
    let path = config_path();
    let content = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&content).ok()
}

fn save_config(config: &LangConfig) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating ~/.travsr dir")?;
    }
    let content = toml::to_string_pretty(config).context("serialising lang config")?;
    std::fs::write(&path, content).context("writing lang.toml")?;
    Ok(())
}

fn which(name: &str) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|dir| dir.join(name).is_file())
}
