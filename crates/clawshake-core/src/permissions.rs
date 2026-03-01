//! Permission store — SQLite-backed (agent_id, tool_name) → Decision.
//!
//! Waterfall lookup (first match wins, most-specific first):
//!   1. (exact agent,    exact tool)
//!   2. (exact agent,    *)
//!   3. (wildcard agent, exact tool)
//!   4. (wildcard agent, *)
//!   5. not found → Local: Ask | P2P/Tailscale: Deny

use std::path::Path;

use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool};
use tracing::{error, info};

use crate::identity::AgentId;

// ---------------------------------------------------------------------------
// Decision
// ---------------------------------------------------------------------------

/// Permission decision for a single (agent_id, tool_name) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    /// Prompt the user (local callers only; remote callers are auto-denied).
    Ask,
}

impl Decision {
    fn from_db(s: &str) -> Self {
        match s {
            "allow" => Decision::Allow,
            "deny" => Decision::Deny,
            "ask" => Decision::Ask,
            _ => Decision::Deny, // unknown value → deny (fail-closed)
        }
    }
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

// ---------------------------------------------------------------------------
// PermissionRecord (in-memory mirror of a DB row)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PermissionRecord {
    pub agent_id: String,
    pub tool_name: String,
    pub decision: Decision,
    pub granted_at: Option<i64>,
}

// ---------------------------------------------------------------------------
// PermissionStore
// ---------------------------------------------------------------------------

/// Async, cheaply-cloneable permission store backed by SQLite.
/// The internal pool is `Arc`-backed, so `Clone` is a refcount bump.
#[derive(Clone)]
pub struct PermissionStore {
    db: SqlitePool,
}

