use anyhow::Result;
use clap::{Parser, Subcommand};

use ashby_memory::ingest::SourceIngestor;
use ashby_memory::{
    db, Config, Db, Embedder, FilesSource, FilesSourceConfig, WatchConfig, MODEL_NAME,
};

#[derive(Parser)]
#[command(
    name = "ashby-memory",
    about = "Ashby memory — transcript chunker and recall tool"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Ingest new transcript entries into the chunk store.
    Ingest,

    /// Generate embeddings for all un-embedded chunks.
    Embed,

    /// Search memory and print results as JSON.
    /// Uses hybrid BM25 + vector recall if embeddings exist, otherwise BM25 only.
    Recall {
        /// The search query.
        query: String,

        /// Maximum number of results to return.
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
    },

    /// Ingest text files from specified directories.
    IngestFiles {
        /// Directories to index (walked recursively).
        #[arg(required = true)]
        paths: Vec<std::path::PathBuf>,

        /// File extensions to include (without dot). If omitted, all files are included.
        #[arg(short, long)]
        ext: Vec<String>,

        /// Patterns to exclude (files whose relative path contains any pattern are skipped).
        #[arg(long)]
        exclude: Vec<String>,

        /// Path prefix for chunk provenance in the index.
        #[arg(long, default_value = "files/")]
        prefix: String,
    },

    /// Watch directories for changes and re-ingest automatically.
    ///
    /// By default watches the transcript directory. Use --files to also
    /// watch additional directories for file-source changes.
    Watch {
        /// Also watch these directories for file changes (walked recursively).
        #[arg(long)]
        files: Vec<std::path::PathBuf>,

        /// File extensions to include for --files dirs (without dot).
        #[arg(short, long)]
        ext: Vec<String>,

        /// Exclude patterns for --files dirs.
        #[arg(long)]
        exclude: Vec<String>,

        /// Path prefix for file chunks.
        #[arg(long, default_value = "files/")]
        prefix: String,

        /// Disable transcript watching.
        #[arg(long)]
        no_transcripts: bool,

        /// Debounce interval in seconds.
        #[arg(long, default_value_t = 2)]
        debounce: u64,
    },
}

fn main() -> Result<()> {
    // On Windows, ensure stdout/stderr use UTF-8 regardless of the system code page.
    #[cfg(windows)]
    unsafe {
        extern "system" {
            fn SetConsoleOutputCP(wCodePageID: u32) -> i32;
        }
        SetConsoleOutputCP(65001);
    }

    let cli = Cli::parse();
    let config = Config::default();

    match cli.command {
        Command::Ingest => {
            let db = Db::open(&config.db_path)?;
            let strategy = ashby_memory::ingest::ChunkStrategy::default();
            let count = ashby_memory::ingest::transcript::run_chunker(&db, &config, &strategy)?;
            eprintln!("ingested {count} chunks");
        }

        Command::Embed => {
            let db = Db::open(&config.db_path)?;
            db.ensure_model_match(MODEL_NAME)?;
            let chunks = db.get_unembedded_chunks()?;
            if chunks.is_empty() {
                eprintln!("nothing to embed");
                return Ok(());
            }
            eprintln!("loading embedding model (downloads on first run)…");
            let mut embedder = Embedder::new()?;
            let total = chunks.len();
            let texts: Vec<String> = chunks.iter().map(|(_, c)| c.clone()).collect();
            let embeddings = embedder.embed(texts)?;
            for ((id, _), emb) in chunks.iter().zip(embeddings.iter()) {
                db.insert_embedding(*id, emb)?;
            }
            eprintln!("embedded {total} chunks");
        }

        Command::Recall { query, limit } => {
            let db = Db::open(&config.db_path)?;
            let results = if db.has_embeddings()? {
                let mut embedder = Embedder::new()?;
                db::search::recall_hybrid(&db, &mut embedder, &query, limit)?
            } else {
                db::search::recall(&db, &query, limit)?
            };
            println!("{}", serde_json::to_string_pretty(&results)?);
        }

        Command::IngestFiles {
            paths,
            ext,
            exclude,
            prefix,
        } => {
            let db = Db::open(&config.db_path)?;
            let src = FilesSourceConfig {
                paths,
                extensions: ext,
                exclude,
                path_prefix: prefix,
                ..Default::default()
            };
            let source = FilesSource { config: src };
            let count = source.ingest(&db, &config)?;
            eprintln!("ingested {count} chunks from files");
        }

        Command::Watch {
            files,
            ext,
            exclude,
            prefix,
            no_transcripts,
            debounce,
        } => {
            let file_sources = if files.is_empty() {
                Vec::new()
            } else {
                vec![FilesSourceConfig {
                    paths: files,
                    extensions: ext,
                    exclude,
                    path_prefix: prefix,
                    ..Default::default()
                }]
            };

            let wc = WatchConfig {
                file_sources,
                watch_transcripts: !no_transcripts,
                debounce: std::time::Duration::from_secs(debounce),
            };

            ashby_memory::watch::watch(&config, &wc)?;
        }
    }

    Ok(())
}
