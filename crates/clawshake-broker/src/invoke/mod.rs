pub mod cli;
pub mod codemode;
pub mod deeplink;
pub mod events;
pub mod http;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn substitute_basic() {
        assert_eq!(
            substitute("echo {{name}}", &json!({"name": "world"})),
            "echo world"
        );
    }

    #[test]
    fn substitute_multiple_params() {
        assert_eq!(
            substitute("{{a}} and {{b}}", &json!({"a": "1", "b": "2"})),
            "1 and 2"
        );
    }

    #[test]
    fn substitute_missing_param_stripped() {
        // Unresolved placeholder and its text are removed; a preceding plain
        // space is NOT a separator so it stays, leaving a trailing space.
        assert_eq!(substitute("hello {{name}}", &json!({})), "hello ");
    }

    #[test]
    fn substitute_missing_with_separator() {
        assert_eq!(substitute("--flag={{value}}", &json!({})), "--flag");
        assert_eq!(substitute("--flag:{{value}}", &json!({})), "--flag");
    }

    #[test]
    fn substitute_separator_kept_when_value_resolves() {
        // The separator must NOT be stripped when the placeholder has a value.
        // A bug that unconditionally strips the char before `{{...}}` would
        // produce "--flagx" or "--flagalice" instead of the correct form.
        assert_eq!(
            substitute("--flag={{value}}", &json!({"value": "x"})),
            "--flag=x"
        );
        assert_eq!(
            substitute("--flag:{{value}}", &json!({"value": "alice"})),
            "--flag:alice"
        );
        // Plain space before placeholder: space is not a separator, so it stays
        // whether the value is present or absent.
        assert_eq!(
            substitute("prefix {{value}}", &json!({"value": "ok"})),
            "prefix ok"
        );
    }

    #[test]
    fn substitute_explicit_empty_value_keeps_separator() {
        // An explicitly-provided empty string is substituted in place
        // (the placeholder is replaced with ""), so the separator is NOT
        // stripped — that only happens for *unresolved* (missing) keys.
        // A bug that stripped the separator unconditionally would produce
        // "--flag" here instead of "--flag=".
        assert_eq!(substitute("--flag={{val}}", &json!({"val": ""})), "--flag=");
        // Plain positional arg: empty string stays, it is up to the caller
        // (cli.rs) to drop empty args — not substitute's job.
        assert_eq!(substitute("{{val}}", &json!({"val": ""})), "");
    }

    #[test]
    fn substitute_non_string_value() {
        assert_eq!(substitute("count={{n}}", &json!({"n": 42})), "count=42");
        assert_eq!(substitute("flag={{b}}", &json!({"b": true})), "flag=true");
    }

    #[test]
    fn substitute_escaped_powershell() {
        // escape_powershell wraps in single quotes and doubles embedded quotes.
        let result = substitute_escaped("{{msg}}", &json!({"msg": "it's here"}), escape_powershell);
        assert_eq!(result, "'it''s here'");
    }

    #[test]
    #[cfg(not(windows))]
    fn substitute_escaped_shell() {
        // escape_shell wraps in single quotes; metacharacters are neutralised.
        let result = substitute_escaped(
            "{{msg}}",
            &json!({"msg": "hello world; rm -rf /"}),
            escape_shell,
        );
        assert_eq!(result, "'hello world; rm -rf /'");
    }

    #[test]
    fn substitute_partial_resolution_strips_separator_of_missing() {
        // When only *some* placeholders in a template are resolved, the
        // separator that precedes an *unresolved* placeholder is still
        // stripped.  A naive implementation might leave ":{{b}}" or ":" in
        // the output.
        assert_eq!(
            substitute("{{a}}:{{b}}", &json!({"a": "x"})),
            "x",
            "separator before unresolved {{b}} must be stripped"
        );
        // Both resolved → both separators kept.
        assert_eq!(
            substitute("{{a}}:{{b}}", &json!({"a": "x", "b": "y"})),
            "x:y"
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn escape_shell_embedded_single_quote() {
        // An embedded ' must survive round-trip via the '\'' idiom.
        let escaped = escape_shell("it's");
        assert_eq!(escaped, "'it'\\''s'");
    }

    #[test]
    fn escape_powershell_embedded_single_quote() {
        let escaped = escape_powershell("say 'hello'");
        assert_eq!(escaped, "'say ''hello'''");
    }
}
