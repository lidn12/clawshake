//! Model proxy helpers for P2P model completion requests.
//!
//! Handles inbound model requests (single-response and streaming) as well as
//! driving outbound model streams.  Extracted from `p2p/mod.rs` to keep the
//! main event-loop file focused on swarm orchestration.

use anyhow::{Context, Result};
use clawshake_core::{
    config::AdvertiseModels,
    identity::AgentId,
    models::{ModelAnnounce, ModelRequest},
    permissions::{Decision, PermissionStore},
    stream::StreamFrame,
};
use clawshake_models::backend::ModelBackend;
use futures::{io::AsyncWriteExt, StreamExt};
use libp2p::{PeerId, StreamProtocol};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::codec;

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

/// Serialize a model-style error response:
/// `{ "error": { "message": "...", "type": "..." } }`
pub(super) fn model_error_bytes(message: impl Into<String>, error_type: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "error": {
            "message": message.into(),
            "type": error_type,
        }
    }))
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Model discovery
// ---------------------------------------------------------------------------

/// Query the model backend for available models, filtered by the advertise
/// configuration.  Returns an empty vec if no model backend is configured.
pub(super) async fn query_models(
    backend: &Option<ModelBackend>,
    advertise: &AdvertiseModels,
) -> Vec<ModelAnnounce> {
    let backend = match backend {
        Some(b) => b,
        None => return Vec::new(),
    };
    if advertise.is_none() {
        return Vec::new();
    }

    let all_models = match backend.list_models().await {
        Ok(m) => m,
        Err(e) => {
            warn!("Failed to query model backend: {e}");
            return Vec::new();
        }
    };

    let result = match advertise {
        AdvertiseModels::All(_) => all_models,
        AdvertiseModels::List(names) => all_models
            .into_iter()
            .filter(|m| names.iter().any(|n| n == &m.name))
            .collect(),
        AdvertiseModels::None(_) => Vec::new(),
    };

    if !result.is_empty() {
        let names: Vec<&str> = result.iter().map(|m| m.name.as_str()).collect();
        info!(count = result.len(), models = ?names, "Advertising models on the network");
    }

    result
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Parse a model request, check permissions, and generate a request ID.
///
/// Returns `Ok((ModelRequest, request_id))` on success, or `Err(error_bytes)`
/// suitable for sending back to the peer.
pub(super) async fn validate_model_request(
    request_bytes: &[u8],
    peer: &str,
    store: &PermissionStore,
) -> std::result::Result<(ModelRequest, String), Vec<u8>> {
    let req: ModelRequest = match serde_json::from_slice(request_bytes) {
        Ok(r) => r,
        Err(e) => {
            warn!(%peer, "Failed to parse model request: {e}");
            return Err(model_error_bytes(
                format!("Invalid model request: {e}"),
                "invalid_request_error",
            ));
        }
    };

    let agent_id = AgentId::P2p(peer.to_string());
    match store.check(&agent_id, &req.model).await {
        Decision::Allow => {
            info!(model = %req.model, %peer, "Model access granted");
        }
        _ => {
            warn!(model = %req.model, %peer, "Model access denied");
            return Err(model_error_bytes(
                format!(
                    "Permission denied: peer '{}' is not allowed to use model '{}'. \
                     Grant access with: clawshake permissions allow p2p:{} {}",
                    peer, req.model, peer, req.model
                ),
                "permission_denied",
            ));
        }
    }

    let request_id = format!(
        "p2p-{}-{}",
        peer.chars().take(8).collect::<String>(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );

    Ok((req, request_id))
}

// ---------------------------------------------------------------------------
// Request-response model handler (single response)
// ---------------------------------------------------------------------------

/// Handle an inbound model completion request from a peer.
///
/// Parses the request bytes as a `ModelRequest`, runs the completion against
/// the local model backend, collects all streamed chunks into a single
/// non-streaming OpenAI-compatible response, and returns the response bytes.
pub(super) async fn handle_model_request(
    backend: &ModelBackend,
    request_bytes: &[u8],
    peer: &str,
    store: &PermissionStore,
) -> Vec<u8> {
    let (req, request_id) = match validate_model_request(request_bytes, peer, store).await {
        Ok(v) => v,
        Err(bytes) => return bytes,
    };

    // Run the completion
    let stream = match backend.complete(&request_id, &req).await {
        Ok(s) => s,
        Err(e) => {
            warn!(%peer, "Model completion failed: {e}");
            return model_error_bytes(format!("Model backend error: {e}"), "server_error");
        }
    };

    // Collect all streamed chunks into a single response.
    // Each Chunk.data is an OpenAI streaming delta — we extract the content
    // text from each and concatenate.
    let mut content = String::new();
    let mut usage = None;
    let mut finish_reason = None;

    tokio::pin!(stream);
    while let Some(frame) = stream.next().await {
        match frame {
            StreamFrame::Chunk { data, .. } => {
                // Extract content from OpenAI delta format:
                // { "choices": [{ "delta": { "content": "..." }, "finish_reason": "stop" }] }
                if let Some(choices) = data.get("choices").and_then(|c| c.as_array()) {
                    for choice in choices {
                        if let Some(text) = choice
                            .get("delta")
                            .and_then(|d| d.get("content"))
                            .and_then(|c| c.as_str())
                        {
                            content.push_str(text);
                        }
                        if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                            finish_reason = Some(reason.to_string());
                        }
                    }
                }
            }
            StreamFrame::Done { meta, .. } => {
                usage = meta;
            }
            StreamFrame::Error { message, .. } => {
                warn!(%peer, "Model stream error: {message}");
                return model_error_bytes(message, "server_error");
            }
        }
    }

    // Build a non-streaming OpenAI-compatible response.
    let mut response = serde_json::json!({
        "id": request_id,
        "object": "chat.completion",
        "model": req.model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content,
            },
            "finish_reason": finish_reason.unwrap_or_else(|| "stop".to_string()),
        }],
    });

    if let Some(u) = usage {
        response["usage"] = u;
    }

    info!(model = %req.model, %peer, content_len = content.len(), "Model completion done");

    serde_json::to_vec(&response).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// libp2p-stream model streaming
