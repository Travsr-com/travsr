//! `travsr lang` subcommands — Phase B language package management.
//!
//! Reads the Phase B catalog from travsr-plugin-host to show what tools
//! exist, then manages registration in ~/.travsr/lang.toml.

use anyhow::{Context as _, Result};
use clap::Subcommand;
use std::path::PathBuf;
use travsr_plugin_host::phase_b::catalog::{
    lookup, SandboxRequirement, ScipBinarySpec, ScipInstall, ZipBinarySpec, CATALOG,
};
use travsr_plugin_host::sandbox::policy::validate_permitted_host;

const APPROVAL_EXPIRY_DAYS: i64 = 365;

#[derive(Debug, Subcommand)]
pub enum LangCommand {
    /// Show all known Phase B language tools and their status.
    List {
        /// Output as a JSON array for programmatic / extension use.
        #[arg(long)]
        json: bool,
    },
    /// Install and register a Phase B tool for a language.
    ///
    /// Downloads the travsr-lang-* wrapper binary from GitHub Releases into
    /// ~/.travsr/bin/ and registers the language for Phase B indexing.
    /// For languages that need network access during indexing (Java, Kotlin,
    /// Scala, C#), an interactive security approval step is included.
    Install {
        /// Canonical language name (e.g. rust, go, python).
        language: String,
        /// Re-download and overwrite the wrapper even if already installed.
        #[arg(long)]
        reinstall: bool,
        /// Skip all interactive prompts — for CI / scripting.
        #[arg(long)]
        no_interactive: bool,
        /// Corpus to activate Phase B for immediately (sets trust grant).
        #[arg(long)]
        corpus: Option<String>,
        /// Skip downloading the wrapper binary; only register in config.
        #[arg(long)]
        skip_wrapper: bool,
        /// Auto-confirm the SCIP tool install command without interactive prompt.
        /// Useful for --no-interactive invocations from the VS Code extension.
        #[arg(long)]
        yes: bool,
        /// Pin the downloaded analysis-tool binary to a specific release tag
        /// (e.g. --version v0.3.0) instead of the latest release. Applies to
        /// tools fetched from GitHub Releases; the travsr-lang wrapper still
        /// tracks latest. Tools with a vendored checksum stay pinned and reject
        /// a mismatching version.
        #[arg(long)]
        version: Option<String>,
    },
    /// Scan the current repo, detect supported languages, and install them.
    Detect,
    /// Register and install a Phase B tool for a language (legacy — prefer `install`).
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
    /// Record security approval for a language that needs network access during indexing.
    /// Must be run before `travsr lang install` for Java, Kotlin, C#, Scala.
    Approve {
        /// Canonical language name (e.g. java, csharp).
        language: String,
        /// GitHub handle of the approver.
        #[arg(long)]
        approved_by: String,
        /// One-sentence justification (recorded in config).
        #[arg(long)]
        reason: String,
        /// Comma-separated list of permitted network hosts.
        /// Example: repo1.maven.org,repo.maven.apache.org,plugins.gradle.org
        #[arg(long, value_delimiter = ',')]
        permitted_hosts: Vec<String>,
    },
}

/// Exit code 2: wrapper installed but underlying SCIP tool missing (partial install).
/// Callers that dispatch install directly should exit(2) on this variant.
#[derive(Debug, PartialEq)]
pub enum InstallStatus {
    FullyReady,
    WrapperOnly,
}

