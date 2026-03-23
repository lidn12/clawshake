/// Database layer: schema, chunk storage, search, embeddings.
///
/// This module is the source-agnostic index core. It knows nothing about
/// where chunks come from (transcript, dream, notes). It stores
/// `(content, embedding, path, lines, ts)` tuples and answers recall queries.
///
/// Source adapters live in `crate::ingest` and call this module's public API.
pub mod chunks;
pub mod embedder;
pub mod search;

use std::sync::Once;

use anyhow::Result;
use rusqlite::{params, Connection};

pub use chunks::chunk_hash;
pub use embedder::Embedder;

static SQLITE_VEC_INIT: Once = Once::new();

/// Register the sqlite-vec extension for every connection opened after this call.
fn register_sqlite_vec() {
    SQLITE_VEC_INIT.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

/// SQLite-backed database: chunks + FTS5 + vec0 KNN + meta.
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (or create) the database at the given path and initialise the schema.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        register_sqlite_vec();
        let conn = Connection::open(path)?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch("
            PRAGMA journal_mode = WAL;

            CREATE TABLE IF NOT EXISTS chunks (
                id           INTEGER PRIMARY KEY,
                hash         TEXT    UNIQUE NOT NULL,
                content      TEXT    NOT NULL,
                ts           INTEGER NOT NULL,
                path         TEXT    NOT NULL,
                start_line   INTEGER NOT NULL,
                end_line     INTEGER NOT NULL
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
                content,
                content='chunks',
                content_rowid='id'
            );

            -- Keep the FTS index in sync via triggers.
            CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
                INSERT INTO chunks_fts(rowid, content) VALUES (new.id, new.content);
            END;

            CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
                INSERT INTO chunks_fts(chunks_fts, rowid, content) VALUES ('delete', old.id, old.content);
            END;

            CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE ON chunks BEGIN
                INSERT INTO chunks_fts(chunks_fts, rowid, content) VALUES ('delete', old.id, old.content);
                INSERT INTO chunks_fts(rowid, content) VALUES (new.id, new.content);
            END;

            CREATE VIRTUAL TABLE IF NOT EXISTS vec_chunks USING vec0(
                embedding float[384]
            );

            CREATE TABLE IF NOT EXISTS cursor (
                file    TEXT    NOT NULL,
                offset  INTEGER NOT NULL,
                line_no INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (file)
            );

            CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            -- ACT-R retrieval history: one row per chunk returned by recall.
            CREATE TABLE IF NOT EXISTS retrieval_log (
                chunk_id INTEGER NOT NULL REFERENCES chunks(id),
                ts       INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS retrieval_log_chunk_id
                ON retrieval_log (chunk_id);

            -- File hash tracking for mutable-file sources (FilesSource).
            -- Stores the SHA-256 digest of each indexed file so the adapter
            -- can detect changes (re-chunk) or deletions (remove chunks).
            CREATE TABLE IF NOT EXISTS file_hashes (
                path TEXT PRIMARY KEY,
                hash TEXT NOT NULL
            );
        ")?;
        Ok(())
    }

    // ── Meta ────────────────────────────────────────────────────────────

    /// Get a value from the meta table, or `None` if not set.
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let result = self.conn.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        );
        match result {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Set a value in the meta table (upsert).
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ── Model enforcement ───────────────────────────────────────────────

    /// Ensure the stored embedding model matches the given model name.
    ///
    /// - First call: stores the model name in meta.
    /// - Same model: no-op.
    /// - Different model: wipes vec_chunks and returns Ok so a full
    ///   re-embed proceeds.
    pub fn ensure_model_match(&self, model: &str) -> Result<()> {
        match self.get_meta("embedding_model")? {
            None => {
                self.set_meta("embedding_model", model)?;
            }
            Some(stored) if stored == model => {}
            Some(stored) => {
                eprintln!(
                    "embedding model changed: {stored} → {model}; wiping vector index for re-embed"
                );
                self.conn.execute("DELETE FROM vec_chunks", [])?;
                self.set_meta("embedding_model", model)?;
            }
        }
        Ok(())
    }

    /// Expose a reference to the underlying connection for the search layer
    /// and for tests in dependent crates.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}
