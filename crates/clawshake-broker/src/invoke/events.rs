//! Invoke handlers for `listen` and `emit` event queue tools.

use crate::event_queue::EventQueue;
use serde_json::{json, Value};

/// Handle a `listen` tool call.
///
/// Blocks until matching events arrive or timeout expires.
pub async fn invoke_listen(arguments: &Value, event_queue: &EventQueue) -> Result<String, String> {
    let topics: Vec<String> = arguments
        .get("topics")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let timeout_secs: f64 = arguments
        .get("timeout_secs")
        .and_then(|v| v.as_f64())
        .unwrap_or(30.0);

    let after: u64 = arguments.get("after").and_then(|v| v.as_u64()).unwrap_or(0);

    let timeout = if timeout_secs <= 0.0 {
        None // block indefinitely
    } else {
        Some(std::time::Duration::from_secs_f64(timeout_secs))
    };

    let (events, cursor) = event_queue.wait_for(&topics, after, timeout).await;

    let events_json: Vec<Value> = events.iter().map(|e| json!(e)).collect();
    let result = json!({
        "events": events_json,
        "cursor": cursor,
    });

    Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()))
}

/// Handle an `emit` tool call.
///
/// Pushes an event into the local queue.  Remote delivery is the agent's
/// responsibility via `network_call(peer_id, "emit", {topic, data})`.
pub async fn invoke_emit(arguments: &Value, event_queue: &EventQueue) -> Result<String, String> {
    let topic = arguments
        .get("topic")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required field: topic".to_string())?;

    let data = arguments.get("data").cloned().unwrap_or(Value::Null);

    let id = event_queue.push(topic, "agent", data).await;

    let result = json!({
        "ok": true,
        "id": id,
    });

    Ok(result.to_string())
}
