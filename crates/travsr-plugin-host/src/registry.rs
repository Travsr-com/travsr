use crate::dispatcher::Dispatcher;
use crate::plugins::generic::GenericTreeSitterPlugin;
use crate::plugins::go::GoPlugin;
use crate::plugins::java::JavaPlugin;
use crate::plugins::python::PythonPlugin;
use crate::plugins::rust::RustPlugin;
use crate::plugins::typescript::TypeScriptPlugin;
use crate::transport::InProcess;
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
    ("cpp", "fuzz_cpp_parser.rs"),   // TODO: create this fuzz target
    ("c", "fuzz_c_parser.rs"),       // TODO: create this fuzz target
    ("swift", "fuzz_swift_parser.rs"),
    ("dart", "fuzz_dart_parser.rs"),
    ("objectivec", "fuzz_objc_parser.rs"), // TODO(#345): create this fuzz target
];

/// Emitted at most once per process, like [`SANDBOX_PROBED`] below.
static FUZZ_CHECKED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// ADR-017 Rule 4 eligibility check: warn if a language registered as in-process
/// does not yet have a cargo-fuzz target. The language is still registered —
/// refusing to index would be worse than the missing fuzz coverage. Track the gap.
///
/// One aggregated line rather than one per language. This used to warn per
/// language per `PluginIndexer` creation, and since the indexer is recreated
/// per file, a single daemon start produced dozens of near-identical lines —
/// on a fresh repo the last 20 lines of the log were nothing else, which made
/// `travsr daemon logs` useless for the case it exists to serve. The
/// information is identical; only the repetition is gone.
fn check_fuzz_targets_once() {
    FUZZ_CHECKED.get_or_init(|| {
        let missing: Vec<&str> = FUZZ_TARGETS
            .iter()
            .filter(|(_, filename)| {
                !std::path::Path::new("fuzz/fuzz_targets")
                    .join(filename)
                    .exists()
            })
            .map(|(lang, _)| *lang)
            .collect();
        if !missing.is_empty() {
            tracing::warn!(
                event = "adr017.fuzz_target.missing",
                count = missing.len(),
                langs = %missing.join(","),
                "ADR-017 Rule 4: {} in-process grammar(s) have no fuzz target under \
                 fuzz/fuzz_targets — add cargo-fuzz targets to satisfy the eligibility \
                 requirement",
                missing.len()
            );
        }
    });
}

/// Cached probe: log sandbox availability once per process lifetime.
/// `register_builtins` is called once per `PluginIndexer` creation; guard here
/// so the log line never repeats even when the indexer is recreated per-file.
static SANDBOX_PROBED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// Probe OS sandbox availability and log the result at startup.
/// ADR-017 Rule 2: if the sandbox is unavailable, Sidecar Phase B is disabled.
/// Phase A (in-process) is always unaffected.
pub fn probe_sandbox() {
    SANDBOX_PROBED.get_or_init(|| {
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
    });
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
            check_fuzz_targets_once();
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
        &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"],
        true
    );
    register!(RustPlugin, "rust", &["rs"], true);
    register!(PythonPlugin, "python", &["py", "pyi"], true);
    register!(GoPlugin, "go", &["go"], false);
    register!(JavaPlugin, "java", &["java"], false);

    // Languages driven entirely by GenericTreeSitterPlugin (config-only, no special logic).
    // Grammar is obtained via config.get_grammar() inside GenericTreeSitterPlugin::new().
    for config in &[
        &crate::plugins::kotlin::CONFIG,
        &crate::plugins::ruby::CONFIG,
        &crate::plugins::csharp::CONFIG,
        &crate::plugins::php::CONFIG,
        &crate::plugins::scala::CONFIG,
        &crate::plugins::cpp::CONFIG,
        &crate::plugins::c::CONFIG,
        &crate::plugins::swift::CONFIG,
        &crate::plugins::dart::CONFIG,
        &crate::plugins::objc::CONFIG,
    ] {
        check_fuzz_targets_once();
        register_generic(dispatcher, &version, config);
    }
}

fn register_generic(
    dispatcher: &mut Dispatcher,
    version: &str,
    config: &'static crate::plugins::generic::LanguageConfig,
) {
    let plugin = GenericTreeSitterPlugin::new(config);
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
