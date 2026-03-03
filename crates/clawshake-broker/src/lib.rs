//! `clawshake-broker` as a library.
//!
//! Exposes the manifest registry, invoke router, MCP server surface, and
//! HTTP SSE transport so the unified `clawshake` binary can embed them.

pub mod builtins;
pub mod cli;
pub mod event_queue;
pub mod http_server;
pub mod invoke;
pub mod mcp_server;
pub mod router;
pub mod watcher;
