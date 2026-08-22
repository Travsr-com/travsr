//! `travsr lang` subcommands — Phase B language package management.
//!
//! Reads the Phase B catalog from travsr-plugin-host to show what tools
//! exist, then manages registration in ~/.travsr/lang.toml.

use anyhow::{Context as _, Result};
use clap::Subcommand;
use std::path::PathBuf;
use travsr_plugin_host::phase_b::catalog::{
    lookup, GzBinarySpec, PhaseBEntry, SandboxRequirement, ScipBinarySpec, ScipInstall,
    ZipBinarySpec, CATALOG,
};

#[derive(Debug, Subcommand)]
pub enum LangCommand {
    /// Show every supported language and whether full analysis is available.
    // Aliased to `status` because every other area of the CLI spells this
    // `status` (`travsr status`, `daemon status`, `embed status`, `rerank
    // status`). `lang` was the only one that did not, which is why the project
    // docs told agents to run `travsr lang status` and got an error.
    #[command(visible_alias = "status")]
    List {
        /// Output as a JSON array for programmatic / extension use.
        #[arg(long)]
        json: bool,
    },
    /// Set up full cross-file analysis (calls and references) for a language.
    ///
    /// Downloads the language's analyzer into ~/.travsr/bin/ and turns on full
    /// analysis for it. Some analyzers reach the network while indexing (Java,
    /// Kotlin, Scala, C#); their elevated access is auto-granted for local use
    /// (ADR-017 Amendment A5), so no approval step is required.
    Install {
        /// Language name (e.g. rust, go, python).
        language: String,
        /// Re-download and overwrite the analyzer even if it is already installed.
        #[arg(long)]
        reinstall: bool,
        /// Skip all interactive prompts — for CI / scripting.
        #[arg(long)]
        no_interactive: bool,
        /// Advanced: turn on full analysis for a specific repository identity
        /// instead of the current one (auto-detected). Rarely needed.
        #[arg(long)]
        corpus: Option<String>,
        /// Only turn the language on in config; do not download its analyzer.
        #[arg(long)]
        skip_wrapper: bool,
        /// Auto-confirm the analyzer install without prompting. Used by the VS
        /// Code extension's non-interactive installs.
        #[arg(long)]
        yes: bool,
        /// Pin the downloaded analyzer to a specific release (e.g. --version
        /// v0.3.0) instead of the latest one. Applies to analyzers fetched from
        /// GitHub releases. Analyzers with a built-in checksum stay pinned and
        /// reject a mismatching version.
        #[arg(long)]
        version: Option<String>,
    },
    /// Scan this repo, detect its languages, and set up full analysis for them.
    Detect {
        /// Install every detected language without prompting. Use this in scripts
        /// and from the editor extension, where there is no interactive terminal to
        /// answer the per-language prompt. Elevated languages that need a security
        /// approval are skipped with a note rather than reaching the network.
        #[arg(long)]
        yes: bool,
    },
    /// Set up full analysis for a language (older name for `install`).
    Add {
        /// Language name (e.g. rust, java, php).
        language: String,
        /// Advanced: turn on full analysis for a specific repository identity
        /// instead of the current one (auto-detected). Rarely needed.
        #[arg(long)]
        corpus: Option<String>,
    },
    /// Turn off full analysis for a language.
    Remove {
        /// Language name.
        language: String,
    },
    /// Permit a language's analyzer to run with your own privileges on Windows.
    ///
    /// A few analyzers (currently Java and Scala) cannot run inside Travsr's
    /// isolation on Windows. This records your one-time permission to run them
    /// with your own privileges instead, so full analysis works. The permission
    /// is remembered, and re-indexing on commit honours it with no extra step.
    AllowUnsandboxed {
        /// Canonical language name (e.g. java, scala).
        language: String,
        /// Who is granting the permission (recorded for your own audit trail).
        #[arg(long)]
        granted_by: Option<String>,
        /// Withdraw a permission granted earlier.
        #[arg(long)]
        revoke: bool,
        /// Skip the confirmation prompt (required to grant non-interactively).
        #[arg(long)]
        yes: bool,
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
        LangCommand::Detect { yes } => cmd_detect(yes),
        LangCommand::Add { language, corpus } => cmd_add(&language, corpus.as_deref()),
        LangCommand::Remove { language } => cmd_remove(&language),
        LangCommand::AllowUnsandboxed {
            language,
            granted_by,
            revoke,
            yes,
        } => cmd_allow_unsandboxed(&language, granted_by.as_deref(), revoke, yes),
    }
}

// ── platform availability (#588) ──────────────────────────────────────────────

// Platform-availability predicates live in `travsr-plugin-host` (the crate both
// the CLI and the MCP server depend on) so `lang list`, `get_lang_status` and the
// VS Code panel derive the same verdict from one place and can never drift apart.
// Re-exported here so existing `crate::lang::…` call sites (and `status.rs`,
// `init.rs`) are unchanged.
pub(crate) use travsr_plugin_host::phase_b::platform::{
    analyzer_unavailable_os, full_analysis_unavailable_here, unsupported_reason,
    wrapper_unavailable_target,
};

/// The line shown wherever an unavailable language surfaces: the honest
/// capability statement plus whatever manual path the catalog records.
fn unavailable_status(entry: &PhaseBEntry, target: &str) -> String {
    let hint = if entry.underlying_tool_hint.is_empty() {
        String::new()
    } else {
        format!(", manual setup: {}", entry.underlying_tool_hint)
    };
    format!(
        "not available on {target} yet ({} ships no prebuilt binary for this platform){hint}",
        entry.provider_binary.unwrap_or(entry.command)
    )
}

// ── list ──────────────────────────────────────────────────────────────────────

/// Whether a bundled analyzer's hidden interpreter is present. travsr-lsif-ts
/// and travsr-lsif-py ship as JS files run through `node` — "bundled" only
/// means the emitter file itself needs no separate install, not that Node.js
/// is guaranteed to exist on the machine. True when the entry declares no such
/// hidden driver (nothing to check).
fn bundled_analyzer_ready(entry: &PhaseBEntry) -> bool {
    entry.runtime_driver.map_or(true, tool_available)
}

/// Whether full cross-file semantic can actually run for `entry` on this machine:
/// the analyzer is present (bundled, or its external binary resolves) AND the
/// language is enabled (built in, or registered for indexing). One rule for every
/// language — nothing is special-cased, so `lang list` and `lang detect` can never
/// disagree again.
fn analyzer_ready(entry: &PhaseBEntry, registered: bool) -> bool {
    let enabled = entry.builtin || registered;
    let present = if entry.analyzer_bundled() {
        bundled_analyzer_ready(entry)
    } else {
        entry.provider_binary.map_or(true, tool_available) && analyzer_command_present(entry)
    };
    enabled && present
}

