//! Permission store — SQLite-backed (agent_id, resource) → Decision.
//!
//! Waterfall lookup (first match wins, most-specific first):
//!   1. (exact agent,    exact resource)
//!   2. (exact agent,    *)
//!   3. (wildcard agent, exact resource)
//!   4. (wildcard agent, *)
//!   5. not found → Local: Ask | P2P/Tailscale: Deny
//!
//! Resources can be tool names (e.g. `read_file`) or tunnel names
//! (e.g. `tunnel:models`).  The wildcard `*` matches all resources.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use tracing::{error, info};

use crate::identity::AgentId;

// ---------------------------------------------------------------------------
// Decision
// ---------------------------------------------------------------------------

/// Permission decision for a single (agent_id, resource) pair.
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
    pub resource: String,
    pub decision: Decision,
    pub granted_at: Option<i64>,
}

// ---------------------------------------------------------------------------
// PermissionStore
// ---------------------------------------------------------------------------

/// Async, cheaply-cloneable permission store backed by SQLite.
/// The internal connection is `Arc<Mutex<…>>`-backed, so `Clone` is a refcount bump.
#[derive(Clone)]
pub struct PermissionStore {
    db: Arc<Mutex<Connection>>,
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

        let path = path.to_path_buf();
        let path2 = path.clone();
        let db = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let conn = Connection::open(&path2)
                .with_context(|| format!("opening permissions DB at {}", path2.display()))?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS permissions (
                    agent_id   TEXT NOT NULL,
                    resource   TEXT NOT NULL,
                    decision   TEXT NOT NULL CHECK(decision IN ('allow','deny','ask')),
                    granted_at INTEGER,
                    PRIMARY KEY (agent_id, resource)
                )",
            )
            .context("creating permissions table")?;
            // Migration: rename legacy `tool_name` column to `resource`.
            // SQLite doesn't support ALTER COLUMN RENAME before 3.25, so we
            // recreate the table when the old schema is detected.
            let has_old_column: bool = conn
                .prepare("SELECT tool_name FROM permissions LIMIT 0")
                .is_ok();
            if has_old_column {
                conn.execute_batch(
                    "ALTER TABLE permissions RENAME TO _permissions_old;
                     CREATE TABLE permissions (
                         agent_id   TEXT NOT NULL,
                         resource   TEXT NOT NULL,
                         decision   TEXT NOT NULL CHECK(decision IN ('allow','deny','ask')),
                         granted_at INTEGER,
                         PRIMARY KEY (agent_id, resource)
                     );
                     INSERT INTO permissions (agent_id, resource, decision, granted_at)
                         SELECT agent_id, tool_name, decision, granted_at
                         FROM _permissions_old;
                     DROP TABLE _permissions_old;",
                )
                .context("migrating tool_name → resource column")?;
            }
            Ok(conn)
        })
        .await
        .context("spawn_blocking join")??;

        info!("Permissions DB open: {}", path.display());
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
        })
    }

    // -----------------------------------------------------------------------
    // Lookup
    // -----------------------------------------------------------------------

    /// Waterfall lookup — returns the first matching `Decision`, falling back
    /// to `Ask` (local) or `Deny` (P2P / Tailscale) when no row matches.
    pub async fn check(&self, agent_id: &AgentId, resource: &str) -> Decision {
        let exact_agent = agent_id.as_str().to_string();

        // Transport-class wildcard: "p2p:abc" → "p2p:*", "local" → "" (no wildcard).
        let wildcard_agent: String = match agent_id {
            AgentId::P2p(_) => "p2p:*".into(),
            AgentId::Tailscale(_) => "tailscale:*".into(),
            AgentId::Local => String::new(), // empty string won't match any real row
        };

        let is_local = matches!(agent_id, AgentId::Local);
        let resource = resource.to_string();
        let db = self.db.clone();

        let result = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
            let conn = db.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT decision FROM permissions
                 WHERE agent_id IN (?1, ?2)
                   AND resource IN (?3, '*')
                 ORDER BY
                   CASE agent_id   WHEN ?1 THEN 0 ELSE 1 END,
                   CASE resource   WHEN ?3 THEN 0 ELSE 1 END
                 LIMIT 1",
            )?;
            let decision = stmt
                .query_row(params![exact_agent, wildcard_agent, resource], |row| {
                    row.get::<_, String>(0)
                })
                .ok();
            Ok(decision)
        })
        .await;

        match result {
            Ok(Ok(Some(s))) => Decision::from_db(&s),
            Ok(Ok(None)) => {
                if is_local {
                    Decision::Ask
                } else {
                    Decision::Deny
                }
            }
            Ok(Err(e)) => {
                error!("Permission check DB error: {e}");
                Decision::Deny // fail-closed on DB error
            }
            Err(e) => {
                error!("Permission check join error: {e}");
                Decision::Deny
            }
        }
    }

    // -----------------------------------------------------------------------
    // Mutation
    // -----------------------------------------------------------------------

    /// Insert or replace a permission rule.
    pub async fn set(&self, agent_id: &str, resource: &str, decision: Decision) -> Result<()> {
        let agent_id = agent_id.to_string();
        let resource = resource.to_string();
        let decision_str = decision.to_string();
        let db = self.db.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO permissions (agent_id, resource, decision, granted_at)
                 VALUES (?1, ?2, ?3, strftime('%s','now'))",
                params![agent_id, resource, decision_str],
            )
            .context("inserting permission record")?;
            Ok(())
        })
        .await
        .context("spawn_blocking join")?
    }

    /// Remove a specific (agent_id, resource) rule.  No-op if the row does not exist.
    pub async fn remove(&self, agent_id: &str, resource: &str) -> Result<()> {
        let agent_id = agent_id.to_string();
        let resource = resource.to_string();
        let db = self.db.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = db.lock().unwrap();
            conn.execute(
                "DELETE FROM permissions WHERE agent_id = ?1 AND resource = ?2",
                params![agent_id, resource],
            )
            .context("removing permission record")?;
            Ok(())
        })
        .await
        .context("spawn_blocking join")?
    }

    /// Return all rows in the permissions table, ordered by agent_id then resource.
    pub async fn list(&self) -> Result<Vec<PermissionRecord>> {
        let db = self.db.clone();

        tokio::task::spawn_blocking(move || -> Result<Vec<PermissionRecord>> {
            let conn = db.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT agent_id, resource, decision, granted_at
                 FROM permissions
                 ORDER BY agent_id, resource",
            )?;
            let records = stmt
                .query_map([], |row| {
                    Ok(PermissionRecord {
                        agent_id: row.get(0)?,
                        resource: row.get(1)?,
                        decision: Decision::from_db(&row.get::<_, String>(2)?),
                        granted_at: row.get(3)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(records)
        })
        .await
        .context("spawn_blocking join")?
    }

    /// Seed a catch-all allow for the local agent (`local` / `*` / allow) if
    /// no `local`+`*` row exists yet.  Called on bridge startup so the default
    /// is open for local tools — the user can restrict individual tools later.
    pub async fn seed_local_allow_default(&self) -> Result<()> {
        let db = self.db.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO permissions (agent_id, resource, decision, granted_at)
                 VALUES ('local', '*', 'allow', strftime('%s','now'))",
                [],
            )
            .context("seeding local allow default")?;
            Ok(())
        })
        .await
        .context("spawn_blocking join")?
    }

    /// Seed a catch-all deny for all P2P callers (`p2p:*` / `*` / deny) if no
    /// such row exists yet.  Called on bridge startup so fresh installs are
    /// closed by default — users must explicitly grant access.
    pub async fn seed_p2p_deny_default(&self) -> Result<()> {
        let db = self.db.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO permissions (agent_id, resource, decision, granted_at)
                 VALUES ('p2p:*', '*', 'deny', strftime('%s','now'))",
                [],
            )
            .context("seeding p2p deny default")?;
            Ok(())
        })
        .await
        .context("spawn_blocking join")?
    }

    /// Count distinct P2P peers that have at least one `allow` rule.
    ///
    /// Counts individual `p2p:<id>` agent_ids only — the class wildcard
    /// `p2p:*` is excluded because it doesn't represent a specific peer.
    pub async fn count_allowed_peers(&self) -> usize {
        let db = self.db.clone();

        let result = tokio::task::spawn_blocking(move || -> Result<usize> {
            let conn = db.lock().unwrap();
            let count: i64 = conn.query_row(
                "SELECT COUNT(DISTINCT agent_id) FROM permissions
                 WHERE agent_id LIKE 'p2p:%'
                   AND agent_id != 'p2p:*'
                   AND decision = 'allow'",
                [],
                |row| row.get(0),
            )?;
            Ok(count as usize)
        })
        .await;

        match result {
            Ok(Ok(n)) => n,
            _ => 0,
        }
    }

    /// Check whether a resource should be published to the DHT.
    ///
    /// Returns `true` if at least one P2P agent — either the class wildcard
    /// `p2p:*` or any specific peer `p2p:<id>` — has an effective `Allow`
    /// decision for `resource`.
    ///
    /// Specificity rule (per agent): an exact resource rule beats a wildcard `*`
    /// rule for the **same** agent.
    ///
    /// Examples:
    ///   • `allow p2p:* *`                        → exposed
    ///   • `deny  p2p:* *`                        → **not** exposed
    ///   • `deny  p2p:* *` + `allow p2p:<id> *`  → exposed (specific peer has access)
    ///   • `allow p2p:* *` + `deny p2p:* tool`   → **not** exposed (exact deny wins)
    ///   • `deny  p2p:* *` + `allow p2p:<id> *`
    ///     + `deny p2p:<id> tool`                → **not** exposed (exact deny overrides)
    pub async fn is_network_exposed(&self, resource: &str) -> bool {
        let resource = resource.to_string();
        let db = self.db.clone();

        let result = tokio::task::spawn_blocking(move || -> Result<bool> {
            let conn = db.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT 1 FROM (
                    SELECT 1 FROM permissions
                    WHERE agent_id LIKE 'p2p:%'
                      AND resource = ?1
                      AND decision = 'allow'
                    UNION ALL
                    SELECT 1 FROM permissions pa
                    WHERE pa.agent_id LIKE 'p2p:%'
                      AND pa.resource = '*'
                      AND pa.decision = 'allow'
                      AND NOT EXISTS (
                        SELECT 1 FROM permissions pd
                        WHERE pd.agent_id = pa.agent_id
                          AND pd.resource = ?1
                          AND pd.decision = 'deny'
                      )
                 ) LIMIT 1",
            )?;
            let found = stmt.query_row(params![resource], |_| Ok(())).is_ok();
            Ok(found)
        })
        .await;

        matches!(result, Ok(Ok(true)))
    }

    /// Remove redundant rules whose removal does not change any query result.
    ///
    /// Uses an **ordered sweep** (standard firewall-rule optimization): rules
    /// are processed from most-specific (Level 1) to broadest (Level 4),
    /// maintaining a live lookup that reflects removals as they happen.
    /// This correctly handles **shielded redundancy** — a Level 1 rule that
    /// survives the sweep is a genuine shield, safe to rely on when analysing
    /// Level 2.
    ///
    /// Waterfall levels (same as `check()`):
    ///   1. (exact agent, exact resource)
    ///   2. (exact agent, `*`)
    ///   3. (class wildcard, exact resource)
    ///   4. (class wildcard, `*`)          — never redundant
    ///
    /// Complexity: O(R + |L2|×|L3|) where L2 = per-agent wildcard-resource rules
    /// and L3 = class-level resource-specific rules.  Worst case O(R²), but
    /// real rule sets are small.  Single DB read, one batched delete.
    pub async fn consolidate(&self) -> Result<usize> {
        use std::collections::HashMap;

        let rules = self.list().await?;
        if rules.is_empty() {
            return Ok(0);
        }

        // Live lookup — entries are removed as rules are found redundant.
        let mut live: HashMap<(&str, &str), &Decision> = rules
            .iter()
            .map(|r| ((r.agent_id.as_str(), r.resource.as_str()), &r.decision))
            .collect();

        // Partition rule indices by waterfall level.
        let (mut l1, mut l2, mut l3) = (Vec::new(), Vec::new(), Vec::new());
        for (i, r) in rules.iter().enumerate() {
            let a = r.agent_id.as_str();
            let t = r.resource.as_str();
            match (t, class_wildcard(a)) {
                (t, Some(_)) if t != "*" => l1.push(i),
                ("*", Some(_)) => l2.push(i),
                (t, None) if t != "*" && (a.ends_with(":*") || a == "local") => l3.push(i),
                _ => {} // L4 — never redundant
            }
        }

        let mut to_remove: Vec<(&str, &str)> = Vec::new();

        // --- Level 1: (exact agent, exact resource) ---
        // Redundant if the first broader match in the live lookup has the
        // same decision.  Removals here update the live set BEFORE Level 2
        // is analysed, so L2's shield check sees only genuine survivors.
        for &i in &l1 {
            let (a, t) = (rules[i].agent_id.as_str(), rules[i].resource.as_str());
            let d = &rules[i].decision;
            let wa = class_wildcard(a).unwrap();

            let next = live
                .get(&(a, "*"))
                .or_else(|| live.get(&(wa, t)))
                .or_else(|| live.get(&(wa, "*")))
                .copied();

            if next == Some(d) {
                live.remove(&(a, t));
                to_remove.push((a, t));
            }
        }

        // --- Level 2: (exact agent, wildcard resource) ---
        // Removing L2 exposes queries to L3/L4.  Redundant iff:
        //  1. L4 (class, *) exists with the same decision, AND
        //  2. every L3 rule for resources not shielded by a surviving L1 also
        //     carries the same decision.
        for &i in &l2 {
            let a = rules[i].agent_id.as_str();
            let d = &rules[i].decision;
            let wa = class_wildcard(a).unwrap();

            let dominated = live.get(&(wa, "*")).copied().map_or(false, |l4_d| {
                l4_d == d
                    && l3.iter().all(|&j| {
                        let r3 = &rules[j];
                        // Different class — irrelevant.
                        if r3.agent_id.as_str() != wa {
                            return true;
                        }
                        // A surviving L1 shields this resource for this agent.
                        if live.contains_key(&(a, r3.resource.as_str())) {
                            return true;
                        }
                        // No shield — L3 must agree.
                        &r3.decision == d
                    })
            });

            if dominated {
                live.remove(&(a, "*"));
                to_remove.push((a, "*"));
            }
        }

        // --- Level 3: (class/local, exact resource) ---
        // Redundant if L4 (same agent, *) carries the same decision.
        for &i in &l3 {
            let (a, t) = (rules[i].agent_id.as_str(), rules[i].resource.as_str());
            let d = &rules[i].decision;

            if live.get(&(a, "*")).copied() == Some(d) {
                live.remove(&(a, t));
                to_remove.push((a, t));
            }
        }

        // L4 is never redundant — nothing broader exists.

        if to_remove.is_empty() {
            return Ok(0);
        }

        let count = to_remove.len();
        for &(a, t) in &to_remove {
            self.remove(a, t).await?;
        }

        Ok(count)
    }
}

