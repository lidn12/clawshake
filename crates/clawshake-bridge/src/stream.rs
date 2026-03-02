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
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use libp2p::{
    request_response::{self, ProtocolSupport},
    StreamProtocol,
};
use std::io;

use async_trait::async_trait;

/// Maximum payload per frame (16 MiB, same as MCP protocol).
const MAX_FRAME_PAYLOAD: u32 = 16 * 1024 * 1024;

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
        read_frame(io).await
    }

    async fn read_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_frame(io).await
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
        write_frame(io, &req).await
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
        write_frame(io, &res).await
    }
}

async fn read_frame<T: AsyncRead + Unpin + Send>(io: &mut T) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    io.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("stream frame too large: {len} bytes"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    io.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn write_frame<T: AsyncWrite + Unpin + Send>(io: &mut T, data: &[u8]) -> io::Result<()> {
    let len = (data.len() as u32).to_be_bytes();
    io.write_all(&len).await?;
    io.write_all(data).await?;
    io.flush().await?;
    Ok(())
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
