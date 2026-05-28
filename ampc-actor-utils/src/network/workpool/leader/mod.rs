mod job_tracker;
mod leader_task;

pub use job_tracker::{JobTracker, JobType};

use crate::{
    execution::player::Identity,
    network::{
        tcp::{
            self,
            connection::server::{BoxTcpServer, TcpServer, TlsServer},
            SetupError, TlsServerConfig,
        },
        workpool::{JobId, Payload, WorkerId, WorkpoolError},
    },
};
use std::{
    collections::HashSet,
    future::Future,
    net::{self, SocketAddr},
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::{mpsc::Sender, oneshot, watch};
use tokio_util::sync::CancellationToken;

pub struct WorkerRsp {
    pub payload: Result<Payload, WorkpoolError>,
    pub job_id: JobId,
    pub worker_id: WorkerId,
}

pub struct WorkerJob {
    pub payload: Payload,
    pub worker_id: WorkerId,
}

pub type WorkpoolRes = Result<Vec<WorkerRsp>, WorkpoolError>;

pub(crate) enum Job {
    Broadcast {
        payload: Payload,
        result_rsp: oneshot::Sender<WorkpoolRes>,
        job_id: JobId,
    },
    ScatterGather {
        msgs: Vec<WorkerJob>,
        result_rsp: oneshot::Sender<WorkpoolRes>,
        job_id: JobId,
    },
    Cancel {
        job_id: JobId,
    },
}

/// Handle to a submitted job that can be awaited or cancelled
pub struct JobHandle {
    result_rx: oneshot::Receiver<WorkpoolRes>,
    cancel_tx: Sender<Job>,
    job_id: JobId,
}

impl JobHandle {
    pub fn job_id(&self) -> JobId {
        self.job_id
    }

    pub async fn cancel(self) -> Result<(), WorkpoolError> {
        self.cancel_tx
            .send(Job::Cancel {
                job_id: self.job_id,
            })
            .await
            .map_err(|_| WorkpoolError::SendFailed)
    }
}

impl Future for JobHandle {
    type Output = WorkpoolRes;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.result_rx)
            .poll(cx)
            .map(|r| r.unwrap_or(Err(WorkpoolError::SendFailed)))
    }
}

pub struct LeaderHandle {
    ct: CancellationToken,
    ch: Sender<Job>,
    next_job_id: Arc<AtomicU32>,
    num_workers: usize,
    worker_connected: Vec<watch::Receiver<bool>>,
}

pub struct LeaderArgs {
    pub leader_id: Identity,
    pub leader_address: String,
    pub worker_ids: Vec<Identity>,
    /// set to None for TCP
    pub tls: Option<TlsServerConfig>,
}

impl Drop for LeaderHandle {
    fn drop(&mut self) {
        self.ct.cancel();
    }
}

impl LeaderHandle {
    /// Broadcast to all workers, returns a handle that can be awaited or cancelled
    ///
    /// Accepts either `Bytes` or `Arc<dyn ToBytes>` via the `Payload` enum.
    /// Returns Err only for immediate failures (e.g., channel closed).
    /// Validation errors and worker failures are returned when awaiting the JobHandle.
    pub async fn broadcast(&self, payload: impl Into<Payload>) -> Result<JobHandle, WorkpoolError> {
        let job_id = self.next_job_id.fetch_add(1, Ordering::SeqCst);
        let (result_tx, result_rx) = oneshot::channel();

        self.ch
            .send(Job::Broadcast {
                job_id,
                payload: payload.into(),
                result_rsp: result_tx,
            })
            .await
            .map_err(|_| WorkpoolError::SendFailed)?;

        Ok(JobHandle {
            job_id,
            result_rx,
            cancel_tx: self.ch.clone(),
        })
    }

    /// Scatter-gather: send different payloads to different workers, returns a handle
    ///
    /// Returns Err for validation failures or immediate failures (e.g., channel closed).
    /// Worker failures are returned when awaiting the JobHandle.
    pub async fn scatter_gather(&self, msgs: Vec<WorkerJob>) -> Result<JobHandle, WorkpoolError> {
        if msgs.is_empty() {
            return Err(WorkpoolError::InvalidInput("empty".into()));
        }

        // Validate inputs
        let mut worker_ids = HashSet::new();
        for msg in &msgs {
            if msg.worker_id as usize >= self.num_workers {
                return Err(WorkpoolError::InvalidInput("invalid worker id".into()));
            }
            if !worker_ids.insert(msg.worker_id) {
                return Err(WorkpoolError::InvalidInput("duplicate worker id".into()));
            }
        }

        let job_id = self.next_job_id.fetch_add(1, Ordering::SeqCst);
        let (result_tx, result_rx) = oneshot::channel();

        self.ch
            .send(Job::ScatterGather {
                job_id,
                msgs,
                result_rsp: result_tx,
            })
            .await
            .map_err(|_| WorkpoolError::SendFailed)?;

        Ok(JobHandle {
            job_id,
            result_rx,
            cancel_tx: self.ch.clone(),
        })
    }

    pub fn num_workers(&self) -> usize {
        self.num_workers
    }

    /// Waits until all workers have established their connections.
    /// Returns immediately if all are already connected.
    /// Returns an error if cancelled, timed out, or if a watch channel is closed.
    pub async fn wait_for_all_connections(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<(), WorkpoolError> {
        let wait_fut = async {
            for rx in &mut self.worker_connected {
                tokio::select! {
                    _ = self.ct.cancelled() => return Err(WorkpoolError::Cancelled),
                    result = rx.wait_for(|&v| v) => {
                        result.map_err(|_| WorkpoolError::SendFailed)?;
                    }
                }
            }
            Ok(())
        };

        match timeout {
            Some(duration) => tokio::time::timeout(duration, wait_fut)
                .await
                .map_err(|_| WorkpoolError::Timeout)?,
            None => wait_fut.await,
        }
    }
}

pub async fn build_leader_handle(
    args: LeaderArgs,
    shutdown_ct: CancellationToken,
) -> Result<LeaderHandle, SetupError> {
    tcp::init_rustls_crypto_provider();
    let shutdown_ct = shutdown_ct.child_token();

    let leader_addr: SocketAddr = args
        .leader_address
        .parse()
        .map_err(|e: net::AddrParseError| SetupError::InvalidAddress(e.to_string()))?;
    let num_workers = args.worker_ids.len();
    let worker_ids = args.worker_ids;

    let (handle_tx, worker_connected) = if let Some(tls) = args.tls {
        tracing::info!("Building WorkPool Leader with TLS");

        let listener = TlsServer::new(leader_addr, tls).await?;

        Result::<_, SetupError>::Ok(leader_task::spawn(
            args.leader_id,
            worker_ids,
            listener,
            shutdown_ct.clone(),
        ))
    } else {
        tracing::info!("Building WorkPool Leader without TLS");

        let listener = BoxTcpServer(
            TcpServer::new(leader_addr)
                .await
                .map_err(|e| SetupError::ListenFailed(e.to_string()))?,
        );

        Result::<_, SetupError>::Ok(leader_task::spawn(
            args.leader_id,
            worker_ids,
            listener,
            shutdown_ct.clone(),
        ))
    }?;

    Ok(LeaderHandle {
        ct: shutdown_ct,
        ch: handle_tx,
        num_workers,
        next_job_id: Arc::new(AtomicU32::new(0)),
        worker_connected,
    })
}
