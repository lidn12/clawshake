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
  "tools": [
    { "name": "tool_name", "description": "Human-readable description", "inputSchema": { "type": "object", "properties": { ... } } },
    ...
  ],
  "addrs": [
    "/ip4/<relay-ip>/tcp/<port>/p2p/<relay-peer-id>/p2p-circuit/p2p/12D3KooW..."
  ],
  "ts": 1740000000
}
```

### Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `v` | integer | yes | Schema version. Always `1` for records conforming to this spec. |
| `peer_id` | string | yes | Base58btc-encoded `PeerId` of the publishing node (libp2p multibase string form). Redundant with the DHT key but included for self-contained parsing. |
| `tools` | object array | yes | Tool entries with `name`, `description`, and `inputSchema`, matching the MCP `tools/list` tool definition shape. May be empty `[]` on nodes with no backend. |
| `addrs` | string array | yes | Multiaddr strings for reaching this node. In practice, relay circuit addresses: `/ip4/<relay-ip>/tcp\|udp/<port>/p2p/<relay-peer-id>/p2p-circuit/p2p/<peer-id>`. One entry per relay. May be empty if no relay reservation is active yet. |
| `ts` | integer | yes | Unix timestamp (seconds) when the record was built. Use to detect stale records — records older than 10 minutes should be treated as potentially stale. |

### `tools` entry

```json
{
  "name": "write_file",
  "description": "Create a new file or completely overwrite an existing file with new content.",
  "inputSchema": {
    "$schema": "http://json-schema.org/draft-07/schema#",
    "type": "object",
    "properties": {
      "path": { "type": "string" },
      "content": { "type": "string" }
    },
    "required": ["path", "content"]
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Tool name. Must use only `[a-z0-9_-]` characters (required by MCP clients such as VS Code). Use underscore as a word separator. |
| `description` | string | Human-readable description, suitable for display or agent reasoning. May be empty string `""`. |
| `inputSchema` | object | JSON Schema object describing the tool's input parameters. Always present; defaults to `{"type": "object"}` per MCP spec. |

---

## Tool Naming Convention

Tool names must only use characters from the set `[a-z0-9_-]`. Dots (`.`) are not permitted — MCP clients such as VS Code reject tool names containing dots.

Use underscore as a word separator:

- `write_file`, `list_directory` — tools from a filesystem MCP backend
- `network_peers`, `network_call` — built-in network explorer tools (present on every node)
- `agent_ping`, `agent_ask` — built-in agent channel tools (when implemented)
- `spotify_play`, `spotify_pause` — tools registered from a Spotify manifest

For namespaced tools (where multiple apps expose tools on the same node), use an underscore-separated prefix: `spotify_play`, `calendar_create_event`. This is a naming convention only — it has no semantic meaning in the DHT record itself.

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
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"write_file","arguments":{"path":"/home/alice/hello.txt","content":"Hello from clawshake!"}}}
```

**Response** — standard MCP `tools/call` result:
```json
{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"Successfully wrote to /home/alice/hello.txt"}]}}
```

The bridge on the receiving end stamps caller identity from the Noise-verified peer ID, checks the permission store, and either proxies the call to its local MCP backend or returns a permission error.

---

## Interoperability Notes

- **Any libp2p node** that can perform a Kademlia GET with the above key format and parse the JSON value can read tool announcements from the Clawshake network.
- **Any libp2p node** that speaks `/clawshake/mcp/1.0.0` with the length-prefixed JSON codec can invoke tools on a bridge node, subject to its permission policy.
- The permission policy is enforced by the bridge — a remote node must be explicitly allowed (`clawshake-bridge permissions allow <peer_id> <tool>`) before calls are accepted. Fresh installs default-deny all P2P callers.
- The `tools` array contains full tool objects with `name`, `description`, and `inputSchema`, matching the MCP tool definition shape.

---

## Example Record

From the live test network (February 2026):

```json
{
  "v": 1,
  "peer_id": "12D3KooWHZq8jRUBzS8ArgQFhzg5M9NXAfDL3vcimLxgQtsFFyyC",
  "tools": [
    {
      "name": "read_text_file",
      "description": "Read the complete contents of a file from the file system as text.",
      "inputSchema": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }
    },
    {
      "name": "write_file",
      "description": "Create a new file or completely overwrite an existing file with new content.",
      "inputSchema": { "type": "object", "properties": { "path": { "type": "string" }, "content": { "type": "string" } }, "required": ["path", "content"] }
    },
    {
      "name": "list_directory",
      "description": "Get a detailed listing of all files and directories in a specified path.",
      "inputSchema": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }
    }
  ],
  "addrs": [
    "/ip4/43.143.33.106/tcp/7474/p2p/12D3KooWDi1ntKAkUYpHfijLNExUTsirFyofnkEB3yjC8P3EGcY5/p2p-circuit/p2p/12D3KooWHZq8jRUBzS8ArgQFhzg5M9NXAfDL3vcimLxgQtsFFyyC",
    "/ip4/43.143.33.106/udp/7474/quic-v1/p2p/12D3KooWDi1ntKAkUYpHfijLNExUTsirFyofnkEB3yjC8P3EGcY5/p2p-circuit/p2p/12D3KooWHZq8jRUBzS8ArgQFhzg5M9NXAfDL3vcimLxgQtsFFyyC"
  ],
  "ts": 1772084902
}
```

Backend: `npx @modelcontextprotocol/server-filesystem ~/shared`
Relay: `12D3KooWDi1ntKAkUYpHfijLNExUTsirFyofnkEB3yjC8P3EGcY5` at `43.143.33.106:7474`
