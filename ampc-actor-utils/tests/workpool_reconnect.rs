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
use futures::future::try_join_all;
use serial_test::serial;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
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
pub struct TcpProxy {
    /// Address the proxy listens on for the "client" side (e.g., leader connects here)
    listen_addr: SocketAddr,
    /// Address the proxy forwards traffic to (e.g., worker listens here)
    forward_addr: SocketAddr,
    /// Listener used by the accept loop
    listener: Arc<TcpListener>,
    /// When true, drop traffic from leader to worker
    drop_leader_to_worker: Arc<AtomicBool>,
    /// When true, drop traffic from worker to leader
    drop_worker_to_leader: Arc<AtomicBool>,
    /// Shutdown signal for active connections
    shutdown: CancellationToken,
    /// Shutdown signal for the accept loop
    accept_shutdown: CancellationToken,
    /// Join handle for the accept loop task
    accept_handle: Option<tokio::task::JoinHandle<()>>,
}

impl TcpProxy {
    pub async fn new(forward_addr: SocketAddr) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let listen_addr = listener.local_addr()?;
        let listener = Arc::new(listener);

        let drop_leader_to_worker = Arc::new(AtomicBool::new(false));
        let drop_worker_to_leader = Arc::new(AtomicBool::new(false));
        let shutdown = CancellationToken::new();
        let accept_shutdown = CancellationToken::new();

        let accept_handle = Some(tokio::spawn(Self::run_accept_loop(
            listener.clone(),
            forward_addr,
            drop_leader_to_worker.clone(),
            drop_worker_to_leader.clone(),
            shutdown.clone(),
            accept_shutdown.clone(),
        )));

        Ok(Self {
            listen_addr,
            forward_addr,
            listener,
            drop_leader_to_worker,
            drop_worker_to_leader,
            shutdown,
            accept_shutdown,
            accept_handle,
        })
    }

    /// Address that clients should connect to
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
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

    pub async fn reconnect(&mut self) {
        tracing::info!("Proxy: reconnect requested");

        self.accept_shutdown.cancel();
        if let Some(handle) = self.accept_handle.take() {
            let _ = handle.await;
        }

        self.shutdown.cancel();

        self.shutdown = CancellationToken::new();
        self.accept_shutdown = CancellationToken::new();
        self.accept_handle = Some(tokio::spawn(Self::run_accept_loop(
            self.listener.clone(),
            self.forward_addr,
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

    async fn run_accept_loop(
        listener: Arc<TcpListener>,
        forward_addr: SocketAddr,
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
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((client_stream, client_addr)) => {
                            tracing::info!("Proxy: accepted connection from {}", client_addr);

                            let drop_leader_to_worker = drop_leader_to_worker.clone();
                            let drop_worker_to_leader = drop_worker_to_leader.clone();
                            let shutdown = shutdown.clone();
                            tokio::spawn(async move {
                                if let Err(e) = Self::handle_connection(
                                    client_stream,
                                    forward_addr,
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

    async fn handle_connection(
        mut client_stream: TcpStream,
        forward_addr: SocketAddr,
        drop_leader_to_worker: Arc<AtomicBool>,
        drop_worker_to_leader: Arc<AtomicBool>,
        shutdown: CancellationToken,
    ) -> io::Result<()> {
        let mut server_stream = TcpStream::connect(forward_addr).await?;
        tracing::info!("Proxy: connected to server at {}", forward_addr);

        let (mut client_read, mut client_write) = client_stream.split();
        let (mut server_read, mut server_write) = server_stream.split();

        // Bidirectional forwarding with drop check
        let worker_to_leader = async {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = client_read.read(&mut buf).await?;
                if n == 0 {
                    return Ok(());
                }
                if !drop_worker_to_leader.load(Ordering::SeqCst) {
                    server_write.write_all(&buf[..n]).await?;
                }
            }
        };

        let leader_to_worker = async {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = server_read.read(&mut buf).await?;
                if n == 0 {
                    return Ok(());
                }
                if !drop_leader_to_worker.load(Ordering::SeqCst) {
                    client_write.write_all(&buf[..n]).await?;
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
    worker_addr: SocketAddr,
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
        worker_address: info.worker_addr.to_string(),
        leader_id: info.leader_id.clone(),
        leader_address: info.leader_addr.to_string(),
        tls: None,
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

    let worker_listeners: Vec<TcpListener> =
        try_join_all((0..NUM_WORKERS).map(|_| TcpListener::bind("127.0.0.1:0"))).await?;

    let worker_addrs: Vec<SocketAddr> = worker_listeners
        .iter()
        .map(|l| l.local_addr().unwrap())
        .collect();

    let leader_listener = TcpListener::bind("127.0.0.1:0").await?;
    let leader_addr = leader_listener.local_addr()?;
    drop(leader_listener); // Release so leader can bind

    // Create proxy for worker 0 -> leader traffic
    let proxy = TcpProxy::new(leader_addr).await?;
    let proxy_addr = proxy.listen_addr();

    // Leader uses real worker addresses (proxy is only on worker->leader path)
    let mut leader_worker_addrs: Vec<String> = Vec::new();
    for addr in worker_addrs.iter() {
        leader_worker_addrs.push(addr.to_string());
    }

    let leader_id = Identity("leader".to_string());
    for (idx, listener) in worker_listeners.into_iter().enumerate() {
        let worker_addr = listener.local_addr()?;
        drop(listener); // Release so worker can bind

        // Worker ID must match what leader expects: "{leader_id}-w-{idx}"
        let worker_id = Identity(format!("{}-w-{}", leader_id.0, idx));
        let leader_addr_for_worker = if idx == 0 { proxy_addr } else { leader_addr };

        let info = WorkerInfo {
            worker_id,
            worker_addr,
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
        worker_addresses: leader_worker_addrs,
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
