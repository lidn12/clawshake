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
    /// Returns `true` if at least one P2P agent — either the class wildcard
    /// `p2p:*` or any specific peer `p2p:<id>` — has an effective `Allow`
    /// decision for `tool_name`.
    ///
    /// Specificity rule (per agent): an exact tool rule beats a wildcard `*`
    /// rule for the **same** agent.
    ///
    /// Examples:
    ///   • `allow p2p:* *`                        → exposed
    ///   • `deny  p2p:* *`                        → **not** exposed
    ///   • `deny  p2p:* *` + `allow p2p:<id> *`  → exposed (specific peer has access)
    ///   • `allow p2p:* *` + `deny p2p:* tool`   → **not** exposed (exact deny wins)
    ///   • `deny  p2p:* *` + `allow p2p:<id> *`
    ///     + `deny p2p:<id> tool`                → **not** exposed (exact deny overrides)
    pub async fn is_network_exposed(&self, tool_name: &str) -> bool {
        // A tool is exposed when ANY p2p agent (wildcard or specific) would
        // be allowed to call it under the specificity waterfall.
        //
        // Case 1 — explicit exact-tool allow for any p2p agent (wins over any * rule).
        // Case 2 — wildcard allow for any p2p agent, not overridden by an
        //          exact-tool deny for that same agent.
        let result = sqlx::query(
            "SELECT 1 FROM (
                SELECT 1 FROM permissions
                WHERE agent_id LIKE 'p2p:%'
                  AND tool_name = ?1
                  AND decision = 'allow'
                UNION ALL
                SELECT 1 FROM permissions pa
                WHERE pa.agent_id LIKE 'p2p:%'
                  AND pa.tool_name = '*'
                  AND pa.decision = 'allow'
                  AND NOT EXISTS (
                    SELECT 1 FROM permissions pd
                    WHERE pd.agent_id = pa.agent_id
                      AND pd.tool_name = ?1
                      AND pd.decision = 'deny'
                  )
             ) LIMIT 1",
        )
        .bind(tool_name)
        .fetch_optional(&self.db)
        .await;

        matches!(result, Ok(Some(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    async fn open_temp_store() -> (PermissionStore, NamedTempFile) {
        let file = NamedTempFile::new().expect("tempfile");
        let store = PermissionStore::open(file.path())
            .await
            .expect("open store");
        (store, file)
    }

    #[tokio::test]
    async fn waterfall_exact_agent_exact_tool() {
        let (store, _f) = open_temp_store().await;
        store
            .set("local", "read_file", Decision::Allow)
            .await
            .unwrap();
        assert_eq!(
            store.check(&AgentId::Local, "read_file").await,
            Decision::Allow
        );
    }

    #[tokio::test]
    async fn waterfall_exact_agent_wildcard_tool() {
        let (store, _f) = open_temp_store().await;
        store.set("local", "*", Decision::Allow).await.unwrap();
        assert_eq!(
            store.check(&AgentId::Local, "any_tool").await,
            Decision::Allow
        );
    }

    #[tokio::test]
    async fn waterfall_wildcard_agent_exact_tool() {
        let (store, _f) = open_temp_store().await;
        store
            .set("p2p:*", "read_file", Decision::Allow)
            .await
            .unwrap();
        assert_eq!(
            store
                .check(&AgentId::P2p("12D3KooWxxxx".into()), "read_file")
                .await,
            Decision::Allow
        );
    }

    #[tokio::test]
    async fn waterfall_wildcard_agent_wildcard_tool() {
        let (store, _f) = open_temp_store().await;
        store.set("p2p:*", "*", Decision::Deny).await.unwrap();
        assert_eq!(
            store
                .check(&AgentId::P2p("12D3KooWxxxx".into()), "any_tool")
                .await,
            Decision::Deny
        );
    }

    #[tokio::test]
    async fn waterfall_specificity_order() {
        let (store, _f) = open_temp_store().await;
        // Blanket allow for all p2p tools.
        store.set("p2p:*", "*", Decision::Allow).await.unwrap();
        // Specific deny for network_call overrides the blanket allow.
        store
            .set("p2p:*", "network_call", Decision::Deny)
            .await
            .unwrap();

        let peer = AgentId::P2p("12D3KooWxxxx".into());
        assert_eq!(
            store.check(&peer, "network_call").await,
            Decision::Deny,
            "exact tool rule must beat wildcard tool rule"
        );
        assert_eq!(
            store.check(&peer, "read_file").await,
            Decision::Allow,
            "wildcard still applies for tools without a specific rule"
        );
    }

    #[tokio::test]
    async fn waterfall_exact_agent_beats_wildcard_agent() {
        // Tests the agent-level dimension of the waterfall:
        // a rule for the exact peer ID must beat a rule for "p2p:*".
        // If the ORDER BY agent priority were wrong, the wildcard deny would win.
        let (store, _f) = open_temp_store().await;
        let peer_id = "12D3KooWspecific";
        let exact_agent = format!("p2p:{peer_id}");

        // Blanket deny for all p2p agents.
        store.set("p2p:*", "*", Decision::Deny).await.unwrap();
        // Specific allow for just this one peer.
        store.set(&exact_agent, "*", Decision::Allow).await.unwrap();

        assert_eq!(
            store.check(&AgentId::P2p(peer_id.into()), "any_tool").await,
            Decision::Allow,
            "exact agent rule must beat wildcard agent rule"
        );
        // A different peer still gets the wildcard deny.
        assert_eq!(
            store
                .check(&AgentId::P2p("12D3KooWother".into()), "any_tool")
                .await,
            Decision::Deny,
            "wildcard deny must still apply to other peers"
        );
    }

    #[tokio::test]
    async fn waterfall_no_match_local_defaults_to_ask() {
        let (store, _f) = open_temp_store().await;
        assert_eq!(store.check(&AgentId::Local, "foo").await, Decision::Ask);
    }

    #[tokio::test]
    async fn waterfall_no_match_p2p_defaults_to_deny() {
        let (store, _f) = open_temp_store().await;
        assert_eq!(
            store.check(&AgentId::P2p("x".into()), "foo").await,
            Decision::Deny
        );
    }

    #[tokio::test]
    async fn waterfall_no_match_tailscale_defaults_to_deny() {
        let (store, _f) = open_temp_store().await;
        assert_eq!(
            store.check(&AgentId::Tailscale("node".into()), "foo").await,
            Decision::Deny
        );
    }

    #[tokio::test]
    async fn unknown_decision_string_defaults_to_deny() {
        let file = NamedTempFile::new().expect("tempfile");
        // Build a table without the CHECK constraint so we can insert an
        // invalid decision value, then verify the store treats it as Deny.
        let url = format!(
            "sqlite://{}?mode=rwc",
            file.path().to_string_lossy().replace('\\', "/")
        );
        let raw = SqlitePool::connect(&url).await.unwrap();
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS permissions (
                agent_id   TEXT NOT NULL,
                tool_name  TEXT NOT NULL,
                decision   TEXT NOT NULL,
                granted_at INTEGER,
                PRIMARY KEY (agent_id, tool_name)
            )",
        )
        .execute(&raw)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO permissions (agent_id, tool_name, decision) \
             VALUES ('local', 'badtool', 'invalid')",
        )
        .execute(&raw)
        .await
        .unwrap();
        raw.close().await;

        // Now open via PermissionStore — the table already exists, so CREATE
        // TABLE IF NOT EXISTS is a no-op and the bad row survives.
        let store = PermissionStore::open(file.path()).await.unwrap();
        assert_eq!(
            store.check(&AgentId::Local, "badtool").await,
            Decision::Deny,
            "unrecognised decision strings must fail-closed to Deny"
        );
    }

    #[tokio::test]
    async fn set_and_remove() {
        let (store, _f) = open_temp_store().await;
        store.set("local", "foo", Decision::Allow).await.unwrap();
        assert_eq!(store.check(&AgentId::Local, "foo").await, Decision::Allow);
        store.remove("local", "foo").await.unwrap();
        // Local default is Ask when no rule exists.
        assert_eq!(store.check(&AgentId::Local, "foo").await, Decision::Ask);
    }

    #[tokio::test]
    async fn remove_p2p_rule_falls_back_to_deny() {
        // P2P and Local have *different* fallback defaults (Deny vs Ask).
        // Removing the only P2P rule must reveal Deny, not Ask.
        let (store, _f) = open_temp_store().await;
        let peer = AgentId::P2p("somepeer".into());
        store.set("p2p:*", "foo", Decision::Allow).await.unwrap();
        assert_eq!(store.check(&peer, "foo").await, Decision::Allow);
        store.remove("p2p:*", "foo").await.unwrap();
        assert_eq!(
            store.check(&peer, "foo").await,
            Decision::Deny,
            "P2P default after removal must be Deny, not Ask"
        );
    }

    #[tokio::test]
    async fn set_replaces_existing() {
        let (store, _f) = open_temp_store().await;
        store.set("local", "foo", Decision::Allow).await.unwrap();
        store.set("local", "foo", Decision::Deny).await.unwrap();
        assert_eq!(store.check(&AgentId::Local, "foo").await, Decision::Deny);
    }

    #[tokio::test]
    async fn list_returns_all_rows() {
        let (store, _f) = open_temp_store().await;
        store.set("local", "alpha", Decision::Allow).await.unwrap();
        store.set("local", "beta", Decision::Deny).await.unwrap();
        store.set("p2p:*", "*", Decision::Deny).await.unwrap();
        let rows = store.list().await.unwrap();
        assert_eq!(rows.len(), 3);
        // ORDER BY agent_id, tool_name: "local" < "p2p:*", and within local
        // "alpha" < "beta".  If the ORDER BY were dropped this would fail.
        assert_eq!(rows[0].agent_id, "local");
        assert_eq!(rows[0].tool_name, "alpha");
        assert_eq!(rows[1].agent_id, "local");
        assert_eq!(rows[1].tool_name, "beta");
        assert_eq!(rows[2].agent_id, "p2p:*");
        assert_eq!(rows[2].tool_name, "*");
    }

    #[tokio::test]
    async fn seed_p2p_deny_default() {
        let (store, _f) = open_temp_store().await;
        store.seed_p2p_deny_default().await.unwrap();
        assert_eq!(
            store
                .check(&AgentId::P2p("anypeer".into()), "anything")
                .await,
            Decision::Deny
        );
        // Second call must not error (INSERT OR IGNORE).
        store.seed_p2p_deny_default().await.unwrap();
    }

    #[tokio::test]
    async fn is_network_exposed_empty_db_returns_false() {
        // Immediately after install there are no rules at all.  The function
        // must return false (not exposed) rather than panicking or returning
        // true by accident.
        let (store, _f) = open_temp_store().await;
        assert!(
            !store.is_network_exposed("read_file").await,
            "empty permission DB must not expose any tool to the network"
        );
    }

    #[tokio::test]
    async fn is_network_exposed_reflects_p2p_wildcard_rules() {
        let (store, _f) = open_temp_store().await;
        // Blanket allow → read_file is exposed.
        store.set("p2p:*", "*", Decision::Allow).await.unwrap();
        assert!(store.is_network_exposed("read_file").await);

        // Specific deny for network_call → not exposed.
        store
            .set("p2p:*", "network_call", Decision::Deny)
            .await
            .unwrap();
        assert!(!store.is_network_exposed("network_call").await);

        // read_file still exposed via wildcard.
        assert!(store.is_network_exposed("read_file").await);
    }

    #[tokio::test]
    async fn is_network_exposed_specific_peer_wildcard_allow() {
        let (store, _f) = open_temp_store().await;
        // Default deny for all peers, but a specific peer has blanket access.
        store.set("p2p:*", "*", Decision::Deny).await.unwrap();
        store
            .set("p2p:12D3KooWxxxx", "*", Decision::Allow)
            .await
            .unwrap();
        // Tool should be exposed because at least one peer can call it.
        assert!(store.is_network_exposed("read_file").await);
        assert!(store.is_network_exposed("write_file").await);
    }

    #[tokio::test]
    async fn is_network_exposed_specific_peer_exact_allow() {
        let (store, _f) = open_temp_store().await;
        // Default deny for all peers, but a specific peer has an exact allow.
        store.set("p2p:*", "*", Decision::Deny).await.unwrap();
        store
            .set("p2p:12D3KooWxxxx", "read_file", Decision::Allow)
            .await
            .unwrap();
        assert!(store.is_network_exposed("read_file").await);
        // Other tools are still not exposed.
        assert!(!store.is_network_exposed("write_file").await);
    }

    #[tokio::test]
    async fn is_network_exposed_specific_peer_exact_deny_overrides_wildcard() {
        let (store, _f) = open_temp_store().await;
        // Specific peer has blanket allow but an exact deny for one tool.
        store.set("p2p:*", "*", Decision::Deny).await.unwrap();
        store
            .set("p2p:12D3KooWxxxx", "*", Decision::Allow)
            .await
            .unwrap();
        store
            .set("p2p:12D3KooWxxxx", "network_call", Decision::Deny)
            .await
            .unwrap();
        // network_call is not callable by any peer → not exposed.
        assert!(!store.is_network_exposed("network_call").await);
        // read_file is still allowed via the specific peer's wildcard.
        assert!(store.is_network_exposed("read_file").await);
    }

    #[test]
    fn decision_display_roundtrip() {
        assert_eq!(Decision::Allow.to_string(), "allow");
        assert_eq!(Decision::Deny.to_string(), "deny");
        assert_eq!(Decision::Ask.to_string(), "ask");

        assert_eq!(Decision::from_db("allow"), Decision::Allow);
        assert_eq!(Decision::from_db("deny"), Decision::Deny);
        assert_eq!(Decision::from_db("ask"), Decision::Ask);
        // Unknown value falls back to Deny.
        assert_eq!(Decision::from_db("unknown"), Decision::Deny);
    }
}
