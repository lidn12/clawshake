//! Re-exports of `network.*` types and handlers from their canonical crates.
//!
//! Domain types live in `clawshake_core::network_channel`.
//! Handler logic lives in `clawshake_tools::network`.
//!
//! Everything is re-exported here so that `p2p`, `ipc`, and `main` can keep
//! their existing `network::X` paths with no further changes.

pub use clawshake_core::network_channel::{
    new_connected_peers, new_outbound_call_channel, ConnectedPeers, OutboundCall, OutboundCallTx,
};
pub use clawshake_tools::network::handle;

// ---------------------------------------------------------------------------
// (no handler logic here — all moved to clawshake-tools::network)
