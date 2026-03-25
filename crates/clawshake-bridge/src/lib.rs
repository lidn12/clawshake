//! `clawshake-bridge` as a library.
//!
//! Exposes the P2P transport stack so the unified `clawshake` binary can
//! embed it alongside the broker registry without spawning a separate process.

pub mod announce;
pub mod cli;
pub(crate) mod codec;
pub(crate) mod ipc_server;
pub(crate) mod network_tools;
pub mod p2p;
pub(crate) mod watcher;
