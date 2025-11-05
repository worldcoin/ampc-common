//! Batch synchronization endpoints and client functions

use crate::config::ServerCoordinationConfig;
use crate::server_coordination::get_check_addresses;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use eyre::{eyre, Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};

/// Shared state for batch synchronization
#[derive(Debug, Clone, Default)]
pub struct BatchSyncSharedState {
    pub batch_id: u64,
    pub messages_to_poll: u32,
}

/// Batch synchronization state
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchSyncState {
    pub messages_to_poll: u32,
    pub batch_id: u64,
}

/// Batch synchronization entries (includes batch hash and valid entries)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchSyncEntries {
    pub valid_entries: Vec<bool>,
    pub batch_sha: [u8; 32],
}

/// Result of batch sync entries synchronization
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchSyncEntriesResult {
    pub my_state: BatchSyncEntries,
    pub all_states: Vec<BatchSyncEntries>,
}

impl BatchSyncEntriesResult {
    pub fn new(my_state: BatchSyncEntries, all_states: Vec<BatchSyncEntries>) -> Self {
        Self {
            my_state,
            all_states,
        }
    }

    pub fn sha_matches(&self) -> bool {
        self.all_states
            .iter()
            .all(|s| s.batch_sha == self.my_state.batch_sha)
    }

    pub fn valid_entries(&self) -> Vec<bool> {
        self.all_states
            .iter()
            .map(|s| s.valid_entries.clone())
            .fold(
                vec![true; self.my_state.valid_entries.len()],
                |mut acc, entries| {
                    for (i, &entry) in entries.iter().enumerate() {
                        if !entry {
                            acc[i] = false;
                        }
                    }
                    acc
                },
            )
    }

    pub fn own_sha_pretty(&self) -> String {
        hex::encode(&self.my_state.batch_sha[0..4])
    }

