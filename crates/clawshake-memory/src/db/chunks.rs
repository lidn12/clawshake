use std::collections::HashMap;

use anyhow::Result;
use rusqlite::params;
use sha2::{Digest, Sha256};

use super::embedder::Embedder;
use super::Db;
use crate::types::SearchResult;

/// Compute the content-address hash for a chunk.
///
/// `hash = hex(sha256(path : start_line : end_line : sha256(content)))[:32]`
///
/// This is model-independent — the chunk's identity is its content + provenance.
pub fn chunk_hash(path: &str, start_line: i64, end_line: i64, content: &str) -> String {
    let content_digest = hex::encode(Sha256::digest(content.as_bytes()));
    let input = format!("{path}:{start_line}:{end_line}:{content_digest}");
    let hash = Sha256::digest(input.as_bytes());
    hex::encode(&hash[..16]) // 32 hex chars — plenty for dedup
}

impl Db {
    // ── Chunks ──────────────────────────────────────────────────────────

    /// Insert a chunk idempotently by content-address hash.
    ///
    /// Returns `Some(id)` if inserted, `None` if the hash already existed.
    pub fn insert_chunk(
        &self,
        hash: &str,
        content: &str,
        ts: i64,
        path: &str,
        start_line: i64,
        end_line: i64,
    ) -> Result<Option<i64>> {
        let changed = self.conn().execute(
            "INSERT OR IGNORE INTO chunks (hash, content, ts, path, start_line, end_line)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![hash, content, ts, path, start_line, end_line],
        )?;
        if changed > 0 {
            Ok(Some(self.conn().last_insert_rowid()))
        } else {
            Ok(None)
        }
    }

