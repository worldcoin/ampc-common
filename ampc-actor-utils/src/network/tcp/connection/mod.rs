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
    network::tcp::{Client, ConnectionId, NetworkConnection, Peer},
};
use eyre::{bail, Result};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc::UnboundedSender, oneshot},
    time::sleep,
};

pub enum ConnectionConfig<T: NetworkConnection + 'static> {
    Bidirectional {
        peer: Arc<Peer>,
        client: Arc<dyn Client<Output = T>>,
        conn_cmd_tx: UnboundedSender<ConnectionRequest<T>>,
    },
    ServerOnly {
        peer_id: Identity,
        conn_cmd_tx: UnboundedSender<ConnectionRequest<T>>,
    },
    ClientOnly {
        peer: Arc<Peer>,
        client: Arc<dyn Client<Output = T>>,
    },
}

impl<T: NetworkConnection + 'static> ConnectionConfig<T> {
    fn get_peer_id(&self) -> Identity {
        match self {
            Self::Bidirectional { peer, .. } => peer.id().clone(),
            Self::ServerOnly { peer_id, .. } => peer_id.clone(),
            Self::ClientOnly { peer, .. } => peer.id().clone(),
        }
    }
}

// connect and perform handshake
pub async fn connect<T: NetworkConnection + 'static>(
    connection_id: ConnectionId,
    own_id: Arc<Identity>,
    connection_state: ConnectionState,
    connection_config: ConnectionConfig<T>,
) -> Result<T> {
    let peer_id = connection_config.get_peer_id();
    let connector = Connector {
        connection_id,
        own_id: own_id.clone(),
        connection_state,
        connection_config,
    };
    let (rsp_tx, rsp_rx) = oneshot::channel();
    tokio::spawn(async move {
        if let Some(c) = connector.run().await {
            let _ = rsp_tx.send(c);
        }
    });
    let r = rsp_rx.await?;
    tracing::debug!(
        "connection succeeded for {:?} -> {:?}, {:?}",
        own_id,
        peer_id,
        connection_id
    );
    Ok(r)
}

struct Connector<T: NetworkConnection + 'static> {
    connection_id: ConnectionId,
    own_id: Arc<Identity>,
    connection_state: ConnectionState,
    connection_config: ConnectionConfig<T>,
}

impl<T: NetworkConnection> Connector<T> {
    async fn connect(&self) -> Result<T> {
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
                        bail!("failed to send connection request");
                    }
                    let r = rsp_rx.await?;
                    Ok(r)
                }
            }
            ConnectionConfig::ServerOnly {
                peer_id,
                conn_cmd_tx,
            } => {
                let (rsp_tx, rsp_rx) = oneshot::channel();
                let req = ConnectionRequest::new(peer_id.clone(), self.connection_id, rsp_tx);
                if conn_cmd_tx.send(req).is_err() {
                    bail!("failed to send connection request");
                }
                let r = rsp_rx.await?;
                Ok(r)
            }
            ConnectionConfig::ClientOnly { peer, client } => {
                let mut stream = client.connect(peer.url().to_string()).await?;
                handshake::outbound(&mut stream, &self.own_id, &self.connection_id).await?;
                handshake::outbound_ok(&mut stream).await?;
                Ok(stream)
            }
        }
    }

    async fn connect_loop(&self) -> T {
        let mut rng: StdRng =
            StdRng::from_rng(&mut rand::thread_rng()).expect("Failed to seed RNG");

        let retry_sec = 2;

        sleep(Duration::from_millis(rng.gen_range(0..=3000))).await;

        loop {
            if let Ok(stream) = self.connect().await {
                return stream;
            }

            sleep(Duration::from_secs(retry_sec)).await;
        }
    }

    async fn run(&self) -> Option<T> {
        let err_ct = self.connection_state.err_ct().await;
        let shutdown_ct = self.connection_state.shutdown_ct().await;

        tokio::select! {
            r = self.connect_loop() => {
                Some(r)
            },
            _ = err_ct.cancelled() => {
                None
            },
            _ = shutdown_ct.cancelled() => {
                 None
            }
        }
    }
}
