//! One honest, jargon-free, platform-aware description of a language's semantic
//! capability — rendered identically by every surface (CLI `lang list`/`status`,
//! `daemon status`, MCP `get_lang_status`, and the VS Code panel).
//!
//! Ground truth, true for every language:
//!   * Structural analysis (tree-sitter) is always available — no install, every
//!     language, always. It also produces best-effort call edges, but not full
//!     cross-file coverage.
//!   * Full cross-file semantic analysis needs the language's analyzer: bundled
//!     for python, typescript, and javascript (they share one bundled Node
//!     emitter), an external tool for the rest (rust-analyzer for rust, a
//!     `travsr-lang-*` analyzer for go/java/…). Those three are full out of the
//!     box; every other language needs its analyzer installed.
//!
//! So the only axis worth a word is whether full cross-file semantic is *live*.
//! The vocabulary is deliberately tiny and uniform across all languages:
//!   active   — full cross-file semantic is live
//!   partial  — tree-sitter only (structure + best-effort calls) + how to reach full
//!
//! Nothing here says "Phase B", "sandbox", "SCIP", "LSIF", "wrapper", "built-in",
//! or "corpus" — those are internal terms an end user does not know. This is the
//! ONLY place the wording lives; every renderer calls it so the words cannot drift.

use super::catalog::PhaseBEntry;

/// The honest per-language semantic state. One vocabulary, every surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LangStatus {
    /// Full cross-file semantic analysis is live for this language.
    Active,
    /// Only tree-sitter structure + best-effort call edges are available.
    /// `next` is the honest, platform-correct step to reach full semantic, if one
    /// exists on this machine (e.g. `travsr lang install go`); `None` when there
    /// is nothing the user can do here.
    Partial { next: Option<String> },
    /// Vestigial since elevated access became auto-granted for local use
    /// (ADR-017 Amendment A5): network-reaching analyzers (Java, Kotlin, Scala,
    /// C#) are no longer gated on a recorded approval, so this build never
    /// constructs this variant. Retained for the MCP/JSON tag contract.
    NeedsApproval { language: String },
    /// On Windows only: this language's analyzer cannot run inside Travsr's
    /// isolation, so it needs the user's one-time permission to run with the
    /// user's own privileges before full analysis can happen. The analyzer is
    /// installed and ready — the only thing missing is that permission.
    NeedsConsent { language: String },
    /// No analyzer build exists for this operating system, so full semantic can
    /// never run here. Structure still works — the rendered line says so.
    PlatformUnsupported { os: String },
}

impl LangStatus {
    /// Stable machine tag for JSON consumers. Never reworded — the VS Code panel
    /// and any other tool key off this, so it is an API surface, not UI copy.
    pub fn tag(&self) -> &'static str {
        match self {
            LangStatus::Active => "active",
            LangStatus::Partial { .. } => "partial",
            LangStatus::NeedsApproval { .. } => "needs_approval",
            LangStatus::NeedsConsent { .. } => "needs_consent",
            LangStatus::PlatformUnsupported { .. } => "unsupported",
        }
    }

    /// The single human line shown in every text UI. No symbols, no jargon.
    /// Uniform across languages — the only thing that varies is the concrete
    /// next step carried in the variant.
    pub fn line(&self) -> String {
        match self {
            LangStatus::Active => "active".to_string(),
            LangStatus::Partial { next: Some(step) } => {
                format!("partial (run: {step} for full analysis)")
            }
            LangStatus::Partial { next: None } => "partial".to_string(),
            LangStatus::NeedsApproval { language } => {
                format!("needs approval (run: travsr lang install {language})")
            }
            LangStatus::NeedsConsent { language } => {
                format!(
                    "partial (full analysis needs your permission; run: \
                     travsr lang allow-unsandboxed {language})"
                )
            }
            LangStatus::PlatformUnsupported { os } => {
                format!("partial (full analysis not available on {os})")
            }
        }
    }
}

/// The operating-system word used in user-facing lines. Plain, lowercase, no jargon.
pub fn os_label() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "this platform"
    }
}

