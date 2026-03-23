use anyhow::{Context, Result};
use chrono::DateTime;
use serde::Deserialize;
use serde_json::Value;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use crate::config::Config;
use crate::db::chunks::chunk_hash;
use crate::db::Db;
use crate::ingest::{snap_to_char_boundary, split_chunks, ChunkStrategy, SourceIngestor};

/// A raw transcript entry as it appears in the JSONL file.
#[derive(Debug, Deserialize)]
pub(crate) struct TranscriptEntry {
    #[allow(dead_code)]
    v: u32,
    pub(crate) ts: String,
    pub(crate) op: String,
    #[serde(default)]
    pub(crate) role: Option<String>,
    #[serde(default)]
    pub(crate) content: Option<Value>,
    #[serde(default)]
    pub(crate) tool_calls: Option<Vec<Value>>,
    #[serde(default)]
    pub(crate) name: Option<String>,
    /// Present on assistant entries from models that refused the request.
    /// Treated as content — the refusal is part of the conversation experience.
    #[serde(default)]
    pub(crate) refusal: Option<String>,
}

/// Parse an ISO-8601 timestamp string to unix seconds.
///
/// Accepts any RFC 3339 / ISO 8601 string with a timezone offset or `Z`.
/// Returns 0 on parse failure so a bad timestamp never kills ingestion.
pub(crate) fn parse_ts(ts: &str) -> i64 {
    DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

/// Maximum characters to include from a `tool` result before truncating.
/// Long command output, file reads, etc. are cut here to avoid flooding chunks.
pub(crate) const TOOL_RESULT_MAX_CHARS: usize = 500;

/// A rendered line from the flat transcript, with its source provenance.
pub(crate) struct FlatLine {
    /// The rendered content line (e.g. "User: hello" or "Call[bash]: cargo test").
    pub(crate) content: String,
    /// 1-indexed JSONL line number this flat line originated from.
    pub(crate) jsonl_line: i64,
    /// Unix seconds timestamp of the originating JSONL entry.
    pub(crate) ts: i64,
}

/// Extract plain text from the `content` field of a transcript entry.
///
/// Handles both string content and content-block arrays (`[{type:"text", text:"..."}]`).
/// Image, file, and other non-text block types are silently dropped.
fn extract_content_text(entry: &TranscriptEntry) -> Option<String> {
    match &entry.content {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Array(blocks)) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(" "))
            }
        }
        _ => None,
    }
}

/// Convert one transcript entry into zero or more labeled flat lines.
///
/// Line format:
///   `User: <text>`
///   `Assistant: <text>`
///   `Call[name]: <args>`   — one per tool call, after the assistant prose line
///   `Tool[name]: <text>`   — tool result, truncated to TOOL_RESULT_MAX_CHARS
///   `System: <text>`
///
/// Structural entries (`op != "append"`) produce no lines.
pub(crate) fn entry_to_flat_lines(
    entry: &TranscriptEntry,
    jsonl_line: i64,
    ts: i64,
) -> Vec<FlatLine> {
    if entry.op != "append" {
        return vec![];
    }

    let role = entry.role.as_deref().unwrap_or("unknown");
    let mut lines: Vec<FlatLine> = Vec::new();

    if role == "tool" {
        // Tool result — rendered as "Tool[name]: <content, truncated>".
        let tool_name = entry.name.as_deref().unwrap_or("unknown");
        let label = format!("Tool[{tool_name}]");
        if let Some(raw) = extract_content_text(entry) {
            let truncated = if raw.len() > TOOL_RESULT_MAX_CHARS {
                let boundary = snap_to_char_boundary(&raw, TOOL_RESULT_MAX_CHARS);
                format!("{} ...[truncated]", &raw[..boundary])
            } else {
                raw
            };
            lines.push(FlatLine {
                content: format!("{label}: {truncated}"),
                jsonl_line,
                ts,
            });
        }
        return lines;
    }

    let label = match role {
        "user" => "User",
        "assistant" => "Assistant",
        "system" => "System",
        other => other,
    };

    // Build the main prose line for user / assistant / system.
    let mut prose_parts: Vec<String> = Vec::new();
    if let Some(text) = extract_content_text(entry) {
        prose_parts.push(text);
    }
    if let Some(refusal) = &entry.refusal {
        if !refusal.is_empty() {
            prose_parts.push(refusal.clone());
        }
    }
    let prose = prose_parts.join(" ");
    if !prose.is_empty() {
        lines.push(FlatLine {
            content: format!("{label}: {prose}"),
            jsonl_line,
            ts,
        });
    }

    // Tool calls become separate Call[name] lines after the prose line.
    if let Some(calls) = &entry.tool_calls {
        for call in calls {
            let name = call
                .get("name")
                .or_else(|| call.get("function").and_then(|f| f.get("name")))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let args = call
                .get("arguments")
                .or_else(|| call.get("function").and_then(|f| f.get("arguments")))
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            lines.push(FlatLine {
                content: format!("Call[{name}]: {args}"),
                jsonl_line,
                ts,
            });
        }
    }

    lines
}

