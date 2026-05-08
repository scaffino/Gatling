use alto_types::Evaluation;
use commonware_cryptography::{
    bls12381::primitives::{group, poly::Poly},
    ed25519::PublicKey,
};

mod actor;
pub use actor::Actor;
pub use actor::run_buffer;
mod ingress;
pub use ingress::{FinalizationPusher, Mailbox};
pub mod mempool;

use std::sync::{Arc, Mutex, atomic::AtomicU64};

/// Configuration for the application.
pub struct Config {
    /// Participants active in consensus.
    pub participants: Vec<PublicKey>,

    /// The unevaluated group polynomial associated with the current dealing.
    pub polynomial: Poly<Evaluation>,

    /// The share of the secret.
    pub share: group::Share,

    /// Number of messages from consensus to hold in our backlog
    /// before blocking.
    pub mailbox_size: usize,

    /// Identifier for the engine instance (for logging/telemetry).
    pub engine_id: String,
    
    /// Public key of this validator (for determining validator index).
    pub public_key: PublicKey,
    
    /// Time offset within each second (in milliseconds, 0-999) for when this engine should send proposals.
    /// For example: 0 means X.000s, 500 means X.500s. This allows staggering multiple consensus instances.
    pub proposal_offset_ms: u64,
    
    /// Channel to send finalized blocks to the gatling thread (if enabled).
    pub gatling_tx: Option<std::sync::mpsc::Sender<crate::engine::GatlingEvent>>,

    /// Per-instance channel to buffer finalized blocks before forwarding to gatling (if enabled).
    /// When present, application and ancestor finalizer forward blocks here instead of directly to gatling.
    pub buffer_tx: Option<futures::channel::mpsc::UnboundedSender<alto_types::Block>>,
    
    /// Instance ID for this consensus engine (1-based, used for gatling ordering).
    pub gatling_instance_id: usize,
    
    /// Shared view tracking across all instances - used to detect lagging instances.
    pub instance_views: Arc<Vec<AtomicU64>>,
    
    /// Number of views an instance must be behind to skip its scheduled wait.
    pub lag_threshold: u64,

    /// Total number of consensus instances (K)
    pub total_instances: usize,

    /// Genesis timestamp in seconds
    pub genesis_timestamp_secs: u64,
    
    /// Maximum number of concurrent ancestor fetch requests
    pub ancestor_fetch_concurrent: usize,
    /// Rate limit per peer for ancestor fetching
    pub ancestor_fetch_rate_per_peer: governor::Quota,

    /// Shared mempool — all consensus instances on this validator read from and write to the same pool.
    pub shared_mempool: Arc<Mutex<mempool::Mempool>>,
}
