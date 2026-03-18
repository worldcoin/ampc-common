//! End-to-end integration tests for the workpool networking layer.
//!
//! Spawns 1 leader and 3 workers over plain TCP (no TLS), then verifies
//! that broadcast and scatter-gather both deliver the correct payloads.

use ampc_actor_utils::{
    execution::player::Identity,
    network::workpool::{
        leader::{build_leader, LeaderArgs, WorkerJob},
        worker::{build_worker_handle, WorkerArgs},
    },
};
use serial_test::serial;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;

const NUM_WORKERS: usize = 3;
const JOB_TIMEOUT: Duration = Duration::from_secs(10);

/// Bind `n` listeners simultaneously so the OS assigns `n` distinct free ports,
/// then return those ports.  Holding all listeners alive until all ports are
/// collected prevents the OS from handing out the same port twice.
fn find_free_ports(n: usize) -> Vec<u16> {
    let listeners: Vec<std::net::TcpListener> = (0..n)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect()
}

/// Start `NUM_WORKERS` echo workers and one leader, all connected over localhost TCP.
///
/// Worker identities follow the naming convention the leader expects:
/// `"{leader_id}-w-{idx}"`.  Workers echo every payload back unchanged.
///
/// Returns the `LeaderHandle` and a `CancellationToken` to shut everything down.
async fn start_cluster() -> (
    ampc_actor_utils::network::workpool::leader::LeaderHandle,
    CancellationToken,
) {
    let ports = find_free_ports(1 + NUM_WORKERS);
    let leader_port = ports[0];
    let worker_ports = &ports[1..];

    let leader_addr = format!("127.0.0.1:{leader_port}");
    let worker_addrs: Vec<String> = worker_ports
        .iter()
        .map(|p| format!("127.0.0.1:{p}"))
        .collect();

    tracing::info!(leader_addr, ?worker_addrs, "starting cluster");

    let leader_id = Identity("leader".to_string());
    let shutdown = CancellationToken::new();

    // Start workers before the leader so their listeners are ready.
    for (idx, worker_addr) in worker_addrs.iter().enumerate() {
        // The leader internally names workers "{leader_id}-w-{idx}",
        // so the worker must announce that same identity in the handshake.
        let worker_id_str = format!("leader-w-{idx}");
        tracing::info!(worker_id = worker_id_str, worker_addr, "building worker");
        let args = WorkerArgs {
            worker_id: Identity(worker_id_str.clone()),
            worker_address: worker_addr.clone(),
            leader_id: leader_id.clone(),
            leader_address: leader_addr.clone(),
            tls: None,
        };
        let mut worker = build_worker_handle(args, shutdown.clone())
            .await
            .expect("failed to build worker");

        // Echo every payload back to the leader unchanged.
        tokio::spawn(async move {
            while let Some(mut job) = worker.recv().await {
                let payload = job.take_payload();
                tracing::info!(
                    worker_id = worker_id_str,
                    bytes = payload.len(),
                    "worker echoing payload"
                );
                job.send_result(payload);
            }
            tracing::info!(worker_id = worker_id_str, "worker echo loop exited");
        });
    }

    tracing::info!(leader_addr, "building leader");
    let leader = build_leader(
        LeaderArgs {
            leader_id,
            leader_address: leader_addr,
            worker_addresses: worker_addrs,
            tls: None,
        },
        shutdown.clone(),
    )
    .await
    .expect("failed to build leader");

    tracing::info!(num_workers = leader.num_workers(), "cluster ready");
    (leader, shutdown)
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Broadcast sends the same payload to all workers and collects one response
/// per worker, ordered by worker_id.
#[serial]
#[tokio::test(flavor = "multi_thread")]
async fn test_broadcast() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    let (mut leader, shutdown) = start_cluster().await;
    assert_eq!(leader.num_workers(), NUM_WORKERS);

    leader.wait_for_all_connections().await;
    tracing::info!("all workers connected");

    let payload = b"hello-broadcast".to_vec();
    tracing::info!("submitting broadcast");

    let responses = timeout(
        JOB_TIMEOUT,
        leader
            .broadcast(payload.clone())
            .expect("failed to submit broadcast"),
    )
    .await
    .expect("broadcast timed out")
    .expect("broadcast job failed");

    tracing::info!(count = responses.len(), "broadcast complete");
    assert_eq!(
        responses.len(),
        NUM_WORKERS,
        "expected one response per worker"
    );

    for (expected_worker_id, rsp) in responses.iter().enumerate() {
        assert_eq!(
            rsp.worker_id, expected_worker_id as u16,
            "responses must be in worker_id order"
        );
        assert_eq!(
            rsp.payload.as_ref().expect("worker returned an error"),
            &payload,
            "worker should echo the payload back unchanged"
        );
    }

    shutdown.cancel();
}

/// Scatter-gather sends a distinct payload to each worker and collects all
/// responses, sorted by worker_id.
#[serial]
#[tokio::test(flavor = "multi_thread")]
async fn test_scatter_gather() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    let (mut leader, shutdown) = start_cluster().await;

    let msgs: Vec<WorkerJob> = (0..NUM_WORKERS)
        .map(|i| WorkerJob {
            worker_id: i as u16,
            payload: format!("data-for-worker-{i}").into_bytes(),
        })
        .collect();

    let expected_payloads: Vec<Vec<u8>> = (0..NUM_WORKERS)
        .map(|i| format!("data-for-worker-{i}").into_bytes())
        .collect();

    leader.wait_for_all_connections().await;
    tracing::info!("all workers connected");

    tracing::info!("submitting scatter_gather");

    let responses = timeout(
        JOB_TIMEOUT,
        leader
            .scatter_gather(msgs)
            .expect("failed to submit scatter_gather"),
    )
    .await
    .expect("scatter_gather timed out")
    .expect("scatter_gather job failed");

    tracing::info!(count = responses.len(), "scatter_gather complete");
    assert_eq!(
        responses.len(),
        NUM_WORKERS,
        "expected one response per worker"
    );

    for (expected_worker_id, rsp) in responses.iter().enumerate() {
        assert_eq!(
            rsp.worker_id, expected_worker_id as u16,
            "responses must be in worker_id order"
        );
        assert_eq!(
            rsp.payload.as_ref().expect("worker returned an error"),
            &expected_payloads[expected_worker_id],
            "worker should echo its own payload back"
        );
    }

    shutdown.cancel();
}
