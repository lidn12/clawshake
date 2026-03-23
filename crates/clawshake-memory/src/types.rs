use serde::{Deserialize, Serialize};

/// One result returned by `recall`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Row id in the chunks table — used internally for RRF merging.
    pub id: i64,
    pub content: String,
    /// Relevance score.  BM25-only: negative FTS5 value (lower = better).
    /// Hybrid: RRF score (higher = better).
    pub score: f64,
    /// Unix seconds of the earliest source entry that contributed to this chunk.
    pub ts: i64,
    /// Source path.
    pub path: String,
    /// 1-indexed line number of the first entry contributing to this chunk.
    pub start_line: i64,
    /// 1-indexed line number of the last entry contributing to this chunk.
    pub end_line: i64,
}
