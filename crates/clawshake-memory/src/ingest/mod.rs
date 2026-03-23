use anyhow::Result;

use crate::config::Config;
use crate::db::Db;

pub mod dream;
pub mod files;
pub mod transcript;

/// A source that can ingest content into the chunk index.
///
/// Each adapter knows how to read its backing data and produce chunks for the
/// index. Multiple source types (transcript, dream, files) share this interface
/// so new sources can be added without changing the db layer.
pub trait SourceIngestor {
    /// Bring the index up to date for this source.
    ///
    /// Returns the number of new chunks written.
    fn ingest(&self, db: &Db, config: &Config) -> Result<u32>;
}

/// Strategy for splitting text into chunks.
///
/// The default `SlidingWindow` strategy works for any text. Format-aware
/// strategies (e.g. splitting on markdown headings or code AST nodes) can
/// be added here as new variants.
#[derive(Debug, Clone)]
pub enum ChunkStrategy {
    /// Fixed-size sliding window with overlap.
    /// This is the universal default — works for any text, no format knowledge needed.
    SlidingWindow {
        max_chars: usize,
        overlap_chars: usize,
    },
    // Future variants:
    // MarkdownHeadings { max_chars: usize },
    // CodeDefinitions { max_chars: usize },
}

impl Default for ChunkStrategy {
    fn default() -> Self {
        Self::SlidingWindow {
            max_chars: 1600,
            overlap_chars: 320,
        }
    }
}

/// Split `text` into chunks according to the given strategy, returning each
/// chunk with its `(text, char_start, char_end)` position within the original.
pub(crate) fn split_chunks(text: &str, strategy: &ChunkStrategy) -> Vec<(String, usize, usize)> {
    match strategy {
        ChunkStrategy::SlidingWindow {
            max_chars,
            overlap_chars,
        } => split_into_chunks_ranged(text, *max_chars, *overlap_chars),
    }
}

/// Snap a byte index to the nearest valid UTF-8 char boundary at or before `idx`.
///
/// `str` slicing panics if the index lands inside a multi-byte character.
/// This walks backward from `idx` until `is_char_boundary` returns true.
pub(crate) fn snap_to_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Split `text` into overlapping chunks, returning each chunk with its
/// `(text, char_start, char_end)` position within the original string.
///
/// This is the core sliding-window implementation used by `ChunkStrategy::SlidingWindow`.
pub(crate) fn split_into_chunks_ranged(
    text: &str,
    max_chars: usize,
    overlap_chars: usize,
) -> Vec<(String, usize, usize)> {
    if text.len() <= max_chars {
        return vec![(text.to_owned(), 0, text.len())];
    }

    let mut chunks = Vec::new();
    let mut start = 0;

    while start < text.len() {
        let end = snap_to_char_boundary(text, (start + max_chars).min(text.len()));
        chunks.push((text[start..end].to_owned(), start, end));
        if end == text.len() {
            break;
        }
        let raw_start = end.saturating_sub(overlap_chars);
        start = snap_to_char_boundary(text, raw_start);
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_short_text_returns_single_chunk() {
        let chunks = split_into_chunks_ranged("hello", 100, 10);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].0, "hello");
    }

    #[test]
    fn split_long_text_overlaps() {
        let text = "a".repeat(100);
        let chunks = split_into_chunks_ranged(&text, 40, 10);
        assert!(chunks.len() > 1);
        // Each chunk except the last should be max_chars wide.
        for (chunk, _, _) in &chunks[..chunks.len() - 1] {
            assert_eq!(chunk.len(), 40);
        }
    }

    #[test]
    fn split_multibyte_text_no_panic() {
        // '─' is 3 bytes (U+2500). A byte-based slice at an arbitrary offset
        // would land inside the multi-byte char and panic.
        let text = "─".repeat(200); // 600 bytes, 200 chars
        let chunks = split_into_chunks_ranged(&text, 100, 20);
        assert!(chunks.len() > 1);
        // Every chunk must be valid UTF-8 (would panic on construction otherwise).
        for (chunk, start, end) in &chunks {
            assert!(
                text.is_char_boundary(*start),
                "start {start} not a char boundary"
            );
            assert!(text.is_char_boundary(*end), "end {end} not a char boundary");
            assert!(!chunk.is_empty());
        }
    }

    #[test]
    fn snap_to_char_boundary_on_ascii() {
        let s = "hello";
        assert_eq!(snap_to_char_boundary(s, 3), 3);
    }

    #[test]
    fn snap_to_char_boundary_inside_multibyte() {
        let s = "a─b"; // bytes: [97, 226, 148, 128, 98]
                       // Index 2 is inside '─' (bytes 1..4). Should snap back to 1.
        assert_eq!(snap_to_char_boundary(s, 2), 1);
        assert_eq!(snap_to_char_boundary(s, 3), 1);
        // Index 4 is 'b' — already a boundary.
        assert_eq!(snap_to_char_boundary(s, 4), 4);
    }
}
