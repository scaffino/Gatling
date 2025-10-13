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
}
