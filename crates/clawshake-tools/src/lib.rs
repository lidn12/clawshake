//! `clawshake-tools` — portable tool layer for the Clawshake broker.
//!
//! This crate provides:
//! - [`schema`] — MCP tool schema definitions for `network_*` and `shell`.
//!   Importable by any Rust MCP host (broker, etc.) to inject tools into their
//!   `tools/list` response without duplicating the schema.
//! - [`shell`] — in-process shell execution with safety guards.
//! - [`client`] — async socket client that talks to the running
//!   `clawshake-bridge` daemon.

pub mod cli;
pub mod client;
pub mod ipc;
pub mod network;
pub mod schema;
pub mod shell;

/// Named pipe path (Windows).
#[cfg(windows)]
pub const SOCKET_PATH: &str = r"\\.\pipe\clawshake-bridge";

/// Unix domain socket path (Linux / macOS).
#[cfg(not(windows))]
pub const SOCKET_PATH: &str = "/tmp/clawshake-bridge.sock";
