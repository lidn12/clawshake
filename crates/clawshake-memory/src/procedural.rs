//! Procedural memory — the agent's always-resident knowledge.
//!
//! Three segments, mapped to the cognitive taxonomy:
//!
//! - **Identity**: who I am — personality, safety rules.
//! - **Instructions**: how I operate — event loop rules, tool conventions.
//! - **Skills**: what I know how to do — discovered via AgentSkills spec.
//!
//! Identity and instructions are human-authored and rarely change.
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
/// The three segments are concatenated into the system prompt's code segment.
/// Identity and instructions are human-authored defaults that ship with the
/// binary.  Skills is a catalog of discovered AgentSkills — metadata only
/// (name + description + location).  Full skill content is loaded on demand
/// by the agent via its file-read tool.
#[derive(Debug, Clone)]
pub struct Procedural {
    /// Who I am — name, personality, safety rules.
    /// Human-authored, rarely changes.
    pub identity: String,

    /// How I operate — event loop rules, context guidance, tool conventions.
    /// Human-authored, changes with the codebase.
    pub instructions: String,

    /// Discovered skill catalog — rendered from AgentSkills `SKILL.md` files.
    /// Contains name + description + location per skill (tier 1 metadata).
    /// Empty if no skills are found.
    pub skills: String,
}

impl Default for Procedural {
    fn default() -> Self {
        Self {
            identity: DEFAULT_IDENTITY.trim().to_string(),
            instructions: DEFAULT_INSTRUCTIONS.trim().to_string(),
            skills: String::new(),
        }
    }
}

impl Procedural {
    /// Load procedural memory from the given paths.
    ///
    /// - `identity_path`: path to `identity.md`; read if it exists, otherwise
    ///   falls back to [`DEFAULT_IDENTITY`].
    /// - `instructions_path`: path to `instructions.md`; read if it exists,
    ///   otherwise falls back to [`DEFAULT_INSTRUCTIONS`].
    /// - `skill_dirs`: directories to scan for AgentSkills folders, in
    ///   precedence order (first wins on name collision).
    ///
    /// Both markdown files are created at their default paths by the
    /// `load_config()` scaffold on first run, so they will normally exist.
    pub fn load(
        identity_path: &Path,
        instructions_path: &Path,
        skill_dirs: &[PathBuf],
    ) -> Result<Self> {
        let identity = read_md(identity_path, DEFAULT_IDENTITY);
        let instructions = read_md(instructions_path, DEFAULT_INSTRUCTIONS);

        let discovered = skills::discover(skill_dirs);
        let catalog = skills::render_catalog(&discovered);

        if !discovered.is_empty() {
            eprintln!("[ashby] {} skill(s) discovered", discovered.len());
        }

        Ok(Self {
            identity,
            instructions,
            skills: catalog,
        })
    }

    /// Render the full procedural memory as a single string for the system prompt.
    ///
    /// Sections are joined with double newlines and clear Markdown headings.
    /// Empty sections are omitted.
    pub fn render(&self) -> String {
        let mut out = String::new();

        if !self.identity.is_empty() {
            out.push_str(&self.identity);
        }

        if !self.instructions.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&self.instructions);
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
/// Falls back to `default` if the file is absent, unreadable, or empty.
fn read_md(path: &Path, default: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => default.trim().to_string(),
    }
}

// ---------------------------------------------------------------------------
// Default content — compiled-in procedural memory
// ---------------------------------------------------------------------------

pub const DEFAULT_IDENTITY: &str = r###"
You are Ashby, a personal assistant running as a persistent daemon on the user's machine.

You have access to tools via Clawshake — a local tool network that exposes the user's apps,
files, and remote capabilities. Use tools freely to get things done.
"###;

pub const DEFAULT_INSTRUCTIONS: &str = r###"
## Event loop

Your execution model is an event loop. Every turn must end with a tool call — never output
text alone without also calling a tool. If you have nothing else to do, end the turn by
calling listen() to block until the next event arrives.

## Handling user messages

When listen() returns an event with topic "user.message", the event's data.content field is
a direct message from the user. Read it, respond directly in your content field, and call
listen() in the same turn to wait for the next message. Do not narrate that you received a
message — just respond to it.

Example of correct behaviour:
  content: "I'm Ashby, your personal assistant. How can I help?"
  tool_calls: [listen()]

Example of incorrect behaviour (never do this):
  content: "It looks like you sent a message. Shall I respond?"
  tool_calls: []   ← missing tool call, wastes a cycle

## Context management

The status line shows: [conversation: X / MAX].
The runtime automatically manages context: large tool results are truncated on ingest,
and older conversation history is periodically compacted into a progress summary that
appears at the start of the conversation. You do not need to manage context yourself.

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

## Recalled memories

When your system prompt includes a "Recalled memories" section, these are facts retrieved
from your long-term memory that may be relevant to the current conversation. Use them
naturally — they are things you know. Do not call attention to the retrieval mechanism.

## Other guidance

- Use tools to get things done rather than asking the user to do it for you.
- Be direct and concise.
"###;
