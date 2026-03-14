use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::BufReader;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use super::{Job, Msg};
use crate::network::workpool::value::NetworkValue;
use crate::network::workpool::{handle_inbound_traffic, handle_outbound_traffic};
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
    let last_received_job_id = Arc::new(Mutex::new(None));
    let last_responded_job_id = Arc::new(Mutex::new(None));

    loop {
        let connection_id = ConnectionId::new(0); // Worker only has one connection
        let conn = match crate::network::tcp::connect(
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
                tracing::error!("Failed to connect to leader: {:?}", e);
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        let (rsp_tx, mut rsp_rx) = mpsc::unbounded_channel::<NetworkValue>();
        let (read_half, write_half) = tokio::io::split(conn);
        let reader = BufReader::new(read_half);

        enum Evt {
            OutboundClosed,
            InboundClosed,
            Shutdown,
        }

        let evt = tokio::select! {
            r = handle_outbound_traffic(write_half, &mut rsp_rx, {
                let last_responded = last_responded_job_id.clone();
                move |msg| {
                    if let NetworkValue::Response { job_id, .. } = msg {
                        *last_responded.lock().unwrap() = Some(*job_id);
                    }
                }
            }) => {
                if let Err(e) = r {
                    tracing::warn!("Worker {} outbound traffic error: {:?}", my_id.0, e);
                }
                Evt::OutboundClosed
            },
            r = handle_inbound_traffic(reader, &job_tx, {
                let last_received = last_received_job_id.clone();
                let last_responded = last_responded_job_id.clone();
                let rsp_tx_clone = rsp_tx.clone();
                move |network_value, job_tx| {
                    convert_to_job(
                        network_value,
                        job_tx,
                        rsp_tx_clone.clone(),
                        &last_received,
                        &last_responded,
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

fn convert_to_job(
    network_value: NetworkValue,
    tx: &UnboundedSender<Job>,
    rsp_tx: UnboundedSender<NetworkValue>,
    last_received_job_id: &Mutex<Option<u32>>,
    last_responded_job_id: &Mutex<Option<u32>>,
) -> io::Result<()> {
    match network_value {
        NetworkValue::Request {
            job_id,
            worker_id,
            payload,
        } => {
            *last_received_job_id.lock().unwrap() = Some(job_id);

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
        NetworkValue::QueryJobState { worker_id } => {
            let last_received = *last_received_job_id.lock().unwrap();
            let last_responded = *last_responded_job_id.lock().unwrap();

            let response = NetworkValue::JobStateResponse {
                worker_id,
                last_received_job_id: last_received,
                last_responded_job_id: last_responded,
            };
            rsp_tx
                .send(response)
                .map_err(|_| io::Error::other("Failed to send job state response"))
        }
        NetworkValue::Response { .. } | NetworkValue::JobStateResponse { .. } => Err(
            io::Error::other("Unexpected Response/JobStateResponse on worker connection"),
        ),
    }
}
