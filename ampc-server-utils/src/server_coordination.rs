use crate::batch_sync::batch_sync_routes;
use crate::config::ServerCoordinationConfig;
use crate::profiling::pprof_routes;
use crate::shutdown_handler::ShutdownHandler;
use crate::task_monitor::TaskMonitor;
use crate::BatchSyncSharedState;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use eyre::{ensure, Error, Result, WrapErr};
use futures::future::try_join_all;
use futures::FutureExt as _;
use itertools::Itertools as _;
use reqwest::Response;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ReadyProbeResponse {
    pub image_name: String,
    pub uuid: String,
    pub shutting_down: bool,
}

/// Build HTTP check addresses from hostnames, ports, and endpoint
pub fn get_check_addresses<S>(hostnames: &[S], ports: &[S], endpoint: &str) -> Vec<String>
where
    S: AsRef<str>,
{
    hostnames
        .iter()
        .zip(ports.iter())
        .map(|(host, port)| format!("http://{}:{}/{}", host.as_ref(), port.as_ref(), endpoint))
        .collect::<Vec<String>>()
}

/// Awaits until other MPC nodes respond to "ready" queries
/// indicating that their coordination servers are running.
/// Initializes and starts HTTP server for coordinating healthcheck, readiness,
/// and synchronization between AMPC nodes.
/// Note: returns a reference to a readiness flag, an `AtomicBool`, which can later
/// be set to indicate to other MPC nodes that this server is ready for operation.
pub async fn start_coordination_server<T>(
    config: &ServerCoordinationConfig,
    task_monitor: &mut TaskMonitor,
    shutdown_handler: &Arc<ShutdownHandler>,
    my_state: &T,
    batch_sync_shared_state: Option<Arc<Mutex<BatchSyncSharedState>>>,
) -> Arc<AtomicBool>
where
    T: Serialize + DeserializeOwned + Clone + Send + 'static,
{
    tracing::info!("⚓️ ANCHOR: Starting Healthcheck, Readiness and Sync server");

    let is_ready_flag = Arc::new(AtomicBool::new(false));

    let health_shutdown_handler = Arc::clone(shutdown_handler);
    let health_check_port = config
        .get_own_healthcheck_port()
        .expect("Failed to get healthcheck port for this server");

    let _health_check_abort = task_monitor.spawn({
        let uuid = uuid::Uuid::new_v4().to_string();
        let is_ready_flag = Arc::clone(&is_ready_flag);
        let image_name = config.image_name.to_string();
        let ready_probe_response = ReadyProbeResponse {
            image_name: image_name.clone(),
            shutting_down: false,
            uuid: uuid.clone(),
        };
        let ready_probe_response_shutdown = ReadyProbeResponse {
            image_name,
            shutting_down: true,
            uuid: uuid.clone(),
        };
        let serialized_response = serde_json::to_string(&ready_probe_response)
            .expect("Serialization to JSON to probe response failed");
        let serialized_response_shutdown = serde_json::to_string(&ready_probe_response_shutdown)
            .expect("Serialization to JSON to probe response failed");
        tracing::info!("Healthcheck probe response: {}", serialized_response);
        let my_state = my_state.clone();
        let batch_sync_shared_state = batch_sync_shared_state.clone();
        async move {
            let mut app = Router::new()
                .route(
                    "/health",
                    get(move || {
                        let shutdown_handler_clone = Arc::clone(&health_shutdown_handler);
                        let serialized_response = serialized_response.clone();
                        let serialized_response_shutdown = serialized_response_shutdown.clone();
                        async move {
                            if shutdown_handler_clone.is_shutting_down() {
                                serialized_response_shutdown
                            } else {
                                serialized_response
                            }
                        }
                    }),
                )
                .route(
                    "/ready",
                    get({
                        let is_ready_flag = Arc::clone(&is_ready_flag);
                        move || async move {
                            if is_ready_flag.load(Ordering::SeqCst) {
                                "ready".into_response()
                            } else {
                                StatusCode::SERVICE_UNAVAILABLE.into_response()
                            }
                        }
                    }),
                )
                .route(
                    "/startup-sync",
                    get(move || {
                        let my_state = my_state.clone();
                        async move { serde_json::to_string(&my_state).unwrap() }
                    }),
                );

            // Add batch sync routes if state is provided
            if let Some(batch_sync_state) = batch_sync_shared_state {
                app = app.merge(batch_sync_routes(batch_sync_state));
            }

            // Merge profiling routes
            app = app.merge(pprof_routes());

            let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", health_check_port))
                .await
                .wrap_err("AMPC coordination server listener bind error")?;
            axum::serve(listener, app)
                .await
                .wrap_err("AMPC coordination server listener launch error")?;

            Ok::<(), Error>(())
        }
    });

    tracing::info!(
        "Healthcheck and Readiness server running on port {}.",
        health_check_port
    );

    is_ready_flag
}
/// Note: The response to this query is expected initially to be `503 Service Unavailable`.
pub async fn wait_for_others_unready(config: &ServerCoordinationConfig) -> Result<()> {
    tracing::info!("⚓️ ANCHOR: Waiting for other servers to be un-ready (syncing on startup)");

    let connected_but_unready = try_get_endpoint_other_nodes(config, "ready").await?;
    let all_unready = connected_but_unready
        .iter()
        .all(|resp| resp.status() == StatusCode::SERVICE_UNAVAILABLE);

    ensure!(all_unready, "One or more nodes were not unready.");

    tracing::info!("All nodes are starting up.");

    Ok(())
}

