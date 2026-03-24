//! Length-prefixed framing for P2P tunnel protocols.
//!
//! The tunnel transport uses 4-byte big-endian length prefixes followed by
//! that many bytes of payload.  [`read_framed`] and [`write_framed`] are the
//! shared primitives used by both TCP tunnel headers and `_rpc` messages.

use std::io;

use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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
