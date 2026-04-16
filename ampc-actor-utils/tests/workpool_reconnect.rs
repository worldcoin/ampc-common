//! Integration tests for workpool reconnection and dropped job detection.
//!
//! Uses a TCP proxy that can selectively drop messages to simulate network failures.

use ampc_actor_utils::network::workpool::leader::LeaderHandle;
use ampc_actor_utils::network::workpool::Payload;
use ampc_actor_utils::{
    execution::player::Identity,
    network::workpool::{
        leader::{build_leader, LeaderArgs, WorkerJob},
        worker::{build_worker_handle, WorkerArgs},
        WorkpoolError,
    },
};
use bytes::Bytes;
use eyre::Result;
use serial_test::serial;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, Duration};
use tokio_util::sync::CancellationToken;
use tracing_test::traced_test;

/// Global signal for tests to control when workers complete their jobs.
/// Used by serial tests to ensure deterministic timing.
static WORKER_PROCEED: OnceLock<Notify> = OnceLock::new();

fn worker_proceed_signal() -> &'static Notify {
    WORKER_PROCEED.get_or_init(Notify::new)
}

// ─────────────────────────────────────────────────────────────────────────────
// TCP Proxy Implementation
// ─────────────────────────────────────────────────────────────────────────────

/// A TCP proxy that sits between leader and worker, allowing message interception.
///
/// The worker connects to the proxy's `worker_to_leader` address; the proxy then
/// initiates the outbound connection to the leader, creating a bidirectional bridge.
/// Both leader-to-worker and worker-to-leader traffic flows through this single bridge.
pub struct TcpProxy {
    leader_addr: SocketAddr,
    worker_to_leader: Arc<TcpListener>,
    /// When true, drop traffic from leader to worker
    drop_leader_to_worker: Arc<AtomicBool>,
    /// When true, drop traffic from worker to leader
    drop_worker_to_leader: Arc<AtomicBool>,
    /// Shutdown signal for active connections
    shutdown: CancellationToken,
    /// Shutdown signal for the accept loop
    accept_shutdown: CancellationToken,
    worker_to_leader_handle: Option<JoinHandle<()>>,
}

impl TcpProxy {
    /// Create a new proxy.
    ///
    /// - `leader_addr`: the real leader address; the proxy connects here when a worker arrives.
    /// - `worker_to_leader_addr`: the address the proxy listens on; workers connect here.
    pub async fn new(
        leader_addr: SocketAddr,
        worker_to_leader_addr: SocketAddr,
        worker_to_leader: Arc<TcpListener>,
    ) -> Result<Self> {
        let drop_leader_to_worker = Arc::new(AtomicBool::new(false));
        let drop_worker_to_leader = Arc::new(AtomicBool::new(false));
        let shutdown = CancellationToken::new();
        let accept_shutdown = CancellationToken::new();

        let worker_to_leader_handle = Some(tokio::spawn(Self::worker_to_leader_accept_loop(
            leader_addr,
            worker_to_leader.clone(),
            drop_leader_to_worker.clone(),
            drop_worker_to_leader.clone(),
            shutdown.clone(),
            accept_shutdown.clone(),
        )));

        Ok(Self {
            leader_addr,
            worker_to_leader,
            drop_leader_to_worker,
            drop_worker_to_leader,
            shutdown,
            accept_shutdown,
            worker_to_leader_handle,
        })
    }

    pub fn drop_leader_to_worker(&self) {
        tracing::info!("Proxy: dropping leader-to-worker traffic");
        self.drop_leader_to_worker.store(true, Ordering::SeqCst);
    }

    pub fn drop_worker_to_leader(&self) {
        tracing::info!("Proxy: dropping worker-to-leader traffic");
        self.drop_worker_to_leader.store(true, Ordering::SeqCst);
    }

    pub fn drop_both_directions(&self) {
        tracing::info!("Proxy: dropping traffic both directions");
        self.drop_leader_to_worker.store(true, Ordering::SeqCst);
        self.drop_worker_to_leader.store(true, Ordering::SeqCst);
    }

    pub fn restore_leader_to_worker(&self) {
        tracing::info!("Proxy: restoring leader-to-worker traffic");
        self.drop_leader_to_worker.store(false, Ordering::SeqCst);
    }

    pub fn restore_worker_to_leader(&self) {
        tracing::info!("Proxy: restoring worker-to-leader traffic");
        self.drop_worker_to_leader.store(false, Ordering::SeqCst);
    }

