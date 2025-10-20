use alto_types::Evaluation;
use commonware_cryptography::{
    bls12381::primitives::{group, poly::Poly},
    ed25519::PublicKey,
};

mod actor;
pub use actor::Actor;
mod ingress;
pub use ingress::Mailbox;
pub mod mempool;

use commonware_cryptography::sha256::Digest;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

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
}
