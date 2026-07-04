#![forbid(unsafe_code)]
//! travsr-plugin-protocol — Plugin trait, wire message types, and frame codec.

pub mod codec;
pub mod ffi_marker;
pub mod language_map;
pub mod plugin;
pub mod types;

pub use codec::{decode_message, decode_message_limited, encode_message, write_message};
pub use ffi_marker::{FfiMarker, FfiMarkerKind};
pub use language_map::{language_from_proto_str, language_id_from_proto_str, language_to_proto_str};
pub use plugin::Plugin;
pub use types::{
    GoldenFixture, HandshakeRequest, HandshakeResponse, InvokeRequest, InvokeResponse,
    ParseRequest, ParseResponse, PluginError, PluginRequest, PluginResponse, PROTOCOL_VERSION,
};
