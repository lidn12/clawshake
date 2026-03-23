use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;
use std::io::Write;

use crate::config::Config;

/// The operation being recorded.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Append,
    Reset,
    Compact,
}

impl Default for Op {
    fn default() -> Self {
        Op::Append
    }
}

/// A single entry to append to the transcript.
///
/// All fields beyond `role` and `content` are optional — omitted fields
/// are not written to JSON (`skip_serializing_if`), keeping lines compact
/// and identical in shape to what the ingestor expects.
#[derive(Debug, Clone, Serialize)]
pub struct WriteEntry {
    /// Transcript format version. Always 1 for now.
    pub v: u32,

    /// RFC 3339 timestamp. If not set, `append_entry` fills it from wall clock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,

    pub op: Op,

    /// user | assistant | tool | system
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,

    /// For tool result entries — the name of the tool that produced this result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Reason for reset or compact entries (e.g. "context_full").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl WriteEntry {
    /// Convenience constructor for a plain user or assistant turn.
    pub fn message(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            v: 1,
            ts: None,
            op: Op::Append,
            role: Some(role.into()),
            content: Some(content.into()),
            name: None,
            reason: None,
        }
    }

    /// Convenience constructor for a tool call entry (LLM → tool invocation request).
    ///
    /// `content` is the raw arguments JSON string as the LLM produced it.
    pub fn tool_call(name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            v: 1,
            ts: None,
            op: Op::Append,
            role: Some("assistant".into()),
            content: Some(content.into()),
            name: Some(name.into()),
            reason: None,
        }
    }

    /// Convenience constructor for a tool result entry.
    pub fn tool_result(name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            v: 1,
            ts: None,
            op: Op::Append,
            role: Some("tool".into()),
            content: Some(content.into()),
            name: Some(name.into()),
            reason: None,
        }
    }

}

/// Append a single entry to today's transcript JSONL file.
///
/// The file is created if it does not exist. The timestamp is filled
/// from the wall clock if not set on the entry. Writes are line-atomic
/// for append-only single-writer use.
pub fn append_entry(config: &Config, mut entry: WriteEntry) -> Result<()> {
    if entry.ts.is_none() {
        entry.ts = Some(Utc::now().to_rfc3339());
    }

    std::fs::create_dir_all(&config.transcript_dir)
        .with_context(|| format!("creating transcript dir {:?}", config.transcript_dir))?;

    let filename = Utc::now().format("%Y-%m-%d.jsonl").to_string();
    let path = config.transcript_dir.join(&filename);

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening transcript file {path:?}"))?;

    let line = serde_json::to_string(&entry).context("serializing transcript entry")?;

    writeln!(file, "{line}").with_context(|| format!("writing to {path:?}"))?;

    Ok(())
}
