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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
}
