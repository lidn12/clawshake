//! FilesSource — generic text-file ingest adapter.
//!
//! Indexes any text files from configured directories into the chunk store.
//! Handles agent-written notes, project documentation, knowledge bases, or
//! any other text on disk. Format-agnostic: reads bytes, treats them as text,
//! chunks them, indexes them.
//!
//! Change detection is hash-based (SHA-256 of file contents). When a file
//! changes, its old chunks are fully deleted and re-ingested. When a file
//! is removed from disk, its chunks and hash record are cleaned up.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::db::chunks::chunk_hash;
use crate::db::Db;
use crate::ingest::{split_chunks, ChunkStrategy, SourceIngestor};

/// Configuration for a directory-based file source.
#[derive(Debug, Clone)]
pub struct FilesSourceConfig {
    /// Directories to index. Each is walked recursively.
    pub paths: Vec<PathBuf>,

    /// File extensions to include (without the dot). Empty = all files.
    /// Example: `vec!["md", "txt", "rs"]`
    pub extensions: Vec<String>,

    /// Glob-style path suffixes to exclude. Simple suffix matching —
    /// a file is excluded if its relative path ends with any of these.
    /// Example: `vec!["target/", ".lock"]`
    pub exclude: Vec<String>,

    /// Path prefix for chunk provenance in the index.
    /// Chunks are stored with `path = "{prefix}{relative_path}"`.
    /// Default: `"files/"`.
    pub path_prefix: String,

    /// Chunking strategy. Default: sliding window (1600 chars, 320 overlap).
    pub chunk_strategy: ChunkStrategy,
}

impl Default for FilesSourceConfig {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            extensions: Vec::new(),
            exclude: Vec::new(),
            path_prefix: "files/".to_owned(),
            chunk_strategy: ChunkStrategy::default(),
        }
    }
}

/// Generic text-file source adapter.
///
/// Walks configured directories, detects changed/new/deleted files via
/// content hashing, and (re-)indexes them into the chunk store.
pub struct FilesSource {
    pub config: FilesSourceConfig,
}

impl SourceIngestor for FilesSource {
    fn ingest(&self, db: &Db, config: &Config) -> Result<u32> {
        ingest_files(db, config, &self.config)
    }
}

/// Core ingest logic: walk → hash → diff → chunk → index.
fn ingest_files(db: &Db, _config: &Config, src: &FilesSourceConfig) -> Result<u32> {
    if src.paths.is_empty() {
        return Ok(0);
    }

    // 1. Collect all eligible files from disk.
    let disk_files = collect_files(src)?;

    // 2. Load all previously tracked file hashes with our prefix.
    let stored: Vec<(String, String)> = db.get_all_file_hashes()?;
    let stored_for_prefix: Vec<(String, String)> = stored
        .into_iter()
        .filter(|(p, _)| p.starts_with(&src.path_prefix))
        .collect();

    let now_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let mut new_chunks: u32 = 0;

    // 3. For each file on disk: hash, compare, (re-)ingest if changed.
    let mut seen_paths: HashSet<String> = HashSet::new();

    for (disk_path, rel_path) in &disk_files {
        let index_path = format!("{}{}", src.path_prefix, rel_path);
        seen_paths.insert(index_path.clone());

        let content = match std::fs::read_to_string(disk_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warning: skipping {}: {e}", disk_path.display());
                continue;
            }
        };

        if content.trim().is_empty() {
            continue;
        }

        let file_hash = hex::encode(Sha256::digest(content.as_bytes()));

        // Check stored hash — unchanged files are skipped.
        if let Some(stored_hash) = db.get_file_hash(&index_path)? {
            if stored_hash == file_hash {
                continue;
            }
            // Changed: delete old chunks, re-ingest below.
            db.delete_chunks_for_path(&index_path)?;
        }

        // Chunk the file.
        let chunks = split_chunks(&content, &src.chunk_strategy);

        for (chunk_text, char_start, char_end) in &chunks {
            // Map character ranges to approximate line numbers for provenance.
            let start_line = content[..*char_start].lines().count().max(1) as i64;
            let end_line = content[..*char_end].lines().count().max(1) as i64;

            let hash = chunk_hash(&index_path, start_line, end_line, chunk_text);
            if db
                .insert_chunk(&hash, chunk_text, now_ts, &index_path, start_line, end_line)?
                .is_some()
            {
                new_chunks += 1;
            }
        }

        // Record the file hash for future change detection.
        db.set_file_hash(&index_path, &file_hash)?;
    }

    // 4. Detect deletions: files previously tracked but no longer on disk.
    for (stored_path, _) in &stored_for_prefix {
        if !seen_paths.contains(stored_path.as_str()) {
            db.delete_chunks_for_path(stored_path)?;
            db.delete_file_hash(stored_path)?;
        }
    }

    Ok(new_chunks)
}

/// Walk all configured directories and collect eligible (path, relative_path) pairs.
fn collect_files(src: &FilesSourceConfig) -> Result<Vec<(PathBuf, String)>> {
    let mut files = Vec::new();

    for root in &src.paths {
        if !root.exists() {
            eprintln!("warning: index path does not exist: {}", root.display());
            continue;
        }

        let root_canonical = root
            .canonicalize()
            .with_context(|| format!("canonicalize {}", root.display()))?;

        walk_dir(&root_canonical, &root_canonical, src, &mut files)?;
    }

    Ok(files)
}

