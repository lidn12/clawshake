//! Interactive I/O channels for clawshake agents.
//!
//! A *channel* is a process that bridges some external input source (terminal,
//! web UI, chat platform) into the broker's event queue via `emit()`/`listen()`.
//!
//! Agents receiving input through channels simply call
//! `listen(topics: ["channel"])` — they do not need to know which channel
//! type produced the event.
//!
//! # Topic conventions
//!
//! All channels use the `channel.*` topic namespace:
//!
//! | Topic                         | Direction     | Payload shape                        |
//! |-------------------------------|---------------|--------------------------------------|
//! | `channel.cli`                 | user → agent  | `{ "text": "..." }` |
//! | `channel.cli.response`        | agent → user  | `{ "text": "..." }` |
//! | `channel.ui.<frame_id>`       | user → agent  | `{ "frame_id", "event", "id", "data" }` |
//! | `channel.ui.<frame_id>.response` | agent → UI | `{ "text": "..." }` |
//!
//! The broker automatically routes `channel.ui.<frame_id>.response` events back
//! to the corresponding webview frame via `WsOutgoing::Push`, so agents only
//! need to `emit()` on the response topic — no explicit `ui_push` call required.

pub mod cli_repl;

/// Topic prefix for all interactive channel events.
pub const TOPIC_PREFIX: &str = "channel";

/// Topic for CLI REPL user input.
pub const TOPIC_CLI: &str = "channel.cli";

/// Topic for CLI REPL agent responses.
pub const TOPIC_CLI_RESPONSE: &str = "channel.cli.response";

/// Topic prefix for webview interaction events (user → agent).
/// Full topic is `channel.ui.<frame_id>`.
pub const TOPIC_UI: &str = "channel.ui";

/// Build the inbound topic for a specific frame: `channel.ui.<frame_id>`.
pub fn ui_topic(frame_id: &str) -> String {
    format!("channel.ui.{frame_id}")
}

/// Build the response topic for a specific frame: `channel.ui.<frame_id>.response`.
pub fn ui_response_topic(frame_id: &str) -> String {
    format!("channel.ui.{frame_id}.response")
}

/// Extract the frame_id from a `channel.ui.<frame_id>.response` topic string.
/// Returns `None` if the topic doesn't match the pattern.
pub fn parse_ui_response_frame_id(topic: &str) -> Option<&str> {
    let rest = topic.strip_prefix("channel.ui.")?;
    let frame_id = rest.strip_suffix(".response")?;
    if frame_id.is_empty() {
        None
    } else {
        Some(frame_id)
    }
}
