//! Batch synchronization endpoints

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Shared state for batch synchronization
#[derive(Debug, Clone, Default)]
pub struct BatchSyncSharedState {
    pub batch_id: u64,
    pub messages_to_poll: u32,
}

#[derive(Debug, Deserialize)]
struct BatchSyncQuery {
    batch_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BatchSyncState {
    pub messages_to_poll: u32,
    pub batch_id: u64,
}

/// Returns router with batch synchronization endpoints.
///
/// Provides:
/// - `/batch-sync-state?batch_id=<id>` - Get batch sync state for a specific batch ID
/// - `/batch-sync-entries` - Get batch sync entries (not implemented)
pub fn batch_sync_routes(batch_sync_shared_state: Arc<Mutex<BatchSyncSharedState>>) -> Router {
    Router::new()
        .route(
            "/batch-sync-state",
            get(move |Query(params): Query<BatchSyncQuery>| {
                let batch_sync_shared_state = batch_sync_shared_state.clone();
                async move {
                    let shared_state = batch_sync_shared_state.lock().await;

                    if params.batch_id != shared_state.batch_id {
                        return (
                            StatusCode::CONFLICT,
                            format!(
                                "Batch ID mismatch: requested {}, current {}",
                                params.batch_id, shared_state.batch_id
                            ),
                        )
                            .into_response();
                    }

                    let batch_sync_state = BatchSyncState {
                        messages_to_poll: shared_state.messages_to_poll,
                        batch_id: shared_state.batch_id,
                    };

                    match serde_json::to_string(&batch_sync_state) {
                        Ok(body) => (StatusCode::OK, body).into_response(),
                        Err(e) => {
                            tracing::error!("Failed to serialize batch sync state: {:?}", e);
                            (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                format!("Serialization error: {}", e),
                            )
                                .into_response()
                        }
                    }
                }
            }),
        )
        .route(
            "/batch-sync-entries",
            get(move || async move {
                (
                    StatusCode::NOT_IMPLEMENTED,
                    "Batch sync entries not implemented in this version",
                )
                    .into_response()
            }),
        )
}