/// The honest next step to reach full semantic for `language`. Uniform for every
/// language: `travsr lang install <lang>` runs the right thing per language
/// (installs rust-analyzer for rust, the npm analyzer for typescript, downloads
/// the analyzer for go/java/…), so the user never has to know the tool's name.
pub fn install_step(language: &str) -> String {
    format!("travsr lang install {language}")
}

/// Inputs for the machine/install ("capability") view used by `lang list` and
/// `lang detect`: "on this machine, can this language reach full semantic, and if
/// not, what is the next step?" It is deliberately store-independent — the
/// repo-level "did we actually produce edges here" question is answered by the
/// caller in `travsr status` using the same [`LangStatus`] vocabulary.
pub struct Capability<'a> {
    pub entry: &'a PhaseBEntry,
    /// The full-semantic analyzer can run on this machine right now: python's
    /// bundled analyzer is present, or the language's external analyzer resolves.
    pub analyzer_ready: bool,
    /// A one-time network approval is on record (only meaningful when the entry
    /// requires elevated approval).
    pub approved: bool,
    /// `Some(os)` when no analyzer build is published for this operating system.
    pub unsupported_on: Option<String>,
    /// True only on Windows, and only for the languages whose analyzer cannot run
    /// inside Travsr's isolation (`entry.windows_sandbox_unsupported()`). When set,
    /// the one-time permission below is the gate for this language instead of the
    /// network approval. Always false off Windows — the mechanism is a no-op there.
    pub windows_unsandboxed: bool,
    /// Whether the user's one-time permission to run this language's analyzer with
    /// their own privileges is on record. Only consulted when `windows_unsandboxed`.
    pub unsandboxed_consent: bool,
}

/// Compute the capability-view status for one language. Same logic for every
/// language — nothing is special-cased.
pub fn capability(cap: &Capability) -> LangStatus {
    // No analyzer build for this OS: full can never run here (structure still does).
    if let Some(os) = &cap.unsupported_on {
        return LangStatus::PlatformUnsupported { os: os.clone() };
    }
    // Windows: analyzers that cannot run inside Travsr's isolation are gated on the
    // user's one-time permission to run with their own privileges. That permission
    // also covers the network access the isolated path would have needed approval
    // for, so it replaces the approval gate for these languages on Windows. Install
    // the analyzer first if it is not present yet.
    if cap.windows_unsandboxed {
        if !cap.analyzer_ready {
            return LangStatus::Partial {
                next: Some(install_step(cap.entry.language)),
            };
        }
        if !cap.unsandboxed_consent {
            return LangStatus::NeedsConsent {
                language: cap.entry.language.to_string(),
            };
        }
        return LangStatus::Active;
    }
    // Elevated (network-reaching) analyzers are auto-granted for local use
    // (ADR-017 amendment): they are no longer gated on a one-time approval and
    // fall through to the normal installed / needs-install status like any other
    // language. `NeedsApproval` is retained as an enum variant for the MCP/JSON
    // contract but is never emitted here.
    // Analyzer present and runnable → full cross-file semantic is available.
    if cap.analyzer_ready {
        return LangStatus::Active;
    }
    // Otherwise tree-sitter only, with the uniform next step.
    LangStatus::Partial {
        next: Some(install_step(cap.entry.language)),
    }
}

/// The per-repo answer to "will `travsr init` trace calls for this language
/// here, and if not, why". One predicate for `init`, `lang list`, `status` and
/// MCP, so they cannot disagree about a language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    /// Something `travsr init` fixes itself: install, registration, trust.
    SettingUp,
    /// The user must install `needs` first; travsr never installs toolchains.
    NeedsToolchain {
        needs: String,
    },
    Unsupported {
        os: String,
    },
    /// The analyzer ran and produced nothing usable, with no known cause.
    Failed,
    /// A tracer that ships inside travsr's install was not found (the binary
    /// was copied out of it). Only reinstalling brings it back.
    PartMissing,
}