    pub fn all_shas_pretty(&self) -> String {
        self.all_states
            .iter()
            .map(|s| hex::encode(&s.batch_sha[0..4]))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Result of batch sync state synchronization
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchSyncResult {
    pub my_state: BatchSyncState,
    pub all_states: Vec<BatchSyncState>,
}

impl BatchSyncResult {
    pub fn new(my_state: BatchSyncState, all_states: Vec<BatchSyncState>) -> Self {
        Self {
            my_state,
            all_states,
        }
    }

    pub fn messages_to_poll(&self) -> u32 {
        self.all_states
            .iter()
            .map(|s| s.messages_to_poll)
            .min()
            .unwrap_or(0)
    }
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

#[derive(Debug, Deserialize)]
struct BatchSyncQuery {
    batch_id: u64,
}

/// Get batch sync states from all other nodes.
///
/// This function polls other nodes' `/batch-sync-state` endpoints until they all
/// have the same batch_id as the reference batch_id (from own_state).
///
/// # Arguments
/// * `config` - Server coordination configuration
/// * `own_state` - Optional own batch sync state. If None, the function will fail.
/// * `polling_timeout_secs` - Timeout in seconds for polling each node
///
/// # Returns
/// A vector of BatchSyncState from all nodes (including own state)
pub async fn get_batch_sync_states(
    config: &ServerCoordinationConfig,
    own_state: Option<&BatchSyncState>,
    polling_timeout_secs: u64,
) -> Result<Vec<BatchSyncState>> {
    let own_sync_state = own_state
        .ok_or_else(|| eyre!("own_state must be provided"))?
        .clone();

    let reference_batch_id = own_sync_state.batch_id;

    let all_batch_size_sync_addresses = get_check_addresses(
        &config.node_hostnames,
        &config.healthcheck_ports,
        "batch-sync-state",
    );

    let next_node = &all_batch_size_sync_addresses[(config.party_id + 1) % 3];
    let prev_node = &all_batch_size_sync_addresses[(config.party_id + 2) % 3];

    let mut states = Vec::with_capacity(3);
    states.push(own_sync_state.clone());

    let polling_timeout_duration = Duration::from_secs(polling_timeout_secs);

    for host in [next_node, prev_node].iter() {
        let mut fetched_state: Option<BatchSyncState> = None;

        match timeout(polling_timeout_duration, async {
            loop {
                // Add batch_id as query parameter
                let url = format!("{}?batch_id={}", host.as_str(), reference_batch_id);
                let res = reqwest::get(&url).await.with_context(|| {
                    format!("Failed to fetch batch sync state from party {}", host)
                })?;

                tracing::info!("Response Status: {}", res.status());

                // Check if we got a 409 Conflict response (batch_id mismatch)
                if res.status() == reqwest::StatusCode::CONFLICT {
                    let error_body = res
                        .text()
                        .await
                        .unwrap_or_else(|_| "Unknown error".to_string());
                    tracing::info!(
                        "Party {} returned batch ID mismatch: {}. Retrying in 1 second...",
                        host,
                        error_body
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }

                // Handle other non-OK status codes
                if !res.status().is_success() {
                    let status = res.status();
                    let error_body = res
                        .text()
                        .await
                        .unwrap_or_else(|_| "Unknown error".to_string());
                    return Err(eyre!(
                        "Party {} returned error status {}: {}",
                        host,
                        status,
                        error_body
                    ));
                }

                let state: BatchSyncState = res.json().await.with_context(|| {
                    format!("Failed to parse batch sync state from party {}", host)
                })?;

                if state.batch_id < reference_batch_id {
                    tracing::info!(
                        "Party {} (batch_id {}) is behind own batch_id {}. Retrying in 1 second...",
                        host,
                        state.batch_id,
                        reference_batch_id
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                } else {
                    fetched_state = Some(state);
                    break;
                }
            }
            Ok::<(), eyre::Error>(())
        })
        .await
        {
            Ok(Ok(_)) => {
                if let Some(state) = fetched_state {
                    match state.batch_id.cmp(&reference_batch_id) {
                        std::cmp::Ordering::Greater => {
                            tracing::warn!(
                                "Received batch sync state from party {} for a future batch_id {} (own is {}). This might indicate this node is behind.",
                                host, state.batch_id, reference_batch_id
                            );
                        }
                        std::cmp::Ordering::Less => {
                            tracing::error!(
                                "Party {} (batch_id {}) is still behind own batch_id {} after polling loop. This is unexpected.",
                                host, state.batch_id, reference_batch_id
                            );
                        }
                        std::cmp::Ordering::Equal => {}
                    }
                    states.push(state);
                } else {
                    tracing::error!("Fetched_state is None after successful polling loop from party {}. This is a bug.", host);
                    return Err(eyre!("Internal logic error fetching state from {}", host));
                }
            }
            Ok(Err(e)) => {
                tracing::error!(
                    "Error polling party {}: {:?}. Using potentially stale or default state.",
                    host,
                    e
                );
                return Err(eyre!(
                    "Failed to get a consistent batch_id from party {} due to: {:?}",
                    host,
                    e
                ));
            }
            Err(_) => {
                tracing::error!("Timeout polling party {} for batch_id {}. Using potentially stale or default state.", host, reference_batch_id);
                return Err(eyre!(
                    "Timeout waiting for party {} to reach batch_id {}",
                    host,
                    reference_batch_id
                ));
            }
        }
    }
    Ok(states)
}

/// Get batch sync entries from all other nodes.
///
/// This function polls other nodes' `/batch-sync-entries` endpoints until they all
/// have the same batch_sha as the own_state.
///
/// # Arguments
/// * `config` - Server coordination configuration
/// * `own_state` - Optional own batch sync entries. If None, the function will fail.
///
/// # Returns
/// A vector of BatchSyncEntries from all nodes (including own state)
pub async fn get_batch_sync_entries(
    config: &ServerCoordinationConfig,
    own_state: Option<BatchSyncEntries>,
) -> Result<Vec<BatchSyncEntries>> {
    let own_sync_state = own_state.ok_or_else(|| eyre!("own_state must be provided"))?;

    let all_batch_size_sync_entries_addresses = get_check_addresses(
        &config.node_hostnames,
        &config.healthcheck_ports,
        "batch-sync-entries",
    );

    let next_node = &all_batch_size_sync_entries_addresses[(config.party_id + 1) % 3];
    let prev_node = &all_batch_size_sync_entries_addresses[(config.party_id + 2) % 3];

    let mut states = Vec::with_capacity(3);
    states.push(own_sync_state.clone());

    let polling_timeout_duration = Duration::from_secs(20);

    for host in [next_node, prev_node].iter() {
        let mut fetched_state: Option<BatchSyncEntries> = None;

        match timeout(polling_timeout_duration, async {
            loop {
                let res = reqwest::get(host.as_str()).await.with_context(|| {
                    format!("Failed to fetch batch sync entries from party {}", host)
                })?;
                let state: BatchSyncEntries = res.json().await.with_context(|| {
                    format!("Failed to parse batch sync entries from party {}", host)
                })?;

                if !state.batch_sha.eq(&own_sync_state.batch_sha) {
                    tracing::info!(
                        "Party {} (batch_hash {}) differs from own ({}). Retrying in 1 second...",
                        host,
                        hex::encode(&state.batch_sha[0..4]),
                        hex::encode(&own_sync_state.batch_sha[0..4])
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                } else {
                    fetched_state = Some(state);
                    break;
                }
            }
            Ok::<(), eyre::Error>(())
        })
        .await
        {
            Ok(Ok(_)) => {
                if let Some(state) = fetched_state {
                    states.push(state);
                } else {
                    tracing::error!("Fetched_state is None after successful polling loop from party {}. This is a bug.", host);
                    return Err(eyre!("Internal logic error fetching state from {}", host));
                }
            }
            Ok(Err(e)) => {
                tracing::error!(
                    "Error polling party {}: {:?}. Using potentially stale or default sync entries.",
                    host,
                    e
                );
                return Err(eyre!(
                    "Failed to get a consistent batch_hash from party {} due to: {:?}",
                    host,
                    e
                ));
            }
            Err(_) => {
                tracing::error!(
                    "Timeout polling party {} for batch sync entries with hash {}",
                    host,
                    hex::encode(&own_sync_state.batch_sha[0..4])
                );
                return Err(eyre!(
                    "Timeout waiting for party {} to reach batch hash {}",
                    host,
                    hex::encode(&own_sync_state.batch_sha[0..4])
                ));
            }
        }
    }
    Ok(states)
}
