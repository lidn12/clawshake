pub mod cli;
pub mod codemode;
pub mod deeplink;
pub mod http;
pub mod mcp;
pub mod script;

/// Replace every `{{key}}` in `template` with the matching value from `arguments`.
/// Unresolved placeholders (and any immediately preceding `:` or `=` separator)
/// are stripped, making optional parameters work without special manifest logic.
///
/// **No escaping is applied** — this variant is safe only when the result is
/// used as discrete OS-level process arguments (the `shell: false` CLI path).
/// For shell / script contexts, use [`substitute_escaped`] instead.
pub(super) fn substitute(template: &str, arguments: &serde_json::Value) -> String {
    substitute_inner(template, arguments, |s| s.to_string())
}

/// Like [`substitute`] but applies `escape` to every interpolated value before
/// inserting it into the template.  Use this whenever the result will be
/// interpreted by a shell or script runtime.
pub(super) fn substitute_escaped(
    template: &str,
    arguments: &serde_json::Value,
    escape: fn(&str) -> String,
) -> String {
    substitute_inner(template, arguments, escape)
}

fn substitute_inner(
    template: &str,
    arguments: &serde_json::Value,
    escape: impl Fn(&str) -> String,
) -> String {
    let mut result = template.to_string();
    if let Some(obj) = arguments.as_object() {
        for (key, val) in obj {
            let placeholder = format!("{{{{{key}}}}}");
            let raw = match val {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let value = escape(&raw);
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

// ---------------------------------------------------------------------------
// Context-specific escapers
// ---------------------------------------------------------------------------

/// Escape a value for interpolation into a POSIX shell string.
///
/// Wraps the value in single quotes and escapes embedded single quotes via the
/// `'\''` idiom (end quote, escaped literal quote, reopen quote).  Single-quoted
/// strings in POSIX shells interpret **nothing** — no `$`, no backticks, no `!`.
#[cfg(not(windows))]
pub(super) fn escape_shell(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Escape a value for interpolation into a PowerShell `-Command` string.
///
/// Wraps the value in single quotes (verbatim strings in PowerShell) and
/// doubles any embedded single quotes — the only character that needs escaping
/// inside a PowerShell single-quoted string.
pub(super) fn escape_powershell(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Escape a value for interpolation into an AppleScript passed to `osascript -e`.
///
/// Wraps the value in escaped double-quotes (`\"...\"`), escaping `\` and `"`
/// inside the value.  AppleScript string literals use `"` delimiters and only
/// recognise `\\` and `\"` as escape sequences.
#[cfg(target_os = "macos")]
pub(super) fn escape_applescript(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\\\"{}\\\"", escaped)
}
