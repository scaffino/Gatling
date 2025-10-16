use alto_types::Transaction;
use commonware_codec::{Encode, DecodeExt};
use commonware_cryptography::{ed25519::PrivateKey, PrivateKeyExt, Signer, Digestible};

fn main() {
    // Create a transaction like the client does
    let sender = PrivateKey::from_seed(1);
    let receiver = PrivateKey::from_seed(2).public_key();
    let amount = 100;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs();

    // Create and encode transaction
    let tx = Transaction::sign(&sender, receiver, amount, timestamp);
    let tx_bytes = tx.encode().to_vec();
    
    println!("=== Transaction Encoding Test ===");
    println!("Sender:    {:?}", tx.sender);
    println!("Receiver:  {:?}", tx.receiver);
    println!("Amount:    {}", tx.amount);
    println!("Timestamp: {}", tx.timestamp);
    println!("Encoded size: {} bytes", tx_bytes.len());
    println!("\nEncoded bytes (hex): {}", hex::encode(&tx_bytes));
    
    // Try to decode it
    match Transaction::decode(tx_bytes.as_ref()) {
        Ok(decoded) => {
            println!("\n✓ Successfully decoded!");
            println!("Decoded sender:    {:?}", decoded.sender);
            println!("Decoded receiver:  {:?}", decoded.receiver);
            println!("Decoded amount:    {}", decoded.amount);
            println!("Decoded timestamp: {}", decoded.timestamp);
            println!("\nVerification: {}", decoded.verify());
        }
        Err(e) => {
            println!("\n✗ Failed to decode: {:?}", e);
        }
    }
}

