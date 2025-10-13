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
                .required(true),
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

    // Parse arguments
    let validator_url = matches.get_one::<String>("validator").unwrap();
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

    // Submit to validator
    let client = reqwest::Client::new();
    let url = format!("{}/transaction", validator_url);
    
    println!("\n=== Submitting to Validator ===");
    println!("URL: {}", url);
    
    let response = client
        .post(&url)
        .body(tx.encode().to_vec())
        .send()
        .await
        .expect("Failed to send request");

    if response.status().is_success() {
        println!("✓ Transaction accepted by validator!");
        println!("\nTransaction has been submitted to the mempool.");
        println!("It will be included in an upcoming block.");
    } else {
        eprintln!("✗ Transaction rejected: {}", response.status());
        eprintln!("Response: {}", response.text().await.unwrap_or_default());
        std::process::exit(1);
    }
}

