mod worker_task;

use super::{JobId, Payload, SetupError, WorkerId};
use crate::network::tcp::Peer;
use crate::network::workpool::value::NetworkValue;
use crate::{
    execution::player::Identity,
    network::tcp::{
        self,
        connection::client::{BoxTcpClient, TcpClient, TlsClient, TlsClientConfig},
    },
};
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
    pub leader_id: Identity,
    pub leader_address: String,
    /// set to None for TCP
    pub tls: Option<TlsClientConfig>,
}

pub async fn build_worker_handle(
    args: WorkerArgs,
    shutdown_ct: CancellationToken,
) -> Result<WorkerHandle, SetupError> {
    tcp::init_rustls_crypto_provider();

    let shutdown_ct = shutdown_ct.child_token();
    let leader = Peer::new(args.leader_id, args.leader_address);

    let job_rx = if let Some(tls) = args.tls {
        tracing::info!("Building WorkPool Worker with TLS");
        let connector = TlsClient::new(tls)
            .await
            .map_err(|e| SetupError::BadConfig(format!("Failed to create TLS client: {}", e)))?;
        worker_task::spawn(args.worker_id, leader, connector, shutdown_ct.clone())
    } else {
        tracing::info!("Building WorkPool Worker without TLS");
        let connector = BoxTcpClient(TcpClient::default());
        worker_task::spawn(args.worker_id, leader, connector, shutdown_ct.clone())
    };

    Ok(WorkerHandle { job_rx })
}
