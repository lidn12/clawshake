pub mod cli;
pub mod deeplink;
pub mod http;
pub mod mcp;
pub mod script;

/// Replace every `{{key}}` in `template` with the matching value from `arguments`.
/// Unresolved placeholders (and any immediately preceding `:` or `=` separator)
/// are stripped, making optional parameters work without special manifest logic.
pub(super) fn substitute(template: &str, arguments: &serde_json::Value) -> String {
    let mut result = template.to_string();
    if let Some(obj) = arguments.as_object() {
        for (key, val) in obj {
            let placeholder = format!("{{{{{key}}}}}");
            let value = match val {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            result = result.replace(&placeholder, &value);
        }
    }
    // Strip remaining unresolved placeholders (with optional preceding separator).
    loop {
        let Some(start) = result.find("{{") else {
            break;
        };
        let prefix_start = if start > 0 && matches!(result.as_bytes()[start - 1], b':' | b'=') {
            start - 1
        } else {
            start
        };
        let Some(rel_end) = result[start..].find("}}") else {
            break;
        };
        result.drain(prefix_start..start + rel_end + 2);
    }
    result
}