    pub fn restore_both_directions(&self) {
        tracing::info!("Proxy: restoring traffic both directions");
        self.drop_leader_to_worker.store(false, Ordering::SeqCst);
        self.drop_worker_to_leader.store(false, Ordering::SeqCst);
    }

    pub async fn disconnect(&mut self) {
        tracing::info!("Proxy: disconnect requested");
        self.shutdown();

        if let Some(handle) = self.worker_to_leader_handle.take() {
            let _ = handle.await;
        }
    }

    pub async fn reconnect(&mut self) {
        tracing::info!("Proxy: reconnect requested");
        self.shutdown();

        if let Some(handle) = self.worker_to_leader_handle.take() {
            let _ = handle.await;
        }

        self.shutdown = CancellationToken::new();
        self.accept_shutdown = CancellationToken::new();
        self.worker_to_leader_handle = Some(tokio::spawn(Self::worker_to_leader_accept_loop(
            self.leader_addr,
            self.worker_to_leader.clone(),
            self.drop_leader_to_worker.clone(),
            self.drop_worker_to_leader.clone(),
            self.shutdown.clone(),
            self.accept_shutdown.clone(),
        )));
    }

    pub fn shutdown(&self) {
        self.accept_shutdown.cancel();
        self.shutdown.cancel();
    }

