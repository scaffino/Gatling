use alto_chain::{application::mempool::Mempool, engine, http_server, Config, Peers};
use alto_client::Client;
use alto_types::{Transaction, NAMESPACE};
use clap::{Arg, Command};
use commonware_codec::{Decode, DecodeExt};
use commonware_cryptography::{
    bls12381::primitives::{group, poly, variant::MinSig},
    ed25519::{PrivateKey, PublicKey},
    Digestible, Signer,
};
use commonware_deployer::ec2::Hosts;
use commonware_p2p::authenticated::discovery as authenticated;
use commonware_runtime::{tokio, Clock, Metrics, Runner, Spawner};
use commonware_utils::{from_hex_formatted, quorum, union_unique};
use futures::future::try_join_all;
use futures::StreamExt;
use governor::Quota;
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroU32,
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex, atomic::AtomicU64},
    time::Duration,
};
use tracing::{error, info, warn, Level};

const PENDING_CHANNEL: u32 = 0;
const RECOVERED_CHANNEL: u32 = 1;
const RESOLVER_CHANNEL: u32 = 2;
const BROADCASTER_CHANNEL: u32 = 3;
const BACKFILL_BY_DIGEST_CHANNEL: u32 = 4;
const TRANSACTION_CHANNEL: u32 = 5;
const ANCESTOR_CHANNEL: u32 = 6;

const LEADER_TIMEOUT: Duration = Duration::from_millis(10000);
const NOTARIZATION_TIMEOUT: Duration = Duration::from_millis(15000);
const NULLIFY_RETRY: Duration = Duration::from_secs(10);
const ACTIVITY_TIMEOUT: u64 = 256;
const SKIP_TIMEOUT: u64 = 32;
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);
const FETCH_CONCURRENT: usize = 4;
const ANCESTOR_FETCH_CONCURRENT: usize = 4;
const MAX_MESSAGE_SIZE: usize = 1024 * 1024;
const MAX_FETCH_COUNT: usize = 16;
const MAX_FETCH_SIZE: usize = 512 * 1024;
const BLOCKS_FREEZER_TABLE_INITIAL_SIZE: u32 = 2u32.pow(21); // 100MB
const FINALIZED_FREEZER_TABLE_INITIAL_SIZE: u32 = 2u32.pow(21); // 100MB

