use crate::dispatcher::Dispatcher;
use crate::plugins::python::PythonPlugin;
use crate::plugins::rust::RustPlugin;
use crate::plugins::typescript::TypeScriptPlugin;
use crate::resolver::{which_binary, PluginSpec};
use crate::sandbox::policy::SandboxPolicy;
use crate::transport::{InProcess, LazySidecar};
use std::sync::Arc;
use travsr_plugin_protocol::{HandshakeResponse, PROTOCOL_VERSION};

/// Maps canonical language name → expected fuzz target filename under fuzz/fuzz_targets/.
/// ADR-017 Rule 4: in-process grammars MUST have a fuzz target.
/// Sidecar languages are covered by their `travsr-lang-*` crates' own tests
/// plus the shared `travsr-analysis` fixtures — not listed here.
const FUZZ_TARGETS: &[(&str, &str)] = &[
    ("typescript", "fuzz_treesitter_indexer.rs"),
    ("rust", "fuzz_treesitter_indexer.rs"), // shared indexer target covers Rust
    ("python", "fuzz_pyright_lsif_parser.rs"),
];

/// ADR-017 Rule 4 eligibility check: warn if a language registered as in-process
/// does not yet have a cargo-fuzz target. The language is still registered —
/// refusing to index would be worse than the missing fuzz coverage. Track the gap.
fn check_fuzz_target(language: &str) {
    let expected = FUZZ_TARGETS
        .iter()
        .find(|(lang, _)| *lang == language)
        .map(|(_, f)| *f);
    let Some(filename) = expected else { return };
    let path = std::path::Path::new("fuzz/fuzz_targets").join(filename);
    if !path.exists() {
        tracing::warn!(
            lang = language,
            missing = %path.display(),
            "ADR-017 Rule 4: in-process grammar '{}' has no fuzz target at {} \
             — add a cargo-fuzz target to satisfy the eligibility requirement",
            language, path.display()
        );
    }
}

/// Cached probe: log sandbox availability once per process lifetime.
/// `register_builtins` is called once per `PluginIndexer` creation; guard here
/// so the log line never repeats even when the indexer is recreated per-file.
static SANDBOX_PROBED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// Probe OS sandbox availability and log the result at startup.
/// ADR-017 Rule 2: if the sandbox is unavailable, sidecar spawn is disabled.
/// In-process Phase A (ts/js/rust/python) is always unaffected.
pub fn probe_sandbox() {
    SANDBOX_PROBED.get_or_init(|| {
        if sandbox_available() {
            tracing::info!("sandbox: available — sidecar plugin spawn enabled");
        } else {
            #[cfg(target_os = "linux")]
            tracing::warn!(
                "sandbox: bubblewrap (bwrap) not found on PATH — \
                 sidecar plugins disabled (ADR-017 Rule 2 fail-closed). \
                 Install with: sudo apt-get install bubblewrap"
            );
            #[cfg(not(target_os = "linux"))]
            tracing::warn!(
                "sandbox: not available on this platform — \
                 sidecar plugins disabled (ADR-017 Rule 2 fail-closed)"
            );
        }
    });
}

/// Cheap sandbox availability check (no subprocess spawned on macOS; one
/// cached probe on Linux).
fn sandbox_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        crate::sandbox::linux::bwrap_is_on_path()
    }
    #[cfg(target_os = "macos")]
    {
        std::path::Path::new("/usr/bin/sandbox-exec").exists()
    }
    #[cfg(target_os = "windows")]
    {
        true // AppContainer (RFC-014 WS4) needs no external binary
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

/// Register the in-process builtin plugins (RFC-013 D-3: ts/js, rust, python).
/// Called once when PluginIndexer is created. Every other language's Phase A
/// is served by a `travsr-lang-*` sidecar — see [`register_phase_a_sidecars`].
pub fn register_builtins(dispatcher: &mut Dispatcher) {
    // ADR-017 Rule 2: probe sandbox at startup so the user sees the status immediately.
    probe_sandbox();

    let version = env!("CARGO_PKG_VERSION").to_string();

    macro_rules! register {
        ($plugin:expr, $lang:expr, $exts:expr, $phase_b:expr) => {{
            // ADR-017 Rule 4: warn if no fuzz target for this in-process grammar.
            check_fuzz_target($lang);
            let hs = HandshakeResponse {
                protocol_version: PROTOCOL_VERSION,
                plugin_version: version.clone(),
                language: $lang.to_string(),
                extensions: $exts.iter().map(|s: &&str| s.to_string()).collect(),
                supports_phase_b: $phase_b,
                golden_fixture: None,
            };
            let t = Arc::new(InProcess::new($plugin));
            dispatcher
                .register(hs, t)
                .expect("built-in plugin registration failed");
        }};
    }

    register!(
        TypeScriptPlugin,
        "typescript",
        &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"],
        true
    );
    register!(RustPlugin, "rust", &["rs"], true);
    register!(PythonPlugin, "python", &["py", "pyi"], true);
}

// ── Phase A sidecar catalog (RFC-013 Direction A, D-3) ────────────────────────

/// One Phase A sidecar language: which binary provides it and which file
/// extensions route to it.
///
/// `extensions` MUST stay in sync with `travsr_core::Language::from_extension`
/// — the daemon's file walk only feeds files whose extension maps to a
/// `Language`. Enforced by the `catalog_extensions_match_core` test below.
///
/// Binary names match the Phase B catalog's `provider_binary` — one plugin
/// binary per language serves both phases (Phase B only where the binary's
/// handshake declares support).
pub struct PhaseASidecar {
    pub language: &'static str,
    pub binary: &'static str,
    pub extensions: &'static [&'static str],
}

