//! In-process handlers for memory tools.
//!
//! These are dispatched by the router for the built-in memory tools.
//! All operations are synchronous (SQLite + filesystem) so they run on
//! `spawn_blocking` to avoid stalling the async event loop.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use clawshake_core::config::MemoryConfig;
use clawshake_memory::{
    append_entry, recall, ChunkStrategy, Config as MemConfig, FilesSourceConfig, Procedural,
    WatchConfig, WriteEntry,
};

/// Shared memory context passed through the broker.
///
/// Holds the memory configuration and resolved paths needed by all
/// memory tool handlers.  Cheaply cloneable (all paths are in an Arc).
#[derive(Clone, Debug)]
pub struct MemoryContext {
    inner: Arc<MemoryContextInner>,
}

#[derive(Debug)]
struct MemoryContextInner {
    pub config: MemConfig,
    pub identity_path: PathBuf,
    pub instructions_path: PathBuf,
}

impl MemoryContext {
    /// Create a new memory context from resolved paths.
    pub fn new(
        config: MemConfig,
        identity_path: PathBuf,
        instructions_path: PathBuf,
    ) -> Self {
        Self {
            inner: Arc::new(MemoryContextInner {
                config,
                identity_path,
                instructions_path,
            }),
        }
    }

    pub fn config(&self) -> &MemConfig {
        &self.inner.config
    }
}

// ---------------------------------------------------------------------------
// Tool handlers
// ---------------------------------------------------------------------------

/// `memory_recall` — search long-term memory (visible to the agent).
///
/// Arguments: `{ "query": "...", "limit": N }`
pub async fn invoke_recall(args: &Value, mem: &MemoryContext) -> Result<String> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(10) as usize;

    let cfg = mem.config().clone();
    let result =
        tokio::task::spawn_blocking(move || recall(&cfg, &query, limit))
            .await
            .context("memory_recall spawn_blocking join")?
            .context("memory_recall search")?;

    serde_json::to_string_pretty(&result).context("serializing recall results")
}

/// `memory_procedural` — load and render the agent's procedural memory
/// (identity + instructions + skills).  Hidden infrastructure tool.
///
/// Arguments: `{}` (none required)
pub async fn invoke_procedural(args: &Value, mem: &MemoryContext) -> Result<String> {
    let _ = args; // no arguments currently used
    let identity_path = mem.inner.identity_path.clone();
    let instructions_path = mem.inner.instructions_path.clone();
    let skill_dirs = mem.config().skill_dirs.clone();

    let result = tokio::task::spawn_blocking(move || {
        Procedural::load(&identity_path, &instructions_path, &skill_dirs)
            .map(|p| p.render())
    })
    .await
    .context("memory_procedural spawn_blocking join")?
    .context("memory_procedural load")?;

    Ok(result)
}

/// `memory_append` — append an entry to today's transcript.  Hidden
/// infrastructure tool called by the agent loop after each message.
///
/// Arguments: `{ "role": "user|assistant|tool", "content": "...",
///              "name": "tool_name" (optional), "op": "append|reset|compact" (optional),
///              "reason": "..." (optional) }`
pub async fn invoke_append(args: &Value, mem: &MemoryContext) -> Result<String> {
    let role = args.get("role").and_then(|v| v.as_str());
    let content = args.get("content").and_then(|v| v.as_str());
    let name = args.get("name").and_then(|v| v.as_str());
    let reason = args.get("reason").and_then(|v| v.as_str());
    let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("append");

    let entry = match op {
        "reset" | "compact" => WriteEntry {
            v: 1,
            ts: None,
            op: if op == "reset" {
                clawshake_memory::write::Op::Reset
            } else {
                clawshake_memory::write::Op::Compact
            },
            role: role.map(|s| s.to_string()),
            content: content.map(|s| s.to_string()),
            name: name.map(|s| s.to_string()),
            reason: reason.map(|s| s.to_string()),
        },
        _ => {
            // Default "append" path — construct via convenience methods where possible.
            if let (Some(name_val), Some(content_val)) = (name, content) {
                if role == Some("tool") {
                    WriteEntry::tool_result(name_val, content_val)
                } else {
                    WriteEntry::tool_call(name_val, content_val)
                }
            } else if let (Some(role_val), Some(content_val)) = (role, content) {
                WriteEntry::message(role_val, content_val)
            } else {
                anyhow::bail!(
                    "memory_append requires at least 'role' + 'content', or 'name' + 'content'"
                );
            }
        }
    };

    let cfg = mem.config().clone();
    tokio::task::spawn_blocking(move || append_entry(&cfg, entry))
        .await
        .context("memory_append spawn_blocking join")?
        .context("memory_append write")?;

    Ok("ok".into())
}

