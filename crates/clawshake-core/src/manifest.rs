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
    #[serde(default)]
    pub parameters: HashMap<String, ToolParameter>,
    /// Optional: if set, the broker returns an error if this value is not configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires: Option<String>,
    pub invoke: InvokeConfig,
}

/// A parameter schema for a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolParameter {
    #[serde(rename = "type")]
    pub param_type: String,
    pub description: String,
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
