//! `travsr lang` subcommands — Phase B language package management.
//!
//! Reads the Phase B catalog from travsr-plugin-host to show what tools
//! exist, then manages registration in ~/.travsr/lang.toml.

use anyhow::{Context as _, Result};
use clap::Subcommand;
use std::path::PathBuf;
use travsr_plugin_host::phase_b::catalog::{lookup, SandboxRequirement, CATALOG};

const APPROVAL_EXPIRY_DAYS: i64 = 365;

#[derive(Debug, Subcommand)]
pub enum LangCommand {
    /// Show all known Phase B language tools and their status.
    List,
    /// Register and install a Phase B tool for a language.
    Add {
        /// Canonical language name (e.g. rust, java, php).
        language: String,
        /// Corpus to activate Phase B for immediately (sets trust grant).
        #[arg(long)]
        corpus: Option<String>,
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
        /// Comma-separated list of permitted network hosts (ADR-017 Rule 1).
        /// Example: repo1.maven.org,repo.maven.apache.org,plugins.gradle.org
        #[arg(long, value_delimiter = ',')]
        permitted_hosts: Vec<String>,
    },
}

pub fn run(cmd: LangCommand) -> Result<()> {
    match cmd {
        LangCommand::List => cmd_list(),
        LangCommand::Add { language, corpus } => cmd_add(&language, corpus.as_deref()),
        LangCommand::Remove { language } => cmd_remove(&language),
        LangCommand::Approve {
            language,
            approved_by,
            reason,
            permitted_hosts,
        } => cmd_approve(&language, &approved_by, &reason, permitted_hosts),
    }
}

// ── list ──────────────────────────────────────────────────────────────────────

