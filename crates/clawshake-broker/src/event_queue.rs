//! In-memory event queue for `listen()` / `emit()` tools.
//!
//! A ring buffer of [`Event`]s consumed via cursor-based replay.  Multiple
//! consumers can independently read the same events (fan-out / pub-sub).
//! Events are never removed on read — they age out when the ring buffer
//! wraps.  Blocked listeners are woken via [`tokio::sync::Notify`].

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Notify};

/// Default ring buffer capacity.
const DEFAULT_MAX_SIZE: usize = 1024;

/// A single event in the queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Monotonically increasing ID, unique within this broker instance.
    pub id: u64,
    /// Unix timestamp in seconds (with fractional milliseconds).
    pub ts: f64,
    /// Dot-separated topic, e.g. `"fs.changed"`, `"peer.online"`.
    pub topic: String,
    /// Human-readable origin, e.g. `"watcher"`, `"peer:12D3KooW..."`.
    pub source: String,
    /// Arbitrary JSON payload.
    pub data: serde_json::Value,
}

/// Inner state behind the lock.
struct Inner {
    events: VecDeque<Event>,
    next_id: u64,
    max_size: usize,
}

/// Thread-safe, notify-capable event queue.
///
/// Wrap in `Arc` and pass everywhere — cloning is cheap.
#[derive(Clone)]
pub struct EventQueue {
    inner: Arc<Mutex<Inner>>,
    notify: Arc<Notify>,
}

impl EventQueue {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                events: VecDeque::with_capacity(DEFAULT_MAX_SIZE),
                next_id: 1,
                max_size: DEFAULT_MAX_SIZE,
            })),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Push a new event into the queue.  Wakes all blocked listeners.
    pub async fn push(&self, topic: impl Into<String>, source: impl Into<String>, data: serde_json::Value) -> u64 {
        let mut inner = self.inner.lock().await;
        let id = inner.next_id;
        inner.next_id += 1;

        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        let event = Event {
            id,
            ts,
            topic: topic.into(),
            source: source.into(),
            data,
        };

        if inner.events.len() >= inner.max_size {
            inner.events.pop_front();
        }
        inner.events.push_back(event);
        drop(inner);

        self.notify.notify_waiters();
        id
    }

    /// Return all events with `id > after` whose topic starts with any of
    /// the given prefixes.  If `topics` is empty, all events match.
    ///
    /// Blocks up to `timeout` waiting for at least one matching event.
    /// A timeout of `None` means block indefinitely.
    pub async fn wait_for(
        &self,
        topics: &[String],
        after: u64,
        timeout: Option<std::time::Duration>,
    ) -> (Vec<Event>, u64) {
        let deadline = timeout.map(|d| tokio::time::Instant::now() + d);

        loop {
            // Scan for matches under the lock.
            {
                let inner = self.inner.lock().await;
                let matches: Vec<Event> = inner
                    .events
                    .iter()
                    .filter(|e| e.id > after)
                    .filter(|e| {
                        topics.is_empty()
                            || topics.iter().any(|prefix| e.topic.starts_with(prefix.as_str()))
                    })
                    .cloned()
                    .collect();

                if !matches.is_empty() {
                    let cursor = matches.last().map(|e| e.id).unwrap_or(after);
                    return (matches, cursor);
                }

                // Return the current head cursor even if no matches.
                let head = if inner.next_id > 0 {
                    inner.next_id - 1
                } else {
                    0
                };

                // If timed out, return empty.
                if let Some(dl) = deadline {
                    if tokio::time::Instant::now() >= dl {
                        return (vec![], head);
                    }
                }
            }

            // Wait for new events (with optional deadline).
            match deadline {
                Some(dl) => {
                    let remaining = dl.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        let inner = self.inner.lock().await;
                        let head = if inner.next_id > 0 { inner.next_id - 1 } else { 0 };
                        return (vec![], head);
                    }
                    tokio::select! {
                        _ = self.notify.notified() => { /* new event — re-scan */ }
                        _ = tokio::time::sleep(remaining) => { /* timeout — will return empty on next loop */ }
                    }
                }
                None => {
                    self.notify.notified().await;
                }
            }
        }
    }

    /// Return the current cursor (last event ID, or 0 if empty).
    #[allow(dead_code)]
    pub async fn cursor(&self) -> u64 {
        let inner = self.inner.lock().await;
        if inner.next_id > 1 {
            inner.next_id - 1
        } else {
            0
        }
    }
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn push_and_read() {
        let q = EventQueue::new();
        let id1 = q.push("fs.changed", "watcher", json!({"path": "/tmp/a"})).await;
        let id2 = q.push("fs.created", "watcher", json!({"path": "/tmp/b"})).await;

        let (events, cursor) = q.wait_for(&[], 0, Some(std::time::Duration::from_millis(10))).await;
        assert_eq!(events.len(), 2);
        assert_eq!(cursor, id2);

        // After cursor should skip the first event.
        let (events, _) = q.wait_for(&[], id1, Some(std::time::Duration::from_millis(10))).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, id2);
    }

    #[tokio::test]
    async fn topic_prefix_filter() {
        let q = EventQueue::new();
        q.push("fs.changed", "watcher", json!({})).await;
        q.push("peer.online", "p2p", json!({})).await;

        let (events, _) = q.wait_for(
            &["fs".to_string()],
            0,
            Some(std::time::Duration::from_millis(10)),
        ).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].topic, "fs.changed");
    }

    #[tokio::test]
    async fn ring_buffer_eviction() {
        let q = EventQueue::new();
        // Push more than DEFAULT_MAX_SIZE events.
        for i in 0..DEFAULT_MAX_SIZE + 10 {
            q.push("test", "test", json!(i)).await;
        }
        let inner = q.inner.lock().await;
        assert_eq!(inner.events.len(), DEFAULT_MAX_SIZE);
    }

    #[tokio::test]
    async fn timeout_returns_empty() {
        let q = EventQueue::new();
        let (events, _) = q.wait_for(&[], 0, Some(std::time::Duration::from_millis(50))).await;
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn notify_wakes_listener() {
        let q = EventQueue::new();
        let q2 = q.clone();

        let handle = tokio::spawn(async move {
            q2.wait_for(&["test".to_string()], 0, Some(std::time::Duration::from_secs(5))).await
        });

        // Small delay to ensure the listener is waiting.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        q.push("test.hello", "test", json!({"msg": "hi"})).await;

        let (events, _) = handle.await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].topic, "test.hello");
    }
}
