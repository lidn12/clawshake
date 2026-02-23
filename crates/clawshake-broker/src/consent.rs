/// OS-level consent dialog shown before a tool call is executed for the
/// first time by a caller without an explicit permission record.
///
/// Local callers: show a native dialog and await user approval.
/// Remote callers: auto-deny (no dialog).
///
/// Implemented in Track 2.
pub async fn ask(_tool_name: &str, _agent_id: &str) -> bool {
    // TODO(track-2): show OS native consent dialog
    // Windows: MessageBoxW
    // macOS: NSAlert via osascript
    false
}