    async fn worker_to_leader_accept_loop(
        leader_addr: SocketAddr,
        worker_to_leader_listener: Arc<TcpListener>,
        drop_leader_to_worker: Arc<AtomicBool>,
        drop_worker_to_leader: Arc<AtomicBool>,
        shutdown: CancellationToken,
        accept_shutdown: CancellationToken,
    ) {
        loop {
            tokio::select! {
                _ = accept_shutdown.cancelled() => {
                    tracing::info!("Proxy accept loop shutting down");
                    break;
                }
                accept_result = worker_to_leader_listener.accept() => {
                    match accept_result {
                        Ok((worker_stream, worker_addr)) => {
                            tracing::info!("Proxy: accepted connection from worker: {}", worker_addr );

                            let drop_leader_to_worker = drop_leader_to_worker.clone();
                            let drop_worker_to_leader = drop_worker_to_leader.clone();
                            let shutdown = shutdown.clone();
                            tokio::spawn(async move {
                                if let Err(e) = Self::handle_worker_to_leader(
                                    worker_stream,
                                    leader_addr,
                                    drop_leader_to_worker,
                                    drop_worker_to_leader,
                                    shutdown,
                                ).await {
                                    tracing::debug!("Proxy connection ended: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Proxy accept error: {}", e);
                        }
                    }
                }
            }
        }
    }

    async fn handle_worker_to_leader(
        worker_stream: TcpStream,
        leader_addr: SocketAddr,
        drop_leader_to_worker: Arc<AtomicBool>,
        drop_worker_to_leader: Arc<AtomicBool>,
        shutdown: CancellationToken,
    ) -> io::Result<()> {
        let leader_stream = TcpStream::connect(leader_addr).await?;
        tracing::info!("Proxy: connected to leader at {}", leader_addr);
        Self::manage_streams(
            worker_stream,
            leader_stream,
            drop_leader_to_worker,
            drop_worker_to_leader,
            shutdown,
        )
        .await
    }

    async fn manage_streams(
        mut worker_stream: TcpStream,
        mut leader_stream: TcpStream,
        drop_leader_to_worker: Arc<AtomicBool>,
        drop_worker_to_leader: Arc<AtomicBool>,
        shutdown: CancellationToken,
    ) -> io::Result<()> {
        let (mut leader_read, mut leader_write) = leader_stream.split();
        let (mut worker_read, mut worker_write) = worker_stream.split();

        // Bidirectional forwarding with drop check
        let worker_to_leader = async {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = worker_read.read(&mut buf).await?;
                if n == 0 {
                    return Ok(());
                }
                if !drop_worker_to_leader.load(Ordering::SeqCst) {
                    leader_write.write_all(&buf[..n]).await?;
                }
            }
        };

        let leader_to_worker = async {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = leader_read.read(&mut buf).await?;
                if n == 0 {
                    return Ok(());
                }
                if !drop_leader_to_worker.load(Ordering::SeqCst) {
                    worker_write.write_all(&buf[..n]).await?;
                }
            }
        };

        tokio::select! {
            _ = shutdown.cancelled() => Ok(()),
            r = worker_to_leader => r,
            r = leader_to_worker => r,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test Infrastructure
// ─────────────────────────────────────────────────────────────────────────────

const NUM_WORKERS: usize = 3;
const JOB_TIMEOUT: Duration = Duration::from_secs(2);

/// Info needed to recreate a worker
struct WorkerInfo {
    worker_id: Identity,
    leader_id: Identity,
    leader_addr: SocketAddr,
}

/// Spawns a worker and returns its cancellation token
async fn spawn_worker(
    info: &WorkerInfo,
    shutdown_ct: CancellationToken,
) -> Result<CancellationToken> {
    let worker_args = WorkerArgs {
        worker_id: info.worker_id.clone(),
        leader_id: info.leader_id.clone(),
        leader_address: info.leader_addr.to_string(),
        root_certs: None,
    };

    let mut worker = build_worker_handle(worker_args, shutdown_ct.clone()).await?;

    // Echo worker - waits for proceed signal then returns what it receives.
    // Tests can control timing by signaling worker_proceed_signal().
    tokio::spawn(async move {
        while let Some(mut job) = worker.recv().await {
            tracing::info!("worker received job");
            worker_proceed_signal().notified().await;
            tracing::info!("worker processing job");
            let payload = job.take_payload();
            job.send_result(payload);
        }
    });

    Ok(shutdown_ct)
}

/// Cluster state returned by start_cluster_with_proxy
struct TestCluster {
    leader: LeaderHandle,
    proxy: TcpProxy,
    global_shutdown: CancellationToken,
}

/// Start a cluster with a proxy between leader and worker 0.
async fn start_cluster_with_proxy() -> Result<TestCluster> {
    let global_shutdown = CancellationToken::new();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_worker_to_leader_addr = proxy_listener.local_addr()?;
    drop(proxy_listener);

    let leader_listener = Arc::new(TcpListener::bind("127.0.0.1:0").await?);
    let leader_addr = leader_listener.local_addr()?;

    // Create proxy for worker 0: worker connects to proxy, proxy connects to leader.
    let proxy = TcpProxy::new(leader_addr, proxy_worker_to_leader_addr, leader_listener).await?;

    let leader_id = Identity("leader".to_string());
    let mut worker_ids = vec![];
    for idx in 0..NUM_WORKERS {
        // Worker ID must match what leader expects: "{leader_id}-w-{idx}"
        let worker_id = Identity(format!("{}-w-{}", leader_id.0, idx));
        worker_ids.push(worker_id.clone());
        // Worker 0 connects through the proxy; all others connect directly to the leader.
        let leader_addr_for_worker = if idx == 0 {
            proxy_worker_to_leader_addr
        } else {
            leader_addr
        };

        let info = WorkerInfo {
            worker_id,
            leader_id: leader_id.clone(),
            leader_addr: leader_addr_for_worker,
        };

        // Each worker gets its own cancellation token (child of global)
        let worker_ct = global_shutdown.child_token();
        spawn_worker(&info, worker_ct.clone()).await?;
    }

    let leader_args = LeaderArgs {
        leader_id,
        leader_address: leader_addr.to_string(),
        worker_ids,
        tls: None,
    };
    let leader = build_leader(leader_args, global_shutdown.clone()).await?;

    Ok(TestCluster {
        leader,
        proxy,
        global_shutdown,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// Test that job responses survive a network disconnect and are delivered after reconnection.
/// This verifies the response channel lives outside the connection loop.
#[serial]
#[traced_test]
#[tokio::test(flavor = "multi_thread")]
async fn test_response_survives_disconnect() {
    let mut cluster = start_cluster_with_proxy()
        .await
        .expect("cluster should start");

    cluster
        .leader
        .wait_for_all_connections(Some(Duration::from_secs(5)))
        .await
        .expect("workers should connect");
    tracing::info!("All workers connected");

    let payload = Bytes::from("test-payload");
    let job_handle = cluster
        .leader
        .scatter_gather(vec![WorkerJob {
            worker_id: 0,
            payload: payload.clone().into(),
        }])
        .await
        .expect("scatter_gather should succeed");

    // Give time for the job to arrive at worker
    sleep(Duration::from_millis(500)).await;
    cluster.proxy.reconnect().await;
    // Wait for connection to fully close before signaling worker to proceed.
    // This ensures the response is queued after the connection is down.
    sleep(Duration::from_secs(5)).await;
    worker_proceed_signal().notify_waiters();

    // The job should complete successfully - response survives the disconnect
    let mut result = timeout(JOB_TIMEOUT, job_handle)
        .await
        .expect("job should complete")
        .expect("job should succeed");

    assert_eq!(result.len(), 1);
    let rsp = result.pop().unwrap();
    assert_eq!(rsp.worker_id, 0);
    let received = rsp.payload.expect("worker should succeed");
    assert_eq!(received.to_bytes(), payload);
    tracing::info!("Job response survived disconnect successfully");

    cluster.proxy.shutdown();
    cluster.global_shutdown.cancel();
}

#[serial]
#[traced_test]
#[tokio::test(flavor = "multi_thread")]
async fn test_proxy_drop() {
    let mut cluster = start_cluster_with_proxy()
        .await
        .expect("cluster should start");

    cluster
        .leader
        .wait_for_all_connections(Some(Duration::from_secs(5)))
        .await
        .expect("workers should connect");
    tracing::info!("All workers connected");

    cluster.proxy.drop_both_directions();
    sleep(Duration::from_millis(200)).await;

    let payload = Bytes::from("test-payload");
    let job_handle = cluster
        .leader
        .scatter_gather(vec![WorkerJob {
            worker_id: 0,
            payload: payload.clone().into(),
        }])
        .await
        .expect("scatter_gather should succeed");

    sleep(Duration::from_millis(200)).await;
    cluster.proxy.restore_both_directions();
    // force an exchange of pending jobs
    cluster.proxy.reconnect().await;
    sleep(Duration::from_millis(500)).await;
    worker_proceed_signal().notify_waiters();
    sleep(Duration::from_secs(5)).await;

    let mut result = timeout(JOB_TIMEOUT, job_handle)
        .await
        .expect("job should complete")
        .expect("job should succeed");

    assert_eq!(result.len(), 1);
    let rsp = result.pop().unwrap();
    assert_eq!(rsp.worker_id, 0);
    assert!(rsp.payload.is_err());

    cluster.proxy.shutdown();
    cluster.global_shutdown.cancel();
}

/// Test that jobs complete successfully through the proxy under normal conditions.
#[serial]
#[traced_test]
#[tokio::test(flavor = "multi_thread")]
async fn test_broadcast_through_proxy() {
    let mut cluster = start_cluster_with_proxy()
        .await
        .expect("cluster should start");

    cluster
        .leader
        .wait_for_all_connections(Some(Duration::from_secs(5)))
        .await
        .expect("workers should connect");

    // Submit a broadcast job (goes through proxy for worker 0)
    let payload: Bytes = b"proxy-test".as_slice().into();
    let job_handle = cluster
        .leader
        .broadcast(payload.clone())
        .await
        .expect("broadcast should succeed");
    sleep(Duration::from_millis(500)).await;
    worker_proceed_signal().notify_waiters();

    let result = timeout(JOB_TIMEOUT, job_handle)
        .await
        .expect("job should complete")
        .expect("job should succeed");

    assert_eq!(result.len(), NUM_WORKERS);
    for rsp in result {
        let received = rsp.payload.expect("worker should succeed").to_bytes();
        assert_eq!(received, payload);
    }

    cluster.proxy.shutdown();
    cluster.global_shutdown.cancel();
}

/// Test that dropping leader->worker traffic delays only worker 0 until reconnect.
///
/// Sequence:
/// 1. Start cluster, wait for connections
/// 2. Block leader->worker traffic via proxy
/// 3. Send job to worker 0 - leader queues but worker never receives
/// 4. Reconnect proxy to trigger job exchange
/// 5. Restore traffic; job should be detected as dropped
#[serial]
#[traced_test]
#[tokio::test(flavor = "multi_thread")]
async fn test_dropped_job_detection() {
    let mut cluster = start_cluster_with_proxy()
        .await
        .expect("cluster should start");

    cluster
        .leader
        .wait_for_all_connections(Some(Duration::from_secs(5)))
        .await
        .expect("workers should connect");

    tracing::info!("All workers connected");

    cluster.proxy.drop_leader_to_worker();
    sleep(Duration::from_millis(200)).await;

    let job_handle = cluster
        .leader
        .scatter_gather(vec![
            WorkerJob {
                worker_id: 0,
                payload: Bytes::from("payload-0").into(),
            },
            WorkerJob {
                worker_id: 1,
                payload: Bytes::from("payload-1").into(),
            },
            WorkerJob {
                worker_id: 2,
                payload: Bytes::from("payload-2").into(),
            },
        ])
        .await
        .expect("scatter_gather should succeed");

    let mut job_handle = Box::pin(job_handle);
    tokio::select! {
        _ = sleep(Duration::from_millis(200)) => {
            tracing::info!("Job not delivered while leader->worker is dropped");
        }
        _result = job_handle.as_mut() => {
            panic!("Job completed unexpectedly before reconnect");
        }
    }

    cluster.proxy.restore_leader_to_worker();
    cluster.proxy.reconnect().await;
    sleep(Duration::from_millis(500)).await;
    worker_proceed_signal().notify_waiters();
    sleep(Duration::from_secs(5)).await;

    // The job should complete successfully after reconnect exchange
    let result = timeout(JOB_TIMEOUT, job_handle.as_mut())
        .await
        .expect("job should complete");

    let b1 = Bytes::from("payload-1");
    let b2 = Bytes::from("payload-2");

    fn match_payload(p: Payload, expected: &Bytes) {
        match p {
            Payload::Bytes(b) => assert_eq!(b, expected),
            _ => panic!(),
        }
    }

    match result {
        Ok(responses) => {
            assert_eq!(responses.len(), 3);
            for rsp in responses {
                match rsp.worker_id {
                    0 => assert!(matches!(
                        rsp.payload,
                        Err(WorkpoolError::JobsLost {
                            worker_id: 0,
                            job_id: 0
                        })
                    )),
                    1 => match_payload(rsp.payload.expect("payload should be ok"), &b1),
                    2 => match_payload(rsp.payload.expect("payload should be ok"), &b2),
                    other => panic!("Unexpected worker id: {}", other),
                }
            }
        }
        Err(e) => panic!("Job failed unexpectedly: {:?}", e),
    }

    cluster.proxy.shutdown();
    cluster.global_shutdown.cancel();
}

// test what happens when PendingJobsReply comes after other job responses.
// this is accomplished by sending a job, taking down the proxy, notifying
// the worker, then bringing the proxy back up. the completed job would
// be sent first, followed by get_pending_jobs. No jobs should be lost
// and the job should complete.
#[serial]
#[traced_test]
#[tokio::test(flavor = "multi_thread")]
async fn test_ordering_pending_jobs_reply() {
    let mut cluster = start_cluster_with_proxy()
        .await
        .expect("cluster should start");

    cluster
        .leader
        .wait_for_all_connections(Some(Duration::from_secs(5)))
        .await
        .expect("workers should connect");

    tracing::info!("All workers connected");

    let job_handle = cluster
        .leader
        .scatter_gather(vec![
            WorkerJob {
                worker_id: 0,
                payload: Bytes::from("payload-0").into(),
            },
            WorkerJob {
                worker_id: 1,
                payload: Bytes::from("payload-1").into(),
            },
            WorkerJob {
                worker_id: 2,
                payload: Bytes::from("payload-2").into(),
            },
        ])
        .await
        .expect("scatter_gather should succeed");

    let mut job_handle = Box::pin(job_handle);
    tokio::select! {
        _ = sleep(Duration::from_millis(200)) => {
            tracing::info!("job should have propagated");
        },
        _result = job_handle.as_mut() => {
            panic!("Job completed unexpectedly");
        }
    }

    cluster.proxy.disconnect().await;
    sleep(Duration::from_secs(1)).await;
    worker_proceed_signal().notify_waiters();
    sleep(Duration::from_millis(200)).await;
    tracing::info!("workers should be trying to send their responses by now");

    cluster.proxy.reconnect().await;

    // The job should complete successfully after reconnect exchange
    let result = timeout(Duration::from_secs(10), job_handle.as_mut())
        .await
        .expect("job should complete");

    let b0 = Bytes::from("payload-0");
    let b1 = Bytes::from("payload-1");
    let b2 = Bytes::from("payload-2");

    fn match_payload(p: Payload, expected: &Bytes) {
        match p {
            Payload::Bytes(b) => assert_eq!(b, expected),
            _ => panic!(),
        }
    }

    match result {
        Ok(responses) => {
            assert_eq!(responses.len(), 3);
            for rsp in responses {
                match rsp.worker_id {
                    0 => match_payload(rsp.payload.expect("payload should be ok"), &b0),
                    1 => match_payload(rsp.payload.expect("payload should be ok"), &b1),
                    2 => match_payload(rsp.payload.expect("payload should be ok"), &b2),
                    other => panic!("Unexpected worker id: {}", other),
                }
            }
        }
        Err(e) => panic!("Job failed unexpectedly: {:?}", e),
    }

    cluster.proxy.shutdown();
    cluster.global_shutdown.cancel();
}
