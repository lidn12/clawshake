//! In-memory expose table for `network_expose` / `network_unexpose`.
//!
//! Tracks which local TCP ports are shared with the P2P network.  Each entry
//! maps a human-readable name (e.g. `"jupyter"`) to a port number and an
//! optional peer allowlist.
//!
//! The table is **ephemeral** — it does not survive broker restarts.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

/// A single expose entry.
#[derive(Debug, Clone)]
pub struct ExposeEntry {
    /// Unique identifier (UUID-like) returned to the caller.
    pub expose_id: String,
    /// The human-readable name (also the suffix in `connect_{name}`).
    pub name: String,
    /// The local TCP port to tunnel to.
    pub port: u16,
    /// Optional description shown in `connect_{name}` tool metadata.
    pub description: Option<String>,
    /// Peer allowlist.  `None` = any connected peer may connect.
    pub peers: Option<Vec<String>>,
}

/// Thread-safe map of expose name → entry.
#[derive(Debug, Clone, Default)]
pub struct ExposeTable {
    inner: Arc<RwLock<HashMap<String, ExposeEntry>>>,
}

impl ExposeTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace an expose entry.  Returns the generated `expose_id`.
    pub fn insert(&self, entry: ExposeEntry) -> String {
        let id = entry.expose_id.clone();
        let mut map = self.inner.write().expect("expose table lock");
        map.insert(entry.name.clone(), entry);
        id
    }

    /// Remove an expose entry by name.  Returns `true` if it existed.
    pub fn remove(&self, name: &str) -> bool {
        let mut map = self.inner.write().expect("expose table lock");
        map.remove(name).is_some()
    }

    /// Look up an expose entry by name.
    pub fn get(&self, name: &str) -> Option<ExposeEntry> {
        let map = self.inner.read().expect("expose table lock");
        map.get(name).cloned()
    }

    /// Return all active exposes.
    pub fn all(&self) -> Vec<ExposeEntry> {
        let map = self.inner.read().expect("expose table lock");
        map.values().cloned().collect()
    }
}
