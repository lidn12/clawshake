//! IPC client and constants for the `clawshake-bridge` daemon socket.
//!
//! The bridge daemon listens on a platform-specific local socket:
//! - **Windows**: named pipe `\\.\pipe\clawshake-bridge`
//! - **Linux / macOS**: Unix domain socket `/tmp/clawshake-bridge.sock`
//!
//! # Wire protocol
//!
//! Newline-delimited JSON over a full-duplex stream connection.
//!
//! ```text
//! → {"method":"network_peers","params":{}}\n
//! ← {"peers":[...]}\n
//! ```

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Named pipe path (Windows).
#[cfg(windows)]
pub const SOCKET_PATH: &str = r"\\.\pipe\clawshake-bridge";

/// Unix domain socket path (Linux / macOS).
#[cfg(not(windows))]
pub const SOCKET_PATH: &str = "/tmp/clawshake-bridge.sock";

/// Send a single request to the bridge daemon and return the JSON response.
///
/// Opens a fresh connection for each call — connections are short-lived.
pub async fn send_request(method: &str, params: Value) -> Result<Value> {
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        send_request_inner(method, params),
    )
    .await
    .context("IPC request to bridge timed out after 15 s")?
}

async fn send_request_inner(method: &str, params: Value) -> Result<Value> {
    let body = serde_json::json!({ "method": method, "params": params });
    let line = serde_json::to_string(&body).context("serialize request")? + "\n";

    #[cfg(windows)]
    let stream = {
        use tokio::net::windows::named_pipe::ClientOptions;
        ClientOptions::new()
            .open(SOCKET_PATH)
            .context("connect to bridge (is clawshake-bridge running?)")?
    };

    #[cfg(not(windows))]
    let stream = {
        use tokio::net::UnixStream;
        UnixStream::connect(SOCKET_PATH)
            .await
            .context("connect to bridge (is clawshake-bridge running?)")?
    };

    let (read_half, mut write_half) = tokio::io::split(stream);
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
