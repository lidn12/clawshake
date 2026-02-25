//! `clawshake-tools` — portable `network.*` tool layer for the Clawshake network.
//!
//! This crate provides:
//! - [`schema`] — MCP tool schema definitions for all `network.*` tools.
//!   Importable by any Rust MCP host (broker, etc.) to inject network tools
//!   into their `tools/list` response without duplicating the schema.
//! - [`client`] — async socket client that talks to the running
//!   `clawshake-bridge` daemon.  Used both by the CLI binary (each invocation
//!   is a short-lived process) and by Rust MCP hosts that want in-process
//!   request dispatch without spawning a subprocess.

pub mod client;
pub mod ipc;
pub mod network;
pub mod schema;
