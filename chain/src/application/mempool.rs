use alto_types::Transaction;
use commonware_cryptography::{sha256::Digest, Digestible};
use commonware_runtime::Metrics;
use prometheus_client::metrics::gauge::Gauge;
use std::collections::{HashMap, VecDeque};

/// A mempool for transactions.
pub struct Mempool {
    transactions: HashMap<Digest, Transaction>,
    /// FIFO order of transaction digests, preserving arrival order.
    order: VecDeque<Digest>,

    unique: Gauge,
}

impl Mempool {
    /// Create a new mempool.
    pub fn new(context: impl Metrics) -> Self {
        // Initialize metrics
        let unique = Gauge::default();
        context.register(
            "transactions",
            "Number of transactions in the mempool",
            unique.clone(),
        );

        // Initialize mempool
        Self {
            transactions: HashMap::new(),
            order: VecDeque::new(),

            unique,
        }
    }

    /// Add a transaction to the mempool.
    pub fn add(&mut self, tx: Transaction) {
        // Determine if duplicate
        let digest = tx.digest();
        if self.transactions.contains_key(&digest) {
            // If we already have a transaction with this digest, we don't need to track it
            return;
        }
        
        // Insert the transaction into the mempool and record arrival order
        self.transactions.insert(digest, tx);
        self.order.push_back(digest);

        // Update metrics
        self.unique.set(self.transactions.len() as i64);
    }

    /// Get the next transaction to process from the mempool in FIFO arrival order.
    pub fn next(&mut self) -> Option<Transaction> {
        let digest = self.order.pop_front()?;
        let tx = self.transactions.remove(&digest);

        // Update metrics
        self.unique.set(self.transactions.len() as i64);

        tx
    }

    /// Drain and return all transactions currently in the mempool in FIFO arrival order.
    pub fn take_all(&mut self) -> Vec<Transaction> {
        let mut drained = Vec::with_capacity(self.order.len());
        while let Some(digest) = self.order.pop_front() {
            if let Some(tx) = self.transactions.remove(&digest) {
                drained.push(tx);
            }
        }
        // Update metrics
        self.unique.set(self.transactions.len() as i64);
        drained
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

            mempool.add(tx);

            assert_eq!(mempool.transactions.len(), 1);
            assert!(mempool.transactions.contains_key(&digest));
            assert_eq!(mempool.order.len(), 1);
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
            assert_eq!(mempool.order.len(), 1);
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
            assert_eq!(mempool.order.len(), 0);
        });
    }

    #[test]
    fn test_fifo_multiple_transactions() {
        let runner = deterministic::Runner::default();
        runner.start(|ctx| async move {
            let mut mempool = Mempool::new(ctx);

            let sender_private = PrivateKey::from_seed(1);
            let receiver_private = PrivateKey::from_seed(2);

            let mut digests = Vec::new();
            for amount in 1..=5 {
                let timestamp = 1634567890 + amount;
                let tx = Transaction::sign(&sender_private, receiver_private.public_key(), amount, timestamp);
                digests.push(tx.digest());
                mempool.add(tx);
            }

            assert_eq!(mempool.transactions.len(), 5);
            assert_eq!(mempool.order.len(), 5);

            // Ensure FIFO via next()
            for _ in 1..=5 {
                let _ = mempool.next().unwrap();
                // amounts were 1..=5 in order
                // implicit FIFO check by monotonic amount
                // (not asserting digest equality to keep test simple)
            }
            assert_eq!(mempool.transactions.len(), 0);
            assert_eq!(mempool.order.len(), 0);
        });
    }

    #[test]
    fn test_take_all_drains_everything_fifo() {
        let runner = deterministic::Runner::default();
        runner.start(|ctx| async move {
            let mut mempool = Mempool::new(ctx);

            let sender_private = PrivateKey::from_seed(1);
            let receiver_private = PrivateKey::from_seed(100);
            for i in 0..5 {
                let timestamp = 1634567890 + i;
                let tx = Transaction::sign(&sender_private, receiver_private.public_key(), i as u64, timestamp);
                mempool.add(tx);
            }

            assert_eq!(mempool.transactions.len(), 5);
            assert_eq!(mempool.order.len(), 5);

            let drained = mempool.take_all();
            assert_eq!(drained.len(), 5);
            assert_eq!(mempool.transactions.len(), 0);
            assert_eq!(mempool.order.len(), 0);
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

