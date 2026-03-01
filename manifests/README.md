# Clawshake Tool Manifests

Reference manifests for common applications and services. Copy them to `~/.clawshake/manifests/` to enable the tools on your node.

## Usage

Copy any manifest to your local Clawshake directory:

```bash
cp manifests/spotify.json ~/.clawshake/manifests/
cp manifests/mail.json ~/.clawshake/manifests/
```

The broker watches `~/.clawshake/manifests/` and automatically loads any `.json` file it finds. No restart needed.

## What's Included

| Manifest | Type | Best For |
|----------|------|----------|
| `spotify.json` | DeepLink + AppleScript | Playing music, checking now playing (macOS) |
| `mail.json` | AppleScript | Composing and sending emails (macOS) |
| `calendar.json` | AppleScript | Creating events, checking availability (macOS) |
| `finder.json` | AppleScript | File system operations on macOS |
| `safari.json` | AppleScript | Web browsing automation (macOS) |
| `vscode.json` | CLI | Editing files, opening workspaces (any platform) |
| `filesystem.json` | MCP (stdio) | Bind [Anthropic's filesystem MCP](https://github.com/modelcontextprotocol/servers/tree/main/src/filesystem) to the broker |
| `homeassistant.json` | MCP (stdio) | Control smart home devices, automations, and sensors via [ha-mcp](https://github.com/homeassistant-ai/ha-mcp) (requires Home Assistant) |

## Creating Your Own

Use the examples as templates. A basic manifest looks like:

```json
{
  "version": "1.0",
  "tools": [
    {
      "name": "myapp_action",
      "description": "What this tool does",
      "inputSchema": {
        "type": "object",
        "properties": {
          "param1": { "type": "string", "description": "A parameter" }
        },
        "required": ["param1"]
      },
      "invoke": {
        "type": "cli",
        "command": "myapp",
        "args": ["--do-action", "{{param1}}"]
      }
    }
  ]
}
```

**Invoke types:**
- `cli` — subprocess command with template substitution
- `http` — HTTP request (localhost or remote)
- `deeplink` — deep-link URL (fire-and-forget)
- `applescript` — AppleScript on macOS
- `powershell` — PowerShell on Windows

See the [README](../README.md) for more details.

## Platform Notes

- **AppleScript manifests** (Mail, Calendar, Safari, Finder) require macOS
- **CLI manifests** (VS Code) work anywhere the command is installed
- **MCP manifests** (filesystem, homeassistant) work anywhere the MCP server is installed
- **DeepLink manifests** (Spotify) require the app to support deep links

### Home Assistant setup

`homeassistant.json` uses [ha-mcp](https://github.com/homeassistant-ai/ha-mcp), which discovers all 97 HA tools dynamically at startup (lights, climate, locks, automations, cameras, dashboards, etc.).

**Prerequisites:** [uv](https://docs.astral.sh/uv/getting-started/installation/) installed (`brew install uv` on macOS, `pip install uv` elsewhere).

Set two environment variables before starting Clawshake:

```bash
export HA_URL=http://homeassistant.local:8123   # or your HA IP
export HA_TOKEN=<long-lived-access-token>        # from HA → Profile → Security
```

Then copy the manifest and start the broker:

```bash
cp manifests/homeassistant.json ~/.clawshake/manifests/
clawshake run
```

Remote agents on any machine connected to the Clawshake network can now call all Home Assistant tools — no Nabu Casa subscription, no port forwarding required.

Missing an app? Please create a manifest and [open a PR](https://github.com/lidn12/clawshake/pulls).
