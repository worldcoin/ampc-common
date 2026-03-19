mod worker_task;

use super::{JobId, Payload, SetupError, WorkerId};
use crate::network::tcp::Peer;
use crate::network::workpool::value::NetworkValue;
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
use std::net::SocketAddr;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio_util::sync::CancellationToken;

pub(crate) struct Msg {
    pub job_id: JobId,
    pub worker_id: WorkerId,
    pub payload: Payload,
}

pub struct Job {
    msg: Msg,
    rsp: mpsc::UnboundedSender<NetworkValue>,
}

impl Job {
    pub fn send_result(self, payload: Payload) {
        let r = NetworkValue::Job {
            job_id: self.msg.job_id,
            worker_id: self.msg.worker_id,
            payload,
        };
        let _ = self.rsp.send(r);
    }

    // consume the payload without a copy
    pub fn take_payload(&mut self) -> Payload {
        std::mem::take(&mut self.msg.payload)
    }
}

pub struct WorkerHandle {
    job_rx: UnboundedReceiver<Job>,
}

impl WorkerHandle {
    pub async fn recv(&mut self) -> Option<Job> {
        self.job_rx.recv().await
    }
}

pub struct WorkerArgs {
    pub worker_id: Identity,
    pub worker_address: String,
    pub leader_id: Identity,
    pub leader_address: String,
    pub tls: Option<TlsConfig>,
}

pub async fn build_worker_handle(
    args: WorkerArgs,
    shutdown_ct: CancellationToken,
) -> Result<WorkerHandle, SetupError> {
    tcp::init_rustls_crypto_provider();

    let shutdown_ct = shutdown_ct.child_token();
    let worker_addr: SocketAddr = args
        .worker_address
        .parse()
        .map_err(|e: std::net::AddrParseError| SetupError::InvalidAddress(e.to_string()))?;

    let job_rx = if let Some(tls) = args.tls.as_ref() {
        tracing::info!("Building WorkPool Worker with TLS");

        let root_certs = tls.clone().root_certs;
        if tls.private_key.is_none() || tls.leaf_cert.is_none() {
            return Err(SetupError::BadConfig(
                "TLS private key and leaf cert required".to_string(),
            ));
        }
        let private_key = tls
            .private_key
            .as_ref()
            .ok_or_else(|| SetupError::BadConfig("TLS private key required".to_string()))?;
        let leaf_cert = tls
            .leaf_cert
            .as_ref()
            .ok_or_else(|| SetupError::BadConfig("TLS leaf cert required".to_string()))?;

        let listener = TlsServer::new(worker_addr, private_key, leaf_cert, &root_certs)
            .await
            .map_err(|e| SetupError::ListenFailed(format!("Failed to create TLS server: {}", e)))?;
        let connector = TlsClient::new_with_ca_certs(&root_certs)
            .await
            .map_err(|e| SetupError::BadConfig(format!("Failed to create TLS client: {}", e)))?;

        let leader = Peer::new(args.leader_id, args.leader_address);

        worker_task::spawn(
            args.worker_id,
            leader,
            connector,
            listener,
            shutdown_ct.clone(),
        )
    } else {
        tracing::info!("Building WorkPool Worker without TLS");

        let listener = BoxTcpServer(TcpServer::new(worker_addr).await.map_err(|e| {
            SetupError::ListenFailed(format!("Failed to create TCP server: {}", e))
        })?);
        let connector = BoxTcpClient(TcpClient::default());

        let leader = Peer::new(args.leader_id, args.leader_address);
        worker_task::spawn(
            args.worker_id,
            leader,
            connector,
            listener,
            shutdown_ct.clone(),
        )
    };

    Ok(WorkerHandle { job_rx })
}
