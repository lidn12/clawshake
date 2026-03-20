//! In-memory cron scheduler.
//!
//! Provides three tools:
//! - `cron_add` — register a cron-style schedule that fires events.
//! - `cron_list` — list all registered cron jobs.
//! - `cron_remove` — remove a cron job by ID.
//!
//! When a job fires, it pushes an event with topic `cron.fired` into the
//! broker's [`EventQueue`], so any agent listening for that topic receives it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::event_queue::EventQueue;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Unique job identifier (monotonically increasing).
type JobId = u64;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CronJob {
    pub id: JobId,
    /// Human-readable label.
    pub label: String,
    /// Interval in seconds between firings.
    pub interval_secs: u64,
    /// Optional JSON data payload attached to each fired event.
    pub data: Value,
}

/// Shared scheduler state.
#[derive(Clone)]
pub struct CronScheduler {
    inner: Arc<Mutex<SchedulerInner>>,
}

struct SchedulerInner {
    next_id: JobId,
    jobs: HashMap<JobId, CronJob>,
    /// Abort handles for running tasks.
    handles: HashMap<JobId, tokio::task::JoinHandle<()>>,
}

impl CronScheduler {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(SchedulerInner {
                next_id: 1,
                jobs: HashMap::new(),
                handles: HashMap::new(),
            })),
        }
    }

    // -----------------------------------------------------------------------
    // Tool handlers
    // -----------------------------------------------------------------------

    /// `cron_add` — create a new cron job.
    ///
    /// # Arguments
    ///
    /// - `label` (required): Human-readable label.
    /// - `interval_secs` (required): Firing interval in seconds (min 5).
    /// - `data` (optional): JSON payload to include in the fired event.
    pub async fn handle_add(&self, arguments: &Value, eq: &EventQueue) -> Result<String> {
        let label = arguments
            .get("label")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing required field: label"))?
            .to_string();

        let interval_secs = arguments
            .get("interval_secs")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow::anyhow!("missing required field: interval_secs"))?;

        if interval_secs < 5 {
            bail!("interval_secs must be >= 5 (got {interval_secs})");
        }

        let data = arguments
            .get("data")
            .cloned()
            .unwrap_or(Value::Null);

        let mut inner = self.inner.lock().await;
        let id = inner.next_id;
        inner.next_id += 1;

        let job = CronJob {
            id,
            label: label.clone(),
            interval_secs,
            data: data.clone(),
        };
        inner.jobs.insert(id, job.clone());

        // Spawn a background task that fires the event periodically.
        let eq = eq.clone();
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
            // Skip the immediate first tick.
            interval.tick().await;
            loop {
                interval.tick().await;
                debug!(id, label = %label, "cron: firing");
                eq.push(
                    "cron.fired",
                    format!("cron:{id}"),
                    json!({
                        "job_id": id,
                        "label": label,
                        "data": data,
                    }),
                )
                .await;
            }
        });
        inner.handles.insert(id, handle);

        info!(id, "cron: added job");
        Ok(serde_json::to_string_pretty(&json!({
            "id": id,
            "label": job.label,
            "interval_secs": job.interval_secs,
        }))?)
    }

    /// `cron_list` — list all registered cron jobs.
    pub async fn handle_list(&self, _arguments: &Value) -> Result<String> {
        let inner = self.inner.lock().await;
        let jobs: Vec<&CronJob> = inner.jobs.values().collect();
        if jobs.is_empty() {
            return Ok("No cron jobs registered.".into());
        }
        Ok(serde_json::to_string_pretty(&jobs)?)
    }

    /// `cron_remove` — remove a cron job by ID.
    ///
    /// # Arguments
    ///
    /// - `id` (required): Job ID to remove.
    pub async fn handle_remove(&self, arguments: &Value) -> Result<String> {
        let id = arguments
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow::anyhow!("missing required field: id"))?;

        let mut inner = self.inner.lock().await;
        if inner.jobs.remove(&id).is_none() {
            bail!("no cron job with id {id}");
        }
        if let Some(handle) = inner.handles.remove(&id) {
            handle.abort();
        }
        info!(id, "cron: removed job");
        Ok(format!("Removed cron job {id}"))
    }
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Returns MCP tool schema objects for cron tools.
pub fn cron_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "cron_add",
            "description": "Register a periodic cron job. The job fires an event with topic 'cron.fired' at the specified interval. Use listen(topics=['cron.fired']) to receive events.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "label": {
                        "type": "string",
                        "description": "Human-readable label for the job."
                    },
                    "interval_secs": {
                        "type": "number",
                        "description": "Interval between firings in seconds (minimum 5)."
                    },
                    "data": {
                        "description": "Optional JSON payload to include with each fired event."
                    }
                },
                "required": ["label", "interval_secs"]
            }
        }),
        json!({
            "name": "cron_list",
            "description": "List all registered cron jobs with their IDs, labels, and intervals.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        }),
        json!({
            "name": "cron_remove",
            "description": "Remove a cron job by its ID. The job stops firing immediately.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {
                        "type": "number",
                        "description": "The job ID (from cron_add or cron_list)."
                    }
                },
                "required": ["id"]
            }
        }),
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn add_list_remove() {
        let sched = CronScheduler::new();
        let eq = EventQueue::new();

        // Add a job.
        let result = sched
            .handle_add(
                &json!({"label": "test", "interval_secs": 60}),
                &eq,
            )
            .await
            .unwrap();
        assert!(result.contains("\"id\": 1"));

        // List should show it.
        let list = sched.handle_list(&json!({})).await.unwrap();
        assert!(list.contains("test"));

        // Remove it.
        let removed = sched.handle_remove(&json!({"id": 1})).await.unwrap();
        assert!(removed.contains("Removed"));

        // List should be empty.
        let list = sched.handle_list(&json!({})).await.unwrap();
        assert!(list.contains("No cron jobs"));
    }

    #[tokio::test]
    async fn minimum_interval() {
        let sched = CronScheduler::new();
        let eq = EventQueue::new();
        let result = sched
            .handle_add(&json!({"label": "fast", "interval_secs": 1}), &eq)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains(">= 5"));
    }

    #[tokio::test]
    async fn remove_nonexistent() {
        let sched = CronScheduler::new();
        let result = sched.handle_remove(&json!({"id": 999})).await;
        assert!(result.is_err());
    }
}
