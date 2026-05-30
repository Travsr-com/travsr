use std::sync::Arc;
use travsr_plugin_protocol::{HandshakeResponse, PROTOCOL_VERSION};
use crate::dispatcher::Dispatcher;
use crate::transport::InProcess;
use crate::plugins::typescript::TypeScriptPlugin;
use crate::plugins::rust::RustPlugin;
use crate::plugins::python::PythonPlugin;
use crate::plugins::go::GoPlugin;
use crate::plugins::java::JavaPlugin;
use crate::plugins::kotlin::KotlinPlugin;

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

    register!(TypeScriptPlugin, "typescript", &["ts", "tsx", "mts", "cts"], true);
    register!(RustPlugin,       "rust",       &["rs"],                       true);
    register!(PythonPlugin,     "python",     &["py", "pyi"],                false);
    register!(GoPlugin,         "go",         &["go"],                       false);
    register!(JavaPlugin,       "java",       &["java"],                     false);
    register!(KotlinPlugin,     "kotlin",     &["kt", "kts"],                false);
}
