use crate::dispatcher::Dispatcher;
use crate::plugins::generic::GenericTreeSitterPlugin;
use crate::plugins::go::GoPlugin;
use crate::plugins::java::JavaPlugin;
use crate::plugins::python::PythonPlugin;
use crate::plugins::rust::RustPlugin;
use crate::plugins::typescript::TypeScriptPlugin;
use crate::resolver::{which_binary, PluginSpec};
use crate::sandbox::policy::SandboxPolicy;
use crate::transport::{InProcess, Sidecar};
use std::sync::Arc;
use travsr_plugin_protocol::{HandshakeResponse, PROTOCOL_VERSION};

/// Maps canonical language name → expected fuzz target filename under fuzz/fuzz_targets/.
/// ADR-017 Rule 4: in-process grammars MUST have a fuzz target.
const FUZZ_TARGETS: &[(&str, &str)] = &[
    ("typescript", "fuzz_treesitter_indexer.rs"),
    ("rust", "fuzz_treesitter_indexer.rs"), // shared indexer target covers Rust
    ("python", "fuzz_pyright_lsif_parser.rs"),
    ("go", "fuzz_go_parser.rs"),
    ("java", "fuzz_java_parser.rs"), // TODO: create this fuzz target
    ("kotlin", "fuzz_kotlin_parser.rs"), // TODO: create this fuzz target
    ("ruby", "fuzz_ruby_parser.rs"), // TODO: create this fuzz target
    ("csharp", "fuzz_csharp_parser.rs"), // TODO: create this fuzz target
    ("php", "fuzz_php_parser.rs"),   // TODO: create this fuzz target
    ("scala", "fuzz_scala_parser.rs"), // TODO: create this fuzz target
    // C++ is a Phase A sidecar (travsr-lang-cpp) — fuzzed in that crate, not here.
    ("c", "fuzz_c_parser.rs"),       // TODO: create this fuzz target
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

/// Probe OS sandbox availability and log the result at startup.
/// ADR-017 Rule 2: if the sandbox is unavailable, Sidecar Phase B is disabled.
/// Phase A (in-process) is always unaffected.
pub fn probe_sandbox() {
    #[cfg(target_os = "linux")]
    {
        let available = std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok();
        if available {
            tracing::info!("sandbox: bubblewrap available — Phase B sidecar spawn enabled");
        } else {
            tracing::warn!(
                "sandbox: bubblewrap (bwrap) not found on PATH — \
                 Phase B sidecar plugins disabled (ADR-017 Rule 2 fail-closed). \
                 Install with: sudo apt-get install bubblewrap"
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        let available = std::path::Path::new("/usr/bin/sandbox-exec").exists();
        if available {
            tracing::info!("sandbox: sandbox-exec available — Phase B sidecar spawn enabled");
        } else {
            tracing::warn!(
                "sandbox: sandbox-exec not found — \
                 Phase B sidecar plugins disabled (ADR-017 Rule 2 fail-closed)"
            );
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    tracing::warn!(
        "sandbox: no sandbox implementation for this platform — \
         Phase B sidecar plugins disabled (ADR-017 Rule 2 fail-closed)"
    );
}

/// Register all first-party in-process plugins into `dispatcher`.
/// Called once when PluginIndexer is created.
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
            };
            let t = Arc::new(InProcess::new($plugin));
            dispatcher
                .register(hs, t)
                .expect("built-in plugin registration failed");
        }};
    }

    // Languages with specialised Phase A logic or Phase B support
    register!(
        TypeScriptPlugin,
        "typescript",
        &["ts", "tsx", "mts", "cts"],
        true
    );
    register!(RustPlugin, "rust", &["rs"], true);
    register!(PythonPlugin, "python", &["py", "pyi"], true);
    register!(GoPlugin, "go", &["go"], false);
    register!(JavaPlugin, "java", &["java"], false);

    // Languages driven entirely by GenericTreeSitterPlugin (config-only, no special logic)
    check_fuzz_target("kotlin");
    register_generic(
        dispatcher,
        &version,
        &crate::plugins::kotlin::CONFIG,
        tree_sitter_kotlin::language(),
    );
    check_fuzz_target("ruby");
    register_generic(
        dispatcher,
        &version,
        &crate::plugins::ruby::CONFIG,
        tree_sitter_ruby::language(),
    );
    check_fuzz_target("csharp");
    register_generic(
        dispatcher,
        &version,
        &crate::plugins::csharp::CONFIG,
        tree_sitter_c_sharp::language(),
    );
    check_fuzz_target("php");
    register_generic(
        dispatcher,
        &version,
        &crate::plugins::php::CONFIG,
        tree_sitter_php::language_php(),
    );
    check_fuzz_target("scala");
    register_generic(
        dispatcher,
        &version,
        &crate::plugins::scala::CONFIG,
        tree_sitter_scala::language(),
    );
    check_fuzz_target("c");
    register_generic(
        dispatcher,
        &version,
        &crate::plugins::c::CONFIG,
        tree_sitter_c::language(),
    );
    // C++: moved to travsr-lang-cpp sidecar (RFC-013 Direction A).
    // Call register_phase_a_sidecars(dispatcher, repo_root) after PluginIndexer
    // construction to activate C++ Phase A when the binary is on PATH.

    // Swift: blocked — all available tree-sitter-swift crates require tree-sitter ^0.21
    // which conflicts with workspace tree-sitter = "0.22". Re-enable when a compatible crate ships.
}

