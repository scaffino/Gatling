use alto_types::Transaction;
use commonware_cryptography::{ed25519::PublicKey, sha256::Digest, Digestible};
use commonware_runtime::Metrics;
use prometheus_client::metrics::gauge::Gauge;
use std::collections::{BTreeMap, HashMap, VecDeque};

/// The maximum number of transactions a single account can have in the mempool.
const MAX_BACKLOG: usize = 16;

/// The maximum number of transactions in the mempool.
const MAX_TRANSACTIONS: usize = 32_768;

/// A mempool for transactions.
pub struct Mempool {
    transactions: HashMap<Digest, Transaction>,
    tracked: HashMap<PublicKey, BTreeMap<u64, Digest>>,
    /// We store the public keys of the transactions to be processed next (rather than transactions
    /// received by digest) because we may receive transactions out-of-order (and/or some may have
    /// already been processed) and should just try return the transaction with the lowest nonce we
    /// are currently tracking.
    queue: VecDeque<PublicKey>,
    nonces: HashMap<PublicKey, u64>,

    unique: Gauge,
    accounts: Gauge,
}

impl Mempool {
    /// Create a new mempool.
    pub fn new(context: impl Metrics) -> Self {
        // Initialize metrics
        let unique = Gauge::default();
        let accounts = Gauge::default();
        context.register(
            "transactions",
            "Number of transactions in the mempool",
            unique.clone(),
        );
        context.register(
            "accounts",
            "Number of accounts in the mempool",
            accounts.clone(),
        );

        // Initialize mempool
        Self {
            transactions: HashMap::new(),
            tracked: HashMap::new(),
            queue: VecDeque::new(),
            nonces: HashMap::new(),

            unique,
            accounts,
        }
    }

    /// Add a transaction to the mempool.
    pub fn add(&mut self, tx: Transaction) {
        // If there are too many transactions, ignore
        if self.transactions.len() >= MAX_TRANSACTIONS {
            return;
        }

        // Determine if duplicate
        let digest = tx.digest();
        if self.transactions.contains_key(&digest) {
            // If we already have a transaction with this digest, we don't need to track it
            return;
        }

        // Generate a pseudo-nonce from transaction content
        // In this simple implementation, we use a counter per sender
        let public = tx.sender.clone();
        let nonce = *self.nonces.get(&public).unwrap_or(&0);
        
        // Track the transaction
        let entry = self.tracked.entry(public.clone()).or_default();

        // If there already exists a transaction at some nonce, return
        if entry.contains_key(&nonce) {
            return;
        }

        // Insert the transaction into the mempool
        assert!(entry.insert(nonce, digest).is_none());
        self.transactions.insert(digest, tx);
        
        // Increment nonce for next transaction from this sender
        self.nonces.insert(public.clone(), nonce + 1);

        // If there are too many transactions, remove the furthest in the future
        let entries = entry.len();
        if entries > MAX_BACKLOG {
            let (_, future) = entry.pop_last().unwrap();
            self.transactions.remove(&future);
        }

        // Add to queue if this is the first entry (otherwise the public key will already be
        // in the queue)
        if entries == 1 {
            self.queue.push_back(public);
        }

        // Update metrics
        self.unique.set(self.transactions.len() as i64);
        self.accounts.set(self.tracked.len() as i64);
    }

