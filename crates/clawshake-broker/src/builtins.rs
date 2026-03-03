//! Built-in manifests that ship with the broker.
//!
//! On first run, `seed()` writes these into `~/.clawshake/manifests/` so they
//! are immediately available as MCP tools without any manual setup.
//!
//! A file is only written if it does not already exist, so users can override
//! or delete individual manifests freely.

use anyhow::Result;
use std::path::Path;
use tracing::info;

/// (filename, JSON content)
const BUILTIN_MANIFESTS: &[(&str, &str)] = &[
    ("network.json", NETWORK_MANIFEST),
    ("events.json", EVENTS_MANIFEST),
];

/// Events manifest — seeded as `events.json` alongside `network.json`.
const EVENTS_MANIFEST: &str = r#"{
  "version": "1.0",
  "tools": [
    {
      "name": "listen",
      "description": "Block until events matching the given topics arrive. Returns an array of events and a cursor for resumption. Use this to wait for filesystem changes, peer messages, webhook deliveries, or any other event. Call with no topics to receive all events. The cursor enables efficient polling — pass the returned cursor as 'after' in the next call to only receive new events.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "topics": {
            "type": "array",
            "items": { "type": "string" },
            "description": "Topic prefixes to filter on (e.g. ['fs', 'peer']). Empty or omitted = all topics."
          },
          "timeout_secs": {
            "type": "number",
            "description": "Max seconds to wait. 0 = block indefinitely. Default: 30"
          },
          "after": {
            "type": "number",
            "description": "Cursor (event ID). Only return events with id > after. Default: 0 (all buffered events)."
          }
        }
      },
      "invoke": { "type": "in_process" }
    },
    {
      "name": "emit",
      "description": "Push an event into the local event queue. Other listeners on this node can receive it via listen(). Use dot-separated topic names (e.g. 'task.complete', 'msg.agent'). To send events to a remote peer, use network_call(peer_id, 'emit', {topic, data}) instead.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "topic": {
            "type": "string",
            "description": "Event topic, e.g. 'task.complete', 'msg.agent'"
          },
          "data": {
            "description": "Arbitrary JSON payload"
          }
        },
        "required": ["topic", "data"]
      },
      "invoke": { "type": "in_process" }
    }
  ]
}"#;

/// Code mode manifest — seeded only when Node.js is detected on PATH.
const CODEMODE_MANIFEST: &str = r#"{
  "version": "1.0",
  "tools": [
    {
      "name": "run_code",
      "description": "Execute a JavaScript script that can call any available tool as an async function. Use describe_tools first to discover the JS API. Intermediate results stay in the JS runtime and only console.log() output is returned. This is dramatically more token-efficient for multi-step workflows.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "script": {
            "type": "string",
            "description": "JavaScript code to execute. All tool functions are pre-loaded as async functions. Wrap multi-step logic in a single script. Output is collected from console.log() calls. A top-level return value is also automatically printed, so both styles work: `console.log(result)` and `return result`."
          }
        },
        "required": ["script"]
      },
      "invoke": { "type": "in_process" }
    },
    {
      "name": "describe_tools",
      "description": "Search available tools and return their JavaScript API definitions for use with run_code. Call with no query to list all tool categories. Call with a query to get specific tool function signatures.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "query": {
            "type": "string",
            "description": "Filter tools by name, source, or description substring. Omit for category summary."
          }
        }
      },
      "invoke": { "type": "in_process" }
    }
  ]
}
"#;