/// Flush a completed segment into the chunk store.
///
/// Joins all flat lines into one text, splits into overlapping chunks,
/// and maps each chunk's character range back to JSONL line numbers via
/// the flat line metadata. `ts` and `start_line`/`end_line` on each chunk
/// reflect the earliest and latest JSONL entries that contributed to it.
fn flush_segment(
    db: &Db,
    segment: &[FlatLine],
    path: &str,
    strategy: &ChunkStrategy,
) -> Result<usize> {
    if segment.is_empty() {
        return Ok(0);
    }

    // Build flat text and record where each flat line starts (char offset).
    let mut flat_text = String::new();
    let mut line_char_starts: Vec<usize> = Vec::with_capacity(segment.len());
    for (i, fl) in segment.iter().enumerate() {
        line_char_starts.push(flat_text.len());
        flat_text.push_str(&fl.content);
        if i + 1 < segment.len() {
            flat_text.push('\n');
        }
    }

    let ranged = split_chunks(&flat_text, strategy);
    let n = ranged.len();

    for (chunk_text, char_start, char_end) in ranged {
        // Find the first and last flat lines covered by this chunk's char range.
        let first_fl = line_char_starts
            .partition_point(|&s| s <= char_start)
            .saturating_sub(1);
        let last_fl = (line_char_starts.partition_point(|&s| s < char_end))
            .saturating_sub(1)
            .min(segment.len() - 1);

        let ts = segment[first_fl].ts;
        let start_line = segment[first_fl].jsonl_line;
        let end_line = segment[last_fl].jsonl_line;

        let hash = chunk_hash(path, start_line, end_line, &chunk_text);
        db.insert_chunk(&hash, &chunk_text, ts, path, start_line, end_line)?;
    }

    Ok(n)
}

/// Re-read a transcript JSONL file and return the full flattened text.
///
/// Used by the dream pass to feed the LLM clean, overlap-free text from
/// the original transcript rather than stored chunks (which have overlap).
/// The `filename` is the transcript basename (e.g. `"2026-03-09.jsonl"`),
/// resolved against `config.transcript_dir`.
pub fn flatten_file(filename: &str, config: &Config) -> Result<String> {
    let path = config.transcript_dir.join(filename);
    let file =
        std::fs::File::open(&path).with_context(|| format!("opening transcript file {path:?}"))?;
    let reader = BufReader::new(file);

    let mut segments: Vec<String> = Vec::new();
    let mut segment_lines: Vec<String> = Vec::new();

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry: TranscriptEntry = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.op != "append" {
            // Segment boundary — flush current segment.
            if !segment_lines.is_empty() {
                segments.push(segment_lines.join("\n"));
                segment_lines.clear();
            }
            continue;
        }
        let ts = parse_ts(&entry.ts);
        for fl in entry_to_flat_lines(&entry, 0, ts) {
            segment_lines.push(fl.content);
        }
    }
    // Flush last segment.
    if !segment_lines.is_empty() {
        segments.push(segment_lines.join("\n"));
    }

    Ok(segments.join("\n\n"))
}

