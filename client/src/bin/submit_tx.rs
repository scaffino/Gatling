use alto_types::Transaction;
use clap::{Arg, Command};
use commonware_codec::{Encode, DecodeExt};
use commonware_cryptography::{ed25519::{PrivateKey, PublicKey}, PrivateKeyExt, Signer, Digestible};

#[tokio::main]
async fn main() {
    // Parse arguments
    let matches = Command::new("submit_tx")
        .about("Submit a transaction to an alto validator")
        .arg(
            Arg::new("validator")
                .long("validator")
                .value_name("URL")
                .help("Validator URL (e.g., http://localhost:8081)")
                .conflicts_with("validator-all"),
        )
        .arg(
            Arg::new("validator-all")
                .long("validator-all")
                .help("Submit to all validators (ports 8081-8084)")
                .action(clap::ArgAction::SetTrue)
                .conflicts_with("validator"),
        )
        .arg(
            Arg::new("sender-seed")
                .long("sender-seed")
                .value_name("SEED")
                .help("Sender private key seed (u64)")
                .required(true),
        )
        .arg(
            Arg::new("receiver")
                .long("receiver")
                .value_name("PUBLIC_KEY")
                .help("Receiver public key (hex)")
                .required(true),
        )
        .arg(
            Arg::new("amount")
                .long("amount")
                .value_name("AMOUNT")
                .help("Amount to send")
                .required(true),
        )
        .get_matches();

    // Validate that either --validator or --validator-all is provided
    let validator_all = matches.get_flag("validator-all");
    if !validator_all && !matches.contains_id("validator") {
        eprintln!("Error: Either --validator or --validator-all must be specified");
        std::process::exit(1);
    }

    // Determine validator URLs
    let validator_urls: Vec<String> = if validator_all {
        // Default local validator ports
        vec![
            "http://localhost:8081".to_string(),
            "http://localhost:8082".to_string(),
            "http://localhost:8083".to_string(),
            "http://localhost:8084".to_string(),
        ]
    } else {
        vec![matches.get_one::<String>("validator").unwrap().to_string()]
    };

    let sender_seed: u64 = matches
        .get_one::<String>("sender-seed")
        .unwrap()
        .parse()
        .expect("Invalid sender seed");
    let receiver_hex = matches.get_one::<String>("receiver").unwrap();
    let amount: u64 = matches
        .get_one::<String>("amount")
        .unwrap()
        .parse()
        .expect("Invalid amount");

    // Parse receiver public key
    let receiver_bytes = hex::decode(receiver_hex).expect("Invalid receiver hex");
    let receiver = PublicKey::decode(receiver_bytes.as_ref()).expect("Invalid receiver public key");

    // Create sender key
    let sender = PrivateKey::from_seed(sender_seed);
    println!("Sender public key: {}", hex::encode(sender.public_key().as_ref()));

    // Create transaction
    let tx = Transaction::sign(&sender, receiver, amount);
    println!("\n=== Creating Transaction ===");
    println!("From:   {}", hex::encode(tx.sender.as_ref()));
    println!("To:     {}", hex::encode(tx.receiver.as_ref()));
    println!("Amount: {}", tx.amount);
    println!("Digest: {:?}", tx.digest());

    // Submit to validator(s)
    let client = reqwest::Client::new();
    let tx_bytes = tx.encode().to_vec();
    
    println!("\n=== Submitting to Validator{} ===", if validator_urls.len() > 1 { "s" } else { "" });
    
    let mut all_success = true;
    let mut success_count = 0;
    
    for validator_url in &validator_urls {
        let url = format!("{}/transaction", validator_url);
        println!("URL: {}", url);
        
        let response = client
            .post(&url)
            .body(tx_bytes.clone())
            .send()
            .await;

        match response {
            Ok(resp) if resp.status().is_success() => {
                println!("  ✓ Transaction accepted by {}!", validator_url);
                success_count += 1;
            }
            Ok(resp) => {
                eprintln!("  ✗ Transaction rejected by {}: {}", validator_url, resp.status());
                eprintln!("  Response: {}", resp.text().await.unwrap_or_default());
                all_success = false;
            }
            Err(e) => {
                eprintln!("  ✗ Failed to connect to {}: {}", validator_url, e);
                all_success = false;
            }
        }
    }

    println!("\n=== Summary ===");
    println!("Submitted to {}/{} validator(s) successfully", success_count, validator_urls.len());
    
    if all_success {
        println!("\nTransaction has been submitted to the mempool.");
        println!("It will be included in an upcoming block.");
    } else {
        eprintln!("\nSome submissions failed. Check the errors above.");
        std::process::exit(1);
    }
}

