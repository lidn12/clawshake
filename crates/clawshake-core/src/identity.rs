/// The stamped identity of a caller, derived from which transport the connection
/// arrived on. The caller has no say in this value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AgentId {
    /// TCP loopback / stdio — physically impossible to spoof from outside the host.
    Local,
    /// libp2p Noise handshake — identity is the verified Ed25519 public key.
    P2p(String),
    /// Tailscale WireGuard interface — identity is the Tailscale-issued node name.
    Tailscale(String),
}

impl AgentId {
    pub fn as_str(&self) -> String {
        match self {
            AgentId::Local => "local".to_string(),
            AgentId::P2p(key) => format!("p2p:{key}"),
            AgentId::Tailscale(node) => format!("tailscale:{node}"),
        }
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}
