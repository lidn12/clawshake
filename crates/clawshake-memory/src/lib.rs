pub mod config;
pub mod db;
pub mod ingest;
pub mod procedural;
pub mod skills;
pub mod types;
pub mod watch;
pub mod write;

pub use config::Config;
pub use db::embedder::{Embedder, MODEL_NAME};
pub use db::Db;
pub use ingest::dream::{dream_pass, DreamPassConfig};
pub use ingest::files::{FilesSource, FilesSourceConfig};
pub use ingest::ChunkStrategy;
pub use procedural::Procedural;
pub use skills::SkillMeta;
pub use types::SearchResult;
pub use watch::WatchConfig;
pub use write::{append_entry, WriteEntry};

/// Synchronous full recall — hybrid (BM25 + vector) when embeddings
/// exist, BM25-only otherwise.
///
/// `Embedder` is constructed inside this function so callers don't need to
/// manage its lifetime across thread boundaries.
pub fn recall(config: &Config, query: &str, limit: usize) -> anyhow::Result<Vec<SearchResult>> {
    let db = Db::open(&config.db_path)?;
    if db.has_embeddings()? {
        let mut embedder = Embedder::new()?;
        db::search::recall_hybrid(&db, &mut embedder, query, limit)
    } else {
        db::search::recall(&db, query, limit)
    }
}

/// Embed all chunks that don't yet have embeddings.
///
/// Returns the number of chunks embedded.  If there are no pending chunks,
/// returns `Ok(0)` without initialising the embedder.
pub fn embed_pending(db: &Db, embedder: &mut Embedder) -> anyhow::Result<usize> {
    let chunks = db.get_unembedded_chunks()?;
    if chunks.is_empty() {
        return Ok(0);
    }

    let texts: Vec<String> = chunks.iter().map(|(_, c)| c.clone()).collect();
    let embeddings = embedder.embed(texts)?;
    for ((id, _), emb) in chunks.iter().zip(embeddings.iter()) {
        db.insert_embedding(*id, emb)?;
    }
    Ok(chunks.len())
}
