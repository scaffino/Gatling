use alto_types::Transaction;
use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Router,
};
use commonware_codec::DecodeExt;
use commonware_cryptography::Digestible;
use std::net::SocketAddr;
use std::sync::Arc;
use futures::channel::mpsc;
use tokio::sync::Mutex;
use tracing::{error, info};

/// Shared state for the HTTP server
#[derive(Clone)]
pub struct ServerState {
    mailbox: Arc<Mutex<crate::application::Mailbox>>,
    tx_broadcast_channel: Option<mpsc::UnboundedSender<Transaction>>,
}

/// Handle POST /transaction endpoint
async fn submit_transaction(
    State(state): State<ServerState>,
    body: Bytes,
) -> impl IntoResponse {
    // Decode transaction
    let tx = match Transaction::decode(body.as_ref()) {
        Ok(tx) => tx,
        Err(e) => {
            error!(?e, "Failed to decode transaction");
            return (StatusCode::BAD_REQUEST, "Invalid transaction encoding");
        }
    };

    // Get transaction identifier before moving tx
    let tx_id = tx.digest();
    let tx_timestamp = tx.timestamp;

    // Verify signature
    if !tx.verify() {
        error!(tx_id = ?tx_id, "Invalid transaction signature");
        return (StatusCode::BAD_REQUEST, "Invalid signature");
    }

    // Submit to local mempool
    let mut mailbox = state.mailbox.lock().await;
    match mailbox.submit_transaction(tx.clone()).await {
        Ok(_) => {
            info!(tx_id = ?tx_id, timestamp = tx_timestamp, "Transaction submitted to mempool via HTTP");
            
            // Send to broadcast task if available
            if let Some(broadcast_tx) = &state.tx_broadcast_channel {
                let _ = broadcast_tx.unbounded_send(tx);
            }
            
            (StatusCode::OK, "Transaction accepted")
        }
        Err(e) => {
            error!(tx_id = ?tx_id, error = %e, "Failed to submit transaction");
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to submit transaction")
        }
    }
}

/// Start the HTTP server for transaction submission (single consensus)
pub async fn start_server(
    addr: SocketAddr,
    mailbox: crate::application::Mailbox,
    tx_broadcast_channel: Option<mpsc::UnboundedSender<Transaction>>,
) -> Result<(), std::io::Error> {
    let state = ServerState {
        mailbox: Arc::new(Mutex::new(mailbox)),
        tx_broadcast_channel,
    };

    let app = Router::new()
        .route("/transaction", post(submit_transaction))
        .with_state(state);

    info!(?addr, "Starting transaction HTTP server");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await
}

/// Shared state for multi-consensus HTTP server
#[derive(Clone)]
pub struct MultiServerState {
    mailboxes: Arc<Mutex<Vec<crate::application::Mailbox>>>,
    tx_broadcast_channel: Option<mpsc::UnboundedSender<Transaction>>,
}

/// Handle POST /transaction endpoint for multiple consensus instances
async fn submit_transaction_multi(
    State(state): State<MultiServerState>,
    body: Bytes,
) -> impl IntoResponse {
    // Decode transaction
    let tx = match Transaction::decode(body.as_ref()) {
        Ok(tx) => tx,
        Err(e) => {
            error!(?e, "Failed to decode transaction");
            return (StatusCode::BAD_REQUEST, "Invalid transaction encoding");
        }
    };

    // Get transaction identifier before moving tx
    let tx_id = tx.digest();
    let tx_timestamp = tx.timestamp;

    // Verify signature
    if !tx.verify() {
        error!(tx_id = ?tx_id, "Invalid transaction signature");
        return (StatusCode::BAD_REQUEST, "Invalid signature");
    }

    // Submit to ALL consensus instances' mempools
    let mut mailboxes = state.mailboxes.lock().await;
    let mut submitted_count = 0;
    for (idx, mailbox) in mailboxes.iter_mut().enumerate() {
        match mailbox.submit_transaction(tx.clone()).await {
            Ok(_) => {
                submitted_count += 1;
                info!("[consensus_{}] Transaction {:?} (timestamp: {} ms) submitted to mempool via HTTP", 
                      idx + 1, tx_id, tx_timestamp);
            }
            Err(e) => {
                error!(
                    tx_id = ?tx_id,
                    consensus_id = idx + 1,
                    error = %e,
                    "Failed to submit transaction"
                );
            }
        }
    }
    
    if submitted_count == 0 {
        return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to submit to any consensus");
    }

    // Send to broadcast task if available
    if let Some(broadcast_tx) = &state.tx_broadcast_channel {
        let _ = broadcast_tx.unbounded_send(tx);
    }
    
    (StatusCode::OK, "Transaction accepted")
}

/// Start the HTTP server for transaction submission (multiple consensus instances)
pub async fn start_server_multi(
    addr: SocketAddr,
    mailboxes: Vec<crate::application::Mailbox>,
    tx_broadcast_channel: Option<mpsc::UnboundedSender<Transaction>>,
) -> Result<(), std::io::Error> {
    let instances_count = mailboxes.len();
    let state = MultiServerState {
        mailboxes: Arc::new(Mutex::new(mailboxes)),
        tx_broadcast_channel,
    };

    let app = Router::new()
        .route("/transaction", post(submit_transaction_multi))
        .with_state(state);

    info!(?addr, instances = instances_count, "Starting transaction HTTP server for multiple consensus instances");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await
}

