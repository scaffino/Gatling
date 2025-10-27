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
use commonware_runtime::{tokio, Metrics, Runner, Spawner};
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
    time::Duration,
};
use tracing::{debug, error, info, Level};

const PENDING_CHANNEL: u32 = 0;
const RECOVERED_CHANNEL: u32 = 1;
const RESOLVER_CHANNEL: u32 = 2;
const BROADCASTER_CHANNEL: u32 = 3;
const BACKFILL_BY_DIGEST_CHANNEL: u32 = 4;
const TRANSACTION_CHANNEL: u32 = 5;

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
const LEADER_TIMEOUT: Duration = Duration::from_millis(2500);
const NOTARIZATION_TIMEOUT: Duration = Duration::from_millis(3000);
const NULLIFY_RETRY: Duration = Duration::from_secs(10);
const ACTIVITY_TIMEOUT: u64 = 256;
const SKIP_TIMEOUT: u64 = 32;
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);
const FETCH_CONCURRENT: usize = 4;
const MAX_MESSAGE_SIZE: usize = 1024 * 1024;
const MAX_FETCH_COUNT: usize = 16;
const MAX_FETCH_SIZE: usize = 512 * 1024;
const BLOCKS_FREEZER_TABLE_INITIAL_SIZE: u32 = 2u32.pow(21); // 100MB
const FINALIZED_FREEZER_TABLE_INITIAL_SIZE: u32 = 2u32.pow(21); // 100MB

/// Gatling thread that orders blocks from all consensus instances by view and instance number.
async fn gatling_thread(
    mut rx: futures::channel::mpsc::UnboundedReceiver<engine::GatlingEvent>,
    validator_index: usize,
    num_instances: usize,
) {
    use std::collections::BTreeMap;
    
    // Per-instance queues of finalized blocks, keyed by view number
    let mut instance_queues: Vec<BTreeMap<u64, alto_types::Block>> = 
        (0..num_instances).map(|_| BTreeMap::new()).collect();
    
    // Track the highest view we've finalized for each instance
    let mut finalized_views: Vec<u64> = vec![0; num_instances];
    
    info!("[gatling] Gatling thread started for {} instances", num_instances);
    
    while let Some(event) = rx.next().await {
        let instance_idx = event.instance_id - 1; // Convert 1-based to 0-based for indexing
        
        // Add the block to the instance's queue
        instance_queues[instance_idx].insert(event.view, event.block);
        
        // Try to finalize blocks in global order
        loop {
            // Find the next block that can be finalized according to:
            // Rule 1: Views are finalized in ascending order
            // Rule 2: For the same view, instances finalize in order (0, 1, 2, ...)
            
            let mut can_finalize = None;
            
            // Step 1: Collect all unique pending views across ALL instances
            let mut all_pending_views: Vec<u64> = Vec::new();
            for k in 0..num_instances {
                if let Some((&view, _)) = instance_queues[k].iter().next() {
                    all_pending_views.push(view);
                }
            }
            
            // If no pending views, nothing to finalize
            if all_pending_views.is_empty() {
                break;
            }
            
            // Step 2: Sort and deduplicate to get unique views in ascending order
            all_pending_views.sort_unstable();
            all_pending_views.dedup();
            
            // Step 3: Try to finalize views in order
            // For each view, check each instance in order (0, 1, 2, ...)
            for view in all_pending_views {
                for inst_idx in 0..num_instances {
                    if let Some((&pending_view, block)) = instance_queues[inst_idx].iter().next() {
                        // Check if this instance has a block at this view
                        if pending_view == view {
                            // Check: all instances with index < inst_idx have finalized this view
                            let all_lower_instances_finalized = (0..inst_idx).all(|k| finalized_views[k] >= view);
                            
                            if all_lower_instances_finalized {
                                // This instance can finalize - it's the lowest index eligible for this view
                                can_finalize = Some((inst_idx, view, block.clone()));
                                break; // Exit inner loop
                            }
                        }
                    }
                }
                
                // If we found a block to finalize, exit the outer loop too
                if can_finalize.is_some() {
                    break;
                }
            }
            
            // Finalize the block if we found one
            if let Some((inst_idx, view, block)) = can_finalize {
                let instance_id = inst_idx + 1; // Convert back to 1-based for display
                let tx_count = block.transactions.len();
                
                // Log the globally finalized block
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
                
                // Update tracking - Rule 2: Implicit finalization of gaps
                // When instance N finalizes view V, mark all views up to V as finalized
                // If previous_finalized was 2 and we're finalizing view 5, then 3, 4, and 5 are now finalized
                finalized_views[inst_idx] = view;
                
                // Remove the finalized block from queue
                instance_queues[inst_idx].remove(&view);
                
                // Remove any pending blocks with views < v - they are now implicitly finalized
                // If there was a gap (e.g., finalized view 2, now finalizing view 5), views 3 and 4 are implicitly finalized
                let mut views_to_remove = Vec::new();
                for (&queued_view, _) in instance_queues[inst_idx].iter() {
                    if queued_view < view {
                        views_to_remove.push(queued_view);
                    }
                }
                for &view_to_remove in &views_to_remove {
                    instance_queues[inst_idx].remove(&view_to_remove);
                    debug!("Removed implicitly finalized block from instance {} at view {} (instance now finalized view {})", 
                           instance_id, view_to_remove, view);
                }
            } else {
                // No more blocks can be finalized right now
                break;
            }
        }
    }
    
    info!("[gatling] Gatling thread stopped");
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

            let pending = network.register(pending_channel, pending_limit, config.message_backlog);
            let recovered = network.register(recovered_channel, recovered_limit, config.message_backlog);
            let resolver = network.register(resolver_channel, resolver_limit, config.message_backlog);
            let broadcaster = network.register(broadcaster_channel, broadcaster_limit, config.message_backlog);
            let backfill = network.register(backfill_channel, backfill_quota, config.message_backlog);

            consensus_channels.push((pending, recovered, resolver, broadcaster, backfill));
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

        // Create shared state for tracking included transactions across all consensus instances
        // This prevents the same transaction from being included in multiple independent blockchains
        let included_transactions = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        
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
                included_transactions: included_transactions.clone(),
                proposal_offset_ms,
                gatling_tx: gatling_tx.clone(),
                gatling_instance_id: chain_id,
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
                        Ok(_) => info!(tx_id = ?tx_id, "Transaction broadcast to peers"),
                        Err(e) => warn!(tx_id = ?tx_id, error = ?e, "Failed to broadcast transaction to peers"),
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
                                                "[consensus_{}] Transaction {:?} received from peer and added to mempool",
                                                idx + 1, tx_id
                                            ),
                                            Err(e) => warn!(
                                                tx_id = ?tx_id, 
                                                consensus_id = idx + 1,
                                                error = %e, 
                                                "Failed to add peer transaction to mempool"
                                            ),
                                        }
                                    }
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
            let (pending, recovered, resolver, broadcaster, backfill) = consensus_channels.remove(0);
            let started_engine = engine.start(pending, recovered, resolver, broadcaster, backfill);
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
        if let Err(e) = try_join_all(all_tasks).await {
            error!(?e, "task failed");
        }
    });
}