/// `memory_ingest` — trigger a manual ingest pass (transcript + files).
/// Hidden operational tool.
///
/// Arguments: `{}` (none required)
pub async fn invoke_ingest(args: &Value, mem: &MemoryContext) -> Result<String> {
    let _ = args;
    let cfg = mem.config().clone();

    let count = tokio::task::spawn_blocking(move || -> Result<usize> {
        let db = clawshake_memory::Db::open(&cfg.db_path)?;
        let strategy = clawshake_memory::ChunkStrategy::default();
        clawshake_memory::ingest::transcript::run_chunker(&db, &cfg, &strategy)
    })
    .await
    .context("memory_ingest spawn_blocking join")?
    .context("memory_ingest chunker")?;

    Ok(format!("{count} chunks ingested"))
}

/// `memory_embed` — trigger embedding of any un-embedded chunks.
/// Hidden operational tool.
///
/// Arguments: `{}` (none required)
pub async fn invoke_embed(args: &Value, mem: &MemoryContext) -> Result<String> {
    let _ = args;
    let cfg = mem.config().clone();

    let count = tokio::task::spawn_blocking(move || -> Result<usize> {
        let db = clawshake_memory::Db::open(&cfg.db_path)?;
        let chunks = db.get_unembedded_chunks()?;
        if chunks.is_empty() {
            return Ok(0);
        }
        db.ensure_model_match(clawshake_memory::MODEL_NAME)?;
        let mut embedder = clawshake_memory::Embedder::new()?;
        let total = chunks.len();
        let texts: Vec<String> = chunks.iter().map(|(_, c)| c.clone()).collect();
        let embeddings = embedder.embed(texts)?;
        let mut inserted = 0usize;
        for ((id, _), vec) in chunks.iter().zip(embeddings.iter()) {
            db.insert_embedding(*id, vec)?;
            inserted += 1;
        }
        Ok(inserted)
    })
    .await
    .context("memory_embed spawn_blocking join")?
    .context("memory_embed embed")?;

    Ok(format!("{count} chunks embedded"))
}

// ---------------------------------------------------------------------------
// Tool definitions (JSON schemas for registration)
// ---------------------------------------------------------------------------

/// Return the memory tool definitions as JSON values compatible with
/// `builtins::register`.
pub fn memory_tool_definitions() -> Vec<Value> {
    vec![
        serde_json::json!({
            "name": "memory_recall",
            "description": "Search your long-term memory (past conversations, notes, skills). Returns the most relevant chunks. Use this to recall information from previous sessions or knowledge you've accumulated.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural-language search query describing what you want to remember."
                    },
                    "limit": {
                        "type": "number",
                        "description": "Maximum number of results to return. Default: 10"
                    }
                },
                "required": ["query"]
            }
        }),
        serde_json::json!({
            "name": "memory_procedural",
            "description": "Load the agent's procedural memory (identity, instructions, skill catalog). Called at startup by the agent runtime.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            },
            "hidden": true
        }),
        serde_json::json!({
            "name": "memory_append",
            "description": "Append an entry to the conversation transcript. Called by the agent runtime after each message turn.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "role": {
                        "type": "string",
                        "description": "Message role: user, assistant, or tool"
                    },
                    "content": {
                        "type": "string",
                        "description": "Message content"
                    },
                    "name": {
                        "type": "string",
                        "description": "Tool name (for tool_call and tool_result entries)"
                    },
                    "op": {
                        "type": "string",
                        "description": "Operation: append (default), reset, or compact"
                    },
                    "reason": {
                        "type": "string",
                        "description": "Reason for reset/compact entries"
                    }
                },
                "required": ["role", "content"]
            },
            "hidden": true
        }),
        serde_json::json!({
            "name": "memory_ingest",
            "description": "Trigger a manual ingest pass over transcripts.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            },
            "hidden": true
        }),
        serde_json::json!({
            "name": "memory_embed",
            "description": "Trigger embedding of un-embedded chunks.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            },
            "hidden": true
        }),
    ]
}

