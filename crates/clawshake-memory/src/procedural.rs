//! Procedural memory — the agent's always-resident knowledge.
//!
//! Two segments, mapped to the cognitive taxonomy:
//!
//! - **System**: who I am and how I operate — loaded from `system.md`.
//! - **Skills**: what I know how to do — discovered via AgentSkills spec.
//!
//! System is human-authored and loaded from `~/.clawshake/system.md`.
//! Skills are discovered from `SKILL.md` folders on disk at startup.
//! Only the catalog (name + description + location) is loaded into the
//! system prompt; full skill content is read on demand by the agent.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::skills;

/// The agent's procedural memory: always-resident knowledge loaded into
/// every context window.
///
/// Corresponds to "procedural memory" in the cognitive taxonomy:
/// knowledge about *how to do things* that doesn't require conscious recall.
///
/// The two segments are concatenated into the system prompt's code segment.
/// System is a human-authored default that ships with the binary.
/// Skills is a catalog of discovered AgentSkills — metadata only
/// (name + description + location).  Full skill content is loaded on demand
/// by the agent via its file-read tool.
#[derive(Debug, Clone)]
pub struct Procedural {
    /// Who I am and how I operate — name, personality, safety rules,
    /// event loop rules, tool conventions.
    /// Human-authored, loaded from `~/.clawshake/system.md`.
    pub system: String,

    /// Discovered skill catalog — rendered from AgentSkills `SKILL.md` files.
    /// Contains name + description + location per skill (tier 1 metadata).
    /// Empty if no skills are found.
    pub skills: String,
}

impl Default for Procedural {
    fn default() -> Self {
        Self {
            system: DEFAULT_SYSTEM.trim().to_string(),
            skills: String::new(),
        }
    }
}

impl Procedural {
    /// Load procedural memory from the given paths.
    ///
    /// - `system_path`: path to `system.md`; read if it exists, otherwise
    ///   falls back to [`DEFAULT_SYSTEM`].
    /// - `skill_dirs`: directories to scan for AgentSkills folders, in
    ///   precedence order (first wins on name collision).
    pub fn load(system_path: &Path, skill_dirs: &[PathBuf]) -> Result<Self> {
        let system = read_md(system_path, DEFAULT_SYSTEM);

        let discovered = skills::discover(skill_dirs);
        let catalog = skills::render_catalog(&discovered);

        if !discovered.is_empty() {
            eprintln!("[ashby] {} skill(s) discovered", discovered.len());
        }

        Ok(Self {
            system,
            skills: catalog,
        })
    }

    /// Render the full procedural memory as a single string for the system prompt.
    ///
    /// Sections are joined with double newlines.
    /// Empty sections are omitted.
    pub fn render(&self) -> String {
        let mut out = String::new();

        if !self.system.is_empty() {
            out.push_str(&self.system);
        }

        if !self.skills.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str("## Skills\n\n");
            out.push_str(&self.skills);
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read a markdown file, trimming whitespace.
/// If the file is absent or empty, seeds it with `default` and returns that.
fn read_md(path: &Path, default: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            // Seed the file so the user can discover and edit it.
            let content = default.trim();
            let _ = std::fs::write(path, content);
            content.to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// Default content — compiled-in procedural memory
// ---------------------------------------------------------------------------

pub const DEFAULT_SYSTEM: &str = r###"
You are Ashby, a personal assistant running as a persistent daemon on the user's machine.

You have access to tools via Clawshake — a local tool network that exposes the user's apps,
files, and remote capabilities. Use tools freely to get things done.

## Event loop

Your execution model is an event loop driven by listen(). The runtime calls listen()
automatically between turns — you only need to call it explicitly if you want to block
mid-turn waiting for a specific event. The runtime also automatically routes your text
responses back to the channel that sent the triggering event.

## Context management

The status line shows: [conversation: X / MAX].
The runtime automatically manages context. You do not need to manage context yourself.

- **Tool result offloading:** When a tool result is too large for the context window,
  the full output is saved to a file and only a short head preview is kept in context.
  The preview includes the file path. If you need the full content, read the file.
- **Compaction:** Older conversation history is periodically compacted into a progress
  summary that appears at the start of the conversation.

When your first message contains a "Session Progress" section, it means your earlier
conversation history was compacted. The summary is your own work — treat it as session
memory and continue naturally from where you left off. Do not re-introduce yourself or
re-greet the user.

### checkpoint(note)

`checkpoint(note)` tells the runtime you finished a discrete task. It triggers aggressive
compaction: most earlier messages are replaced by a concise progress summary, freeing context
for the next task. Pass a one-line note like "wrote comparison doc to docs/foo.md". After
checkpoint, you will only see the progress summary and your most recent turn — earlier tool
results and reasoning will be gone.

## Other guidance

- Use tools to get things done rather than asking the user to do it for you.
- Be direct and concise.
"###;