/// Gatling thread that orders blocks from all consensus instances by view and instance number.
/// 
/// Uses a cursor-based algorithm to finalize blocks strictly in ascending order of (view, instance).
/// The algorithm maintains a cursor that always points to the top-leftmost unfinalized cell in the
/// matrix of instances (rows) × views (columns), ensuring strict ordering guarantees.
async fn gatling_thread(
    mut rx: futures::channel::mpsc::UnboundedReceiver<engine::GatlingEvent>,
    validator_index: usize,
    num_instances: usize,
) {
    use std::collections::BTreeMap;
    
    // ============================================================================
    // DATA STRUCTURES
    // ============================================================================
    
    // Per-instance queues of finalized blocks, keyed by view number (from block.view, not proof)
    // - Some(block) = view that has a finalized block from consensus (directly finalized view)
    // - None = view marked as finalized without a block (indirectly finalized view) to fill gaps when instances skip views
    // Note: All blocks are finalized with the same event - what differs is how VIEWS are finalized (directly vs indirectly)
    let mut instance_queues: Vec<BTreeMap<u64, Option<alto_types::Block>>> = 
        (0..num_instances).map(|_| BTreeMap::new()).collect();
    
    // Per-instance highest directly finalized view (only tracks views with actual blocks from consensus)
    // Used to track progress: if finalized_up_to[k] >= v, then all views <= v for instance k are already finalized
    let mut finalized_up_to: Vec<u64> = vec![0; num_instances];
    
    // Global cursor position: always points to the next top-leftmost unfinalized cell
    // Cursor moves lexicographically: (view, instance) = (1,0), (1,1), ..., (1,K-1), (2,0), ...
    let mut cursor_view: u64 = 1;
    let mut cursor_instance: usize = 0;
    
    info!("[gatling] Gatling thread started for {} instances", num_instances);
    
    // ============================================================================
    // MAIN EVENT LOOP: Process finalized blocks from all instances
    // ============================================================================
    
    while let Some(event) = rx.next().await {
        // Convert 1-based instance ID to 0-based index
        let instance_idx = event.instance_id - 1;
        
        // Get view number directly from the block (not from finalization proof)
        let view = event.block.view;
        
        // ========================================================================
        // STEP 1: Insert the new block and fill any gaps
        // ========================================================================
        
        // Find the highest view we've seen for this instance (either finalized or queued)
        let highest_queued_view = instance_queues[instance_idx]
            .keys()
            .max()
            .copied()
            .unwrap_or(0);
        let highest_seen_view = finalized_up_to[instance_idx].max(highest_queued_view);
        
        // If the new block creates a gap (e.g., we have view V and now receive view V+3),
        // mark the intermediate views as indirectly finalized (no blocks, but views are finalized)
        if view > highest_seen_view + 1 && highest_seen_view > 0 {
            for gap_view in (highest_seen_view + 1)..view {
                if !instance_queues[instance_idx].contains_key(&gap_view) {
                    instance_queues[instance_idx].insert(gap_view, None); // Indirectly finalized view
                }
            }
        }
        
        // Insert the block into the queue at this view (view is directly finalized with a block)
        instance_queues[instance_idx].insert(view, Some(event.block));
        
        // ========================================================================
        // STEP 2: Try to finalize as much as possible using cursor-based algorithm
        // ========================================================================
        
        loop {
            // Safety check: ensure cursor is within valid bounds
            if cursor_view == 0 || cursor_instance >= num_instances {
                break;
            }
            
            // Check if this view has already been finalized
            // Since finalized_up_to tracks the highest directly finalized view, and finalizing a view
            // at V implies all views <= V are finalized, we can skip if cursor_view <= finalized_up_to
            if finalized_up_to[cursor_instance] >= cursor_view {
                advance_cursor(&mut cursor_view, &mut cursor_instance, num_instances);
                continue;
            }
            
            // Check what's at the cursor position (view cursor_view for instance cursor_instance)
            match instance_queues[cursor_instance].remove(&cursor_view) {
                Some(Some(block)) => {
                    // ============================================================
                    // Directly finalized view: view has a block from consensus - finalize it with logging
                    // ============================================================
                    
                    finalize_block_at_view(
                        &block,
                        cursor_view,
                        cursor_instance,
                        validator_index,
                        &mut instance_queues[cursor_instance],
                        &mut finalized_up_to[cursor_instance],
                    );
                    
                    advance_cursor(&mut cursor_view, &mut cursor_instance, num_instances);
                    continue;
                }
                Some(None) => {
                    // ============================================================
                    // Indirectly finalized view: view is marked as finalized but has no block
                    // Just advance cursor (no logging, no update to finalized_up_to)
                    // ============================================================
                    
                    advance_cursor(&mut cursor_view, &mut cursor_instance, num_instances);
                    continue;
                }
                None => {
                    // No entry at this view - check if instance has moved past it
                    let has_higher_view = instance_queues[cursor_instance]
                        .keys()
                        .any(|&v| v > cursor_view);
                    
                    if has_higher_view {
                        // Instance has moved past cursor_view, so cursor_view is a gap
                        // Mark this view as indirectly finalized (no block, but view is finalized)
                        instance_queues[cursor_instance].insert(cursor_view, None);
                        
                        advance_cursor(&mut cursor_view, &mut cursor_instance, num_instances);
                        continue;
                        } else {
                        // Cannot make progress: waiting for block at cursor_view or evidence
                        // that this instance has moved past it
                        break;
                    }
                }
            }
        }
    }
    
    info!("[gatling] Gatling thread stopped");
}

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

/// Advance the cursor to the next position in lexicographic order.
/// Moves through instances first, then increments view.
fn advance_cursor(cursor_view: &mut u64, cursor_instance: &mut usize, num_instances: usize) {
    *cursor_instance += 1;
    if *cursor_instance >= num_instances {
        *cursor_instance = 0;
        *cursor_view += 1;
    }
}

