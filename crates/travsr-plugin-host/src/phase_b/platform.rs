//! Platform availability: can a language's full-semantic analyzer actually run on
//! THIS operating system?
//!
//! One place, so `travsr lang list` (CLI), `get_lang_status` (MCP) and the VS Code
//! panel can never disagree about whether an install is even possible here. Two
//! independent reasons `travsr lang install <lang>` cannot reach full analysis:
//!
//!   1. the language's `travsr-lang-*` wrapper has no published build for this
//!      target (objectivec off Apple), and
//!   2. its analyzer ships only as a prebuilt GitHub-release binary with no asset
//!      for this target (scip-clang for c/cpp, scip-ruby, the swift emitter, all
//!      on Windows).
//!
//! Either one means the honest line is "not available on <os>", never
//! "run: install". Both checks are host-target parameterised, so the same code is
//! correct on Windows, macOS and Linux.

use super::catalog::{lookup, PhaseBEntry, ScipInstall};
use super::status::os_label;

/// The Rust target triple for the current machine, or `None` on a platform travsr
/// has no triple for. Mirrors `travsr-cli`'s download-side `current_target`; both
/// route through here so a naming rule and a capability claim cannot drift apart.
pub fn current_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

/// Target triples a *published* travsr-lang release contains wrappers for.
///
/// Not the same as the targets the release workflow can build: a target belongs
/// here only once a tag has actually shipped it. The CLI's `wrapper_release_drift`
/// test checks this against the live release inventory, so the two cannot drift
/// apart silently.
pub const WRAPPER_RELEASE_TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
];

/// Wrappers built by a macOS-only release job. `travsr-lang-objectivec` links
/// against libclang and shells out to `xcrun`, so it exists for Apple targets
/// only — on every other platform it has no published asset.
pub const MACOS_ONLY_WRAPPERS: &[&str] = &["travsr-lang-objectivec"];

/// True when a *published* travsr-lang release contains `binary_name` for
/// `target`. `false` means state the limitation, not fail with a raw 404.
pub fn wrapper_available(binary_name: &str, target: &str) -> bool {
    if !WRAPPER_RELEASE_TARGETS.contains(&target) {
        return false;
    }
    !MACOS_ONLY_WRAPPERS.contains(&binary_name) || target.ends_with("-apple-darwin")
}

/// The host target when `entry` needs a `travsr-lang-*` wrapper the release matrix
/// does not publish for this host, else `None`.
///
/// Reported only when the wrapper is not already present: a user who built one
/// themselves and put it on `PATH` has a working setup, and telling them it is
/// unavailable would be its own false statement. The gate is about what can be
/// *downloaded*, not about what can run.
pub fn wrapper_unavailable_target(entry: &PhaseBEntry) -> Option<&'static str> {
    let bin = entry.provider_binary?;
    let target = current_target()?;
    if wrapper_available(bin, target) || travsr_core::exec::tool_available(bin) {
        return None;
    }
    Some(target)
}

/// `Some(<os>)` when the language's analyzer is installed only from a prebuilt
/// GitHub-release binary that has no asset for the host platform — so `travsr lang
/// install <lang>` genuinely cannot provide it here. `None` for analyzers
/// installed via a command or manual step (go, scala's sbt, php's composer
/// package): those are not asset-gated, so the uniform install step still points
/// at a real path.
pub fn analyzer_unavailable_os(entry: &PhaseBEntry) -> Option<String> {
    let target = current_target()?;
    let no_asset = match &entry.scip_install {
        ScipInstall::GithubBinary(s) => (s.asset_fn)(s.version_fallback, target).is_none(),
        ScipInstall::CommandThenGithubGz(_, s) => {
            (s.asset_fn)(s.version_fallback, target).is_none()
        }
        // ZipBinary assets are platform-independent; Command/Manual are not
        // asset-gated. None of these can be "unavailable for this OS" by asset.
        _ => false,
    };
    no_asset.then(|| os_label().to_string())
}

/// The single "not available on <os>" reason for `entry`, or `None` when full
/// analysis can be reached on this machine. Combines both independent causes
/// (wrapper and analyzer) so every surface — CLI, MCP, extension — renders the
/// same verdict. This is the value every consumer feeds into
/// [`super::status::Capability::unsupported_on`].
pub fn unsupported_reason(entry: &PhaseBEntry) -> Option<String> {
    wrapper_unavailable_target(entry)
        .map(|_| os_label().to_string())
        .or_else(|| analyzer_unavailable_os(entry))
}

/// Whether full (cross-file) analysis can *never* run for `language` on the current
/// platform, because neither its wrapper nor its analyzer ships a build here. When
/// true, no `travsr lang install <lang>` invocation can help — the only honest UX
/// is "not available on <os>; structural analysis still works".
pub fn full_analysis_unavailable_here(language: &str) -> bool {
    lookup(language).is_some_and(|e| unsupported_reason(e).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_available_is_target_and_macos_gated() {
        // Objectivec ships for Apple targets only.
        assert!(wrapper_available(
            "travsr-lang-objectivec",
            "aarch64-apple-darwin"
        ));
        assert!(!wrapper_available(
            "travsr-lang-objectivec",
            "x86_64-pc-windows-msvc"
        ));
        assert!(!wrapper_available(
            "travsr-lang-objectivec",
            "x86_64-unknown-linux-gnu"
        ));
        // A normal wrapper ships for every published target.
        for t in WRAPPER_RELEASE_TARGETS {
            assert!(wrapper_available("travsr-lang-java", t), "{t}");
        }
        // An unpublished target is never available.
        assert!(!wrapper_available(
            "travsr-lang-java",
            "riscv64-unknown-none"
        ));
    }

    #[test]
    fn unsupported_reason_agrees_with_capability_inputs() {
        // Whatever the host, `unsupported_reason` and `full_analysis_unavailable_here`
        // must agree for every catalogued language.
        for entry in super::super::catalog::CATALOG {
            assert_eq!(
                unsupported_reason(entry).is_some(),
                full_analysis_unavailable_here(entry.language),
                "{}",
                entry.language
            );
        }
    }
}