/// Seed all built-in manifests into `manifests_dir` if they are not already present.
///
/// When `code_mode` is true (Node.js detected), also seeds `codemode.json`.
pub fn seed(manifests_dir: &Path, code_mode: bool) -> Result<()> {
    std::fs::create_dir_all(manifests_dir)?;
    for (filename, content) in BUILTIN_MANIFESTS {
        let path = manifests_dir.join(filename);
        if !path.exists() {
            std::fs::write(&path, content)?;
            info!("Seeded built-in manifest: {filename}");
        }
    }

    let codemode_path = manifests_dir.join("codemode.json");
    if code_mode {
        if !codemode_path.exists() {
            std::fs::write(&codemode_path, CODEMODE_MANIFEST)?;
            info!("Seeded built-in manifest: codemode.json (Node.js detected)");
        }
    } else {
        // If Node.js is not available, remove a previously seeded codemode manifest
        // so the tools don't appear in listings.
        if codemode_path.exists() {
            std::fs::remove_file(&codemode_path)?;
            info!("Removed codemode.json (Node.js not found)");
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// network.json
// ---------------------------------------------------------------------------
//
// Every tool uses `clawshake-tools` as the command so the PATH-resolved binary
// is always used.  Arguments are passed as discrete array entries to avoid
// shell-quoting issues with {{param}} substitution.
//
// network_call passes `{{arguments}}` through substitute(), which serialises
// the object to its JSON string representation — exactly what `--args` expects.

const NETWORK_MANIFEST: &str = r#"{
  "version": "1.0",
  "tools": [
    {
      "name": "network_peers",
      "description": "List all discovered bridge nodes on the P2P network. Returns a JSON array of peer objects with peer_id, addrs, tools, and latency.",
      "inputSchema": {
        "type": "object",
        "properties": {}
      },
      "invoke": {
        "type": "cli",
        "command": "clawshake-tools",
        "args": ["network", "peers"]
      }
    },
    {
      "name": "network_tools",
      "description": "Progressive tool discovery for a remote peer. Without a query, returns a compact category summary of the peer's published tools. With a query, returns matching tools with their full name, description, and inputSchema — just enough to call them via network_call.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "peer_id": {
            "type": "string",
            "description": "The libp2p peer ID of the target node (e.g. 12D3KooW...)."
          },
          "query": {
            "type": "string",
            "description": "Filter tools by name or description substring. Omit for a category summary."
          }
        },
        "required": ["peer_id"]
      },
      "invoke": {
        "type": "cli",
        "command": "clawshake-tools",
        "args": ["network", "tools", "--peer-id", "{{peer_id}}", "--query", "{{query}}"]
      }
    },
    {
      "name": "network_search",
      "description": "Search for tools across all known peers by name or description substring.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "query": {
            "type": "string",
            "description": "Search term to match against tool names and descriptions."
          }
        },
        "required": ["query"]
      },
      "invoke": {
        "type": "cli",
        "command": "clawshake-tools",
        "args": ["network", "search", "--query", "{{query}}"]
      }
    },
    {
      "name": "network_ping",
      "description": "Check whether a peer is currently reachable from this node. Returns latency in milliseconds or an error.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "peer_id": {
            "type": "string",
            "description": "The libp2p peer ID to ping."
          }
        },
        "required": ["peer_id"]
      },
      "invoke": {
        "type": "cli",
        "command": "clawshake-tools",
        "args": ["network", "ping", "--peer-id", "{{peer_id}}"]
      }
    },
    {
      "name": "network_record",
      "description": "Fetch the raw DHT announcement record for a peer. Returns the full AnnouncementRecord including schema version, tools list, listen addresses, and timestamp.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "peer_id": {
            "type": "string",
            "description": "The libp2p peer ID whose DHT record to retrieve."
          }
        },
        "required": ["peer_id"]
      },
      "invoke": {
        "type": "cli",
        "command": "clawshake-tools",
        "args": ["network", "record", "--peer-id", "{{peer_id}}"]
      }
    },
    {
      "name": "network_call",
      "description": "Invoke a tool on a specific remote peer over the P2P network and return its result. The peer must be currently connected (use network_ping to check). Use network_tools with a query to inspect the tool's inputSchema before calling. Pass the tool's input parameters as the `arguments` field (not `params`), e.g. { \"peer_id\": \"...\", \"tool\": \"read_file\", \"arguments\": { \"path\": \"/tmp/foo.txt\" } }.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "peer_id": {
            "type": "string",
            "description": "libp2p peer ID string of the target peer"
          },
          "tool": {
            "type": "string",
            "description": "Fully-qualified tool name to invoke on the remote peer (e.g. \"spotify_play\")"
          },
          "arguments": {
            "type": "object",
            "description": "Arguments to pass to the tool. Must match the tool's inputSchema. Omit or pass {} for tools with no required arguments."
          }
        },
        "required": ["peer_id", "tool"]
      },
      "invoke": {
        "type": "cli",
        "command": "clawshake-tools",
        "args": ["network", "call", "--peer-id", "{{peer_id}}", "--tool", "{{tool}}", "--args", "{{arguments}}"]
      }
    },
    {
      "name": "network_models",
      "description": "List AI models available on the peer-to-peer network. Returns model names, context lengths, parameter counts, and the peer hosting each model.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "peer_id": {
            "type": "string",
            "description": "Filter to models from a specific peer. Omit to list models from all peers."
          }
        }
      },
      "invoke": {
        "type": "cli",
        "command": "clawshake-tools",
        "args": ["network", "models", "--peer-id", "{{peer_id}}"]
      }
    }
  ]
}
"#;
