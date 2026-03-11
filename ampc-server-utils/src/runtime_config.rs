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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::LazyLock;

/// When non-zero, overrides the dynamic batch size calculation.
/// This value is mapped to an Option. Zero corresponds to None.
/// All parties must have the same value for correct MPC operation.
static FIXED_BATCH_SIZE: LazyLock<AtomicUsize> = LazyLock::new(|| AtomicUsize::new(0));

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

pub fn get_fixed_batch_size() -> Option<usize> {
    match FIXED_BATCH_SIZE.load(Ordering::Relaxed) {
        0 => None,
        x => Some(x),
    }
}

async fn get_config() -> impl IntoResponse {
    Json(ConfigResponse {
        fixed_batch_size: get_fixed_batch_size(),
    })
}

async fn post_config(Json(update): Json<ConfigUpdate>) -> impl IntoResponse {
    FIXED_BATCH_SIZE.store(
        update.fixed_batch_size.unwrap_or_default(),
        Ordering::Relaxed,
    );
    match update.fixed_batch_size {
        Some(size) if size != 0 => {
            tracing::info!("Runtime config: fixed_batch_size set to {}", size);
        }
        _ => {
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
