use anyhow::Result;

/// Accept inbound P2P connections and proxy them to the local MCP server.
///
/// Security model (Milestone 2+):
///   Remote agent ──── libp2p ────▶  this proxy  ────▶  local MCP server
///                                   ↑ stamps identity       (unmodified,
///                                   ↑ checks permissions      localhost only)
///                                   ↑ filters tools/list
///
/// The local MCP server never touches the P2P network directly.
/// It only ever receives localhost calls from this proxy.
pub async fn run(_mcp_port: u16) -> Result<()> {
    // TODO(milestone-2): listen for inbound P2P streams, stamp AgentId from
    // the Noise transport pubkey, enforce permissions, proxy onwards.
    Ok(())
}
