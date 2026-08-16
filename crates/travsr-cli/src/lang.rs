//! `travsr lang` subcommands — Phase B language package management.
//!
//! Reads the Phase B catalog from travsr-plugin-host to show what tools
//! exist, then manages registration in ~/.travsr/lang.toml.

use anyhow::{Context as _, Result};
use clap::Subcommand;
use std::path::PathBuf;
use travsr_plugin_host::phase_b::catalog::{
    lookup, PhaseBEntry, SandboxRequirement, ScipBinarySpec, ScipInstall, ZipBinarySpec, CATALOG,
};
use travsr_plugin_host::sandbox::policy::validate_permitted_host;

const APPROVAL_EXPIRY_DAYS: i64 = 365;

#[derive(Debug, Subcommand)]
pub enum LangCommand {
    /// Show all known Phase B language tools and their status.
    ///
    /// #727: aliased to `status` because every other area of the CLI spells this
    /// `status` (`travsr status`, `daemon status`, `embed status`, `rerank
    /// status`). `lang` was the only one that did not, which is why the project
    /// docs told agents to run `travsr lang status` and got an error.
    #[command(visible_alias = "status")]
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
        /// Advanced: enable semantic analysis for a specific repository identity
        /// instead of the current one (auto-detected). Rarely needed.
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
        /// Advanced: enable semantic analysis for a specific repository identity
        /// instead of the current one (auto-detected). Rarely needed.
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

// ── platform availability (#588) ──────────────────────────────────────────────

/// The host target triple this wrapper's release asset would be named for, or
/// `None` on a platform travsr has no triple for.
fn host_target() -> Option<&'static str> {
    crate::install::current_target().ok()
}

/// The host target when this entry needs a travsr-lang wrapper that the release
/// matrix does not publish for it, else `None`.
///
/// #588: `current_target()` returning a triple was being read as "the wrapper
/// exists for this triple". It does not follow — Windows had a triple and zero
/// published assets — so every `lang install` on Windows offered a setup flow
/// that ended in a raw 404. Anything that offers, lists or performs a wrapper
/// install asks this first and states the limitation instead.
/// Reported only when the wrapper is not already present: a user who built one
/// themselves and put it on PATH has a working setup, and telling them it is
/// unavailable would be its own false statement. The gate is about what can be
/// *downloaded*, not about what can run.
pub(crate) fn wrapper_unavailable_target(entry: &PhaseBEntry) -> Option<&'static str> {
    let bin = entry.provider_binary?;
    let target = host_target()?;
    if crate::install::wrapper_available(bin, target) || tool_available(bin) {
        return None;
    }
    Some(target)
}

/// The line shown wherever an unavailable language surfaces: the honest
/// capability statement plus whatever manual path the catalog records.
fn unavailable_status(entry: &PhaseBEntry, target: &str) -> String {
    let hint = if entry.underlying_tool_hint.is_empty() {
        String::new()
    } else {
        format!(" — manual setup: {}", entry.underlying_tool_hint)
    };
    format!(
        "not available on {target} yet ({} ships no prebuilt binary for this platform){hint}",
        entry.provider_binary.unwrap_or(entry.command)
    )
}

// ── list ──────────────────────────────────────────────────────────────────────

/// Whether full cross-file semantic can actually run for `entry` on this machine:
/// the analyzer is present (bundled, or its external binary resolves) AND the
/// language is enabled (built in, or registered for indexing). One rule for every
/// language — nothing is special-cased, so `lang list` and `lang detect` can never
/// disagree again.
fn analyzer_ready(entry: &PhaseBEntry, registered: bool) -> bool {
    let enabled = entry.builtin || registered;
    let present = if entry.analyzer_bundled() {
        true
    } else {
        entry.provider_binary.map_or(true, tool_available) && tool_available(entry.command)
    };
    enabled && present
}

