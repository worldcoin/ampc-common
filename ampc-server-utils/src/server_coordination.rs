use crate::batch_sync::batch_sync_routes;
use crate::config::ServerCoordinationConfig;
use crate::profiling::pprof_routes;
use crate::shutdown_handler::ShutdownHandler;
use crate::task_monitor::TaskMonitor;
use crate::BatchSyncSharedState;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response as AxumResponse};
use axum::routing::get;
use axum::Router;
use eyre::{bail, ensure, Error, Result, WrapErr};
use futures::future::try_join_all;
use itertools::Itertools as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::error::Error as _;
use std::future::IntoFuture;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Mutex};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ReadyProbeResponse {
    pub image_name: String,
    pub uuid: String,
    pub shutting_down: bool,
    pub verified_peers: HashSet<String>,
    pub is_ready: bool,
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

/// Axum middleware that emits one line per request with method, path,
/// response status, and elapsed time.
async fn log_request(req: Request, next: Next) -> AxumResponse {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let started = Instant::now();
    let response = next.run(req).await;
    let status = response.status().as_u16();
    let elapsed = started.elapsed().as_millis();
    // High-frequency liveness/readiness probes are noise at info level.
    if path == "/health" || path == "/ready" {
        tracing::debug!("{} {} -> {} ({} ms)", method, path, status, elapsed);
    } else {
        tracing::info!("{} {} -> {} ({} ms)", method, path, status, elapsed);
    }
    response
}

/// Tag a reqwest error with a short kind for log grepping.
/// Examples: "connect-refused", "connect-timeout", "connect-reset",
/// "addr-not-available", "connect-other", "timeout", "body", "decode",
/// "status", "request", "builder", "other".
fn classify_reqwest_error(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        return "timeout";
    }
    if err.is_connect() {
        let mut source: Option<&(dyn std::error::Error + 'static)> = err.source();
        while let Some(e) = source {
            if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                use std::io::ErrorKind::*;
                return match io_err.kind() {
                    ConnectionRefused => "connect-refused",
                    TimedOut => "connect-timeout",
                    ConnectionReset => "connect-reset",
                    ConnectionAborted => "connect-aborted",
                    AddrNotAvailable => "addr-not-available",
                    _ => "connect-other",
                };
            }
            source = e.source();
        }
        return "connect";
    }
    if err.is_body() {
        return "body";
    }
    if err.is_decode() {
        return "decode";
    }
    if err.is_status() {
        return "status";
    }
    if err.is_request() {
        return "request";
    }
    if err.is_builder() {
        return "builder";
    }
    "other"
}

/// Awaits until other MPC nodes respond to "ready" queries
/// indicating that their coordination servers are running.
/// Initializes and starts HTTP server for coordinating healthcheck, readiness,
/// and synchronization between AMPC nodes.
/// Note: returns a reference to a readiness flag, an `AtomicBool`, which can later
/// be set to indicate to other MPC nodes that this server is ready for operation.
/// Also returns a handle to the set of verified peers, and the UUID of this node.
pub async fn start_coordination_server<T>(
    config: &ServerCoordinationConfig,
    task_monitor: &mut TaskMonitor,
    shutdown_handler: &Arc<ShutdownHandler>,
    my_state: &T,
    batch_sync_shared_state: Option<Arc<Mutex<BatchSyncSharedState>>>,
) -> (Arc<AtomicBool>, Arc<Mutex<HashSet<String>>>, String)
where
    T: Serialize + DeserializeOwned + Clone + Send + 'static,
{
    start_coordination_server_with_extra_routes(
        config,
        task_monitor,
        shutdown_handler,
        my_state,
        batch_sync_shared_state,
        None,
    )
    .await
}

