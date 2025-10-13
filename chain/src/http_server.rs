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
use tokio::sync::Mutex;
use tracing::{error, info};

/// Shared state for the HTTP server
#[derive(Clone)]
pub struct ServerState {
    mailbox: Arc<Mutex<crate::application::Mailbox>>,
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

    // Verify signature
    if !tx.verify() {
        error!(tx_id = ?tx_id, "Invalid transaction signature");
        return (StatusCode::BAD_REQUEST, "Invalid signature");
    }

    // Submit to validator
    let mut mailbox = state.mailbox.lock().await;
    match mailbox.submit_transaction(tx).await {
        Ok(_) => {
            info!(tx_id = ?tx_id, "Transaction submitted to mempool via HTTP");
            (StatusCode::OK, "Transaction accepted")
        }
        Err(e) => {
            error!(tx_id = ?tx_id, error = %e, "Failed to submit transaction");
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to submit transaction")
        }
    }
}

/// Start the HTTP server for transaction submission
pub async fn start_server(
    addr: SocketAddr,
    mailbox: crate::application::Mailbox,
) -> Result<(), std::io::Error> {
    let state = ServerState {
        mailbox: Arc::new(Mutex::new(mailbox)),
    };

    let app = Router::new()
        .route("/transaction", post(submit_transaction))
        .with_state(state);

    info!(?addr, "Starting transaction HTTP server");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await
}