impl Readiness {
    /// Stable machine tag for `--json` and MCP. Never reworded.
    pub fn tag(&self) -> &'static str {
        match self {
            Readiness::Ready => "ready",
            Readiness::SettingUp => "setting_up",
            Readiness::NeedsToolchain { .. } => "needs_toolchain",
            Readiness::Unsupported { .. } => "unsupported_os",
            Readiness::Failed | Readiness::PartMissing => "failed",
        }
    }

    /// The state in plain words, for text output (plan 3.1).
    pub fn label(&self) -> String {
        match self {
            Readiness::Ready => "ready".into(),
            Readiness::SettingUp => "setting up".into(),
            Readiness::NeedsToolchain { needs } => format!("needs {needs}"),
            Readiness::Unsupported { os } => format!("not available on {os}"),
            Readiness::Failed => "could not trace calls".into(),
            Readiness::PartMissing => "could not trace calls: part of travsr is missing".into(),
        }
    }

    /// The one next action, in plain words; `None` when there is nothing to do.
    pub fn fix(&self) -> Option<String> {
        match self {
            Readiness::Ready | Readiness::Unsupported { .. } => None,
            Readiness::SettingUp => Some("Run `travsr init` to finish tracing calls.".into()),
            Readiness::NeedsToolchain { needs } if needs == "compile_commands.json" => Some(
                "Generate compile_commands.json with your build, then run `travsr init`.".into(),
            ),
            Readiness::NeedsToolchain { needs } => {
                Some(format!("Install {needs}, then run `travsr init`."))
            }
            Readiness::Failed => Some("See `travsr status --verbose`.".into()),
            Readiness::PartMissing => Some("Reinstall travsr, then run `travsr init`.".into()),
        }
    }
}

/// Inputs to [`readiness`], gathered by [`gather`]. Plain facts so the ladder
/// is tested without disk, PATH or processes (same shape as [`Capability`]).
pub struct RepoCapability<'a> {
    pub entry: &'a PhaseBEntry,
    pub unsupported_on: Option<String>,
    /// A tool the user must install that is absent: the analyzer's runtime,
    /// the command its install runs, or the project's compile_commands.json.
    pub driver_missing: Option<String>,
    /// The analyzer must be built here and the installed Go is older than the
    /// build needs, so `travsr init` cannot finish setting it up.
    pub driver_too_old: bool,
    pub registered: bool,
    pub corpus_trusted: bool,
    pub analyzer_ready: bool,
    /// This language's class from the stored `phase_b_warnings` of the last run.
    pub last_warning: Option<&'a str>,
}

/// The ladder, in the order `invoke_phase_b_all` gates a language.
pub fn readiness(c: &RepoCapability) -> Readiness {
    if let Some(os) = &c.unsupported_on {
        return Readiness::Unsupported { os: os.clone() };
    }
    let prerequisites = c.entry.effective_prerequisites();
    let known_prerequisite = !prerequisites.is_empty() && prerequisites != "none";
    if let Some(driver) = &c.driver_missing {
        let needs = if known_prerequisite {
            prerequisites
        } else {
            driver
        };
        return Readiness::NeedsToolchain {
            needs: needs.to_string(),
        };
    }
    if c.driver_too_old {
        return Readiness::NeedsToolchain {
            needs: format!("a newer {prerequisites}"),
        };
    }
    // A bundled emitter that could not be found or started ships with travsr,
    // so `travsr init` cannot restore it; send the user to the reason instead.
    if c.entry.analyzer_bundled() {
        match c.last_warning {
            Some("emitter_missing") => return Readiness::PartMissing,
            Some("emitter_failed") => return Readiness::Failed,
            _ => {}
        }
    }
    let enabled = c.entry.builtin || (c.registered && c.corpus_trusted);
    if !enabled || !c.analyzer_ready {
        return Readiness::SettingUp;
    }
    match c.last_warning {
        Some(
            "crashed" | "zero_nodes" | "no_references" | "emitter_failed" | "emitter_missing"
            | "version_mismatch",
        ) => Readiness::Failed,
        _ => Readiness::Ready,
    }
}

/// This language's warning class in a stored `phase_b_warnings` value
/// (`class:lang,class:lang,...`; `version_mismatch:lang:expected:got` carries
/// more after the language).
fn warning_class<'a>(warnings: &'a str, language: &str) -> Option<&'a str> {
    warnings
        .split(',')
        .filter_map(|w| w.trim().split_once(':'))
        .find(|(_, rest)| rest.split(':').next() == Some(language))
        .map(|(class, _)| class)
}

