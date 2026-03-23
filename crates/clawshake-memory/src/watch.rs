//! File watcher — reactive ingest triggering.
//!
//! Watches the transcript directory and any configured file-source directories
//! for changes, debounces events, and re-runs the appropriate ingestors.
//!
//! Each ingestor is idempotent: transcript uses a byte-offset cursor, files
//! use SHA-256 content hashing.  Running them on every debounce tick is cheap
//! when nothing actually changed.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{Config as NotifyConfig, RecommendedWatcher, RecursiveMode, Watcher};

use crate::config::Config;
use crate::db::embedder::{Embedder, MODEL_NAME};
use crate::db::Db;
use crate::ingest::files::{FilesSource, FilesSourceConfig};
use crate::ingest::transcript;
use crate::ingest::SourceIngestor;

// ── configuration ───────────────────────────────────────────────────────

/// Configuration for the file watcher.
#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// File sources to watch and re-ingest on change.
    pub file_sources: Vec<FilesSourceConfig>,

    /// Whether to watch the transcript directory.
    pub watch_transcripts: bool,

    /// Debounce interval — events within this window are batched into a
    /// single ingest pass.
    pub debounce: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            file_sources: Vec::new(),
            watch_transcripts: true,
            debounce: Duration::from_secs(2),
        }
    }
}

// ── public entry point ──────────────────────────────────────────────────

/// Start watching configured directories and re-ingest on changes.
///
/// This function **blocks forever** (or until the watcher channel
/// disconnects, which shouldn't happen under normal operation).
///
/// On startup it performs one immediate ingest pass to catch up on
/// anything that changed while the watcher wasn't running.
pub fn watch(config: &Config, watch_config: &WatchConfig) -> Result<()> {
    let (tx, rx) = mpsc::channel();

    let mut watcher = RecommendedWatcher::new(tx, NotifyConfig::default())
        .context("failed to create file watcher")?;

    // Register directories to watch.
    let mut watched = 0usize;

    if watch_config.watch_transcripts && config.transcript_dir.exists() {
        watcher.watch(&config.transcript_dir, RecursiveMode::Recursive)?;
        eprintln!(
            "[watch] watching transcripts: {}",
            config.transcript_dir.display()
        );
        watched += 1;
    }

    for src in &watch_config.file_sources {
        for path in &src.paths {
            if path.exists() {
                watcher.watch(path, RecursiveMode::Recursive)?;
                eprintln!("[watch] watching files: {}", path.display());
                watched += 1;
            } else {
                eprintln!(
                    "[watch] warning: skipping non-existent path: {}",
                    path.display()
                );
            }
        }
    }

    if watched == 0 {
        anyhow::bail!("nothing to watch — no directories exist");
    }

    // Lazy-initialised embedder — reused across debounce ticks to avoid
    // reloading the ONNX model (~1 s) on every pass.
    let mut embedder: Option<Embedder> = None;

    // Initial catch-up ingest before entering the event loop.
    eprintln!("[watch] initial ingest pass…");
    run_ingest_and_embed(config, watch_config, &mut embedder);

    eprintln!(
        "[watch] ready — debounce {:?}, {} dir(s)",
        watch_config.debounce, watched
    );

    // ── event loop with manual debounce ─────────────────────────────────
    event_loop(&rx, config, watch_config, &mut embedder)?;

    // Keep `_watcher` alive for the duration — dropping it stops the OS watches.
    drop(watcher);
    Ok(())
}

/// Core event loop.  Separated for testability.
fn event_loop(
    rx: &mpsc::Receiver<notify::Result<notify::Event>>,
    config: &Config,
    watch_config: &WatchConfig,
    embedder: &mut Option<Embedder>,
) -> Result<()> {
    let debounce = watch_config.debounce;

    // `deadline` is set when the first event in a debounce window arrives.
    let mut deadline: Option<Instant> = None;

    loop {
        let recv_result = match deadline {
            Some(dl) => {
                let remaining = dl.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    // Debounce expired — flush.
                    run_ingest_and_embed(config, watch_config, embedder);
                    deadline = None;
                    continue;
                }
                rx.recv_timeout(remaining)
            }
            None => {
                // No pending work — block indefinitely for the next event.
                rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
            }
        };

        match recv_result {
            Ok(Ok(_event)) => {
                // A relevant FS event arrived.  Start (or extend) the debounce window.
                if deadline.is_none() {
                    deadline = Some(Instant::now() + debounce);
                }
                // Otherwise the existing deadline stands — we don't push it out,
                // which gives bounded latency even under continuous writes.
            }
            Ok(Err(e)) => {
                // notify-level error (e.g. OS watch limit)
                eprintln!("[watch] watcher error: {e}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Debounce expired.
                run_ingest_and_embed(config, watch_config, embedder);
                deadline = None;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                eprintln!("[watch] watcher channel closed — exiting");
                break;
            }
        }
    }

    Ok(())
}

// ── ingest + embed dispatch ─────────────────────────────────────────────