/// Whether the entry's analyzer command resolves on this machine.
///
/// Like `tool_available(entry.command)`, but also consults `rustup which` for
/// rust-analyzer: `rustup component add rust-analyzer` installs it into the
/// active toolchain's bin dir (`~/.rustup/toolchains/<tc>/bin`), which is not on
/// PATH and not in `~/.cargo/bin`, so `tool_available` alone can't see it. Every
/// analyzer-presence decision routes through here so `lang list`, `lang detect`,
/// `lang status`, `lang install`, and the index-time resolver never disagree.
fn analyzer_command_present(entry: &PhaseBEntry) -> bool {
    let command_present = tool_available(entry.command)
        || (entry.command == "rust-analyzer"
            && travsr_indexer::ra_runner::resolve_ra_binary().is_some());
    command_present && entry.runtime_driver.map_or(true, tool_available)
}

/// The capability-view status for one language, shared by `lang list` (text and
/// JSON) and `lang detect`. Store-independent: the repo-level "did we actually
/// produce edges here" answer lives in `travsr status`, in the same vocabulary.
fn lang_capability_status(
    entry: &PhaseBEntry,
    registered: bool,
    consent: bool,
) -> travsr_plugin_host::phase_b::status::LangStatus {
    travsr_plugin_host::phase_b::status::capability(
        &travsr_plugin_host::phase_b::status::Capability {
            entry,
            analyzer_ready: analyzer_ready(entry, registered),
            // Elevated access is auto-granted for local use (ADR-017 amendment);
            // `capability()` no longer reads this, so it is always true.
            approved: true,
            // A language is "not available on this OS" either because its
            // travsr-lang-* wrapper has no build here (objectivec on non-Apple), or
            // because its analyzer ships only as a prebuilt binary with no asset for
            // this platform (scip-clang / scip-ruby / the swift emitter on Windows).
            // Both mean `travsr lang install <lang>` cannot reach full analysis here,
            // so the honest line is "not available on <os>", not "run: install".
            unsupported_on: unsupported_reason(entry),
            // On Windows, the analyzers that cannot run isolated are gated on the
            // user's one-time permission instead of the network approval. No effect
            // off Windows.
            windows_unsandboxed: cfg!(windows) && entry.windows_sandbox_unsupported(),
            unsandboxed_consent: consent,
        },
    )
}

/// Whether the user has permitted `entry`'s analyzer to run with their own
/// privileges: a recorded per-language grant in lang.toml, or the session-wide
/// `TRAVSR_ALLOW_UNSANDBOXED` opt-in. Mirrors the resolver so `lang list` /
/// `status` and the index-time decision cannot disagree.
fn unsandboxed_consent_present(config: Option<&LangConfig>, language: &str) -> bool {
    config.is_some_and(|c| c.has_unsandboxed_consent(language))
        || travsr_plugin_host::resolver::session_unsandboxed_opt_in()
}

/// Whether a language's full analysis is turned on for the repo we are standing
/// in. Mirrors the gate in `invoke_phase_b_all` exactly: builtins always run;
/// every other language runs only when it is registered AND this repo's corpus
/// carries the per-repo trust grant.
enum RepoState {
    /// Builtin (ts/js/python/rust) with its analyzer present — ships/runs without
    /// a per-repo step, and full analysis is actually available here.
    BuiltinAlwaysOn,
    /// Registered and this repo's corpus is trusted, and the analyzer is present —
    /// full analysis runs here.
    Enabled,
    /// Full analysis is authorized for this repo (builtin, or registered+trusted)
    /// but its analyzer is not installed on this machine, so only structural
    /// analysis runs until it is. The remedy is `travsr lang install <lang>` — not
    /// a per-repo trust step. This is the honest cell for rust when rust-analyzer
    /// is absent: it never claims a green "always on" while STATUS says "partial".
    NeedsAnalyzer,
    /// Inside a repo, but not registered and/or the corpus is untrusted.
    NotEnabled,
    /// Not inside a git repo, so there is no repo to enable for.
    NotInRepo,
}

impl RepoState {
    /// `analyzer_ready` is whether full cross-file analysis can actually run for
    /// this language on this machine (analyzer bundled or its binary resolves) —
    /// the same fact `lang list`'s STATUS column reflects. Threading it in keeps
    /// the THIS REPO column from claiming a language is on here while STATUS shows
    /// it is only partial (the rust-without-rust-analyzer case).
    fn compute(
        entry: &PhaseBEntry,
        registered: bool,
        in_repo: bool,
        corpus_trusted: bool,
        analyzer_ready: bool,
    ) -> Self {
        if entry.builtin {
            // Builtins bypass the per-repo trust gate, but "always on" is only
            // honest when the analyzer is actually present. Rust is the one builtin
            // whose analyzer (rust-analyzer) is external and can be missing.
            return if analyzer_ready {
                RepoState::BuiltinAlwaysOn
            } else {
                RepoState::NeedsAnalyzer
            };
        }
        if !in_repo {
            return RepoState::NotInRepo;
        }
        if registered && corpus_trusted {
            // Authorized for this repo; still gated on the analyzer being present.
            if analyzer_ready {
                RepoState::Enabled
            } else {
                RepoState::NeedsAnalyzer
            }
        } else {
            RepoState::NotEnabled
        }
    }

    /// The cell text for the THIS REPO column.
    fn cell(&self) -> &'static str {
        match self {
            RepoState::BuiltinAlwaysOn => "always on",
            RepoState::Enabled => "enabled",
            RepoState::NeedsAnalyzer => "not enabled",
            RepoState::NotEnabled => "not enabled",
            RepoState::NotInRepo => "n/a",
        }
    }

    /// Stable machine tag for JSON consumers (the VS Code panel). Never reworded
    /// once shipped: it is an API surface, not UI copy.
    fn tag(&self) -> &'static str {
        match self {
            RepoState::BuiltinAlwaysOn => "always_on",
            RepoState::Enabled => "enabled",
            RepoState::NeedsAnalyzer => "needs_analyzer",
            RepoState::NotEnabled => "not_enabled",
            RepoState::NotInRepo => "no_repo",
        }
    }
}

fn json_str(s: &str) -> String {
    // Full JSON string escaping, not just `\` and `"`: a control character
    // (newline, tab, …) in any field — an `underlying_tool_hint`, a status line —
    // would otherwise emit invalid JSON that the VS Code panel's `JSON.parse`
    // rejects. No catalog field carries one today, so this only hardens the
    // contract against a future hint that does.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_arr(items: &[&str]) -> String {
    let elems: Vec<String> = items.iter().map(|s| json_str(s)).collect();
    format!("[{}]", elems.join(","))
}

