use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Top-level manifest file, dropped into ~/.clawshake/manifests/.
///
/// The file stem (e.g. `spotify` from `spotify.json`) serves as the
/// grouping key — there is no explicit `app` field.
///
/// A manifest either contains static tool definitions (`tools`) or points to
/// an external MCP server whose tools are discovered dynamically (`mcp`).
/// Both fields are optional but at least one must be present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// Static tool definitions with explicit invoke configs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// Dynamic tool source: connect to an existing MCP server and import its
    /// tools.  The server's `tools/list` response becomes the tool set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpSource>,
}

/// Configuration for connecting to an external MCP server as a tool source.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpSource {
    /// Spawn the MCP server as a child process and communicate over stdin/stdout.
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// Connect to an MCP server listening on HTTP.
    Http { url: String },
}

/// A single callable tool declared in a manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing the tool's inputs — matches the MCP `inputSchema` field.
    #[serde(default, rename = "inputSchema")]
    pub input_schema: InputSchema,
    /// Optional: if set, the broker returns an error if this value is not configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires: Option<String>,
    pub invoke: InvokeConfig,
}

/// JSON Schema object for a tool's inputs (`inputSchema` in MCP).
///
/// ```json
/// {
///   "type": "object",
///   "properties": {
///     "query": { "type": "string", "description": "Search query or Spotify URI" }
///   },
///   "required": ["query"]
/// }
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InputSchema {
    /// Always "object" for MCP tools.
    #[serde(default = "default_object_type")]
    pub r#type: String,
    /// Property sub-schemas kept as raw JSON so that any fields
    /// (`items`, `enum`, `anyOf`, etc.) are preserved round-trip.
    #[serde(default)]
    pub properties: HashMap<String, Value>,
    /// Names of required properties.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
}

fn default_object_type() -> String {
    "object".to_string()
}

/// How the broker should invoke a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InvokeConfig {
    Deeplink {
        template: String,
    },
    Cli {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        /// When true, route through `cmd /C` (Windows) or `sh -c` (Unix).
        /// Use only when you need shell features: `.cmd`/`.bat` files, pipes,
        /// glob expansion, etc.  Default is direct process spawn, which
        /// handles arguments with spaces, quotes, and JSON safely.
        #[serde(default)]
        shell: bool,
    },
    Http {
        url: String,
        #[serde(default)]
        method: Option<String>,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    AppleScript {
        script: String,
    },
    PowerShell {
        script: String,
    },
    /// Proxy to a running MCP server.  Not written in manifest JSON — this
    /// variant is synthesized at runtime by the watcher when it discovers
    /// tools from an `mcp` source.  `server_key` identifies the running
    /// server in the broker's `McpServerMap`.
    #[serde(skip)]
    Mcp {
        server_key: String,
    },
    /// Code-mode tool (`run_code` / `describe_tools`).  Not written in
    /// manifest JSON — synthesized at runtime by `builtins::seed()` when
    /// Node.js is detected on the PATH.  The broker handles these tools
    /// directly in the router without spawning a child process per call.
    #[serde(skip)]
    CodeMode,
}
