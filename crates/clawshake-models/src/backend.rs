//! Backend adapter for local model servers.
//!
//! Connects to any OpenAI-compatible endpoint (Ollama, vLLM, llama.cpp,
//! text-generation-inference, LocalAI) for model discovery.  The backend
//! is queried at startup and on reannounce to populate the DHT with the
//! list of models this node serves.
//!
//! Completion requests flow through TCP tunnels — the model proxy proxies
//! raw HTTP to the backend via the tunnel infrastructure.

use anyhow::{Context, Result};
use clawshake_core::models::ModelAnnounce;
use serde::Deserialize;

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
                let body: OpenAiModelsResponse =
                    resp.json().await.context("parsing /v1/models response")?;
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
        let body: OllamaTagsResponse = resp.json().await.context("parsing /api/tags response")?;
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