fn cmd_list(json: bool) -> Result<()> {
    let config = load_config();

    // Per-repo enablement (corpus trust gate): languages install globally, but
    // full analysis only runs in a repo whose corpus carries the trust grant.
    // Resolve the current repo's corpus once and share it across the JSON and
    // text renderings, so both agree with what `invoke_phase_b_all` enforces.
    let corpus = current_repo_corpus();
    let in_repo = corpus.is_some();
    let corpus_trusted = match (&corpus, &config) {
        (Some(c), Some(cfg)) => cfg.is_corpus_trusted(c),
        _ => false,
    };

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
            let provider_on_path = entry.provider_binary.map_or(true, tool_available);
            let tool_on_path = analyzer_command_present(entry);
            // Honest: the analyzer is installed when it is bundled (python) or its
            // external binary resolves — never just because a language is built in.
            let installed = (entry.analyzer_bundled() && bundled_analyzer_ready(entry))
                || (provider_on_path && tool_on_path);
            // Elevated access is auto-granted for local use (ADR-017 amendment), so
            // no language ever needs approval. The `needsApproval` field is kept as a
            // constant false so an older extension parsing this JSON keeps working.
            let needs_approval = false;
            let package = entry.npm_package.unwrap_or(entry.command);
            let scip_type = match entry.scip_install {
                ScipInstall::GithubBinary(_) => "GithubBinary",
                ScipInstall::ZipBinary(_) => "ZipBinary",
                ScipInstall::Command(_) => "Command",
                ScipInstall::CommandThenGithubGz(_, _) => "CommandThenGithubGz",
                ScipInstall::Manual => "Manual",
            };
            // #588: `installed:false` alone cannot distinguish "run the install
            // command" from "no build exists for this platform". Consumers that
            // surface an install prompt (the VS Code extension included) need
            // that difference, so it is stated rather than implied. Uses the same
            // combined predicate as `statusLine` (wrapper OR analyzer asset), so
            // `availableOnThisPlatform` can never contradict a `status:unsupported`
            // line the way it did when it looked at the wrapper alone. The value is
            // the OS word ("windows"), matching the wording in `statusLine`.
            let unavailable_on = unsupported_reason(entry);
            // The authoritative status every consumer renders. `status` is a stable
            // machine tag; `statusLine` is the exact human wording used in the CLI,
            // so the extension shows the same words without re-deriving them.
            let consent = unsandboxed_consent_present(config.as_ref(), entry.language);
            let status = lang_capability_status(entry, registered, consent);
            // Per-repo enablement for the repo we are being run in (corpus trust
            // gate). The VS Code panel runs `lang list --json` with the target
            // repo as cwd, so this reflects that repo.
            let repo_state = RepoState::compute(
                entry,
                registered,
                in_repo,
                corpus_trusted,
                analyzer_ready(entry, registered),
            );
            entries.push(format!(
                r#"{{"language":{},"package":{},"sandbox":{},"status":{},"statusLine":{},"repoState":{},"installed":{},"registered":{},"builtin":{},"needsApproval":{},"scipInstallType":{},"installHint":{},"underlyingToolHint":{},"prerequisites":{},"elevatedHosts":{},"availableOnThisPlatform":{},"unavailableTarget":{}}}"#,
                json_str(entry.language),
                json_str(package),
                json_str(sandbox),
                json_str(status.tag()),
                json_str(&status.line()),
                json_str(repo_state.tag()),
                installed,
                registered,
                entry.builtin,
                needs_approval,
                json_str(scip_type),
                json_str(entry.install_hint),
                json_str(entry.underlying_tool_hint),
                json_str(entry.effective_prerequisites()),
                json_arr(entry.elevated_hosts),
                unavailable_on.is_none(),
                unavailable_on.as_deref().map_or("null".to_string(), json_str),
            ));
        }
        println!("[{}]", entries.join(",\n"));
        return Ok(());
    }

    // `corpus`, `in_repo` and `corpus_trusted` were resolved once at the top of
    // this function and are shared with the JSON branch above.
    let mut any_not_enabled = false;

    println!(
        "{:<12} {:<13} {:<24} STATUS",
        "LANGUAGE", "THIS REPO", "PREREQUISITES"
    );
    println!("{}", "-".repeat(84));

    for entry in CATALOG {
        let registered = config
            .as_ref()
            .map(|c| c.is_registered(entry.language))
            .unwrap_or(false);
        // One computed status for every language — the same call `lang detect` and
        // the JSON branch make, so the three can never drift apart again.
        let consent = unsandboxed_consent_present(config.as_ref(), entry.language);
        let status = lang_capability_status(entry, registered, consent);

        // Is full analysis turned on for the repo we are in? (corpus trust gate)
        let repo_state = RepoState::compute(
            entry,
            registered,
            in_repo,
            corpus_trusted,
            analyzer_ready(entry, registered),
        );
        if matches!(repo_state, RepoState::NotEnabled) {
            any_not_enabled = true;
        }

        println!(
            "{:<12} {:<13} {:<24} {}",
            entry.language,
            repo_state.cell(),
            entry.effective_prerequisites(),
            status.line(),
        );
    }

    // Explain the THIS REPO column once, below the table, rather than repeating a
    // remedy on every row.
    if !in_repo {
        println!();
        println!(
            "THIS REPO shows 'n/a' because you are not inside a git repository. \
             cd into a repo to enable languages there."
        );
    } else if any_not_enabled {
        println!();
        println!(
            "'not enabled' means full analysis is off for THIS repo even when the tool \
             is installed globally."
        );
        println!(
            "Turn a language on for this repo:  travsr lang install <language>   \
             (run inside the repo)"
        );
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
             this platform, only call/reference analysis needs this binary.",
            unavailable_status(entry, target)
        );
    }

    // The wrapper builds here, but the analyzer ships only as a prebuilt binary
    // with no asset for this platform (scip-clang, scip-ruby, the swift emitter on
    // Windows). Installing the wrapper would register the repo and then dead-end at
    // "no prebuilt binary", so refuse up front with the honest reason — the same
    // treatment `lang detect` gives it by omitting it from the install menu.
    if let Some(os) = analyzer_unavailable_os(entry) {
        anyhow::bail!(
            "'{language}' can't reach full analysis on {os}: its analyzer '{}' has \
             no prebuilt binary for this platform.\n\
             \n\
             Structural indexing (symbols, definitions, repo map) still works here \
, only call/reference analysis needs that binary.",
            entry.command
        );
    }

    // On Windows, the analyzers that cannot run isolated (java/scala) take the
    // plain-child permission path (`travsr lang allow-unsandboxed <lang>`); the
    // consent gate for that is checked further below. Elevated network access for
    // RequiresElevated languages is otherwise auto-granted for local use (ADR-017
    // amendment), so there is no install-time approval demand here anymore.
    let windows_unsandboxed = cfg!(windows) && entry.windows_sandbox_unsupported();

    // Download and install the travsr-lang-* wrapper binary.
    let wrapper_installed = match entry.provider_binary {
        None => true, // builtin — no external wrapper needed
        // G1: the fast-path must see a wrapper installed in ~/.travsr/bin even when
        // that dir is not on PATH (the default on Windows, common elsewhere).
        // `which` (PATH only) missed it, so every `lang install` re-downloaded the
        // wrapper over the network and skipped the version advisory below.
        // `tool_available` includes ~/.travsr/bin, matching how `lang list` reports
        // the same wrapper as present.
        Some(bin) if tool_available(bin) && !reinstall => {
            // RFC-025 Point B: presence never re-checks the release the wrapper
            // was pinned to. Surface a below-floor WARN (offline) and a newer-
            // release advisory (best-effort) over the installed wrapper. Never
            // fails install.
            if let Some(path) = travsr_core::exec::tool_path(bin) {
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
                tool_available(bin)
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

                true
            }
        }
    };

    // Handle the underlying SCIP tool (scip-go, scip-python, etc.).
    // Builtins are bundled in the travsr binary — no external tool needed.
    let mut tool_ready = if entry.builtin && entry.analyzer_bundled() {
        // The analyzer file ships inside the travsr binary (python/ts/js) —
        // nothing to install. But "bundled" is about the file, not its
        // interpreter: ts/js/python all run through `node`, which still has to
        // actually be on the machine, or this would claim "active" for an
        // emitter that will fail to spawn. A builtin whose analyzer is external
        // (rust → rust-analyzer) is NOT bundled, so it falls through to the
        // install path below instead of short-circuiting to a false "active"
        // without ever fetching the analyzer.
        bundled_analyzer_ready(entry)
    } else if wrapper_installed && (reinstall || !analyzer_command_present(entry)) {
        // UX-4: `--reinstall` must re-run the underlying SCIP tool install even when
        // it is already on PATH, not just the wrapper. Otherwise a user following the
        // documented remedy (`travsr lang install <lang> --reinstall`) to refresh a
        // tool whose `--version` is unreliable — scip-java, whose coursier launcher
        // reports the `0.0.0` sentinel — never re-downloads it and so never writes the
        // `<bin>.version` file that makes it visible/version-checked in `travsr status`.
        let interactive = !no_interactive && std::io::IsTerminal::is_terminal(&std::io::stdin());
        install_scip_tool(entry, interactive, yes, override_version)?
    } else {
        analyzer_command_present(entry)
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

    // provider_ready: wrapper_installed already accounts for downloaded wrappers
    // (including those just placed in ~/.travsr/bin); tool_available covers a
    // wrapper present in ~/.travsr/bin off PATH, consistent with `lang list`.
    let provider_ready = match entry.provider_binary {
        None => true,
        Some(bin) => wrapper_installed || tool_available(bin),
    };
    let full_ready = provider_ready && tool_ready;

    // One PATH hint per `lang install` run, not one per downloaded binary — the
    // wrapper and the underlying analyzer used to each print the identical
    // "add ~/.travsr/bin to your PATH" block back to back when both were fresh
    // downloads, telling the user the same thing twice in a row.
    if !crate::install::path_contains_travsr_bin() {
        println!("\n{}", crate::install::path_hint());
    }

    // The success line at the end of this function carries the repo-scope
    // confirmation ("... on for this repository") when the language was enabled
    // here AND is fully ready — one line instead of a separate "Semantic
    // analysis enabled for this repository." above it saying the same thing.
    // When the analyzer is still missing, the WrapperOnly branch below states
    // the honest "set up, but analyzer not installed"; the repo trust grant was
    // recorded either way.
    if !full_ready {
        // The most common "not ready" cause across every platform (go/npm/dotnet
        // missing) collapses to one line instead of stacking the generic
        // "analyzer not installed yet" paragraph on top of it — the driver is the
        // whole story here, there is nothing else to add.
        if let ScipInstall::Command(cmd_args) = entry.scip_install {
            if !tool_available(cmd_args[0]) {
                println!(
                    "'{}' is not installed on your machine. Install it, then run \
                     `travsr lang install {language}` again.",
                    cmd_args[0]
                );
                return Ok(InstallStatus::WrapperOnly);
            }
            // Driver present, analyzer still absent, and travsr did NOT attempt an
            // auto-install (non-interactive without --yes — with --yes or an
            // interactive prompt it already tried, so a "how to install" nudge
            // would misread). One actionable block: the install command plus the
            // --yes shortcut. run_pkg_command stays silent in this mode, so this
            // is the only message — no stacked hint + generic paragraph.
            let attempted_auto =
                yes || (!no_interactive && std::io::IsTerminal::is_terminal(&std::io::stdin()));
            if !attempted_auto {
                println!(
                    "'{language}' isn't fully set up yet: its analyzer '{}' isn't installed.\n\
                     Install it, or re-run `travsr lang install {language} --yes` to let travsr do it:\n\t{}\n\
                     Basic analysis still runs until then.",
                    entry.command,
                    cmd_args.join(" ")
                );
                return Ok(InstallStatus::WrapperOnly);
            }
        }
        // Same one-line treatment for a driver hidden inside a generated
        // launcher (scip-java / kotlin-language-server both need a JVM on
        // PATH to run at all, on every platform) — the analyzer file being
        // present is not the whole story, so don't claim it's "active".
        if let Some(driver) = entry.runtime_driver {
            if !tool_available(driver) {
                println!(
                    "'{driver}' is not installed on your machine. Install it, then run \
                     `travsr lang install {language}` again."
                );
                return Ok(InstallStatus::WrapperOnly);
            }
        }
        // Manual (scala/sbt, php/composer): there's nothing travsr can auto-run,
        // just a pointer to where the tool comes from — one line, not the
        // generic paragraph below.
        if matches!(entry.scip_install, ScipInstall::Manual) && !tool_available(entry.command) {
            print!(
                "'{}' is not installed. Install it, then run `travsr lang install {language}` again.",
                entry.command
            );
            if entry.underlying_tool_hint.is_empty() {
                println!();
            } else {
                println!("\n\t{}", entry.underlying_tool_hint);
            }
            return Ok(InstallStatus::WrapperOnly);
        }
        println!(
            "'{language}' isn't fully set up yet: its analyzer '{}' is not installed.\n\
             Full cross-file analysis stays off until it is; basic analysis still runs.\n\
             After it installs, run `travsr init` in your repository.",
            entry.command
        );
        return Ok(InstallStatus::WrapperOnly);
    }

    // Windows-only: the analyzer is installed, but it cannot run inside Travsr's
    // isolation here, so full analysis stays off until the user grants the one-time
    // permission. Say that honestly instead of claiming "active".
    if windows_unsandboxed && !config.has_unsandboxed_consent(language) {
        println!(
            "'{language}' analyzer is installed. One more step: its build tools can't run \
             inside Travsr's isolation on Windows, so full analysis needs your permission \
             to run them with your own privileges.\n\
             Grant it:  travsr lang allow-unsandboxed {language}\n\
             Basic analysis runs until then."
        );
        return Ok(InstallStatus::FullyReady);
    }

    if enabled_here {
        println!("'{language}' is active, full cross-file analysis is on for this repository.");
    } else {
        println!("'{language}' is active, full cross-file analysis is on.");
    }
    Ok(InstallStatus::FullyReady)
}

