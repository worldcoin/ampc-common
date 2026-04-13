use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::sync::mpsc::{self, Receiver, Sender, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::network::workpool::Error;
use crate::network::workpool::{
    leader::job_tracker::{JobTracker, JobType},
    leader::{Job, WorkerRsp},
    WorkpoolError,
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
) -> (Sender<Job>, Vec<watch::Receiver<bool>>)
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
    let (job_sent_tx, job_sent_rx) = mpsc::unbounded_channel();
    let mut workers_connected = Vec::with_capacity(workers.len());

    for (idx, worker) in workers.iter().enumerate() {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        worker_cmd_txs.push(cmd_tx);
        let (worker_conn_tx, worker_conn_rx) = watch::channel(false);
        workers_connected.push(worker_conn_rx);
        tokio::spawn(worker_mgr(
            idx as WorkerId,
            my_id.clone(),
            worker.clone(),
            job_tracker.clone(),
            worker_conn_tx,
            connector.clone(),
            connection_state.clone(),
            conn_cmd_tx.clone(),
            cmd_rx,
            job_sent_tx.clone(),
            worker_rsp_tx.clone(),
            shutdown_ct.clone(),
        ));
    }

    let (job_tx, job_rx) = mpsc::channel(1024);
    tokio::spawn(leader_task(
        job_tracker,
        job_rx,
        worker_rsp_rx,
        job_sent_rx,
        worker_cmd_txs,
        worker_rsp_tx,
        shutdown_ct,
    ));

    (job_tx, workers_connected)
}

async fn leader_task(
    job_tracker: Arc<JobTracker>,
    mut job_rx: Receiver<Job>,
    mut worker_rsp_rx: UnboundedReceiver<WorkerRsp>,
    mut job_sent_rx: UnboundedReceiver<(JobId, WorkerId)>,
    worker_cmd_ch: Vec<UnboundedSender<NetworkValue>>,
    worker_rsp_tx: UnboundedSender<WorkerRsp>,
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
                    send_to_workpool(job, &worker_cmd_ch, &job_tracker, &worker_rsp_tx);
                } else {
                    tracing::info!("Job channel closed, shutting down leader task");
                    break;
                }
            },
            job_opt = job_sent_rx.recv() => {
                if let Some((job_id, worker_id)) = job_opt {
                    if let Err(e) = job_tracker.set_to_pending(job_id, worker_id) {
                        tracing::warn!("set_to_pending: {}", e);
                    }
                } else {
                    tracing::warn!("job_sent_rx was closed. stopping leader_task");
                    break;
                }
            },
            rsp_opt = worker_rsp_rx.recv() => {
                if let Some(rsp) = rsp_opt {
                    if let Err(e) = job_tracker.record_response(rsp) {
                        tracing::warn!("record_response: {}", e);
                    }
                } else {
                    // the leader holds worker_rsp_tx as well
                    unreachable!("channel should not return None while leader is alive");
                }
            },
        };
    }
}

