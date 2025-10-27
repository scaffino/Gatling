use alto_types::{Activity, Block, Transaction};
use commonware_consensus::{
    marshal,
    threshold_simplex::types::{Context, View},
    Automaton, Relay, Reporter, Viewable,
};
use commonware_cryptography::{
    bls12381::primitives::variant::MinSig,
    sha256::Digest,
};
use commonware_runtime::{Metrics, Spawner};
use futures::{
    channel::{mpsc, oneshot},
    SinkExt,
};

/// Messages sent to the application.
pub enum Message {
    Genesis {
        response: oneshot::Sender<Digest>,
    },
    Propose {
        view: View,
        parent: (View, Digest),
        response: oneshot::Sender<Digest>,
    },
    Broadcast {
        payload: Digest,
    },
    Verify {
        view: View,
        parent: (View, Digest),
        payload: Digest,
        response: oneshot::Sender<bool>,
    },
    Finalized {
        view: View,
        block: Block,
    },
    SubmitTransaction {
        transaction: Transaction,
    },
}

/// Mailbox for the application.
#[derive(Clone)]
pub struct Mailbox {
    sender: mpsc::Sender<Message>,
}

impl Mailbox {
    pub(super) fn new(sender: mpsc::Sender<Message>) -> Self {
        Self { sender }
    }
    
    pub fn sender_clone(&self) -> mpsc::Sender<Message> {
        self.sender.clone()
    }

    /// Submit a transaction to the mempool.
    pub async fn submit_transaction(&mut self, transaction: Transaction) -> Result<(), String> {
        self.sender
            .send(Message::SubmitTransaction { transaction })
            .await
            .map_err(|_| "Failed to send transaction".to_string())
    }
}

/// Pusher that connects Reporter to the application actor
#[derive(Clone)]
pub struct FinalizationPusher<E: Spawner + Metrics> {
    context: E,
    sender: mpsc::Sender<Message>,
    marshal: marshal::Mailbox<MinSig, Block>,
}

impl<E: Spawner + Metrics> FinalizationPusher<E> {
    pub fn new(context: E, sender: mpsc::Sender<Message>, marshal: marshal::Mailbox<MinSig, Block>) -> Self {
        Self { context, sender, marshal }
    }
}

impl<E: Spawner + Metrics> Reporter for FinalizationPusher<E> {
    type Activity = Activity;

    async fn report(&mut self, activity: Self::Activity) {
        match activity {
            Activity::Finalization(finalization) => {
                let view = finalization.view();
                let payload = finalization.proposal.payload;
                
                // Spawn a task to subscribe to the block
                let mut sender = self.sender.clone();
                let mut marshal = self.marshal.clone();
                let context = self.context.with_label("finalization_fetch");
                context.spawn(move |_| async move {
                    // Subscribe to get the block
                    let block_result = marshal
                        .subscribe(Some(view), payload)
                        .await
                        .await;
                    
                    if let Ok(block) = block_result {
                        let _ = sender.send(Message::Finalized { view, block }).await;
                    }
                });
            }
            _ => {
                // Ignore notarizations and other activities
            }
        }
    }
}

impl Automaton for Mailbox {
    type Digest = Digest;
    type Context = Context<Self::Digest>;

    async fn genesis(&mut self) -> Self::Digest {
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(Message::Genesis { response })
            .await
            .expect("Failed to send genesis");
        receiver.await.expect("Failed to receive genesis")
    }

    async fn propose(&mut self, context: Context<Self::Digest>) -> oneshot::Receiver<Self::Digest> {
        // If we linked payloads to their parent, we would include
        // the parent in the `Context` in the payload.
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(Message::Propose {
                view: context.view,
                parent: context.parent,
                response,
            })
            .await
            .expect("Failed to send propose");
        receiver
    }

    async fn verify(
        &mut self,
        context: Context<Self::Digest>,
        payload: Self::Digest,
    ) -> oneshot::Receiver<bool> {
        // If we linked payloads to their parent, we would verify
        // the parent included in the payload matches the provided `Context`.
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(Message::Verify {
                view: context.view,
                parent: context.parent,
                payload,
                response,
            })
            .await
            .expect("Failed to send verify");
        receiver
    }
}

impl Relay for Mailbox {
    type Digest = Digest;

    async fn broadcast(&mut self, digest: Self::Digest) {
        self.sender
            .send(Message::Broadcast { payload: digest })
            .await
            .expect("Failed to send broadcast");
    }
}

impl Reporter for Mailbox {
    type Activity = Block;

    async fn report(&mut self, block: Self::Activity) {
        // Marshal reports Block, so we need to look up the view
        // This will be handled by the Finalized message handler in the actor
        self.sender
            .send(Message::Finalized { view: 0, block })
            .await
            .expect("Failed to send finalized");
    }
}
