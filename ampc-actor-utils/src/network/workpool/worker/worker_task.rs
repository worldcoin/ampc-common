use bytes::BytesMut;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use super::{Job, Msg};
use crate::network::workpool::value::NetworkValue;
use crate::network::workpool::{
    read_and_parse_inbound, serialize_and_write_outbound, Error, JobId,
};
use crate::{
    execution::player::Identity,
    network::tcp::{
        Client, ConnectionConfig, ConnectionId, ConnectionState, NetworkConnection, Peer,
    },
};

pub fn spawn<T, C>(
    my_id: Identity,
    leader: Peer,
    client: C,
    shutdown_ct: CancellationToken,
) -> UnboundedReceiver<Job>
where
    T: NetworkConnection + 'static,
    C: Client<Output = T> + 'static,
{
    let my_id = Arc::new(my_id);
    let leader = Arc::new(leader);
    let shutdown_ct = shutdown_ct.child_token();
    let connection_state = ConnectionState::new(shutdown_ct.clone(), CancellationToken::new());

    let (job_tx, job_rx) = mpsc::unbounded_channel::<Job>();
    tokio::spawn(worker_task(
        my_id,
        leader,
        client,
        connection_state,
        job_tx,
        shutdown_ct,
    ));
    job_rx
}

async fn worker_task<T: NetworkConnection + 'static, C: Client<Output = T> + 'static>(
    my_id: Arc<Identity>,
    leader: Arc<Peer>,
    client: C,
    connection_state: ConnectionState,
    job_tx: UnboundedSender<Job>,
    shutdown_ct: CancellationToken,
) {
    let client = Arc::new(client);
    let pending_jobs = Arc::new(Mutex::new(HashSet::<JobId>::new()));

    // This channel allows the workpool worker to send responses back to the
    // leader. It lives outside the connection loop so that responses survive
    // reconnects - if a job completes while disconnected, the response is
    // queued and will be written when the connection is re-established.
    let (rsp_tx, mut rsp_rx) = mpsc::unbounded_channel::<NetworkValue>();

    // note that sleeping before continuing this loop is redundant.
    // tcp::connect() already does that
    loop {
        let connection_id = ConnectionId::new(0); // Worker only has one connection
        let mut conn = match crate::network::tcp::connect(
            connection_id,
            my_id.clone(),
            connection_state.clone(),
            ConnectionConfig::Client {
                peer: leader.clone(),
                client: client.clone(),
            },
        )
        .await
        {
            Ok(conn) => {
                tracing::info!("Connected to leader");
                conn
            }
            Err(e) => {
                if connection_state.shutdown_ct().await.is_cancelled() {
                    tracing::info!("Worker {} task is shutting down", my_id.0);
                    break;
                } else {
                    tracing::error!("Failed to connect to leader: {:?}", e);
                    continue;
                }
            }
        };

        // Complete handshake before flushing any queued responses.
        // This ensures the leader gets an accurate view of pending jobs before
        // we start sending responses that would remove jobs from pending_jobs.
        let pending_job_ids: Vec<JobId> = pending_jobs.lock().unwrap().iter().cloned().collect();
        let res = tokio::select! {
            r = worker_handshake(&mut conn, pending_job_ids) => r,
            _ = shutdown_ct.cancelled() => {
                tracing::info!("Worker {} task shutting down during handshake", my_id.0);
                break;
            }
        };

        if let Err(e) = res {
            tracing::error!("Worker {} error: {:?}", my_id.0, e);
            continue;
        };

        // Proceed with normal traffic
        let (read_half, write_half) = tokio::io::split(conn);
        let reader = BufReader::new(read_half);

        enum Evt {
            OutboundClosed,
            InboundClosed,
            Shutdown,
        }

        let evt = tokio::select! {
            r = serialize_and_write_outbound(write_half, &mut rsp_rx, {
                let pending = pending_jobs.clone();
                move |msg| {
                    if let NetworkValue::Job { job_id, .. } = msg {
                        pending.lock().unwrap().remove(job_id);
                    }
                }
            }) => {
                if let Err(e) = r {
                    tracing::warn!("Worker {} outbound traffic error: {:?}", my_id.0, e);
                }
                Evt::OutboundClosed
            },
            r = read_and_parse_inbound(reader, &job_tx, {
                let pending = pending_jobs.clone();
                let rsp = rsp_tx.clone();
                move |network_value, job_tx| {
                    worker_handle_inbound_msg(
                        network_value,
                        job_tx,
                        rsp.clone(),
                        &pending
                    )
                }
            }) => {
                if let Err(e) = r {
                    tracing::warn!("Worker {} inbound traffic error: {:?}", my_id.0, e);
                }
                Evt::InboundClosed
            },
            _ = shutdown_ct.cancelled() => {
                Evt::Shutdown
            }
        };

        match evt {
            Evt::OutboundClosed | Evt::InboundClosed => {
                tracing::info!("Worker {} connection closed, reconnecting...", my_id.0);
                sleep(Duration::from_secs(1)).await;
            }
            Evt::Shutdown => {
                tracing::info!("Worker {} task shutting down", my_id.0);
                break;
            }
        }
    }
}

