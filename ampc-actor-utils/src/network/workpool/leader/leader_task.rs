use bytes::Bytes;
use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::network::workpool::LeaderError;
use crate::network::workpool::{
    leader::job_tracker::{JobTracker, JobType},
    leader::{Job, WorkerRsp},
};
use crate::network::workpool::{
    read_and_parse_inbound, serialize_and_write_outbound, value::NetworkValue, JobId, WorkerId,
};
use crate::{
    execution::player::Identity,
    network::tcp::{
        accept_loop, Client, ConnectionId, ConnectionRequest, ConnectionState, NetworkConnection,
        Peer, Server,
    },
};

/// Spawns the leader task and per-worker manager tasks.
///
/// Returns the job sender and one `watch::Receiver<bool>` per worker.
/// Each receiver starts as `false` and flips to `true` once that worker's
/// connection (including the job-state handshake) is fully established.
pub fn spawn<T, C, I, S>(
    my_id: Identity,
    worker_urls: I,
    connector: C,
    listener: S,
    shutdown_ct: CancellationToken,
) -> (UnboundedSender<Job>, Vec<watch::Receiver<bool>>)
where
    T: NetworkConnection + 'static,
    C: Client<Output = T> + 'static,
    I: Iterator<Item = String>,
    S: Server<Output = T> + 'static,
{
    let my_id = Arc::new(my_id);
    let job_tracker = Arc::new(JobTracker::new());
    let shutdown_ct = shutdown_ct.child_token();
    let connection_state = ConnectionState::new(shutdown_ct.clone(), CancellationToken::new());
    let workers: Vec<_> = worker_urls
        .enumerate()
        .map(|(idx, url)| Arc::new(Peer::new(Identity(format!("{}-w-{}", &my_id.0, idx)), url)))
        .collect();

    let (conn_cmd_tx, conn_cmd_rx) = mpsc::unbounded_channel::<ConnectionRequest<T>>();
    tokio::spawn(accept_loop(listener, conn_cmd_rx, shutdown_ct.clone()));

    let mut worker_cmd_txs = Vec::new();
    let (worker_rsp_tx, worker_rsp_rx) = mpsc::unbounded_channel();
    let mut conn_rxs = Vec::with_capacity(workers.len());

    for (idx, worker) in workers.iter().enumerate() {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        worker_cmd_txs.push(cmd_tx);
        let (conn_tx, conn_rx) = watch::channel(false);
        conn_rxs.push(conn_rx);
        tokio::spawn(worker_mgr(
            idx as WorkerId,
            my_id.clone(),
            worker.clone(),
            job_tracker.clone(),
            conn_tx,
            connector.clone(),
            connection_state.clone(),
            conn_cmd_tx.clone(),
            cmd_rx,
            worker_rsp_tx.clone(),
            shutdown_ct.clone(),
        ));
    }

    let (job_tx, job_rx) = mpsc::unbounded_channel();
    tokio::spawn(leader_task(
        job_tracker,
        job_rx,
        worker_rsp_rx,
        worker_cmd_txs,
        shutdown_ct,
    ));

    (job_tx, conn_rxs)
}

async fn leader_task(
    job_tracker: Arc<JobTracker>,
    mut job_rx: UnboundedReceiver<Job>,
    mut worker_rsp_rx: UnboundedReceiver<WorkerRsp>,
    worker_cmd_ch: Vec<UnboundedSender<NetworkValue>>,
    shutdown_ct: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown_ct.cancelled() => {
                tracing::info!("Leader task shutting down");
                break;
            },
            job_opt = job_rx.recv() => {
                if let Some(job) = job_opt {
                    send_to_workpool(job, &worker_cmd_ch, &job_tracker);
                } else {
                    tracing::info!("Job channel closed, shutting down leader task");
                    break;
                }
            },
            rsp_opt = worker_rsp_rx.recv() => {
                if let Some(rsp) = rsp_opt {
                    if let Err(e) = job_tracker.record_response(rsp) {
                        tracing::warn!("handle_worker_response: {}", e);
                    }
                } else {
                    tracing::debug!("Worker event channel closed");
                }
            },
        };
    }
}