fn send_to_workpool(
    job: Job,
    worker_cmd_ch: &[UnboundedSender<NetworkValue>],
    job_tracker: &JobTracker,
    worker_rsp_tx: &UnboundedSender<WorkerRsp>,
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
                let worker_id = worker_id as WorkerId;
                if cmd_tx
                    .send(NetworkValue::new_job(job_id, worker_id, payload.clone()))
                    .is_err()
                {
                    let _ = worker_rsp_tx.send(WorkerRsp {
                        job_id,
                        worker_id,
                        payload: Err(WorkpoolError::SendFailed),
                    });
                }
            }
        }
        Job::ScatterGather {
            job_id,
            msgs,
            result_rsp,
        } => {
            // Validate all worker IDs before registering the job to avoid orphaned jobs
            for msg in &msgs {
                if worker_cmd_ch.get(msg.worker_id as usize).is_none() {
                    tracing::warn!("invalid worker id: {}, not registering job", msg.worker_id);
                    let _ = result_rsp.send(Err(WorkpoolError::InvalidInput(format!(
                        "invalid worker id: {}",
                        msg.worker_id
                    ))));
                    return;
                }
            }

            // Collect worker IDs (validation already done in LeaderHandle and in the above loop)
            let worker_ids: HashSet<WorkerId> = msgs.iter().map(|msg| msg.worker_id).collect();
            job_tracker.register_job(job_id, JobType::ScatterGather { worker_ids }, result_rsp);

            for msg in msgs.into_iter() {
                // Safe to unwrap: we validated all worker_ids above
                let cmd_tx = &worker_cmd_ch[msg.worker_id as usize];
                if cmd_tx
                    .send(NetworkValue::new_job(job_id, msg.worker_id, msg.payload))
                    .is_err()
                {
                    let _ = worker_rsp_tx.send(WorkerRsp {
                        job_id,
                        worker_id: msg.worker_id,
                        payload: Err(WorkpoolError::SendFailed),
                    });
                }
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
    worker_conn_tx: watch::Sender<bool>,
    connector: C,
    connection_state: ConnectionState,
    conn_cmd_tx: UnboundedSender<ConnectionRequest<T>>,
    mut cmd_rx: UnboundedReceiver<NetworkValue>,
    job_sent_tx: UnboundedSender<(JobId, WorkerId)>,
    worker_rsp_tx: UnboundedSender<WorkerRsp>,
    shutdown_ct: CancellationToken,
) {
    // note that sleeping before continuing this loop is redundant.
    // tcp::connect() already does that
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
                if connection_state.shutdown_ct().is_cancelled() {
                    tracing::info!("manager task for worker {} is shutting down", worker_id);
                    break;
                } else {
                    tracing::error!("Failed to connect to worker {}: {:?}", worker_id, e);
                    continue;
                }
            }
        };

        let maybe_pending = tokio::select! {
            r = leader_handshake(&mut conn, worker_id) => r,
            _ = shutdown_ct.cancelled() => {
                let _ = worker_conn_tx.send(false);
                tracing::info!("manager task for worker {} is shutting down", worker_id);
                break;
            }
        };

        match maybe_pending {
            Ok(pending_jobs) => {
                job_tracker.validate_pending_jobs(worker_id, pending_jobs);
                let _ = worker_conn_tx.send(true);
            }
            Err(e) => {
                tracing::error!(
                    "Failed to get pending jobs from worker {}: {}",
                    worker_id,
                    e
                );
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

        // if the job tracker marks something as sent but then the connection is interrupted before
        // serialize_and_write_outbound actually sends the job, the job tracker will report a job
        // as lost.
        let evt = tokio::select! {
            r = serialize_and_write_outbound(write_half, &mut cmd_rx, |nv| {
                if let NetworkValue::Job { job_id, worker_id, ..} = &nv {
                    let _ = job_sent_tx.send((*job_id, *worker_id));
                }
            }) => {
                if let Err(e) = r {
                    tracing::warn!("Worker {} outbound traffic error: {:?}", worker_id, e);
                }
                Evt::OutboundClosed
            },
            r = read_and_parse_inbound(reader, &worker_rsp_tx, leader_forward_inbound_msg) => {
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
                let _ = worker_conn_tx.send(false);
                tracing::info!("Worker {} connection closed, reconnecting...", worker_id);
                sleep(Duration::from_secs(1)).await;
            }
            Evt::Shutdown => {
                let _ = worker_conn_tx.send(false);
                tracing::info!("manager task for worker {} is shutting down", worker_id);
                break;
            }
        }
    }
}

// returns a list of job ids which the worker has received but has
// not finished processing yet. also known as pending jobs
async fn leader_handshake<T: NetworkConnection>(
    conn: &mut T,
    worker_id: WorkerId,
) -> Result<Vec<JobId>, Error> {
    use crate::network::workpool::read_single_message;
    use bytes::BytesMut;
    use tokio::io::AsyncWriteExt;

    let query = NetworkValue::PendingJobsRequest { worker_id };
    let mut buf = BytesMut::with_capacity(query.byte_len());
    query.serialize(&mut buf);
    conn.write_all(&buf).await?;
    conn.flush().await?;

    match read_single_message(conn).await? {
        NetworkValue::PendingJobsReply { job_ids, .. } => Ok(job_ids),
        other => Err(Error::HandshakeFailed(format!(
            "expected PendingJobsReply, got {:?}",
            other
        ))),
    }
}

fn leader_forward_inbound_msg(
    network_value: NetworkValue,
    tx: &UnboundedSender<WorkerRsp>,
) -> Result<(), Error> {
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
            tx.send(msg).map_err(|_| Error::SendFailed)
        }
        NetworkValue::PendingJobsReply { .. } => {
            tracing::warn!("received PendingJobsReply after handshake");
            Ok(())
        }
        NetworkValue::Cancel { .. } => {
            tracing::warn!("received cancel from worker");
            Ok(())
        }
        NetworkValue::PendingJobsRequest { .. } => Err(Error::InvalidInput(
            "received PendingJobsRequest from worker".into(),
        )),
    }
}
