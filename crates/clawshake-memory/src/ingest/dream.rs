use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rusqlite::params;

use crate::config::Config;
use crate::db::chunks::chunk_hash;
use crate::db::embedder::Embedder;
use crate::db::Db;
use crate::ingest::transcript::flatten_file;
use crate::ingest::{snap_to_char_boundary, split_chunks, ChunkStrategy, SourceIngestor};

/// Path used for all dream-pass output chunks.
const DREAM_PATH: &str = "dream/<dream>";

/// Approximate chars-per-token ratio for input budget estimation.
const CHARS_PER_TOKEN: usize = 4;

/// Parameters controlling a single dream-pass run.
#[derive(Debug, Clone)]
pub struct DreamPassConfig {
    /// Maximum number of sessions to consolidate per run.
    pub max_sessions: usize,

    /// Maximum input tokens (approximate) per LLM call. Sessions whose
    /// flat text exceeds this budget are split into sequential batches
    /// at line boundaries with a small character overlap for continuity.
    pub max_input_tokens: usize,

    /// Characters of trailing context from the previous batch to prepend
    /// to the next batch for continuity.
    pub batch_overlap_chars: usize,

    /// Chunking strategy used to split LLM output into storable chunks.
    pub chunk_strategy: ChunkStrategy,
}

impl Default for DreamPassConfig {
    fn default() -> Self {
        Self {
            max_sessions: 5,
            max_input_tokens: 30_000,
            batch_overlap_chars: 500,
            chunk_strategy: ChunkStrategy::default(),
        }
    }
}

/// A dream-pass source adapter. Holds configuration and an LLM callback.
///
/// Call `ingest` (via the `SourceIngestor` trait) to consolidate pending
/// sessions into dream chunks using an existing store handle, or use the
/// `dream_pass` free function for a self-contained call that opens its
/// own store.
pub struct DreamSource<F: Fn(String) -> Result<String>> {
    pub pass_config: DreamPassConfig,
    pub llm: F,
}

impl<F: Fn(String) -> Result<String>> SourceIngestor for DreamSource<F> {
    fn ingest(&self, db: &Db, config: &Config) -> Result<u32> {
        dream_pass_with_store(db, config, &self.pass_config, &self.llm)
    }
}

/// Run a temporal dream pass using an existing `Db` handle.
///
/// This is the core implementation used by both `DreamSource::ingest`
/// and the `dream_pass` free function.
fn dream_pass_with_store(
    db: &Db,
    config: &Config,
    pass_config: &DreamPassConfig,
    llm: impl Fn(String) -> Result<String>,
) -> Result<u32> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let mut embedder = Embedder::new()?;

    let sessions = get_sessions_pending_dream(db, pass_config.max_sessions)?;
    if sessions.is_empty() {
        return Ok(0);
    }

    let max_input_chars = pass_config.max_input_tokens * CHARS_PER_TOKEN;
    let mut total_written = 0u32;

    for session_path in &sessions {
        // Re-read the original JSONL — overlap-free flat text.
        let flat_text = flatten_file(session_path, config)?;
        if flat_text.trim().is_empty() {
            continue;
        }

        // Split into batches that fit the LLM context window.
        let batches = batch_text(&flat_text, max_input_chars, pass_config.batch_overlap_chars);

        for batch in &batches {
            let prompt = build_prompt(batch, session_path);
            let reply = llm(prompt)?;
            if reply.trim().is_empty() {
                continue;
            }

            // Chunk the LLM output using the dream pass's own strategy.
            let sub_chunks = split_chunks(reply.trim(), &pass_config.chunk_strategy);

            for (chunk_text, _, _) in &sub_chunks {
                let hash = chunk_hash(DREAM_PATH, 0, 0, chunk_text);
                let new_id = match db.insert_chunk(&hash, chunk_text, now, DREAM_PATH, 0, 0)? {
                    Some(id) => id,
                    None => continue, // duplicate content hash — skip
                };
                let embeddings = embedder.embed(vec![chunk_text.clone()])?;
                db.insert_embedding(new_id, &embeddings[0])?;
                total_written += 1;
            }
        }

        // Delete all source chunks for this session — the dream chunks
        // are their replacement. Original JSONL remains on disk.
        db.delete_chunks_for_path(session_path)?;
    }

    Ok(total_written)
}