/// All Phase A sidecar languages (everything except the ts/js/rust/python
/// builtins). Grammar blobs for parsing live in the sidecar binaries; the
/// host retains grammars only for retrieval-time skeletonization (RFC-017).
pub const PHASE_A_SIDECARS: &[PhaseASidecar] = &[
    PhaseASidecar {
        language: "go",
        binary: "travsr-lang-go",
        extensions: &["go"],
    },
    PhaseASidecar {
        language: "java",
        binary: "travsr-lang-java",
        extensions: &["java"],
    },
    PhaseASidecar {
        language: "kotlin",
        binary: "travsr-lang-kotlin",
        extensions: &["kt", "kts"],
    },
    PhaseASidecar {
        language: "ruby",
        binary: "travsr-lang-ruby",
        extensions: &["rb", "rake", "gemspec"],
    },
    PhaseASidecar {
        language: "csharp",
        binary: "travsr-lang-csharp",
        extensions: &["cs"],
    },
    PhaseASidecar {
        language: "php",
        binary: "travsr-lang-php",
        extensions: &["php", "phtml", "php8"],
    },
    PhaseASidecar {
        language: "scala",
        binary: "travsr-lang-scala",
        extensions: &["scala", "sc"],
    },
    PhaseASidecar {
        language: "cpp",
        binary: "travsr-lang-cpp",
        extensions: &["cpp", "cc", "cxx", "hpp", "hh", "hxx"],
    },
    PhaseASidecar {
        language: "c",
        binary: "travsr-lang-c",
        extensions: &["c", "h"],
    },
    PhaseASidecar {
        language: "swift",
        binary: "travsr-lang-swift",
        extensions: &["swift"],
    },
    PhaseASidecar {
        language: "dart",
        binary: "travsr-lang-dart",
        extensions: &["dart"],
    },
    PhaseASidecar {
        language: "objectivec",
        binary: "travsr-lang-objectivec",
        extensions: &["m", "mm"],
    },
];