/// Starts a heartbeat task which periodically polls the "health" endpoints of
/// all other MPC nodes to ensure that the other nodes are still running and
/// responding to network requests.
pub async fn init_heartbeat_task(
    config: &ServerCoordinationConfig,
    task_monitor: &mut TaskMonitor,
    shutdown_handler: &Arc<ShutdownHandler>,
) -> Result<()> {
    let (heartbeat_tx, heartbeat_rx) = oneshot::channel();
    let mut heartbeat_tx = Some(heartbeat_tx);

    let all_health_addresses =
        get_check_addresses(&config.node_hostnames, &config.healthcheck_ports, "health");

    let party_id = config.party_id;
    let image_name = config.image_name.to_string();
    let heartbeat_initial_retries = config.heartbeat_initial_retries;
    let heartbeat_interval_secs = config.heartbeat_interval_secs;

    let heartbeat_shutdown_handler = Arc::clone(shutdown_handler);
    let _heartbeat = task_monitor.spawn(async move {
        let next_node = &all_health_addresses[(party_id + 1) % 3];
        let prev_node = &all_health_addresses[(party_id + 2) % 3];
        let mut last_response = [String::default(), String::default()];
        let mut connected = [false, false];
        let mut retries = [0, 0];
        let mut consecutive_failures = [0, 0];
        const MAX_CONSECUTIVE_FAILURES: u32 = 3;

        loop {
            for (i, host) in [next_node, prev_node].iter().enumerate() {
                let res = reqwest::get(host.as_str()).await;
                if res.is_err() || !res.as_ref().unwrap().status().is_success() {
                    tracing::warn!(
                        "Node {} did not respond with success, response: {:?}",
                        host,
                        res
                    );
                    if last_response[i] == String::default()
                        && retries[i] < heartbeat_initial_retries
                    {
                        retries[i] += 1;
                        tracing::warn!("Node {} did not respond with success, retrying...", host);
                        continue;
                    }

                    if last_response[i] == String::default() {
                        panic!(
                            "Node {} did not respond with success during heartbeat init phase, killing server...",
                            host
                        );
                    }

                    consecutive_failures[i] += 1;
                    tracing::warn!(
                        "Node {} failed health check {} times consecutively",
                        host,
                        consecutive_failures[i]
                    );

                    if consecutive_failures[i] >= MAX_CONSECUTIVE_FAILURES {
                        tracing::error!(
                            "Node {} has failed {} consecutive health checks, starting graceful shutdown",
                            host,
                            MAX_CONSECUTIVE_FAILURES
                        );

                        if !heartbeat_shutdown_handler.is_shutting_down() {
                            heartbeat_shutdown_handler.trigger_manual_shutdown();
                            tracing::error!(
                                "Node {} has failed consecutive health checks, therefore graceful shutdown has been triggered",
                                host
                            );
                        } else {
                            tracing::info!("Node {} has already started graceful shutdown.", host);
                        }
                    }
                    continue;
                }

                consecutive_failures[i] = 0;

                let probe_response = res
                    .unwrap()
                    .json::<ReadyProbeResponse>()
                    .await
                    .expect("Deserialization of probe response failed");
                if probe_response.image_name != image_name {
                    tracing::error!(
                        "Host {} is using image {} which differs from current node image: {}",
                        host,
                        probe_response.image_name.clone(),
                        image_name
                    );
                }
                if last_response[i] == String::default() {
                    last_response[i] = probe_response.uuid;
                    connected[i] = true;

                    if connected.iter().all(|&c| c) {
                        if let Some(tx) = heartbeat_tx.take() {
                            tx.send(()).unwrap();
                        }
                    }
                } else if probe_response.uuid != last_response[i] {
                    panic!("Node {} seems to have restarted, killing server...", host);
                } else if probe_response.shutting_down {
                    tracing::info!("Node {} has started graceful shutdown", host);

                    if !heartbeat_shutdown_handler.is_shutting_down() {
                        heartbeat_shutdown_handler.trigger_manual_shutdown();
                        tracing::error!(
                            "Node {} has started graceful shutdown, therefore triggering \
                             graceful shutdown",
                            host
                        );
                    }
                } else {
                    tracing::debug!("Heartbeat: Node {} is healthy", host);
                }
            }

            tokio::time::sleep(Duration::from_secs(heartbeat_interval_secs)).await;
        }
    });

    tracing::info!("Heartbeat starting...");
    heartbeat_rx.await?;
    tracing::info!("Heartbeat on all nodes started.");

    Ok(())
}