fn send_to_workpool(
    job: Job,
    worker_cmd_ch: &[UnboundedSender<NetworkValue>],
    job_tracker: &JobTracker,
) {
    match job {
        Job::Broadcast {
            job_id,
            payload,
            result_rsp,
        } => {
            let num_workers = worker_cmd_ch.len() as WorkerId;
            job_tracker.register_job(job_id, JobType::Broadcast { num_workers }, result_rsp);

            for (worker_id, cmd_tx) in worker_cmd_ch.iter().enumerate() {
                let _ = cmd_tx.send(NetworkValue::new_job(
                    job_id,
                    worker_id as WorkerId,
                    payload.clone(),
                ));
            }
        }
        Job::ScatterGather {
            job_id,
            msgs,
            result_rsp,
        } => {
            // Collect worker IDs (validation already done in LeaderHandle)
            let worker_ids: HashSet<WorkerId> = msgs.iter().map(|msg| msg.worker_id).collect();
            job_tracker.register_job(job_id, JobType::ScatterGather { worker_ids }, result_rsp);

            for msg in msgs.into_iter() {
                let Some(cmd_tx) = worker_cmd_ch.get(msg.worker_id as usize) else {
                    tracing::warn!("invalid worker id: {}", msg.worker_id);
                    return;
                };
                let _ = cmd_tx.send(NetworkValue::new_job(job_id, msg.worker_id, msg.payload));
            }
        }
        Job::Cancel { job_id } => {
            // currently do nothing else when cancelling a job. The worker
            // doesn't have a way to skip the work yet.
            if job_tracker.cancel_job(job_id).is_err() {
                tracing::warn!("failed to cancel job");
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn worker_mgr<T: NetworkConnection + 'static, C: Client<Output = T> + 'static>(
    worker_id: WorkerId,
    my_id: Arc<Identity>,
    worker: Arc<Peer>,
    job_tracker: Arc<JobTracker>,
    conn_tx: watch::Sender<bool>,
    connector: C,
    connection_state: ConnectionState,
    conn_cmd_tx: UnboundedSender<ConnectionRequest<T>>,
    mut cmd_rx: UnboundedReceiver<NetworkValue>,
    worker_rsp_tx: UnboundedSender<WorkerRsp>,
    shutdown_ct: CancellationToken,
) {
    loop {
        let connection_id = ConnectionId::new(0); // workers always connect with id 0
        let mut conn = match crate::network::tcp::connect(
            connection_id,
            my_id.clone(),
            worker.clone(),
            connection_state.clone(),
            connector.clone(),
            conn_cmd_tx.clone(),
        )
        .await
        {
            Ok(conn) => {
                tracing::info!("Connected to worker {}", worker_id);
                conn
            }
            Err(e) => {
                tracing::error!("Failed to connect to worker {}: {:?}", worker_id, e);
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        match get_last_rxd_job_id(&mut conn, worker_id).await {
            Ok(opt) => {
                job_tracker.check_for_dropped_jobs(worker_id, opt);
                let _ = conn_tx.send(true);
            }
            Err(e) => {
                tracing::error!("Failed to check for dropped jobs: {}", e);
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        }

        let (read_half, write_half) = tokio::io::split(conn);
        let reader = BufReader::new(read_half);

        enum Evt {
            OutboundClosed,
            InboundClosed,
            Shutdown,
        }

        let evt = tokio::select! {
            r = serialize_and_write_outbound(write_half, &mut cmd_rx, |_| {}) => {
                if let Err(e) = r {
                    tracing::warn!("Worker {} outbound traffic error: {:?}", worker_id, e);
                }
                Evt::OutboundClosed
            },
            r = read_and_parse_inbound(reader, &worker_rsp_tx, convert_to_worker_rsp) => {
                if let Err(e) = r {
                    tracing::warn!("Worker {} inbound traffic error: {:?}", worker_id, e);
                }
                Evt::InboundClosed
            },
            _ = shutdown_ct.cancelled() => {
                Evt::Shutdown
            }
        };

        match evt {
            Evt::OutboundClosed | Evt::InboundClosed => {
                let _ = conn_tx.send(false);
                tracing::info!("Worker {} connection closed, reconnecting...", worker_id);
                sleep(Duration::from_secs(1)).await;
            }
            Evt::Shutdown => {
                let _ = conn_tx.send(false);
                tracing::info!("Worker {} task shutting down", worker_id);
                break;
            }
        }
    }
}

// returns the last received job id
async fn get_last_rxd_job_id<T: NetworkConnection>(
    conn: &mut T,
    worker_id: WorkerId,
) -> Result<Option<JobId>, LeaderError> {
    use bytes::BytesMut;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let query = NetworkValue::QueryJobState { worker_id };
    let mut buf = BytesMut::with_capacity(query.byte_len());
    query.serialize(&mut buf);
    conn.write_all(&buf).await?;
    conn.flush().await?;

    // Read JobStateResponse: fixed 12 bytes
    // descriptor (1) + worker_id (2) + bitfield (1) + last_received (4) + last_responded (4)
    let mut response_buf = [0u8; 12];
    conn.read_exact(&mut response_buf).await?;

    let response = NetworkValue::deserialize(Bytes::copy_from_slice(&response_buf))
        .map_err(|e| io::Error::other(format!("Deserialize error: {}", e)))?;

    if let NetworkValue::JobStateResponse {
        last_received_job_id,
        ..
    } = response
    {
        Ok(last_received_job_id)
    } else {
        Err(LeaderError::BadResponse("Expected JobStateResponse".into()))
    }
}

fn convert_to_worker_rsp(
    network_value: NetworkValue,
    tx: &UnboundedSender<WorkerRsp>,
) -> io::Result<()> {
    match network_value {
        NetworkValue::Job {
            job_id,
            worker_id,
            payload,
        } => {
            let msg = WorkerRsp {
                worker_id,
                payload: Ok(payload),
                job_id,
            };
            tx.send(msg)
                .map_err(|_| io::Error::other("Failed to send worker response"))
        }
        NetworkValue::JobStateResponse { .. } => {
            // Should not receive during normal traffic (only during reconciliation)
            tracing::warn!("Unexpected JobStateResponse during normal traffic");
            Ok(())
        }
        NetworkValue::Cancel { .. } => {
            // Should not receive Cancel from worker
            tracing::warn!("Unexpected Cancel from worker");
            Ok(())
        }
        NetworkValue::QueryJobState { .. } => Err(io::Error::other(
            "Unexpected QueryJobState on leader connection",
        )),
    }
}
