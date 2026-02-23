use anyhow::Result;

/// Invoke a tool via a native scripting host.
///
/// - **Windows**: `powershell.exe -NoProfile -Command <script>`
/// - **macOS**:   `osascript -e <script>`
///
/// Both capture stdout as the return value.
pub async fn invoke(_script: &str) -> Result<String> {
    // TODO(track-2): dispatch to powershell (Windows) or osascript (macOS)
    // via std::process::Command, capture stdout.
    anyhow::bail!("script invoke not yet implemented")
}
