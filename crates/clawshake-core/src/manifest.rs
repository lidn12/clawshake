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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Default for InputSchema {
    fn default() -> Self {
        Self {
            r#type: default_object_type(),
            properties: HashMap::new(),
            required: Vec::new(),
        }
    }
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
    /// An in-process tool handled directly by the broker router without
    /// spawning a child process.  Not serialised — constructed in Rust code
    /// by `builtins::register()` at startup.
    /// Used for built-in tools like `emit`, `listen`, `run_code`,
    /// `describe_tools`, and all `network_*` tools.
    #[serde(skip)]
    InProcess,
}

impl InvokeConfig {
    /// Returns `true` for built-in tools handled in-process by the broker.
    pub fn is_in_process(&self) -> bool {
        matches!(self, InvokeConfig::InProcess)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_static_manifest() {
        let json = r#"{"version":"1","tools":[{"name":"test_tool","description":"A test","inputSchema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]},"invoke":{"type":"cli","command":"echo","args":["{{query}}"]}}]}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.version, "1");
        assert_eq!(m.tools.len(), 1);
        let tool = &m.tools[0];
        assert_eq!(tool.name, "test_tool");
        assert_eq!(tool.description, "A test");
        assert!(tool.input_schema.properties.contains_key("query"));
        assert_eq!(tool.input_schema.required, vec!["query"]);
        match &tool.invoke {
            InvokeConfig::Cli {
                command,
                args,
                shell,
            } => {
                assert_eq!(command, "echo");
                assert_eq!(args, &["{{query}}"]);
                assert!(!shell);
            }
            other => panic!("expected Cli, got {other:?}"),
        }
    }

    #[test]
    fn parse_mcp_source_stdio() {
        let json = r#"{"version":"1","mcp":{"transport":"stdio","command":"npx","args":["-y","some-server"]}}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert!(m.tools.is_empty());
        match m.mcp.as_ref().unwrap() {
            McpSource::Stdio { command, args } => {
                assert_eq!(command, "npx");
                assert_eq!(args, &["-y", "some-server"]);
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn parse_mcp_source_http() {
        let json =
            r#"{"version":"1","mcp":{"transport":"http","url":"http://localhost:3000/sse"}}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        match m.mcp.as_ref().unwrap() {
            McpSource::Http { url } => assert_eq!(url, "http://localhost:3000/sse"),
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn parse_all_invoke_types() {
        let json = r#"{"version":"1","tools":[
            {"name":"a","description":"","invoke":{"type":"cli","command":"echo"}},
            {"name":"b","description":"","invoke":{"type":"http","url":"https://example.com"}},
            {"name":"c","description":"","invoke":{"type":"deeplink","template":"myapp://{{q}}"}},
            {"name":"d","description":"","invoke":{"type":"apple_script","script":"tell app\"X\""}},
            {"name":"e","description":"","invoke":{"type":"power_shell","script":"Write-Output hi"}}
        ]}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.tools.len(), 5);
        assert!(matches!(m.tools[0].invoke, InvokeConfig::Cli { .. }));
        assert!(matches!(m.tools[1].invoke, InvokeConfig::Http { .. }));
        assert!(matches!(m.tools[2].invoke, InvokeConfig::Deeplink { .. }));
        assert!(matches!(
            m.tools[3].invoke,
            InvokeConfig::AppleScript { .. }
        ));
        assert!(matches!(m.tools[4].invoke, InvokeConfig::PowerShell { .. }));
    }

    #[test]
    fn parse_http_invoke_with_headers() {
        let json = r#"{"version":"1","tools":[{"name":"x","description":"",
            "invoke":{"type":"http","url":"https://api.example.com/{{action}}",
            "method":"DELETE","headers":{"Authorization":"Bearer token123"}}}]}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        match &m.tools[0].invoke {
            InvokeConfig::Http {
                method, headers, ..
            } => {
                assert_eq!(method.as_deref(), Some("DELETE"));
                assert_eq!(
                    headers.get("Authorization").map(|s| s.as_str()),
                    Some("Bearer token123")
                );
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn input_schema_defaults() {
        let json = r#"{"version":"1","tools":[{"name":"t","description":"","invoke":{"type":"cli","command":"x"}}]}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        let schema = &m.tools[0].input_schema;
        assert_eq!(schema.r#type, "object");
        assert!(schema.properties.is_empty());
        assert!(schema.required.is_empty());
    }

    #[test]
    fn parse_cli_shell_flag() {
        let with_shell = r#"{"version":"1","tools":[{"name":"t","description":"","invoke":{"type":"cli","command":"x","shell":true}}]}"#;
        let without_shell = r#"{"version":"1","tools":[{"name":"t","description":"","invoke":{"type":"cli","command":"x"}}]}"#;

        let m1: Manifest = serde_json::from_str(with_shell).unwrap();
        let m2: Manifest = serde_json::from_str(without_shell).unwrap();

        match &m1.tools[0].invoke {
            InvokeConfig::Cli { shell, .. } => assert!(shell),
            _ => panic!("expected Cli"),
        }
        match &m2.tools[0].invoke {
            InvokeConfig::Cli { shell, .. } => assert!(!shell),
            _ => panic!("expected Cli"),
        }
    }
}
