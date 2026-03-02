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
use futures::io::{AsyncRead, AsyncWrite};
use libp2p::{
    request_response::{self, ProtocolSupport},
    StreamProtocol,
};
use std::io;

use async_trait::async_trait;

use crate::proxy::{read_framed, write_framed};

// ---------------------------------------------------------------------------
// Codec — length-prefixed StreamFrame JSON
// ---------------------------------------------------------------------------

/// Codec for the streaming protocol.
///
/// For the initial implementation we use libp2p's `request_response` with
/// the stream protocol ID.  Each "request" is a `StreamFrame` and each
/// "response" is also a `StreamFrame`.  This is a simplification — a true
/// bidirectional stream will use libp2p `Stream` in a future iteration.
///
/// This gets us protocol negotiation and connection reuse for free while
/// we build out the model proxy pipeline.
#[derive(Clone, Default)]
pub struct StreamCodec;

#[async_trait]
impl request_response::Codec for StreamCodec {
    type Protocol = StreamProtocol;
    type Request = Vec<u8>;
    type Response = Vec<u8>;

    async fn read_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_framed(io).await
    }

    async fn read_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_framed(io).await
    }

    async fn write_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_framed(io, &req).await
    }

    async fn write_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
        res: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_framed(io, &res).await
    }
}

// ---------------------------------------------------------------------------
// Behaviour type alias + constructor
// ---------------------------------------------------------------------------

/// The libp2p behaviour type for the streaming protocol.
pub type Behaviour = request_response::Behaviour<StreamCodec>;

/// Event type for the streaming protocol behaviour.
pub type Event = request_response::Event<Vec<u8>, Vec<u8>>;

/// Create a new streaming protocol behaviour.
pub fn new_behaviour() -> Behaviour {
    let protocol = StreamProtocol::try_from_owned(STREAM_PROTOCOL.to_string())
        .expect("valid stream protocol string");
    request_response::Behaviour::<StreamCodec>::new(
        vec![(protocol, ProtocolSupport::Full)],
        request_response::Config::default(),
    )
}
