//! `clawshake-tools` — portable `network_*` tool layer for the Clawshake network.
//!
//! This crate provides:
//! - [`schema`] — MCP tool schema definitions for all `network_*` tools.
//!   Importable by any Rust MCP host (broker, etc.) to inject network tools
//!   into their `tools/list` response without duplicating the schema.
//! - [`client`] — async socket client that talks to the running
//!   `clawshake-bridge` daemon.  Used both by the CLI binary (each invocation
//!   is a short-lived process) and by Rust MCP hosts that want in-process
//!   request dispatch without spawning a subprocess.

pub mod cli;
pub mod client;
pub mod ipc;
pub mod network;
pub mod schema;

/// Named pipe path (Windows).
#[cfg(windows)]
pub const SOCKET_PATH: &str = r"\\.\pipe\clawshake-bridge";

/// Unix domain socket path (Linux / macOS).
#[cfg(not(windows))]
pub const SOCKET_PATH: &str = "/tmp/clawshake-bridge.sock";
