/// MCP stdio server surface.
///
/// Exposes tools/list and tools/call over JSON-RPC 2.0 on stdio (or TCP
/// port 7474 for local agent connections).
///
/// Implemented in Track 2.
pub async fn serve(_port: u16) -> anyhow::Result<()> {
    // TODO(track-2): read JSON-RPC requests from stdin (or TCP), dispatch
    // to the manifest registry + invoke router, write responses to stdout.
    Ok(())
}