/// Run the chunker over a single transcript file, starting from the cursor
/// position recorded in the store.
///
/// Entries are flattened into labeled lines (`User:`, `Assistant:`,
/// `Call[name]:`, `Tool[name]:`) within each reset-bounded segment. The
/// segment is then split into overlapping chunks so each chunk spans a
/// coherent dialogue slice. `start_line`/`end_line` on each chunk reference
/// the original 1-indexed JSONL line numbers of contributing entries.
pub fn process_file(db: &Db, path: &Path, strategy: &ChunkStrategy) -> Result<usize> {
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid transcript filename")?
        .to_owned();

    let (cursor_offset, cursor_line) = db.get_cursor(&filename)?;

    let file =
        std::fs::File::open(path).with_context(|| format!("opening transcript file {path:?}"))?;
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(cursor_offset))?;

    let mut chunks_written = 0usize;
    let mut last_offset = cursor_offset;
    let mut jsonl_line_no = cursor_line;
    let mut line = String::new();
    let mut segment: Vec<FlatLine> = Vec::new();

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            // EOF: flush whatever is buffered.
            chunks_written += flush_segment(db, &segment, &filename, strategy)?;
            break;
        }

        last_offset += bytes_read as u64;
        jsonl_line_no += 1;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let entry: TranscriptEntry = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(_) => continue, // skip malformed lines
        };

        if entry.op != "append" {
            // reset / compact: flush current segment and start fresh.
            chunks_written += flush_segment(db, &segment, &filename, strategy)?;
            segment.clear();
            continue;
        }

        let ts = parse_ts(&entry.ts);
        for fl in entry_to_flat_lines(&entry, jsonl_line_no, ts) {
            segment.push(fl);
        }
    }

    if last_offset > cursor_offset {
        db.set_cursor(&filename, last_offset, jsonl_line_no)?;
    }

    Ok(chunks_written)
}

/// Run the chunker over all JSONL files in the transcript directory.
pub fn run_chunker(db: &Db, config: &Config, strategy: &ChunkStrategy) -> Result<usize> {
    let dir = &config.transcript_dir;
    if !dir.exists() {
        return Ok(0);
    }

    let mut total = 0usize;

    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| x == "jsonl")
                .unwrap_or(false)
        })
        .collect();

    // Process in chronological order (filenames are YYYY-MM-DD.jsonl).
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        total += process_file(db, &entry.path(), strategy)?;
    }

    Ok(total)
}

// ── SourceIngestor impl ─────────────────────────────────────────────────

/// Source adapter for conversation transcript JSONL files.
///
/// Reads all JSONL files in `config.transcript_dir` incrementally (cursor-based)
/// and writes new chunks to the index.
pub struct TranscriptSource {
    /// Chunking strategy for splitting transcript segments.
    pub chunk_strategy: ChunkStrategy,
}

impl Default for TranscriptSource {
    fn default() -> Self {
        Self {
            chunk_strategy: ChunkStrategy::default(),
        }
    }
}

