//! Model proxy wire types.
//!
//! These types define the request/response format for proxying LLM completions
//! over the P2P network.  They follow the OpenAI chat completions shape so
//! the local proxy can translate with minimal mapping.
//!
//! The types are intentionally in `clawshake-core` (not `clawshake-models`)
//! so the bridge can handle model requests without depending on the full
//! model proxy crate.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// DHT advertisement
// ---------------------------------------------------------------------------

/// A model entry in the DHT announcement record.
///
/// Published alongside tools so peers can discover which models a node serves.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelAnnounce {
    /// Model identifier as reported by the backend (e.g. `"llama3.1:70b"`).
    pub name: String,

    /// Maximum context length in tokens, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,

    /// Human-readable parameter count (e.g. `"70B"`), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<String>,
}

// ---------------------------------------------------------------------------
// Request / response over the stream protocol
// ---------------------------------------------------------------------------

/// A chat completion request sent over `/clawshake/stream/1.0.0`.
///
/// Sent as the first `StreamFrame::Chunk` data payload when the initiator
/// wants a model completion from a peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    /// Which model to use (e.g. `"llama3.1:70b"`).
    pub model: String,

    /// Conversation messages.
    pub messages: Vec<Message>,

    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,

    /// Maximum tokens to generate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,

    /// Whether to stream the response.  Always `true` over P2P — individual
    /// tokens are forwarded as `StreamFrame::Chunk`s.  For non-streaming
    /// callers the local proxy collects and merges.
    #[serde(default = "default_true")]
    pub stream: bool,
}

fn default_true() -> bool {
    true
}

/// A single message in a chat conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// One of `"system"`, `"user"`, `"assistant"`.
    pub role: String,
    /// Message text content.
    pub content: String,
}

/// A streaming delta chunk — sent back inside `StreamFrame::Chunk` data.
///
/// Mirrors the OpenAI `chat.completion.chunk` shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDelta {
    pub choices: Vec<DeltaChoice>,
}

/// One choice in a streaming delta.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaChoice {
    pub index: u32,
    pub delta: DeltaContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// The incremental content in a streaming delta.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// Token usage stats — sent in `StreamFrame::Done.meta`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_request_default_stream_true() {
        let json = r#"{"model":"llama3.1:70b","messages":[{"role":"user","content":"hi"}]}"#;
        let req: ModelRequest = serde_json::from_str(json).unwrap();
        assert!(req.stream);
    }

    #[test]
    fn model_announce_minimal() {
        let ann = ModelAnnounce {
            name: "codestral:22b".into(),
            context_length: None,
            params: None,
        };
        let json = serde_json::to_string(&ann).unwrap();
        assert!(!json.contains("context_length"));
        assert!(!json.contains("params"));
    }

    #[test]
    fn model_announce_full() {
        let ann = ModelAnnounce {
            name: "llama3.1:70b".into(),
            context_length: Some(131072),
            params: Some("70B".into()),
        };
        let json = serde_json::to_string(&ann).unwrap();
        assert!(json.contains("131072"));
        assert!(json.contains("70B"));

        let parsed: ModelAnnounce = serde_json::from_str(&json).unwrap();
        assert_eq!(ann, parsed);
    }

    #[test]
    fn delta_round_trip() {
        let delta = ModelDelta {
            choices: vec![DeltaChoice {
                index: 0,
                delta: DeltaContent {
                    role: None,
                    content: Some("Hello".into()),
                },
                finish_reason: None,
            }],
        };
        let bytes = serde_json::to_vec(&delta).unwrap();
        let parsed: ModelDelta = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.choices[0].delta.content.as_deref(), Some("Hello"));
    }

    #[test]
    fn usage_round_trip() {
        let usage = ModelUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
        };
        let v = serde_json::to_value(&usage).unwrap();
        assert_eq!(v["total_tokens"], 150);
    }
}