/// Finalize a block at a directly finalized view: log the block and its transactions, update state, and clean up old entries.
/// This view has a block from consensus (directly finalized view), so we log it and update finalized_up_to.
fn finalize_block_at_view(
    block: &alto_types::Block,
    view: u64,
    instance_idx: usize,
    validator_index: usize,
    queue: &mut std::collections::BTreeMap<u64, Option<alto_types::Block>>,
    finalized_up_to: &mut u64,
) {
    let instance_id = instance_idx + 1; // Convert to 1-based for display
                let tx_count = block.transactions.len();
                
    // Log the globally finalized block (all blocks are finalized the same way)
                info!(
                    "[gatling] Validator {} finalized block {} from instance {} (view {}) with {} transactions",
        validator_index, block.height, instance_id, view, tx_count
                );
                
                // Log each transaction in the block
                for tx in &block.transactions {
                    let tx_id = tx.digest();
                    info!(
                        "[gatling] Transaction {:?} (timestamp: {} ms) is now final in block {} from instance {} (view {})",
            tx_id, tx.timestamp, block.height, instance_id, view
        );
    }
    
    // Update highest directly finalized view (only when a view has a block from consensus)
    *finalized_up_to = view;
    
    // Clean up: remove any entries for views < view (they've already been processed)
    let views_to_remove: Vec<u64> = queue
        .keys()
        .copied()
        .filter(|&v| v < view)
        .collect();
    for view_to_remove in views_to_remove {
        queue.remove(&view_to_remove);
    }
}