/// Run a temporal dream pass: consolidate raw conversation sessions into
/// denoised factual chunks.
///
/// For each session (= transcript file), the pass:
///   1. Re-reads the original JSONL file and flattens it into clean text
///      (no chunk overlaps — the LLM sees the conversation as it happened).
///   2. Batches the flat text to respect the LLM context window budget.
///   3. Sends each batch to the LLM callback for denoising.
///   4. Splits the LLM output using the dream pass's own `ChunkStrategy`.
///   5. Embeds each dream chunk.
///   6. Deletes all source chunks for the session (from `chunks`,
///      `chunks_fts`, and `vec_chunks`). The original JSONL file on disk
///      is preserved if re-ingest is ever needed.
///
/// `llm` is a synchronous callback: given a prompt string, return the LLM's
/// reply. The caller is responsible for the API call, model selection, and
/// error handling.
///
/// Returns the number of dream chunks written.
pub fn dream_pass(
    config: &Config,
    pass_config: &DreamPassConfig,
    llm: impl Fn(String) -> Result<String>,
) -> Result<u32> {
    let db = Db::open(&config.db_path)?;
    dream_pass_with_store(&db, config, pass_config, llm)
}

/// Return session paths that have not yet been processed by the dream pass.
///
/// Selects distinct paths that do NOT start with `"dream/"` (i.e. raw
/// transcript, remember, or other source chunks), ordered by
/// most-recently-retrieved first, then by newest creation timestamp.
fn get_sessions_pending_dream(db: &Db, n: usize) -> Result<Vec<String>> {
    let mut stmt = db.conn().prepare(
        "SELECT c.path
         FROM chunks c
         WHERE c.path NOT LIKE 'dream/%'
         GROUP BY c.path
         ORDER BY MAX((
             SELECT COUNT(*) FROM retrieval_log r WHERE r.chunk_id = c.id
         )) DESC, MAX(c.ts) DESC
         LIMIT ?1",
    )?;
    let results = stmt
        .query_map(params![n as i64], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(results)
}

/// Split flat text into batches that each fit within `max_chars`.
///
/// Splits at line boundaries (`\n`). When the text exceeds the budget,
/// `overlap_chars` of trailing context from the previous batch is
/// prepended to the next batch for LLM continuity.
fn batch_text(text: &str, max_chars: usize, overlap_chars: usize) -> Vec<String> {
    if text.len() <= max_chars {
        return vec![text.to_owned()];
    }

    let lines: Vec<&str> = text.lines().collect();
    let mut batches: Vec<String> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let mut batch_lines: Vec<&str> = Vec::new();
        let mut budget = max_chars;

        // Prepend overlap from the end of the previous batch.
        if let Some(prev) = batches.last() {
            let start = snap_to_char_boundary(prev, prev.len().saturating_sub(overlap_chars));
            let tail = &prev[start..];
            if tail.len() < budget {
                budget -= tail.len();
                // We prepend the overlap as a single block — the LLM gets
                // continuity context without us tracking individual lines.
                batch_lines.push(tail);
            }
        }

        // Fill the batch from the current position.
        while i < lines.len() {
            let line_len = lines[i].len() + 1; // +1 for newline
            if line_len > budget && !batch_lines.is_empty() {
                break;
            }
            budget = budget.saturating_sub(line_len);
            batch_lines.push(lines[i]);
            i += 1;
        }

        if !batch_lines.is_empty() {
            batches.push(batch_lines.join("\n"));
        }
    }

    batches
}

