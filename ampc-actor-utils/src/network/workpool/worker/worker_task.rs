use bytes::{Bytes, BytesMut};
use std::collections::HashSet;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use super::{Job, Msg};
use crate::network::workpool::value::{DescriptorByte, NetworkValue};
use crate::network::workpool::{read_and_parse_inbound, serialize_and_write_outbound, JobId};
use crate::{
    execution::player::Identity,
    network::tcp::{
        accept_loop, Client, ConnectionId, ConnectionRequest, ConnectionState, NetworkConnection,
        Peer, Server,
    },
};

pub fn spawn<T, C, S>(
    my_id: Identity,
    leader: Peer,
    connector: C,
    listener: S,
    shutdown_ct: CancellationToken,
) -> UnboundedReceiver<Job>
where
    T: NetworkConnection + 'static,
    C: Client<Output = T> + 'static,
    S: Server<Output = T> + 'static,
{
    let my_id = Arc::new(my_id);
    let leader = Arc::new(leader);
    let shutdown_ct = shutdown_ct.child_token();
    let connection_state = ConnectionState::new(shutdown_ct.clone(), CancellationToken::new());

    let (conn_cmd_tx, conn_cmd_rx) = mpsc::unbounded_channel::<ConnectionRequest<T>>();
    tokio::spawn(accept_loop(listener, conn_cmd_rx, shutdown_ct.clone()));

    let (job_tx, job_rx) = mpsc::unbounded_channel::<Job>();
    tokio::spawn(worker_task(
        my_id,
        leader,
        connector,
        connection_state,
        conn_cmd_tx,
        job_tx,
        shutdown_ct,
    ));
    job_rx
}

async fn worker_task<T: NetworkConnection + 'static, C: Client<Output = T> + 'static>(
    my_id: Arc<Identity>,
    leader: Arc<Peer>,
    connector: C,
    connection_state: ConnectionState,
    conn_cmd_tx: UnboundedSender<ConnectionRequest<T>>,
    job_tx: UnboundedSender<Job>,
    shutdown_ct: CancellationToken,
) {
    let pending_jobs = Arc::new(Mutex::new(HashSet::<JobId>::new()));

    // This channel allows the workpool worker to send responses back to the
    // leader. It lives outside the connection loop so that responses survive
    // reconnects - if a job completes while disconnected, the response is
    // queued and will be written when the connection is re-established.
    let (rsp_tx, mut rsp_rx) = mpsc::unbounded_channel::<NetworkValue>();

    loop {
        let connection_id = ConnectionId::new(0); // Worker only has one connection
        let mut conn = match crate::network::tcp::connect(
            connection_id,
            my_id.clone(),
            leader.clone(),
            connection_state.clone(),
            connector.clone(),
            conn_cmd_tx.clone(),
        )
        .await
        {
            Ok(conn) => {
                tracing::info!("Worker connected to leader");
                conn
            }
            Err(e) => {
                if connection_state.shutdown_ct().await.is_cancelled() {
                    tracing::info!("Worker {} task shutting down", my_id.0);
                    break;
                } else {
                    tracing::error!("Failed to connect to leader: {:?}", e);
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
        };

        // Complete handshake before flushing any queued responses.
        // This ensures the leader gets an accurate view of pending jobs before
        // we start sending responses that would remove jobs from pending_jobs.
        let res = tokio::select! {
            r = complete_handshake(&mut conn, &pending_jobs) => r,
            _ = shutdown_ct.cancelled() => {
                tracing::info!("Worker {} task shutting down during handshake", my_id.0);
                break;
            }
        };

        if let Err(e) = res {
            tracing::warn!("Worker {} handshake failed: {:?}", my_id.0, e);
            sleep(Duration::from_secs(1)).await;
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
                    handle_inbound_msg(
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
async fn complete_handshake<T: NetworkConnection>(
    conn: &mut T,
    pending_jobs: &Mutex<HashSet<JobId>>,
) -> io::Result<()> {
    // Read until we get PendingJobsRequest
    loop {
        // Read descriptor byte
        let mut header_buf = vec![0u8; 1];
        conn.read_exact(&mut header_buf).await?;
        let descriptor: DescriptorByte = header_buf[0]
            .try_into()
            .map_err(|_| io::Error::other(format!("Invalid descriptor byte: {}", header_buf[0])))?;

        // Read remaining header bytes
        let header_len = descriptor.header_len();
        header_buf.resize(header_len, 0);
        conn.read_exact(&mut header_buf[1..header_len]).await?;

        // For Job messages, read the variable-length payload
        if descriptor == DescriptorByte::Job {
            let payload_len = u32::from_le_bytes(header_buf[7..11].try_into().unwrap()) as usize;
            header_buf.resize(header_len + payload_len, 0);
            conn.read_exact(&mut header_buf[header_len..]).await?;
        }

        let network_value = NetworkValue::deserialize(Bytes::from(header_buf))
            .map_err(|e| io::Error::other(format!("Deserialize error: {}", e)))?;

        match network_value {
            NetworkValue::PendingJobsRequest { worker_id } => {
                // Take snapshot of pending jobs and send reply
                let pending: Vec<JobId> = pending_jobs.lock().unwrap().iter().cloned().collect();
                let reply = NetworkValue::PendingJobsReply {
                    worker_id,
                    job_ids: pending,
                };
                let mut buf = BytesMut::with_capacity(reply.byte_len());
                reply.serialize(&mut buf);
                conn.write_all(&buf).await?;
                conn.flush().await?;
                return Ok(());
            }
            NetworkValue::Job { .. } => {
                // Leader shouldn't send jobs before handshake completes
                return Err(io::Error::other("Received Job before handshake completed"));
            }
            other => {
                tracing::warn!("Unexpected message during handshake: {:?}", other);
            }
        }
    }
}

fn handle_inbound_msg(
    network_value: NetworkValue,
    tx: &UnboundedSender<Job>,
    rsp_tx: UnboundedSender<NetworkValue>,
    pending_jobs: &Mutex<HashSet<JobId>>,
) -> io::Result<()> {
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
            tx.send(job)
                .map_err(|_| io::Error::other("Failed to send job"))?;

            Ok(())
        }
        NetworkValue::PendingJobsRequest { worker_id } => {
            // PendingJobsRequest should only be received during handshake phase.
            // If we get it here, either the leader sent a duplicate request or
            // there's a protocol mismatch. Respond anyway to be resilient.
            tracing::warn!(
                "Received PendingJobsRequest for worker {} during normal traffic (expected only during handshake)",
                worker_id
            );
            let pending = pending_jobs.lock().unwrap().iter().cloned().collect();
            let response = NetworkValue::PendingJobsReply {
                worker_id,
                job_ids: pending,
            };
            rsp_tx
                .send(response)
                .map_err(|_| io::Error::other("Failed to send job state response"))
        }
        NetworkValue::Cancel { job_id, worker_id } => {
            tracing::info!(
                "Received cancellation for job {} on worker {}",
                job_id,
                worker_id
            );
            Ok(())
        }
        NetworkValue::PendingJobsReply { .. } => Err(io::Error::other(
            "Unexpected JobStateResponse on worker connection",
        )),
    }
}
