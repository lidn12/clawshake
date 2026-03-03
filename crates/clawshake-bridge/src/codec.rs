//! Length-prefixed framing for libp2p request-response protocols.
//!
//! Both `/clawshake/mcp/1.0.0` and `/clawshake/stream/1.0.0` use the same
//! wire format: a 4-byte big-endian length prefix followed by that many bytes
//! of payload.  This module provides the shared framing helpers and a single
//! [`LengthPrefixedCodec`] that can be used with any protocol.

use std::io;

use async_trait::async_trait;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use libp2p::{request_response, StreamProtocol};

/// Maximum payload we'll accept from a remote peer (16 MiB).
pub(crate) const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;

/// Read one length-prefixed frame from an async reader.
pub(crate) async fn read_framed<T: AsyncRead + Unpin + Send>(io: &mut T) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    io.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("payload too large: {len} bytes"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    io.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Write one length-prefixed frame to an async writer.
pub(crate) async fn write_framed<T: AsyncWrite + Unpin + Send>(
    io: &mut T,
    data: &[u8],
) -> io::Result<()> {
    let len = (data.len() as u32).to_be_bytes();
    io.write_all(&len).await?;
    io.write_all(data).await?;
    io.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Generic codec
// ---------------------------------------------------------------------------

/// A codec that length-prefixes `Vec<u8>` payloads.
///
/// Works identically for any protocol — the protocol string is specified when
/// constructing the `request_response::Behaviour`, not at the codec level.
#[derive(Clone, Default)]
pub struct LengthPrefixedCodec;

#[async_trait]
impl request_response::Codec for LengthPrefixedCodec {
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
