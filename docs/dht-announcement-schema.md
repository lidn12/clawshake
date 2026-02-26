# Clawshake DHT Announcement Schema

**Version:** 1  
**Status:** Stable (in production use)

This document specifies the record format that `clawshake-bridge` nodes publish to the Kademlia DHT to advertise their available tools and reachable addresses. Any implementation that can read or write Kademlia records can interoperate with the Clawshake network using this spec alone.

---

## DHT Key

```
key = peer_id.to_bytes()
```

The key is the raw **multihash bytes** of the publishing node's `PeerId` — the same encoding used by `libp2p::PeerId::to_bytes()`. This is the Ed25519 public key wrapped in a multihash envelope, not the base58 string form.

To look up a peer's record, you must already know their `PeerId`. Discovery of unknown peers happens through rendezvous and mDNS, not by scanning the DHT.

---

## Record Value

The value is a **UTF-8 encoded JSON object** with the following fields:

```json
{
  "v": 1,
  "peer_id": "12D3KooW...",
  "tools": ["tool.name", ...],
  "tool_details": [
    { "name": "tool.name", "description": "Human-readable description" },
    ...
  ],
  "addrs": [
    "/ip4/43.143.33.106/tcp/7474/p2p/12D3.../p2p-circuit/p2p/12D3KooW..."
  ],
  "ts": 1740000000
}
```

### Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `v` | integer | yes | Schema version. Always `1` for records conforming to this spec. |
| `peer_id` | string | yes | Base58btc-encoded `PeerId` of the publishing node (libp2p multibase string form). Redundant with the DHT key but included for self-contained parsing. |
| `tools` | string array | yes | Flat list of fully-qualified tool names (e.g. `"spotify.play"`). Present in all versions. Readers that only need tool names can use this field and ignore `tool_details`. |
| `tool_details` | object array | yes (v1+) | Full tool entries with `name` and `description`. Supersedes `tools` for readers that want descriptions. May be empty `[]` on nodes with no backend. |
| `addrs` | string array | yes | Multiaddr strings for reaching this node. In practice, relay circuit addresses: `/ip4/<relay>/tcp|udp/<port>/p2p/<relay_id>/p2p-circuit/p2p/<peer_id>`. One entry per relay. May be empty if no relay reservation is active yet. |
| `ts` | integer | yes | Unix timestamp (seconds) when the record was built. Use to detect stale records — records older than 10 minutes should be treated as potentially stale. |

### `tool_details` entry

```json
{
  "name": "spotify.play",
  "description": "Play a track, album, or playlist by name or URI"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Fully-qualified tool name. Dot-separated namespace prefix + tool name. |
| `description` | string | Human-readable description, suitable for display or agent reasoning. May be empty string `""`. |

---

## Tool Naming Convention

Tool names follow the pattern `<namespace>.<tool>`:

- `spotify.play`, `spotify.pause` — tools registered from the Spotify manifest
- `network.peers`, `network.call` — built-in network explorer tools (present on every node)
- `agent.ping`, `agent.ask` — built-in agent channel tools (when implemented)

The namespace prefix is a naming convention only — it has no semantic meaning in the DHT record itself.

---

## Refresh and Expiry

- Records are **published on startup** and **refreshed every 300 seconds** (5 minutes).
- Records are also re-published immediately when a new external address is confirmed (e.g. relay reservation established, UPnP mapping confirmed).
- Kademlia records have no built-in expiry in this deployment — the `expires` field is `None`. Consumers should treat records with `ts` older than 10 minutes as potentially stale (the publishing node may have gone offline).

---

## Calling a Tool

Once a peer's record is retrieved from the DHT, tool invocation uses the Clawshake MCP proxy protocol over a direct or relayed libp2p connection:

**Protocol ID:** `/clawshake/mcp/1.0.0`  
**Transport:** `request_response` behaviour (libp2p)  
**Codec:** 4-byte big-endian length prefix + UTF-8 JSON payload

**Request** — standard MCP `tools/call` JSON-RPC:
```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"spotify.play","arguments":{"query":"Bohemian Rhapsody"}}}
```

**Response** — standard MCP `tools/call` result:
```json
{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"Now playing: Bohemian Rhapsody — Queen"}]}}
```

The bridge on the receiving end stamps caller identity from the Noise-verified peer ID, checks the permission store, and either proxies the call to its local MCP backend or returns a permission error.

---

## Interoperability Notes

- **Any libp2p node** that can perform a Kademlia GET with the above key format and parse the JSON value can read tool announcements from the Clawshake network.
- **Any libp2p node** that speaks `/clawshake/mcp/1.0.0` with the length-prefixed JSON codec can invoke tools on a bridge node, subject to its permission policy.
- The permission policy is enforced by the bridge — a remote node must be explicitly allowed (`clawshake-bridge permissions allow <peer_id> <tool>`) before calls are accepted. Fresh installs default-deny all P2P callers.
- The `tools` array is kept for backward compatibility with v1 readers. New implementations should read `tool_details` and fall back to `tools` if `tool_details` is absent or empty.

---

## Example Record

From the live test network (February 2026):

```json
{
  "v": 1,
  "peer_id": "12D3KooWHZq8jRUBzS8ArgQFhzg5M9NXAfDL3vcimLxgQtsFFyyC",
  "tools": [
    "read_file", "read_text_file", "read_media_file", "read_multiple_files",
    "write_file", "edit_file", "create_directory", "list_directory",
    "list_directory_with_sizes", "directory_tree", "move_file",
    "search_files", "get_file_info", "list_allowed_directories"
  ],
  "tool_details": [
    { "name": "read_file", "description": "Read the complete contents of a file as text." },
    { "name": "write_file", "description": "Create a new file or completely overwrite an existing file with new content." },
    { "name": "list_directory", "description": "Get a detailed listing of all files and directories in a specified path." }
  ],
  "addrs": [
    "/ip4/43.143.33.106/tcp/7474/p2p/12D3KooWDi1ntKAkUYpHfijLNExUTsirFyofnkEB3yjC8P3EGcY5/p2p-circuit/p2p/12D3KooWHZq8jRUBzS8ArgQFhzg5M9NXAfDL3vcimLxgQtsFFyyC",
    "/ip4/43.143.33.106/udp/7474/quic-v1/p2p/12D3KooWDi1ntKAkUYpHfijLNExUTsirFyofnkEB3yjC8P3EGcY5/p2p-circuit/p2p/12D3KooWHZq8jRUBzS8ArgQFhzg5M9NXAfDL3vcimLxgQtsFFyyC"
  ],
  "ts": 1772084902
}
```

Backend: `npx @modelcontextprotocol/server-filesystem C:\Users\li\Desktop`  
Relay: `12D3KooWDi1ntKAkUYpHfijLNExUTsirFyofnkEB3yjC8P3EGcY5` at `43.143.33.106:7474`
