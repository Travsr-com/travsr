//! `travsr-ipc` — platform-agnostic IPC transport for the Travsr daemon control plane.
//!
//! # Why this crate?
//! The daemon's control plane previously lived in `travsr-daemon` behind `#[cfg(unix)]`,
//! making the daemon uncompilable on Windows. This crate extracts:
//! - [`ControlMessage`] / [`ControlResponse`] — wire types (no platform gates)
//! - [`ControlAddr`] — blake3-derived socket/pipe name so hook and daemon always agree
//! - [`ControlTransport`] — sync client trait for CLI use
//! - [`unix::UnixTransport`] — Unix domain socket impl
//! - [`windows::NamedPipeTransport`] — Windows Named Pipe impl (RFC-013, full since v0.8.0)

#![forbid(unsafe_code)]

pub mod addr;
pub mod message;
pub mod transport;

#[cfg(unix)]
pub mod unix;

#[cfg(windows)]
pub mod windows;

pub use addr::ControlAddr;
pub use message::{ControlMessage, ControlResponse, QUERY_PROTOCOL_VERSION};
pub use transport::ControlTransport;