/// Retrieves synchronization state of other MPC nodes.  This data is
/// used to ensure that all nodes are in a consistent state prior
/// to starting MPC operations.
pub async fn get_others_sync_state<State>(config: &ServerCoordinationConfig) -> Result<Vec<State>>
where
    State: DeserializeOwned + Clone,
{
    tracing::info!("⚓️ ANCHOR: Syncing latest node state");

    let connected_and_ready = try_get_endpoint_other_nodes(config, "startup-sync").await?;

    let response_texts_futs: Vec<_> = connected_and_ready
        .into_iter()
        .map(|resp| resp.json())
        .collect();
    let sync_states: Vec<State> = try_join_all(response_texts_futs).await?;

    Ok(sync_states)
}

/// Toggle `is_ready_flag` to `true` to signal to other nodes that this node
/// is ready to execute the main server loop.
pub fn set_node_ready(is_ready_flag: Arc<AtomicBool>) {
    tracing::info!("⚓️ ANCHOR: Enable readiness and check all nodes");

    is_ready_flag.store(true, Ordering::SeqCst);
}

/// Awaits until other MPC nodes respond to "ready" queries
/// indicating readiness to execute the main server loop.
pub async fn wait_for_others_ready(config: &ServerCoordinationConfig) -> Result<()> {
    tracing::info!("⚓️ ANCHOR: Waiting for other servers to be ready");

    // Check other nodes and wait until all nodes are ready.
    'outer: loop {
        'retry: {
            let connected_and_ready_res = try_get_endpoint_other_nodes(config, "ready").await;

            if connected_and_ready_res.is_err() {
                break 'retry;
            }

            let connected_and_ready = connected_and_ready_res.unwrap();

            let all_ready = connected_and_ready
                .iter()
                .all(|resp| resp.status().is_success());

            if all_ready {
                break 'outer;
            }
        }
        tracing::debug!("One or more nodes were not ready.  Retrying ..");
    }

    tracing::info!("All nodes are ready.");
    Ok(())
}

/// Retrieve outputs from a healthcheck endpoint from all other server nodes.
///
/// Upon failure, retries with wait duration `config.http_query_retry_delay_ms`
/// between attempts, until `config.startup_sync_timeout_secs` seconds have elapsed.
pub async fn try_get_endpoint_other_nodes(
    config: &ServerCoordinationConfig,
    endpoint: &str,
) -> Result<Vec<Response>> {
    const NODE_COUNT: usize = 3;
    let full_urls =
        get_check_addresses(&config.node_hostnames, &config.healthcheck_ports, endpoint);
    let node_urls = (1..NODE_COUNT)
        .map(|j| (config.party_id + j) % NODE_COUNT)
        .map(|i| (i, full_urls[i].to_owned()))
        .sorted_by(|a, b| Ord::cmp(&a.0, &b.0))
        .map(|(_i, full_url)| full_url);

    let mut handles = Vec::with_capacity(NODE_COUNT - 1);
    let mut rxs = Vec::with_capacity(NODE_COUNT - 1);

    let retry_duration = Duration::from_millis(config.http_query_retry_delay_ms);
    for node_url in node_urls {
        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            loop {
                if let Ok(resp) = reqwest::get(&node_url).await {
                    let _ = tx.send(resp);
                    return;
                }
                tokio::time::sleep(retry_duration).await;
            }
        });
        handles.push(handle);
        rxs.push(rx);
    }

    let all_handles = try_join_all(handles);
    let _all_handles_with_timeout = tokio::time::timeout(
        Duration::from_secs(config.startup_sync_timeout_secs),
        all_handles,
    )
    .await;

    let msg = "Error occurred reading response channels";
    try_join_all(rxs)
        .now_or_never()
        .ok_or_else(|| eyre::eyre!(msg))?
        .inspect_err(|err| {
            tracing::error!("{}: {}", msg, err);
        })
        .wrap_err(msg)
}
