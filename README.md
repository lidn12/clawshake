# Clawshake

[![CI](https://github.com/lidn12/clawshake/actions/workflows/ci.yml/badge.svg)](https://github.com/lidn12/clawshake/actions/workflows/ci.yml)

**Give your AI agent tools that work across machines.**

Clawshake is a lightweight daemon that turns any machine into a node on a peer-to-peer tool network. Agents discover and call tools on remote machines — opening files, searching directories, controlling apps — without cloud servers, port forwarding, or API keys.

Drop a 10-line JSON manifest, and the tool is live on the network. Or wrap any existing MCP server — Clawshake proxies it over P2P so remote agents can call it without any changes. Any MCP-compatible agent can use both.

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

**Invoke types:** `cli`, `http`, `applescript`, `powershell`, `deeplink`, `mcp`.

The `mcp` type is special: instead of defining tools inline, you point it at any existing MCP server process (stdio or SSE). Clawshake spawns it, discovers its tools, and re-exposes them over the P2P network — so a remote agent can call tools from the [Playwright MCP](https://github.com/microsoft/playwright-mcp), the [filesystem server](https://github.com/modelcontextprotocol/servers), or any of the 18,000+ servers in the MCP ecosystem, without those servers knowing anything about P2P.

See [manifests/](manifests/) for ready-made examples (Spotify, Calendar, Mail, Safari, Finder, Messages, Notes, Reminders, Contacts, Apple Music, system controls, and more).

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
| `network_tools` | Progressive tool discovery — summary by default, full schemas with `--query` |
| `network_search` | Search for tools across peers by name |
| `network_ping` | Check if a peer is connected |
| `network_call` | Invoke a tool on a remote peer |
| `network_record` | Fetch a peer's raw DHT announcement |

These are regular MCP tools — your agent can use them to discover and call tools on other machines without any manual setup.

## Code mode

By default the broker exposes every registered tool directly as an MCP tool. With `--code-mode`, the broker instead exposes two meta-tools:

| Tool | What it does |
|------|--------------|
| `describe_tools` | Returns tool names, descriptions, and JS function signatures grouped by category. Accepts an optional query to filter. |
| `run_code` | Executes a JavaScript snippet in Node.js with all local tools pre-loaded as async functions. |

Start in code mode:

```bash
clawshake run --code-mode
```

The agent calls `describe_tools` to discover what is available, then `run_code` to act — keeping multiple tool calls, conditional logic, and state in a single round trip:

```js
// Inside run_code
const peers = await network.peers();
const summary = await network.tools({ peer_id: peers.peers[0].peer_id });
console.log(summary.description);
```

**Why this matters for token cost:** Every MCP session starts with `tools/list`, which serializes the full `inputSchema` for every registered tool. With dozens of tools loaded, this alone can consume a significant chunk of the context budget — before the agent has done any actual work. In code mode, `tools/list` returns just two schemas. The agent pays only for what it looks up via `describe_tools`, and only when it needs it.

The other saving comes from round trips. A multi-step workflow — list a directory, read a file, transform it, write it back — normally requires one tool call per step, each carrying the full prompt context. Inside `run_code`, all of that runs as a single call with intermediate state kept in the JS runtime, not serialized back and forth through the model.

Code mode is most effective for sessions with many tools loaded and workflows with more than one or two steps. For a single one-off call to a well-known tool it adds unnecessary overhead, so the tradeoff is workload-dependent.

> **Security note:** `run_code` executes JavaScript in a Node.js process running as the same OS user as the broker. A peer with access to `run_code` has **equivalent access to a shell** — they can read files, spawn processes, and access the network directly from JavaScript, bypassing per-tool permission rules entirely. Only grant `run_code` access to peers you fully trust. If you need fine-grained tool-level access control for remote peers, do not use code mode on that node.

> This approach was independently validated by [Anthropic](https://www.anthropic.com/engineering/code-execution-with-mcp) (measuring up to 98.7% token reduction on real workflows) and [Cloudflare](https://blog.cloudflare.com/code-mode/) ("LLMs are better at writing code to call MCP, than at calling MCP directly"). Clawshake's implementation follows the same pattern.

## Model proxy

Clawshake can share a local AI model (Ollama, vLLM, llama.cpp, or any OpenAI-compatible server) across machines over the same P2P network. A laptop with no GPU can transparently call a model running on a desktop with one.

**On the machine with the model**, add a `[models]` section to `~/.clawshake/config.toml`:

```toml
[models]
endpoint = "http://127.0.0.1:11434"   # your local model server
# advertise = "all"                   # share all models (default)
# advertise = ["llama3.1:70b"]        # or share specific ones
# proxy_port = 11435                  # local proxy port (default)
```

Restart `clawshake run`. The node now advertises its models on the DHT and starts an OpenAI-compatible proxy on port 11435.

**On any other machine**, also add the `[models]` section (the `endpoint` can be absent if the machine has no local model — it will still be able to call models on peers):

```toml
[models]
# No endpoint needed to use other peers' models
proxy_port = 11435
```

Then point any OpenAI SDK at the local proxy:

```bash
export OPENAI_BASE_URL=http://127.0.0.1:11435/v1
export OPENAI_API_KEY=ignored
```

Models are addressed as `model_name@peer_id`. Use the `network_models` MCP tool to discover what is available:

```js
// Inside run_code
const models = await network.models({});
// → [{ id: "qwen2.5:7b@12D3KooW...", owned_by: "ollama", ... }]
```

Or use the `/v1/models` endpoint directly:

```bash
curl http://127.0.0.1:11435/v1/models
```

Both streaming (`stream: true`, SSE) and non-streaming requests are supported. Streaming is carried over a dedicated libp2p stream (`/clawshake/models/stream/1.0.0`) — tokens arrive in real time rather than buffered until the peer finishes generating.

## Architecture

```
crates/
  clawshake/          Unified binary — runs broker + bridge in one process
  clawshake-broker/   MCP server, manifest loading, permission checks
  clawshake-bridge/   libp2p swarm — Kademlia, relay, mDNS, QUIC/TCP
  clawshake-core/     Shared types — identity, permissions, protocol, config
  clawshake-models/   Model proxy — OpenAI-compatible HTTP server + P2P streaming backend
  clawshake-tools/    Network tools + IPC between broker and bridge
```

**P2P stack:** libp2p 0.56 with Kademlia DHT, mDNS, relay + DCUtR (hole punching), QUIC and TCP transports, Noise encryption, and libp2p-stream for real-time bidirectional streaming.

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
  # Virginia (US East)
  "/ip4/43.166.241.142/tcp/7474/p2p/12D3KooWPfjVhJYqj2V1cJqNTzzFnG19LN1ggKpTX6fdw2SKV3vP",
  "/ip4/43.166.241.142/udp/7474/quic-v1/p2p/12D3KooWPfjVhJYqj2V1cJqNTzzFnG19LN1ggKpTX6fdw2SKV3vP",
  # Tokyo (Asia)
  "/ip4/43.167.199.188/tcp/7474/p2p/12D3KooWDRtC9K43jkTpgc6urj9Y85fy8oKe1ijNE7YGuuTbMe1m",
  "/ip4/43.167.199.188/udp/7474/quic-v1/p2p/12D3KooWDRtC9K43jkTpgc6urj9Y85fy8oKe1ijNE7YGuuTbMe1m",
]
```

A reference config with the public relay address is included at [config.toml](config.toml). Additional `--boot` flags on the CLI are merged with the config file entries.

## CLI reference

```
clawshake run                     Start the daemon (broker + bridge)
clawshake run --code-mode         Start in code mode (run_code + describe_tools only)
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
