//! #760: the per-language Phase B warning classes, defined once.
//!
//! The daemon stamps a language's Phase B outcome into the `phase_b_warnings`
//! meta key as comma-separated `class:lang[:extra]` entries. Three surfaces read
//! it back: `travsr status` (travsr-cli), the MCP `get_index_status` tool
//! (travsr-mcp), and that tool's guard test. Before this, each of them carried
//! its own hand-written list of the class names and nothing derived from the
//! writer, so a class added to the daemon and forgotten in a consumer was
//! silently dropped: it fell through to the availability ladder and could
//! surface as a terminal `done`, telling the user the language SUCCEEDED when it
//! did not. `zero_nodes` and `needs_consent` sat in exactly that hole, invisible
//! to the guard because the guard was itself a third hand-written list with
//! nothing to disagree with.
//!
//! So this enum is the one source. The daemon formats its entries from
//! [`PhaseBWarningClass::entry`], and each consumer's guard iterates
//! [`PhaseBWarningClass::ALL`] and asserts that consumer handles every variant.
//! Adding a class here without teaching both consumers about it fails the build,
//! and there is no third list to keep in step.
//!
//! It lives in travsr-plugin-host because that is the crate the classes actually
//! come from: every variant below is one field of
//! [`PhaseBOutcome`](crate::PhaseBOutcome), which this crate produces, and it is
//! the only crate all three surfaces already depend on (travsr-daemon depends on
//! travsr-mcp, so the daemon itself cannot hold a definition travsr-mcp reads).
//! The shape follows [`LangStatus::tag`](super::status::LangStatus::tag): a
//! stable machine tag per variant, never reworded.

/// One per-language Phase B warning class.
///
/// Repo-wide diagnostics that are not a per-language state are deliberately not
/// here: `scip_unification_misses` is a missed/attempted rate for the whole
/// index, and neither consumer treats it as a language's status, so putting it
/// in [`ALL`](Self::ALL) would force both guards to assert something false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseBWarningClass {
    /// The analyzer was found and spawned but died or errored mid-invoke.
    Crashed,
    /// #712: the analyzer ran cleanly and produced no graph output at all, even
    /// though the language is present in the repo.
    ZeroNodes,
    /// #724: the analyzer returned definitions and not one occurrence, so no
    /// call edge can be derived from it.
    NoReferences,
    /// The sidecar speaks a different plugin protocol version than expected.
    /// The only class that carries extra fields: `lang:expected:got`.
    VersionMismatch,
    /// Vestigial since elevated access became auto-granted for local use
    /// (ADR-017 Amendment A5). This build never writes it, but a pre-upgrade
    /// index can still hold it in stored meta, so both consumers still decode it.
    NeedsApproval,
    /// Windows only: the analyzer cannot run inside Travsr's isolation and the
    /// user has not granted permission to run it with their own privileges.
    NeedsConsent,
    /// #449: the language is present in the repo but not registered in lang.toml.
    SkippedUnregistered,
    /// #414 (ADR-017 Rule 3): registered globally, but this repository's corpus
    /// has no trust grant, so the sidecar was never spawned.
    UntrustedCorpus,
    /// Registered, but the analyzer binary could not be resolved.
    SkippedNoAnalyzer,
    /// L5a: scip-clang (c/cpp) needs a `compile_commands.json` at the repo root
    /// and there is none.
    SkippedNoCompdb,
}

impl PhaseBWarningClass {
    /// Every class the daemon can write. The guards in travsr-cli and travsr-mcp
    /// iterate this, so a variant added here must be handled by both.
    pub const ALL: [PhaseBWarningClass; 10] = [
        PhaseBWarningClass::Crashed,
        PhaseBWarningClass::ZeroNodes,
        PhaseBWarningClass::NoReferences,
        PhaseBWarningClass::VersionMismatch,
        PhaseBWarningClass::NeedsApproval,
        PhaseBWarningClass::NeedsConsent,
        PhaseBWarningClass::SkippedUnregistered,
        PhaseBWarningClass::UntrustedCorpus,
        PhaseBWarningClass::SkippedNoAnalyzer,
        PhaseBWarningClass::SkippedNoCompdb,
    ];

    /// The stable machine tag written to `phase_b_warnings`. Never reworded: it
    /// is persisted in every existing index and read back by `travsr status` and
    /// the MCP tool, so it is an API surface, not UI copy.
    pub fn tag(&self) -> &'static str {
        match self {
            PhaseBWarningClass::Crashed => "crashed",
            PhaseBWarningClass::ZeroNodes => "zero_nodes",
            PhaseBWarningClass::NoReferences => "no_references",
            PhaseBWarningClass::VersionMismatch => "version_mismatch",
            PhaseBWarningClass::NeedsApproval => "needs_approval",
            PhaseBWarningClass::NeedsConsent => "needs_consent",
            PhaseBWarningClass::SkippedUnregistered => "skipped_unregistered",
            PhaseBWarningClass::UntrustedCorpus => "untrusted_corpus",
            PhaseBWarningClass::SkippedNoAnalyzer => "skipped_no_analyzer",
            PhaseBWarningClass::SkippedNoCompdb => "skipped_no_compdb",
        }
    }

    /// The `class:lang` entry the daemon writes for `lang`.
    /// [`VersionMismatch`](Self::VersionMismatch) appends `:expected:got` to this.
    pub fn entry(&self, lang: &str) -> String {
        format!("{}:{lang}", self.tag())
    }

    /// A complete, well-formed entry for `lang`, including whatever extra fields
    /// the class carries. Exists so a consumer guard can iterate
    /// [`ALL`](Self::ALL) and feed each class a decodable entry without keeping a
    /// second list of which classes carry what, which is the drift #760 removed.
    pub fn sample_entry(&self, lang: &str) -> String {
        match self {
            PhaseBWarningClass::VersionMismatch => format!("{}:2:1", self.entry(lang)),
            _ => self.entry(lang),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #760: `ALL` is what both consumer guards iterate, so a duplicated or
    /// missing tag would quietly weaken them rather than fail loudly here.
    #[test]
    fn every_class_has_a_distinct_tag_and_a_decodable_sample() {
        let tags: std::collections::HashSet<&str> =
            PhaseBWarningClass::ALL.iter().map(|c| c.tag()).collect();
        assert_eq!(
            tags.len(),
            PhaseBWarningClass::ALL.len(),
            "two classes share a tag, so one of them can never be decoded"
        );
        for class in PhaseBWarningClass::ALL {
            let sample = class.sample_entry("go");
            let (tag, rest) = sample.split_once(':').expect("class:lang at minimum");
            assert_eq!(tag, class.tag());
            assert!(
                rest.starts_with("go"),
                "the language must follow the tag: {sample}"
            );
        }
        // The one class with extra fields carries them in the sample, so a guard
        // that only knows about `ALL` still hands it something decodable.
        assert_eq!(
            PhaseBWarningClass::VersionMismatch.sample_entry("go"),
            "version_mismatch:go:2:1"
        );
    }
}
