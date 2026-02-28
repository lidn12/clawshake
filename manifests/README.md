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

See [../docs/clawshake.md](../docs/clawshake.md#manifest-format) for full format details.

## Platform Notes

- **AppleScript manifests** (Mail, Calendar, Safari, Finder) require macOS
- **HTTP manifests** (VS Code) work on any platform
- **CLI manifests** work anywhere the command is installed
- **DeepLink manifests** (Spotify) require the app to support deep links

Missing an app? Please create a manifest and [open a PR](https://github.com/lidn12/clawshake/pulls).
