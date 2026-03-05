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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_as_str() {
        assert_eq!(AgentId::Local.as_str(), "local");
        assert_eq!(AgentId::P2p("abc".into()).as_str(), "p2p:abc");
        assert_eq!(AgentId::Tailscale("mynode".into()).as_str(), "tailscale:mynode");
    }

    #[test]
    fn agent_id_display() {
        assert_eq!(format!("{}", AgentId::Local), "local");
        assert_eq!(format!("{}", AgentId::P2p("abc".into())), "p2p:abc");
        assert_eq!(format!("{}", AgentId::Tailscale("mynode".into())), "tailscale:mynode");
    }

    #[test]
    fn agent_id_equality() {
        assert_eq!(AgentId::Local, AgentId::Local);
        assert_ne!(AgentId::P2p("a".into()), AgentId::P2p("b".into()));
        assert_ne!(AgentId::P2p("a".into()), AgentId::Local);
        assert_eq!(AgentId::P2p("x".into()), AgentId::P2p("x".into()));
        assert_ne!(AgentId::Tailscale("n".into()), AgentId::Local);
    }
}
