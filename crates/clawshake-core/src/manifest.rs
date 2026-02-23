use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level manifest file — one per app, dropped into ~/.clawshake/manifests/.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub app: String,
    pub version: String,
    pub tools: Vec<Tool>,
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
    #[serde(default)]
    pub properties: HashMap<String, PropertySchema>,
    /// Names of required properties.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
}

fn default_object_type() -> String {
    "object".to_string()
}

/// Schema for a single property within `inputSchema.properties`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertySchema {
    #[serde(rename = "type")]
    pub param_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
}

impl Manifest {
    /// Returns the qualified MCP tool name: `{app}.{tool.name}`.
    pub fn qualified_name(app: &str, tool_name: &str) -> String {
        format!("{app}.{tool_name}")
    }
}