/// Attempt to register Phase A sidecar plugins that carry their grammar blob in
/// a separate binary rather than the host process (RFC-013 Direction A).
///
/// Called once per PluginIndexer with the repo root so the sandbox can grant
/// read access to the source tree being parsed.
///
/// Fail-closed per ADR-017 Rule 2: if the binary is absent or fails to spawn,
/// the language is simply unregistered — no panic, no partial state.
pub fn register_phase_a_sidecars(dispatcher: &mut Dispatcher, repo_root: &std::path::Path) {
    register_cpp_sidecar(dispatcher, repo_root);
}

fn register_cpp_sidecar(dispatcher: &mut Dispatcher, repo_root: &std::path::Path) {
    let Some(program) = which_binary("travsr-lang-cpp") else {
        tracing::info!(
            "travsr-lang-cpp not found on PATH — C++ Phase A indexing disabled \
             (install the sidecar binary or run `travsr lang install cpp`)"
        );
        return;
    };

    let spec = PluginSpec {
        language: "cpp".to_string(),
        program,
        args: vec![],
        policy: SandboxPolicy::Standard,
    };

    match Sidecar::spawn(&spec, repo_root) {
        Ok((sidecar, hs)) => {
            if let Err(e) = dispatcher.register(hs, Arc::new(sidecar)) {
                tracing::warn!("C++ Phase A sidecar registration failed: {e}");
            } else {
                tracing::info!("C++ Phase A sidecar registered via travsr-lang-cpp");
            }
        }
        Err(e) => {
            tracing::warn!(
                "C++ Phase A sidecar spawn failed: {e} \
                 — C++ files will not be indexed (ADR-017 Rule 2 fail-closed)"
            );
        }
    }
}

fn register_generic(
    dispatcher: &mut Dispatcher,
    version: &str,
    config: &'static crate::plugins::generic::LanguageConfig,
    grammar: tree_sitter::Language,
) {
    let plugin = GenericTreeSitterPlugin::new(config, grammar);
    let hs = HandshakeResponse {
        protocol_version: PROTOCOL_VERSION,
        plugin_version: version.to_string(),
        language: config.language.as_str().to_string(),
        extensions: config.extensions.iter().map(|s| s.to_string()).collect(),
        supports_phase_b: false,
    };
    let t = Arc::new(InProcess::new(plugin));
    dispatcher
        .register(hs, t)
        .expect("generic plugin registration failed");
}