/// The capability-view status for one language, shared by `lang list` (text and
/// JSON) and `lang detect`. Store-independent: the repo-level "did we actually
/// produce edges here" answer lives in `travsr status`, in the same vocabulary.
fn lang_capability_status(
    entry: &PhaseBEntry,
    registered: bool,
    approved: bool,
) -> travsr_plugin_host::phase_b::status::LangStatus {
    travsr_plugin_host::phase_b::status::capability(
        &travsr_plugin_host::phase_b::status::Capability {
            entry,
            analyzer_ready: analyzer_ready(entry, registered),
            approved,
            unsupported_on: wrapper_unavailable_target(entry)
                .map(|_| travsr_plugin_host::phase_b::status::os_label().to_string()),
        },
    )
}

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
            // Honest: the analyzer is installed when it is bundled (python) or its
            // external binary resolves — never just because a language is built in.
            let installed = entry.analyzer_bundled() || (provider_on_path && tool_on_path);
            let needs_approval =
                matches!(entry.sandbox, SandboxRequirement::RequiresElevated) && !approved;
            let package = entry.npm_package.unwrap_or(entry.command);
            let scip_type = match entry.scip_install {
                ScipInstall::GithubBinary(_) => "GithubBinary",
                ScipInstall::ZipBinary(_) => "ZipBinary",
                ScipInstall::Command(_) => "Command",
                ScipInstall::Manual => "Manual",
            };
            // #588: `installed:false` alone cannot distinguish "run the install
            // command" from "no build exists for this platform". Consumers that
            // surface an install prompt (the VS Code extension included) need
            // that difference, so it is stated rather than implied.
            let unavailable_on = wrapper_unavailable_target(entry);
            // The authoritative status every consumer renders. `status` is a stable
            // machine tag; `statusLine` is the exact human wording used in the CLI,
            // so the extension shows the same words without re-deriving them.
            let status = lang_capability_status(entry, registered, approved);
            entries.push(format!(
                r#"{{"language":{},"package":{},"sandbox":{},"status":{},"statusLine":{},"installed":{},"registered":{},"builtin":{},"needsApproval":{},"scipInstallType":{},"installHint":{},"underlyingToolHint":{},"elevatedHosts":{},"availableOnThisPlatform":{},"unavailableTarget":{}}}"#,
                json_str(entry.language),
                json_str(package),
                json_str(sandbox),
                json_str(status.tag()),
                json_str(&status.line()),
                installed,
                registered,
                entry.builtin,
                needs_approval,
                json_str(scip_type),
                json_str(entry.install_hint),
                json_str(entry.underlying_tool_hint),
                json_arr(entry.elevated_hosts),
                unavailable_on.is_none(),
                unavailable_on.map_or("null".to_string(), json_str),
            ));
        }
        println!("[{}]", entries.join(",\n"));
        return Ok(());
    }

    println!("{:<12} STATUS", "LANGUAGE");
    println!("{}", "-".repeat(48));

    for entry in CATALOG {
        let registered = config
            .as_ref()
            .map(|c| c.is_registered(entry.language))
            .unwrap_or(false);
        let approval = config.as_ref().and_then(|c| c.get_approval(entry.language));

        // One computed status for every language — the same call `lang detect` and
        // the JSON branch make, so the three can never drift apart again.
        let status = lang_capability_status(entry, registered, approval.is_some());

        // A recorded network approval that has aged past expiry is surfaced in
        // plain words on top of the state — no symbols.
        let expiry = approval
            .as_ref()
            .and_then(|a| chrono::NaiveDate::parse_from_str(&a.approved_date, "%Y-%m-%d").ok())
            .filter(|d| (today - *d).num_days() > APPROVAL_EXPIRY_DAYS)
            .map(|_| {
                format!(
                    "  (approval expired, run: travsr lang install {})",
                    entry.language
                )
            })
            .unwrap_or_default();

        println!("{:<12} {}{}", entry.language, status.line(), expiry);
    }

    // RFC-025 §8: sidecar version health for the installed Phase B tools
    // (installed vs required vs latest), with the exact remedy. Text output only
    // — the JSON branch returned above.
    println!();
    crate::sidecar_health::print_block();

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

    // #588: state the platform gap before anything else — before the approval
    // prompt, before registering the language, before any network work. Nothing
    // downstream can succeed, and the previous behaviour (walk the whole flow,
    // then fail on a 404 from an asset that was never published) told the user
    // nothing about why.
    if let Some(target) = wrapper_unavailable_target(entry) {
        anyhow::bail!(
            "'{language}' is {}\n\
             \n\
             Structural indexing (symbols, definitions, repo map) still works on \
             this platform — only call/reference analysis needs this binary.",
            unavailable_status(entry, target)
        );
    }

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
            println!("{bin} already installed.");
            // RFC-025 Point B: presence never re-checks the release the wrapper
            // was pinned to. Surface a below-floor WARN (offline) and a newer-
            // release advisory (best-effort) over the installed wrapper. Never
            // fails install.
            if let Some(path) = travsr_core::exec::resolve_executable(bin) {
                crate::install::advise_installed_sidecar(
                    entry,
                    &path,
                    &format!("travsr lang install {language} --reinstall"),
                );
            }
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

                println!("{bin} installed to {}", path.display());

                if entry.has_share_assets {
                    let sv = version.clone();
                    let sb = bin.to_string();
                    match run_async(
                        async move { crate::install::install_share_assets(&sv, &sb).await },
                    ) {
                        Ok(()) => println!("{bin} emitter files installed"),
                        Err(e) => println!("warning: could not install {bin} share assets: {e:#}"),
                    }
                }

                if !crate::install::path_contains_travsr_bin() {
                    println!("\n{}", crate::install::path_hint());
                }

                true
            }
        }
    };

    // Handle the underlying SCIP tool (scip-go, scip-python, etc.).
    // Builtins are bundled in the travsr binary — no external tool needed.
    let mut tool_ready = if entry.builtin {
        true
    } else if wrapper_installed && (reinstall || !tool_available(entry.command)) {
        // UX-4: `--reinstall` must re-run the underlying SCIP tool install even when
        // it is already on PATH, not just the wrapper. Otherwise a user following the
        // documented remedy (`travsr lang install <lang> --reinstall`) to refresh a
        // tool whose `--version` is unreliable — scip-java, whose coursier launcher
        // reports the `0.0.0` sentinel — never re-downloads it and so never writes the
        // `<bin>.version` file that makes it visible/version-checked in `travsr status`.
        let interactive = !no_interactive && std::io::IsTerminal::is_terminal(&std::io::stdin());
        install_scip_tool(entry, interactive, yes, override_version)?
    } else {
        tool_available(entry.command)
    };

    // RFC-025 Point A parity: if the underlying tool is present but below its
    // declared floor, refuse it here with the same actionable message shape as
    // embed, exactly as `tool_available` gates readiness. Every Phase B entry
    // declares `Semver::ZERO` today (no known behavioral floor), so this never
    // fires until a floor is raised in the same commit a tool behavior needs it.
    if tool_ready {
        if let Some(refusal) = phase_b_tool_floor_refusal(entry) {
            eprintln!("{refusal}");
            tool_ready = false;
        }
    }

    // Register the language globally, then enable it for the repo we're being
    // run in. Running `install` inside a repo is the consent signal for that
    // repo, so we derive its identity and grant trust automatically — no second
    // command, no flag to remember. An explicit `--corpus` still overrides, for
    // scripting or enabling a different repo. Builtins ship inside the binary and
    // are never gated (ADR-017 Rule 3, enforced in invoke_phase_b_all), so they
    // need no grant.
    let mut config = load_config().unwrap_or_default();
    config.register(language);
    let enabled_here = match corpus {
        Some(c) => {
            config.trust_corpus(c);
            false
        }
        None if entry.builtin => false,
        None => match current_repo_corpus() {
            Some(c) => {
                config.trust_corpus(&c);
                true
            }
            None => false,
        },
    };
    save_config(&config)?;
    if enabled_here {
        println!("Semantic analysis enabled for this repository.");
    }

    // provider_ready: check PATH for builtins; wrapper_installed already accounts
    // for downloaded wrappers (including those just placed in ~/.travsr/bin).
    let provider_ready = match entry.provider_binary {
        None => true,
        Some(bin) => wrapper_installed || which(bin),
    };

    if !provider_ready || !tool_ready {
        println!(
            "'{language}' is set up, but its analyzer '{}' is not installed yet.\n\
             Full cross-file analysis stays off until it is; basic analysis still runs.\n\
             After it installs, run `travsr init` in your repository.",
            entry.command
        );
        return Ok(InstallStatus::WrapperOnly);
    }

    println!("'{language}' is active — full cross-file analysis is on.");
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
                    println!("{} installed.", entry.command);
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
                     Install it, then re-run `travsr lang install {}`.\n\
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
/// Shared tag-resolution for every downloadable sidecar (Phase B scip-* tools
/// and, via `install_backend_with_progress`, the embed sidecar). Precedence:
/// explicit `--version` override -> live `releases/latest` -> offline
/// `version_fallback`. A hash-pinned entry (`pinned`) ignores the network and
/// stays on `version_fallback` for supply-chain integrity (#410 M2). Folding
/// embed onto this closes RFC-025 G3 at the fetch layer — one tag resolver, both
/// families.
pub(crate) fn resolve_install_tag(
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
            println!("{} installed to {}", spec.install_name, path.display());
            // UX-4: some SCIP launchers (scip-java's coursier wrapper) report the
            // `0.0.0` "unset" sentinel from `--version`, so `travsr status` can only
            // show a real version via the `<bin>.version` fallback file. Nothing was
            // writing it, so scip-java installed but was silently omitted from the
            // sidecars block. Record the resolved release tag now — the version we
            // just downloaded — so the tool is visible and floor-checked.
            if let Some(vpath) = travsr_plugin_host::sidecar_version::version_sidecar_path(&path) {
                if let Err(e) = std::fs::write(&vpath, format!("{tag}\n")) {
                    tracing::debug!(path = %vpath.display(), error = %e, "could not write version sidecar file");
                }
            }
            if !crate::install::path_contains_travsr_bin() {
                println!("\n{}", crate::install::path_hint());
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

    println!("{} installed to {}", spec.install_name, wrapper.display());
    if !crate::install::path_contains_travsr_bin() {
        println!("\n{}", crate::install::path_hint());
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
        let approved = config
            .as_ref()
            .map(|c| c.is_approved(lang))
            .unwrap_or(false);

        // Same computed status as `lang list`, so the two commands agree exactly.
        let status = lang_capability_status(entry, registered, approved);

        println!(
            "  [{}] {}  ({})  {}",
            i + 1,
            lang,
            entry.extensions.join(", "),
            status.line()
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
        println!("{lang}:");
        match cmd_install(lang, false, false, None, false, false, None) {
            Ok(InstallStatus::FullyReady) => {}
            Ok(InstallStatus::WrapperOnly) => {
                println!("  {lang}: analyzer not installed yet — full analysis stays off")
            }
            Err(e) => eprintln!("  error: {e:#}"),
        }
        println!();
    }

    Ok(())
}

// ── add (legacy alias for install) ──────────────────────────────────────────

/// `add` predates `install` and used to fetch the analyzer via `npm install -g`.
/// Distribution moved to GitHub-release binaries (see `cmd_install`), so the npm
/// path is gone: `add` now just forwards to `install`, which downloads the right
/// binary, handles elevated approval interactively, and auto-enables the current
/// repo. Kept as an alias because existing docs and muscle memory still reach for
/// it.
fn cmd_add(language: &str, corpus: Option<&str>) -> Result<()> {
    cmd_install(language, false, false, corpus, false, false, None).map(|_| ())
}

// ── remove ────────────────────────────────────────────────────────────────────

fn cmd_remove(language: &str) -> Result<()> {
    if lookup(language).is_none() {
        // UX-014: match `install`'s unknown-language error — point the user at the
        // list so both failure paths are equally actionable.
        anyhow::bail!(
            "Unknown language '{language}'. Run `travsr lang list` to see available languages."
        );
    }
    let mut config = load_config().unwrap_or_default();
    if config.unregister(language) {
        save_config(&config)?;
        println!("'{language}' full analysis turned off — basic analysis still runs.");
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
        "Security approval recorded for '{language}'.\n\
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
        "Approval recorded. Permitted hosts: {}\n\
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

/// Languages detected in `repo_root` that genuinely still need a
/// `travsr lang install` step for semantic (call/reference) indexing.
///
/// UX-001/UX-013: built-in languages (rust, typescript, python, dart) ship in
/// the binary and their semantic analysis already works, so they must never
/// appear in the `init` "not set up" nudge — otherwise the summary reports a
/// language as both *enabled* and *not set up* in the same breath. This returns
/// only detected languages that are non-built-in and not yet registered.
pub(crate) fn languages_needing_setup(repo_root: &std::path::Path) -> Vec<String> {
    let config = load_config();
    detect_languages_in(repo_root)
        .into_iter()
        .filter(|l| {
            // Skip built-ins — they work without registration.
            if lookup(l).map(|e| e.builtin).unwrap_or(false) {
                return false;
            }
            config.as_ref().map(|c| !c.is_registered(l)).unwrap_or(true)
        })
        .collect()
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

/// The corpus identity of the repo we're being run in, or `None` when we're not
/// inside a git repo. Mirrors the daemon's `detect_corpus` exactly (git origin
/// remote canonicalised, else a local hash) so the grant we write lands on the
/// same key the Phase B gate checks — a mismatch would silently leave the repo
/// untrusted. `install` uses this to enable semantic analysis for the current
/// repo without the user having to pass or even know the corpus id.
fn current_repo_corpus() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let repo_root = crate::repo::find_git_root(&cwd).ok()?;
    let remote = std::process::Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "remote",
            "get-url",
            "origin",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    Some(match remote {
        Some(url) => travsr_core::canonical_corpus(&url),
        None => travsr_core::canonical_corpus_local(&repo_root),
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
    /// Repos enabled for Phase B, keyed by corpus id. Auto-added when `install`
    /// runs inside a repo; also settable via `--corpus` or `travsr config set`.
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

/// RFC-025 Point A parity for the Phase B family: if the underlying `scip-*` /
/// zip tool for `entry` is present but below its declared version floor, return
/// the actionable refuse message (same shape as embed). Returns `None` when the
/// tool is at/above the floor, when no floor is declared (the state of every
/// Phase B entry today), or when the tool is absent — the caller then treats it
/// as usable.
fn phase_b_tool_floor_refusal(entry: &travsr_plugin_host::PhaseBEntry) -> Option<String> {
    use travsr_plugin_host::sidecar_version::{
        below_floor_message, floor_status, unreadable_message, FloorStatus, Semver, SidecarSpec,
    };
    let (spec, install_name): (&dyn SidecarSpec, &str) = match &entry.scip_install {
        ScipInstall::GithubBinary(s) => (s as &dyn SidecarSpec, s.install_name),
        ScipInstall::ZipBinary(z) => (z as &dyn SidecarSpec, z.install_name),
        _ => return None,
    };
    // No Phase B entry declares a floor today (RFC-025 decision 3: a floor is
    // only set once a behavior relies on it). With no floor, `floor_status` can
    // only ever return a usable state, so the probe cannot change any decision -
    // and running it would exec a PATH-resolved binary inside the trust-boundary
    // crate purely to learn a version nothing consumes. Skip it entirely until a
    // real floor exists; this activates the instant one is declared, and keeps
    // the no-floor family paying nothing (no exec, no probe timeout, no log).
    if spec.min_version() == Semver::ZERO {
        return None;
    }
    let path = travsr_core::exec::resolve_executable(install_name)
        .or_else(|| dirs::home_dir().map(|h| h.join(".travsr").join("bin").join(install_name)))
        .filter(|p| p.exists())?;
    let remedy = format!("travsr lang install {} --reinstall", entry.language);
    match floor_status(spec, &path, None) {
        FloorStatus::BelowFloor {
            installed,
            required,
        } => Some(below_floor_message(
            install_name,
            &installed,
            &required,
            &remedy,
        )),
        FloorStatus::Unreadable { required } => {
            Some(unreadable_message(install_name, &required, &remedy))
        }
        // Usable states (floor met, no floor, or a transient probe timeout that
        // degrades to usable) do not refuse.
        FloorStatus::Ok(_) | FloorStatus::UnreadableNoFloor | FloorStatus::ProbeTimeout { .. } => {
            None
        }
    }
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