impl PermissionStore {
    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    /// Open (or create) the permissions database at `path` and run the schema
    /// migration.  Creates parent directories if needed.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        // SQLite URL — forward slashes required even on Windows.
        let url = format!(
            "sqlite://{}?mode=rwc",
            path.to_string_lossy().replace('\\', "/")
        );
        let db = SqlitePool::connect(&url)
            .await
            .with_context(|| format!("opening permissions DB at {}", path.display()))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS permissions (
                agent_id   TEXT NOT NULL,
                tool_name  TEXT NOT NULL,
                decision   TEXT NOT NULL CHECK(decision IN ('allow','deny','ask')),
                granted_at INTEGER,
                PRIMARY KEY (agent_id, tool_name)
            )",
        )
        .execute(&db)
        .await
        .context("creating permissions table")?;

        info!("Permissions DB open: {}", path.display());
        Ok(Self { db })
    }

    // -----------------------------------------------------------------------
    // Lookup
    // -----------------------------------------------------------------------

    /// Waterfall lookup — returns the first matching `Decision`, falling back
    /// to `Ask` (local) or `Deny` (P2P / Tailscale) when no row matches.
    pub async fn check(&self, agent_id: &AgentId, tool_name: &str) -> Decision {
        let exact_agent = agent_id.as_str();

        // Transport-class wildcard: "p2p:abc" → "p2p:*", "local" → "" (no wildcard).
        let wildcard_agent: &str = match agent_id {
            AgentId::P2p(_) => "p2p:*",
            AgentId::Tailscale(_) => "tailscale:*",
            AgentId::Local => "", // empty string won't match any real row
        };

        let result = sqlx::query(
            "SELECT decision FROM permissions
             WHERE agent_id IN (?1, ?2)
               AND tool_name IN (?3, '*')
             ORDER BY
               CASE agent_id   WHEN ?1 THEN 0 ELSE 1 END,
               CASE tool_name  WHEN ?3 THEN 0 ELSE 1 END
             LIMIT 1",
        )
        .bind(exact_agent.as_str())
        .bind(wildcard_agent)
        .bind(tool_name)
        .fetch_optional(&self.db)
        .await;

        match result {
            Ok(Some(row)) => {
                let s: &str = row.try_get("decision").unwrap_or("deny");
                Decision::from_db(s)
            }
            Ok(None) => match agent_id {
                AgentId::Local => Decision::Ask,
                AgentId::P2p(_) | AgentId::Tailscale(_) => Decision::Deny,
            },
            Err(e) => {
                error!("Permission check DB error: {e}");
                Decision::Deny // fail-closed on DB error
            }
        }
    }

    // -----------------------------------------------------------------------
    // Mutation
    // -----------------------------------------------------------------------

    /// Insert or replace a permission rule.
    pub async fn set(&self, agent_id: &str, tool_name: &str, decision: Decision) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO permissions (agent_id, tool_name, decision, granted_at)
             VALUES (?1, ?2, ?3, strftime('%s','now'))",
        )
        .bind(agent_id)
        .bind(tool_name)
        .bind(decision.to_string())
        .execute(&self.db)
        .await
        .context("inserting permission record")?;
        Ok(())
    }

    /// Remove a specific (agent_id, tool_name) rule.  No-op if the row does not exist.
    pub async fn remove(&self, agent_id: &str, tool_name: &str) -> Result<()> {
        sqlx::query("DELETE FROM permissions WHERE agent_id = ?1 AND tool_name = ?2")
            .bind(agent_id)
            .bind(tool_name)
            .execute(&self.db)
            .await
            .context("removing permission record")?;
        Ok(())
    }

    /// Return all rows in the permissions table, ordered by agent_id then tool_name.
    pub async fn list(&self) -> Result<Vec<PermissionRecord>> {
        let rows = sqlx::query(
            "SELECT agent_id, tool_name, decision, granted_at
             FROM permissions
             ORDER BY agent_id, tool_name",
        )
        .fetch_all(&self.db)
        .await
        .context("listing permission records")?;

        let records = rows
            .iter()
            .map(|row| PermissionRecord {
                agent_id: row.try_get("agent_id").unwrap_or_default(),
                tool_name: row.try_get("tool_name").unwrap_or_default(),
                decision: Decision::from_db(row.try_get("decision").unwrap_or("deny")),
                granted_at: row.try_get("granted_at").ok(),
            })
            .collect();
        Ok(records)
    }

    /// Seed a catch-all deny for all P2P callers (`p2p:*` / `*` / deny) if no
    /// such row exists yet.  Called on bridge startup so fresh installs are
    /// closed by default — users must explicitly grant access.
    pub async fn seed_p2p_deny_default(&self) -> Result<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO permissions (agent_id, tool_name, decision, granted_at)
             VALUES ('p2p:*', '*', 'deny', strftime('%s','now'))",
        )
        .execute(&self.db)
        .await
        .context("seeding p2p deny default")?;
        Ok(())
    }

    /// Check whether a tool should be published to the DHT.
    ///
    /// Runs the same specificity waterfall as `check()` for the representative
    /// agent `p2p:*` (the wildcard that covers all remote P2P peers):
    ///   1. `(p2p:*, exact tool)` — most specific
    ///   2. `(p2p:*, *)` — wildcard tool
    ///   3. No match → Deny (default for P2P)
    ///
    /// This means `deny p2p:* network_call` correctly overrides a blanket
    /// `allow p2p:* *` and the tool is not published to the DHT.
    pub async fn is_network_exposed(&self, tool_name: &str) -> bool {
        let result = sqlx::query(
            "SELECT decision FROM permissions
             WHERE agent_id = 'p2p:*'
               AND tool_name IN (?1, '*')
             ORDER BY CASE tool_name WHEN ?1 THEN 0 ELSE 1 END
             LIMIT 1",
        )
        .bind(tool_name)
        .fetch_optional(&self.db)
        .await;

        matches!(result, Ok(Some(row)) if row.try_get::<&str, _>("decision").unwrap_or("deny") == "allow")
    }
}
