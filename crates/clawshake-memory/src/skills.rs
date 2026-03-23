//! AgentSkills discovery and catalog rendering.
//!
//! Implements the [AgentSkills](https://agentskills.io/) open spec:
//!
//! - **Discovery**: scan directories for `<name>/SKILL.md` folders.
//! - **Parsing**: extract YAML frontmatter (`name`, `description`, optional fields).
//! - **Catalog**: render a compact list for the system prompt (tier 1 — metadata only).
//!
//! Tier 2 (full SKILL.md body) and tier 3 (bundled resources) are loaded on
//! demand by the agent via its file-read tool.  This module only handles tier 1.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Parsed metadata from one `SKILL.md` file.
///
/// Corresponds to the AgentSkills spec frontmatter:
/// - `name` (required): lowercase alphanum + hyphens, 1–64 chars.
/// - `description` (required): what the skill does and when to use it, 1–1024 chars.
/// - `license`, `compatibility`, `metadata`, `allowed-tools`: optional.
#[derive(Debug, Clone)]
pub struct SkillMeta {
    /// Short identifier.  Lowercase letters, digits, hyphens.
    pub name: String,
    /// What the skill does and when to use it.
    pub description: String,
    /// Absolute path to the `SKILL.md` file.
    pub location: PathBuf,
    /// Optional license identifier or reference.
    pub license: Option<String>,
    /// Optional environment requirements note (max 500 chars per spec).
    pub compatibility: Option<String>,
    /// Optional arbitrary key-value metadata (client extensions).
    pub metadata: Option<HashMap<String, serde_yaml::Value>>,
    /// Optional space-delimited list of pre-approved tools (experimental).
    pub allowed_tools: Option<String>,
}

/// Raw YAML frontmatter — deserialized directly, then validated.
#[derive(Deserialize)]
struct RawFrontmatter {
    name: Option<String>,
    description: Option<String>,
    license: Option<String>,
    compatibility: Option<String>,
    metadata: Option<HashMap<String, serde_yaml::Value>>,
    #[serde(rename = "allowed-tools")]
    allowed_tools: Option<String>,
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Discover skills from a list of directories, in precedence order.
///
/// Earlier directories win on name collision (project > user > bundled).
/// Directories that don't exist are silently skipped.
///
/// Returns the discovered skills and logs warnings to stderr for
/// malformed entries (per the spec's lenient validation guidance).
pub fn discover(dirs: &[PathBuf]) -> Vec<SkillMeta> {
    let mut seen: HashMap<String, usize> = HashMap::new(); // name → index in result
    let mut skills: Vec<SkillMeta> = Vec::new();

    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }

        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[skills] cannot read {}: {e}", dir.display());
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let skill_md = path.join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }

            match parse_skill_md(&skill_md) {
                Ok(meta) => {
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        seen.entry(meta.name.clone())
                    {
                        e.insert(skills.len());
                        skills.push(meta);
                    } else {
                        eprintln!(
                            "[skills] shadowed: {} (already loaded from higher-precedence dir)",
                            skill_md.display()
                        );
                    }
                }
                Err(e) => {
                    eprintln!("[skills] skipping {}: {e}", skill_md.display());
                }
            }
        }
    }

    skills
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a single `SKILL.md` file: extract YAML frontmatter, validate, return metadata.
fn parse_skill_md(path: &Path) -> Result<SkillMeta> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    let (yaml_str, _body) = split_frontmatter(&content)
        .with_context(|| format!("no valid YAML frontmatter in {}", path.display()))?;

    let raw: RawFrontmatter = serde_yaml::from_str(yaml_str)
        .with_context(|| format!("invalid YAML frontmatter in {}", path.display()))?;

    let name = raw
        .name
        .filter(|n| !n.is_empty())
        .with_context(|| format!("missing 'name' in {}", path.display()))?;

    let description = raw
        .description
        .filter(|d| !d.is_empty())
        .with_context(|| format!("missing 'description' in {}", path.display()))?;

    // Validate name per spec (warn but still load — lenient validation).
    if let Err(e) = validate_name(&name) {
        eprintln!("[skills] warning: {} in {}: {e}", name, path.display());
    }

    // Warn if name doesn't match parent directory.
    if let Some(parent_name) = path.parent().and_then(|p| p.file_name()) {
        if parent_name.to_string_lossy() != name {
            eprintln!(
                "[skills] warning: name '{}' does not match parent dir '{}' in {}",
                name,
                parent_name.to_string_lossy(),
                path.display()
            );
        }
    }

    // Canonicalize the path so the agent gets a usable absolute path.
    let location = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    Ok(SkillMeta {
        name,
        description,
        location,
        license: raw.license,
        compatibility: raw.compatibility,
        metadata: raw.metadata,
        allowed_tools: raw.allowed_tools,
    })
}

