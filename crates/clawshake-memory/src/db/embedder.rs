use anyhow::Result;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

/// Embedding dimensionality for the chosen model (BAAI/bge-small-en-v1.5).
pub const DIMS: usize = 384;

/// Canonical model name stored in the meta table for mismatch detection.
pub const MODEL_NAME: &str = "BAAI/bge-small-en-v1.5";

/// Local embedding model backed by fastembed + ONNX Runtime.
///
/// Uses `BAAI/bge-small-en-v1.5` (384 dims, ~66 MB, no API key required).
/// Model weights are cached in `~/.clawshake/fastembed_cache/`.
pub struct Embedder {
    model: TextEmbedding,
}

impl Embedder {
    /// Initialise the embedding model. Downloads weights on first run.
    pub fn new() -> Result<Self> {
        let cache_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".clawshake")
            .join("fastembed_cache");
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::BGESmallENV15)
                .with_show_download_progress(true)
                .with_cache_dir(cache_dir),
        )?;
        Ok(Self { model })
    }

    /// Embed a batch of texts. Returns one `Vec<f32>` per input.
    /// fastembed handles internal batching (default 256 per batch).
    pub fn embed(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        Ok(self.model.embed(texts, None)?)
    }

    /// Serialise a float vector to the compact little-endian binary format
    /// expected by sqlite-vec `float[N]` columns.
    pub fn to_blob(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|f| f.to_le_bytes()).collect()
    }
}
