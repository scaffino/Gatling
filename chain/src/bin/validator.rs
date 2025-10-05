use alto_chain::{engine, Config, Peers};
use alto_client::Client;
use alto_types::NAMESPACE;
use clap::{Arg, Command};
use commonware_codec::{Decode, DecodeExt};
use commonware_cryptography::{
    bls12381::primitives::{group, poly, variant::MinSig},
    ed25519::{PrivateKey, PublicKey},
    Signer,
};
use commonware_deployer::ec2::Hosts;
use commonware_p2p::authenticated::discovery as authenticated;
use commonware_runtime::{tokio, Metrics, Runner};
use commonware_utils::{from_hex_formatted, quorum, union_unique};
use futures::future::try_join_all;
use governor::Quota;
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroU32,
    path::PathBuf,
    str::FromStr,
    time::Duration,
};
use tracing::{error, info, Level};

const PENDING_CHANNEL: u32 = 0;
const RECOVERED_CHANNEL: u32 = 1;
const RESOLVER_CHANNEL: u32 = 2;
const BROADCASTER_CHANNEL: u32 = 3;
const BACKFILL_BY_DIGEST_CHANNEL: u32 = 4;

const LEADER_TIMEOUT: Duration = Duration::from_secs(1);
const NOTARIZATION_TIMEOUT: Duration = Duration::from_secs(2);
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

fn main() {
    // Parse arguments
    let matches = Command::new("validator")
        .about("Validator for an alto chain.")
        .arg(Arg::new("hosts").long("hosts").required(false))
        .arg(Arg::new("peers").long("peers").required(false))
        .arg(Arg::new("config").long("config").required(true))
        .arg(Arg::new("engines").long("engines").value_name("COUNT").help("Number of engine instances to start").default_value("2"))
        .get_matches();

    // Load ip file
    let hosts_file = matches.get_one::<String>("hosts");
    let peers_file = matches.get_one::<String>("peers");
    assert!(
        hosts_file.is_some() || peers_file.is_some(),
        "Either --hosts or --peers must be provided"
    );

    // Parse engines count
    let engines_count: usize = matches.get_one::<String>("engines")
        .unwrap()
        .parse()
        .expect("Invalid engines count");
    assert!(engines_count > 0, "Number of engines must be at least 1");
    assert!(engines_count <= 100, "Number of engines cannot exceed 100");

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

            let peer_keys = peers.keys().cloned().collect::<Vec<_>>();
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

            let peer_keys = peers.keys().cloned().collect::<Vec<_>>();
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
            engines_count,
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

        // Register channels for all engines dynamically
        let pending_limit = Quota::per_second(NonZeroU32::new(128).unwrap());
        let recovered_limit = Quota::per_second(NonZeroU32::new(128).unwrap());
        let resolver_limit = Quota::per_second(NonZeroU32::new(128).unwrap());
        let broadcaster_limit = Quota::per_second(NonZeroU32::new(8).unwrap());
        let backfill_quota = Quota::per_second(NonZeroU32::new(8).unwrap());

        let mut engine_channels = Vec::new();
        for i in 0..engines_count {
            let base_channel = if i == 0 { 0 } else { i as u32 * 10 };
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

            engine_channels.push((pending, recovered, resolver, broadcaster, backfill));
        }

        // Create network
        let p2p = network.start();

        // Create indexer
        let mut indexer = None;
        if let Some(uri) = config.indexer {
            indexer = Some(Client::new(&uri, identity));
        }

        // Create engines dynamically
        let mut engines = Vec::new();
        for i in 0..engines_count {
            let engine_config = engine::Config {
                blocker: oracle.clone(),
                partition_prefix: format!("engine_{}", i + 1),
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
            };
            let engine = engine::Engine::new(context.with_label(&format!("engine_{}", i + 1)), engine_config).await;
            engines.push(engine);
        }

        // Start engines
        let mut started_engines = Vec::new();
        for (_i, engine) in engines.into_iter().enumerate() {
            let (pending, recovered, resolver, broadcaster, backfill) = engine_channels.remove(0);
            let started_engine = engine.start(pending, recovered, resolver, broadcaster, backfill);
            started_engines.push(started_engine);
        }

        // Wait for any task to error
        let mut all_tasks = vec![p2p];
        all_tasks.extend(started_engines);
        if let Err(e) = try_join_all(all_tasks).await {
            error!(?e, "task failed");
        }
    });
}