/// Recursively walk a directory, applying extension and exclusion filters.
fn walk_dir(
    dir: &Path,
    root: &Path,
    src: &FilesSourceConfig,
    out: &mut Vec<(PathBuf, String)>,
) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        // Compute relative path for filtering and provenance.
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        // Check exclusions.
        if src.exclude.iter().any(|ex| rel.contains(ex)) {
            continue;
        }

        if path.is_dir() {
            walk_dir(&path, root, src, out)?;
        } else if path.is_file() {
            // Extension filter.
            if !src.extensions.is_empty() {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !src.extensions.iter().any(|e| e == ext) {
                    continue;
                }
            }

            out.push((path, rel));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use tempfile::TempDir;

    /// Helper: create a minimal config pointing at a temp db.
    fn test_config(dir: &Path) -> Config {
        Config {
            db_path: dir.join("test.db"),
            transcript_dir: dir.join("log"),
            skill_dirs: vec![],
        }
    }

    #[test]
    fn ingest_new_files() {
        let tmp = TempDir::new().unwrap();
        let notes_dir = tmp.path().join("notes");
        std::fs::create_dir_all(&notes_dir).unwrap();
        std::fs::write(notes_dir.join("a.md"), "Hello world from note A").unwrap();
        std::fs::write(notes_dir.join("b.txt"), "Note B content here").unwrap();

        let config = test_config(tmp.path());
        let db = Db::open(&config.db_path).unwrap();

        let src_config = FilesSourceConfig {
            paths: vec![notes_dir.clone()],
            extensions: vec!["md".into(), "txt".into()],
            ..Default::default()
        };
        let source = FilesSource { config: src_config };
        let count = source.ingest(&db, &config).unwrap();
        assert_eq!(count, 2); // one chunk per small file

        // Re-ingest with no changes — should produce 0 new chunks.
        let src_config2 = FilesSourceConfig {
            paths: vec![notes_dir],
            extensions: vec!["md".into(), "txt".into()],
            ..Default::default()
        };
        let source2 = FilesSource {
            config: src_config2,
        };
        let count2 = source2.ingest(&db, &config).unwrap();
        assert_eq!(count2, 0);
    }

    #[test]
    fn detects_file_changes() {
        let tmp = TempDir::new().unwrap();
        let notes_dir = tmp.path().join("notes");
        std::fs::create_dir_all(&notes_dir).unwrap();
        std::fs::write(notes_dir.join("a.md"), "Version 1").unwrap();

        let config = test_config(tmp.path());
        let db = Db::open(&config.db_path).unwrap();

        let src_config = FilesSourceConfig {
            paths: vec![notes_dir.clone()],
            ..Default::default()
        };

        // First ingest.
        let s = FilesSource {
            config: src_config.clone(),
        };
        assert_eq!(s.ingest(&db, &config).unwrap(), 1);

        // Modify file.
        std::fs::write(notes_dir.join("a.md"), "Version 2 with changes").unwrap();
        let s2 = FilesSource {
            config: src_config.clone(),
        };
        let count = s2.ingest(&db, &config).unwrap();
        assert_eq!(count, 1); // old chunk replaced

        // Verify content is updated.
        let results = crate::db::search::recall(&db, "Version 2", 10).unwrap();
        assert!(!results.is_empty());
        assert!(results[0].content.contains("Version 2"));
    }

    #[test]
    fn detects_file_deletions() {
        let tmp = TempDir::new().unwrap();
        let notes_dir = tmp.path().join("notes");
        std::fs::create_dir_all(&notes_dir).unwrap();
        std::fs::write(notes_dir.join("a.md"), "Temporary content").unwrap();

        let config = test_config(tmp.path());
        let db = Db::open(&config.db_path).unwrap();

        let src_config = FilesSourceConfig {
            paths: vec![notes_dir.clone()],
            ..Default::default()
        };

        // Ingest.
        let s = FilesSource {
            config: src_config.clone(),
        };
        assert_eq!(s.ingest(&db, &config).unwrap(), 1);

        // Delete file.
        std::fs::remove_file(notes_dir.join("a.md")).unwrap();

        // Re-ingest — should clean up.
        let s2 = FilesSource { config: src_config };
        s2.ingest(&db, &config).unwrap();

        // Verify chunk is gone.
        let results = crate::db::search::recall(&db, "Temporary content", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn extension_filter() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("docs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("readme.md"), "Markdown content").unwrap();
        std::fs::write(dir.join("data.bin"), "Binary content").unwrap();

        let config = test_config(tmp.path());
        let db = Db::open(&config.db_path).unwrap();

        let src_config = FilesSourceConfig {
            paths: vec![dir],
            extensions: vec!["md".into()],
            ..Default::default()
        };
        let s = FilesSource { config: src_config };
        let count = s.ingest(&db, &config).unwrap();
        assert_eq!(count, 1); // only .md
    }

    #[test]
    fn exclude_patterns() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("project");
        let target = dir.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(dir.join("README.md"), "Project readme").unwrap();
        std::fs::write(target.join("output.txt"), "Build output").unwrap();

        let config = test_config(tmp.path());
        let db = Db::open(&config.db_path).unwrap();

        let src_config = FilesSourceConfig {
            paths: vec![dir],
            exclude: vec!["target/".into()],
            ..Default::default()
        };
        let s = FilesSource { config: src_config };
        let count = s.ingest(&db, &config).unwrap();
        assert_eq!(count, 1); // target/ excluded
    }
}
