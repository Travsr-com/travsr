//! One honest, jargon-free, platform-aware description of a language's semantic
//! capability — rendered identically by every surface (CLI `lang list`/`status`,
//! `daemon status`, MCP `get_lang_status`, and the VS Code panel).
//!
//! Ground truth, true for every language:
//!   * Structural analysis (tree-sitter) is always available — no install, every
//!     language, always. It also produces best-effort call edges, but not full
//!     cross-file coverage.
//!   * Full cross-file semantic analysis needs the language's analyzer: bundled
//!     for python, an external tool for the rest (rust-analyzer for rust, a
//!     `travsr-lang-*` analyzer for go/java/…). Only python is full out of the box.
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
    /// A one-time security approval must be recorded before this language's
    /// analyzer (which reaches the network while indexing) can be installed.
    NeedsApproval { language: String },
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
}

/// Compute the capability-view status for one language. Same logic for every
/// language — nothing is special-cased.
pub fn capability(cap: &Capability) -> LangStatus {
    // No analyzer build for this OS: full can never run here (structure still does).
    if let Some(os) = &cap.unsupported_on {
        return LangStatus::PlatformUnsupported { os: os.clone() };
    }
    // Network-reaching analyzers need a one-time approval before they can install.
    if cap.entry.sandbox == super::catalog::SandboxRequirement::RequiresElevated && !cap.approved {
        return LangStatus::NeedsApproval {
            language: cap.entry.language.to_string(),
        };
    }
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
        })
    }

    #[test]
    fn analyzer_ready_reads_active_for_every_language() {
        for lang in ["python", "rust", "typescript", "go"] {
            // Elevated languages still need approval; test the non-elevated set here.
            if lookup(lang).unwrap().sandbox
                != crate::phase_b::catalog::SandboxRequirement::RequiresElevated
            {
                assert_eq!(cap(lang, true, false), LangStatus::Active, "{lang}");
            }
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
    fn elevated_language_without_approval_reads_needs_approval() {
        // java requires elevated approval; with the tool absent and unapproved it
        // must ask for approval, not claim "disabled".
        let java = cap("java", false, false);
        assert_eq!(java.tag(), "needs_approval");
        assert!(java.line().contains("travsr lang install java"));
    }

    #[test]
    fn unsupported_platform_stays_partial_not_dead() {
        let status = capability(&Capability {
            entry: lookup("objectivec").expect("known language"),
            analyzer_ready: false,
            approved: false,
            unsupported_on: Some("windows".to_string()),
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
            LangStatus::PlatformUnsupported {
                os: "windows".into(),
            }
            .line(),
        ];
        for l in lines {
            for banned in [
                "Phase B", "phase b", "sandbox", "SCIP", "LSIF", "built-in", "✓", "⚠", "–",
            ] {
                assert!(
                    !l.contains(banned),
                    "line {l:?} must not contain {banned:?}"
                );
            }
        }
    }
}
