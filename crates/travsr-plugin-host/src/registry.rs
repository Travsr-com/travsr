use std::sync::Arc;
use travsr_plugin_protocol::{HandshakeResponse, PROTOCOL_VERSION};
use crate::dispatcher::Dispatcher;
use crate::transport::InProcess;
use crate::plugins::generic::GenericTreeSitterPlugin;
use crate::plugins::typescript::TypeScriptPlugin;
use crate::plugins::rust::RustPlugin;
use crate::plugins::python::PythonPlugin;
use crate::plugins::go::GoPlugin;
use crate::plugins::java::JavaPlugin;

/// Register all first-party in-process plugins into `dispatcher`.
/// Called once when PluginIndexer is created.
pub fn register_builtins(dispatcher: &mut Dispatcher) {
    let version = env!("CARGO_PKG_VERSION").to_string();

    macro_rules! register {
        ($plugin:expr, $lang:expr, $exts:expr, $phase_b:expr) => {{
            let hs = HandshakeResponse {
                protocol_version: PROTOCOL_VERSION,
                plugin_version: version.clone(),
                language: $lang.to_string(),
                extensions: $exts.iter().map(|s: &&str| s.to_string()).collect(),
                supports_phase_b: $phase_b,
            };
            let t = Arc::new(InProcess::new($plugin));
            dispatcher.register(hs, t).expect("built-in plugin registration failed");
        }};
    }

    // Languages with specialised Phase A logic or Phase B support
    register!(TypeScriptPlugin, "typescript", &["ts", "tsx", "mts", "cts"], true);
    register!(RustPlugin,       "rust",       &["rs"],                       true);
    register!(PythonPlugin,     "python",     &["py", "pyi"],                false);
    register!(GoPlugin,         "go",         &["go"],                       false);
    register!(JavaPlugin,       "java",       &["java"],                     false);

    // Languages driven entirely by GenericTreeSitterPlugin (config-only, no special logic)
    register_generic(dispatcher, &version, &crate::plugins::kotlin::CONFIG, tree_sitter_kotlin::language());
    register_generic(dispatcher, &version, &crate::plugins::ruby::CONFIG,   tree_sitter_ruby::language());
    register_generic(dispatcher, &version, &crate::plugins::csharp::CONFIG, tree_sitter_c_sharp::language());
    register_generic(dispatcher, &version, &crate::plugins::php::CONFIG,    tree_sitter_php::language_php());
    // Swift: blocked — all available tree-sitter-swift crates require tree-sitter ^0.21
    // which conflicts with workspace tree-sitter = "0.22". Re-enable when a compatible crate ships.
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
    dispatcher.register(hs, t).expect("generic plugin registration failed");
}