/// Result of attempting a package-manager install command.
enum CmdOutcome {
    /// The command was run; `success` is its exit status.
    Ran { success: bool },
    /// The command was not run — declined in interactive mode, or only printed
    /// as a hint in non-interactive/non-`--yes` mode.
    NotRun,
}

/// Run (or hint at) a package-manager install command for `entry`, honouring the
/// same interactive / `--yes` / hint gating every `Command`-style install uses.
/// Shared by the plain `Command` path and the `CommandThenGithubGz` path so the
/// two never drift.
fn run_pkg_command(
    entry: &PhaseBEntry,
    cmd_args: &[&str],
    interactive: bool,
    yes: bool,
) -> Result<CmdOutcome> {
    // Check the install driver (e.g. `go`, `dotnet`, `npm`) is present BEFORE
    // offering to run it. Prompting "Run it now? [Y/n]" for `go install ...` when
    // `go` itself is not installed is a dead-end interaction: the answer cannot
    // matter, and reaching the failure only after a "yes" reads as a bug. Stay
    // quiet here and let the caller (`install`'s `!full_ready` branch) print the
    // one-line "driver itself is missing" message — it already has to special-
    // case this to avoid stacking a redundant "analyzer not installed yet"
    // paragraph on top. (The spawn below keeps an ErrorKind::NotFound arm as a
    // fallback for a driver that resolves here but fails to spawn, e.g. one
    // found only in an extra dir that is not on the child process's PATH.)
    if !tool_available(cmd_args[0]) {
        return Ok(CmdOutcome::Ran { success: false });
    }

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
        // Non-interactive without --yes: stay silent. install()'s `!full_ready`
        // branch prints ONE actionable block for this case (the install command
        // plus the --yes shortcut). Printing a near-identical "Note: not found"
        // hint here too just stacked two blocks saying the same thing.
        false
    };
    if !do_run {
        return Ok(CmdOutcome::NotRun);
    }
    let mut child = match std::process::Command::new(cmd_args[0])
        .args(&cmd_args[1..])
        .spawn()
    {
        Ok(child) => child,
        // The install driver itself (e.g. `go`, `dotnet`) is not on PATH. That is a
        // normal "can't auto-install here" outcome, not a fatal error — report it
        // and return so the caller still reaches registration and the honest
        // "analyzer not installed yet" summary, exactly like every other
        // missing-analyzer path. Aborting via `?` here is what made `go` the lone
        // language that skipped that summary (it errored out mid-flow instead).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "'{}' is not installed, so '{}' can't be set up automatically.\n\
                 Install it manually, then re-run:\n\t{}",
                cmd_args[0],
                entry.command,
                cmd_args.join(" ")
            );
            return Ok(CmdOutcome::Ran { success: false });
        }
        Err(e) => {
            return Err(e).with_context(|| format!("running '{}'", cmd_args.join(" ")));
        }
    };
    let status = child.wait()?;
    if status.success() {
        println!("{} installed.", entry.command);
    } else {
        println!(
            "Install command exited with {status}.\nRun manually: {}",
            cmd_args.join(" ")
        );
    }
    Ok(CmdOutcome::Ran {
        success: status.success(),
    })
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
            ScipInstall::GithubBinary(_)
                | ScipInstall::ZipBinary(_)
                | ScipInstall::CommandThenGithubGz(_, _)
        )
    {
        eprintln!(
            "warning: --version has no effect for '{}': its analysis tool is not \
             installed from GitHub Releases. Ignoring the pin.",
            entry.language
        );
    }
    match entry.scip_install {
        ScipInstall::Command(cmd_args) => match run_pkg_command(entry, cmd_args, interactive, yes)?
        {
            // Trust exit 0: the binary may be in $GOPATH/bin, ~/.cargo/bin, or
            // another tool-managed dir not yet in the process's PATH.
            CmdOutcome::Ran { success: true } => return Ok(true),
            // Not run (declined / no --yes) or the command failed: the tool may
            // still already be present — e.g. `--reinstall` forces this path even
            // when nothing is missing. Re-check instead of reporting a false "not
            // installed" (the old code returned false unconditionally here → G4).
            _ => return Ok(analyzer_command_present(entry)),
        },
        ScipInstall::CommandThenGithubGz(cmd_args, ref spec) => {
            // Preferred path: the toolchain-managed command (e.g. `rustup
            // component add rust-analyzer`) — version-matched and no
            // decompression. Only attempt it when its driver is actually present;
            // a user without rustup skips straight to the pinned GitHub download.
            let driver = cmd_args[0];
            if tool_available(driver) {
                run_pkg_command(entry, cmd_args, interactive, yes)?;
                // rustup installs rust-analyzer as a toolchain COMPONENT (into
                // ~/.rustup/toolchains/<tc>/bin), not onto PATH or ~/.cargo/bin,
                // so `tool_available` can't see it and would wrongly fall through
                // to the GitHub download. resolve_ra_binary() also consults
                // `rustup which`, the cross-platform resolver for a toolchain
                // component. Non-memoized on purpose: ra_binary_path() may have
                // cached a pre-install `None` earlier in this process.
                if analyzer_command_present(entry) {
                    return Ok(true);
                }
                // Driver present but the tool is still missing (declined or the
                // command failed): fall through to the download rather than
                // leaving semantic analysis off.
                println!(
                    "'{}' still isn't available via {driver}. Downloading a \
                     ready-to-run {} from its official releases instead.",
                    entry.command, entry.command
                );
            } else {
                println!(
                    "'{driver}' isn't installed, so '{}' can't be added that way. \
                     Downloading a ready-to-run {} from its official releases instead.",
                    entry.command, entry.command
                );
            }
            install_gz_github_binary(entry, spec, override_version)?;
            return Ok(analyzer_command_present(entry));
        }
        ScipInstall::GithubBinary(ref spec) => {
            install_scip_github_binary(entry, spec, override_version)?;
            return Ok(analyzer_command_present(entry));
        }
        ScipInstall::ZipBinary(ref spec) => {
            install_zip_binary(entry, spec, override_version)?;
            return Ok(analyzer_command_present(entry));
        }
        // Stay quiet here (like the Command path's run_pkg_command above) and let
        // the caller (install()'s `!full_ready` branch) print the one-line
        // "underlying tool is missing" message — otherwise the two stack: this
        // message plus the near-identical generic "isn't fully set up yet"
        // paragraph the caller used to always print after it.
        ScipInstall::Manual => {}
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

    // Windows compat pin: scip-java's current upstream line dropped Windows build
    // support, so on Windows the catalog pins it to a Windows-capable tag (0.12.x)
    // regardless of `releases/latest`. Expressed as a pin so the same
    // supply-chain-safe resolution applies: no live fetch, and a `--version`
    // asking for a different tag is refused. mac/linux keep tracking latest.
    let windows_pin = spec.windows_pin.filter(|_| cfg!(windows));

    // #410 M2: an entry carrying a vendored hash is pinned, because the hash is
    // only meaningful against one exact asset — a floating `releases/latest` (or
    // a `--version` override) would move the target out from under it. Everything
    // else honors the override, else fetches latest, else the offline fallback.
    let repo = spec.repo.to_string();
    let tag = resolve_install_tag(
        spec.sha256_fn.is_some() || windows_pin.is_some(),
        windows_pin.unwrap_or(spec.version_fallback),
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
            // scip-java's asset is a coursier polyglot launcher: a `#!/usr/bin/env
            // sh` preamble in front of a real JAR. Windows cannot exec the sh
            // preamble, but the identical file runs as `java -jar <asset>`. Drop a
            // `.cmd` beside it that launches it through the JVM so a PATHEXT-aware
            // lookup resolves a runnable `scip-java` on Windows — the bare
            // downloaded file alone was unrunnable, which is why java installed
            // "successfully" yet produced nothing. Needs a JVM on PATH, exactly as
            // any Java project already does.
            #[cfg(windows)]
            if spec.install_name == "scip-java" {
                let cmd_path = path.with_extension("cmd");
                let script = format!("@echo off\r\njava -jar \"{}\" %*\r\n", path.display());
                if let Err(e) = std::fs::write(&cmd_path, &script) {
                    tracing::debug!(path = %cmd_path.display(), error = %e, "could not write scip-java .cmd launcher");
                }
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

/// Download a pinned single-executable release that ships compressed (`.gz` on
/// unix, `.zip` on windows) and install the binary into `~/.travsr/bin`. This is
/// the rust-analyzer fallback used when `rustup` is unavailable.
///
/// The entry is always pinned (its hash is vendored for one exact tag), so
/// `--version` may only ask for that same tag; `resolve_install_tag` rejects any
/// other. A platform with no vendored hash is refused rather than downloaded
/// unverified.
fn install_gz_github_binary(
    entry: &travsr_plugin_host::phase_b::catalog::PhaseBEntry,
    spec: &GzBinarySpec,
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

    // Always pinned: the vendored hash only matches `version_fallback`, so the
    // tag never floats to `releases/latest`.
    let tag = resolve_install_tag(
        true,
        spec.version_fallback,
        override_version,
        spec.install_name,
        || Ok(spec.version_fallback.to_string()),
    )?;

    let asset_name = match (spec.asset_fn)(&tag, target) {
        Some(a) => a,
        None => {
            println!(
                "'{}' has no pre-built binary for your platform ({target}).\n\
                 Install it manually:\n\t{}",
                entry.command, entry.underlying_tool_hint
            );
            return Ok(());
        }
    };

    // Fail closed: an asset we never hashed is refused, not fetched over TLS alone.
    let expected = match (spec.sha256_fn)(&tag, target) {
        Some(h) => h.to_string(),
        None => {
            println!(
                "'{}' {tag} for {target} has no verified checksum on record, so it \
                 will not be downloaded automatically.\n\
                 Install it manually:\n\t{}",
                entry.command, entry.underlying_tool_hint
            );
            return Ok(());
        }
    };

    println!("Downloading {} {} ...", spec.install_name, tag);

    let repo = spec.repo.to_string();
    let tag2 = tag.clone();
    let asset2 = asset_name.clone();
    let name2 = spec.install_name.to_string();
    let target2 = target.to_string();

    match run_async(async move {
        crate::install::download_ra_binary(&repo, &tag2, &asset2, &name2, &target2, &expected).await
    }) {
        Ok(path) => {
            println!("{} installed to {}", spec.install_name, path.display());
            // Record the resolved release tag next to the binary — rust-analyzer
            // uses date-based tags that don't parse as semver, so this is what
            // `travsr status` reads back rather than a `--version` probe.
            if let Some(vpath) = travsr_plugin_host::sidecar_version::version_sidecar_path(&path) {
                if let Err(e) = std::fs::write(&vpath, format!("{tag}\n")) {
                    tracing::debug!(path = %vpath.display(), error = %e, "could not write version sidecar file");
                }
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

    // Write a wrapper the host OS can actually execute, aimed at the launcher
    // that host ships. The extracted archive carries one launcher per platform in
    // the same `bin` dir: the `#!/bin/sh` script named by `binary_subpath` on
    // unix, and a `.bat` of the same stem next to it on Windows. Previously this
    // ALWAYS wrote a `#!/bin/sh` wrapper pointing at the unix launcher, so on
    // Windows the installed wrapper was a non-runnable sh script aimed at a
    // non-runnable launcher — yet the language still reported as installed and
    // "active". Write a `.cmd` targeting the `.bat` launcher on Windows instead.
    let bin_dir = crate::install::travsr_bin_dir()?;

    #[cfg(windows)]
    let (wrapper, script) = {
        // `kotlin-language-server` -> `kotlin-language-server.bat` in the same dir.
        let launcher = extract_path.join(spec.binary_subpath).with_extension("bat");
        // `.cmd` so PATHEXT resolution (and the sandbox's PE-image spawn) treat it
        // as an executable, matching how every other Windows provider is found.
        let wrapper = bin_dir.join(format!("{}.cmd", spec.install_name));
        let script = format!("@echo off\r\n\"{}\" %*\r\n", launcher.display());
        (wrapper, script)
    };
    #[cfg(unix)]
    let (wrapper, script) = {
        let binary_abs = extract_path.join(spec.binary_subpath);
        let wrapper = bin_dir.join(spec.install_name);
        let script = format!("#!/bin/sh\nexec {} \"$@\"\n", binary_abs.display());
        (wrapper, script)
    };

    std::fs::write(&wrapper, &script)
        .with_context(|| format!("writing wrapper to {}", wrapper.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
            .context("chmod +x wrapper")?;
    }

    println!("{} installed to {}", spec.install_name, wrapper.display());

    Ok(())
}

// ── detect ────────────────────────────────────────────────────────────────────

fn cmd_detect(yes: bool) -> Result<()> {
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

    // Partition detected languages by whether full analysis can ever run here.
    // Some analyzers ship only as a prebuilt binary with no build for this
    // platform (scip-clang for c/cpp, scip-ruby, the swift emitter on Windows).
    // Installing their wrapper would register the repo and then dead-end at "no
    // prebuilt binary", so they are shown for context but never offered as an
    // install target — only `installable` is numbered and selectable.
    use travsr_plugin_host::phase_b::status::LangStatus;
    let status_of = |lang: &str| {
        let entry = lookup(lang).expect("lang came from CATALOG");
        let registered = config
            .as_ref()
            .map(|c| c.is_registered(lang))
            .unwrap_or(false);
        let consent = unsandboxed_consent_present(config.as_ref(), lang);
        lang_capability_status(entry, registered, consent)
    };
    let mut installable: Vec<&str> = Vec::new();
    let mut unavailable: Vec<&str> = Vec::new();
    for lang in &found {
        if matches!(status_of(lang), LangStatus::PlatformUnsupported { .. }) {
            unavailable.push(lang);
        } else {
            installable.push(lang);
        }
    }

    println!("Detected languages in this repo:\n");
    for (i, lang) in installable.iter().enumerate() {
        let entry = lookup(lang).expect("lang came from CATALOG");
        // Same computed status as `lang list`, so the two commands agree exactly.
        println!(
            "  [{}] {}  ({})  {}",
            i + 1,
            lang,
            entry.extensions.join(", "),
            status_of(lang).line()
        );
    }
    if !unavailable.is_empty() {
        println!();
        println!(
            "Not installable on {}; the analyzer has no build for this platform \
             (structural indexing still works):",
            travsr_plugin_host::phase_b::status::os_label()
        );
        for lang in &unavailable {
            let entry = lookup(lang).expect("lang came from CATALOG");
            println!("      {}  ({})", lang, entry.extensions.join(", "));
        }
    }
    println!();

    if installable.is_empty() {
        println!("Nothing to install on this platform.");
        return Ok(());
    }

    // `--yes`: install everything installable without prompting. This is the path
    // the VS Code "Detect & install" button takes — it spawns `lang detect` with
    // no terminal attached, so without this it would only ever print the list and
    // install nothing. Each install is non-interactive; elevated languages install
    // with no approval step now that elevated access is auto-granted for local use.
    if yes {
        install_selected(
            &installable,
            /*no_interactive*/ true,
            /*yes*/ true,
        );
        return Ok(());
    }

    if !std::io::stdin().is_terminal() {
        println!(
            "(non-interactive; run `travsr lang install <lang>` to install one, or \
             `travsr lang detect --yes` to set up all detected)"
        );
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
        installable.clone()
    } else {
        let mut sel = Vec::new();
        for part in input.split(',') {
            let part = part.trim();
            match part.parse::<usize>() {
                Ok(n) if n >= 1 && n <= installable.len() => sel.push(installable[n - 1]),
                _ => println!("  skipping unrecognised selection: '{part}'"),
            }
        }
        sel
    };

    if selected.is_empty() {
        println!("Nothing selected.");
        return Ok(());
    }

    install_selected(&selected, /*no_interactive*/ false, /*yes*/ false);
    Ok(())
}

/// Install each detected language in turn, reporting per-language outcome without
/// aborting the batch on a single failure. Shared by the interactive selection and
/// the `--yes` path so both install exactly the same way — only the interactivity
/// of each underlying `cmd_install` differs.
fn install_selected(selected: &[&str], no_interactive: bool, yes: bool) {
    println!();
    for lang in selected {
        println!("{lang}:");
        match cmd_install(lang, false, no_interactive, None, false, yes, None) {
            Ok(InstallStatus::FullyReady) => {}
            Ok(InstallStatus::WrapperOnly) => {
                println!("  {lang}: analyzer not installed yet, full analysis stays off")
            }
            Err(e) => eprintln!("  error: {e:#}"),
        }
        println!();
    }
}

// ── add (legacy alias for install) ──────────────────────────────────────────

/// `add` predates `install` and used to fetch the analyzer via `npm install -g`.
/// Distribution moved to GitHub-release binaries (see `cmd_install`), so the npm
/// path is gone: `add` now just forwards to `install`, which downloads the right
/// binary and auto-enables the current repo. Kept as an alias because existing
/// docs and muscle memory still reach for it.
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
        println!("'{language}' full analysis turned off, basic analysis still runs.");
    } else {
        println!("'{language}' was not registered.");
    }
    Ok(())
}

// ── allow-unsandboxed (permission to run with the user's own privileges) ──────

fn cmd_allow_unsandboxed(
    language: &str,
    granted_by: Option<&str>,
    revoke: bool,
    yes: bool,
) -> Result<()> {
    let entry =
        lookup(language).ok_or_else(|| anyhow::anyhow!("Unknown language '{language}'."))?;

    // Only the analyzers that cannot run inside Travsr's isolation need this. For
    // every other language it would grant privileges for no reason, so refuse it.
    if !entry.windows_sandbox_unsupported() {
        anyhow::bail!(
            "'{language}' already runs inside Travsr's isolation, so it does not need this. \
             Run `travsr lang install {language}` to set up full analysis."
        );
    }

    let mut config = load_config().unwrap_or_default();

    if revoke {
        if config.revoke_unsandboxed_consent(language) {
            save_config(&config)?;
            println!(
                "Permission for '{language}' withdrawn. Full analysis will pause on \
                 Windows until you grant it again with `travsr lang allow-unsandboxed {language}`."
            );
        } else {
            println!("No permission was on record for '{language}', nothing to withdraw.");
        }
        return Ok(());
    }

    // Explain the trade-off BEFORE recording anything, then confirm. This grant
    // lifts Travsr's isolation for one language, so the user must see what they
    // are agreeing to first — plain language, no internal jargon.
    println!(
        "Granting '{language}' permission to run with your own privileges on Windows.\n\
         Its build tools cannot run inside Travsr's isolation there, so full analysis \
         needs this.\n\
         \n\
         What this allows: when Travsr indexes this project, '{language}' analysis will \
         download dependencies and run this project's own build with your privileges — \
         the same as if you ran the build yourself. Only grant it for a project whose \
         build you trust.\n"
    );

    if !confirm_unsandboxed_grant(yes)? {
        println!("No permission recorded for '{language}'.");
        return Ok(());
    }

    let granted_by = granted_by
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("USERNAME").ok())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "user".to_string());

    config.grant_unsandboxed_consent(language, &granted_by);
    save_config(&config)?;

    println!(
        "Permission recorded for '{language}'.\n\
         Re-index to use it now:  travsr init --semantic --force\n\
         To withdraw it later:    travsr lang allow-unsandboxed {language} --revoke"
    );
    Ok(())
}

/// Confirm an unsandboxed grant. `--yes` records it non-interactively; otherwise
/// an interactive terminal is prompted `[y/N]`. With no terminal and no `--yes`
/// the grant is refused rather than recorded silently, since it relaxes isolation.
fn confirm_unsandboxed_grant(yes: bool) -> Result<bool> {
    use std::io::{IsTerminal as _, Write as _};
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "Refusing to grant this permission without confirmation. Re-run in a terminal, \
             or pass `--yes` to grant it non-interactively."
        );
    }
    print!("Grant this permission? [y/N]: ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
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
    /// Repos enabled for Phase B, keyed by corpus id. Auto-added when `install`
    /// runs inside a repo; also settable via `--corpus` or `travsr config set`.
    #[serde(default)]
    trusted_corpora: Vec<String>,
    /// Per-language permission to run an analyzer that cannot run inside Travsr's
    /// isolation (java/scala on Windows) with the user's own privileges. Written by
    /// `travsr lang allow-unsandboxed`; honoured by the indexer resolver so the
    /// daemon/git-hook reindex path picks it up with no per-run flag.
    #[serde(default)]
    unsandboxed_consent: Vec<UnsandboxedConsent>,
    /// Vestigial: elevated access is auto-granted for local use now (ADR-017
    /// Amendment A5), so nothing writes this. Kept as an opaque round-trip so an
    /// upgrade-then-save does not silently destroy a pre-upgrade user's recorded
    /// `[[elevated_approvals]]` audit block (which they still need if they roll
    /// back). Placed last so the array-of-tables serialises after the plain-array
    /// fields (toml requires values before tables). Never read by this build.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    elevated_approvals: Vec<toml::Value>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct UnsandboxedConsent {
    language: String,
    /// Who granted the permission (recorded for auditability).
    granted_by: String,
    /// ISO-8601 date the permission was granted.
    granted_date: String,
}

impl LangConfig {
    pub(crate) fn is_registered(&self, language: &str) -> bool {
        self.registered.iter().any(|l| l == language)
    }

    /// Whether a repo (by corpus id) has the per-repo Phase B trust grant.
    /// This is the half of the gate that `invoke_phase_b_all` checks per repo;
    /// `is_registered` is the global per-language half.
    pub(crate) fn is_corpus_trusted(&self, corpus: &str) -> bool {
        self.trusted_corpora.iter().any(|c| c == corpus)
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

    fn trust_corpus(&mut self, corpus: &str) {
        if !self.trusted_corpora.iter().any(|c| c == corpus) {
            self.trusted_corpora.push(corpus.to_string());
        }
    }

    fn has_unsandboxed_consent(&self, language: &str) -> bool {
        self.unsandboxed_consent
            .iter()
            .any(|c| c.language == language)
    }

    fn grant_unsandboxed_consent(&mut self, language: &str, granted_by: &str) {
        self.unsandboxed_consent.retain(|c| c.language != language);
        self.unsandboxed_consent.push(UnsandboxedConsent {
            language: language.to_string(),
            granted_by: granted_by.to_string(),
            granted_date: chrono::Local::now().format("%Y-%m-%d").to_string(),
        });
    }

    /// Remove any permission for `language`. Returns true if one was present.
    fn revoke_unsandboxed_consent(&mut self, language: &str) -> bool {
        let before = self.unsandboxed_consent.len();
        self.unsandboxed_consent.retain(|c| c.language != language);
        self.unsandboxed_consent.len() < before
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

/// Whether `name` resolves on this machine, checking PATH plus the tool-managed
/// dirs not always on it ($GOBIN, $GOPATH/bin, ~/.cargo/bin, ~/.travsr/bin,
/// ~/.dotnet/tools) — because `go install`, `rustup component add`, `dotnet tool
/// install`, and `travsr lang install` each write into their own directory the
/// current process may not have inherited in PATH.
///
/// Delegates to the one shared presence check so the index-time Phase B resolver
/// and `lang list` can never disagree about whether an analyzer is installed.
fn tool_available(name: &str) -> bool {
    travsr_core::exec::tool_available(name)
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
    use super::LangConfig;

    #[test]
    fn elevated_approvals_survive_a_save_load_round_trip() {
        // #756 review: dropping the field used to rewrite lang.toml without a
        // pre-upgrade user's [[elevated_approvals]] audit block, a one-way
        // deletion. It must round-trip opaquely and must not trip toml's
        // "values must be emitted before tables" ordering rule.
        let src = r#"
registered = ["java", "go"]
trusted_corpora = ["github.com/acme/repo"]

[[elevated_approvals]]
language = "java"
approved_by = "octocat"
reason = "enable cross-file analysis"
approved_date = "2026-01-01"

[[unsandboxed_consent]]
language = "scala"
granted_by = "octocat"
granted_date = "2026-01-02"
"#;
        let cfg: LangConfig = toml::from_str(src).expect("parse pre-upgrade config");
        assert_eq!(
            cfg.elevated_approvals.len(),
            1,
            "block must be retained on load"
        );
        let out = toml::to_string_pretty(&cfg).expect("serialise must not hit ValueAfterTable");
        assert!(
            out.contains("[[elevated_approvals]]"),
            "block must survive a save: {out}"
        );
        assert!(
            out.contains("octocat"),
            "audit fields must survive a save: {out}"
        );
        let reparsed: LangConfig = toml::from_str(&out).expect("reparse round-tripped config");
        assert_eq!(reparsed.elevated_approvals.len(), 1);
    }

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

    use super::RepoState;
    use travsr_plugin_host::phase_b::catalog::lookup;

    #[test]
    fn builtin_without_its_analyzer_is_not_reported_always_on() {
        // Rust is builtin but its analyzer (rust-analyzer) is external and can be
        // missing. The THIS REPO column must not claim "always on" while STATUS
        // says "partial" — it reads "not enabled" (matching the other partial
        // languages), whose remedy is the same `travsr lang install rust`. The
        // machine tag stays `needs_analyzer` so JSON consumers keep the precise
        // reason.
        let rust = lookup("rust").expect("rust entry present");
        let missing = RepoState::compute(
            rust, /*registered*/ true, true, true, /*ready*/ false,
        );
        assert_eq!(missing.tag(), "needs_analyzer");
        assert_eq!(missing.cell(), "not enabled");

        // With rust-analyzer present, the builtin is honestly always on.
        let present = RepoState::compute(rust, true, true, true, /*ready*/ true);
        assert_eq!(present.tag(), "always_on");
    }

    #[test]
    fn bundled_builtin_stays_always_on() {
        // Python's analyzer is bundled, so it is always ready and always on.
        let python = lookup("python").expect("python entry present");
        let state = RepoState::compute(python, true, true, true, /*ready*/ true);
        assert_eq!(state.tag(), "always_on");
    }

    #[test]
    fn registered_trusted_but_analyzer_missing_reads_needs_analyzer() {
        // A non-builtin authorized for the repo but with no analyzer installed
        // reads "not enabled" (tag `needs_analyzer`), not a green "enabled".
        let go = lookup("go").expect("go entry present");
        let state = RepoState::compute(
            go, /*registered*/ true, true, /*trusted*/ true, false,
        );
        assert_eq!(state.tag(), "needs_analyzer");
    }

    #[test]
    fn untrusted_repo_still_reads_not_enabled() {
        // The trust gate is the first blocker for a non-builtin: no grant → the
        // per-repo "not enabled", regardless of analyzer presence.
        let go = lookup("go").expect("go entry present");
        let state = RepoState::compute(go, true, true, /*trusted*/ false, /*ready*/ true);
        assert_eq!(state.tag(), "not_enabled");
    }

    #[test]
    fn available_on_platform_matches_status_tag_for_every_language() {
        use super::lang_capability_status;
        use travsr_plugin_host::phase_b::catalog::CATALOG;
        use travsr_plugin_host::phase_b::platform::unsupported_reason;
        for entry in CATALOG {
            // What `lang list --json` emits for availableOnThisPlatform.
            let available = unsupported_reason(entry).is_none();
            // The status tag the same row shows. `unsupported` is the only tag that
            // means "cannot reach full analysis on this OS"; it comes from the same
            // predicate, so the boolean can never say "available" while the status
            // says "unsupported" (the #588 regression this locks out). Consent is
            // set so a merely-elevated language does not read as unsupported for an
            // unrelated reason.
            let status =
                lang_capability_status(entry, /*registered*/ true, /*consent*/ true);
            let unsupported = status.tag() == "unsupported";
            assert_eq!(available, !unsupported, "{}", entry.language);
        }
    }

    #[test]
    fn json_str_escapes_control_characters() {
        use super::json_str;
        // Quotes and backslashes were always escaped; a control char (newline,
        // tab, CR) must be too, or the emitted `lang list --json` is invalid JSON.
        assert_eq!(json_str("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(json_str("line1\nline2"), "\"line1\\nline2\"");
        assert_eq!(json_str("col1\tcol2"), "\"col1\\tcol2\"");
        assert_eq!(json_str("\r"), "\"\\r\"");
        // A bare low control char uses the \u escape.
        assert_eq!(json_str("\u{01}"), "\"\\u0001\"");
    }
}