// ---------------------------------------------------------------------------
// Startup helpers
// ---------------------------------------------------------------------------

/// Build a [`MemoryContext`] from the node's `[memory]` config section.
///
/// Resolves all paths relative to `clawshake_dir` (`~/.clawshake`).
pub fn build_memory_context(clawshake_dir: &Path, cfg: &MemoryConfig) -> MemoryContext {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));

    let mem_config = MemConfig {
        db_path: cfg
            .db_path
            .clone()
            .unwrap_or_else(|| clawshake_dir.join("memory.db")),
        transcript_dir: cfg
            .transcript_dir
            .clone()
            .unwrap_or_else(|| clawshake_dir.join("log")),
        skill_dirs: cfg.skill_dirs.clone().unwrap_or_else(|| {
            vec![
                clawshake_dir.join("skills"),
                home.join(".agents").join("skills"),
            ]
        }),
    };

    let identity_path = cfg
        .identity_path
        .clone()
        .unwrap_or_else(|| clawshake_dir.join("identity.md"));
    let instructions_path = cfg
        .instructions_path
        .clone()
        .unwrap_or_else(|| clawshake_dir.join("instructions.md"));

    MemoryContext::new(mem_config, identity_path, instructions_path)
}

/// Spawn the memory file-system watcher on a dedicated OS thread.
///
/// Watches the transcript directory and optional notes directory for changes,
/// debounces events, and re-runs ingest + embed.  The thread blocks forever.
pub fn start_watcher(clawshake_dir: &Path, cfg: &MemoryConfig) {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));

    let mem_config = MemConfig {
        db_path: cfg
            .db_path
            .clone()
            .unwrap_or_else(|| clawshake_dir.join("memory.db")),
        transcript_dir: cfg
            .transcript_dir
            .clone()
            .unwrap_or_else(|| clawshake_dir.join("log")),
        skill_dirs: cfg.skill_dirs.clone().unwrap_or_else(|| {
            vec![
                clawshake_dir.join("skills"),
                home.join(".agents").join("skills"),
            ]
        }),
    };

    let notes_dir = cfg
        .notes_dir
        .clone()
        .unwrap_or_else(|| clawshake_dir.join("notes"));

    let notes_src = FilesSourceConfig {
        paths: vec![notes_dir],
        chunk_strategy: ChunkStrategy::SlidingWindow {
            max_chars: cfg.ingest.chunk_max_chars,
            overlap_chars: cfg.ingest.chunk_overlap_chars,
        },
        ..FilesSourceConfig::default()
    };

    let wc = WatchConfig {
        watch_transcripts: cfg.watch.watch_transcripts,
        debounce: Duration::from_secs(cfg.watch.debounce_secs),
        file_sources: vec![notes_src],
    };

    // Ensure directories exist before the watcher tries to register them.
    let _ = std::fs::create_dir_all(&mem_config.transcript_dir);
    let _ = std::fs::create_dir_all(clawshake_dir.join("notes"));
    for d in &mem_config.skill_dirs {
        let _ = std::fs::create_dir_all(d);
    }

    std::thread::Builder::new()
        .name("memory-watcher".into())
        .spawn(move || {
            if let Err(e) = clawshake_memory::watch::watch(&mem_config, &wc) {
                tracing::warn!("Memory watcher failed: {e}");
            }
        })
        .expect("failed to spawn memory watcher thread");

    tracing::info!("Memory watcher started");
}
