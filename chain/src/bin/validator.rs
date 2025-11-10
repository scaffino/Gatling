use alto_chain::{engine, http_server, Config, Peers};
use alto_client::Client;
use alto_types::NAMESPACE;
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
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroU32,
    path::PathBuf,
    str::FromStr,
    sync::{Arc, atomic::{AtomicU64, Ordering}},
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

// Timeouts tuned for 1 proposal per second with rotating leaders
// Goal: Each view completes in ~1 second, leader proposes at second boundary
// 
// LEADER_TIMEOUT: Must allow leader to wait for next second boundary + propose
//   - Leader may need to wait up to 1s for their time slot
//   - Then proposal propagates (~100-200ms)
//   - Setting to 2.5s: allows 1s wait + 1.5s buffer for network delays
//
// NOTARIZATION_TIMEOUT: Must allow signature collection from all validators
//   - After proposal, validators sign and send back (~200-400ms)
//   - With network delays and multiple validators, need generous timeout
//   - Setting to 3s: ensures consensus stability
//
// Result: Views may take 1-2 seconds, but consensus remains stable
const LEADER_TIMEOUT: Duration = Duration::from_millis(10000);
const NOTARIZATION_TIMEOUT: Duration = Duration::from_millis(12000);
const NULLIFY_RETRY: Duration = Duration::from_secs(10);
const ACTIVITY_TIMEOUT: u64 = 256;
const SKIP_TIMEOUT: u64 = 4;
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);
const FETCH_CONCURRENT: usize = 4;
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
    assert!(consensus_instances <= 10, "Number of consensus instances cannot exceed 10");

    // Parse gossip flag (default: enabled if neither flag is specified)
    let gossip_enabled = if matches.get_flag("no-gossip-txs") {
        false
    } else {
        true // Default to enabled, or explicitly enabled with --gossip-txs
    };

    // Parse gatling flag (default: disabled)
    let gatling_enabled = matches.get_flag("gatling");

    // Load config
    let config_file = matches.get_one::<String>("config").unwrap();
    let config_file = std::fs::read_to_string(config_file).expect("Could not read config file");
    let config: Config = serde_yaml::from_str(&config_file).expect("Could not parse config file");
    let key = from_hex_formatted(&config.private_key).expect("Could not parse private key");
    let signer = PrivateKey::decode(key.as_ref()).expect("Private key is invalid");
    let public_key = signer.public_key();

    // Gate startup until genesis timestamp (no runtime/network/consensus before this time)
    {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        if now_secs < config.genesis_timestamp {
            let wait_secs = config.genesis_timestamp - now_secs;
            info!(genesis_timestamp = config.genesis_timestamp, wait_secs, "waiting for genesis timestamp before starting validator");
            std::thread::sleep(std::time::Duration::from_secs(wait_secs));
        }
    }

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
        let (ip, peers, bootstrappers, peer_addresses) = if let Some(hosts_file) = hosts_file {
            let hosts_file = std::fs::read_to_string(hosts_file).unwrap();
            let hosts: Hosts =
                serde_yaml::from_str(&hosts_file).expect("Could not parse peers file");
            let peers_map: HashMap<PublicKey, IpAddr> = hosts
                .hosts
                .into_iter()
                .map(|peer| {
                    let key = from_hex_formatted(&peer.name).expect("Could not parse peer key");
                    let key = PublicKey::decode(key.as_ref()).expect("Peer key is invalid");
                    (key, peer.ip)
                })
                .collect();

            let mut peer_keys: Vec<_> = peers_map.keys().cloned().collect();
            peer_keys.sort();  // CRITICAL: Sort to ensure consistent ordering across all validators
            let mut bootstrappers = Vec::new();
            for bootstrapper in &config.bootstrappers {
                let key =
                    from_hex_formatted(bootstrapper).expect("Could not parse bootstrapper key");
                let key = PublicKey::decode(key.as_ref()).expect("Bootstrapper key is invalid");
                let ip = peers_map.get(&key).expect("Could not find bootstrapper in IPs");
                let bootstrapper_socket = format!("{}:{}", ip, config.port);
                let bootstrapper_socket = SocketAddr::from_str(&bootstrapper_socket)
                    .expect("Could not parse bootstrapper socket");
                bootstrappers.push((key, bootstrapper_socket));
            }
            let ip = peers_map.get(&public_key).expect("Could not find self in IPs");
            // Create address mapping for logging
            let peer_addresses: HashMap<PublicKey, String> = peers_map
                .iter()
                .map(|(key, ip)| (key.clone(), format!("{}:{}", ip, config.port)))
                .collect();
            (*ip, peer_keys, bootstrappers, peer_addresses)
        } else {
            let peers_file = std::fs::read_to_string(peers_file.unwrap()).unwrap();
            let peers: Peers =
                serde_yaml::from_str(&peers_file).expect("Could not parse peers file");
            let peers_map: HashMap<PublicKey, SocketAddr> = peers
                .addresses
                .into_iter()
                .map(|peer| {
                    let key = from_hex_formatted(&peer.0).expect("Could not parse peer key");
                    let key = PublicKey::decode(key.as_ref()).expect("Peer key is invalid");
                    (key, peer.1)
                })
                .collect();

            let mut peer_keys: Vec<_> = peers_map.keys().cloned().collect();
            peer_keys.sort();  // CRITICAL: Sort to ensure consistent ordering across all validators
            let mut bootstrappers = Vec::new();
            for bootstrapper in &config.bootstrappers {
                let key =
                    from_hex_formatted(bootstrapper).expect("Could not parse bootstrapper key");
                let key = PublicKey::decode(key.as_ref()).expect("Bootstrapper key is invalid");
                let socket = peers_map.get(&key).expect("Could not find bootstrapper in IPs");
                bootstrappers.push((key, *socket));
            }
            let ip = peers_map
                .get(&public_key)
                .expect("Could not find self in IPs")
                .ip();
            // Create address mapping for logging
            let peer_addresses: HashMap<PublicKey, String> = peers_map
                .iter()
                .map(|(key, socket)| (key.clone(), socket.to_string()))
                .collect();
            (ip, peer_keys, bootstrappers, peer_addresses)
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

        // ============================================================================
        // STARTUP DIAGNOSTIC LOGGING
        // ============================================================================
        
        // Log timeout configuration
        info!(
            "[setup] Timeout configuration: leader_timeout={}ms, notarization_timeout={}ms, nullify_retry={}s, fetch_timeout={}ms, activity_timeout={}, skip_timeout={}",
            LEADER_TIMEOUT.as_millis(),
            NOTARIZATION_TIMEOUT.as_millis(),
            NULLIFY_RETRY.as_secs(),
            FETCH_TIMEOUT.as_millis(),
            ACTIVITY_TIMEOUT,
            SKIP_TIMEOUT
        );
        
        // Log network configuration
        info!(
            "[setup] Network configuration: peers={}, mailbox_size={}, message_backlog={}, max_message_size={}MB, worker_threads={}",
            peers.len(),
            config.mailbox_size,
            config.message_backlog,
            MAX_MESSAGE_SIZE / (1024 * 1024),
            config.worker_threads
        );
        
        // Log fetch configuration
        info!(
            "[setup] Fetch configuration: fetch_concurrent={}, max_fetch_count={}, max_fetch_size={}KB",
            FETCH_CONCURRENT,
            MAX_FETCH_COUNT,
            MAX_FETCH_SIZE / 1024
        );
        
        // Log peer configuration
        info!("[setup] Peer configuration ({} peers):", peers.len());
        for (idx, peer_key) in peers.iter().enumerate() {
            if let Some(peer_addr) = peer_addresses.get(peer_key) {
                info!("[setup]   validator_{}: {} (public_key: {:?})", 
                      idx, peer_addr, peer_key);
            } else {
                warn!("[setup]   validator_{}: address not found (public_key: {:?})", 
                      idx, peer_key);
            }
        }
        
        // Log bootstrapper configuration
        info!("[setup] Bootstrapper configuration ({} bootstrappers):", bootstrappers.len());
        for (idx, (bootstrapper_key, bootstrapper_socket)) in bootstrappers.iter().enumerate() {
            info!("[setup]   bootstrapper_{}: {} (public_key: {:?})", 
                  idx, bootstrapper_socket, bootstrapper_key);
        }
        
        // Log consensus instances configuration
        info!(
            "[setup] Consensus instances: count={}, genesis_timestamp={} ({})",
            consensus_instances,
            config.genesis_timestamp,
            genesis_time_utc
        );
        
        // Log self configuration
        info!(
            "[setup] Self configuration: public_key={:?}, identity={:?}, ip={}, port={}, transaction_port={}",
            public_key, identity, ip, config.port, config.transaction_port
        );
        
        // Validate setup
        if peers.len() < 2 {
            warn!("[setup] WARNING: Only {} peer(s) configured. Consensus requires at least 2 validators.", peers.len());
        }
        if threshold as usize > peers.len() {
            warn!("[setup] WARNING: Threshold ({}) exceeds number of peers ({}). This may cause consensus issues.", threshold, peers.len());
        }
        if bootstrappers.len() == 0 {
            warn!("[setup] WARNING: No bootstrappers configured. Network discovery may be affected.");
        }
        if LEADER_TIMEOUT.as_millis() < 1000 {
            warn!("[setup] WARNING: Leader timeout ({}) is very short. May cause frequent timeouts.", LEADER_TIMEOUT.as_millis());
        }
        if NOTARIZATION_TIMEOUT.as_millis() < 1000 {
            warn!("[setup] WARNING: Notarization timeout ({}) is very short. May cause frequent view changes.", NOTARIZATION_TIMEOUT.as_millis());
        }

        // Configure network
        info!("[network] Initializing P2P network: bind_addr=0.0.0.0:{}, public_addr={}:{}, max_message_size={}MB, mailbox_size={}",
              config.port, ip, config.port, MAX_MESSAGE_SIZE / (1024 * 1024), config.mailbox_size);
        
        let p2p_namespace = union_unique(NAMESPACE, b"_P2P");
        let mut p2p_cfg = authenticated::Config::aggressive(
            signer.clone(),
            &p2p_namespace,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), config.port),
            SocketAddr::new(ip, config.port),
            bootstrappers.clone(),
            MAX_MESSAGE_SIZE,
        );
        p2p_cfg.mailbox_size = config.mailbox_size;

        // Start p2p
        info!("[network] Creating P2P network instance...");
        let (mut network, mut oracle) =
            authenticated::Network::new(context.with_label("network"), p2p_cfg);

        // Provide authorized peers
        info!("[network] Registering {} authorized peers with network oracle", peers.len());
        oracle.register(0, peers.clone()).await;
        info!("[network] Peer registration complete");

        // Register channels for each independent consensus instance
        info!("[network] Registering network channels for {} consensus instances", consensus_instances);
        
        let pending_limit = Quota::per_second(NonZeroU32::new(128).unwrap());
        let recovered_limit = Quota::per_second(NonZeroU32::new(128).unwrap());
        let resolver_limit = Quota::per_second(NonZeroU32::new(128).unwrap());
        let broadcaster_limit = Quota::per_second(NonZeroU32::new(8).unwrap());
        let backfill_quota = Quota::per_second(NonZeroU32::new(8).unwrap());
        let ancestor_quota = Quota::per_second(NonZeroU32::new(16).unwrap());
        let transaction_limit = Quota::per_second(NonZeroU32::new(256).unwrap());

        info!("[network] Channel quotas: pending=128/s, recovered=128/s, resolver=128/s, broadcaster=8/s, backfill=8/s, ancestor=16/s, transaction=256/s");
        info!("[network] Message backlog size: {}", config.message_backlog);

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

            info!("[network] Registering channels for consensus instance {}: pending={}, recovered={}, resolver={}, broadcaster={}, backfill={}, ancestor={}",
                  i + 1, pending_channel, recovered_channel, resolver_channel, broadcaster_channel, backfill_channel, ancestor_channel);

            let pending = network.register(pending_channel, pending_limit, config.message_backlog);
            let recovered = network.register(recovered_channel, recovered_limit, config.message_backlog);
            let resolver = network.register(resolver_channel, resolver_limit, config.message_backlog);
            let broadcaster = network.register(broadcaster_channel, broadcaster_limit, config.message_backlog);
            let backfill = network.register(backfill_channel, backfill_quota, config.message_backlog);
            let ancestor = network.register(ancestor_channel, ancestor_quota, config.message_backlog);

            consensus_channels.push((pending, recovered, resolver, broadcaster, backfill, ancestor));
            info!("[network] Registered 6 channels for consensus instance {}", i + 1);
        }

        // Register a shared transaction channel for gossip (shared across all consensus instances)
        info!("[network] Registering shared transaction channel: channel={}, quota=256/s", TRANSACTION_CHANNEL);
        let (mut tx_sender, mut tx_receiver) = network.register(TRANSACTION_CHANNEL, transaction_limit, config.message_backlog);
        info!("[network] Transaction channel registered");

        // Create network
        info!("[network] Starting P2P network...");
        info!("[network] Bootstrap peers: {} bootstrapper(s) configured", bootstrappers.len());
        for (idx, (_, bootstrapper_socket)) in bootstrappers.iter().enumerate() {
            info!("[network]   bootstrap_{}: {}", idx, bootstrapper_socket);
        }
        let p2p = network.start();
        info!("[network] P2P network started successfully");

        // Create indexer
        let mut indexer = None;
        if let Some(uri) = config.indexer {
            indexer = Some(Client::new(&uri, identity));
        }
        
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
        
        // Create multiple independent consensus instances
        let mut consensus_engines = Vec::new();
        for i in 0..consensus_instances {
            // Each consensus instance has a unique chain ID via partition_prefix
            // This makes them completely independent blockchains
            let chain_id = i + 1;
            // Create unique namespace for each instance to enable independent leader schedules
            let namespace = format!("{}_{}",
                std::str::from_utf8(NAMESPACE).unwrap(),
                chain_id).into_bytes();
            
            // Calculate time offset for this instance to stagger proposals evenly across the second
            // For N instances: instance 0 gets 0ms, instance 1 gets 1000/N ms, instance 2 gets 2000/N ms, etc.
            let proposal_offset_ms = (i * 1000 / consensus_instances) as u64;
            tracing::info!("Consensus instance {} will send proposals at offset {} ms within each second", 
                          chain_id, proposal_offset_ms);
            
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
                lag_threshold: 1, // Default threshold: skip wait if 1+ views behind
                total_instances: consensus_instances,
                genesis_timestamp_secs: config.genesis_timestamp,
            };
            let engine = engine::Engine::new(
                context.with_label(&format!("consensus_{}", chain_id)), 
                engine_config
            ).await;
            consensus_engines.push(engine);
        }
        
        // Collect all application mailboxes for transaction distribution
        let all_mailboxes: Vec<_> = consensus_engines
            .iter()
            .map(|e| e.application_mailbox().clone())
            .collect();

        // Create channel for broadcasting transactions (only if gossip is enabled)
        let broadcast_channel = if gossip_enabled {
            Some(futures::channel::mpsc::unbounded())
        } else {
            None
        };
        
        // Start HTTP server for transaction submission in background
        // HTTP server will distribute transactions to all consensus instances
        let transaction_addr = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            config.transaction_port,
        );
        let http_mailboxes = all_mailboxes.clone();
        let broadcast_tx_for_http = broadcast_channel.as_ref().map(|(tx, _)| tx.clone());
        let http_server = context.with_label("http_server").spawn(move |_| async move {
            if let Err(e) = http_server::start_server_multi(
                transaction_addr, 
                http_mailboxes, 
                broadcast_tx_for_http
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
                        Ok(_) => info!(tx_id = ?tx_id, "[network] Transaction broadcast to peers"),
                        Err(e) => warn!(tx_id = ?tx_id, error = ?e, "[network] ERROR: Failed to broadcast transaction to peers. May indicate network connectivity issues."),
                    }
                }
            });
            gossip_tasks.push(tx_broadcast_task);
            
            // Start background task to receive transactions from other validators
            // Distribute to ALL consensus instances' mempools
            let mut gossip_mailboxes = all_mailboxes.clone();
            let tx_gossip_task = context.with_label("tx_gossip").spawn(move |_| async move {
                use commonware_codec::DecodeExt;
                use alto_types::Transaction;
                use commonware_cryptography::Digestible;
                use commonware_p2p::Receiver;
                use tracing::{info, warn};
                
                loop {
                    match tx_receiver.recv().await {
                        Ok((_, tx_bytes)) => {
                            // Decode the transaction
                            match Transaction::decode(tx_bytes.as_ref()) {
                                Ok(tx) => {
                                    let tx_id = tx.digest();
                                    // Submit to ALL consensus instances' mempools
                                    for (idx, mailbox) in gossip_mailboxes.iter_mut().enumerate() {
                                        match mailbox.submit_transaction(tx.clone()).await {
                                            Ok(_) => info!(
                                                "[consensus_{}] [network] Transaction {:?} received from peer and added to mempool",
                                                idx + 1, tx_id
                                            ),
                                            Err(e) => warn!(
                                                tx_id = ?tx_id, 
                                                consensus_id = idx + 1,
                                                error = %e, 
                                                "[network] ERROR: Failed to add peer transaction to mempool. May indicate mempool full or invalid transaction."
                                            ),
                                        }
                                    }
                                }
                                Err(e) => warn!(error = ?e, "[network] ERROR: Failed to decode transaction from peer. May indicate message corruption or protocol mismatch."),
                            }
                        }
                        Err(e) => {
                            warn!(error = ?e, "[network] ERROR: Transaction receiver error. May indicate network connectivity issues.");
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
        
        // Spawn periodic health monitoring task (every 30 seconds)
        let instance_views_health = instance_views.clone();
        let genesis_timestamp_health = config.genesis_timestamp;
        let consensus_instances_health = consensus_instances;
        let peers_count = peers.len();
        let health_monitor_task = context.with_label("health_monitor").spawn(move |ctx| async move {
            loop {
                // Sleep for 30 seconds using context's sleep method
                ctx.sleep(Duration::from_secs(30)).await;
                
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                let time_since_genesis = now_secs.saturating_sub(genesis_timestamp_health);
                
                // Collect current views for all instances
                let mut view_status = Vec::new();
                for i in 0..consensus_instances_health {
                    let view = instance_views_health[i].load(Ordering::Relaxed);
                    view_status.push(format!("instance_{}={}", i + 1, view));
                }
                
                info!(
                    "[health] System status: time_since_genesis={}s, consensus_instances={}, views=[{}], peers_configured={}",
                    time_since_genesis,
                    consensus_instances_health,
                    view_status.join(", "),
                    peers_count
                );
                
                // Check for potential issues
                if time_since_genesis > 60 {
                    // After 60 seconds, check if views are progressing
                    let mut stalled_instances = Vec::new();
                    for i in 0..consensus_instances_health {
                        let view = instance_views_health[i].load(Ordering::Relaxed);
                        if view == 0 && time_since_genesis > 120 {
                            // Instance hasn't progressed past view 0 after 2 minutes
                            stalled_instances.push(i + 1);
                        }
                    }
                    if !stalled_instances.is_empty() {
                        warn!(
                            "[health] WARNING: Potential stalled instances detected: {:?}. Views may not be progressing.",
                            stalled_instances
                        );
                    }
                }
            }
        });

        // Wait for any task to error
        let mut all_tasks = vec![p2p, http_server, health_monitor_task];
        all_tasks.extend(gossip_tasks);
        all_tasks.extend(started_consensus);
        if let Some(task) = gatling_task {
            all_tasks.push(task);
        }
        if let Err(e) = try_join_all(all_tasks).await {
            error!(?e, "task failed");
        }
    });
}