fn cmd_list() -> Result<()> {
    let config = load_config();
    let today = chrono::Local::now().date_naive();

    println!("{:<12} {:<26} {:<10} STATUS", "LANGUAGE", "PACKAGE", "SANDBOX");
    println!("{}", "-".repeat(80));

    for entry in CATALOG {
        let sandbox_label = match entry.sandbox {
            SandboxRequirement::Standard => "Standard",
            SandboxRequirement::RequiresElevated => "Elevated",
        };

        let package_col = entry.npm_package.unwrap_or(entry.command);
        let on_path = which(entry.command);
        let registered = config
            .as_ref()
            .map(|c| c.is_registered(entry.language))
            .unwrap_or(false);
        let approval = config.as_ref().and_then(|c| c.get_approval(entry.language));
        let approved = approval.is_some();
        let sandbox_ok = sandbox_available();

        // Check approval expiry
        let expiry_warning = if let Some(appr) = &approval {
            if let Ok(date) = chrono::NaiveDate::parse_from_str(&appr.approved_date, "%Y-%m-%d") {
                let age = (today - date).num_days();
                if age > APPROVAL_EXPIRY_DAYS {
                    Some(format!(
                        " ⚠ approval expired ({age} days ago — re-run travsr lang approve)"
                    ))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        let status = if entry.sandbox == SandboxRequirement::RequiresElevated && !approved {
            "needs PSE approval (travsr lang approve)".to_string()
        } else if registered && on_path && !sandbox_ok {
            #[cfg(target_os = "linux")]
            let hint = "install bubblewrap: sudo apt-get install bubblewrap";
            #[cfg(target_os = "macos")]
            let hint = "sandbox-exec unavailable";
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let hint = "sandbox not available on this platform";
            format!("disabled (sandbox unavailable — {hint})")
        } else if registered && on_path {
            let base = "\u{2713} active".to_string();
            format!("{}{}", base, expiry_warning.as_deref().unwrap_or(""))
        } else if registered && !on_path {
            format!(
                "registered but {} not on PATH — run: travsr lang add {}",
                entry.command, entry.language
            )
        } else if on_path {
            format!(
                "on PATH, not registered — run: travsr lang add {}",
                entry.language
            )
        } else {
            format!("not installed — {}", entry.install_hint)
        };

        println!(
            "{:<12} {:<26} {:<10} {}",
            entry.language, package_col, sandbox_label, status
        );
    }
    Ok(())
}

// ── add ───────────────────────────────────────────────────────────────────────

fn cmd_add(language: &str, corpus: Option<&str>) -> Result<()> {
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
                 \t--reason \"<one-sentence justification>\" \\\n\
                 \t--permitted-hosts repo1.maven.org,repo.maven.apache.org\n\
                 \n\
                 Then re-run: travsr lang add {language}",
                entry.install_hint
            );
        }
    }

    // Try to install via npm if tool not on PATH
    if !which(entry.command) {
        if let Some(pkg) = entry.npm_package {
            println!("Installing {pkg} via npm...");
            match std::process::Command::new("npm")
                .args(["install", "-g", pkg])
                .status()
            {
                Ok(s) if s.success() => {
                    println!("\u{2713} {pkg} installed successfully.");
                }
                Ok(s) => {
                    println!(
                        "Warning: npm install exited with {s}.\n\
                         Install manually: {}",
                        entry.install_hint
                    );
                }
                Err(e) => {
                    println!(
                        "Warning: could not run npm ({e}).\n\
                         Install manually: {}",
                        entry.install_hint
                    );
                }
            }
        } else {
            println!(
                "Warning: '{}' is not on PATH.\n\
                 Install manually: {}",
                entry.command, entry.install_hint
            );
        }
    }

    // Register in config
    let mut config = load_config().unwrap_or_default();
    config.register(language);

    // If --corpus provided, record trust grant
    if let Some(c) = corpus {
        config.trust_corpus(c);
        println!("\u{2713} Phase B trust granted for corpus '{c}'.");
    }

    save_config(&config)?;

    println!(
        "\u{2713} '{language}' Phase B registered.\n\
         {}",
        if corpus.is_none() {
            format!(
                "To activate for a repository:\n\n    travsr lang add {language} --corpus <your-corpus>"
            )
        } else {
            String::new()
        }
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

fn cmd_approve(
    language: &str,
    approved_by: &str,
    reason: &str,
    permitted_hosts: Vec<String>,
) -> Result<()> {
    let entry =
        lookup(language).ok_or_else(|| anyhow::anyhow!("Unknown language '{language}'."))?;

    if entry.sandbox != SandboxRequirement::RequiresElevated {
        anyhow::bail!(
            "'{language}' uses Standard sandbox — no approval needed. \
             Run `travsr lang add {language}` directly."
        );
    }

    anyhow::ensure!(!approved_by.is_empty(), "--approved-by must not be empty");
    anyhow::ensure!(!reason.is_empty(), "--reason must not be empty");
    anyhow::ensure!(
        !permitted_hosts.is_empty(),
        "--permitted-hosts must not be empty for RequiresElevated languages (ADR-017 Rule 1).\n\
         Example: --permitted-hosts repo1.maven.org,repo.maven.apache.org,plugins.gradle.org"
    );

    let mut config = load_config().unwrap_or_default();
    config.approve(language, approved_by, reason, permitted_hosts.clone());
    save_config(&config)?;

    println!(
        "\u{2713} PSE approval recorded for '{language}'.\n\
         Permitted hosts: {}\n\
         Run `travsr lang add {language}` to complete registration.",
        permitted_hosts.join(", ")
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
    /// Corpora trusted for Phase B (set via --corpus or travsr config set).
    #[serde(default)]
    trusted_corpora: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ElevatedApproval {
    language: String,
    approved_by: String,
    reason: String,
    /// Comma-separated permitted network hosts (ADR-017 Rule 1).
    permitted_hosts: Vec<String>,
    /// ISO-8601 date. Re-review required after 12 months.
    approved_date: String,
}

impl LangConfig {
    fn is_registered(&self, language: &str) -> bool {
        self.registered.iter().any(|l| l == language)
    }

    fn is_approved(&self, language: &str) -> bool {
        self.elevated_approvals
            .iter()
            .any(|a| a.language == language)
    }

    fn get_approval(&self, language: &str) -> Option<&ElevatedApproval> {
        self.elevated_approvals
            .iter()
            .find(|a| a.language == language)
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

    fn approve(
        &mut self,
        language: &str,
        approved_by: &str,
        reason: &str,
        permitted_hosts: Vec<String>,
    ) {
        self.elevated_approvals.retain(|a| a.language != language);
        self.elevated_approvals.push(ElevatedApproval {
            language: language.to_string(),
            approved_by: approved_by.to_string(),
            reason: reason.to_string(),
            permitted_hosts,
            approved_date: chrono::Local::now().format("%Y-%m-%d").to_string(),
        });
    }

    fn trust_corpus(&mut self, corpus: &str) {
        if !self.trusted_corpora.iter().any(|c| c == corpus) {
            self.trusted_corpora.push(corpus.to_string());
        }
    }
}

fn config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".travsr")
        .join("lang.toml")
}

fn load_config() -> Option<LangConfig> {
    let content = std::fs::read_to_string(config_path()).ok()?;
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

fn sandbox_available() -> bool {
    #[cfg(target_os = "linux")]
    return which("bwrap");
    #[cfg(target_os = "macos")]
    return std::path::Path::new("/usr/bin/sandbox-exec").exists();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return false;
}
