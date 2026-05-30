#![forbid(unsafe_code)]
//! travsr-plugin-sdk — ergonomic plugin authoring for the Travsr plugin system.

pub use travsr_plugin_protocol::{
    FfiMarker, FfiMarkerKind, HandshakeRequest, HandshakeResponse, InvokeRequest, InvokeResponse,
    ParseRequest, ParseResponse, Plugin, PluginError, PluginRequest, PluginResponse,
    PROTOCOL_VERSION,
};
// Re-export core types so language crates only need travsr-plugin-sdk as a dep.
pub use travsr_core::{Edge, Language, Node, NodeId, VName};

mod runner;
pub use runner::run_plugin;
