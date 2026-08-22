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