impl SourceIngestor for TranscriptSource {
    fn ingest(&self, db: &Db, config: &Config) -> Result<u32> {
        run_chunker(db, config, &self.chunk_strategy).map(|n| n as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    // ── parse_ts ────────────────────────────────────────────────────────────

    #[test]
    fn parse_ts_utc_z() {
        let ts = parse_ts("2026-03-07T14:22:00Z");
        assert_eq!(ts, 1772893320, "Z suffix");
    }

    #[test]
    fn parse_ts_offset() {
        let ts_utc = parse_ts("2026-03-07T14:22:00Z");
        let ts_offset = parse_ts("2026-03-07T15:22:00+01:00");
        assert_eq!(
            ts_utc, ts_offset,
            "+01:00 offset should equal UTC counterpart"
        );
    }

    #[test]
    fn parse_ts_bad_returns_zero() {
        assert_eq!(parse_ts("not-a-timestamp"), 0);
        assert_eq!(parse_ts(""), 0);
    }

    // ── entry_to_flat_lines ──────────────────────────────────────────────────

    fn make_entry(op: &str, role: &str, content: Option<Value>) -> TranscriptEntry {
        TranscriptEntry {
            v: 1,
            ts: "2026-03-07T14:22:00Z".to_string(),
            op: op.to_string(),
            role: Some(role.to_string()),
            content,
            tool_calls: None,
            name: None,
            refusal: None,
        }
    }

    fn flat_texts(entry: &TranscriptEntry) -> Vec<String> {
        entry_to_flat_lines(entry, 1, 0)
            .into_iter()
            .map(|fl| fl.content)
            .collect()
    }

    #[test]
    fn flat_lines_structural_returns_empty() {
        assert!(flat_texts(&make_entry("reset", "system", None)).is_empty());
        assert!(flat_texts(&make_entry("compact", "system", None)).is_empty());
    }

    #[test]
    fn flat_lines_user_prefixed() {
        let entry = make_entry("append", "user", Some(Value::String("hello".into())));
        assert_eq!(flat_texts(&entry), vec!["User: hello"]);
    }

    #[test]
    fn flat_lines_assistant_prefixed() {
        let entry = make_entry("append", "assistant", Some(Value::String("hi".into())));
        assert_eq!(flat_texts(&entry), vec!["Assistant: hi"]);
    }

    #[test]
    fn flat_lines_content_blocks_joined() {
        let blocks = serde_json::json!([
            {"type": "text", "text": "hello"},
            {"type": "image_url", "url": "http://example.com/img.png"},
            {"type": "text", "text": "world"},
        ]);
        let entry = make_entry("append", "user", Some(blocks));
        assert_eq!(flat_texts(&entry), vec!["User: hello world"]);
    }

    #[test]
    fn flat_lines_tool_calls_separate_lines() {
        let mut entry = make_entry(
            "append",
            "assistant",
            Some(Value::String("Let me check".into())),
        );
        entry.tool_calls = Some(vec![
            serde_json::json!({"name": "bash", "arguments": "cargo test"}),
            serde_json::json!({"name": "read_file", "arguments": "src/main.rs"}),
        ]);
        let lines = flat_texts(&entry);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "Assistant: Let me check");
        assert_eq!(lines[1], "Call[bash]: cargo test");
        assert_eq!(lines[2], "Call[read_file]: src/main.rs");
    }

    #[test]
    fn flat_lines_tool_result_prefixed_and_short() {
        let mut entry = make_entry(
            "append",
            "tool",
            Some(Value::String("3 tests failed".into())),
        );
        entry.name = Some("bash".to_string());
        assert_eq!(flat_texts(&entry), vec!["Tool[bash]: 3 tests failed"]);
    }

    #[test]
    fn flat_lines_tool_result_truncated() {
        let long = "x".repeat(TOOL_RESULT_MAX_CHARS + 100);
        let mut entry = make_entry("append", "tool", Some(Value::String(long)));
        entry.name = Some("bash".to_string());
        let lines = flat_texts(&entry);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with("...[truncated]"));
        assert!(lines[0].len() < TOOL_RESULT_MAX_CHARS + 30);
    }

    #[test]
    fn flat_lines_refusal_treated_as_content() {
        let mut entry = make_entry("append", "assistant", None);
        entry.refusal = Some("I can't help with that.".to_string());
        assert_eq!(
            flat_texts(&entry),
            vec!["Assistant: I can't help with that."]
        );
    }

    #[test]
    fn flat_lines_no_content_no_refusal_returns_empty() {
        let entry = make_entry("append", "assistant", None);
        assert!(flat_texts(&entry).is_empty());
    }

    // ── process_file integration ─────────────────────────────────────────────

    #[test]
    fn process_file_flat_segment_one_chunk() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let jsonl_path = tmp.path().join("2026-03-07.jsonl");

        let lines = [
            r#"{"v":1,"ts":"2026-03-07T14:22:00Z","op":"append","role":"user","content":"what is the status of the auth module?"}"#,
            r#"{"v":1,"ts":"2026-03-07T14:22:05Z","op":"append","role":"assistant","content":"Let me check.","tool_calls":[{"name":"bash","arguments":"cargo test --lib auth"}]}"#,
            r#"{"v":1,"ts":"2026-03-07T14:22:08Z","op":"append","role":"tool","name":"bash","content":"3 tests failed"}"#,
            r#"{"v":1,"ts":"2026-03-07T14:22:15Z","op":"append","role":"assistant","content":"Three tests are failing."}"#,
        ];
        std::fs::write(&jsonl_path, lines.join("\n") + "\n").unwrap();

