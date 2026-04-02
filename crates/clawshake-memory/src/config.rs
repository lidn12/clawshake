use std::path::PathBuf;

/// Runtime configuration for the memory store.
///
/// Infrastructure-level settings only: paths to the database, transcript
/// directory, and skill discovery directories.  Chunking parameters are
/// owned by each source adapter (via `ChunkStrategy`) so every source can
/// choose its own strategy.
#[derive(Debug, Clone)]
pub struct Config {
    /// Path to the SQLite database file.
    pub db_path: PathBuf,

    /// Path to the directory containing daily transcript JSONL files.
    pub transcript_dir: PathBuf,

    /// Directories to scan for AgentSkills `SKILL.md` folders, in precedence
    /// order (first wins on name collision).
    ///
    /// Convention: project-level > user-level > cross-client.
    pub skill_dirs: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self {
            db_path: home.join(".clawshake").join("memory.db"),
            transcript_dir: home.join(".clawshake").join("log"),
            skill_dirs: vec![home.join(".clawshake").join("skills")],
        }
    }
}
