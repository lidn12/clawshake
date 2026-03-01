# Clawshake

[![CI](https://github.com/lidn12/clawshake/actions/workflows/ci.yml/badge.svg)](https://github.com/lidn12/clawshake/actions/workflows/ci.yml)

**Give your AI agent tools that work across machines.**

Clawshake is a lightweight daemon that turns any machine into a node on a peer-to-peer tool network. Agents discover and call tools on remote machines — opening files, searching directories, controlling apps — without cloud servers, port forwarding, or API keys.

Drop a 10-line JSON manifest, and the tool is live on the network. Any MCP-compatible agent can call it.

## How it works

```
  Your machine                                 Remote machine
┌────────────────────────┐                   ┌────────────────────────┐
│                        │                   │  ~/.clawshake/         │
│  Agent (VS Code, etc.) │                   │  manifests/            │
│       │                │                   │       │                │
│       │ MCP            │                   │       ▼                │
│       ▼                │                   │    Broker              │
│    Broker              │      libp2p       │       ▲                │
│       ▲                │   QUIC · TCP      │       │                │
│       │                │   relay · mDNS    │   Permissions          │
│    Bridge ◄────────────────────────────►   │       ▲                │
│                        │                   │    Bridge              │
└────────────────────────┘                   └────────────────────────┘
```

1. Each machine runs `clawshake run` — a single binary that starts an MCP broker and a P2P bridge.
2. The broker reads manifest files from `~/.clawshake/manifests/` and exposes them as MCP tools.
3. The bridge announces tools to a Kademlia DHT and discovers other nodes via mDNS + relay.
4. Agents connect via MCP (SSE on `localhost:7475` or stdio) and get local + remote tools in one list.
5. Remote tool calls are routed bridge-to-bridge. The receiving bridge checks **permissions** before forwarding to the local broker.

## Quick start

**Prerequisites:** [Rust toolchain](https://rustup.rs/) (for building from source).

```bash
git clone https://github.com/lidn12/clawshake.git
cd clawshake
cargo build --release
```

The build produces several binaries. Add them to your PATH:

```bash
# Copy all binaries to a directory on your PATH, e.g.
cp target/release/clawshake target/release/clawshake-tools ~/.local/bin/
```

`clawshake-tools` **must be on PATH** — the broker shells out to it for the six `network_*` tools. Without it, network discovery and remote invocation won't work.

Start the node:

```bash
# Run the unified daemon (broker + bridge)
./target/release/clawshake run
```

By default the node discovers peers only on the local network (mDNS). To join the wider network, copy the reference config and uncomment the bootstrap peers:

```bash
cp config.toml ~/.clawshake/config.toml
# Edit ~/.clawshake/config.toml — uncomment the bootstrap lines
```

Or run your own private network by pointing `bootstrap` to your own relay node.

Point your MCP client at it. For VS Code, add to `.vscode/mcp.json`:

```json
{
  "servers": {
    "clawshake": {
      "type": "sse",
      "url": "http://127.0.0.1:7475/sse"
    }
  }
}
```

For clients that prefer **stdio** (Claude Desktop, etc.), run the broker directly:

```json
{
  "servers": {
    "clawshake": {
      "type": "stdio",
      "command": "clawshake-broker"
    }
  }
}
```

Check node status:

```bash
clawshake status
```

## Manifests

A manifest is a JSON file that describes how to invoke a tool. Drop it in `~/.clawshake/manifests/` and the broker picks it up automatically — no restart needed.

```json
{
  "version": "1.0",
  "description": "MCP filesystem server",
  "mcp": {
    "transport": "stdio",
    "command": "npx",
    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/dir"]
  }
}
```

Manifests can also define tools directly with different invoke types:

```json
{
  "version": "1.0",
  "tools": [
    {
      "name": "spotify_play",
      "description": "Play a track on Spotify",
      "inputSchema": {
        "type": "object",
        "properties": {
          "query": { "type": "string", "description": "Song or artist name" }
        },
        "required": ["query"]
      },
      "invoke": {
        "type": "deeplink",
        "url": "spotify:search:{{query}}"
      }
    }
  ]
}
```

**Invoke types:** `cli`, `http`, `applescript`, `powershell`, `deeplink`.

See [manifests/](manifests/) for ready-made examples (filesystem, calendar, mail, VS Code, Spotify, and more).

## Permissions

Clawshake is **closed by default** for remote callers. A fresh install blocks all P2P tool calls until you explicitly allow them.

```bash
# Allow a specific peer to call filesystem tools
clawshake permissions allow --agent "p2p:<peer_id>" --tool "list_directory"

# Allow all tools for a peer
clawshake permissions allow --agent "p2p:<peer_id>" --tool "*"

# See current rules
clawshake permissions list
```

The permission waterfall (first match wins):
1. Exact agent + exact tool
2. Exact agent + wildcard (`*`)
3. Agent-class wildcard + exact tool
4. Agent-class wildcard + wildcard
5. **No match → local: ask / remote: deny**

Local callers (same machine) are auto-allowed. Remote callers must be explicitly granted access.

## Network tools

Every clawshake node exposes six built-in tools for peer discovery and cross-machine invocation:

| Tool | What it does |
|------|-------------|
| `network_peers` | List discovered nodes (from local cache) |
| `network_tools` | Fetch a peer's tools live from the DHT |
| `network_search` | Search for tools across peers by name |
| `network_ping` | Check if a peer is connected |
| `network_call` | Invoke a tool on a remote peer |
| `network_record` | Fetch a peer's raw DHT announcement |

These are regular MCP tools — your agent can use them to discover and call tools on other machines without any manual setup.

## Architecture

```
crates/
  clawshake/          Unified binary — runs broker + bridge in one process
  clawshake-broker/   MCP server, manifest loading, permission checks
  clawshake-bridge/   libp2p swarm — Kademlia, relay, mDNS, QUIC/TCP
  clawshake-core/     Shared types — identity, permissions, protocol
  clawshake-tools/    Network tools + IPC between broker and bridge
```

**P2P stack:** libp2p with Kademlia DHT, mDNS, relay + DCUtR (hole punching), QUIC and TCP transports, Noise encryption.

**Identity:** Ed25519 keypair generated on first run, stored at `~/.clawshake/identity.key`. Peer identity is verified cryptographically via the Noise handshake — it cannot be spoofed.

## Running a relay node

To let peers behind NAT reach each other, run a relay on a machine with a public IP:

```bash
clawshake run --relay-server
```

This binds to port 7474 (configurable via `--p2p-port`) and prints a copy-ready multiaddr on startup. Share that address with other users — they add it to their `config.toml` under `bootstrap` to join your network.

## Configuration

Node configuration lives at `~/.clawshake/config.toml`. The file is optional — when absent, the node runs in local-only mode (mDNS discovery on your LAN).

```toml
[network]
bootstrap = [
  "/ip4/43.143.33.106/tcp/7474/p2p/12D3KooWDi1ntKAkUYpHfijLNExUTsirFyofnkEB3yjC8P3EGcY5",
  "/ip4/43.143.33.106/udp/7474/quic-v1/p2p/12D3KooWDi1ntKAkUYpHfijLNExUTsirFyofnkEB3yjC8P3EGcY5",
]
```

A reference config with the public relay address is included at [config.toml](config.toml). Additional `--boot` flags on the CLI are merged with the config file entries.

## CLI reference

```
clawshake run                     Start the daemon (broker + bridge)
clawshake status [--json]         Show node identity and peer count
clawshake tools list [--json]     List registered tools
clawshake tools add <file>        Install a manifest
clawshake tools remove <name>     Remove a manifest
clawshake tools validate <file>   Validate a manifest file
clawshake permissions <subcommand>  Manage access rules
clawshake network <subcommand>    Peer discovery and remote invocation
```

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

at your option.

Manifests in [manifests/](manifests/) are licensed under [MIT](manifests/LICENSE).
