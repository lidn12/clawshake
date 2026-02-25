//! Shared domain types for the `network.*` P2P call channel.
//!
//! Used by both `clawshake-bridge` (swarm event loop, IPC listener)
//! and `clawshake-tools` (network.* handler logic).
//!
//! Kept in `clawshake-core` so neither consuming crate depends on the other.

use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
};

use tokio::sync::{mpsc, oneshot};

// ---------------------------------------------------------------------------
// Connected-peer tracker
// ---------------------------------------------------------------------------

/// A shared, cheaply-cloneable set of currently-connected peer ID strings.
/// Populated by the p2p event loop via `ConnectionEstablished` /
/// `ConnectionClosed` events.
pub type ConnectedPeers = Arc<RwLock<HashSet<String>>>;

/// Create a new empty [`ConnectedPeers`] set.
pub fn new_connected_peers() -> ConnectedPeers {
    Arc::new(RwLock::new(HashSet::new()))
}

// ---------------------------------------------------------------------------
// Outbound P2P call channel
// ---------------------------------------------------------------------------

/// A single outbound P2P tool call to be routed through the swarm.
/// `p2p::run()` owns the receiver and drives the send; callers hold a sender.
pub struct OutboundCall {
    /// Target peer ID string — parsed to `PeerId` inside the swarm loop.
    pub peer_id: String,
    /// Raw MCP JSON-RPC request bytes to send via the proxy behaviour.
    pub request: Vec<u8>,
    /// Oneshot channel to deliver the raw response bytes (or an error string).
    pub response_tx: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// Sender half of the outbound P2P call channel.
pub type OutboundCallTx = mpsc::Sender<OutboundCall>;

/// Create a new outbound call channel.
/// Pass the receiver to `p2p::run()`; keep the sender for IPC / network handlers.
pub fn new_outbound_call_channel() -> (OutboundCallTx, mpsc::Receiver<OutboundCall>) {
    mpsc::channel(16)
}
