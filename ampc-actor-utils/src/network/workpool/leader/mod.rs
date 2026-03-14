mod job_tracker;
mod leader_task;

use super::{LeaderError, NetworkFailure, Payload};
pub use job_tracker::{JobTracker, JobType};

use crate::{
    execution::player::Identity,
    network::tcp::{
        self,
        connection::{
            client::{BoxTcpClient, TcpClient, TlsClient},
            server::{BoxTcpServer, TcpServer, TlsServer},
        },
        TlsConfig,
    },
};
use std::net::{self, SocketAddr};
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tokio_util::sync::CancellationToken;

pub struct WorkerRsp {
    pub job_id: u32,
    pub payload: Payload,
    pub worker_id: u16,
}

enum Job {
    Broadcast {
        payload: Payload,
        rsp: oneshot::Sender<Vec<WorkerRsp>>,
    },
    ScatterGather {
        msgs: Vec<WorkerRsp>,
        rsp: oneshot::Sender<Vec<WorkerRsp>>,
    },
}

#[derive(Clone)]
pub struct LeaderHandle {
    ct: CancellationToken,
    ch: UnboundedSender<Job>,
    num_workers: usize,
}

pub struct LeaderArgs {
    pub leader_id: Identity,
    pub leader_address: String,
    pub worker_addresses: Vec<String>,
    pub tls: Option<TlsConfig>,
}

impl Drop for LeaderHandle {
    fn drop(&mut self) {
        self.ct.cancel();
    }
}

impl LeaderHandle {
    /// Broadcast to all workers, wait for all acks
    pub async fn broadcast(&self, payload: Payload) -> Result<Vec<WorkerRsp>, NetworkFailure> {
        let (tx, rx) = oneshot::channel();
        self.ch
            .send(Job::Broadcast { payload, rsp: tx })
            .map_err(|_| NetworkFailure::ChannelClosed)?;
        rx.await.map_err(|_| NetworkFailure::ChannelClosed)
    }

    /// Scatter-gather: send different payloads to different workers, gather results in partition order
    pub async fn scatter_gather(
        &self,
        msgs: Vec<WorkerRsp>,
    ) -> Result<Vec<WorkerRsp>, NetworkFailure> {
        let (tx, rx) = oneshot::channel();
        self.ch
            .send(Job::ScatterGather { msgs, rsp: tx })
            .map_err(|_| NetworkFailure::ChannelClosed)?;
        rx.await.map_err(|_| NetworkFailure::ChannelClosed)
    }

    pub fn num_workers(&self) -> usize {
        self.num_workers
    }
}

pub async fn build_leader(
    args: LeaderArgs,
    shutdown_ct: CancellationToken,
) -> Result<LeaderHandle, LeaderError> {
    tcp::init_rustls_crypto_provider();
    let shutdown_ct = shutdown_ct.child_token();

    let leader_addr: SocketAddr = args
        .leader_address
        .parse()
        .map_err(|e: net::AddrParseError| LeaderError::ParseError(e.to_string()))?;
    let num_workers = args.worker_addresses.len();
    let worker_urls = args.worker_addresses.into_iter();

    let handle_tx =
        if let Some(tls) = args.tls.as_ref() {
            tracing::info!("Building WorkPool Leader with TLS");

            let root_certs = tls.clone().root_certs;
            if tls.private_key.is_none() || tls.leaf_cert.is_none() {
                return Err(LeaderError::ConnectionError(
                    "TLS private key and leaf cert required".to_string(),
                ));
            }
            let private_key = tls.private_key.as_ref().ok_or_else(|| {
                LeaderError::ConnectionError("TLS private key required".to_string())
            })?;

            let leaf_cert = tls.leaf_cert.as_ref().ok_or_else(|| {
                LeaderError::ConnectionError("TLS leaf cert required".to_string())
            })?;

            let listener = TlsServer::new(leader_addr, private_key, leaf_cert, &root_certs)
                .await
                .map_err(|e| {
                    LeaderError::ConnectionError(format!("Failed to create TLS server: {}", e))
                })?;
            let connector = TlsClient::new_with_ca_certs(&root_certs)
                .await
                .map_err(|e| {
                    LeaderError::ConnectionError(format!("Failed to create TLS client: {}", e))
                })?;

            leader_task::spawn(
                args.leader_id,
                worker_urls,
                connector,
                listener,
                shutdown_ct.clone(),
            )
            .map_err(|e| LeaderError::ConnectionError(format!("Failed to create leader: {}", e)))
        } else {
            tracing::info!("Building WorkPool Leader without TLS");

            let listener = BoxTcpServer(TcpServer::new(leader_addr).await.map_err(|e| {
                LeaderError::ConnectionError(format!("Failed to create TCP server: {}", e))
            })?);
            let connector = BoxTcpClient(TcpClient::default());

            leader_task::spawn(
                args.leader_id,
                worker_urls,
                connector,
                listener,
                shutdown_ct.clone(),
            )
            .map_err(|e| LeaderError::ConnectionError(format!("Failed to create leader: {}", e)))
        }?;

    Ok(LeaderHandle {
        ct: shutdown_ct,
        ch: handle_tx,
        num_workers,
    })
}