    /// Return chunks that have no embedding in vec_chunks.
    pub fn get_unembedded_chunks(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, content FROM chunks
             WHERE id NOT IN (SELECT rowid FROM vec_chunks)",
        )?;
        let results = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        results.map(|r| r.map_err(Into::into)).collect()
    }

    // ── Embeddings ──────────────────────────────────────────────────────

    /// Insert an embedding into vec_chunks.
    pub fn insert_embedding(&self, id: i64, embedding: &[f32]) -> Result<()> {
        let blob = Embedder::to_blob(embedding);
        self.conn().execute(
            "INSERT INTO vec_chunks(rowid, embedding) VALUES (?1, ?2)",
            params![id, blob],
        )?;
        Ok(())
    }

    /// KNN search: return (chunk_id, distance) ordered by distance ascending.
    pub fn knn_search(&self, embedding: &[f32], limit: usize) -> Result<Vec<(i64, f64)>> {
        let blob = Embedder::to_blob(embedding);
        let mut stmt = self.conn().prepare(
            "SELECT rowid, distance FROM vec_chunks
             WHERE embedding MATCH ? ORDER BY distance LIMIT ?",
        )?;
        let results = stmt.query_map(params![blob, limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
        })?;
        results.map(|r| r.map_err(Into::into)).collect()
    }

    /// Return true if at least one embedding has been stored.
    pub fn has_embeddings(&self) -> Result<bool> {
        let count: i64 = self
            .conn()
            .query_row("SELECT COUNT(*) FROM vec_chunks", [], |row| row.get(0))?;
        Ok(count > 0)
    }

    /// Fetch full chunk rows for an arbitrary list of ids.
    pub fn get_chunks_by_ids(&self, ids: &[i64]) -> Result<Vec<SearchResult>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT id, content, ts, path, start_line, end_line
             FROM chunks WHERE id IN ({placeholders})"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let results = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
            Ok(SearchResult {
                id: row.get(0)?,
                content: row.get(1)?,
                ts: row.get(2)?,
                path: row.get(3)?,
                start_line: row.get(4)?,
                end_line: row.get(5)?,
                score: 0.0,
            })
        })?;
        results.map(|r| r.map_err(Into::into)).collect()
    }

    /// Delete all chunks for a given path from `chunks`, `chunks_fts` (via
    /// trigger), `vec_chunks`, and `retrieval_log`.
    pub fn delete_chunks_for_path(&self, path: &str) -> Result<()> {
        // Delete embeddings first (no FK cascade on vec0 virtual table).
        self.conn().execute(
            "DELETE FROM vec_chunks WHERE rowid IN (
                 SELECT id FROM chunks WHERE path = ?1
             )",
            params![path],
        )?;
        // Delete retrieval log entries for these chunks.
        self.conn().execute(
            "DELETE FROM retrieval_log WHERE chunk_id IN (
                 SELECT id FROM chunks WHERE path = ?1
             )",
            params![path],
        )?;
        // Delete chunks — FTS5 trigger handles chunks_fts cleanup.
        self.conn()
            .execute("DELETE FROM chunks WHERE path = ?1", params![path])?;
        Ok(())
    }

    // ── ACT-R retrieval log ────────────────────────────────────────────

    /// Append retrieval events for the given chunk ids at timestamp `ts`.
    ///
    /// Called by the search layer after every successful recall so the
    /// ACT-R activation formula can account for frequency × recency.
    pub fn log_retrievals(&self, ids: &[i64], ts: i64) -> Result<()> {
        for id in ids {
            self.conn().execute(
                "INSERT INTO retrieval_log (chunk_id, ts) VALUES (?1, ?2)",
                params![id, ts],
            )?;
        }
        Ok(())
    }

    /// Compute ACT-R base-level activation for a set of chunk ids.
    ///
    /// `activation_i = Σ max(1, now − t_k)^(−d)` summed over:
    ///   - the chunk's creation timestamp (acts as the first retrieval event), and
    ///   - every row in `retrieval_log` for that chunk.
    ///
    /// With `d ≈ 0.5` this is a power-law decay: recently retrieved chunks
    /// stay active, forgotten chunks decay toward zero.
    pub fn get_activation(&self, ids: &[i64], d: f64, now: i64) -> Result<HashMap<i64, f64>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        // Build numbered placeholders for the two IN clauses.
        let p1: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
        let p2: Vec<String> = (ids.len() + 1..=ids.len() * 2)
            .map(|i| format!("?{i}"))
            .collect();
        let sql = format!(
            "SELECT id, ts FROM chunks WHERE id IN ({})\n\
             UNION ALL\n\
             SELECT chunk_id, ts FROM retrieval_log WHERE chunk_id IN ({})",
            p1.join(", "),
            p2.join(", ")
        );
        // Bind ids twice: once for each IN clause.
        let all_ids: Vec<i64> = ids.iter().chain(ids.iter()).copied().collect();
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(all_ids.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut map: HashMap<i64, f64> = HashMap::new();
        for row in rows {
            let (id, ts) = row?;
            let elapsed = (now - ts).max(1) as f64;
            *map.entry(id).or_default() += elapsed.powf(-d);
        }
        Ok(map)
    }

    // ── Cursor ──────────────────────────────────────────────────────────

    /// Return the last processed byte offset and line number for a file, or (0, 0).
    pub fn get_cursor(&self, file: &str) -> Result<(u64, i64)> {
        let result: rusqlite::Result<(i64, i64)> = self.conn().query_row(
            "SELECT offset, line_no FROM cursor WHERE file = ?1",
            params![file],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );
        match result {
            Ok((offset, line_no)) => Ok((offset as u64, line_no)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok((0, 0)),
            Err(e) => Err(e.into()),
        }
    }

    /// Persist the byte offset and line number cursor for a file.
    pub fn set_cursor(&self, file: &str, offset: u64, line_no: i64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO cursor (file, offset, line_no) VALUES (?1, ?2, ?3)
             ON CONFLICT(file) DO UPDATE SET offset = excluded.offset, line_no = excluded.line_no",
            params![file, offset as i64, line_no],
        )?;
        Ok(())
    }

    // ── File hashes (mutable-file change detection) ─────────────────────

    /// Return the stored hash for a file path, or `None` if not tracked.
    pub fn get_file_hash(&self, path: &str) -> Result<Option<String>> {
        let result: rusqlite::Result<String> = self.conn().query_row(
            "SELECT hash FROM file_hashes WHERE path = ?1",
            params![path],
            |row| row.get(0),
        );
        match result {
            Ok(hash) => Ok(Some(hash)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Store (upsert) the hash for a file path.
    pub fn set_file_hash(&self, path: &str, hash: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO file_hashes (path, hash) VALUES (?1, ?2)
             ON CONFLICT(path) DO UPDATE SET hash = excluded.hash",
            params![path, hash],
        )?;
        Ok(())
    }

    /// Remove the stored hash for a file path.
    pub fn delete_file_hash(&self, path: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM file_hashes WHERE path = ?1", params![path])?;
        Ok(())
    }

    /// Return all tracked file paths (for detecting deletions).
    pub fn get_all_file_hashes(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT path, hash FROM file_hashes")?;
        let results = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        results.map(|r| r.map_err(Into::into)).collect()
    }
}
