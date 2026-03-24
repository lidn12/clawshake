//! Model discovery helpers for DHT announcements.
//!
//! `query_models()` queries the local model backend for available models so
//! they can be advertised via the Kademlia DHT.  Inbound and outbound model
//! traffic now flows over TCP tunnels (`network_expose` / `connect_models`)
//! — there is no bespoke model streaming protocol.

use clawshake_core::{
    config::AdvertiseModels,
    models::ModelAnnounce,
};
use clawshake_models::backend::ModelBackend;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Model discovery
// ---------------------------------------------------------------------------

/// Query the model backend for available models, filtered by the advertise
/// configuration.  Returns an empty vec if no model backend is configured.
pub(super) async fn query_models(
    backend: &Option<ModelBackend>,
    advertise: &AdvertiseModels,
) -> Vec<ModelAnnounce> {
    let backend = match backend {
        Some(b) => b,
        None => return Vec::new(),
    };
    if advertise.is_none() {
        return Vec::new();
    }

    let all_models = match backend.list_models().await {
        Ok(m) => m,
        Err(e) => {
            warn!("Failed to query model backend: {e}");
            return Vec::new();
        }
    };

    let result = match advertise {
        AdvertiseModels::All(_) => all_models,
        AdvertiseModels::List(names) => all_models
            .into_iter()
            .filter(|m| names.iter().any(|n| n == &m.name))
            .collect(),
        AdvertiseModels::None(_) => Vec::new(),
    };

    if !result.is_empty() {
        let names: Vec<&str> = result.iter().map(|m| m.name.as_str()).collect();
        info!(count = result.len(), models = ?names, "Advertising models on the network");
    }

    result
}
