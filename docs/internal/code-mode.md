# Code Mode — Design Document

**Status:** Planned  
**Author:** Internal  
**Date:** March 2, 2026  
**References:**
- [Cloudflare — Code Mode: the better way to use MCP](https://blog.cloudflare.com/code-mode/)
- [Anthropic — Code execution with MCP](https://www.anthropic.com/engineering/code-execution-with-mcp)
- [Simon Willison — Commentary](https://simonwillison.net/2025/Nov/4/code-execution-with-mcp/)

---

## Problem

As Clawshake nodes accumulate tools (macOS apps, MCP servers like Playwright and Home Assistant, network tools), the tool list grows quickly. A node running 5 manifests can easily expose 40–120 tools. This creates two compounding costs:

1. **Definition bloat.** Every tool definition (~200 tokens each) is loaded into the agent's context window before the first interaction. 40 tools ≈ 8,000 tokens. 120 tools ≈ 24,000 tokens. This is a substantial chunk of context consumed before the agent does anything useful.

2. **Intermediate round-trips.** In a multi-step workflow (e.g., fetch document → create calendar event → send email), each tool result passes through the model's context window just to be copied into the next call's arguments. Large payloads like transcripts or screenshots multiply this cost.

Anthropic measured a **98.7% token reduction** (150k → 2k) on a real workflow using the code execution approach. Cloudflare independently arrived at the same conclusion: "LLMs are better at writing code to call MCP, than at calling MCP directly."

---

## Solution

Expose a `run_code` tool that accepts JavaScript. The agent writes code that calls tools as functions. Intermediate results stay in the JS runtime and never touch the model's context. Only `console.log()` output returns.

This is fully MCP-compliant — `run_code` is a standard tool served via `tools/list` and invoked via `tools/call`.

---

## Architecture

```
Agent                    Broker (Rust/axum)              Node.js subprocess
  │                          │                                │
  ├─ tools/call run_code ──► │                                │
  │   {script: "..."}        │                                │
  │                          ├─ generate JS shim              │
  │                          ├─ spawn node -, pipe via stdin ──►
  │                          │                                │
  │                          │   ◄── POST /invoke ────────────┤ tool call
  │                          │   ──► JSON result ────────────►│
  │                          │   ◄── POST /invoke ────────────┤ another
  │                          │   ──► JSON result ────────────►│
  │                          │                                │
  │                          │   ◄── process exit ────────────┤
  │                          ├─ collect stdout                │
  │  ◄── result (stdout) ───┤                                │
```

### Runtime choice: Node.js subprocess

Node.js was chosen over embedded alternatives (deno_core, boa, rquickjs) because:

- **Agent code quality.** LLMs write the best Node.js — deepest training data coverage of any JS runtime. QuickJS/boa incompatibilities would cause silent failures.
- **Already a soft dependency.** MCP manifests using `npx` (filesystem, playwright) already require Node.js. No new dependency for most users.
- **Zero new Rust deps.** No impact on compile time or binary size.
- **~30ms startup overhead.** Negligible compared to model inference latency.
- **Stdin pipe delivery.** Script is piped to `node -` via stdin, avoiding both the Windows 32k command-line limit (`node -e`) and temp file overhead. Direct memory-to-pipe transfer — no filesystem I/O, no cleanup.
- **Upgrade path.** Can swap to `deno run --allow-net=127.0.0.1:PORT` later for sandboxing with zero architecture changes.

---

## Components

### 1. `POST /invoke` endpoint

New HTTP endpoint on the existing axum server. Synchronous REST wrapper around `router::dispatch`.

```
POST /invoke
Content-Type: application/json

{"tool": "mail_send", "arguments": {"to": "...", "subject": "...", "body": "..."}}

→ 200 {"result": "Email sent", "is_error": false}
```

This is the callback target for the Node.js subprocess. Unlike `/messages` (SSE-based, async), this blocks until the tool completes and returns the result directly.

**File:** `crates/clawshake-broker/src/http_server.rs` — new route on existing router.

### 2. JS shim generator

Reads the `ManifestRegistry` and generates JavaScript with:
- A `_call(name, args)` helper that `fetch()`es `POST /invoke` on the broker
- Tool functions grouped by manifest source (e.g., `mail.send()`, `calendar.create()`)
- JSDoc comments carrying each tool's description and parameter types from `inputSchema`
- Remote peer tools routed transparently through `network_call`

Example output:
```javascript
const _BROKER = 'http://127.0.0.1:8910';
async function _call(name, args) {
  const r = await fetch(`${_BROKER}/invoke`, {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({tool: name, arguments: args || {}})
  });
  const j = await r.json();
  if (j.is_error) throw new Error(j.result);
  return JSON.parse(j.result);
}

const mail = {
  /** Compose and send an email via Apple Mail.
   *  @param {object} args
   *  @param {string} args.to - Recipient email address
   *  @param {string} args.subject - Email subject line
   *  @param {string} args.body - Email body text */
  send: (args) => _call('mail_send', args),
};

const calendar = {
  /** Create a new calendar event.
   *  @param {object} args
   *  @param {string} args.title - Event title
   *  @param {string} args.date - ISO 8601 date */
  create: (args) => _call('calendar_create', args),
};
```

The generator can produce either the full shim or a filtered subset based on a search query.

**File:** `crates/clawshake-broker/src/invoke/codemode.rs` (NEW)

### 3. `run_code` handler

Invocation flow:
1. Receive `script` from `tools/call` arguments
2. Generate JS shim for all registered tools (cached, regenerated on registry change)
3. Wrap: `<shim>\n;(async () => {\n<agent_script>\n})()`
4. Spawn `node -` with stdin piped; write combined script to stdin, then close
5. Apply timeout (configurable, default 30s)
6. Capture stdout → return as tool result
7. On non-zero exit or timeout → return stderr as error result

Stdin pipe approach:
```rust
let mut child = Command::new("node")
    .arg("-")           // read script from stdin
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;

child.stdin.take().unwrap().write_all(script.as_bytes()).await?;
// stdout/stderr collected after exit
```

This avoids the Windows 32k command-line length limit, skips filesystem I/O entirely (no temp files), and works identically on Windows/macOS/Linux.

**File:** `crates/clawshake-broker/src/invoke/codemode.rs`

### 4. `describe_tools` tool (progressive discovery)

Enables the agent to discover tools on-demand without loading all definitions upfront.

```json
{
  "name": "describe_tools",
  "description": "Search available tools and return their JavaScript API definitions for use with run_code. Call with no query to list all tool categories. Call with a query to get specific tool function signatures.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "query": {
        "type": "string",
        "description": "Filter tools by name or description substring. Omit for category summary."
      }
    }
  }
}
```

**Without query** — returns a compact category summary (~200 tokens):
```
Available tool categories:
- mail (3 tools): send, read, search
- calendar (4 tools): create, list, update, delete
- home_assistant (97 tools): lights, climate, locks, automations, ...
- browser (20 tools): navigate, click, type, screenshot, ...
- network (6 tools): peers, tools, search, ping, call, record
```

**With query** — returns the JS shim for matching tools only (~200–500 tokens):
```javascript
const mail = {
  /** Compose and send an email. @param {to, subject, body} */
  send: (args) => _call('mail_send', args),
  /** Search mail by query. @param {query, mailbox?} */
  search: (args) => _call('mail_search', args),
};
```

**File:** `crates/clawshake-broker/src/invoke/codemode.rs`

### 5. Builtin registration

Both tools are added to `builtins.rs` alongside the existing `network.json` builtin. They are seeded to `~/.clawshake/manifests/codemode.json` on first run.

**File:** `crates/clawshake-broker/src/builtins.rs`

### 6. Code mode toggle

A `--code-mode` CLI flag or `tools.code_mode = true` in `~/.clawshake/config.toml` controls `tools/list` behavior:

| Mode | `tools/list` returns | `run_code` available | Individual tools callable |
|------|---------------------|---------------------|--------------------------|
| OFF (default) | All tools + `run_code` + `describe_tools` | Yes | Yes |
| ON | `run_code` + `describe_tools` only | Yes | Yes (still callable by name, just not listed) |

Future: auto-detect Node.js on PATH at startup. If found, enable code mode automatically with log message. If not found, stay off and do not register `run_code`/`describe_tools`.

**Files:** `crates/clawshake-broker/src/mcp_server.rs` (tools/list filtering), broker CLI args.

---

## Token Budget Comparison

### Standard mode — "Send email and create event"
```
tools/list:           40 tools × ~200 tokens  =  8,000 tokens
tools/call mail_send: request                 =    200 tokens
  result in context:                          =     50 tokens
tools/call calendar_create: request           =    200 tokens
  result in context:                          =     50 tokens
                                        Total ≈  8,500 tokens
```

### Code mode — same task
```
tools/list:           2 tools                 =    500 tokens
describe_tools:       request                 =     50 tokens
  category summary result:                    =    200 tokens
describe_tools(mail,calendar): request        =     80 tokens
  JS shim result:                             =    400 tokens
run_code: request (script)                    =    200 tokens
  result: "done"                              =     10 tokens
                                        Total ≈  1,440 tokens

Savings: ~83%
```

Savings increase with more tools and longer chains. The Anthropic case (150k → 2k, 98.7%) involved larger payloads flowing between steps.

---

## Agent Experience

```
User: "Download the meeting transcript from my notes, create a calendar
       follow-up for next Tuesday, and email the summary to alice@co.com"

Agent thinks: This requires notes, calendar, and mail. Let me discover those tools.

→ tools/call describe_tools({})
← "mail (3 tools), calendar (4 tools), notes (3 tools), ..."

→ tools/call describe_tools({query: "notes mail calendar"})
← JS shim for notes.search, notes.read, mail.send, calendar.create

→ tools/call run_code({script: `
  const results = await notes.search({query: "meeting transcript"});
  const note = await notes.read({title: results[0].title});
  
  await calendar.create({
    title: "Follow-up: " + note.title,
    date: "2026-03-10T10:00:00",
    notes: note.body.substring(0, 200)
  });
  
  await mail.send({
    to: "alice@co.com",
    subject: "Meeting summary: " + note.title,
    body: note.body
  });
  
  console.log("Created follow-up event and sent summary email");
`})
← "Created follow-up event and sent summary email"
```

The full note body (potentially thousands of tokens) never enters model context. It flows directly from `notes.read` → `mail.send` inside the Node.js process.

---

## Decisions

All open questions resolved on March 2, 2026.

| # | Question | Decision |
|---|----------|----------|
| 1 | Permission model | **Per-tool checks inside the script.** Each `POST /invoke` goes through the same permission check as a normal `tools/call`. Permission denied → JS exception → stderr → error returned to agent. Consistent with existing model. |
| 2 | Timeout | **30 seconds default, configurable** via `broker.code_mode_timeout_secs`. |
| 3 | Output semantics | **`console.log()` only.** No magic return values. Matches Cloudflare's design. |
| 4 | Error reporting | **stderr + exit code → `is_error: true`** in MCP response. Full JS stack trace returned so agent can fix and retry. |
| 5 | Remote peer tools | **Include in shim**, auto-generated as proxy functions routed via `network_call`. Remote tools grouped by peer ID. Local and remote tools look identical in code. Shim reads `PeerTable` at generation time; if no peers discovered, remote section is empty. |
| 6 | Shim caching | **Cache in memory, invalidate on registry change.** Watcher already fires change events. |
| 7 | Node.js absence | **Don't register code mode tools.** Log: `"Node.js not found — code mode unavailable. Install Node.js 18+ to enable."` All other tools work normally. |
| 8 | Security (fs/network) | **Accept for v1.** Agent is trusted; tools already have full local access. Future: Deno sandbox option. |
| 9 | TypeScript | **JS only for v1.** JSDoc in shim provides type hints. TypeScript via `tsx` is a future enhancement. |
| 10 | DHT announcement | **Always announce all tools** regardless of code mode. DHT is for peer discovery; code mode is a local presentation concern. |

---

## File Changes

| File | Change |
|------|--------|
| `crates/clawshake-broker/src/invoke/mod.rs` | Add `pub mod codemode;` |
| `crates/clawshake-broker/src/invoke/codemode.rs` | **NEW** — shim generator, `invoke_codemode()`, `describe_tools()` |
| `crates/clawshake-broker/src/http_server.rs` | Add `POST /invoke` route |
| `crates/clawshake-broker/src/router.rs` | Add `InvokeConfig::InProcess` dispatch arm |
| `crates/clawshake-core/src/manifest.rs` | Add `InvokeConfig::InProcess` variant |
| `crates/clawshake-broker/src/builtins.rs` | Add `run_code` + `describe_tools` builtins |
| `crates/clawshake-broker/src/mcp_server.rs` | Code mode toggle on `tools/list` |
| `crates/clawshake-broker/Cargo.toml` | Add `which = { workspace = true }` (already in workspace) |
| Broker CLI / config | `--code-mode` flag, `broker.code_mode_timeout_secs` |

### Dependencies

No new crates added to the workspace. The only change is exposing `which` (already a workspace dependency used by other crates) to `clawshake-broker` for Node.js detection at startup. All other functionality uses existing deps:

| Need | Provided by |
|------|-------------|
| Subprocess spawn + stdin pipe | `tokio::process::Command` (tokio `full`) |
| 30s timeout | `tokio::time::timeout` |
| HTTP callback endpoint | `axum` |
| JSON serialization | `serde_json` |
| Detect `node` on PATH | `which` |
| Shim string building | `std::fmt::Write` |

---

## Implementation Order

1. `POST /invoke` endpoint — standalone, testable independently
2. Shim generator — core logic, generates JS from registry
3. `run_code` handler — subprocess spawn, shim + script combiner, timeout
4. `describe_tools` handler — filtered shim generation, category summary
5. Builtins — wire both tools into builtin manifests
6. Code mode toggle — `--code-mode` flag on `tools/list`
7. Node.js detection — auto-disable if `node` not on PATH
8. End-to-end test — agent writes code, tools execute, results return

Estimated effort: 2–3 days for steps 1–5, half day for 6–7, half day for 8.

---

## Future Enhancements (post-launch)

- **Auto-enable code mode** when Node.js detected on PATH
- **Deno sandboxing** option via `--sandbox` flag
- **Skill persistence** — agent saves working scripts to `~/.clawshake/skills/` for reuse
- **Streaming output** — pipe stdout back to agent in real-time for long-running scripts
- **TypeScript support** via `tsx` if detected

---

## Token Tracking (independent of code mode)

A lightweight token usage tracker built into the broker. Works in both standard and code mode, giving users visibility into how much context their tool interactions consume.

### What to track

Every MCP message passes through `mcp_server::handle()`. Instrument it to record:

| Metric | Source | Estimate |
|--------|--------|----------|
| `tools_list_tokens` | `tools/list` response payload size | `bytes / 4` |
| `tools_call_input_tokens` | `tools/call` request params size | `bytes / 4` |
| `tools_call_output_tokens` | `tools/call` result payload size | `bytes / 4` |
| `tool_calls_count` | Number of `tools/call` requests | exact |
| `session_total_tokens` | Sum of all above per session | `bytes / 4` |

The `bytes / 4` heuristic is standard for English/JSON text and close enough for comparative use (standard vs code mode).

### Storage

In-memory `Arc<RwLock<SessionMetrics>>` keyed by session ID (SSE sessions already have UUIDs, stdio gets a fixed "local" key). No persistence needed — metrics reset on broker restart.

```rust
struct SessionMetrics {
    tools_list_tokens: u64,
    tools_call_input_tokens: u64,
    tools_call_output_tokens: u64,
    tool_calls_count: u64,
}
```

### Exposure

Three ways to surface the data:

**1. CLI command**
```bash
clawshake metrics
# Session: local
#   tools/list tokens:  8,240
#   tools/call input:   1,450
#   tools/call output:  3,200
#   Total tokens:      12,890
#   Tool calls:            14
```

**2. HTTP endpoint**
```
GET /metrics → JSON
{
  "sessions": {
    "local": {
      "tools_list_tokens": 8240,
      "tools_call_input_tokens": 1450,
      "tools_call_output_tokens": 3200,
      "total_tokens": 12890,
      "tool_calls": 14
    }
  }
}
```

**3. Log line on session close**
```
INFO  Session local: 12,890 tokens estimated (14 tool calls)
```

### Code mode comparison

When code mode is enabled, the metrics naturally show the difference:
- `tools_list_tokens` drops from ~8,000 to ~500
- `tool_calls_count` drops from N to 2-3 (describe_tools + run_code)
- `tools_call_output_tokens` drops dramatically (intermediate results stay in Node.js)

Users see the savings directly without any synthetic benchmarking.

### Implementation

~50 lines of Rust. No new dependencies. Instrument `mcp_server::handle()` to measure request/response sizes and increment counters. Add a `/metrics` route to the existing axum server. Add a `clawshake metrics` subcommand to `clawshake-tools`.

### File changes

| File | Change |
|------|--------|
| `crates/clawshake-broker/src/mcp_server.rs` | Measure request/response sizes, update counters |
| `crates/clawshake-broker/src/http_server.rs` | Add `GET /metrics` route, pass metrics to AppState |
| `crates/clawshake-tools/src/main.rs` | Add `metrics` subcommand |

