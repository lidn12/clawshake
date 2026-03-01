//! Local IPC socket listener for the `clawshake-bridge` daemon.
//!
//! Exposes the `network_*` tool handlers to any process on the local machine
//! through a platform-specific socket:
//! - **Windows**: named pipe `\\.\pipe\clawshake-bridge`
//! - **Linux / macOS**: Unix domain socket `/tmp/clawshake-bridge.sock`
//!
//! # Wire protocol
//!
//! Newline-delimited JSON over a full-duplex stream connection.
//!
//! Each client sends exactly **one** request line then optionally shuts down
//! the write half.  The server reads one line, dispatches it, and writes one
//! response line back.
//!
//! ```text
//! → {"method":"network_peers","params":{}}\n
//! ← {"peers":[...]}\n
//! ```
//!
//! `clawshake-tools` CLI and the broker's in-process client both use this
//! protocol.  Any language that can open a named pipe / Unix socket can use
//! it directly.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error, warn};

use clawshake_core::{
    network_channel::{ConnectedPeers, DhtLookupTx, OutboundCallTx},
    peer_table::PeerTable,
};

/// Named pipe path (Windows).
#[cfg(windows)]
const SOCKET_PATH: &str = r"\\.\pipe\clawshake-bridge";

/// Unix domain socket path (Linux / macOS).
#[cfg(not(windows))]
const SOCKET_PATH: &str = "/tmp/clawshake-bridge.sock";

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the IPC listener forever.
///
/// Spawns a new Tokio task for each incoming connection.  Each connection
/// handles a single request/response pair then closes.
///
/// Intended to be started with `tokio::spawn(clawshake_tools::ipc::run(...))`
/// from `clawshake-bridge`'s `main`.
pub async fn run(
    table: Arc<PeerTable>,
    connected: ConnectedPeers,
    call_tx: OutboundCallTx,
    dht_tx: DhtLookupTx,
) -> Result<()> {
    #[cfg(windows)]
    run_windows(table, connected, call_tx, dht_tx).await?;

    #[cfg(not(windows))]
    run_unix(table, connected, call_tx, dht_tx).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Windows: named pipe listener
// ---------------------------------------------------------------------------

#[cfg(windows)]
async fn run_windows(
    table: Arc<PeerTable>,
    connected: ConnectedPeers,
    call_tx: OutboundCallTx,
    dht_tx: DhtLookupTx,
) -> Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(SOCKET_PATH)?;

    tracing::info!("IPC listener ready on {}", SOCKET_PATH);

    loop {
        if let Err(e) = server.connect().await {
            warn!("IPC pipe connect error: {e}");
            continue;
        }
        let client = std::mem::replace(
            &mut server,
            match ServerOptions::new().create(SOCKET_PATH) {
                Ok(s) => s,
                Err(e) => {
                    error!("IPC failed to create next pipe instance: {e}");
                    break;
                }
            },
        );

        let table2 = Arc::clone(&table);
        let connected2 = connected.clone();
        let call_tx2 = call_tx.clone();
        let dht_tx2 = dht_tx.clone();
        tokio::spawn(async move {
            handle_connection(client, table2, connected2, call_tx2, dht_tx2).await;
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Linux / macOS: Unix domain socket listener
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
async fn run_unix(
    table: Arc<PeerTable>,
    connected: ConnectedPeers,
    call_tx: OutboundCallTx,
    dht_tx: DhtLookupTx,
) -> Result<()> {
    use tokio::net::UnixListener;

    let _ = std::fs::remove_file(SOCKET_PATH);
    let listener = UnixListener::bind(SOCKET_PATH)?;
    tracing::info!("IPC listener ready on {}", SOCKET_PATH);

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                warn!("IPC accept error: {e}");
                continue;
            }
        };
        let table2 = Arc::clone(&table);
        let connected2 = connected.clone();
        let call_tx2 = call_tx.clone();
        let dht_tx2 = dht_tx.clone();
        tokio::spawn(async move {
            handle_connection(stream, table2, connected2, call_tx2, dht_tx2).await;
        });
    }
}

// ---------------------------------------------------------------------------
// Per-connection handler (generic over AsyncRead + AsyncWrite)
// ---------------------------------------------------------------------------

async fn handle_connection<S>(
    stream: S,
    table: Arc<PeerTable>,
    connected: ConnectedPeers,
    call_tx: OutboundCallTx,
    dht_tx: DhtLookupTx,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    match reader.read_line(&mut line).await {
        Ok(0) => return,
        Ok(_) => {}
        Err(e) => {
            warn!("IPC read error: {e}");
            return;
        }
    }

    let response = dispatch(line.trim(), &table, &connected, &call_tx, &dht_tx).await;

    let mut out = match serde_json::to_string(&response) {
        Ok(s) => s,
        Err(e) => {
            error!("IPC failed to serialize response: {e}");
            return;
        }
    };
    out.push('\n');

    if let Err(e) = write_half.write_all(out.as_bytes()).await {
        debug!("IPC write error (client may have disconnected): {e}");
    }
}

// ---------------------------------------------------------------------------
// Request dispatcher
// ---------------------------------------------------------------------------

async fn dispatch(
    line: &str,
    table: &Arc<PeerTable>,
    connected: &ConnectedPeers,
    call_tx: &OutboundCallTx,
    dht_tx: &DhtLookupTx,
) -> Value {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return serde_json::json!({ "error": format!("invalid JSON: {e}") }),
    };

    let method = match req["method"].as_str() {
        Some(m) => m,
        None => return serde_json::json!({ "error": "missing field: method" }),
    };

    let empty = Value::Object(Default::default());
    let params = req.get("params").unwrap_or(&empty);

    crate::network::handle(method, Some(params), table, connected, Some(call_tx), Some(dht_tx)).await
}
