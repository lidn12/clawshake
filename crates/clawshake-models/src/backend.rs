//! Backend adapter for local model servers.
//!
//! Connects to any OpenAI-compatible endpoint (Ollama, vLLM, llama.cpp,
//! text-generation-inference, LocalAI) and translates streaming completions
//! into [`StreamFrame`]s for forwarding over P2P.

use anyhow::{Context, Result};
use clawshake_core::models::{ModelAnnounce, ModelRequest};
use clawshake_core::stream::StreamFrame;
use futures::Stream;
use serde::Deserialize;
use std::pin::Pin;
use tokio_stream::StreamExt;
use tracing::warn;

/// A backend adapter that talks to a local OpenAI-compatible model server.
#[derive(Debug, Clone)]
pub struct ModelBackend {
    endpoint: String,
    client: reqwest::Client,
}

impl ModelBackend {
    /// Create a new backend pointing at the given base URL
    /// (e.g. `"http://127.0.0.1:11434"`).
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            client: reqwest::Client::new(),
        }
    }

    /// Query the backend for available models.
    ///
    /// Tries the OpenAI-compatible `/v1/models` endpoint first.
    /// Falls back to Ollama's `/api/tags` if that fails.
    pub async fn list_models(&self) -> Result<Vec<ModelAnnounce>> {
        // Try OpenAI format first
        let url = format!("{}/v1/models", self.endpoint);
        match self.client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: OpenAiModelsResponse = resp
                    .json()
                    .await
                    .context("parsing /v1/models response")?;
                return Ok(body
                    .data
                    .into_iter()
                    .map(|m| ModelAnnounce {
                        name: m.id,
                        context_length: None,
                        params: None,
                    })
                    .collect());
            }
            _ => {}
        }

        // Fall back to Ollama's native endpoint
        let url = format!("{}/api/tags", self.endpoint);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("querying /api/tags")?;
        let body: OllamaTagsResponse = resp
            .json()
            .await
            .context("parsing /api/tags response")?;
        Ok(body
            .models
            .into_iter()
            .map(|m| ModelAnnounce {
                name: m.name,
                context_length: None,
                params: m.details.and_then(|d| d.parameter_size),
            })
            .collect())
    }

    /// Send a chat completion request to the backend and return a stream of
    /// [`StreamFrame`]s suitable for forwarding over P2P.
    ///
    /// The backend always requests streaming mode (`stream: true`) so we can
    /// forward tokens as they arrive.
    pub async fn complete(
        &self,
        request_id: &str,
        req: &ModelRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamFrame> + Send>>> {
        let url = format!("{}/v1/chat/completions", self.endpoint);

        let resp = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .await
            .context("sending completion request to model backend")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Ok(Box::pin(tokio_stream::once(StreamFrame::error(
                request_id,
                format!("Model backend returned {status}: {body}"),
                Some(-32603),
            ))));
        }

        let rid = request_id.to_string();
        let byte_stream = resp.bytes_stream();

        // SSE streams come as `data: {json}\n\n` lines.
        // We accumulate partial lines and parse complete ones.
        let frame_stream = parse_sse_stream(byte_stream, rid);

        Ok(Box::pin(frame_stream))
    }
}

/// Parse an SSE byte stream into StreamFrames.
fn parse_sse_stream(
    byte_stream: impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    request_id: String,
) -> impl Stream<Item = StreamFrame> + Send + 'static {
    async_stream::stream! {
        let mut buffer = String::new();

        tokio::pin!(byte_stream);

        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    yield StreamFrame::error(&request_id, format!("Stream read error: {e}"), None);
                    return;
                }
            };

            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE lines
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim().to_string();
                buffer = buffer[pos + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                if let Some(data) = line.strip_prefix("data: ") {
                    let data = data.trim();
                    if data == "[DONE]" {
                        yield StreamFrame::done(&request_id);
                        return;
                    }

                    match serde_json::from_str::<serde_json::Value>(data) {
                        Ok(parsed) => {
                            // Extract usage from the final chunk if present
                            let usage = parsed.get("usage").cloned();

                            // Forward the delta as a Chunk
                            yield StreamFrame::Chunk {
                                request_id: request_id.clone(),
                                data: parsed,
                            };

                            // If this chunk had usage info and a stop finish_reason,
                            // the next line will be [DONE]
                            let _ = usage; // usage forwarded in the data; Done.meta will carry it
                        }
                        Err(e) => {
                            warn!(data, "Failed to parse SSE data as JSON: {e}");
                        }
                    }
                }
            }
        }

        // Stream ended without [DONE] — send Done anyway
        yield StreamFrame::done(&request_id);
    }
}

// ---------------------------------------------------------------------------
// Response types for model listing
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModel>,
}

#[derive(Debug, Deserialize)]
struct OpenAiModel {
    id: String,
}

#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    models: Vec<OllamaModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaModel {
    name: String,
    #[serde(default)]
    details: Option<OllamaModelDetails>,
}

#[derive(Debug, Deserialize)]
struct OllamaModelDetails {
    #[serde(default)]
    parameter_size: Option<String>,
}