fn main() {
    // Parse arguments
    let matches = Command::new("validator")
        .about("Validator for an alto chain.")
        .arg(Arg::new("hosts").long("hosts").required(false))
        .arg(Arg::new("peers").long("peers").required(false))
        .arg(Arg::new("config").long("config").required(true))
        .arg(
            Arg::new("consensus-instances")
                .long("consensus-instances")
                .value_name("COUNT")
                .help("Number of independent consensus instances to run in parallel")
                .default_value("1")
        )
        .arg(
            Arg::new("gossip-txs")
                .long("gossip-txs")
                .help("Enable transaction gossiping to other validators")
                .action(clap::ArgAction::SetTrue)
                .conflicts_with("no-gossip-txs")
        )
        .arg(
            Arg::new("no-gossip-txs")
                .long("no-gossip-txs")
                .help("Disable transaction gossiping to other validators")
                .action(clap::ArgAction::SetTrue)
                .conflicts_with("gossip-txs")
        )
        .arg(
            Arg::new("gatling")
                .long("gatling")
                .help("Enable gatling global finalization tracking and logging")
                .action(clap::ArgAction::SetTrue)
        )
        .arg(
            Arg::new("submit-tx")
                .long("submit-tx")
                .value_name("RATE START_DELAY DURATION")
                .help("Generate transactions at rate R (tx/sec) starting T seconds after genesis, for duration D seconds")
                .num_args(3)
                .value_delimiter(' ')
        )
        .get_matches();

    // Load ip file
    let hosts_file = matches.get_one::<String>("hosts");
    let peers_file = matches.get_one::<String>("peers");
    assert!(
        hosts_file.is_some() || peers_file.is_some(),
        "Either --hosts or --peers must be provided"
    );

    // Parse consensus instances count
    let consensus_instances: usize = matches.get_one::<String>("consensus-instances")
        .unwrap()
        .parse()
        .expect("Invalid consensus instances count");
    assert!(consensus_instances > 0, "Number of consensus instances must be at least 1");

    // Parse gossip flag (default: enabled if neither flag is specified)
    let gossip_enabled = if matches.get_flag("no-gossip-txs") {
        false
    } else {
        true // Default to enabled, or explicitly enabled with --gossip-txs
    };

    // Parse gatling flag (default: disabled)
    let gatling_enabled = matches.get_flag("gatling");

    // Parse submit-tx arguments (optional)
    let submit_tx_config: Option<(f64, u64, u64)> = if let Some(values) = matches.get_many::<String>("submit-tx") {
        let values: Vec<&String> = values.collect();
        if values.len() == 3 {
            let rate: f64 = values[0].parse().expect("Invalid rate (must be a number)");
            let start_delay: u64 = values[1].parse().expect("Invalid start delay (must be a positive integer)");
            let duration: u64 = values[2].parse().expect("Invalid duration (must be a positive integer)");
            
            assert!(rate > 0.0, "Rate must be greater than 0");
            assert!(duration > 0, "Duration must be greater than 0");
            
            Some((rate, start_delay, duration))
        } else {
            None
        }
    } else {
        None
    };

    // Load config
    let config_file = matches.get_one::<String>("config").unwrap();
    let config_file = std::fs::read_to_string(config_file).expect("Could not read config file");
    let config: Config = serde_yaml::from_str(&config_file).expect("Could not parse config file");
    let key = from_hex_formatted(&config.private_key).expect("Could not parse private key");
    let signer = PrivateKey::decode(key.as_ref()).expect("Private key is invalid");
    let public_key = signer.public_key();

    // Initialize runtime
    let cfg = tokio::Config::default()
        .with_tcp_nodelay(Some(true))
        .with_worker_threads(config.worker_threads)
        .with_storage_directory(PathBuf::from(config.directory))
        .with_catch_panics(false);
    let executor = tokio::Runner::new(cfg);

    // Start runtime
    executor.start(|context| async move {
        // Configure telemetry
        let log_level = Level::from_str(&config.log_level).expect("Invalid log level");
        tokio::telemetry::init(
            context.with_label("telemetry"),
            tokio::telemetry::Logging {
                level: log_level,
                // If we are using `commonware-deployer`, we should use structured logging.
                json: hosts_file.is_some(),
            },
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                config.metrics_port,
            )),
            None,
        );

        // Log genesis time (UTC) once after telemetry is initialized
        let genesis_time_utc = chrono::DateTime::<chrono::Utc>::from_timestamp(config.genesis_timestamp as i64, 0)
            .map(|dt| dt.format("%H:%M:%S.%3f UTC").to_string())
            .unwrap_or_else(|| "invalid-ts".to_string());
        info!(genesis = %genesis_time_utc, "genesis time");

        // Load peers
        let (ip, peers, bootstrappers) = if let Some(hosts_file) = hosts_file {
            let hosts_file = std::fs::read_to_string(hosts_file).unwrap();
            let hosts: Hosts =
                serde_yaml::from_str(&hosts_file).expect("Could not parse peers file");
            let peers: HashMap<PublicKey, IpAddr> = hosts
                .hosts
                .into_iter()
                .map(|peer| {
                    let key = from_hex_formatted(&peer.name).expect("Could not parse peer key");
                    let key = PublicKey::decode(key.as_ref()).expect("Peer key is invalid");
                    (key, peer.ip)
                })
                .collect();

            let mut peer_keys: Vec<_> = peers.keys().cloned().collect();
            peer_keys.sort();  // CRITICAL: Sort to ensure consistent ordering across all validators
            let mut bootstrappers = Vec::new();
            for bootstrapper in &config.bootstrappers {
                let key =
                    from_hex_formatted(bootstrapper).expect("Could not parse bootstrapper key");
                let key = PublicKey::decode(key.as_ref()).expect("Bootstrapper key is invalid");
                let ip = peers.get(&key).expect("Could not find bootstrapper in IPs");
                let bootstrapper_socket = format!("{}:{}", ip, config.port);
                let bootstrapper_socket = SocketAddr::from_str(&bootstrapper_socket)
                    .expect("Could not parse bootstrapper socket");
                bootstrappers.push((key, bootstrapper_socket));
            }
            let ip = peers.get(&public_key).expect("Could not find self in IPs");
            (*ip, peer_keys, bootstrappers)
        } else {
            let peers_file = std::fs::read_to_string(peers_file.unwrap()).unwrap();
            let peers: Peers =
                serde_yaml::from_str(&peers_file).expect("Could not parse peers file");
            let peers: HashMap<PublicKey, SocketAddr> = peers
                .addresses
                .into_iter()
                .map(|peer| {
                    let key = from_hex_formatted(&peer.0).expect("Could not parse peer key");
                    let key = PublicKey::decode(key.as_ref()).expect("Peer key is invalid");
                    (key, peer.1)
                })
                .collect();

            let mut peer_keys: Vec<_> = peers.keys().cloned().collect();
            peer_keys.sort();  // CRITICAL: Sort to ensure consistent ordering across all validators
            let mut bootstrappers = Vec::new();
            for bootstrapper in &config.bootstrappers {
                let key =
                    from_hex_formatted(bootstrapper).expect("Could not parse bootstrapper key");
                let key = PublicKey::decode(key.as_ref()).expect("Bootstrapper key is invalid");
                let socket = peers.get(&key).expect("Could not find bootstrapper in IPs");
                bootstrappers.push((key, *socket));
            }
            let ip = peers
                .get(&public_key)
                .expect("Could not find self in IPs")
                .ip();
            (ip, peer_keys, bootstrappers)
        };
        info!(peers = peers.len(), "loaded peers");
        let peers_u32 = peers.len() as u32;

        // Parse config
        let share = from_hex_formatted(&config.share).expect("Could not parse share");
        let share = group::Share::decode(share.as_ref()).expect("Share is invalid");
        let threshold = quorum(peers_u32);
        let polynomial =
            from_hex_formatted(&config.polynomial).expect("Could not parse polynomial");
        let polynomial =
            poly::Public::<MinSig>::decode_cfg(polynomial.as_ref(), &(threshold as usize))
                .expect("polynomial is invalid");
        let identity = *poly::public::<MinSig>(&polynomial);
        info!(
            ?public_key,
            ?identity,
            ?ip,
            port = config.port,
            consensus_instances,
            "loaded config"
        );

        // Configure network
        let p2p_namespace = union_unique(NAMESPACE, b"_P2P");
        let mut p2p_cfg = authenticated::Config::aggressive(
            signer.clone(),
            &p2p_namespace,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), config.port),
            SocketAddr::new(ip, config.port),
            bootstrappers,
            MAX_MESSAGE_SIZE,
        );
        p2p_cfg.mailbox_size = config.mailbox_size;

        // Start p2p
        let (mut network, mut oracle) =
            authenticated::Network::new(context.with_label("network"), p2p_cfg);

        // Provide authorized peers
        oracle.register(0, peers.clone()).await;

        // Register channels for each independent consensus instance
        let pending_limit = Quota::per_second(NonZeroU32::new(128).unwrap());
        let recovered_limit = Quota::per_second(NonZeroU32::new(128).unwrap());
        let resolver_limit = Quota::per_second(NonZeroU32::new(128).unwrap());
        let broadcaster_limit = Quota::per_second(NonZeroU32::new(8).unwrap());
        let backfill_quota = Quota::per_second(NonZeroU32::new(8).unwrap());
        let ancestor_quota = Quota::per_second(NonZeroU32::new(16).unwrap());
        let transaction_limit = Quota::per_second(NonZeroU32::new(256).unwrap());

        let mut consensus_channels = Vec::new();
        for i in 0..consensus_instances {
            // Each consensus instance gets its own set of channels (10 channels apart)
            let base_channel = i as u32 * 10;
            let pending_channel = base_channel + PENDING_CHANNEL;
            let recovered_channel = base_channel + RECOVERED_CHANNEL;
            let resolver_channel = base_channel + RESOLVER_CHANNEL;
            let broadcaster_channel = base_channel + BROADCASTER_CHANNEL;
            let backfill_channel = base_channel + BACKFILL_BY_DIGEST_CHANNEL;
            let ancestor_channel = base_channel + ANCESTOR_CHANNEL;

            let pending = network.register(pending_channel, pending_limit, config.message_backlog);
            let recovered = network.register(recovered_channel, recovered_limit, config.message_backlog);
            let resolver = network.register(resolver_channel, resolver_limit, config.message_backlog);
            let broadcaster = network.register(broadcaster_channel, broadcaster_limit, config.message_backlog);
            let backfill = network.register(backfill_channel, backfill_quota, config.message_backlog);
            let ancestor = network.register(ancestor_channel, ancestor_quota, config.message_backlog);

            consensus_channels.push((pending, recovered, resolver, broadcaster, backfill, ancestor));
        }

        // Register a shared transaction channel for gossip (shared across all consensus instances)
        let (mut tx_sender, mut tx_receiver) = network.register(TRANSACTION_CHANNEL, transaction_limit, config.message_backlog);

        // Create network
        let p2p = network.start();

        // Create indexer
        let mut indexer = None;
        if let Some(uri) = config.indexer {
            indexer = Some(Client::new(&uri, identity));
        }
        
        // Create shared mempool — one pool for all K consensus instances on this validator.
        // Transactions are added once; each instance pulls up to MAX_BLOCK_TRANSACTIONS per proposal.
        let shared_mempool = Arc::new(Mutex::new(Mempool::new(context.with_label("mempool"))));

        // Create shared state for tracking the highest view reached by each consensus instance
        // This allows instances to detect if they're lagging and skip their scheduled wait time
        let instance_views: Arc<Vec<AtomicU64>> = Arc::new(
            (0..consensus_instances)
                .map(|_| AtomicU64::new(0))
                .collect()
        );
        
        // Create gatling event channel if gatling is enabled
        let (gatling_tx, gatling_rx) = if gatling_enabled {
            let (tx, rx) = futures::channel::mpsc::unbounded();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        
        // Initialize all K engines concurrently so storage opens happen in parallel.
        // Each future owns its config (all shared state is Arc/Clone), so there are no conflicts.
        info!(consensus_instances, "initializing consensus engines");
        let consensus_engines = futures::future::join_all(
            (0..consensus_instances).map(|i| {
                let chain_id = i + 1;
                let namespace = format!("{}_{}", std::str::from_utf8(NAMESPACE).unwrap(), chain_id).into_bytes();
                let proposal_offset_ms = (i * 3000 / consensus_instances) as u64;
                let engine_config = engine::Config {
                    blocker: oracle.clone(),
                    partition_prefix: format!("consensus_{}", chain_id),
                    namespace,
                    blocks_freezer_table_initial_size: BLOCKS_FREEZER_TABLE_INITIAL_SIZE,
                    finalized_freezer_table_initial_size: FINALIZED_FREEZER_TABLE_INITIAL_SIZE,
                    signer: signer.clone(),
                    polynomial: polynomial.clone(),
                    share: share.clone(),
                    participants: peers.clone(),
                    mailbox_size: config.mailbox_size,
                    deque_size: config.deque_size,
                    backfill_quota,
                    leader_timeout: LEADER_TIMEOUT,
                    notarization_timeout: NOTARIZATION_TIMEOUT,
                    nullify_retry: NULLIFY_RETRY,
                    activity_timeout: ACTIVITY_TIMEOUT,
                    skip_timeout: SKIP_TIMEOUT,
                    fetch_timeout: FETCH_TIMEOUT,
                    max_fetch_count: MAX_FETCH_COUNT,
                    max_fetch_size: MAX_FETCH_SIZE,
                    fetch_concurrent: FETCH_CONCURRENT,
                    fetch_rate_per_peer: resolver_limit,
                    indexer: indexer.clone(),
                    proposal_offset_ms,
                    gatling_tx: gatling_tx.clone(),
                    gatling_instance_id: chain_id,
                    instance_views: instance_views.clone(),
                    lag_threshold: 1,
                    total_instances: consensus_instances,
                    genesis_timestamp_secs: config.genesis_timestamp,
                    ancestor_fetch_concurrent: ANCESTOR_FETCH_CONCURRENT,
                    ancestor_fetch_rate_per_peer: resolver_limit,
                    shared_mempool: shared_mempool.clone(),
                };
                engine::Engine::new(
                    context.with_label(&format!("consensus_{}", chain_id)),
                    engine_config,
                )
            })
        ).await;

        // Wait until genesis inside the runtime so P2P can bootstrap during this window.
        // This replaces the blocking thread::sleep that previously ran before the runtime started.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let genesis_ms = config.genesis_timestamp * 1000;
        if now_ms < genesis_ms {
            let wait_ms = genesis_ms - now_ms;
            info!(wait_ms, "engines ready, waiting for genesis");
            context.sleep(Duration::from_millis(wait_ms)).await;
        } else {
            let overrun_ms = now_ms.saturating_sub(genesis_ms);
            warn!(overrun_ms, "engines initialized after genesis; consider increasing the genesis delay");
        }
        
        // Create channel for broadcasting transactions (only if gossip is enabled)
        let broadcast_channel = if gossip_enabled {
            Some(futures::channel::mpsc::unbounded())
        } else {
            None
        };

        // Start submit-tx task if configured
        let submit_tx_task = if let Some((rate, start_delay, duration)) = submit_tx_config {
            let submit_mempool = shared_mempool.clone();
            let signer_clone = signer.clone();
            let receiver = public_key.clone();
            let genesis = config.genesis_timestamp;
            let broadcast_tx_for_submit_tx = broadcast_channel.as_ref().map(|(tx, _)| tx.clone());
            
            Some(context.with_label("submit_tx").spawn(move |ctx| async move {
                use std::time::Instant;
                
                // Calculate start time
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                
                let start_time_secs = genesis + start_delay;
                
                // Wait until start time
                if now_secs < start_time_secs {
                    let wait_secs = start_time_secs - now_secs;
                    info!(
                        rate,
                        start_delay,
                        duration,
                        wait_secs,
                        "submit-tx: waiting until start time"
                    );
                    ctx.sleep(Duration::from_secs(wait_secs)).await;
                }
                
                let start_instant = Instant::now();
                let end_time_secs = start_time_secs + duration;
                let target_interval = 1.0 / rate; // seconds between transactions
                // Use StdRng which is Send + Sync, seeded from current time
                let seed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64;
                let mut rng = StdRng::seed_from_u64(seed);
                let mut tx_count = 0u64;
                
                info!(
                    rate,
                    start_delay,
                    duration,
                    "submit-tx: starting transaction generation"
                );
                
                // Generate transactions until duration expires
                loop {
                    // Check if we've exceeded the duration
                    let current_secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    
                    if current_secs >= end_time_secs {
                        break;
                    }
                    
                    // Generate random jitter: uniform between 0.5x and 1.5x target interval
                    let jitter_factor = 0.5 + rng.gen::<f64>();
                    let sleep_duration_secs = target_interval * jitter_factor;
                    
                    // Convert to Duration (handle fractional seconds)
                    let sleep_duration = if sleep_duration_secs < 1.0 {
                        Duration::from_millis((sleep_duration_secs * 1000.0) as u64)
                    } else {
                        Duration::from_secs(sleep_duration_secs as u64) + 
                        Duration::from_millis(((sleep_duration_secs % 1.0) * 1000.0) as u64)
                    };
                    
                    // Sleep with jitter
                    ctx.sleep(sleep_duration).await;
                    
                    // Check again after sleep
                    let current_secs_after_sleep = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    
                    if current_secs_after_sleep >= end_time_secs {
                        break;
                    }
                    
                    // Create transaction
                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .expect("Time went backwards")
                        .as_millis() as u64;
                    
                    let tx = Transaction::sign(&signer_clone, receiver.clone(), 1, timestamp);
                    let tx_id = tx.digest();

                    // Add to shared mempool once — all K consensus instances read from the same pool
                    submit_mempool.lock().unwrap().add(tx.clone());

                    // Send to broadcast channel for gossip (if enabled)
                    if let Some(broadcast_tx) = &broadcast_tx_for_submit_tx {
                        let _ = broadcast_tx.unbounded_send(tx);
                    }

                    tx_count += 1;
                    if tx_count % 100 == 0 {
                        info!(tx_count, tx_id = ?tx_id, "submit-tx: generated transactions");
                    }
                }
                
                let elapsed = start_instant.elapsed();
                info!(
                    tx_count,
                    elapsed_secs = elapsed.as_secs(),
                    "submit-tx: finished transaction generation"
                );
            }))
        } else {
            None
        };
        
        // Start HTTP server for transaction submission in background
        // HTTP server will distribute transactions to all consensus instances
        let transaction_addr = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            config.transaction_port,
        );
        let http_mempool = shared_mempool.clone();
        let broadcast_tx_for_http = broadcast_channel.as_ref().map(|(tx, _)| tx.clone());
        let http_server = context.with_label("http_server").spawn(move |_| async move {
            if let Err(e) = http_server::start_server_multi(
                transaction_addr,
                http_mempool,
                broadcast_tx_for_http,
            ).await {
                error!(?e, "HTTP server failed");
            }
        });
        
        // Conditionally start background tasks for transaction gossiping
        let mut gossip_tasks = Vec::new();
        
        if let Some((_broadcast_tx, mut broadcast_rx)) = broadcast_channel {
            info!(gossip_enabled, "Transaction gossiping is ENABLED");
            
            // Start background task to broadcast transactions via P2P
            let tx_broadcast_task = context.with_label("tx_broadcast").spawn(move |_| async move {
                use commonware_codec::Encode;
                use commonware_cryptography::Digestible;
                use commonware_p2p::{Sender, Recipients};
                use tracing::{info, warn};
                use futures::StreamExt;
                use bytes::Bytes;
                
                while let Some(tx) = broadcast_rx.next().await {
                    let tx_id = tx.digest();
                    let tx_bytes = Bytes::from(tx.encode().to_vec());
                    match tx_sender.send(Recipients::All, tx_bytes, true).await {
                        Ok(_) => info!(tx_id = ?tx_id, "Transaction broadcast to peers"),
                        Err(e) => warn!(tx_id = ?tx_id, error = ?e, "Failed to broadcast transaction to peers"),
                    }
                }
            });
            gossip_tasks.push(tx_broadcast_task);
            
            // Start background task to receive transactions from other validators
            let gossip_mempool = shared_mempool.clone();
            let tx_gossip_task = context.with_label("tx_gossip").spawn(move |_| async move {
                use commonware_codec::DecodeExt;
                use alto_types::Transaction;
                use commonware_cryptography::Digestible;
                use commonware_p2p::Receiver;
                use tracing::{debug, warn};

                loop {
                    match tx_receiver.recv().await {
                        Ok((_, tx_bytes)) => {
                            match Transaction::decode(tx_bytes.as_ref()) {
                                Ok(tx) => {
                                    let tx_id = tx.digest();
                                    gossip_mempool.lock().unwrap().add(tx);
                                    debug!(tx_id = ?tx_id, "received transaction from peer, added to shared mempool");
                                }
                                Err(e) => warn!(error = ?e, "Failed to decode transaction from peer"),
                            }
                        }
                        Err(e) => {
                            warn!(error = ?e, "Transaction receiver error");
                            break;
                        }
                    }
                }
            });
            gossip_tasks.push(tx_gossip_task);
        } else {
            info!(gossip_enabled, "Transaction gossiping is DISABLED");
        }

        // Start all independent consensus instances
        let mut started_consensus = Vec::new();
        for (_i, engine) in consensus_engines.into_iter().enumerate() {
            let (pending, recovered, resolver, broadcaster, backfill, ancestor) = consensus_channels.remove(0);
            let started_engine = engine.start(pending, recovered, resolver, broadcaster, backfill, ancestor);
            started_consensus.push(started_engine);
        }

        // Spawn gatling thread if enabled
        let gatling_task = if let Some(gatling_rx) = gatling_rx {
            // Calculate validator index from sorted peers list
            let validator_index = peers
                .iter()
                .position(|p| p == &public_key)
                .expect("Public key not found in peers");
            
            Some(context.with_label("gatling").spawn(move |_| async move {
                gatling_thread(gatling_rx, validator_index, consensus_instances).await
            }))
        } else {
            None
        };

        // Wait for any task to error
        let mut all_tasks = vec![p2p, http_server];
        all_tasks.extend(gossip_tasks);
        all_tasks.extend(started_consensus);
        if let Some(task) = gatling_task {
            all_tasks.push(task);
        }
        if let Some(task) = submit_tx_task {
            all_tasks.push(task);
        }
        if let Err(e) = try_join_all(all_tasks).await {
            error!(?e, "task failed");
        }
    });
}
