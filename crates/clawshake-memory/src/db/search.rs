use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

use super::embedder::Embedder;
use super::Db;
use crate::types::SearchResult;

/// Default number of results returned by `recall`.
pub const DEFAULT_LIMIT: usize = 10;

/// RRF rank fusion constant — standard default.
const RRF_K: f64 = 60.0;

/// Current UNIX time in seconds.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Sanitize a free-text query for safe use in FTS5 MATCH expressions.
///
/// Strips characters that FTS5 treats as query syntax operators:
///   `-`  unary NOT (query grammar intercepts it before tokenization — cannot
///        be fixed via tokenizer config alone);
///   `"` phrase delimiter, `*` prefix search, `^` initial-token boost, `()` grouping.
/// Also strips uppercase keywords (`OR`, `AND`, `NOT`, `NEAR`) that FTS5
/// interprets as boolean operators — e.g. "NOT working" would exclude all
/// chunks containing "working".
///
/// Other punctuation (`.`, `_`, `'`) is left intact — no FTS5 special meaning.
fn sanitize_fts(query: &str) -> String {
    let s: String = query
        .chars()
        .map(|c| {
            if matches!(c, '-' | '"' | '*' | '^' | '(' | ')') {
                ' '
            } else {
                c
            }
        })
        .collect();
    // Split into words and strip FTS5 boolean keywords.
    s.split_whitespace()
        .filter(|w| !matches!(w.to_uppercase().as_str(), "OR" | "AND" | "NOT" | "NEAR"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// BM25-only recall (FTS5). Used when no embeddings have been generated yet.
///
/// SQLite FTS5 returns BM25 scores as negative values — lower is better.
/// Scores are converted to rank-based values before ACT-R reranking so that
/// the activation multiplier works uniformly across both recall paths.
pub fn recall(db: &Db, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
    let q = sanitize_fts(query);
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT c.id, c.content, c.ts, c.path, c.start_line, c.end_line, bm25(chunks_fts) AS score
         FROM chunks_fts
         JOIN chunks c ON chunks_fts.rowid = c.id
         WHERE chunks_fts MATCH ?1
         ORDER BY score
         LIMIT ?2",
    )?;
    let mut results: Vec<SearchResult> = stmt
        .query_map(rusqlite::params![q, limit as i64], |row| {
            Ok(SearchResult {
                id: row.get(0)?,
                content: row.get(1)?,
                ts: row.get(2)?,
                path: row.get(3)?,
                start_line: row.get(4)?,
                end_line: row.get(5)?,
                score: row.get(6)?,
            })
        })?
        .map(|r| r.map_err(Into::into))
        .collect::<Result<_>>()?;

    // Convert BM25 scores (negative, lower = better) to rank-based scores.
    for (rank, r) in results.iter_mut().enumerate() {
        r.score = 1.0 / (RRF_K + rank as f64 + 1.0);
    }
    log_recall(&results, db, now_secs())?;
    Ok(results)
}

/// Hybrid recall: BM25 + vector KNN merged with Reciprocal Rank Fusion.
///
/// Falls back gracefully to BM25-only if the embedder returns no KNN hits
/// (e.g. the vec_chunks table is empty).
pub fn recall_hybrid(
    db: &Db,
    embedder: &mut Embedder,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let candidate_n = (limit * 3).max(30);
    let q = sanitize_fts(query);

    // BM25 candidates
    let bm25_ids = bm25_ranked_ids(db, &q, candidate_n)?;

    // Vector KNN candidates.
    let embs = embedder.embed(vec![query.to_string()])?;
    let knn_ids = db.knn_search(&embs[0], candidate_n)?;

    // RRF scoring — each list contributes 1/(k + 1-based rank)
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for (rank, (id, _)) in bm25_ids.iter().enumerate() {
        *scores.entry(*id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
    }
    for (rank, (id, _)) in knn_ids.iter().enumerate() {
        *scores.entry(*id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
    }

    // Sort descending by RRF score, keep top `limit` ids
    let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(limit);

    // Fetch full chunk data for the winning ids
    let ids: Vec<i64> = ranked.iter().map(|(id, _)| *id).collect();
    let mut chunks = db.get_chunks_by_ids(&ids)?;

    // Apply RRF score and sort
    for chunk in &mut chunks {
        if let Some(pos) = ranked.iter().position(|(id, _)| *id == chunk.id) {
            chunk.score = ranked[pos].1;
        }
    }
    chunks.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    log_recall(&chunks, db, now_secs())?;
    Ok(chunks)
}

/// Log retrieved chunk ids to `retrieval_log` for future ACT-R analysis.
///
/// Scoring is intentionally not applied — retrieval ranking uses BM25 / RRF
/// only for now.  The logged data enables offline ACT-R experiments later.
fn log_recall(results: &[SearchResult], db: &Db, now: i64) -> Result<()> {
    if results.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = results.iter().map(|r| r.id).collect();
    db.log_retrievals(&ids, now)?;
    Ok(())
}

/// Return (id, bm25_score) ordered by BM25 rank (best first).
fn bm25_ranked_ids(db: &Db, query: &str, limit: usize) -> Result<Vec<(i64, f64)>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT c.id, bm25(chunks_fts) AS score
         FROM chunks_fts
         JOIN chunks c ON chunks_fts.rowid = c.id
         WHERE chunks_fts MATCH ?1
         ORDER BY score
         LIMIT ?2",
    )?;
    let results = stmt.query_map(rusqlite::params![query, limit as i64], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
    })?;
    results.map(|r| r.map_err(Into::into)).collect()
}
