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
//! | Topic                    | Direction     | Payload shape                        |
//! |--------------------------|---------------|--------------------------------------|
//! | `channel.cli`            | user → agent  | `{ "text": "..." }` |
//! | `channel.cli.response`   | agent → user  | `{ "text": "..." }` |

pub mod cli_repl;

/// Topic prefix for all interactive channel events.
pub const TOPIC_PREFIX: &str = "channel";

/// Topic for CLI REPL user input.
pub const TOPIC_CLI: &str = "channel.cli";

/// Topic for CLI REPL agent responses.
pub const TOPIC_CLI_RESPONSE: &str = "channel.cli.response";
