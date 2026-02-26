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
const BUILTIN_MANIFESTS: &[(&str, &str)] = &[("network.json", NETWORK_MANIFEST)];

/// Seed all built-in manifests into `manifests_dir` if they are not already present.
pub fn seed(manifests_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(manifests_dir)?;
    for (filename, content) in BUILTIN_MANIFESTS {
        let path = manifests_dir.join(filename);
        if !path.exists() {
            std::fs::write(&path, content)?;
            info!("Seeded built-in manifest: {filename}");
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
// network_call accepts `args` as a raw JSON string (e.g. '{"track":"Bohemian
// Rhapsody"}') exactly as clawshake-tools expects.

const NETWORK_MANIFEST: &str = r#"{
  "app": "network",
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
      "description": "List all tools exposed by a specific peer.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "peer_id": {
            "type": "string",
            "description": "The libp2p peer ID of the target node (e.g. 12D3KooW...)."
          }
        },
        "required": ["peer_id"]
      },
      "invoke": {
        "type": "cli",
        "command": "clawshake-tools",
        "args": ["network", "tools", "--peer-id", "{{peer_id}}"]
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
      "name": "network_describe",
      "description": "Get the full description and input schema for a specific tool on a peer.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "peer_id": {
            "type": "string",
            "description": "The libp2p peer ID of the target node."
          },
          "tool_name": {
            "type": "string",
            "description": "Fully-qualified tool name (e.g. \"spotify.play\")."
          }
        },
        "required": ["peer_id", "tool_name"]
      },
      "invoke": {
        "type": "cli",
        "command": "clawshake-tools",
        "args": ["network", "describe", "--peer-id", "{{peer_id}}", "--tool-name", "{{tool_name}}"]
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
      "description": "Invoke a tool on a remote peer and return the result. The peer must be reachable and have granted permission for this tool.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "peer_id": {
            "type": "string",
            "description": "The libp2p peer ID of the node that owns the tool."
          },
          "tool": {
            "type": "string",
            "description": "Fully-qualified tool name to invoke (e.g. \"spotify.play\")."
          },
          "args": {
            "type": "string",
            "description": "Tool arguments as a JSON object string, e.g. '{\"track\":\"Bohemian Rhapsody\"}'. Omit or pass '{}' for tools with no arguments."
          }
        },
          "required": ["peer_id", "tool", "args"]
      },
      "invoke": {
        "type": "cli",
        "command": "clawshake-tools",
        "args": ["network", "call", "--peer-id", "{{peer_id}}", "--tool", "{{tool}}", "--args", "{{args}}"]
      }
    }
  ]
}
"#;