/// Wait for PendingJobsRequest and send PendingJobsReply before starting normal traffic.
/// This ensures the leader gets an accurate snapshot of pending jobs before we start
/// sending queued responses (which would remove jobs from pending_jobs).
async fn worker_handshake<T: NetworkConnection>(
    conn: &mut T,
    pending_job_ids: Vec<JobId>,
) -> Result<(), Error> {
    use crate::network::workpool::read_single_message;

    let worker_id = match read_single_message(conn).await? {
        NetworkValue::PendingJobsRequest { worker_id } => worker_id,
        other => {
            return Err(Error::HandshakeFailed(format!(
                "expected PendingJobsRequest, got {:?}",
                other
            )))
        }
    };

    let reply = NetworkValue::PendingJobsReply {
        worker_id,
        job_ids: pending_job_ids,
    };
    let mut buf = BytesMut::with_capacity(reply.byte_len());
    reply.serialize(&mut buf);
    conn.write_all(&buf).await?;
    conn.flush().await?;
    Ok(())
}

fn worker_handle_inbound_msg(
    network_value: NetworkValue,
    tx: &UnboundedSender<Job>,
    rsp_tx: UnboundedSender<NetworkValue>,
    pending_jobs: &Mutex<HashSet<JobId>>,
) -> Result<(), Error> {
    match network_value {
        NetworkValue::Job {
            job_id,
            worker_id,
            payload,
        } => {
            pending_jobs.lock().unwrap().insert(job_id);
            let msg = Msg {
                job_id,
                worker_id,
                payload,
            };
            let job = Job { msg, rsp: rsp_tx };
            tx.send(job).map_err(|_| Error::SendFailed)?;

            Ok(())
        }
        NetworkValue::PendingJobsRequest { worker_id } => {
            // PendingJobsRequest should only be received during handshake phase.
            // If we get it here, either the leader sent a duplicate request or
            // there's a protocol mismatch. Respond anyway to be resilient.
            tracing::warn!(
                "Received PendingJobsRequest for worker {} after handshake",
                worker_id
            );
            let pending = pending_jobs.lock().unwrap().iter().cloned().collect();
            let response = NetworkValue::PendingJobsReply {
                worker_id,
                job_ids: pending,
            };
            rsp_tx.send(response).map_err(|_| Error::SendFailed)
        }
        NetworkValue::Cancel { job_id, worker_id } => {
            tracing::info!(
                "Received cancellation for job {} on worker {}",
                job_id,
                worker_id
            );
            Ok(())
        }
        NetworkValue::PendingJobsReply { .. } => Err(Error::InvalidInput(
            "Received PendingJobsReply from leader".into(),
        )),
    }
}
