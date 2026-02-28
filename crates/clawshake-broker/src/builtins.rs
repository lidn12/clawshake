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
      "description": "Get all tools for a specific peer with their names, descriptions, and inputSchema. Use this before network_call to inspect parameter requirements.",
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
      "description": "Invoke a tool on a specific remote peer over the P2P network and return its result. The peer must be currently connected (use network_ping to check). Use network_tools to inspect the tool's inputSchema before calling.",
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
    }
  ]
}
"#;
