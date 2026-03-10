//! Runtime configuration endpoints for dynamic parameter tuning.
//!
//! Provides HTTP endpoints to get/set runtime-configurable parameters
//! (e.g., fixed batch size) without restarting the server. All parties
//! must be configured identically for correct MPC operation.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::{LazyLock, Mutex};

/// When set to `Some(n)`, overrides the dynamic batch size calculation.
/// All parties must have the same value for correct MPC operation.
pub static FIXED_BATCH_SIZE: LazyLock<Mutex<Option<usize>>> = LazyLock::new(|| Mutex::new(None));

#[derive(Debug, Deserialize)]
pub struct ConfigUpdate {
    /// If `Some(n)`, sets a fixed batch size of `n`. If `None`, clears the
    /// override and reverts to dynamic batch sizing.
    pub fixed_batch_size: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    pub fixed_batch_size: Option<usize>,
}

/// Returns a router with runtime configuration endpoints.
///
/// Provides:
/// - `GET /config` — returns the current runtime config
/// - `POST /config` — updates the runtime config
pub fn runtime_config_routes() -> Router {
    Router::new().route("/config", get(get_config).post(post_config))
}

async fn get_config() -> impl IntoResponse {
    let fixed = *FIXED_BATCH_SIZE.lock().expect("FIXED_BATCH_SIZE poisoned");
    Json(ConfigResponse {
        fixed_batch_size: fixed,
    })
}

async fn post_config(Json(update): Json<ConfigUpdate>) -> impl IntoResponse {
    let mut fixed = FIXED_BATCH_SIZE.lock().expect("FIXED_BATCH_SIZE poisoned");
    *fixed = update.fixed_batch_size;

    match update.fixed_batch_size {
        Some(size) => {
            tracing::info!("Runtime config: fixed_batch_size set to {}", size);
        }
        None => {
            tracing::info!("Runtime config: fixed_batch_size cleared (dynamic sizing)");
        }
    }

    (
        StatusCode::OK,
        Json(ConfigResponse {
            fixed_batch_size: update.fixed_batch_size,
        }),
    )
}