// ---------------------------------------------------------------------------

/// Handle an inbound model stream from a remote peer.
///
/// Reads the `ModelRequest` from the stream, runs the completion against the
/// local model backend, and writes each `StreamFrame` back as it's produced.
/// The remote proxy reads these frames and forwards them as SSE events to
/// the HTTP client — giving true token-by-token streaming.
pub(super) async fn handle_inbound_model_stream(
    backend: &ModelBackend,
    mut stream: libp2p::swarm::Stream,
    peer: &str,
    store: &PermissionStore,
) -> Result<()> {
    // Read the request (length-prefixed).
    let request_bytes = codec::read_framed(&mut stream)
        .await
        .context("reading model request from stream")?;

    let (req, request_id) = match validate_model_request(&request_bytes, peer, store).await {
        Ok(v) => v,
        Err(deny_bytes) => {
            // Permission denied / parse error — send as a StreamFrame::Error.
            let err =
                StreamFrame::error("denied", String::from_utf8_lossy(&deny_bytes), Some(-32001));
            codec::write_framed(&mut stream, &err.to_bytes()).await?;
            stream.close().await?;
            return Ok(());
        }
    };

    // Run the completion — returns a stream of StreamFrames.
    let frame_stream = match backend.complete(&request_id, &req).await {
        Ok(s) => s,
        Err(e) => {
            let err = StreamFrame::error(&request_id, format!("Backend error: {e}"), Some(-32603));
            codec::write_framed(&mut stream, &err.to_bytes()).await?;
            stream.close().await?;
            return Ok(());
        }
    };

    // Forward each frame from the backend to the remote peer.
    tokio::pin!(frame_stream);
    while let Some(frame) = StreamExt::next(&mut frame_stream).await {
        let bytes = frame.to_bytes();
        if codec::write_framed(&mut stream, &bytes).await.is_err() {
            warn!(%peer, "Stream write failed — peer disconnected?");
            return Ok(());
        }
    }

    stream.close().await?;
    info!(model = %req.model, %peer, "Model stream completed");
    Ok(())
}

/// Drive an outbound model stream: open a bidirectional stream to the peer,
/// write the request, and read `StreamFrame`s back, forwarding each through
/// the mpsc channel to the proxy's SSE response.
pub(super) async fn drive_outbound_model_stream(
    control: &mut libp2p_stream::Control,
    peer: PeerId,
    protocol: StreamProtocol,
    request: &[u8],
    frame_tx: mpsc::Sender<Result<Vec<u8>, String>>,
) -> Result<()> {
    info!(%peer, "Opening model stream");

    let mut stream = control
        .open_stream(peer, protocol)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to open model stream to {peer}: {e}"))?;

    // Write the request (length-prefixed).
    codec::write_framed(&mut stream, request)
        .await
        .context("writing model request to stream")?;

    // Read frames until the stream is closed or a Done/Error frame arrives.
    loop {
        match codec::read_framed(&mut stream).await {
            Ok(frame_bytes) => {
                // Check if this is a terminal frame (Done or Error)
                let is_terminal = matches!(
                    StreamFrame::from_bytes(&frame_bytes),
                    Ok(StreamFrame::Done { .. }) | Ok(StreamFrame::Error { .. })
                );

                if frame_tx.send(Ok(frame_bytes)).await.is_err() {
                    warn!(%peer, "Frame receiver dropped — client disconnected?");
                    break;
                }

                if is_terminal {
                    break;
                }
            }
            Err(e) => {
                // EOF = stream closed by peer (normal after Done frame).
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    break;
                }
                let _ = frame_tx.send(Err(format!("Stream read error: {e}"))).await;
                break;
            }
        }
    }

    let _ = stream.close().await;
    info!(%peer, "Model stream finished");
    Ok(())
}