/// Build a denoising prompt from a batch of flat text.
fn build_prompt(batch: &str, session_path: &str) -> String {
    format!(
        "You are rewriting a conversation transcript as a clean factual record.\n\
         Rules:\n\
         - Keep every factual statement, decision, code change, and technical detail.\n\
         - Remove conversational filler, greetings, repetition, and meta-commentary.\n\
         - Resolve contradictions: keep the final position, note the change.\n\
         - Preserve temporal context (when something changed or was decided).\n\
         - Maintain the chronological order of events.\n\
         - Output only the denoised factual text — no preamble, no commentary.\n\n\
         --- SESSION: {session_path} ---\n\n\
         {batch}\n\n\
         --- END SESSION ---\n\n\
         Denoised factual record:"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    use crate::ingest::transcript::process_file;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// fastembed uses a file lock that fails when multiple test threads
    /// try to load the model simultaneously. Serialize embedder tests.
    static EMBEDDER_LOCK: Mutex<()> = Mutex::new(());

    fn test_config(db_path: PathBuf, transcript_dir: PathBuf) -> Config {
        Config {
            db_path,
            transcript_dir,
            skill_dirs: vec![],
        }
    }

    // ── batch_text ───────────────────────────────────────────────────────

    #[test]
    fn batch_text_short_text_single_batch() {
        let text = "line one\nline two\nline three";
        let batches = batch_text(text, 1000, 50);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], text);
    }

    #[test]
    fn batch_text_splits_at_line_boundaries() {
        // Create text that exceeds max_chars so it must split.
        let line = "a".repeat(40);
        // 5 lines × 41 chars each (40 + newline) = 205 chars total
        let text = (0..5).map(|_| line.as_str()).collect::<Vec<_>>().join("\n");
        // Budget of 100 chars — should fit ~2 lines per batch.
        let batches = batch_text(&text, 100, 0);
        assert!(batches.len() >= 2, "should split into multiple batches");
        // Every line should appear in at least one batch.
        for batch in &batches {
            assert!(!batch.is_empty());
        }
        // No batch (except possibly the last) should exceed max_chars.
        for batch in &batches[..batches.len() - 1] {
            assert!(batch.len() <= 100, "batch too large: {} chars", batch.len());
        }
    }

    #[test]
    fn batch_text_overlap_prepends_tail() {
        let line = "x".repeat(40);
        let text = (0..6).map(|_| line.as_str()).collect::<Vec<_>>().join("\n");
        // Budget of 100, overlap of 30 chars.
        let batches = batch_text(&text, 100, 30);
        assert!(batches.len() >= 2);
        // The second batch should start with overlap from the first batch's tail.
        if batches.len() >= 2 {
            let tail = &batches[0][batches[0].len().saturating_sub(30)..];
            assert!(
                batches[1].starts_with(tail),
                "second batch should start with tail of first batch"
            );
        }
    }

    // ── delete_chunks_for_path ────────────────────────────────────────────────

    #[test]
    fn delete_session_chunks_cleans_all_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let db = Db::open(&db_path).unwrap();

        // Insert a raw chunk.
        let id = db
            .insert_chunk("hash1", "some content", 1000, "session.jsonl", 1, 4)
            .unwrap()
            .unwrap();

        // Insert a fake embedding (384 zeroes).
        let fake_emb = vec![0.0f32; 384];
        db.insert_embedding(id, &fake_emb).unwrap();

        // Log a retrieval event.
        db.log_retrievals(&[id], 2000).unwrap();

        // Verify everything exists.
        assert!(db.has_embeddings().unwrap());
        let act = db.get_activation(&[id], 0.5, 3000).unwrap();
        assert!(act.contains_key(&id), "retrieval log should exist");

        // Delete the session.
        db.delete_chunks_for_path("session.jsonl").unwrap();

        // Chunks gone.
        let chunks = db.get_chunks_by_ids(&[id]).unwrap();
        assert!(chunks.is_empty(), "chunk should be deleted");

        // Embeddings gone.
        assert!(!db.has_embeddings().unwrap(), "embedding should be deleted");

        // Retrieval log gone — activation should be empty.
        let act = db.get_activation(&[id], 0.5, 3000).unwrap();
        assert!(act.is_empty(), "retrieval log entries should be deleted");

        // FTS should return nothing.
        let fts_results = crate::db::search::recall(&db, "content", 10).unwrap();
        assert!(fts_results.is_empty(), "FTS5 index should be cleaned");
    }

    // ── dream_pass integration ──────────────────────────────────────────

    #[test]
    fn dream_pass_replaces_source_with_dream_chunks() {
        let _guard = EMBEDDER_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let jsonl_path = tmp.path().join("2026-03-07.jsonl");

        // Write a small transcript with recognizable content.
        let lines = [
            r#"{"v":1,"ts":"2026-03-07T14:00:00Z","op":"append","role":"user","content":"We decided to use SQLite for the memory store."}"#,
            r#"{"v":1,"ts":"2026-03-07T14:00:05Z","op":"append","role":"assistant","content":"Good choice. SQLite is simple and reliable."}"#,
            r#"{"v":1,"ts":"2026-03-07T14:00:10Z","op":"append","role":"user","content":"Also, use content-addressed hashing for dedup."}"#,
            r#"{"v":1,"ts":"2026-03-07T14:00:15Z","op":"append","role":"assistant","content":"Agreed. SHA-256 hash of path, lines, and content."}"#,
        ];
        std::fs::write(&jsonl_path, lines.join("\n") + "\n").unwrap();

        let config = test_config(db_path.clone(), tmp.path().to_path_buf());
        let db = Db::open(&db_path).unwrap();

        // Ingest the transcript — creates raw chunks.
        let ingest_count = process_file(&db, &jsonl_path, &ChunkStrategy::default()).unwrap();
        assert!(ingest_count > 0, "should ingest at least one chunk");

        // Verify raw chunks exist with the transcript path.
        let sessions = get_sessions_pending_dream(&db, 10).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0], "2026-03-07.jsonl");

        // Run dream pass with a mock LLM that returns a cleaned summary.
        let pass_config = DreamPassConfig {
            max_sessions: 5,
            max_input_tokens: 30_000,
            batch_overlap_chars: 500,
            chunk_strategy: ChunkStrategy::default(),
        };
        let mock_llm = |_prompt: String| -> Result<String> {
            Ok("Decision: use SQLite for the memory store. \
                Use content-addressed SHA-256 hashing for chunk dedup."
                .to_string())
        };

        let dream_count = dream_pass(&config, &pass_config, mock_llm).unwrap();
        assert!(dream_count > 0, "should produce at least one dream chunk");

        // Source chunks should be deleted — no undreamed sessions left.
        let sessions_after = get_sessions_pending_dream(&db, 10).unwrap();
        assert!(
            sessions_after.is_empty(),
            "source session should be gone after dream pass"
        );

        // Dream chunks should exist.
        let dream_chunks: Vec<(i64, String)> = {
            let mut stmt = db
                .conn()
                .prepare("SELECT id, content FROM chunks WHERE path LIKE 'dream/%'")
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };
        assert!(!dream_chunks.is_empty(), "dream chunks should be created");

        // Dream chunk content should reflect the mock LLM output.
        let all_content: String = dream_chunks
            .iter()
            .map(|(_, c)| c.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            all_content.contains("SQLite"),
            "dream content should contain 'SQLite'"
        );

        // Dream chunks should have embeddings.
        assert!(
            db.has_embeddings().unwrap(),
            "dream chunks should be embedded"
        );
    }

    #[test]
    fn dream_pass_empty_llm_reply_produces_no_chunks() {
        let _guard = EMBEDDER_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let jsonl_path = tmp.path().join("2026-03-08.jsonl");

        let lines = [
            r#"{"v":1,"ts":"2026-03-08T10:00:00Z","op":"append","role":"user","content":"hello"}"#,
            r#"{"v":1,"ts":"2026-03-08T10:00:01Z","op":"append","role":"assistant","content":"hi"}"#,
        ];
        std::fs::write(&jsonl_path, lines.join("\n") + "\n").unwrap();

        let config = test_config(db_path.clone(), tmp.path().to_path_buf());
        let db = Db::open(&db_path).unwrap();
        process_file(&db, &jsonl_path, &ChunkStrategy::default()).unwrap();

        // LLM returns empty — nothing worth keeping.
        let mock_llm = |_prompt: String| -> Result<String> { Ok("".to_string()) };
        let pass_config = DreamPassConfig::default();

        let dream_count = dream_pass(&config, &pass_config, mock_llm).unwrap();
        assert_eq!(
            dream_count, 0,
            "empty reply should produce zero dream chunks"
        );

        // Source chunks should still be deleted (the LLM processed the session,
        // it just had nothing to say — the session is "done").
        let sessions = get_sessions_pending_dream(&db, 10).unwrap();
        assert!(
            sessions.is_empty(),
            "source chunks should still be deleted even with empty LLM reply"
        );
    }

    #[test]
    fn dream_pass_no_sessions_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let config = test_config(db_path.clone(), tmp.path().to_path_buf());

        // No transcript files, no chunks — dream pass should be a no-op.
        let pass_config = DreamPassConfig::default();
        let mock_llm = |_prompt: String| -> Result<String> {
            panic!("LLM should not be called when there are no sessions");
        };
        let count = dream_pass(&config, &pass_config, mock_llm).unwrap();
        assert_eq!(count, 0);
    }
}