// ---------------------------------------------------------------------------
// Consolidation helpers (module-private)
// ---------------------------------------------------------------------------

/// Return the class wildcard for a specific agent, e.g. `"p2p:abc"` → `"p2p:*"`.
fn class_wildcard(agent: &str) -> Option<&'static str> {
    if agent.starts_with("p2p:") && agent != "p2p:*" {
        Some("p2p:*")
    } else if agent.starts_with("tailscale:") && agent != "tailscale:*" {
        Some("tailscale:*")
    } else {
        None
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
        {
            let raw = Connection::open(file.path()).unwrap();
            raw.execute_batch(
                "CREATE TABLE IF NOT EXISTS permissions (
                    agent_id   TEXT NOT NULL,
                    resource   TEXT NOT NULL,
                    decision   TEXT NOT NULL,
                    granted_at INTEGER,
                    PRIMARY KEY (agent_id, resource)
                )",
            )
            .unwrap();
            raw.execute(
                "INSERT INTO permissions (agent_id, resource, decision) \
                 VALUES ('local', 'badtool', 'invalid')",
                [],
            )
            .unwrap();
        }

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
        assert_eq!(rows[0].agent_id, "local");
        assert_eq!(rows[0].resource, "alpha");
        assert_eq!(rows[1].agent_id, "local");
        assert_eq!(rows[1].resource, "beta");
        assert_eq!(rows[2].agent_id, "p2p:*");
        assert_eq!(rows[2].resource, "*");
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
        let (store, _f) = open_temp_store().await;
        assert!(
            !store.is_network_exposed("read_file").await,
            "empty permission DB must not expose any tool to the network"
        );
    }

    #[tokio::test]
    async fn is_network_exposed_reflects_p2p_wildcard_rules() {
        let (store, _f) = open_temp_store().await;
        store.set("p2p:*", "*", Decision::Allow).await.unwrap();
        assert!(store.is_network_exposed("read_file").await);

        store
            .set("p2p:*", "network_call", Decision::Deny)
            .await
            .unwrap();
        assert!(!store.is_network_exposed("network_call").await);
        assert!(store.is_network_exposed("read_file").await);
    }

    #[tokio::test]
    async fn is_network_exposed_specific_peer_wildcard_allow() {
        let (store, _f) = open_temp_store().await;
        store.set("p2p:*", "*", Decision::Deny).await.unwrap();
        store
            .set("p2p:12D3KooWxxxx", "*", Decision::Allow)
            .await
            .unwrap();
        assert!(store.is_network_exposed("read_file").await);
        assert!(store.is_network_exposed("write_file").await);
    }

    #[tokio::test]
    async fn is_network_exposed_specific_peer_exact_allow() {
        let (store, _f) = open_temp_store().await;
        store.set("p2p:*", "*", Decision::Deny).await.unwrap();
        store
            .set("p2p:12D3KooWxxxx", "read_file", Decision::Allow)
            .await
            .unwrap();
        assert!(store.is_network_exposed("read_file").await);
        assert!(!store.is_network_exposed("write_file").await);
    }

    #[tokio::test]
    async fn is_network_exposed_specific_peer_exact_deny_overrides_wildcard() {
        let (store, _f) = open_temp_store().await;
        store.set("p2p:*", "*", Decision::Deny).await.unwrap();
        store
            .set("p2p:12D3KooWxxxx", "*", Decision::Allow)
            .await
            .unwrap();
        store
            .set("p2p:12D3KooWxxxx", "network_call", Decision::Deny)
            .await
            .unwrap();
        assert!(!store.is_network_exposed("network_call").await);
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
        assert_eq!(Decision::from_db("unknown"), Decision::Deny);
    }

    #[tokio::test]
    async fn count_allowed_peers_excludes_wildcards_and_local() {
        let (store, _f) = open_temp_store().await;
        // p2p wildcard and local should not be counted.
        store.set("p2p:*", "*", Decision::Allow).await.unwrap();
        store.set("local", "foo", Decision::Allow).await.unwrap();
        assert_eq!(store.count_allowed_peers().await, 0);

        // Specific peers with allow rules should be counted.
        store
            .set("p2p:12D3KooWaaaa", "read_file", Decision::Allow)
            .await
            .unwrap();
        store
            .set("p2p:12D3KooWbbbb", "*", Decision::Allow)
            .await
            .unwrap();
        assert_eq!(store.count_allowed_peers().await, 2);

        // A peer with only deny rules should not be counted.
        store
            .set("p2p:12D3KooWcccc", "read_file", Decision::Deny)
            .await
            .unwrap();
        assert_eq!(store.count_allowed_peers().await, 2);

        // Multiple allow rules for the same peer count as one.
        store
            .set("p2p:12D3KooWaaaa", "write_file", Decision::Allow)
            .await
            .unwrap();
        assert_eq!(store.count_allowed_peers().await, 2);
    }

    // -----------------------------------------------------------------------
    // Consolidation tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn consolidate_empty_store() {
        let (store, _f) = open_temp_store().await;
        assert_eq!(store.consolidate().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn consolidate_no_redundancy() {
        let (store, _f) = open_temp_store().await;
        store.set("p2p:*", "*", Decision::Deny).await.unwrap();
        store
            .set("p2p:peer1", "read_file", Decision::Allow)
            .await
            .unwrap();
        assert_eq!(store.consolidate().await.unwrap(), 0);
        assert_eq!(store.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn consolidate_specific_shadowed_by_agent_wildcard_tool() {
        let (store, _f) = open_temp_store().await;
        store.set("p2p:peer1", "*", Decision::Allow).await.unwrap();
        store
            .set("p2p:peer1", "read_file", Decision::Allow)
            .await
            .unwrap();
        assert_eq!(store.consolidate().await.unwrap(), 1);
        let rules = store.list().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].resource, "*");
    }

    #[tokio::test]
    async fn consolidate_specific_shadowed_by_class_wildcard() {
        let (store, _f) = open_temp_store().await;
        store.set("p2p:*", "*", Decision::Deny).await.unwrap();
        store
            .set("p2p:peer1", "read_file", Decision::Deny)
            .await
            .unwrap();
        assert_eq!(store.consolidate().await.unwrap(), 1);
        let rules = store.list().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].agent_id, "p2p:*");
    }

    #[tokio::test]
    async fn consolidate_keeps_different_decision() {
        let (store, _f) = open_temp_store().await;
        store.set("p2p:*", "*", Decision::Deny).await.unwrap();
        store
            .set("p2p:peer1", "read_file", Decision::Allow)
            .await
            .unwrap();
        assert_eq!(store.consolidate().await.unwrap(), 0);
        assert_eq!(store.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn consolidate_preserves_system_default_baseline() {
        let (store, _f) = open_temp_store().await;
        // deny p2p:* * matches the system default for P2P, but since
        // there's no other explicit rule covering it, it should be kept.
        store.set("p2p:*", "*", Decision::Deny).await.unwrap();
        assert_eq!(store.consolidate().await.unwrap(), 0);
        assert_eq!(store.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn consolidate_cascading_removal() {
        let (store, _f) = open_temp_store().await;
        // deny p2p:* * → deny p2p:* read_file → deny p2p:peer1 read_file
        // The most-specific is shadowed by the mid-level, which is shadowed
        // by the broadest.  Both should be removed in cascading passes.
        store.set("p2p:*", "*", Decision::Deny).await.unwrap();
        store
            .set("p2p:*", "read_file", Decision::Deny)
            .await
            .unwrap();
        store
            .set("p2p:peer1", "read_file", Decision::Deny)
            .await
            .unwrap();
        assert_eq!(store.consolidate().await.unwrap(), 2);
        let rules = store.list().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].agent_id, "p2p:*");
        assert_eq!(rules[0].resource, "*");
    }

    #[tokio::test]
    async fn consolidate_keeps_peer_wildcard_overriding_class_exception() {
        let (store, _f) = open_temp_store().await;
        // allow p2p:* *
        // deny  p2p:* network_call
        // allow p2p:peer1 *          ← NOT redundant! Needed to override
        //                               the deny on network_call for peer1.
        store.set("p2p:*", "*", Decision::Allow).await.unwrap();
        store
            .set("p2p:*", "network_call", Decision::Deny)
            .await
            .unwrap();
        store.set("p2p:peer1", "*", Decision::Allow).await.unwrap();
        assert_eq!(store.consolidate().await.unwrap(), 0);
        assert_eq!(store.list().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn consolidate_multiple_peers_mixed() {
        let (store, _f) = open_temp_store().await;
        store.set("p2p:*", "*", Decision::Allow).await.unwrap();
        // These are all shadowed by the class wildcard.
        store
            .set("p2p:peer1", "read_file", Decision::Allow)
            .await
            .unwrap();
        store
            .set("p2p:peer2", "write_file", Decision::Allow)
            .await
            .unwrap();
        store.set("p2p:peer2", "*", Decision::Allow).await.unwrap();
        // This one is NOT shadowed — different decision.
        store
            .set("p2p:peer3", "shell", Decision::Deny)
            .await
            .unwrap();
        assert_eq!(store.consolidate().await.unwrap(), 3);
        let rules = store.list().await.unwrap();
        assert_eq!(rules.len(), 2);
        let names: Vec<&str> = rules.iter().map(|r| r.agent_id.as_str()).collect();
        assert!(names.contains(&"p2p:*"));
        assert!(names.contains(&"p2p:peer3"));
    }

    #[tokio::test]
    async fn consolidate_tailscale_class() {
        let (store, _f) = open_temp_store().await;
        // Tailscale wildcard covers the specific rule.
        store
            .set("tailscale:*", "*", Decision::Allow)
            .await
            .unwrap();
        store
            .set("tailscale:node1", "read_file", Decision::Allow)
            .await
            .unwrap();
        assert_eq!(store.consolidate().await.unwrap(), 1);
        let rules = store.list().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].agent_id, "tailscale:*");
    }

    #[tokio::test]
    async fn consolidate_local_agent() {
        let (store, _f) = open_temp_store().await;
        // (local, read_file, allow) is redundant when (local, *, allow) exists.
        store.set("local", "*", Decision::Allow).await.unwrap();
        store
            .set("local", "read_file", Decision::Allow)
            .await
            .unwrap();
        assert_eq!(store.consolidate().await.unwrap(), 1);
        let rules = store.list().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].agent_id, "local");
        assert_eq!(rules[0].resource, "*");
    }

    #[tokio::test]
    async fn consolidate_local_keeps_different_decision() {
        let (store, _f) = open_temp_store().await;
        store.set("local", "*", Decision::Allow).await.unwrap();
        store.set("local", "shell", Decision::Ask).await.unwrap();
        assert_eq!(store.consolidate().await.unwrap(), 0);
        assert_eq!(store.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn consolidate_cross_class_isolation() {
        let (store, _f) = open_temp_store().await;
        // p2p deny-all, tailscale allow-all.  Neither should affect the other.
        store.set("p2p:*", "*", Decision::Deny).await.unwrap();
        store
            .set("tailscale:*", "*", Decision::Allow)
            .await
            .unwrap();
        // Redundant within its own class.
        store
            .set("p2p:peer1", "read_file", Decision::Deny)
            .await
            .unwrap();
        // NOT redundant — different decision from its class wildcard.
        store
            .set("tailscale:node1", "shell", Decision::Deny)
            .await
            .unwrap();
        assert_eq!(store.consolidate().await.unwrap(), 1);
        let rules = store.list().await.unwrap();
        assert_eq!(rules.len(), 3);
        // p2p:peer1/read_file removed; tailscale:node1/shell kept.
        assert!(rules
            .iter()
            .any(|r| r.agent_id == "tailscale:node1" && r.resource == "shell"));
    }

    #[tokio::test]
    async fn consolidate_orphan_l1_no_broader_levels() {
        let (store, _f) = open_temp_store().await;
        // Single L1 rule with no L2/L3/L4.  Must be kept.
        store
            .set("p2p:peer1", "read_file", Decision::Allow)
            .await
            .unwrap();
        assert_eq!(store.consolidate().await.unwrap(), 0);
        assert_eq!(store.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn consolidate_l1_falls_through_l2_to_l3() {
        let (store, _f) = open_temp_store().await;
        // No L2 for peer1.  L1 matches L3 → redundant.
        store
            .set("p2p:*", "read_file", Decision::Deny)
            .await
            .unwrap();
        store
            .set("p2p:peer1", "read_file", Decision::Deny)
            .await
            .unwrap();
        assert_eq!(store.consolidate().await.unwrap(), 1);
        let rules = store.list().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].agent_id, "p2p:*");
    }

    #[tokio::test]
    async fn consolidate_l2_kept_when_one_l3_disagrees() {
        let (store, _f) = open_temp_store().await;
        // L4: allow p2p:* *
        // L3: allow p2p:* read_file   (agrees with L2)
        // L3: deny  p2p:* shell       (disagrees with L2!)
        // L2: allow p2p:peer1 *       ← must be kept
        store.set("p2p:*", "*", Decision::Allow).await.unwrap();
        store
            .set("p2p:*", "read_file", Decision::Allow)
            .await
            .unwrap();
        store.set("p2p:*", "shell", Decision::Deny).await.unwrap();
        store.set("p2p:peer1", "*", Decision::Allow).await.unwrap();

        store.consolidate().await.unwrap();

        // L2 must survive because 'shell' L3 disagrees.
        let peer1 = AgentId::P2p("peer1".into());
        assert_eq!(store.check(&peer1, "shell").await, Decision::Allow);
        assert!(store
            .list()
            .await
            .unwrap()
            .iter()
            .any(|r| r.agent_id == "p2p:peer1" && r.resource == "*"));
    }

    #[tokio::test]
    async fn consolidate_is_idempotent() {
        let (store, _f) = open_temp_store().await;
        store.set("p2p:*", "*", Decision::Deny).await.unwrap();
        store
            .set("p2p:*", "read_file", Decision::Deny)
            .await
            .unwrap();
        store
            .set("p2p:peer1", "read_file", Decision::Deny)
            .await
            .unwrap();
        store.set("p2p:peer1", "*", Decision::Deny).await.unwrap();

        let first = store.consolidate().await.unwrap();
        assert!(first > 0);
        let second = store.consolidate().await.unwrap();
        assert_eq!(second, 0, "second consolidation must be a no-op");
    }

    #[tokio::test]
    async fn consolidate_l1_shield_must_not_cause_l2_removal() {
        let (store, _f) = open_temp_store().await;
        // L4: allow p2p:* *
        // L3: deny  p2p:* read_file
        // L2: allow p2p:peer1 *
        // L1: allow p2p:peer1 read_file
        //
        // Before: peer1/read_file → L1 allow.
        // L1 is redundant (L2 has same decision), but L2 must NOT be removed
        // because without L1 *and* L2, peer1/read_file falls to L3 (deny).
        store.set("p2p:*", "*", Decision::Allow).await.unwrap();
        store
            .set("p2p:*", "read_file", Decision::Deny)
            .await
            .unwrap();
        store.set("p2p:peer1", "*", Decision::Allow).await.unwrap();
        store
            .set("p2p:peer1", "read_file", Decision::Allow)
            .await
            .unwrap();

        store.consolidate().await.unwrap();

        // After consolidation, peer1/read_file must still resolve to Allow.
        let peer1 = AgentId::P2p("peer1".into());
        assert_eq!(
            store.check(&peer1, "read_file").await,
            Decision::Allow,
            "consolidation must not change effective decisions"
        );
    }

    #[tokio::test]
    async fn consolidate_l2_removed_when_surviving_l1_shields() {
        let (store, _f) = open_temp_store().await;
        // L4: allow p2p:* *
        // L3: deny  p2p:* read_file
        // L2: allow p2p:peer1 *
        // L1: deny  p2p:peer1 read_file  ← kept (differs from L2)
        //
        // L1 survives because it overrides L2. With L1 still shielding
        // peer1/read_file, L2 is genuinely redundant — without it,
        // peer1/read_file → L1 (deny), peer1/other → L4 (allow).
        store.set("p2p:*", "*", Decision::Allow).await.unwrap();
        store
            .set("p2p:*", "read_file", Decision::Deny)
            .await
            .unwrap();
        store.set("p2p:peer1", "*", Decision::Allow).await.unwrap();
        store
            .set("p2p:peer1", "read_file", Decision::Deny)
            .await
            .unwrap();

        assert_eq!(
            store.consolidate().await.unwrap(),
            1,
            "L2 should be removed"
        );
        let rules = store.list().await.unwrap();
        assert_eq!(rules.len(), 3);

        // Verify effective decisions are preserved.
        let peer1 = AgentId::P2p("peer1".into());
        let other = AgentId::P2p("other".into());
        assert_eq!(store.check(&peer1, "read_file").await, Decision::Deny);
        assert_eq!(store.check(&peer1, "other_tool").await, Decision::Allow);
        assert_eq!(store.check(&other, "read_file").await, Decision::Deny);
        assert_eq!(store.check(&other, "other_tool").await, Decision::Allow);
    }
}
