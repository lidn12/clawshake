//! Stream protocol handler for `/clawshake/stream/1.0.0`.
//!
//! This module provides the libp2p behaviour for the generic streaming
//! protocol.  Unlike the request-response `/clawshake/mcp/1.0.0` protocol,
//! streams carry multiple frames in both directions — suitable for model
//! completions, progressive tool output, and large transfers.
//!
//! Uses the same length-delimited framing as the MCP protocol (4-byte BE
//! u32 prefix), but frames are [`StreamFrame`] JSON objects rather than
//! full JSON-RPC envelopes.

use clawshake_core::stream::STREAM_PROTOCOL;
use libp2p::{
    request_response::{self, ProtocolSupport},
    StreamProtocol,
};

use crate::codec::LengthPrefixedCodec;

// ---------------------------------------------------------------------------
// Behaviour type alias + constructor
// ---------------------------------------------------------------------------

/// The libp2p behaviour type for the streaming protocol.
pub type Behaviour = request_response::Behaviour<LengthPrefixedCodec>;

/// Event type for the streaming protocol behaviour.
pub type Event = request_response::Event<Vec<u8>, Vec<u8>>;

/// Create a new streaming protocol behaviour.
pub fn new_behaviour() -> Behaviour {
    let protocol = StreamProtocol::try_from_owned(STREAM_PROTOCOL.to_string())
        .expect("valid stream protocol string");
    request_response::Behaviour::<LengthPrefixedCodec>::new(
        vec![(protocol, ProtocolSupport::Full)],
        request_response::Config::default(),
    )
}
