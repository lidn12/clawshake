//! Shared domain types for the `network_*` P2P call channel.
//!
//! Used by both `clawshake-bridge` (swarm event loop, IPC listener)
//! and `clawshake-tools` (network_* handler logic).
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

// ---------------------------------------------------------------------------
// DHT lookup channel
// ---------------------------------------------------------------------------

/// A request to fetch a peer's DHT announcement record on-demand.
/// The swarm event loop owns the receiver; `network_tools` / `network_record`
/// send through the sender, then await the oneshot for the result.
pub struct DhtLookup {
    /// Target peer ID string.
    pub peer_id: String,
    /// Oneshot channel to deliver the parsed `PeerInfo` (or an error string).
    pub response_tx: oneshot::Sender<Result<crate::peer_table::PeerInfo, String>>,
}

/// Sender half of the DHT lookup channel.
pub type DhtLookupTx = mpsc::Sender<DhtLookup>;

/// Create a new DHT lookup channel.
/// Pass the receiver to `p2p::run()`; keep the sender for IPC / network handlers.
pub fn new_dht_lookup_channel() -> (DhtLookupTx, mpsc::Receiver<DhtLookup>) {
    mpsc::channel(16)
}

// ---------------------------------------------------------------------------
// Outbound stream (model proxy) call channel
// ---------------------------------------------------------------------------

/// A single outbound model completion request to be routed through the swarm's
/// stream protocol to a specific peer.
///
/// Sent by the local model proxy (`clawshake-models`) when a client calls
/// `POST /v1/chat/completions` with a `model@peer_id` model ID.
pub struct OutboundStreamCall {
    /// Target peer ID string — parsed to `PeerId` inside the swarm loop.
    pub peer_id: String,
    /// Raw JSON bytes of the [`ModelRequest`](crate::models::ModelRequest).
    pub request: Vec<u8>,
    /// Oneshot channel to deliver the raw response bytes (or an error string).
    pub response_tx: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// Sender half of the outbound stream call channel.
pub type OutboundStreamCallTx = mpsc::Sender<OutboundStreamCall>;

/// Create a new outbound stream call channel.
/// Pass the receiver to `p2p::run()`; keep the sender for the model proxy.
pub fn new_outbound_stream_call_channel() -> (OutboundStreamCallTx, mpsc::Receiver<OutboundStreamCall>) {
    mpsc::channel(16)
}
