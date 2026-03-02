//! Generic streaming wire protocol — `/clawshake/stream/1.0.0`.
//!
//! This module defines the framing types for bidirectional streaming over
//! libp2p streams.  The protocol supports:
//!
//! - **Model completions** — tokens stream back over seconds/minutes.
//! - **Long-running tools** — progressive `run_code` output.
//! - **Tool calls** — non-streaming is the degenerate case: one Chunk + Done.
//!
//! # Wire format
//!
//! Each frame is length-delimited: a 4-byte big-endian `u32` length prefix
//! followed by that many bytes of UTF-8 JSON, identical to the existing
//! `/clawshake/mcp/1.0.0` framing.  The difference is that a single libp2p
//! stream carries multiple frames in both directions, rather than one
//! request + one response.
//!
//! # Protocol negotiation
//!
//! New nodes advertise both `/clawshake/stream/1.0.0` and the legacy
//! `/clawshake/mcp/1.0.0`.  When opening a connection the caller tries the
//! stream protocol first and falls back to the old one if the peer does not
//! support it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The libp2p stream protocol identifier.
pub const STREAM_PROTOCOL: &str = "/clawshake/stream/1.0.0";

// ---------------------------------------------------------------------------
// Frame types
// ---------------------------------------------------------------------------

/// A single frame in the streaming protocol.
///
/// Frames are tagged JSON objects.  Both sides of a stream can send any
/// variant at any time; the `request_id` field correlates frames belonging
/// to the same logical request/response exchange.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamFrame {
    /// Data chunk.  May be sent by either side.
    Chunk {
        /// Correlates frames belonging to the same logical request.
        request_id: String,
        /// Arbitrary JSON payload (model delta, tool result chunk, etc.).
        data: Value,
    },

    /// Signals successful completion of a request.
    Done {
        /// The request this completes.
        request_id: String,
        /// Optional metadata (e.g. token usage for model completions).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },

    /// Signals an error.
    Error {
        /// The request this error belongs to.
        request_id: String,
        /// Human-readable error message.
        message: String,
        /// Machine-readable error code (mirrors JSON-RPC conventions).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
    },
}

impl StreamFrame {
    /// Return the `request_id` regardless of variant.
    pub fn request_id(&self) -> &str {
        match self {
            Self::Chunk { request_id, .. } => request_id,
            Self::Done { request_id, .. } => request_id,
            Self::Error { request_id, .. } => request_id,
        }
    }

    /// Convenience: build a single-chunk "request" frame.
    pub fn request(request_id: impl Into<String>, data: Value) -> Self {
        Self::Chunk {
            request_id: request_id.into(),
            data,
        }
    }

    /// Convenience: build a Done frame with no metadata.
    pub fn done(request_id: impl Into<String>) -> Self {
        Self::Done {
            request_id: request_id.into(),
            meta: None,
        }
    }

    /// Convenience: build a Done frame with metadata.
    pub fn done_with_meta(request_id: impl Into<String>, meta: Value) -> Self {
        Self::Done {
            request_id: request_id.into(),
            meta: Some(meta),
        }
    }

    /// Convenience: build an Error frame.
    pub fn error(
        request_id: impl Into<String>,
        message: impl Into<String>,
        code: Option<i32>,
    ) -> Self {
        Self::Error {
            request_id: request_id.into(),
            message: message.into(),
            code,
        }
    }

    /// Serialize this frame to JSON bytes (for writing to the wire).
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("StreamFrame serialization cannot fail")
    }

    /// Deserialize a frame from JSON bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chunk_round_trip() {
        let frame = StreamFrame::Chunk {
            request_id: "req-1".into(),
            data: json!({"content": "hello"}),
        };
        let bytes = frame.to_bytes();
        let parsed = StreamFrame::from_bytes(&bytes).unwrap();
        assert_eq!(frame, parsed);
    }

    #[test]
    fn done_round_trip() {
        let frame = StreamFrame::done_with_meta("req-2", json!({"tokens": 42}));
        let bytes = frame.to_bytes();
        let parsed = StreamFrame::from_bytes(&bytes).unwrap();
        assert_eq!(frame, parsed);
    }

    #[test]
    fn error_round_trip() {
        let frame = StreamFrame::error("req-3", "something went wrong", Some(-32600));
        let bytes = frame.to_bytes();
        let parsed = StreamFrame::from_bytes(&bytes).unwrap();
        assert_eq!(frame, parsed);
    }

    #[test]
    fn done_without_meta_omits_field() {
        let frame = StreamFrame::done("req-4");
        let json_str = serde_json::to_string(&frame).unwrap();
        assert!(!json_str.contains("meta"));
    }

    #[test]
    fn request_id_accessor() {
        let chunk = StreamFrame::request("r1", json!({}));
        assert_eq!(chunk.request_id(), "r1");

        let done = StreamFrame::done("r2");
        assert_eq!(done.request_id(), "r2");

        let error = StreamFrame::error("r3", "oops", None);
        assert_eq!(error.request_id(), "r3");
    }

    #[test]
    fn serde_tag_format() {
        let frame = StreamFrame::request("r1", json!({"key": "val"}));
        let v: Value = serde_json::to_value(&frame).unwrap();
        assert_eq!(v["type"], "chunk");
        assert_eq!(v["request_id"], "r1");

        let done = StreamFrame::done("r2");
        let v: Value = serde_json::to_value(&done).unwrap();
        assert_eq!(v["type"], "done");

        let err = StreamFrame::error("r3", "fail", Some(500));
        let v: Value = serde_json::to_value(&err).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["code"], 500);
    }
}