/// Like [`start_coordination_server`] but accepts optional extra axum routes
/// that are merged into the coordination HTTP server. Use this to add
/// endpoints that need live DB access (e.g., `/rerand-watermark`).
pub async fn start_coordination_server_with_extra_routes<T>(
    config: &ServerCoordinationConfig,
    task_monitor: &mut TaskMonitor,
    shutdown_handler: &Arc<ShutdownHandler>,
    my_state: &T,
    batch_sync_shared_state: Option<Arc<Mutex<BatchSyncSharedState>>>,
    extra_routes: Option<Router>,
) -> (Arc<AtomicBool>, Arc<Mutex<HashSet<String>>>, String)
where
    T: Serialize + DeserializeOwned + Clone + Send + 'static,
{
    tracing::info!("⚓️ ANCHOR: Starting Healthcheck, Readiness and Sync server");

    let is_ready_flag = Arc::new(AtomicBool::new(false));
    let verified_peers = Arc::new(Mutex::new(HashSet::new()));

    let health_shutdown_handler = Arc::clone(shutdown_handler);
    let health_check_port = config
        .get_own_healthcheck_port()
        .expect("Failed to get healthcheck port for this server");

    let uuid = uuid::Uuid::new_v4().to_string();
    let _health_check_abort = task_monitor.spawn({
        let uuid = uuid.clone();
        let is_ready_flag = Arc::clone(&is_ready_flag);
        let verified_peers = Arc::clone(&verified_peers);
        let image_name = config.image_name.to_string();

        // Pre-calculate parts of the response that don't change
        let base_response = ReadyProbeResponse {
            image_name: image_name.clone(),
            shutting_down: false,
            uuid: uuid.clone(),
            verified_peers: HashSet::new(),
            is_ready: false,
        };

        let my_state = my_state.clone();
        let batch_sync_shared_state = batch_sync_shared_state.clone();

        async move {
            let is_ready_flag_health = Arc::clone(&is_ready_flag);
            let is_ready_flag_ready = Arc::clone(&is_ready_flag);

            let mut app = Router::new()
                .route(
                    "/health",
                    get(move || {
                        let shutdown_handler_clone = Arc::clone(&health_shutdown_handler);
                        let verified_peers_clone = Arc::clone(&verified_peers);
                        let is_ready_flag_clone = Arc::clone(&is_ready_flag_health);
                        let mut response = base_response.clone();
                        async move {
                            response.shutting_down = shutdown_handler_clone.is_shutting_down();
                            response.verified_peers = verified_peers_clone.lock().await.clone();
                            response.is_ready = is_ready_flag_clone.load(Ordering::SeqCst);

                            serde_json::to_string(&response)
                                .expect("Serialization to JSON to probe response failed")
                        }
                    }),
                )
                .route(
                    "/ready",
                    get(move || async move {
                        if is_ready_flag_ready.load(Ordering::SeqCst) {
                            "ready".into_response()
                        } else {
                            StatusCode::SERVICE_UNAVAILABLE.into_response()
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

            // Merge caller-provided extra routes (e.g., /rerand-watermark)
            if let Some(extra) = extra_routes {
                app = app.merge(extra);
            }

            // Per-request access logging wraps the full route set (extras included).
            app = app.layer(middleware::from_fn(log_request));

            let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", health_check_port))
                .await
                .wrap_err("AMPC coordination server listener bind error")?;

            tracing::info!(
                "Coordination server listener bound on port {}",
                health_check_port
            );

            let mut heartbeat = tokio::time::interval(Duration::from_secs(60));
            heartbeat.tick().await; // consume the immediate first tick

            let serve_fut = axum::serve(listener, app).into_future();
            tokio::pin!(serve_fut);

            tracing::info!(
                "Coordination server listener accepting connections on port {}",
                health_check_port
            );

            loop {
                tokio::select! {
                    result = &mut serve_fut => {
                        result.wrap_err("AMPC coordination server listener launch error")?;
                        break;
                    }
                    _ = heartbeat.tick() => {
                        tracing::info!(
                            "Coordination server heartbeat: listening on port {}",
                            health_check_port
                        );
                    }
                }
            }

            Ok::<(), Error>(())
        }
    });

    (is_ready_flag, verified_peers, uuid)
}

/// Note: The response to this query is expected initially to be `503 Service Unavailable`.
/// However, due to race conditions where a peer might restart and become ready (200 OK)
/// before we check it, we also accept 200 OK *IF* the peer can prove it saw us during its startup.
/// This is verified by checking if the peer's `verified_peers` list contains our UUID.
pub async fn wait_for_others_unready(
    config: &ServerCoordinationConfig,
    my_verified_peers: &Arc<Mutex<HashSet<String>>>,
    my_uuid: &str,
) -> Result<()> {
    tracing::info!("⚓️ ANCHOR: Waiting for other servers to be un-ready (syncing on startup)");

    // We use "/health" to check state and verified peers.
    let connected_health_resps = try_get_endpoint_other_nodes(config, "health").await?;

    let mut all_safe = true;

    for (_status, body) in connected_health_resps {
        let probe_response: ReadyProbeResponse =
            serde_json::from_slice(&body).wrap_err("Failed to deserialize ReadyProbeResponse")?;

        // 1. Add their UUID to our list (so we can prove to them we saw them later)
        my_verified_peers
            .lock()
            .await
            .insert(probe_response.uuid.clone());

        // 2. Check status
        if !probe_response.is_ready {
            // They are unready (503 equivalent). Safe.
        } else {
            // They are ready (200 equivalent).
            // Check if they verified us.
            if probe_response.verified_peers.contains(my_uuid) {
                // They verified us. Safe (Race condition handled).
                tracing::info!(
                    "Peer {} is already ready but verified us. Accepting as race condition.",
                    probe_response.uuid
                );
            } else {
                // They are ready but didn't verify us. BAD.
                tracing::error!(
                    "Node {} is ready but did not verify our UUID {}",
                    probe_response.uuid,
                    my_uuid
                );
                all_safe = false;
            }
        }
    }

    ensure!(
        all_safe,
        "One or more nodes were not unready (and did not verify us)."
    );

    tracing::info!("All nodes are starting up (or validly raced ahead).");

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

    let sync_states: Vec<State> = connected_and_ready
        .into_iter()
        .map(|(_status, body)| {
            serde_json::from_slice(&body).wrap_err("Failed to deserialize sync state")
        })
        .collect::<Result<Vec<_>>>()?;

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
                .all(|(status, _body)| status.is_success());

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
) -> Result<Vec<(StatusCode, Vec<u8>)>> {
    use tokio::time::{sleep, timeout};

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
    let request_timeout = Duration::from_millis(config.http_query_timeout_ms);
    let client = reqwest::Client::new();
    for node_url in node_urls {
        let (tx, rx) = oneshot::channel();
        let client = client.clone();
        let handle = tokio::spawn(async move {
            let mut attempt: u32 = 0;
            loop {
                attempt += 1;
                let resp = match timeout(request_timeout, client.get(&node_url).send()).await {
                    Ok(Ok(resp)) => resp,
                    Ok(Err(err)) => {
                        tracing::warn!(
                            "Failed to reach endpoint {} [{}] (attempt {}): {}. Retrying in {:?}",
                            node_url,
                            classify_reqwest_error(&err),
                            attempt,
                            err,
                            retry_duration
                        );
                        sleep(retry_duration).await;
                        continue;
                    }
                    Err(_) => {
                        tracing::warn!(
                            "Request to {} [send-timeout] timed out after {:?} (attempt {}). Retrying in {:?}",
                            node_url, request_timeout, attempt, retry_duration
                        );
                        sleep(retry_duration).await;
                        continue;
                    }
                };

                let status = resp.status();
                match timeout(request_timeout, resp.bytes()).await {
                    Ok(Ok(body)) => {
                        let _ = tx.send((status, body.to_vec()));
                        return;
                    }
                    Ok(Err(err)) => {
                        tracing::warn!(
                            "Failed to read body from {} [{}] (attempt {}): {}. Retrying in {:?}",
                            node_url,
                            classify_reqwest_error(&err),
                            attempt,
                            err,
                            retry_duration
                        );
                    }
                    Err(_) => {
                        tracing::warn!(
                            "Body read from {} [body-timeout] timed out after {:?} (attempt {}). Retrying in {:?}",
                            node_url, request_timeout, attempt, retry_duration
                        );
                    }
                }
                sleep(retry_duration).await;
            }
        });
        handles.push(handle);
        rxs.push(rx);
    }

    let results = tokio::time::timeout(
        Duration::from_secs(config.startup_sync_timeout_secs),
        try_join_all(rxs),
    )
    .await;

    // if timeout or one of the channels recv() returned an error, due to a panic
    // (which would mean that the other tasks could still be running)
    if results.is_err() || matches!(results, Ok(Err(_))) {
        for handle in handles {
            handle.abort();
        }
    }

    let all_responses = results
        .map_err(|_| eyre::eyre!("Startup sync timed out"))?? // First ? is timeout, second is join_all
        .into_iter()
        .collect::<Vec<_>>();

    Ok(all_responses)
}

/// Waits until every node has observed every other node during startup.
///
/// `wait_for_others_unready` proves that this node saw its peers, but not that
/// all peers have also observed each other. HNSW startup does heavy loading
/// immediately after the startup checks, so hold here until cluster visibility
/// is complete.
pub async fn wait_until_startup_visibility_is_complete(
    config: &ServerCoordinationConfig,
    verified_peers: &Arc<Mutex<HashSet<String>>>,
    my_uuid: &str,
) -> Result<()> {
    tracing::info!("Waiting for full peer visibility during startup");

    let started = Instant::now();
    let startup_timeout = Duration::from_secs(config.startup_sync_timeout_secs);
    let retry_duration = Duration::from_millis(config.http_query_retry_delay_ms);
    loop {
        let connected_health_resps = match try_get_endpoint_other_nodes(config, "health").await {
            Ok(resps) => resps,
            Err(err) if started.elapsed() < startup_timeout => {
                tracing::warn!(
                    "Failed to query peer health while waiting for full startup visibility: {:?}",
                    err
                );
                tokio::time::sleep(retry_duration).await;
                continue;
            }
            Err(err) => {
                return Err(
                    err.wrap_err("timed out waiting for full peer visibility during startup")
                );
            }
        };

        let mut peer_responses = Vec::with_capacity(connected_health_resps.len());
        for (_status, body) in connected_health_resps {
            let probe_response: ReadyProbeResponse = serde_json::from_slice(&body)
                .wrap_err("Failed to deserialize ReadyProbeResponse")?;
            peer_responses.push(probe_response);
        }

        {
            let mut verified = verified_peers.lock().await;
            for probe_response in &peer_responses {
                verified.insert(probe_response.uuid.clone());
            }
        }

        let expected_uuids = current_startup_uuid_set(my_uuid, &peer_responses);

        let mut incomplete_visibility = Vec::new();
        for probe_response in peer_responses {
            let missing_uuids = missing_startup_visibility(
                &expected_uuids,
                &probe_response.uuid,
                &probe_response.verified_peers,
            );

            if !missing_uuids.is_empty() {
                incomplete_visibility.push((
                    probe_response.uuid,
                    probe_response.verified_peers,
                    missing_uuids,
                ));
            }
        }

        if incomplete_visibility.is_empty() {
            tracing::info!("All nodes have full peer visibility during startup");
            return Ok(());
        }

        if started.elapsed() >= startup_timeout {
            bail!(
                "Timed out waiting for full peer visibility during startup: {:?}",
                incomplete_visibility
            );
        }

        tracing::debug!(
            "Waiting for full peer visibility during startup: {:?}",
            incomplete_visibility
        );
        tokio::time::sleep(retry_duration).await;
    }
}

fn current_startup_uuid_set(
    my_uuid: &str,
    peer_responses: &[ReadyProbeResponse],
) -> HashSet<String> {
    let mut expected_uuids = HashSet::from([my_uuid.to_owned()]);
    expected_uuids.extend(
        peer_responses
            .iter()
            .map(|probe_response| probe_response.uuid.clone()),
    );
    expected_uuids
}

fn missing_startup_visibility(
    expected_uuids: &HashSet<String>,
    peer_uuid: &str,
    peer_verified_peers: &HashSet<String>,
) -> Vec<String> {
    let mut missing_uuids = expected_uuids
        .iter()
        .filter(|uuid| uuid.as_str() != peer_uuid)
        .filter(|uuid| !peer_verified_peers.contains(*uuid))
        .cloned()
        .collect::<Vec<_>>();
    missing_uuids.sort();
    missing_uuids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(values: &[&str]) -> HashSet<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn missing_startup_visibility_ignores_peer_own_uuid() {
        let expected = set(&["node-0", "node-1", "node-2"]);
        let verified = set(&["node-0"]);

        assert_eq!(
            missing_startup_visibility(&expected, "node-1", &verified),
            vec!["node-2".to_string()]
        );
    }

    #[test]
    fn missing_startup_visibility_accepts_full_peer_visibility() {
        let expected = set(&["node-0", "node-1", "node-2"]);
        let verified = set(&["node-0", "node-2", "stale-node-2"]);

        assert!(missing_startup_visibility(&expected, "node-1", &verified).is_empty());
    }
}
