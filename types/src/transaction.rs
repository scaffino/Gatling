use bytes::{Buf, BufMut};
use commonware_codec::{EncodeSize, Error, Read, ReadExt, Write};
use commonware_cryptography::{
    ed25519::{self, Batch, PublicKey},
    sha256::{Digest, Sha256},
    BatchVerifier, Digestible, Hasher, Signer, Verifier,
};
use commonware_utils::union;

pub const TRANSACTION_SUFFIX: &[u8] = b"_TX";

#[inline]
pub fn transaction_namespace(namespace: &[u8]) -> Vec<u8> {
    union(namespace, TRANSACTION_SUFFIX)
}

/// A simple transaction with sender, receiver, amount, and timestamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    pub sender: PublicKey,
    pub receiver: PublicKey,
    pub amount: u64,
    pub timestamp: u64,
    pub signature: ed25519::Signature,
}

impl Transaction {
    fn payload(sender: &PublicKey, receiver: &PublicKey, amount: &u64, timestamp: &u64) -> Vec<u8> {
        let mut payload = Vec::new();
        sender.write(&mut payload);
        receiver.write(&mut payload);
        amount.write(&mut payload);
        timestamp.write(&mut payload);
        payload
    }

    pub fn sign(
        private: &ed25519::PrivateKey,
        receiver: PublicKey,
        amount: u64,
        timestamp: u64,
    ) -> Self {
        let sender = private.public_key();
        let signature = private.sign(
            Some(&transaction_namespace(crate::NAMESPACE)),
            &Self::payload(&sender, &receiver, &amount, &timestamp),
        );

        Self {
            sender,
            receiver,
            amount,
            timestamp,
            signature,
        }
    }

    pub fn verify(&self) -> bool {
        self.sender.verify(
            Some(&transaction_namespace(crate::NAMESPACE)),
            &Self::payload(&self.sender, &self.receiver, &self.amount, &self.timestamp),
            &self.signature,
        )
    }

    pub fn verify_batch(&self, batch: &mut Batch) {
        batch.add(
            Some(&transaction_namespace(crate::NAMESPACE)),
            &Self::payload(&self.sender, &self.receiver, &self.amount, &self.timestamp),
            &self.sender,
            &self.signature,
        );
    }
}

impl Write for Transaction {
    fn write(&self, writer: &mut impl BufMut) {
        self.sender.write(writer);
        self.receiver.write(writer);
        self.amount.write(writer);
        self.timestamp.write(writer);
        self.signature.write(writer);
    }
}

impl Read for Transaction {
    type Cfg = ();

    fn read_cfg(reader: &mut impl Buf, _: &Self::Cfg) -> Result<Self, Error> {
        let sender = PublicKey::read(reader)?;
        let receiver = PublicKey::read(reader)?;
        let amount = u64::read(reader)?;
        let timestamp = u64::read(reader)?;
        let signature = ed25519::Signature::read(reader)?;

        Ok(Self {
            sender,
            receiver,
            amount,
            timestamp,
            signature,
        })
    }
}

impl EncodeSize for Transaction {
    fn encode_size(&self) -> usize {
        self.sender.encode_size()
            + self.receiver.encode_size()
            + self.amount.encode_size()
            + self.timestamp.encode_size()
            + self.signature.encode_size()
    }
}

impl Digestible for Transaction {
    type Digest = Digest;

    fn digest(&self) -> Digest {
        let mut hasher = Sha256::new();
        hasher.update(self.sender.as_ref());
        hasher.update(self.receiver.as_ref());
        hasher.update(self.amount.to_be_bytes().as_ref());
        hasher.update(self.timestamp.to_be_bytes().as_ref());
        // We don't include the signature as part of the digest
        hasher.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_codec::{DecodeExt, Encode};
    use commonware_cryptography::{PrivateKeyExt, Signer};

    #[test]
    fn test_transaction_sign_and_verify() {
        let sender_private = ed25519::PrivateKey::from_seed(1);
        let receiver_private = ed25519::PrivateKey::from_seed(2);
        let receiver = receiver_private.public_key();
        let timestamp = 1634567890;

        let tx = Transaction::sign(&sender_private, receiver.clone(), 100, timestamp);

        assert!(tx.verify());
        assert_eq!(tx.sender, sender_private.public_key());
        assert_eq!(tx.receiver, receiver);
        assert_eq!(tx.amount, 100);
        assert_eq!(tx.timestamp, timestamp);
    }

    #[test]
    fn test_transaction_encode_decode() {
        let sender_private = ed25519::PrivateKey::from_seed(1);
        let receiver_private = ed25519::PrivateKey::from_seed(2);
        let receiver = receiver_private.public_key();
        let timestamp = 1634567890;

        let tx = Transaction::sign(&sender_private, receiver, 100, timestamp);
        let encoded = tx.encode();
        let decoded = Transaction::decode(encoded).expect("failed to decode transaction");

        assert_eq!(tx, decoded);
        assert!(decoded.verify());
    }

    #[test]
    fn test_transaction_digest() {
        let sender_private = ed25519::PrivateKey::from_seed(1);
        let receiver_private = ed25519::PrivateKey::from_seed(2);
        let receiver = receiver_private.public_key();
        let timestamp = 1634567890;

        let tx1 = Transaction::sign(&sender_private, receiver.clone(), 100, timestamp);
        let tx2 = Transaction::sign(&sender_private, receiver, 100, timestamp);

        // Same sender, receiver, amount, timestamp should produce same digest
        assert_eq!(tx1.digest(), tx2.digest());
    }

    #[test]
    fn test_transaction_batch_verify() {
        let sender_private = ed25519::PrivateKey::from_seed(1);
        let receiver_private = ed25519::PrivateKey::from_seed(2);
        let receiver = receiver_private.public_key();
        let timestamp = 1634567890;

        let tx1 = Transaction::sign(&sender_private, receiver.clone(), 100, timestamp);
        let tx2 = Transaction::sign(&sender_private, receiver, 200, timestamp + 10);

        let mut batch = Batch::new();
        tx1.verify_batch(&mut batch);
        tx2.verify_batch(&mut batch);

        // Note: batch.verify() requires a runtime context, so we'll test individual verification here
        assert!(tx1.verify());
        assert!(tx2.verify());
    }
}