        let db = Db::open(&db_path).unwrap();
        let count = process_file(&db, &jsonl_path, &ChunkStrategy::default()).unwrap();

        assert_eq!(count, 1);
    }

    #[test]
    fn process_file_chunk_text_has_role_labels() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let jsonl_path = tmp.path().join("2026-03-07.jsonl");

        let lines = [
            r#"{"v":1,"ts":"2026-03-07T14:22:00Z","op":"append","role":"user","content":"run tests"}"#,
            r#"{"v":1,"ts":"2026-03-07T14:22:01Z","op":"append","role":"assistant","content":"ok","tool_calls":[{"name":"bash","arguments":"cargo test"}]}"#,
            r#"{"v":1,"ts":"2026-03-07T14:22:02Z","op":"append","role":"tool","name":"bash","content":"all pass"}"#,
        ];
        std::fs::write(&jsonl_path, lines.join("\n") + "\n").unwrap();

        let db = Db::open(&db_path).unwrap();
        process_file(&db, &jsonl_path, &ChunkStrategy::default()).unwrap();

        let results = crate::db::search::recall(&db, "tests", 1).unwrap();
        assert_eq!(results.len(), 1);
        let content = &results[0].content;
        assert!(content.contains("User:"));
        assert!(content.contains("Assistant:"));
        assert!(content.contains("Call[bash]:"));
        assert!(content.contains("Tool[bash]:"));
    }

    #[test]
    fn process_file_reset_starts_new_segment() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let jsonl_path = tmp.path().join("2026-03-07.jsonl");

        let lines = [
            r#"{"v":1,"ts":"2026-03-07T14:22:00Z","op":"append","role":"user","content":"hello"}"#,
            r#"{"v":1,"ts":"2026-03-07T15:00:00Z","op":"reset","reason":"context_full"}"#,
            r#"{"v":1,"ts":"2026-03-07T15:00:01Z","op":"append","role":"user","content":"world"}"#,
        ];
        std::fs::write(&jsonl_path, lines.join("\n") + "\n").unwrap();

        let db = Db::open(&db_path).unwrap();
        let count = process_file(&db, &jsonl_path, &ChunkStrategy::default()).unwrap();

        assert_eq!(count, 2);
    }

    #[test]
    fn process_file_cursor_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let jsonl_path = tmp.path().join("2026-03-07.jsonl");

        let lines = [
            r#"{"v":1,"ts":"2026-03-07T14:22:00Z","op":"append","role":"user","content":"hello"}"#,
        ];
        std::fs::write(&jsonl_path, lines.join("\n") + "\n").unwrap();

        let db = Db::open(&db_path).unwrap();

        let first = process_file(&db, &jsonl_path, &ChunkStrategy::default()).unwrap();
        let second = process_file(&db, &jsonl_path, &ChunkStrategy::default()).unwrap();

        assert_eq!(first, 1);
        assert_eq!(
            second, 0,
            "re-processing same file should produce no new chunks"
        );
    }

    #[test]
    fn process_file_provenance_lines_are_correct() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let jsonl_path = tmp.path().join("2026-03-07.jsonl");

        let lines = [
            r#"{"v":1,"ts":"2026-03-07T14:22:00Z","op":"append","role":"user","content":"q"}"#,
            r#"{"v":1,"ts":"2026-03-07T14:22:01Z","op":"append","role":"assistant","content":"a"}"#,
        ];
        std::fs::write(&jsonl_path, lines.join("\n") + "\n").unwrap();

        let db = Db::open(&db_path).unwrap();
        process_file(&db, &jsonl_path, &ChunkStrategy::default()).unwrap();

        let results = crate::db::search::recall(&db, "q a", 1).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].start_line, 1);
        assert_eq!(results[0].end_line, 2);
    }
}
