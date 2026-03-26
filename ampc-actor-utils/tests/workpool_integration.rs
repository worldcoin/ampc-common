//! End-to-end integration tests for the workpool networking layer.
//!
//! Spawns 1 leader and 3 workers over plain TCP (no TLS), then verifies
//! that broadcast and scatter-gather both deliver the correct payloads.

use std::sync::Arc;

use ampc_actor_utils::{
    execution::player::Identity,
    network::workpool::{
        leader::{build_leader, LeaderArgs, WorkerJob},
        worker::{build_worker_handle, WorkerArgs},
        Payload, ToBytes,
    },
};
use bytes::{Bytes, BytesMut};
use futures::future::join_all;
use serial_test::serial;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;
use tracing_test::traced_test;

/// Test struct for BigJob serialization
#[derive(Debug, Clone, PartialEq)]
struct FakeIrises {
    ids: Vec<u32>,
    codes: Vec<Vec<u32>>,
}

const FAKE_IRISES_DESCRIPTOR: u8 = 0xAB;

impl ToBytes for FakeIrises {
    fn to_bytes(&self, buf: &mut BytesMut) {
        // Descriptor byte
        buf.extend_from_slice(&[FAKE_IRISES_DESCRIPTOR]);

        // 4-byte length for ids, then ids
        buf.extend_from_slice(&(self.ids.len() as u32).to_le_bytes());
        for id in &self.ids {
            buf.extend_from_slice(&id.to_le_bytes());
        }

        // 4-byte length for codes (number of inner vecs)
        buf.extend_from_slice(&(self.codes.len() as u32).to_le_bytes());
        for code in &self.codes {
            // 4-byte length for each inner vec
            buf.extend_from_slice(&(code.len() as u32).to_le_bytes());
            for val in code {
                buf.extend_from_slice(&val.to_le_bytes());
            }
        }
    }

    fn len(&self) -> usize {
        // descriptor (1) + ids_len (4) + ids (4 * n) + codes_len (4) + codes (4 + 4*m each)
        1 + 4 + (self.ids.len() * 4) + 4 + self.codes.iter().map(|c| 4 + c.len() * 4).sum::<usize>()
    }
}

impl From<Bytes> for FakeIrises {
    fn from(bytes: Bytes) -> Self {
        let mut offset = 0;

        // Skip descriptor byte
        assert_eq!(bytes[offset], FAKE_IRISES_DESCRIPTOR);
        offset += 1;

        // Read ids length
        let ids_len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;

        // Read ids
        let mut ids = Vec::with_capacity(ids_len);
        for _ in 0..ids_len {
            ids.push(u32::from_le_bytes(
                bytes[offset..offset + 4].try_into().unwrap(),
            ));
            offset += 4;
        }

        // Read codes length
        let codes_len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;

        // Read codes
        let mut codes = Vec::with_capacity(codes_len);
        for _ in 0..codes_len {
            let inner_len =
                u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;

            let mut inner = Vec::with_capacity(inner_len);
            for _ in 0..inner_len {
                inner.push(u32::from_le_bytes(
                    bytes[offset..offset + 4].try_into().unwrap(),
                ));
                offset += 4;
            }
            codes.push(inner);
        }

        FakeIrises { ids, codes }
    }
}

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
/// per worker
#[serial]
#[traced_test]
#[tokio::test(flavor = "multi_thread")]
async fn test_broadcast() {
    let (mut leader, shutdown) = start_cluster().await;
    assert_eq!(leader.num_workers(), NUM_WORKERS);

    leader
        .wait_for_all_connections(Some(Duration::from_secs(10)))
        .await
        .unwrap();
    tracing::info!("all workers connected");

    let payload: Bytes = b"hello-broadcast".as_slice().into();
    tracing::info!("submitting broadcast");

    let responses = timeout(
        JOB_TIMEOUT,
        leader
            .broadcast(payload.clone())
            .await
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

    for rsp in responses.into_iter() {
        let received = rsp.payload.expect("worker returned an error").to_bytes();
        assert_eq!(
            received, payload,
            "worker should echo the payload back unchanged"
        );
    }

    shutdown.cancel();
}

/// Scatter-gather sends a distinct payload to each worker and collects all
/// responses
#[serial]
#[traced_test]
#[tokio::test(flavor = "multi_thread")]
async fn test_scatter_gather() {
    let (mut leader, shutdown) = start_cluster().await;
    leader
        .wait_for_all_connections(Some(Duration::from_secs(10)))
        .await
        .unwrap();

    let expected_payloads: Vec<Vec<u8>> = (0..NUM_WORKERS)
        .map(|i| format!("data-for-worker-{i}").into_bytes())
        .collect();

    let mut job_handles = vec![];
    for workers_to_use in 1..=NUM_WORKERS {
        let msgs: Vec<WorkerJob> = (0..workers_to_use)
            .map(|i| WorkerJob {
                worker_id: i as u16,
                payload: format!("data-for-worker-{i}").into_bytes().into(),
            })
            .collect();

        let handle = leader
            .scatter_gather(msgs)
            .await
            .expect("scatter_gather failed");
        job_handles.push(handle);
    }

    let jobs = timeout(JOB_TIMEOUT, join_all(job_handles))
        .await
        .expect("scatter_gather timed out");

    for (idx, responses) in jobs.into_iter().enumerate() {
        let responses = responses.expect("job failed");
        assert_eq!(idx + 1, responses.len(), "incorrect number of responses");

        for rsp in responses.into_iter() {
            let received = rsp.payload.expect("worker returned an error").to_bytes();
            assert_eq!(
                received.as_ref(),
                expected_payloads[rsp.worker_id as usize].as_slice(),
                "worker should echo its own payload back"
            );
        }
    }

    shutdown.cancel();
}

/// BigJob broadcast sends a large payload using ToPacked and receives it back
#[serial]
#[traced_test]
#[tokio::test(flavor = "multi_thread")]
async fn test_broadcast_big_job() {
    let (mut leader, shutdown) = start_cluster().await;
    assert_eq!(leader.num_workers(), NUM_WORKERS);

    leader
        .wait_for_all_connections(Some(Duration::from_secs(10)))
        .await
        .unwrap();
    tracing::info!("all workers connected");

    let expected = FakeIrises {
        ids: vec![1, 2, 3, 4, 5],
        codes: vec![vec![10, 20, 30], vec![40, 50], vec![60, 70, 80, 90]],
    };
    let payload: Payload = Arc::new(expected.clone()).into();

    tracing::info!("submitting broadcast with packed payload");

    let responses = timeout(
        JOB_TIMEOUT,
        leader
            .broadcast(payload)
            .await
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

    for rsp in responses.into_iter() {
        let payload = rsp.payload.expect("worker returned an error");
        let received = FakeIrises::from(payload.to_bytes());
        assert_eq!(
            received, expected,
            "worker should echo the FakeIrises back unchanged"
        );
    }

    shutdown.cancel();
}
