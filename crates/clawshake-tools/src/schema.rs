//! MCP tool schema definitions for the `network.*` namespace.
//!
//! These are the canonical schemas for all six built-in P2P tools.  The bridge
//! daemon serves them to inbound P2P callers; any Rust MCP host (broker, etc.)
//! can call [`tool_definitions`] to inject them into its own `tools/list`
//! response.

use serde_json::{json, Value};

/// Returns MCP tool schema objects for all six `network.*` tools.
/// Suitable for embedding directly in a `tools/list` JSON-RPC response.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "network.peers",
            "description": "List all discovered bridge nodes on the network with their peer IDs, addresses, tool counts, and last-seen timestamps.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        }),
        json!({
            "name": "network.tools",
            "description": "Get the full tool list (name + description) for a specific peer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": {
                        "type": "string",
                        "description": "libp2p peer ID string (from network.peers)"
                    }
                },
                "required": ["peer_id"]
            }
        }),
        json!({
            "name": "network.search",
            "description": "Search for tools across all known peers by tool name or description substring.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Case-insensitive substring to match against tool names and descriptions"
                    }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "network.describe",
            "description": "Get the description for a specific tool on a specific peer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": {
                        "type": "string",
                        "description": "libp2p peer ID string"
                    },
                    "tool_name": {
                        "type": "string",
                        "description": "Fully-qualified tool name (e.g. \"spotify.play\")"
                    }
                },
                "required": ["peer_id", "tool_name"]
            }
        }),
        json!({
            "name": "network.ping",
            "description": "Check whether a peer currently has an active connection to this node.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": {
                        "type": "string",
                        "description": "libp2p peer ID string"
                    }
                },
                "required": ["peer_id"]
            }
        }),
        json!({
            "name": "network.call",
            "description": "Invoke a tool on a specific remote peer over the P2P network and return its result. The peer must be currently connected (use network.ping to check). Arguments must match the tool's inputSchema (use network.describe to inspect it).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": {
                        "type": "string",
                        "description": "libp2p peer ID string of the target peer"
                    },
                    "tool": {
                        "type": "string",
                        "description": "Fully-qualified tool name to invoke on the remote peer (e.g. \"spotify.play\")"
                    },
                    "arguments": {
                        "type": "object",
                        "description": "Arguments to pass to the tool. Must match the tool's inputSchema. Omit or pass {} for tools with no required arguments."
                    }
                },
                "required": ["peer_id", "tool"]
            }
        }),
        json!({
            "name": "network.record",
            "description": "Return the raw DHT announcement record for a peer exactly as stored in the Kademlia DHT — schema version, peer_id, tools, tool_details, addrs, and timestamp. Useful for verifying the live record matches the published spec or debugging schema mismatches.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": {
                        "type": "string",
                        "description": "libp2p peer ID string (from network.peers)"
                    }
                },
                "required": ["peer_id"]
            }
        }),
    ]
}
