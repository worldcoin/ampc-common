use eyre::Result;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::{
    execution::player::Identity,
    network::tcp::{
        accept_loop, Client, ConnectionId, ConnectionRequest, ConnectionState, NetworkConnection,
        Peer, Server,
    },
};

use super::{
    job_tracker::{JobTracker, JobType},
    Job, LeaderError, WorkerRsp,
};
use crate::network::workpool::{
    handle_inbound_traffic, handle_outbound_traffic, value::NetworkValue, NetworkFailure,
};

pub fn spawn<T, C, I, S>(
    my_id: Identity,
    worker_urls: I,
    connector: C,
    listener: S,
    shutdown_ct: CancellationToken,
) -> Result<UnboundedSender<Job>, LeaderError>
where
    T: NetworkConnection + 'static,
    C: Client<Output = T> + 'static,
    I: Iterator<Item = String>,
    S: Server<Output = T> + 'static,
{
    let my_id = Arc::new(my_id);
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

    for (idx, worker) in workers.iter().enumerate() {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        worker_cmd_txs.push(cmd_tx);
        tokio::spawn(worker_mgr(
            idx as u16,
            my_id.clone(),
            worker.clone(),
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
        job_rx,
        worker_rsp_rx,
        worker_cmd_txs,
        shutdown_ct,
    ));

    Ok(job_tx)
}

async fn leader_task(
    mut job_rx: UnboundedReceiver<Job>,
    mut worker_rsp_rx: UnboundedReceiver<WorkerRsp>,
    worker_cmd_ch: Vec<UnboundedSender<NetworkValue>>,
    shutdown_ct: CancellationToken,
) {
    let job_tracker = JobTracker::new();

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
                    handle_worker_response(rsp, &job_tracker);
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
        Job::Broadcast { payload, rsp } => {
            let num_workers = worker_cmd_ch.len() as u16;
            let job_id = job_tracker.register_job(JobType::Broadcast { num_workers }, rsp);

            for (worker_id, cmd_tx) in worker_cmd_ch.iter().enumerate() {
                let _ = cmd_tx.send(NetworkValue::new_request(
                    job_id,
                    worker_id as _,
                    payload.clone(),
                ));
            }
        }
        Job::ScatterGather { msgs, rsp } => {
            let num_partitions = worker_cmd_ch.len() as u16;
            let job_id = job_tracker.register_job(JobType::ScatterGather { num_partitions }, rsp);

            for msg in msgs.into_iter() {
                if let Some(cmd_tx) = worker_cmd_ch.get(msg.worker_id as usize) {
                    let _ = cmd_tx.send(NetworkValue::new_request(
                        job_id,
                        msg.worker_id as _,
                        msg.payload,
                    ));
                } else {
                    tracing::warn!("invalid worker id: {}", msg.worker_id);
                }
            }
        }
    }
}

fn handle_worker_response(rsp: WorkerRsp, job_tracker: &JobTracker) {
    if let Err(e) = job_tracker.record_response(rsp) {
        tracing::error!("Failed to record worker response: {}", e);
    }
}

async fn worker_mgr<T: NetworkConnection + 'static, C: Client<Output = T> + 'static>(
    worker_id: u16,
    my_id: Arc<Identity>,
    worker: Arc<Peer>,
    connector: C,
    connection_state: ConnectionState,
    conn_cmd_tx: UnboundedSender<ConnectionRequest<T>>,
    mut cmd_rx: UnboundedReceiver<NetworkValue>,
    worker_rsp_tx: UnboundedSender<WorkerRsp>,
    shutdown_ct: CancellationToken,
) {
    let mut last_sent_job_id: Option<u32> = None;

    loop {
        let connection_id = ConnectionId::new(worker_id as _);
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

        if let Err(e) = detect_dropped_jobs(&mut conn, worker_id, last_sent_job_id).await {
            tracing::error!("Worker {} reconciliation failed: {:?}", worker_id, e);
            todo!("emit error for every job being tracked");
        }

        let (read_half, write_half) = tokio::io::split(conn);
        let reader = BufReader::new(read_half);

        enum Evt {
            OutboundClosed,
            InboundClosed,
            Shutdown,
        }

        let evt = tokio::select! {
            r = handle_outbound_traffic(write_half, &mut cmd_rx, |msg| {
                if let NetworkValue::Request { job_id, .. } = msg {
                    last_sent_job_id = Some(*job_id);
                }
            }) => {
                if let Err(e) = r {
                    tracing::warn!("Worker {} outbound traffic error: {:?}", worker_id, e);
                }
                Evt::OutboundClosed
            },
            r = handle_inbound_traffic(reader, &worker_rsp_tx, convert_to_worker_rsp) => {
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
                tracing::info!("Worker {} connection closed, reconnecting...", worker_id);
                sleep(Duration::from_secs(1)).await;
            }
            Evt::Shutdown => {
                tracing::info!("Worker {} task shutting down", worker_id);
                break;
            }
        }
    }
}

async fn detect_dropped_jobs<T: NetworkConnection>(
    conn: &mut T,
    worker_id: u16,
    expected_last_sent: Option<u32>,
) -> io::Result<()> {
    use bytes::BytesMut;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let query = NetworkValue::QueryJobState { worker_id };
    let mut buf = BytesMut::with_capacity(query.byte_len());
    query.serialize(&mut buf);
    conn.write_all(&buf).await?;
    conn.flush().await?;

    // Read JobStateResponse (13 bytes)
    let mut response_buf = vec![0u8; 13];
    conn.read_exact(&mut response_buf).await?;

    let response = NetworkValue::deserialize(&response_buf)
        .map_err(|e| io::Error::other(format!("Deserialize error: {}", e)))?;

    if let NetworkValue::JobStateResponse {
        last_received_job_id,
        ..
    } = response
    {
        if expected_last_sent != last_received_job_id {
            tracing::error!(
                "Worker {} job mismatch: expected last sent {:?}, worker reports last received {:?}",
                worker_id,
                expected_last_sent,
                last_received_job_id
            );
            let failure = NetworkFailure::JobsLost {
                worker_id,
                expected: expected_last_sent.unwrap_or(0),
                actual: last_received_job_id,
            };
            tracing::error!("Network failure detected: {}", failure);
            todo!("send error via channel");
        }
    } else {
        return Err(io::Error::other("Expected JobStateResponse"));
    }

    Ok(())
}

fn convert_to_worker_rsp(
    network_value: NetworkValue,
    tx: &UnboundedSender<WorkerRsp>,
) -> io::Result<()> {
    match network_value {
        NetworkValue::Response {
            job_id,
            worker_id,
            payload,
        } => {
            let msg = WorkerRsp {
                worker_id,
                payload,
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
        NetworkValue::Request { .. } | NetworkValue::QueryJobState { .. } => Err(io::Error::other(
            "Unexpected Request/QueryJobState on leader connection",
        )),
    }
}
