//! `clawshake-bridge` as a library.
//!
//! Exposes the P2P transport stack so the unified `clawshake` binary can
//! embed it alongside the broker registry without spawning a separate process.

pub mod announce;
pub mod backend;
pub mod p2p;
pub mod proxy;