/// Section 3.0: words that must not reach default output, as whole words,
/// case-insensitive. `Node.js` is a real tool the user installs, so it is
/// allowed although `node` is not.
const JARGON: &[&str] = &[
    "phase a",
    "phase b",
    "semantic",
    "lsif",
    "scip",
    "sidecar",
    "wrapper",
    "analyzer",
    "corpus",
    "trust grant",
    "registered",
    "sandbox",
    "bwrap",
    "unsandboxed",
    "daemon",
    "node",
    "nodes",
    "edge",
    "edges",
    "schema",
    "provenance",
    "vname",
    "ppr",
    "knapsack",
    "seed",
    "knn",
    "live overlay",
    "live lane",
    "control socket",
    "rust_log",
    "lang install",
    "lang add",
    "allow-unsandboxed",
    "init --semantic",
    "<lang>",
];

/// The first section 3.0 banned word in `text`, or `None` when it reads plainly.
/// Every surface's string tests share this, so the list cannot drift.
pub fn jargon_in(text: &str) -> Option<&'static str> {
    let text = text.to_lowercase().replace("node.js", "");
    let word = |c: char| c.is_alphanumeric() || c == '_';
    JARGON.iter().copied().find(|term| {
        text.match_indices(term).any(|(i, _)| {
            let before = text[..i].chars().next_back();
            let after = text[i + term.len()..].chars().next();
            let edge_ok = |c: Option<char>, t: Option<char>| {
                // A term that starts or ends in punctuation needs no boundary there.
                t.is_some_and(|t| !word(t)) || c.map_or(true, |c| !word(c))
            };
            edge_ok(before, term.chars().next()) && edge_ok(after, term.chars().next_back())
        })
    })
}

/// Fill [`RepoCapability`] for one language in `repo_root`. `warnings` is the
/// store's `phase_b_warnings` meta, passed in because this crate has no store.
pub fn gather<'a>(
    entry: &'a PhaseBEntry,
    repo_root: &std::path::Path,
    corpus: &str,
    lang_toml: &crate::trust::LangToml,
    resolver: &crate::resolver::CatalogResolver,
    warnings: &'a str,
) -> RepoCapability<'a> {
    use crate::resolver::PluginResolver as _;
    use travsr_core::exec::tool_available;

    let analyzer_ready = if entry.builtin {
        analyzer_present(entry)
    } else {
        // The indexer runs dart's emitter without the resolver, so registration
        // is the whole test for it (same as MCP `phase_b_availability`).
        entry.language == "dart" || resolver.resolve(entry.language).is_some()
    };
    let driver_missing = entry
        .runtime_driver
        .filter(|d| !tool_available(d))
        .or(match entry.scip_install {
            // The install command's driver only matters while there is
            // something left to install.
            super::catalog::ScipInstall::Command(args)
                if !analyzer_ready && !tool_available(args[0]) =>
            {
                Some(args[0])
            }
            // Nothing travsr can install: the tool itself is the prerequisite.
            super::catalog::ScipInstall::Manual if !tool_available(entry.command) => {
                Some(entry.command)
            }
            _ => None,
        })
        .map(str::to_string)
        .or_else(|| {
            (entry.command == "scip-clang" && !repo_root.join("compile_commands.json").exists())
                .then(|| "compile_commands.json".to_string())
        });
    let driver_too_old = match entry.scip_install {
        super::catalog::ScipInstall::GithubBinary(ref spec) if !analyzer_ready => {
            spec.fallback_min_go.is_some_and(|min| {
                let prebuilt = super::platform::current_target()
                    .is_some_and(|t| (spec.asset_fn)(spec.version_fallback, t).is_some());
                !prebuilt && installed_go().is_some_and(|go| go < min)
            })
        }
        _ => false,
    };
    RepoCapability {
        entry,
        unsupported_on: super::platform::unsupported_reason(entry),
        driver_missing,
        driver_too_old,
        registered: lang_toml.registered.iter().any(|r| r == entry.language),
        corpus_trusted: lang_toml.trusted_corpora.contains(corpus),
        analyzer_ready,
        last_warning: warning_class(warnings, entry.language),
    }
}

