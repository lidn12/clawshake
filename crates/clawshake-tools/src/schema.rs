//! MCP tool schema definitions for the `network_*` tools.
//!
//! These are the canonical schemas for all six built-in P2P tools.  The bridge
//! daemon serves them to inbound P2P callers; any Rust MCP host (broker, etc.)
//! can call [`tool_definitions`] to inject them into its own `tools/list`
//! response.

use serde_json::{json, Value};

/// Returns MCP tool schema objects for all six `network_*` tools.
/// Suitable for embedding directly in a `tools/list` JSON-RPC response.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "network_peers",
            "description": "List all discovered bridge nodes on the network with their peer IDs, addresses, and last-seen timestamps. Data comes from the local cache; use network_tools with a specific peer_id for authoritative live info.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        }),
        json!({
            "name": "network_tools",
            "description": "Fetch all tools for a specific peer directly from the DHT (live, not cached). Returns each tool's name, description, and inputSchema. Use this before network_call to inspect parameter requirements.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": {
                        "type": "string",
                        "description": "libp2p peer ID string (from network_peers)"
                    }
                },
                "required": ["peer_id"]
            }
        }),
        json!({
            "name": "network_search",
            "description": "Search for tools across all known peers by tool name or description substring. Searches the local cache; results may be stale. Use network_tools on a specific peer for authoritative data.",
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
            "name": "network_ping",
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
            "name": "network_describe",
            "description": "Progressive tool discovery for a remote peer. Without a query, returns a compact category summary of the peer's published tools. With a query, returns matching tools with their full name, description, and inputSchema — just enough to call them via network_call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": {
                        "type": "string",
                        "description": "libp2p peer ID string (from network_peers)"
                    },
                    "query": {
                        "type": "string",
                        "description": "Filter tools by name or description substring. Omit for a category summary."
                    }
                },
                "required": ["peer_id"]
            }
        }),
        json!({
            "name": "network_record",
            "description": "Fetch the raw DHT announcement record for a peer directly from the Kademlia DHT (live, not cached) — schema version, peer_id, tools, addrs, and timestamp. Useful for verifying what a peer is currently announcing or debugging schema mismatches.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peer_id": {
                        "type": "string",
                        "description": "libp2p peer ID string (from network_peers)"
                    }
                },
                "required": ["peer_id"]
            }
        }),
    ]
}