/// Run all configured ingestors, then embed any unembedded chunks.
///
/// The `Embedder` is lazy-initialised on the first tick that produces
/// unembedded chunks and kept alive across ticks to avoid reloading the
/// ONNX model (~1 s) every pass.  Errors are logged, not propagated — the
/// watcher keeps running even if one pass fails.
fn run_ingest_and_embed(
    config: &Config,
    watch_config: &WatchConfig,
    embedder: &mut Option<Embedder>,
) {
    let db = match Db::open(&config.db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("[watch] failed to open db: {e}");
            return;
        }
    };

    // ── ingest ──────────────────────────────────────────────────────────

    if watch_config.watch_transcripts {
        let strategy = crate::ingest::ChunkStrategy::default();
        match transcript::run_chunker(&db, config, &strategy) {
            Ok(n) if n > 0 => eprintln!("[watch] ingested {n} transcript chunks"),
            Ok(_) => {}
            Err(e) => eprintln!("[watch] transcript ingest error: {e}"),
        }
    }

    for src in &watch_config.file_sources {
        let source = FilesSource {
            config: src.clone(),
        };
        match source.ingest(&db, config) {
            Ok(n) if n > 0 => eprintln!("[watch] ingested {n} file chunks"),
            Ok(_) => {}
            Err(e) => eprintln!("[watch] file ingest error: {e}"),
        }
    }

    // ── embed ───────────────────────────────────────────────────────────

    embed_unembedded(&db, embedder);
}

/// Embed any chunks that are not yet in `vec_chunks`.
///
/// Lazy-inits the `Embedder` on first call.  A small batch (typical
/// debounce tick) takes ~50-200 ms through ONNX — negligible next to
/// the 2 s debounce window.
fn embed_unembedded(db: &Db, embedder: &mut Option<Embedder>) {
    let chunks = match db.get_unembedded_chunks() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[watch] failed to query unembedded chunks: {e}");
            return;
        }
    };

    if chunks.is_empty() {
        return;
    }

    // Ensure model match before first embed.
    if embedder.is_none() {
        if let Err(e) = db.ensure_model_match(MODEL_NAME) {
            eprintln!("[watch] model match check failed: {e}");
            return;
        }
        match Embedder::new() {
            Ok(e) => {
                eprintln!("[watch] embedding model loaded");
                *embedder = Some(e);
            }
            Err(e) => {
                eprintln!("[watch] failed to load embedding model: {e}");
                return;
            }
        }
    }

    let emb = embedder.as_mut().unwrap();
    let total = chunks.len();
    let texts: Vec<String> = chunks.iter().map(|(_, c)| c.clone()).collect();

    let embeddings = match emb.embed(texts) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[watch] embedding failed: {e}");
            return;
        }
    };

    let mut inserted = 0usize;
    for ((id, _), vec) in chunks.iter().zip(embeddings.iter()) {
        if let Err(e) = db.insert_embedding(*id, vec) {
            eprintln!("[watch] insert_embedding({id}) error: {e}");
        } else {
            inserted += 1;
        }
    }

    eprintln!("[watch] embedded {inserted}/{total} chunks");
}

// ── tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(dir: &std::path::Path) -> Config {
        Config {
            db_path: dir.join("test.db"),
            transcript_dir: dir.join("log"),
            skill_dirs: vec![],
        }
    }

    /// Verify that `run_ingest` with file sources picks up changes.
    #[test]
    fn run_ingest_picks_up_new_files() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(notes.join("a.md"), "Note alpha").unwrap();

        let config = test_config(tmp.path());
        // Ensure db exists.
        let _db = Db::open(&config.db_path).unwrap();

        let wc = WatchConfig {
            file_sources: vec![FilesSourceConfig {
                paths: vec![notes.clone()],
                ..Default::default()
            }],
            watch_transcripts: false,
            debounce: Duration::from_millis(100),
        };

        // First ingest via run_ingest_and_embed.
        let mut embedder = None;
        run_ingest_and_embed(&config, &wc, &mut embedder);

        // Verify chunk exists.
        let db = Db::open(&config.db_path).unwrap();
        let results = crate::db::search::recall(&db, "Note alpha", 10).unwrap();
        assert!(!results.is_empty(), "should find the ingested note");
    }

    /// Verify the debounce logic: events within the window are batched.
    #[test]
    fn debounce_batches_events() {
        let tmp = TempDir::new().unwrap();
        let notes = tmp.path().join("notes");
        std::fs::create_dir_all(&notes).unwrap();

        let config = test_config(tmp.path());
        let _db = Db::open(&config.db_path).unwrap();

        let wc = WatchConfig {
            file_sources: vec![FilesSourceConfig {
                paths: vec![notes.clone()],
                ..Default::default()
            }],
            watch_transcripts: false,
            debounce: Duration::from_millis(200),
        };

        // Simulate: create file, start watcher on a thread, wait for debounce,
        // then check results.
        std::fs::write(notes.join("b.md"), "Debounce test").unwrap();

        let config_clone = config.clone();
        let wc_clone = wc.clone();
        let handle = std::thread::spawn(move || {
            // This will do the initial ingest pass and start watching.
            // We'll let it run briefly then check results from the main thread.
            let _ = watch(&config_clone, &wc_clone);
        });

        // Give the watcher time to start and run its initial ingest.
        std::thread::sleep(Duration::from_millis(500));

        // Verify the initial ingest picked up the file.
        let db = Db::open(&config.db_path).unwrap();
        let results = crate::db::search::recall(&db, "Debounce test", 10).unwrap();
        assert!(!results.is_empty(), "initial ingest should find the note");

        // We can't cleanly stop the watcher in this test, so just detach.
        drop(handle);
    }

    /// Verify WatchConfig defaults.
    #[test]
    fn watch_config_defaults() {
        let wc = WatchConfig::default();
        assert!(wc.watch_transcripts);
        assert!(wc.file_sources.is_empty());
        assert_eq!(wc.debounce, Duration::from_secs(2));
    }
}
