/// Permission decision for a single (agent_id, tool_name) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    /// Prompt the user (local callers only; remote defaults to Deny).
    Ask,
}

impl std::fmt::Display for Decision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Decision::Allow => write!(f, "allow"),
            Decision::Deny => write!(f, "deny"),
            Decision::Ask => write!(f, "ask"),
        }
    }
}

/// In-memory permission record, mirroring the SQLite schema.
///
/// Schema:
/// ```sql
/// CREATE TABLE permissions (
///   agent_id   TEXT NOT NULL,
///   tool_name  TEXT NOT NULL,
///   decision   TEXT NOT NULL CHECK(decision IN ('allow','deny','ask')),
///   granted_at INTEGER,
///   PRIMARY KEY (agent_id, tool_name)
/// );
/// ```
///
/// Wildcards:
///   - `agent_id`  = "p2p:*"  matches all p2p callers
///   - `tool_name` = "mail.*" matches all mail tools
///   - `tool_name` = "*"      matches everything
///
/// Lookup waterfall (first match wins, specific → general):
///   1. (exact agent_id, exact tool_name)
///   2. (exact agent_id, prefix wildcard)
///   3. (exact agent_id, "*")
///   4. (wildcard agent_id, exact tool_name)
///   5. (wildcard agent_id, prefix wildcard)
///   6. (wildcard agent_id, "*")
///   7. not found → Local: Ask | P2P/Tailscale: Deny
#[derive(Debug, Clone)]
pub struct PermissionRecord {
    pub agent_id: String,
    pub tool_name: String,
    pub decision: Decision,
}

// Full permission store implementation (SQLite via sqlx) lives here in a
// future milestone. For now the structs establish the schema contract.