pub fn run(cmd: LangCommand) -> Result<()> {
    match cmd {
        LangCommand::List { json } => cmd_list(json),
        LangCommand::Install {
            language,
            reinstall,
            no_interactive,
            corpus,
            skip_wrapper,
            yes,
            version,
        } => {
            match cmd_install(
                &language,
                reinstall,
                no_interactive,
                corpus.as_deref(),
                skip_wrapper,
                yes,
                version.as_deref(),
            )? {
                InstallStatus::WrapperOnly => std::process::exit(2),
                InstallStatus::FullyReady => Ok(()),
            }
        }
        LangCommand::Detect => cmd_detect(),
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

fn json_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn json_arr(items: &[&str]) -> String {
    let elems: Vec<String> = items.iter().map(|s| json_str(s)).collect();
    format!("[{}]", elems.join(","))
}

fn cmd_list(json: bool) -> Result<()> {
    let config = load_config();
    let today = chrono::Local::now().date_naive();

    if json {
        let mut entries: Vec<String> = Vec::new();
        for entry in CATALOG {
            let sandbox = match entry.sandbox {
                SandboxRequirement::Standard => "Standard",
                SandboxRequirement::NativeIpc => "NativeIpc",
                SandboxRequirement::RequiresElevated => "Elevated",
            };
            let registered = config
                .as_ref()
                .map(|c| c.is_registered(entry.language))
                .unwrap_or(false);
            let approved = config
                .as_ref()
                .map(|c| c.is_approved(entry.language))
                .unwrap_or(false);
            let provider_on_path = entry.provider_binary.map_or(true, tool_available);
            let tool_on_path = tool_available(entry.command);
            let installed = entry.builtin || (provider_on_path && tool_on_path);
            let needs_approval =
                matches!(entry.sandbox, SandboxRequirement::RequiresElevated) && !approved;
            let package = entry.npm_package.unwrap_or(entry.command);
            let scip_type = match entry.scip_install {
                ScipInstall::GithubBinary(_) => "GithubBinary",
                ScipInstall::ZipBinary(_) => "ZipBinary",
                ScipInstall::Command(_) => "Command",
                ScipInstall::Manual => "Manual",
            };
            entries.push(format!(
                r#"{{"language":{},"package":{},"sandbox":{},"installed":{},"registered":{},"builtin":{},"needsApproval":{},"scipInstallType":{},"installHint":{},"underlyingToolHint":{},"elevatedHosts":{}}}"#,
                json_str(entry.language),
                json_str(package),
                json_str(sandbox),
                installed,
                registered,
                entry.builtin,
                needs_approval,
                json_str(scip_type),
                json_str(entry.install_hint),
                json_str(entry.underlying_tool_hint),
                json_arr(entry.elevated_hosts),
            ));
        }
        println!("[{}]", entries.join(",\n"));
        return Ok(());
    }

    println!(
        "{:<12} {:<26} {:<10} STATUS",
        "LANGUAGE", "PACKAGE", "SANDBOX"
    );
    println!("{}", "-".repeat(80));

    for entry in CATALOG {
        let sandbox_label = match entry.sandbox {
            SandboxRequirement::Standard => "Standard",
            SandboxRequirement::NativeIpc => "NativeIpc",
            SandboxRequirement::RequiresElevated => "Elevated",
        };

        let package_col = entry.npm_package.unwrap_or(entry.command);
        let provider_on_path = entry.provider_binary.map_or(true, tool_available);
        let tool_on_path = tool_available(entry.command);
        let fully_ready = entry.builtin || (provider_on_path && tool_on_path);
        let wrapper_only =
            !entry.builtin && provider_on_path && !tool_on_path && entry.provider_binary.is_some();
        let registered = config
            .as_ref()
            .map(|c| c.is_registered(entry.language))
            .unwrap_or(false);
        let approval = config.as_ref().and_then(|c| c.get_approval(entry.language));
        let approved = approval.is_some();
        let sandbox_ok = sandbox_available();

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
            "needs security approval (travsr lang install — run interactively)".to_string()
        } else if wrapper_only {
            let hint = if entry.underlying_tool_hint.is_empty() {
                entry.install_hint
            } else {
                entry.underlying_tool_hint
            };
            format!(
                "wrapper-only  ({} installed, {} missing — {})",
                entry.provider_binary.unwrap(),
                entry.command,
                hint,
            )
        } else if registered && fully_ready && !sandbox_ok && !entry.builtin {
            #[cfg(target_os = "linux")]
            let hint = "install bubblewrap: sudo apt-get install bubblewrap";
            #[cfg(target_os = "macos")]
            let hint = "sandbox-exec unavailable";
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let hint = "sandbox not available on this platform";
            format!("disabled (sandbox unavailable — {hint})")
        } else if registered && fully_ready {
            let phase_b_note = if entry.builtin
                && !entry.native_phase_b
                && !tool_on_path
                && !entry.install_hint.is_empty()
            {
                format!(" (Phase A only — Phase B needs: {})", entry.install_hint)
            } else {
                String::new()
            };
            format!(
                "\u{2713} active{}{}",
                phase_b_note,
                expiry_warning.as_deref().unwrap_or("")
            )
        } else if registered && !fully_ready {
            let missing = if !provider_on_path {
                entry.provider_binary.unwrap_or(entry.command)
            } else {
                entry.command
            };
            format!(
                "registered but {missing} not on PATH — run: travsr lang install {}",
                entry.language
            )
        } else if fully_ready {
            format!(
                "on PATH, not registered — run: travsr lang install {}",
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

// ── install ───────────────────────────────────────────────────────────────────

fn cmd_install(
    language: &str,
    reinstall: bool,
    no_interactive: bool,
    corpus: Option<&str>,
    skip_wrapper: bool,
    yes: bool,
    override_version: Option<&str>,
) -> Result<InstallStatus> {
    let entry = lookup(language).ok_or_else(|| {
        anyhow::anyhow!(
            "Unknown language '{language}'. Run `travsr lang list` to see available languages."
        )
    })?;

    // RequiresElevated: a security approval must be on record before install.
    // ADR-017 Rule 1 is the internal policy behind this check — never surface
    // the ADR name in user output; use plain language instead.
    if entry.sandbox == SandboxRequirement::RequiresElevated {
        let config = load_config();
        let approved = config
            .as_ref()
            .map(|c| c.is_approved(language))
            .unwrap_or(false);

        if !approved {
            let interactive =
                !no_interactive && std::io::IsTerminal::is_terminal(&std::io::stdin());
            if interactive {
                inline_approval_prompt(entry, language)?;
            } else {
                anyhow::bail!(
                    "'{language}' needs network access during indexing to resolve dependencies.\n\
                     A security approval must be recorded before it can be installed.\n\
                     \n\
                     Run interactively (without --no-interactive) to approve inline, or:\n\
                     \n\
                     travsr lang approve {language} \\\n\
                     \t--approved-by <approver-github-handle> \\\n\
                     \t--reason \"<one-sentence justification>\" \\\n\
                     \t--permitted-hosts {}\n\
                     \n\
                     Then re-run: travsr lang install {language}",
                    entry.elevated_hosts.join(",")
                );
            }
        }
    }

    // Download and install the travsr-lang-* wrapper binary.
    let wrapper_installed = match entry.provider_binary {
        None => true, // builtin — no external wrapper needed
        Some(bin) if which(bin) && !reinstall => {
            println!("\u{2713} {bin} already installed.");
            true
        }
        Some(bin) => {
            if skip_wrapper {
                which(bin)
            } else {
                let target =
                    crate::install::current_target().context("determining install target")?;

                // Fetch the latest release version; fall back to catalog default
                // if GitHub is unreachable (e.g. offline / CI without network).
                let version = fetch_version_with_fallback(entry.wrapper_version_fallback);

                println!("Installing {bin} {version}...");

                // Clone into owned values so the async move block is 'static
                // (run_async requires 'static due to thread spawn semantics).
                let v = version.clone();
                let b = bin.to_string();
                let path = run_async(async move {
                    crate::install::download_and_install_wrapper(&v, &b, target).await
                })
                .context("downloading wrapper binary")?;

                println!("\u{2713} {bin} installed to {}", path.display());

                if entry.has_share_assets {
                    let sv = version.clone();
                    let sb = bin.to_string();
                    match run_async(
                        async move { crate::install::install_share_assets(&sv, &sb).await },
                    ) {
                        Ok(()) => println!("\u{2713} {bin} emitter files installed"),
                        Err(e) => println!("warning: could not install {bin} share assets: {e:#}"),
                    }
                }

                if !crate::install::path_contains_travsr_bin() {
                    println!(
                        "\nhint: add ~/.travsr/bin to your PATH:\n\n\
                         \texport PATH=\"$HOME/.travsr/bin:$PATH\"\n\n\
                         Add this line to your ~/.zshrc or ~/.bashrc to make it permanent.\n"
                    );
                }

                true
            }
        }
    };

    // Handle the underlying SCIP tool (scip-go, scip-python, etc.).
    // Builtins are bundled in the travsr binary — no external tool needed.
    let tool_ready = if entry.builtin {
        true
    } else if wrapper_installed && !tool_available(entry.command) {
        let interactive = !no_interactive && std::io::IsTerminal::is_terminal(&std::io::stdin());
        install_scip_tool(entry, interactive, yes, override_version)?
    } else {
        tool_available(entry.command)
    };

    // Register in config and optionally trust a corpus.
    let mut config = load_config().unwrap_or_default();
    config.register(language);
    if let Some(c) = corpus {
        config.trust_corpus(c);
        println!("\u{2713} Semantic indexing enabled for corpus '{c}'.");
    }
    save_config(&config)?;

    // provider_ready: check PATH for builtins; wrapper_installed already accounts
    // for downloaded wrappers (including those just placed in ~/.travsr/bin).
    let provider_ready = match entry.provider_binary {
        None => true,
        Some(bin) => wrapper_installed || which(bin),
    };

    if !provider_ready || !tool_ready {
        println!(
            "\u{26A0} '{language}' is registered but '{}' is not installed yet.\n\
             Deep code analysis will be inactive until it is set up.\n\
             Once the tool is installed, run `travsr init` inside your repository.",
            entry.command
        );
        return Ok(InstallStatus::WrapperOnly);
    }

    println!("\u{2713} '{language}' is ready for deep code analysis.");
    Ok(InstallStatus::FullyReady)
}

/// Attempt to install or hint for the underlying SCIP tool. Returns true if the
/// tool is now on PATH (or was already present), false if still missing.
fn install_scip_tool(
    entry: &travsr_plugin_host::phase_b::catalog::PhaseBEntry,
    interactive: bool,
    yes: bool,
    override_version: Option<&str>,
) -> Result<bool> {
    // --version pins a downloaded GitHub-release asset. Command/Manual installs
    // resolve their own version (the package manager / the user), so an override
    // there would be silently ignored — say so rather than pretend it applied.
    if override_version.is_some()
        && !matches!(
            entry.scip_install,
            ScipInstall::GithubBinary(_) | ScipInstall::ZipBinary(_)
        )
    {
        eprintln!(
            "warning: --version has no effect for '{}': its analysis tool is not \
             installed from GitHub Releases. Ignoring the pin.",
            entry.language
        );
    }
    match entry.scip_install {
        ScipInstall::Command(cmd_args) => {
            let do_run = if interactive {
                use std::io::Write as _;
                print!(
                    "'{}' is not installed.\nInstall via: {}\nRun it now? [Y/n]: ",
                    entry.command,
                    cmd_args.join(" ")
                );
                std::io::stdout().flush()?;
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                answer.trim().is_empty() || answer.trim().eq_ignore_ascii_case("y")
            } else if yes {
                println!("Auto-installing: {}", cmd_args.join(" "));
                true
            } else {
                println!(
                    "Note: '{}' not found. Install it:\n\n\t{}",
                    entry.command,
                    cmd_args.join(" ")
                );
                false
            };
            if do_run {
                let mut child = std::process::Command::new(cmd_args[0])
                    .args(&cmd_args[1..])
                    .spawn()
                    .with_context(|| format!("running '{}'", cmd_args.join(" ")))?;
                let status = child.wait()?;
                if status.success() {
                    println!("\u{2713} {} installed.", entry.command);
                    // Trust exit 0: the binary may be in $GOPATH/bin, ~/.cargo/bin,
                    // or another tool-managed dir not yet in the process's PATH.
                    return Ok(true);
                } else {
                    println!(
                        "Install command exited with {status}.\nRun manually: {}",
                        cmd_args.join(" ")
                    );
                }
            }
        }
        ScipInstall::GithubBinary(ref spec) => {
            install_scip_github_binary(entry, spec, override_version)?;
            return Ok(tool_available(entry.command));
        }
        ScipInstall::ZipBinary(ref spec) => {
            install_zip_binary(entry, spec, override_version)?;
            return Ok(tool_available(entry.command));
        }
        ScipInstall::Manual => {
            if !entry.underlying_tool_hint.is_empty() {
                println!(
                    "'{}' not found.\n\
                     \n\
                     SCIP is an open standard for deep code intelligence (call graphs,\n\
                     type resolution, cross-file references). '{}' is the SCIP indexer\n\
                     for {} that travsr needs for semantic analysis.\n\
                     \n\
                     Install it, then re-run `travsr lang install {} --corpus <your-corpus>`.\n\
                     \n\
                     Docs / install instructions:\n\
                     \t{}",
                    entry.command,
                    entry.command,
                    entry.language,
                    entry.language,
                    entry.underlying_tool_hint
                );
            }
        }
    }
    Ok(false)
}

/// Resolve which release tag to install.
///
/// Precedence: explicit `override_version` → live `releases/latest` →
/// `version_fallback` (the offline floor, used only when the API is down).
///
/// A vendored-hash tool (`pinned == true`, i.e. `sha256_fn: Some`) is locked to
/// its `version_fallback`: the vendored checksum only matches that one asset
/// (#410 M2), so an override asking for a different tag is refused rather than
/// installed unverified. Requesting the pinned version explicitly is allowed.
///
/// `fetch_latest` is only invoked on the live path, so callers pay the network
/// cost only when neither an override nor a pin decides the tag.
fn resolve_install_tag(
    pinned: bool,
    version_fallback: &str,
    override_version: Option<&str>,
    install_name: &str,
    fetch_latest: impl FnOnce() -> anyhow::Result<String>,
) -> Result<String> {
    match override_version {
        Some(v) if pinned && v != version_fallback => anyhow::bail!(
            "'{install_name}' is pinned to {version_fallback} for supply-chain \
             integrity (its checksum is vendored for that exact release); \
             installing a different version ({v}) is not supported."
        ),
        Some(v) => Ok(v.to_string()),
        None if pinned => Ok(version_fallback.to_string()),
        None => Ok(fetch_latest().unwrap_or_else(|e| {
            eprintln!(
                "warning: could not fetch latest {install_name} version ({e:#}), using {version_fallback}"
            );
            version_fallback.to_string()
        })),
    }
}

/// Download a SCIP tool binary from its GitHub releases. Fetches the latest
/// version live by default; `--version` (`override_version`) pins an exact tag
/// and `version_fallback` is the offline floor. See [`resolve_install_tag`].
fn install_scip_github_binary(
    entry: &travsr_plugin_host::phase_b::catalog::PhaseBEntry,
    spec: &ScipBinarySpec,
    override_version: Option<&str>,
) -> Result<()> {
    let target = match crate::install::current_target() {
        Ok(t) => t,
        Err(e) => {
            println!(
                "Cannot determine your platform ({e}).\n\
                 Install '{}' manually:\n\t{}",
                entry.command, entry.underlying_tool_hint
            );
            return Ok(());
        }
    };

    // #410 M2: an entry carrying a vendored hash is pinned, because the hash is
    // only meaningful against one exact asset — a floating `releases/latest` (or
    // a `--version` override) would move the target out from under it. Everything
    // else honors the override, else fetches latest, else the offline fallback.
    let repo = spec.repo.to_string();
    let tag = resolve_install_tag(
        spec.sha256_fn.is_some(),
        spec.version_fallback,
        override_version,
        spec.install_name,
        move || {
            run_async(async move { crate::install::fetch_latest_version_for_repo(&repo).await })
        },
    )?;

    let asset_name = match (spec.asset_fn)(&tag, target) {
        Some(a) => a,
        None => {
            println!(
                "'{}' does not have a pre-built binary for your platform ({target}).\n\
                 Install it manually:\n\t{}",
                entry.command, entry.underlying_tool_hint
            );
            return Ok(());
        }
    };

    println!("Downloading {} {} ...", spec.install_name, tag);

    let repo2 = spec.repo.to_string();
    let tag2 = tag.clone();
    let asset2 = asset_name.clone();
    let name2 = spec.install_name.to_string();
    let verify = spec.verify_sha256;
    // Vendored hash for this exact (tag, target), when the upstream ships no
    // sidecar. `None` leaves the sidecar / TLS-only path unchanged.
    let expected = spec.sha256_fn.and_then(|f| f(&tag, target));

    match run_async(async move {
        crate::install::download_scip_binary(&repo2, &tag2, &asset2, &name2, verify, expected).await
    }) {
        Ok(path) => {
            println!(
                "\u{2713} {} installed to {}",
                spec.install_name,
                path.display()
            );
            if !crate::install::path_contains_travsr_bin() {
                println!(
                    "\nhint: add ~/.travsr/bin to your PATH:\n\n\
                     \texport PATH=\"$HOME/.travsr/bin:$PATH\"\n\n\
                     Add this to your ~/.zshrc or ~/.bashrc to make it permanent.\n"
                );
            }
        }
        Err(e) => {
            println!(
                "Download failed: {e:#}\n\
                 Install '{}' manually:\n\t{}",
                entry.command, entry.underlying_tool_hint
            );
        }
    }

    Ok(())
}

/// Download a GitHub zip release, extract it, and create a wrapper script in
/// `~/.travsr/bin/<spec.install_name>` pointing at the binary inside the archive.
fn install_zip_binary(
    entry: &travsr_plugin_host::phase_b::catalog::PhaseBEntry,
    spec: &ZipBinarySpec,
    override_version: Option<&str>,
) -> Result<()> {
    // #410 M2: same rule as the binary path — an entry carrying a vendored hash
    // is pinned, because the hash only means anything against one exact asset.
    // This was previously wired on the `GithubBinary` path only, which left the
    // single `ZipBinary` entry (kotlin-language-server) reading `releases/latest`
    // and never checking its hash at all.
    let repo = spec.repo.to_string();
    let tag = resolve_install_tag(
        spec.sha256_fn.is_some(),
        spec.version_fallback,
        override_version,
        spec.install_name,
        move || {
            run_async(async move { crate::install::fetch_latest_version_for_repo(&repo).await })
        },
    )?;

    let asset_name = (spec.asset_fn)(&tag);
    println!("Downloading {} {} ...", spec.install_name, tag);

    let repo2 = spec.repo.to_string();
    let tag2 = tag.clone();
    let asset2 = asset_name.clone();
    let extract_dir = spec.extract_dir.to_string();
    let expected = spec.sha256_fn.and_then(|f| f(&tag)).map(str::to_string);

    let extract_path = match run_async(async move {
        crate::install::download_zip_and_extract(
            &repo2,
            &tag2,
            &asset2,
            &extract_dir,
            expected.as_deref(),
        )
        .await
    }) {
        Ok(p) => p,
        Err(e) => {
            println!(
                "Download failed: {e:#}\nInstall '{}' manually:\n\t{}",
                entry.command, entry.underlying_tool_hint
            );
            return Ok(());
        }
    };

    // Write wrapper script: #!/bin/sh\nexec <binary_abs> "$@"\n
    let bin_dir = crate::install::travsr_bin_dir()?;
    let wrapper = bin_dir.join(spec.install_name);
    let binary_abs = extract_path.join(spec.binary_subpath);
    let script = format!("#!/bin/sh\nexec {} \"$@\"\n", binary_abs.display());
    std::fs::write(&wrapper, &script)
        .with_context(|| format!("writing wrapper to {}", wrapper.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
            .context("chmod +x wrapper")?;
    }

    println!(
        "\u{2713} {} installed to {}",
        spec.install_name,
        wrapper.display()
    );
    if !crate::install::path_contains_travsr_bin() {
        println!(
            "\nhint: add ~/.travsr/bin to your PATH:\n\n\
             \texport PATH=\"$HOME/.travsr/bin:$PATH\"\n\n\
             Add this to your ~/.zshrc or ~/.bashrc to make it permanent.\n"
        );
    }

    Ok(())
}

// ── detect ────────────────────────────────────────────────────────────────────

fn cmd_detect() -> Result<()> {
    use std::io::IsTerminal as _;

    let cwd = std::env::current_dir().context("getting current directory")?;
    // L5: `travsr lang detect` should operate from the repo root, not an
    // arbitrary subdirectory, so it scans the full project.
    let repo_root = crate::repo::find_git_root(&cwd).unwrap_or(cwd);
    let found = detect_languages_in(&repo_root);

    if found.is_empty() {
        println!("No supported languages detected in the current directory.");
        return Ok(());
    }

    let config = load_config();

    println!("Detected languages in this repo:\n");
    for (i, lang) in found.iter().enumerate() {
        let entry = lookup(lang).expect("lang came from CATALOG");
        let registered = config
            .as_ref()
            .map(|c| c.is_registered(lang))
            .unwrap_or(false);
        let provider_ready = entry.provider_binary.map_or(true, tool_available);
        let fully_ready = entry.builtin || (provider_ready && tool_available(entry.command));

        let status = if registered && fully_ready {
            "\u{2713} already active"
        } else if registered {
            "registered (SCIP tool missing)"
        } else {
            "not installed"
        };

        println!(
            "  [{}] {}  ({})  — {}",
            i + 1,
            lang,
            entry.extensions.join(", "),
            status
        );
    }
    println!();

    if !std::io::stdin().is_terminal() {
        println!("(non-interactive — run `travsr lang install <lang>` to install individually)");
        return Ok(());
    }

    use std::io::Write as _;
    print!("Which to install? (numbers separated by commas, 'a' for all, 'q' to quit): ");
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim();

    if input.eq_ignore_ascii_case("q") || input.is_empty() {
        return Ok(());
    }

    let selected: Vec<&str> = if input.eq_ignore_ascii_case("a") {
        found.iter().map(|s| s.as_str()).collect()
    } else {
        let mut sel = Vec::new();
        for part in input.split(',') {
            let part = part.trim();
            match part.parse::<usize>() {
                Ok(n) if n >= 1 && n <= found.len() => sel.push(found[n - 1].as_str()),
                _ => println!("  skipping unrecognised selection: '{part}'"),
            }
        }
        sel
    };

    if selected.is_empty() {
        println!("Nothing selected.");
        return Ok(());
    }

    println!();
    for lang in &selected {
        println!("── {} ──", lang);
        match cmd_install(lang, false, false, None, false, false, None) {
            Ok(InstallStatus::FullyReady) => {}
            Ok(InstallStatus::WrapperOnly) => {
                println!("  \u{26A0} {lang}: wrapper installed but SCIP tool missing")
            }
            Err(e) => eprintln!("  error: {e:#}"),
        }
        println!();
    }

    Ok(())
}

// ── add (legacy) ──────────────────────────────────────────────────────────────

fn cmd_add(language: &str, corpus: Option<&str>) -> Result<()> {
    let entry = lookup(language).ok_or_else(|| {
        anyhow::anyhow!(
            "Unknown language '{language}'. Run `travsr lang list` to see available languages."
        )
    })?;

    // RequiresElevated: must be approved first.
    // ADR-017 Rule 1 is the internal policy behind this check — the user-facing
    // message uses plain language instead.
    if entry.sandbox == SandboxRequirement::RequiresElevated {
        let config = load_config();
        let approved = config
            .as_ref()
            .map(|c| c.is_approved(language))
            .unwrap_or(false);
        if !approved {
            anyhow::bail!(
                "'{language}' needs network access during indexing to resolve dependencies.\n\
                 {}\n\
                 \n\
                 Record security approval first:\n\
                 \n\
                 travsr lang approve {language} \\\n\
                 \t--approved-by <approver-github-handle> \\\n\
                 \t--reason \"<one-sentence justification>\" \\\n\
                 \t--permitted-hosts repo1.maven.org,repo.maven.apache.org\n\
                 \n\
                 Then re-run: travsr lang add {language}\n\
                 \n\
                 Alternatively, use `travsr lang install {language}` which handles approval interactively.",
                entry.install_hint
            );
        }
    }

    let wrapper_installed = match entry.provider_binary {
        None => true,
        Some(bin) if which(bin) => true,
        Some(_) => {
            if let Some(pkg) = entry.npm_package {
                println!("Installing {pkg} via npm...");
                // #502: on Windows npm is npm.cmd, which a bare Command::new
                // never finds (CreateProcessW only appends .exe). Resolve the
                // real path; std spawns .cmd via cmd.exe with strict escaping.
                let npm = travsr_core::exec::resolve_executable("npm")
                    .map(|p| p.into_os_string())
                    .unwrap_or_else(|| "npm".into());
                match std::process::Command::new(npm)
                    .args(["install", "-g", pkg])
                    .status()
                {
                    Ok(s) if s.success() => {
                        println!("\u{2713} {pkg} installed.");
                        true
                    }
                    Ok(s) => {
                        println!(
                            "warning: npm install exited with {s}.\n\
                             Install manually: {}",
                            entry.install_hint
                        );
                        false
                    }
                    Err(e) => {
                        println!(
                            "warning: could not run npm ({e}).\n\
                             Install manually: {}",
                            entry.install_hint
                        );
                        false
                    }
                }
            } else {
                println!(
                    "warning: '{}' is not on PATH.\n\
                     Install manually: {}",
                    entry.provider_binary.unwrap_or(entry.command),
                    entry.install_hint
                );
                false
            }
        }
    };

    if wrapper_installed && !which(entry.command) && !entry.underlying_tool_hint.is_empty() {
        println!(
            "warning: {} not found on PATH.\n\
             Install it: {}\n\
             Phase B for '{}' will be inactive until {} is installed.",
            entry.command, entry.underlying_tool_hint, entry.language, entry.command,
        );
    }

    let mut config = load_config().unwrap_or_default();
    config.register(language);

    if let Some(c) = corpus {
        config.trust_corpus(c);
        println!("\u{2713} Semantic indexing enabled for corpus '{c}'.");
    }

    save_config(&config)?;

    println!(
        "\u{2713} '{language}' registered for semantic indexing.\n\
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
             Run `travsr lang install {language}` directly."
        );
    }

    anyhow::ensure!(!approved_by.is_empty(), "--approved-by must not be empty");
    anyhow::ensure!(!reason.is_empty(), "--reason must not be empty");
    anyhow::ensure!(
        !permitted_hosts.is_empty(),
        "--permitted-hosts must not be empty for languages that need network access.\n\
         Example: --permitted-hosts repo1.maven.org,repo.maven.apache.org,plugins.gradle.org"
    );
    // ADR-017 Rule 1: explicit allowlist only — no wildcards, no CIDR ranges.
    for host in &permitted_hosts {
        validate_permitted_host(host)
            .map_err(|e| anyhow::anyhow!("invalid --permitted-hosts entry '{host}': {e}"))?;
    }

    let mut config = load_config().unwrap_or_default();
    config.approve(language, approved_by, reason, permitted_hosts.clone());
    save_config(&config)?;

    println!(
        "\u{2713} Security approval recorded for '{language}'.\n\
         Permitted hosts: {}\n\
         note: the sandbox does not filter traffic per host — this allowlist is\n\
         only enforced if a host-level firewall or egress proxy backs it.\n\
         Run `travsr lang install {language}` to complete installation.",
        permitted_hosts.join(", ")
    );
    Ok(())
}

// ── interactive approval prompt ───────────────────────────────────────────────

/// Interactively collect security approval for a RequiresElevated language.
/// Called from cmd_install when stdin is a TTY and approval is not yet on record.
///
/// User-facing strings use plain language — no "ADR-017" in output.
/// The ADR-017 Rule 1 policy is explained in code comments for developers.
fn inline_approval_prompt(
    entry: &travsr_plugin_host::phase_b::catalog::PhaseBEntry,
    language: &str,
) -> Result<()> {
    use std::io::Write as _;

    println!(
        "'{language}' needs network access during indexing to resolve dependencies.\n\
         A security approval must be recorded before it can be enabled.\n\
         \n\
         Network hosts that will be contacted:\n{}",
        entry
            .elevated_hosts
            .iter()
            .map(|h| format!("  - {h}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    println!();

    let approved_by = prompt_field("Approver's GitHub handle: ")?;
    let reason = prompt_field("One-sentence justification: ")?;

    let default_hosts = entry.elevated_hosts.join(",");
    print!("Permitted hosts (comma-separated) [{default_hosts}]: ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let hosts_str = answer.trim();
    let permitted_hosts: Vec<String> = if hosts_str.is_empty() {
        entry.elevated_hosts.iter().map(|s| s.to_string()).collect()
    } else {
        hosts_str.split(',').map(|s| s.trim().to_string()).collect()
    };

    // ADR-017 Rule 1: each host must be a plain FQDN — no wildcards, no CIDR.
    for host in &permitted_hosts {
        validate_permitted_host(host).map_err(|e| anyhow::anyhow!("invalid host '{host}': {e}"))?;
    }

    let mut config = load_config().unwrap_or_default();
    config.approve(language, &approved_by, &reason, permitted_hosts.clone());
    save_config(&config)?;

    println!(
        "\u{2713} Approval recorded. Permitted hosts: {}\n\
         note: the sandbox does not filter traffic per host — this allowlist is\n\
         only enforced if a host-level firewall or egress proxy backs it.\n",
        permitted_hosts.join(", ")
    );
    Ok(())
}

fn prompt_field(prompt: &str) -> Result<String> {
    use std::io::Write as _;
    loop {
        print!("{prompt}");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        let val = answer.trim().to_string();
        if !val.is_empty() {
            return Ok(val);
        }
        println!("  (cannot be empty, please try again)");
    }
}

// ── language detection ────────────────────────────────────────────────────────

/// Walk `dir` and return catalog language names whose file extensions appear
/// in the tree. Skips .git, node_modules, target, build, dist, .cache.
/// Returns languages in catalog order for stable output.
pub(crate) fn detect_languages_in(dir: &std::path::Path) -> Vec<String> {
    const SKIP_DIRS: &[&str] = &[
        ".git",
        "node_modules",
        "target",
        "build",
        "dist",
        ".cache",
        ".next",
        "__pycache__",
    ];

    let mut found = std::collections::HashSet::new();

    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !SKIP_DIRS.iter().any(|d| *d == name.as_ref())
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        if let Some(ext) = entry.path().extension().and_then(|e| e.to_str()) {
            let ext_dot = format!(".{ext}");
            for catalog_entry in CATALOG {
                if catalog_entry.extensions.contains(&ext_dot.as_str()) {
                    found.insert(catalog_entry.language);
                }
            }
        }
    }

    CATALOG
        .iter()
        .filter(|e| found.contains(e.language))
        .map(|e| e.language.to_string())
        .collect()
}

/// Public accessor so init.rs can check which detected languages are unregistered.
pub(crate) fn load_lang_config() -> Option<LangConfig> {
    load_config()
}

// ── async helper ──────────────────────────────────────────────────────────────

/// Run an async future on a fresh single-thread runtime in a scoped thread.
///
/// lang::run() is a sync fn called from inside #[tokio::main(flavor = "current_thread")].
/// Handle::current().block_on() would panic on a current-thread runtime (cannot block
/// the thread driving it). Spawning a scoped thread with its own Runtime avoids this.
pub(crate) fn run_async<F, T>(fut: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    std::thread::scope(|s| -> Result<T> {
        s.spawn(|| -> Result<T> {
            tokio::runtime::Runtime::new()
                .context("creating tokio runtime")?
                .block_on(fut)
        })
        .join()
        .map_err(|_| anyhow::anyhow!("download thread panicked"))?
    })
}

fn fetch_version_with_fallback(fallback: &str) -> String {
    match run_async(crate::install::fetch_latest_version()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("warning: could not fetch latest version ({e:#}), using {fallback}");
            fallback.to_string()
        }
    }
}

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct LangConfig {
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
    /// Explicit permitted network hosts.
    // ADR-017 Rule 1: explicit allowlist only — stored verbatim from user input
    // after validation by validate_permitted_host().
    permitted_hosts: Vec<String>,
    /// ISO-8601 date. Re-review required after 12 months.
    approved_date: String,
}

impl LangConfig {
    pub(crate) fn is_registered(&self, language: &str) -> bool {
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
    // TRAVSR_LANG_TOML overrides the path — used in tests to avoid reading the
    // real ~/.travsr/lang.toml (mirrors the same override in resolver.rs and trust.rs).
    if let Ok(p) = std::env::var("TRAVSR_LANG_TOML") {
        return PathBuf::from(p);
    }
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

/// #502: PATHEXT-aware — on Windows tools are `scip-go.exe` / `npm.cmd`,
/// which a bare-name `dir.join(name).is_file()` probe never finds.
fn which(name: &str) -> bool {
    travsr_core::exec::resolve_executable(name).is_some()
}

/// Like `which`, but also checks tool-managed directories not always in PATH:
/// - $GOBIN (explicit Go binary dir)
/// - $GOPATH/bin (defaults to ~/go/bin when GOPATH is unset)
/// - ~/.cargo/bin (Rust toolchain binaries)
///
/// Needed because `go install`, `rustup component add`, etc. write into their
/// own directories which the current process may not have inherited in PATH.
fn tool_available(name: &str) -> bool {
    if which(name) {
        return true;
    }
    // #502: the extra dirs get the same PATHEXT-aware probe as PATH.
    let mut extra_dirs: Vec<std::path::PathBuf> = Vec::new();
    // $GOBIN takes precedence over $GOPATH/bin per go toolchain semantics.
    if let Some(gobin) = std::env::var_os("GOBIN") {
        extra_dirs.push(std::path::PathBuf::from(gobin));
    }
    let gopath = std::env::var_os("GOPATH")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join("go")));
    if let Some(gp) = gopath {
        extra_dirs.push(gp.join("bin"));
    }
    if let Some(home) = dirs::home_dir() {
        extra_dirs.push(home.join(".cargo").join("bin"));
        // ~/.travsr/bin — travsr's own managed directory for downloaded scip-* binaries.
        extra_dirs.push(home.join(".travsr").join("bin"));
        // ~/.dotnet/tools — dotnet global tool install location (`dotnet tool install --global`).
        extra_dirs.push(home.join(".dotnet").join("tools"));
    }
    travsr_core::exec::resolve_executable_in(extra_dirs, name).is_some()
}

fn sandbox_available() -> bool {
    #[cfg(target_os = "linux")]
    return which("bwrap");
    #[cfg(target_os = "macos")]
    return std::path::Path::new("/usr/bin/sandbox-exec").exists();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return false;
}

#[cfg(test)]
mod tests {
    use super::resolve_install_tag;

    // A `fetch_latest` that must never run — asserts the live path is skipped
    // when an override or a pin already decides the tag.
    fn never_latest() -> anyhow::Result<String> {
        panic!("fetch_latest must not be called when the tag is already decided");
    }

    #[test]
    fn override_wins_for_unpinned_tool() {
        let tag = resolve_install_tag(false, "v0.3.0", Some("v0.2.1"), "t", never_latest).unwrap();
        assert_eq!(tag, "v0.2.1");
    }

    #[test]
    fn override_matching_pin_is_allowed() {
        // Requesting exactly the pinned version is a no-op, not a refusal.
        let tag = resolve_install_tag(true, "v1.0.0", Some("v1.0.0"), "t", never_latest).unwrap();
        assert_eq!(tag, "v1.0.0");
    }

    #[test]
    fn override_mismatch_on_pinned_tool_is_refused() {
        // A vendored-hash tool must never be floated off its pinned tag (#410 M2).
        let err = resolve_install_tag(true, "v1.0.0", Some("v2.0.0"), "kls", never_latest)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("pinned"),
            "message should explain the pin: {err}"
        );
        assert!(
            err.contains("v2.0.0"),
            "message should name the rejected version: {err}"
        );
    }

    #[test]
    fn no_override_pinned_uses_fallback_without_network() {
        let tag = resolve_install_tag(true, "v1.0.0", None, "t", never_latest).unwrap();
        assert_eq!(tag, "v1.0.0");
    }

    #[test]
    fn no_override_unpinned_uses_latest() {
        let tag =
            resolve_install_tag(false, "v0.3.0", None, "t", || Ok("v0.9.0".to_string())).unwrap();
        assert_eq!(tag, "v0.9.0");
    }

    #[test]
    fn no_override_unpinned_falls_back_when_latest_fails() {
        let tag = resolve_install_tag(false, "v0.3.0", None, "t", || anyhow::bail!("network down"))
            .unwrap();
        assert_eq!(tag, "v0.3.0");
    }
}
