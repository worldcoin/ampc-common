pub mod client; // trait for initiating a connection. hides details of TCP vs TLS
mod connection_state;
mod handshake;
mod listener; // accept inbound connections
pub mod server; // trait for accepting connections. hides details of TCP vs TLS // used to determine the peer id and connection id

pub use connection_state::ConnectionState;
pub use listener::{accept_loop, ConnectionRequest};
use rand::{rngs::StdRng, Rng, SeedableRng};

use crate::{
    execution::player::Identity,
    network::tcp::{ConnectError, ConnectionConfig, ConnectionId, NetworkConnection},
};
use std::{sync::Arc, time::Duration};
use tokio::{sync::oneshot, time::sleep};

impl<T: NetworkConnection + 'static> ConnectionConfig<T> {
    // used for logging
    fn get_peer_id(&self) -> Identity {
        match self {
            Self::Bidirectional { peer, .. } => peer.id().clone(),
            Self::Server { peer_id, .. } => peer_id.clone(),
            Self::Client { peer, .. } => peer.id().clone(),
        }
    }
}

// connect and perform handshake
pub async fn connect<T: NetworkConnection + 'static>(
    connection_id: ConnectionId,
    own_id: Arc<Identity>,
    connection_state: ConnectionState,
    connection_config: ConnectionConfig<T>,
) -> Result<T, ConnectError> {
    let peer_id = connection_config.get_peer_id();
    let connector = Connector {
        connection_id,
        own_id: own_id.clone(),
        connection_state,
        connection_config,
    };
    let (rsp_tx, rsp_rx) = oneshot::channel::<Result<T, ConnectError>>();
    tokio::spawn(async move {
        let r = connector.run().await;
        let _ = rsp_tx.send(r);
    });
    let result = rsp_rx
        .await
        .map_err(|_| ConnectError::Other("unreachable error in connect()".into()))??;
    tracing::debug!(
        "connection succeeded for {:?} -> {:?}, {:?}",
        own_id,
        peer_id,
        connection_id
    );
    Ok(result)
}

struct Connector<T: NetworkConnection + 'static> {
    connection_id: ConnectionId,
    own_id: Arc<Identity>,
    connection_state: ConnectionState,
    connection_config: ConnectionConfig<T>,
}

impl<T: NetworkConnection> Connector<T> {
    async fn connect(&self) -> Result<T, ConnectError> {
        match &self.connection_config {
            ConnectionConfig::Bidirectional {
                peer,
                client,
                conn_cmd_tx,
            } => {
                if &*self.own_id > peer.id() {
                    let mut stream = client.connect(peer.url().to_string()).await?;
                    handshake::outbound(&mut stream, &self.own_id, &self.connection_id).await?;
                    handshake::outbound_ok(&mut stream).await?;
                    Ok(stream)
                } else {
                    let (rsp_tx, rsp_rx) = oneshot::channel();
                    let req = ConnectionRequest::new(peer.id().clone(), self.connection_id, rsp_tx);
                    if conn_cmd_tx.send(req).is_err() {
                        return Err(ConnectError::Other(
                            "failed to send connection request".into(),
                        ));
                    }
                    let r = rsp_rx.await?;
                    Ok(r)
                }
            }
            ConnectionConfig::Server {
                peer_id,
                conn_cmd_tx,
            } => {
                let (rsp_tx, rsp_rx) = oneshot::channel();
                let req = ConnectionRequest::new(peer_id.clone(), self.connection_id, rsp_tx);
                if conn_cmd_tx.send(req).is_err() {
                    return Err(ConnectError::Other(
                        "failed to send connection request".into(),
                    ));
                }
                let r = rsp_rx.await?;
                Ok(r)
            }
            ConnectionConfig::Client { peer, client } => {
                let mut stream = client.connect(peer.url().to_string()).await?;
                handshake::outbound(&mut stream, &self.own_id, &self.connection_id).await?;
                handshake::outbound_ok(&mut stream).await?;
                Ok(stream)
            }
        }
    }

    async fn connect_loop(&self) -> Result<T, ConnectError> {
        let mut rng: StdRng =
            StdRng::from_rng(&mut rand::thread_rng()).expect("Failed to seed RNG");

        const RETRY_SECS: u64 = 2;

        sleep(Duration::from_millis(rng.gen_range(0..=3000))).await;

        loop {
            let err = match self.connect().await {
                Ok(stream) => return Ok(stream),
                Err(e) => e,
            };

            // Log all errors
            tracing::warn!(
                "connection attempt failed for {:?} -> {:?}: {}",
                self.own_id,
                self.connection_config.get_peer_id(),
                err
            );

            // Fatal errors - don't retry
            match &err {
                ConnectError::TlsCertificateError(_)
                | ConnectError::TlsError(_)
                | ConnectError::TcpConfigFailed(_)
                | ConnectError::InvalidInput(_) => {
                    return Err(err);
                }
                // Transient errors - retry after sleep
                ConnectError::IoError(_)
                | ConnectError::Other(_)
                | ConnectError::HandshakeError(_) => {
                    sleep(Duration::from_secs(RETRY_SECS)).await;
                }
            }
        }
    }

    async fn run(&self) -> Result<T, ConnectError> {
        let err_ct = self.connection_state.err_ct();
        let shutdown_ct = self.connection_state.shutdown_ct();

        tokio::select! {
            result = self.connect_loop() => {
                result
            },
            _ = err_ct.cancelled() => {
                Err(ConnectError::Other("connection task cancelled".into()))
            },
            _ = shutdown_ct.cancelled() => {
                Err(ConnectError::Other("connection task cancelled".into()))
            }
        }
    }
}
