use alto_types::Evaluation;
use commonware_cryptography::{
    bls12381::primitives::{group, poly::Poly},
    ed25519::PublicKey,
};

mod actor;
pub use actor::Actor;
mod ingress;
pub use ingress::{FinalizationPusher, Mailbox};
pub mod mempool;

use commonware_cryptography::sha256::Digest;
use std::collections::HashSet;
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
    
    /// Shared set of transaction digests that have been included in blocks across all consensus instances.
    /// This prevents the same transaction from being included in multiple instances.
    pub included_transactions: Arc<Mutex<HashSet<Digest>>>,
    
    /// Time offset within each second (in milliseconds, 0-999) for when this engine should send proposals.
    /// For example: 0 means X.000s, 500 means X.500s. This allows staggering multiple consensus instances.
    pub proposal_offset_ms: u64,
    
    /// Channel to send finalized blocks to the gatling thread (if enabled).
    pub gatling_tx: Option<futures::channel::mpsc::UnboundedSender<crate::engine::GatlingEvent>>,
    
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
}
