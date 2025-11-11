use crate::{application, indexer, indexer::Indexer, supervisor::Supervisor};
use alto_types::{Activity, Block, Evaluation};
use commonware_broadcast::buffered;
use commonware_consensus::{
    marshal,
    threshold_simplex::{self, types::View, Engine as Consensus},
    Reporters,
};
use commonware_cryptography::{
    bls12381::primitives::{
        group,
        poly::{public, Poly},
        variant::MinSig,
    },
    ed25519::{PrivateKey, PublicKey},
    sha256::Digest,
    Signer,
};
use commonware_p2p::{Blocker, Receiver, Sender};
use commonware_runtime::{buffer::PoolRef, Clock, Handle, Metrics, Spawner, Storage};
use commonware_utils::{NZUsize, NZU64};
use futures::future::try_join_all;
use governor::clock::Clock as GClock;
use governor::Quota;
use rand::{CryptoRng, Rng};
use std::{num::NonZero, sync::{Arc, atomic::AtomicU64}, time::Duration};
use tracing::{error, warn};

/// Event sent to the gatling thread when a block is finalized.
#[derive(Clone)]
pub struct GatlingEvent {
    pub instance_id: usize,
    pub view: View,
    pub block: Block,
}

/// Reporter type for [threshold_simplex::Engine].
type Reporter<E, I> =
    Reporters<Activity, application::FinalizationPusher<E>, Option<indexer::Pusher<E, I>>>;

/// To better support peers near tip during network instability, we multiply
/// the consensus activity timeout by this factor.
const SYNCER_ACTIVITY_TIMEOUT_MULTIPLIER: u64 = 10;
const PRUNABLE_ITEMS_PER_SECTION: NonZero<u64> = NZU64!(4_096);
const IMMUTABLE_ITEMS_PER_SECTION: NonZero<u64> = NZU64!(262_144);
const FREEZER_TABLE_RESIZE_FREQUENCY: u8 = 4;
const FREEZER_TABLE_RESIZE_CHUNK_SIZE: u32 = 2u32.pow(16); // 3MB
const FREEZER_JOURNAL_TARGET_SIZE: u64 = 1024 * 1024 * 1024; // 1GB
const FREEZER_JOURNAL_COMPRESSION: Option<u8> = Some(3);
const REPLAY_BUFFER: NonZero<usize> = NZUsize!(8 * 1024 * 1024); // 8MB
const WRITE_BUFFER: NonZero<usize> = NZUsize!(1024 * 1024); // 1MB
const BUFFER_POOL_PAGE_SIZE: NonZero<usize> = NZUsize!(4_096); // 4KB
const BUFFER_POOL_CAPACITY: NonZero<usize> = NZUsize!(8_192); // 32MB
const MAX_REPAIR: u64 = 20;

/// Configuration for the [Engine].
pub struct Config<B: Blocker<PublicKey = PublicKey>, I: Indexer> {
    pub blocker: B,
    pub partition_prefix: String,
    pub namespace: Vec<u8>,
    pub blocks_freezer_table_initial_size: u32,
    pub finalized_freezer_table_initial_size: u32,
    pub signer: PrivateKey,
    pub polynomial: Poly<Evaluation>,
    pub share: group::Share,
    pub participants: Vec<PublicKey>,
    pub mailbox_size: usize,
    pub backfill_quota: Quota,
    pub deque_size: usize,

    pub leader_timeout: Duration,
    pub notarization_timeout: Duration,
    pub nullify_retry: Duration,
    pub fetch_timeout: Duration,
    pub activity_timeout: u64,
    pub skip_timeout: u64,
    pub max_fetch_count: usize,
    pub max_fetch_size: usize,
    pub fetch_concurrent: usize,
    pub fetch_rate_per_peer: Quota,

    /// Maximum number of concurrent ancestor fetch requests
    pub ancestor_fetch_concurrent: usize,
    /// Rate limit per peer for ancestor fetching
    pub ancestor_fetch_rate_per_peer: Quota,

    pub indexer: Option<I>,
    
    /// Time offset within each second (in milliseconds, 0-999) for when this engine should send proposals.
    /// For example: 0 means X.000s, 500 means X.500s. This allows staggering multiple consensus instances.
    pub proposal_offset_ms: u64,
    
    /// Channel to send finalized blocks to the gatling thread (if enabled).
    pub gatling_tx: Option<futures::channel::mpsc::UnboundedSender<GatlingEvent>>,
    
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