    /// Get the next transaction to process from the mempool.
    pub fn next(&mut self) -> Option<Transaction> {
        let tx = loop {
            // Get the transaction with the lowest nonce
            let address = self.queue.pop_front()?;
            let Some(tracked) = self.tracked.get_mut(&address) else {
                // We don't prune the queue when we drop a transaction, so we may need to
                // read through some untracked addresses.
                continue;
            };
            let Some((_, digest)) = tracked.pop_first() else {
                continue;
            };

            // If the address still has transactions, add it to the end of the queue (to
            // ensure everyone gets a chance to process their transactions)
            if !tracked.is_empty() {
                self.queue.push_back(address);
            } else {
                // If the address has no transactions, remove it from the tracked map
                self.tracked.remove(&address);
            }

            // Remove the transaction from the mempool
            let tx = self.transactions.remove(&digest).unwrap();
            break Some(tx);
        };

        // Update metrics
        self.unique.set(self.transactions.len() as i64);
        self.accounts.set(self.tracked.len() as i64);

        tx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_cryptography::{ed25519::PrivateKey, PrivateKeyExt, Signer};
    use commonware_runtime::{deterministic, Runner};

    #[test]
    fn test_add_single_transaction() {
        let runner = deterministic::Runner::default();
        runner.start(|ctx| async move {
            let mut mempool = Mempool::new(ctx);

            let sender_private = PrivateKey::from_seed(1);
            let receiver_private = PrivateKey::from_seed(2);
            let timestamp = 1634567890;
            let tx = Transaction::sign(&sender_private, receiver_private.public_key(), 100, timestamp);
            let digest = tx.digest();
            let sender = tx.sender.clone();

            mempool.add(tx);

            assert_eq!(mempool.transactions.len(), 1);
            assert!(mempool.transactions.contains_key(&digest));
            assert_eq!(mempool.tracked.len(), 1);
            assert!(mempool.tracked.contains_key(&sender));
            assert_eq!(mempool.queue.len(), 1);
        });
    }

    #[test]
    fn test_add_duplicate_transaction() {
        let runner = deterministic::Runner::default();
        runner.start(|ctx| async move {
            let mut mempool = Mempool::new(ctx);

            let sender_private = PrivateKey::from_seed(1);
            let receiver_private = PrivateKey::from_seed(2);
            let timestamp = 1634567890;
            let tx = Transaction::sign(&sender_private, receiver_private.public_key(), 100, timestamp);

            mempool.add(tx.clone());
            mempool.add(tx);

            assert_eq!(mempool.transactions.len(), 1);
            assert_eq!(mempool.tracked.len(), 1);
            assert_eq!(mempool.queue.len(), 1);
        });
    }

    #[test]
    fn test_next_single_transaction() {
        let runner = deterministic::Runner::default();
        runner.start(|ctx| async move {
            let mut mempool = Mempool::new(ctx);

            let sender_private = PrivateKey::from_seed(1);
            let receiver_private = PrivateKey::from_seed(2);
            let timestamp = 1634567890;
            let tx = Transaction::sign(&sender_private, receiver_private.public_key(), 100, timestamp);
            let expected_amount = tx.amount;

            mempool.add(tx);

            let next = mempool.next();
            assert!(next.is_some());
            assert_eq!(next.unwrap().amount, expected_amount);

            assert_eq!(mempool.transactions.len(), 0);
            assert_eq!(mempool.tracked.len(), 0);
            assert_eq!(mempool.queue.len(), 0);
        });
    }

    #[test]
    fn test_add_multiple_transactions_same_account() {
        let runner = deterministic::Runner::default();
        runner.start(|ctx| async move {
            let mut mempool = Mempool::new(ctx);

            let sender_private = PrivateKey::from_seed(1);
            let receiver_private = PrivateKey::from_seed(2);

            for amount in 1..=5 {
                let timestamp = 1634567890 + amount;
                let tx = Transaction::sign(&sender_private, receiver_private.public_key(), amount, timestamp);
                mempool.add(tx);
            }

            assert_eq!(mempool.transactions.len(), 5);
            assert_eq!(mempool.tracked.len(), 1);
            assert_eq!(mempool.queue.len(), 1);
        });
    }

    #[test]
    fn test_add_multiple_accounts() {
        let runner = deterministic::Runner::default();
        runner.start(|ctx| async move {
            let mut mempool = Mempool::new(ctx);

            for seed in 0..5 {
                let sender_private = PrivateKey::from_seed(seed);
                let receiver_private = PrivateKey::from_seed(100);
                let timestamp = 1634567890 + seed;
                let tx = Transaction::sign(&sender_private, receiver_private.public_key(), 100, timestamp);
                mempool.add(tx);
            }

            assert_eq!(mempool.transactions.len(), 5);
            assert_eq!(mempool.tracked.len(), 5);
            assert_eq!(mempool.queue.len(), 5);
        });
    }

    #[test]
    fn test_next_empty_mempool() {
        let runner = deterministic::Runner::default();
        runner.start(|ctx| async move {
            let mut mempool = Mempool::new(ctx);

            let next = mempool.next();
            assert!(next.is_none());
        });
    }
}