/// Split `---` delimited YAML frontmatter from the Markdown body.
///
/// Returns `(yaml_content, body)`.
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }

    // Skip the opening `---` line.
    let after_open = &trimmed[3..];
    let after_open = after_open
        .strip_prefix('\n')
        .or_else(|| after_open.strip_prefix("\r\n"))?;

    // Find the closing `---`.
    let close_pos = after_open
        .find("\n---")
        .map(|p| (p, p + 4))
        .or_else(|| after_open.find("\r\n---").map(|p| (p, p + 5)))?;

    let yaml = &after_open[..close_pos.0];

    // Body starts after the closing `---` line.
    let rest = &after_open[close_pos.1..];
    let body = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
        .unwrap_or(rest);

    Some((yaml, body))
}

/// Validate a skill name per the AgentSkills spec.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("name must be 1–64 characters, got {}", name.len());
    }
    if name.starts_with('-') || name.ends_with('-') {
        bail!("name must not start or end with a hyphen");
    }
    if name.contains("--") {
        bail!("name must not contain consecutive hyphens");
    }
    for ch in name.chars() {
        if !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && ch != '-' {
            bail!(
                "invalid character '{}' — only lowercase a-z, 0-9, hyphens allowed",
                ch
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Catalog rendering
// ---------------------------------------------------------------------------

/// Render the skill catalog for system prompt injection.
///
/// Produces a compact list with name, description, and location per skill.
/// The agent uses the location to read the full `SKILL.md` via its file-read
/// tool when it decides a skill is relevant (tier 2 activation).
///
/// Returns an empty string if there are no skills (caller should omit the
/// section entirely).
pub fn render_catalog(skills: &[SkillMeta]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "The following skills provide specialized instructions for specific tasks.\n\
         When a task matches a skill's description, use your file-read tool to load\n\
         the SKILL.md at the listed location before proceeding.\n\n",
    );

    for skill in skills {
        out.push_str(&format!(
            "- **{}**: {} \\\n  `{}`\n",
            skill.name,
            skill.description,
            skill.location.display(),
        ));
    }

    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn split_frontmatter_basic() {
        let content = "---\nname: foo\ndescription: bar\n---\n# Body\n";
        let (yaml, body) = split_frontmatter(content).unwrap();
        assert_eq!(yaml, "name: foo\ndescription: bar");
        assert_eq!(body, "# Body\n");
    }

    #[test]
    fn split_frontmatter_no_delimiters() {
        assert!(split_frontmatter("# Just markdown").is_none());
    }

    #[test]
    fn split_frontmatter_only_opening() {
        assert!(split_frontmatter("---\nname: foo\n").is_none());
    }

    #[test]
    fn validate_name_valid() {
        assert!(validate_name("pdf-processing").is_ok());
        assert!(validate_name("a").is_ok());
        assert!(validate_name("code-review-2").is_ok());
    }

    #[test]
    fn validate_name_invalid() {
        assert!(validate_name("").is_err());
        assert!(validate_name("PDF-Processing").is_err());
        assert!(validate_name("-pdf").is_err());
        assert!(validate_name("pdf-").is_err());
        assert!(validate_name("pdf--proc").is_err());
        assert!(validate_name("pdf proc").is_err());
        assert!(validate_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn parse_minimal_skill() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        fs::create_dir(&skill_dir).unwrap();

        let skill_md = skill_dir.join("SKILL.md");
        fs::write(
            &skill_md,
            "---\nname: my-skill\ndescription: Does things.\n---\n# Instructions\n",
        )
        .unwrap();

        let meta = parse_skill_md(&skill_md).unwrap();
        assert_eq!(meta.name, "my-skill");
        assert_eq!(meta.description, "Does things.");
        assert!(meta.license.is_none());
        assert!(meta.compatibility.is_none());
        assert!(meta.metadata.is_none());
        assert!(meta.allowed_tools.is_none());
    }

    #[test]
    fn parse_full_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("pdf-processing");
        fs::create_dir(&skill_dir).unwrap();

        let content = "\
---
name: pdf-processing
description: Extract PDF text, fill forms, merge files.
license: Apache-2.0
compatibility: Requires Python 3.14+ and uv
metadata:
  author: example-org
  version: \"1.0\"
allowed-tools: Bash(git:*) Read
---

# PDF Processing
";
        let skill_md = skill_dir.join("SKILL.md");
        fs::write(&skill_md, content).unwrap();

        let meta = parse_skill_md(&skill_md).unwrap();
        assert_eq!(meta.name, "pdf-processing");
        assert_eq!(meta.license.as_deref(), Some("Apache-2.0"));
        assert_eq!(
            meta.compatibility.as_deref(),
            Some("Requires Python 3.14+ and uv")
        );
        assert!(meta.metadata.is_some());
        assert_eq!(meta.allowed_tools.as_deref(), Some("Bash(git:*) Read"));
    }

    #[test]
    fn parse_missing_description_fails() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("bad-skill");
        fs::create_dir(&skill_dir).unwrap();

        let skill_md = skill_dir.join("SKILL.md");
        fs::write(&skill_md, "---\nname: bad-skill\n---\n# Stuff\n").unwrap();

        assert!(parse_skill_md(&skill_md).is_err());
    }

    #[test]
    fn discover_with_precedence() {
        let high = tempfile::tempdir().unwrap();
        let low = tempfile::tempdir().unwrap();

        // Same skill name in both dirs — high-precedence should win.
        for (dir, desc) in [(&high, "high-priority"), (&low, "low-priority")] {
            let skill_dir = dir.path().join("my-skill");
            fs::create_dir(&skill_dir).unwrap();
            fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: my-skill\ndescription: {desc}\n---\n"),
            )
            .unwrap();
        }

        // Also add a unique skill in the low-precedence dir.
        let other = low.path().join("other-skill");
        fs::create_dir(&other).unwrap();
        fs::write(
            other.join("SKILL.md"),
            "---\nname: other-skill\ndescription: Another one.\n---\n",
        )
        .unwrap();

        let skills = discover(&[high.path().to_path_buf(), low.path().to_path_buf()]);
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "my-skill");
        assert_eq!(skills[0].description, "high-priority");
        assert_eq!(skills[1].name, "other-skill");
    }

    #[test]
    fn discover_skips_nonexistent_dirs() {
        let skills = discover(&[PathBuf::from("/nonexistent/path/12345")]);
        assert!(skills.is_empty());
    }

    #[test]
    fn render_catalog_empty() {
        assert_eq!(render_catalog(&[]), "");
    }

    #[test]
    fn render_catalog_with_skills() {
        let skills = vec![SkillMeta {
            name: "pdf-processing".into(),
            description: "Extract PDF text.".into(),
            location: PathBuf::from("/home/user/.agents/skills/pdf-processing/SKILL.md"),
            license: None,
            compatibility: None,
            metadata: None,
            allowed_tools: None,
        }];

        let catalog = render_catalog(&skills);
        assert!(catalog.contains("pdf-processing"));
        assert!(catalog.contains("Extract PDF text."));
        assert!(catalog.contains("SKILL.md"));
        assert!(catalog.contains("file-read tool"));
    }
}