    /// The engine that drives the [application].
pub struct Engine<
    E: Clock + GClock + Rng + CryptoRng + Spawner + Storage + Metrics,
    B: Blocker<PublicKey = PublicKey>,
    I: Indexer,
> {
    context: E,

    application: application::Actor<E>,
    application_mailbox: application::Mailbox,
    buffer: buffered::Engine<E, PublicKey, Block>,
    buffer_mailbox: buffered::Mailbox<PublicKey, Block>,
    marshal: marshal::Actor<Block, E, MinSig, PublicKey, Supervisor>,
    marshal_mailbox: marshal::Mailbox<MinSig, Block>,

    consensus: Consensus<
        E,
        PrivateKey,
        B,
        MinSig,
        Digest,
        application::Mailbox,
        application::Mailbox,
        Reporter<E, I>,
        Supervisor,
    >,
}

impl<
        E: Clock + GClock + Rng + CryptoRng + Spawner + Storage + Metrics,
        B: Blocker<PublicKey = PublicKey>,
        I: Indexer,
    > Engine<E, B, I>
{
    /// Get a reference to the application mailbox for transaction submission.
    pub fn application_mailbox(&self) -> &application::Mailbox {
        &self.application_mailbox
    }

    /// Create a new [Engine].
    pub async fn new(context: E, cfg: Config<B, I>) -> Self {
        // Prepare optional per-instance buffer channel (only when gatling is enabled)
        let gatling_sender_opt = cfg.gatling_tx.clone();
        let (app_buffer_tx_opt, buffer_rx_opt) = if gatling_sender_opt.is_some() {
            let (tx, rx) = futures::channel::mpsc::unbounded::<Block>();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        // Create the application
        let identity = *public::<MinSig>(&cfg.polynomial);
        let (application, supervisor, application_mailbox) = application::Actor::new(
            context.with_label("application"),
            application::Config {
                participants: cfg.participants.clone(),
                polynomial: cfg.polynomial,
                share: cfg.share,
                mailbox_size: cfg.mailbox_size,
                engine_id: cfg.partition_prefix.clone(),
                public_key: cfg.signer.public_key(),
                proposal_offset_ms: cfg.proposal_offset_ms,
                gatling_tx: gatling_sender_opt.clone(),
                buffer_tx: app_buffer_tx_opt.clone(),
                gatling_instance_id: cfg.gatling_instance_id,
                instance_views: cfg.instance_views,
                lag_threshold: cfg.lag_threshold,
                total_instances: cfg.total_instances,
                genesis_timestamp_secs: cfg.genesis_timestamp_secs,
                ancestor_fetch_concurrent: cfg.ancestor_fetch_concurrent,
                ancestor_fetch_rate_per_peer: cfg.ancestor_fetch_rate_per_peer,
            },
        ); 

        // Spawn per-instance buffer task if enabled
        if let (Some(buffer_rx), Some(g_tx)) = (buffer_rx_opt, gatling_sender_opt) {
            let instance_id = cfg.gatling_instance_id;
            context.with_label("instance_buffer").spawn(move |_| async move {
                application::run_buffer(buffer_rx, g_tx, instance_id).await
            });
        }

        // Create the buffer
        let (buffer, buffer_mailbox) = buffered::Engine::new(
            context.with_label("buffer"),
            buffered::Config {
                public_key: cfg.signer.public_key(),
                mailbox_size: cfg.mailbox_size,
                deque_size: cfg.deque_size,
                priority: true,
                codec_config: (),
            },
        );

        // Create the buffer pool
        let buffer_pool = PoolRef::new(BUFFER_POOL_PAGE_SIZE, BUFFER_POOL_CAPACITY);

        // Create marshal
        let (marshal, marshal_mailbox): (_, marshal::Mailbox<MinSig, Block>) =
            marshal::Actor::init(
                context.with_label("marshal"),
                marshal::Config {
                    public_key: cfg.signer.public_key(),
                    identity,
                    coordinator: supervisor.clone(),
                    partition_prefix: cfg.partition_prefix.clone(),
                    mailbox_size: cfg.mailbox_size,
                    backfill_quota: cfg.backfill_quota,
                    view_retention_timeout: cfg
                        .activity_timeout
                        .saturating_mul(SYNCER_ACTIVITY_TIMEOUT_MULTIPLIER),
                    namespace: cfg.namespace.clone(),
                    prunable_items_per_section: PRUNABLE_ITEMS_PER_SECTION,
                    immutable_items_per_section: IMMUTABLE_ITEMS_PER_SECTION,
                    freezer_table_initial_size: cfg.blocks_freezer_table_initial_size,
                    freezer_table_resize_frequency: FREEZER_TABLE_RESIZE_FREQUENCY,
                    freezer_table_resize_chunk_size: FREEZER_TABLE_RESIZE_CHUNK_SIZE,
                    freezer_journal_target_size: FREEZER_JOURNAL_TARGET_SIZE,
                    freezer_journal_compression: FREEZER_JOURNAL_COMPRESSION,
                    freezer_journal_buffer_pool: buffer_pool.clone(),
                    replay_buffer: REPLAY_BUFFER,
                    write_buffer: WRITE_BUFFER,
                    codec_config: (),
                    max_repair: MAX_REPAIR,
                },
            )
            .await;

        // Create the FinalizationPusher to send finalizations to the application  
        let finalization_pusher = application::FinalizationPusher::new(
            context.with_label("finalization_pusher"),
            application_mailbox.sender_clone(),
            marshal_mailbox.clone(),
        );
        
        // Create the reporter chain for consensus (sends Activity with finalizations)
        // FinalizationPusher extracts view from Finalization proof and fetches block
        let reporter = (
            finalization_pusher,
            cfg.indexer.map(|indexer| {
                indexer::Pusher::new(
                    context.with_label("indexer"),
                    indexer,
                    marshal_mailbox.clone(),
                )
            }),
        )
            .into();

        // Create the consensus engine
        let consensus = Consensus::new(
            context.with_label("consensus"),
            threshold_simplex::Config {
                namespace: cfg.namespace.clone(),
                crypto: cfg.signer,
                automaton: application_mailbox.clone(),
                relay: application_mailbox.clone(),
                reporter,
                supervisor,
                partition: format!("{}-consensus", cfg.partition_prefix),
                mailbox_size: cfg.mailbox_size,
                leader_timeout: cfg.leader_timeout,
                notarization_timeout: cfg.notarization_timeout,
                nullify_retry: cfg.nullify_retry,
                fetch_timeout: cfg.fetch_timeout,
                activity_timeout: cfg.activity_timeout,
                skip_timeout: cfg.skip_timeout,
                max_fetch_count: cfg.max_fetch_count,
                fetch_concurrent: cfg.fetch_concurrent,
                fetch_rate_per_peer: cfg.fetch_rate_per_peer,
                replay_buffer: REPLAY_BUFFER,
                write_buffer: WRITE_BUFFER,
                blocker: cfg.blocker,
                buffer_pool,
            },
        );

        // Return the engine
        Self {
            context,

            application,
            application_mailbox,
            buffer,
            buffer_mailbox,
            marshal,
            marshal_mailbox,
            consensus,
        }
    }

    /// Start the [threshold_simplex::Engine].
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        self,
        pending_network: (
            impl Sender<PublicKey = PublicKey>,
            impl Receiver<PublicKey = PublicKey>,
        ),
        recovered_network: (
            impl Sender<PublicKey = PublicKey>,
            impl Receiver<PublicKey = PublicKey>,
        ),
        resolver_network: (
            impl Sender<PublicKey = PublicKey>,
            impl Receiver<PublicKey = PublicKey>,
        ),
        broadcast_network: (
            impl Sender<PublicKey = PublicKey>,
            impl Receiver<PublicKey = PublicKey>,
        ),
        backfill_network: (
            impl Sender<PublicKey = PublicKey>,
            impl Receiver<PublicKey = PublicKey>,
        ),
        ancestor_network: (
            impl Sender<PublicKey = PublicKey>,
            impl Receiver<PublicKey = PublicKey>,
        ),
    ) -> Handle<()> {
        self.context.clone().spawn(|_| {
            self.run(
                pending_network,
                recovered_network,
                resolver_network,
                broadcast_network,
                backfill_network,
                ancestor_network,
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn run(
        self,
        pending_network: (
            impl Sender<PublicKey = PublicKey>,
            impl Receiver<PublicKey = PublicKey>,
        ),
        recovered_network: (
            impl Sender<PublicKey = PublicKey>,
            impl Receiver<PublicKey = PublicKey>,
        ),
        resolver_network: (
            impl Sender<PublicKey = PublicKey>,
            impl Receiver<PublicKey = PublicKey>,
        ),
        broadcast_network: (
            impl Sender<PublicKey = PublicKey>,
            impl Receiver<PublicKey = PublicKey>,
        ),
        backfill_network: (
            impl Sender<PublicKey = PublicKey>,
            impl Receiver<PublicKey = PublicKey>,
        ),
        ancestor_network: (
            impl Sender<PublicKey = PublicKey>,
            impl Receiver<PublicKey = PublicKey>,
        ),
    ) {
        // Start the application
        let application_handle = self.application.start(self.marshal_mailbox, ancestor_network);

        // Start the buffer
        let buffer_handle = self.buffer.start(broadcast_network);

        // Start marshal
        let marshal_handle = self.marshal.start(
            self.application_mailbox.clone(),
            self.buffer_mailbox,
            backfill_network,
        );

        // Start consensus
        //
        // We start the application prior to consensus to ensure we can handle enqueued events from consensus (otherwise
        // restart could block).
        let consensus_handle =
            self.consensus
                .start(pending_network, recovered_network, resolver_network);

        // Wait for any actor to finish
        if let Err(e) = try_join_all(vec![
            application_handle,
            buffer_handle,
            marshal_handle,
            consensus_handle,
        ])
        .await
        {
            error!(?e, "engine failed");
        } else {
            warn!("engine stopped");
        }
    }
}