/// Register all Phase A sidecar plugins from [`PHASE_A_SIDECARS`] through the
/// runtime SUBSCRIBE → AUTHORIZE → VERIFY → ADMIT lifecycle (RFC-013 D-4,
/// ADR-019 Rule 1).
///
/// Called once per PluginIndexer with the repo root so the sandbox can grant
/// read access to the source tree being parsed.
///
/// Per language:
/// - **SUBSCRIBE**: the binary is resolved from `~/.travsr/bin` / PATH.
/// - **AUTHORIZE**: sandbox availability (ADR-017 Rule 2). ADR-020 signature
///   verification is pending sidecar release signing — accepted gap.
/// - **VERIFY**: cached verdict from `~/.travsr/subscriptions.lock` when the
///   binary hash matches (ADR-019 Rules 4–5); otherwise the conformance probe
///   runs now (structural + determinism, reproducibility rule). HARD-rejected
///   binaries are never admitted; SOFT (environment) failures skip this run
///   and re-probe on the next trigger.
/// - **ADMIT**: extensions register against a [`LazySidecar`] transport —
///   the actual worker subprocess spawn is still deferred to the first file
///   of the language, and respawn reuses the cached verdict (ADR-018 Rule 3).
///
/// Tier (RFC-013 D-6): `Active(A+B)` vs `Active(A only)` is logged from the
/// verified `supports_phase_b` capability plus a live Phase B toolchain check
/// — a toolchain installed later upgrades the tier on the next registration
/// with no re-probe.
pub fn register_phase_a_sidecars(dispatcher: &mut Dispatcher, repo_root: &std::path::Path) {
    if !sandbox_available() {
        tracing::warn!(
            "sandbox unavailable — Phase A sidecar languages disabled \
             (ADR-017 Rule 2 fail-closed); ts/js/rust/python remain in-process"
        );
        return;
    }

    let version = env!("CARGO_PKG_VERSION").to_string();

    for entry in PHASE_A_SIDECARS {
        let Some(program) = which_binary(entry.binary) else {
            tracing::info!(
                "{} not found on PATH — {} Phase A indexing disabled \
                 (install the sidecar binary or run `travsr lang install {}`)",
                entry.binary,
                entry.language,
                entry.language
            );
            continue;
        };

        let spec = PluginSpec {
            language: entry.language.to_string(),
            program,
            args: vec![],
            policy: SandboxPolicy::Standard,
        };

        // VERIFY: cached verdict or conformance probe (once per process).
        let supports_phase_b = match crate::subscriptions::check_subscription(&spec, false) {
            crate::subscriptions::Verdict::Active { supports_phase_b } => supports_phase_b,
            crate::subscriptions::Verdict::Rejected { reason } => {
                tracing::warn!(
                    lang = entry.language,
                    %reason,
                    "plugin quarantined (HARD verdict) — not admitted; replace the \
                     binary or run a forced re-verify to clear"
                );
                continue;
            }
            crate::subscriptions::Verdict::Unverified { reason } => {
                tracing::warn!(
                    lang = entry.language,
                    %reason,
                    "cannot verify plugin (environment) — skipped this run, \
                     re-probed on the next trigger"
                );
                continue;
            }
            crate::subscriptions::Verdict::Disabled => {
                tracing::info!(
                    lang = entry.language,
                    "language disabled by user policy — run `travsr lang enable {}` to restore",
                    entry.language
                );
                continue;
            }
        };

        // D-6 tier: verified capability + live toolchain presence.
        let tool_present = crate::phase_b::catalog::lookup(entry.language)
            .map(|cat| which_binary(cat.command).is_some())
            .unwrap_or(false);
        let tier = if supports_phase_b && tool_present {
            "Active(A+B)"
        } else {
            "Active(A only)"
        };

        // ADMIT: register extensions against the lazy transport.
        let hs = HandshakeResponse {
            protocol_version: PROTOCOL_VERSION,
            plugin_version: version.clone(),
            language: entry.language.to_string(),
            extensions: entry.extensions.iter().map(|s| s.to_string()).collect(),
            // Phase B for sidecar languages routes through the resolver +
            // Phase B catalog (with its per-language sandbox policy), never
            // through this Phase A dispatcher registration.
            supports_phase_b: false,
            golden_fixture: None,
        };

        let transport = Arc::new(LazySidecar::new(spec, repo_root));
        if let Err(e) = dispatcher.register(hs, transport) {
            tracing::warn!(
                lang = entry.language,
                err = %e,
                "Phase A sidecar registration failed"
            );
        } else {
            tracing::debug!(
                lang = entry.language,
                binary = entry.binary,
                tier,
                "plugin admitted (lazy spawn)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_core::Language;

    /// Every catalog extension must route to the catalog's language in
    /// travsr-core, and the language string must be a valid proto string.
    /// A mismatch means the daemon walk and the dispatcher disagree about
    /// which files belong to which plugin.
    #[test]
    fn catalog_extensions_match_core() {
        for entry in PHASE_A_SIDECARS {
            let expected = travsr_plugin_protocol::language_from_proto_str(entry.language)
                .unwrap_or_else(|| panic!("unknown proto language: {}", entry.language));
            for ext in entry.extensions {
                assert_eq!(
                    Language::from_extension(ext),
                    Some(expected),
                    "extension {ext:?} does not map to {} in travsr-core",
                    entry.language
                );
            }
        }
    }

    /// Sidecar languages must not overlap the in-process builtins.
    #[test]
    fn catalog_does_not_overlap_builtins() {
        const BUILTINS: &[&str] = &["typescript", "rust", "python"];
        for entry in PHASE_A_SIDECARS {
            assert!(
                !BUILTINS.contains(&entry.language),
                "{} is both a builtin and a sidecar",
                entry.language
            );
        }
    }

    /// Every Phase A sidecar binary name must match the Phase B catalog's
    /// provider_binary for the same language — one binary serves both phases.
    #[test]
    fn sidecar_binaries_match_phase_b_catalog() {
        for entry in PHASE_A_SIDECARS {
            if let Some(cat) = crate::phase_b::catalog::lookup(entry.language) {
                if let Some(provider) = cat.provider_binary {
                    assert_eq!(
                        entry.binary, provider,
                        "{}: Phase A binary != Phase B provider_binary",
                        entry.language
                    );
                }
            }
        }
    }
}
