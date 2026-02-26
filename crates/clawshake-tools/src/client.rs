//! Socket client for the `clawshake-bridge` IPC endpoint.
//!
//! The bridge daemon listens on a platform-specific local socket:
//! - **Windows**: named pipe `\\.\pipe\clawshake-bridge`
//! - **Linux / macOS**: Unix domain socket `/tmp/clawshake-bridge.sock`
//!
//! The wire protocol is newline-delimited JSON.
//! Request:  `{"method":"network_peers","params":{}}\n`
//! Response: `{ ... }\n`

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Named pipe path used on Windows.
#[cfg(windows)]
pub const SOCKET_PATH: &str = r"\\.\pipe\clawshake-bridge";

/// Unix domain socket path used on Linux / macOS.
#[cfg(not(windows))]
pub const SOCKET_PATH: &str = "/tmp/clawshake-bridge.sock";

/// Send a single `network.*` request to the bridge daemon and return the
/// JSON response.
///
/// Opens a fresh connection for each call — connections are short-lived since
/// this is the client side of a CLI subprocess invocation.  Rust MCP hosts
/// that call this in a hot path may want to pool connections in the future,
/// but for now simplicity wins.
pub async fn send_request(method: &str, params: Value) -> Result<Value> {
    let body = serde_json::json!({ "method": method, "params": params });
    let line = serde_json::to_string(&body).context("serialize request")? + "\n";

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;

        let pipe = ClientOptions::new()
            .open(SOCKET_PATH)
            .context("connect to bridge (is clawshake-bridge running?)")?;

        let (read_half, mut write_half) = tokio::io::split(pipe);
        write_half
            .write_all(line.as_bytes())
            .await
            .context("write request")?;
        // Signal end-of-write so the server can start reading (half-close).
        write_half.shutdown().await.context("shutdown write")?;

        let mut reader = BufReader::new(read_half);
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .await
            .context("read response")?;
        serde_json::from_str(response.trim()).context("parse response JSON")
    }

    #[cfg(not(windows))]
    {
        use tokio::net::UnixStream;

        let stream = UnixStream::connect(SOCKET_PATH)
            .await
            .context("connect to bridge (is clawshake-bridge running?)")?;

        let (read_half, mut write_half) = stream.into_split();
        write_half
            .write_all(line.as_bytes())
            .await
            .context("write request")?;
        write_half.shutdown().await.context("shutdown write")?;

        let mut reader = BufReader::new(read_half);
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .await
            .context("read response")?;
        serde_json::from_str(response.trim()).context("parse response JSON")
    }
}