/// The installed Go as (major, minor), or `None` when there is no `go`.
fn installed_go() -> Option<(u32, u32)> {
    let out = std::process::Command::new("go")
        .arg("version")
        .output()
        .ok()?;
    parse_go_version(&String::from_utf8_lossy(&out.stdout))
}

/// `go version go1.23.4 darwin/amd64` -> `(1, 23)`.
fn parse_go_version(text: &str) -> Option<(u32, u32)> {
    let ver = text.split_whitespace().nth(2)?.strip_prefix("go")?;
    let mut parts = ver.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// Whether `entry`'s analyzer is on this machine: a bundled one with its Node
/// runtime, or an external one whose binaries resolve.
pub fn analyzer_present(entry: &PhaseBEntry) -> bool {
    if entry.analyzer_bundled() {
        bundled_analyzer_ready(entry)
    } else {
        entry
            .provider_binary
            .map_or(true, travsr_core::exec::tool_available)
            && analyzer_command_present(entry)
    }
}

/// Whether a bundled analyzer's hidden interpreter is present. travsr-lsif-ts
/// and travsr-lsif-py ship as JS files run through `node` — "bundled" only
/// means the emitter file itself needs no separate install, not that Node.js
/// is guaranteed to exist on the machine. True when the entry declares no such
/// hidden driver (nothing to check).
pub fn bundled_analyzer_ready(entry: &PhaseBEntry) -> bool {
    // Both halves are required and neither implies the other: node is the
    // runtime, the emitter is the program it runs. Checking only node is what
    // let `lang install typescript` answer "full cross-file analysis is on" in
    // a repo where `travsr status` reported the analyzer could not be started.
    entry
        .runtime_driver
        .map_or(true, travsr_core::exec::tool_available)
        && travsr_indexer::bundled_lsif_emitter_available(entry.language)
}

/// Whether the entry's analyzer command resolves on this machine.
///
/// Like `tool_available(entry.command)`, but also consults `rustup which` for
/// rust-analyzer: `rustup component add rust-analyzer` installs it into the
/// active toolchain's bin dir (`~/.rustup/toolchains/<tc>/bin`), which is not on
/// PATH and not in `~/.cargo/bin`, so `tool_available` alone can't see it. Every
/// analyzer-presence decision routes through here so `lang list`, `lang detect`,
/// `lang status`, `lang install`, and the index-time resolver never disagree.
pub fn analyzer_command_present(entry: &PhaseBEntry) -> bool {
    use travsr_core::exec::tool_available;
    let command_present = tool_available(entry.command)
        || (entry.command == "rust-analyzer"
            && travsr_indexer::ra_runner::resolve_ra_binary().is_some());
    command_present && entry.runtime_driver.map_or(true, tool_available)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phase_b::catalog::lookup;

    fn cap(lang: &str, analyzer_ready: bool, approved: bool) -> LangStatus {
        capability(&Capability {
            entry: lookup(lang).expect("known language"),
            analyzer_ready,
            approved,
            unsupported_on: None,
            windows_unsandboxed: false,
            unsandboxed_consent: false,
        })
    }

    #[test]
    fn analyzer_ready_reads_active_for_every_language() {
        for lang in ["python", "rust", "typescript", "go", "java"] {
            // Every language, including elevated ones (java) now that elevated
            // access is auto-granted, reads Active once its analyzer is ready.
            assert_eq!(cap(lang, true, false), LangStatus::Active, "{lang}");
        }
    }

    #[test]
    fn missing_analyzer_reads_partial_with_uniform_install_step() {
        // Rust is not special: no analyzer → partial, same as go.
        let rust = cap("rust", false, false);
        assert_eq!(rust.tag(), "partial");
        assert_eq!(
            rust,
            LangStatus::Partial {
                next: Some("travsr lang install rust".to_string())
            }
        );
        assert!(rust.line().contains("travsr lang install rust"));
        assert!(
            !rust.line().contains("rust-analyzer"),
            "no tool jargon in the line"
        );
    }

    #[test]
    fn elevated_language_is_auto_approved_and_reads_install() {
        // java is an elevated language, but elevated access is auto-granted for
        // local use (ADR-017 amendment): with the analyzer absent it reads the
        // uniform install step, never needs_approval.
        let java = cap("java", false, false);
        assert_eq!(java.tag(), "partial");
        assert!(java.line().contains("travsr lang install java"));
        assert!(cap("java", true, false).tag() != "needs_approval");
        // Auto-grant does not depend on the `approved` flag either way.
        assert_eq!(cap("java", true, true), LangStatus::Active);
    }

    #[test]
    fn unsupported_platform_stays_partial_not_dead() {
        let status = capability(&Capability {
            entry: lookup("objectivec").expect("known language"),
            analyzer_ready: false,
            approved: false,
            unsupported_on: Some("windows".to_string()),
            windows_unsandboxed: false,
            unsandboxed_consent: false,
        });
        assert_eq!(status.tag(), "unsupported");
        // Honest: structure still works, only full analysis is unavailable.
        assert!(status.line().starts_with("partial"));
        assert!(status.line().contains("windows"));
    }

    #[test]
    fn no_symbols_or_internal_jargon_in_any_rendered_line() {
        let lines = [
            LangStatus::Active.line(),
            LangStatus::Partial {
                next: Some("travsr lang install go".into()),
            }
            .line(),
            LangStatus::NeedsApproval {
                language: "java".into(),
            }
            .line(),
            LangStatus::NeedsConsent {
                language: "java".into(),
            }
            .line(),
            LangStatus::PlatformUnsupported {
                os: "windows".into(),
            }
            .line(),
        ];
        for l in lines {
            // `allow-unsandboxed` is a fixed command name mirroring the existing
            // `travsr init --allow-unsandboxed` (rust) precedent, not explanatory
            // jargon. Strip that one literal token before scanning so the guard
            // still catches any "sandbox" used to describe the mechanism in prose.
            let prose = l.replace("allow-unsandboxed", "");
            for banned in [
                "Phase B", "phase b", "sandbox", "SCIP", "LSIF", "built-in", "✓", "⚠", "–",
            ] {
                assert!(
                    !prose.contains(banned),
                    "line {l:?} must not contain {banned:?}"
                );
            }
        }
    }

    #[test]
    fn windows_unsupported_language_without_consent_needs_consent() {
        // java on Windows: analyzer installed, no permission on record → the line
        // asks for the one-time permission and names the exact command.
        let status = capability(&Capability {
            entry: lookup("java").expect("known language"),
            analyzer_ready: true,
            approved: true,
            unsupported_on: None,
            windows_unsandboxed: true,
            unsandboxed_consent: false,
        });
        assert_eq!(status.tag(), "needs_consent");
        assert!(status.line().starts_with("partial"));
        assert!(status.line().contains("travsr lang allow-unsandboxed java"));
    }

    #[test]
    fn windows_unsupported_language_with_consent_is_active() {
        let status = capability(&Capability {
            entry: lookup("scala").expect("known language"),
            analyzer_ready: true,
            approved: false, // consent replaces approval on this path
            unsupported_on: None,
            windows_unsandboxed: true,
            unsandboxed_consent: true,
        });
        assert_eq!(status, LangStatus::Active);
    }

    fn repo(lang: &str) -> RepoCapability<'static> {
        RepoCapability {
            entry: lookup(lang).expect("known language"),
            unsupported_on: None,
            driver_missing: None,
            driver_too_old: false,
            registered: true,
            corpus_trusted: true,
            analyzer_ready: true,
            last_warning: None,
        }
    }

    #[test]
    fn go_version_reads_major_and_minor() {
        assert_eq!(
            parse_go_version("go version go1.23.4 darwin/amd64"),
            Some((1, 23))
        );
        assert_eq!(
            parse_go_version("go version go1.25 windows/amd64"),
            Some((1, 25))
        );
        assert_eq!(parse_go_version("go version devel +abc"), None);
    }

    #[test]
    fn readiness_ladder_every_rung() {
        let needs = |s: &str| Readiness::NeedsToolchain { needs: s.into() };
        let cases: Vec<(&str, RepoCapability, Readiness)> = vec![
            ("all set", repo("go"), Readiness::Ready),
            (
                "no build for this os wins over everything",
                RepoCapability {
                    unsupported_on: Some("windows".into()),
                    driver_missing: Some("go".into()),
                    ..repo("go")
                },
                Readiness::Unsupported {
                    os: "windows".into(),
                },
            ),
            (
                "missing driver names the catalog prerequisite",
                RepoCapability {
                    driver_missing: Some("go".into()),
                    analyzer_ready: false,
                    ..repo("go")
                },
                needs("Go toolchain"),
            ),
            (
                // macOS Intel / Windows build scip-go with `go install`, which
                // needs a newer Go than 1.23: without this it read "setting up.
                // Run travsr init" after every run.
                "a driver too old to build the analyzer",
                RepoCapability {
                    driver_too_old: true,
                    analyzer_ready: false,
                    ..repo("go")
                },
                needs("a newer Go toolchain"),
            ),
            (
                "prerequisite 'none' falls back to the driver name",
                RepoCapability {
                    driver_missing: Some("swiftc".into()),
                    ..repo("swift")
                },
                needs("swiftc"),
            ),
            (
                // Nothing is missing, so "needs JDK, Maven or Gradle" would be
                // false (this repo's java fixtures have no pom.xml).
                "no symbols with every checked tool present",
                RepoCapability {
                    last_warning: Some("zero_nodes"),
                    ..repo("java")
                },
                Readiness::Failed,
            ),
            (
                "no symbols with the driver missing names the prerequisite",
                RepoCapability {
                    last_warning: Some("zero_nodes"),
                    driver_missing: Some("java".into()),
                    ..repo("java")
                },
                // Windows reports Gradle only (see `effective_prerequisites`).
                needs(lookup("java").unwrap().effective_prerequisites()),
            ),
            (
                "builtin without its analyzer is fixed by init",
                RepoCapability {
                    registered: false,
                    corpus_trusted: false,
                    analyzer_ready: false,
                    ..repo("rust")
                },
                Readiness::SettingUp,
            ),
            (
                "builtin needs no registration or trust",
                RepoCapability {
                    registered: false,
                    corpus_trusted: false,
                    ..repo("rust")
                },
                Readiness::Ready,
            ),
            (
                "not registered",
                RepoCapability {
                    registered: false,
                    ..repo("go")
                },
                Readiness::SettingUp,
            ),
            (
                // Bug 1 shape: set up in another repo, never trusted in this one.
                "registered but this repo untrusted",
                RepoCapability {
                    corpus_trusted: false,
                    ..repo("go")
                },
                Readiness::SettingUp,
            ),
            (
                "registered and trusted but analyzer not resolvable",
                RepoCapability {
                    analyzer_ready: false,
                    ..repo("go")
                },
                Readiness::SettingUp,
            ),
            (
                "unexplained crash",
                RepoCapability {
                    last_warning: Some("crashed"),
                    ..repo("go")
                },
                Readiness::Failed,
            ),
            (
                // python's prerequisite is travsr's own runtime (Node.js), which
                // `driver_missing` already checked, so nothing is left to blame.
                "no symbols from a bundled analyzer",
                RepoCapability {
                    last_warning: Some("zero_nodes"),
                    ..repo("python")
                },
                Readiness::Failed,
            ),
            (
                "builtin crash is not hidden behind ready",
                RepoCapability {
                    last_warning: Some("crashed"),
                    ..repo("typescript")
                },
                Readiness::Failed,
            ),
            (
                "a gate-skip warning from an earlier run does not outlive its fix",
                RepoCapability {
                    last_warning: Some("untrusted_corpus"),
                    ..repo("go")
                },
                Readiness::Ready,
            ),
        ];
        for (name, cap, want) in cases {
            assert_eq!(readiness(&cap), want, "{name}");
        }
    }

    #[test]
    fn readiness_tags_labels_and_fixes_are_plain() {
        let needs = |s: &str| Readiness::NeedsToolchain { needs: s.into() };
        let cases = [
            (Readiness::Ready, "ready", "ready", None),
            (
                Readiness::SettingUp,
                "setting_up",
                "setting up",
                Some("Run `travsr init` to finish tracing calls."),
            ),
            (
                needs("Go toolchain"),
                "needs_toolchain",
                "needs Go toolchain",
                Some("Install Go toolchain, then run `travsr init`."),
            ),
            (
                needs("compile_commands.json"),
                "needs_toolchain",
                "needs compile_commands.json",
                Some("Generate compile_commands.json with your build, then run `travsr init`."),
            ),
            (
                Readiness::Unsupported {
                    os: "windows".into(),
                },
                "unsupported_os",
                "not available on windows",
                None,
            ),
            (
                Readiness::Failed,
                "failed",
                "could not trace calls",
                Some("See `travsr status --verbose`."),
            ),
            (
                Readiness::PartMissing,
                "failed",
                "could not trace calls: part of travsr is missing",
                Some("Reinstall travsr, then run `travsr init`."),
            ),
        ];
        for (r, tag, label, fix) in cases {
            assert_eq!(r.tag(), tag);
            assert_eq!(r.label(), label, "{tag}");
            assert_eq!(r.fix().as_deref(), fix, "{tag}");
            let text = format!("{} {}", r.label(), r.fix().unwrap_or_default());
            assert_eq!(jargon_in(&text), None, "{tag}: {text:?}");
        }
    }

    /// Section 3.0's check itself: whole words, case-insensitive, with the one
    /// real tool name that contains a banned word allowed.
    #[test]
    fn jargon_in_finds_internal_words_only() {
        assert_eq!(jargon_in("Install Node.js, then run `travsr init`."), None);
        assert_eq!(jargon_in("the daemon is running"), Some("daemon"));
        assert_eq!(jargon_in("live overlay active"), Some("live overlay"));
        assert_eq!(
            jargon_in("run `travsr lang install <lang>`"),
            Some("lang install")
        );
        assert_eq!(jargon_in("see the Semantic pass"), Some("semantic"));
        assert_eq!(jargon_in("3 edges"), Some("edges"));
        assert_eq!(
            jargon_in("a knowledge edgeless design"),
            None,
            "whole words only"
        );
    }

    #[test]
    fn warning_class_is_read_for_this_language_only() {
        let w = "zero_nodes:java,crashed:go,scip_unification_misses:2/38,version_mismatch:php:2:1";
        assert_eq!(warning_class(w, "go"), Some("crashed"));
        assert_eq!(warning_class(w, "java"), Some("zero_nodes"));
        assert_eq!(warning_class(w, "php"), Some("version_mismatch"));
        assert_eq!(warning_class(w, "rust"), None);
        assert_eq!(warning_class("", "go"), None);
    }

    /// A pass that started and failed, or an analyzer too old to talk to, is
    /// not ready: `status` would otherwise call it ready over a warning.
    #[test]
    fn a_failed_type_checked_pass_or_old_analyzer_is_failed() {
        for class in ["emitter_failed", "version_mismatch"] {
            let cap = RepoCapability {
                last_warning: Some(class),
                ..repo("typescript")
            };
            assert_eq!(readiness(&cap), Readiness::Failed, "{class}");
        }
    }

    /// A bundled analyzer whose emitter could not be found (a travsr binary
    /// copied out of its install) is not "setting up": `travsr init` cannot
    /// bring back a file that ships with travsr, so that remedy would loop.
    #[test]
    fn a_missing_bundled_emitter_is_failed_not_setting_up() {
        for (class, want) in [
            ("emitter_failed", Readiness::Failed),
            ("emitter_missing", Readiness::PartMissing),
        ] {
            for analyzer_ready in [false, true] {
                let cap = RepoCapability {
                    analyzer_ready,
                    last_warning: Some(class),
                    ..repo("typescript")
                };
                assert_eq!(readiness(&cap), want, "{class}");
            }
        }
        // Without the record, an absent bundled analyzer is still set up by init.
        let cap = RepoCapability {
            analyzer_ready: false,
            ..repo("typescript")
        };
        assert_eq!(readiness(&cap), Readiness::SettingUp);
    }

    #[test]
    fn windows_unsupported_language_missing_analyzer_says_install_first() {
        // No analyzer yet: the honest next step is install, not the permission.
        let status = capability(&Capability {
            entry: lookup("java").expect("known language"),
            analyzer_ready: false,
            approved: true,
            unsupported_on: None,
            windows_unsandboxed: true,
            unsandboxed_consent: false,
        });
        assert_eq!(status.tag(), "partial");
        assert!(status.line().contains("travsr lang install java"));
    }
}
