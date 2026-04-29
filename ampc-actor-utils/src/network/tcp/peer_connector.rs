use std::{net::SocketAddr, sync::Arc};

use async_trait::async_trait;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::{
    execution::player::Identity,
    network::tcp::{
        self, accept_loop, connect,
        connection::{
            client::{BoxTcpClient, TcpClient, TlsClient},
            server::{BoxTcpServer, TcpServer, TlsServer},
        },
        Client, ConnectError, ConnectionConfig, ConnectionId, ConnectionRequest, ConnectionState,
        NetworkConnection, Peer, Server, SetupError, TlsClientConfig, TlsConfig, TlsServerConfig,
    },
};

#[async_trait]
pub trait Connector {
    async fn connect(&self, peer: Peer) -> Result<Box<dyn NetworkConnection>, ConnectError>;
}

pub struct PeerConnector<T: NetworkConnection + 'static, C: Client<Output = T> + 'static> {
    id: Arc<Identity>,
    connector: Arc<C>,
    conn_cmd_tx: UnboundedSender<ConnectionRequest<T>>,
    shutdown_ct: CancellationToken,
}

pub struct PeerConnectorArgs {
    pub id: Identity,
    pub address: String,
    /// set to None for TCP
    pub tls: Option<TlsConfig>,
}

impl<T: NetworkConnection + 'static, C: Client<Output = T> + 'static> Drop for PeerConnector<T, C> {
    fn drop(&mut self) {
        self.shutdown_ct.cancel();
        tracing::debug!("PeerConnector dropped");
    }
}

#[async_trait]
impl<T: NetworkConnection + 'static, C: Client<Output = T> + 'static> Connector
    for PeerConnector<T, C>
{
    async fn connect(&self, peer: Peer) -> Result<Box<dyn NetworkConnection>, ConnectError> {
        let err_ct = CancellationToken::new();
        let connection_state = ConnectionState::new(self.shutdown_ct.clone(), err_ct);
        let conn = connect(
            ConnectionId::new(0),
            self.id.clone(),
            connection_state,
            ConnectionConfig::Bidirectional {
                peer: Arc::new(peer),
                client: self.connector.clone(),
                conn_cmd_tx: self.conn_cmd_tx.clone(),
            },
        )
        .await?;

        Ok(Box::new(conn))
    }
}

impl<T: NetworkConnection + 'static, C: Client<Output = T> + 'static> PeerConnector<T, C> {
    pub fn new<S>(id: Identity, connector: C, listener: S, shutdown_ct: CancellationToken) -> Self
    where
        S: Server<Output = T> + 'static,
    {
        let shutdown_ct = shutdown_ct.child_token();
        let (conn_cmd_tx, conn_cmd_rx) = mpsc::unbounded_channel::<ConnectionRequest<T>>();

        // be sure not to make more than one network handle...
        tokio::spawn(accept_loop(listener, conn_cmd_rx, shutdown_ct.clone()));

        Self {
            id: Arc::new(id),
            connector: Arc::new(connector),
            conn_cmd_tx,
            shutdown_ct,
        }
    }
}

pub async fn build_peer_connector<
    T: NetworkConnection + 'static,
    C: Client<Output = T> + 'static,
>(
    args: PeerConnectorArgs,
    shutdown_ct: CancellationToken,
) -> Result<Box<dyn Connector>, SetupError> {
    tcp::init_rustls_crypto_provider();

    let my_addr = tcp::to_inaddr_any(
        args.address
            .parse::<SocketAddr>()
            .map_err(|e| SetupError::InvalidAddress(e.to_string()))?,
    );

    macro_rules! build_network_handle {
        ($listener:expr, $connector:expr) => {
            Ok(Box::new(PeerConnector::new(
                args.id,
                $connector,
                $listener,
                shutdown_ct,
            )))
        };
    }

    if let Some(tls) = args.tls.as_ref() {
        tracing::info!(
            "Building PeerConnector with TLS, listen_addr: {:?}",
            my_addr,
        );

        let root_certs = tls.clone().root_certs;

        tracing::info!("Running in full app TLS mode.");
        if tls.private_key.is_none() || tls.leaf_cert.is_none() {
            return Err(SetupError::BadConfig(
                "TLS configuration is required for this operation".to_string(),
            ));
        }
        let private_key = tls.private_key.as_ref().ok_or(SetupError::BadConfig(
            "Private key is required for TLS".to_string(),
        ))?;

        let leaf_cert = tls.leaf_cert.as_ref().ok_or(SetupError::BadConfig(
            "Leaf certificate is required for TLS".to_string(),
        ))?;

        let listener = TlsServer::new(
            my_addr,
            TlsServerConfig::Mutual {
                root_certs: root_certs.clone(),
                key_file: private_key.clone(),
                cert_file: leaf_cert.clone(),
            },
        )
        .await?;
        let connector = TlsClient::new(TlsClientConfig::Mutual {
            root_certs,
            key_file: private_key.clone(),
            cert_file: leaf_cert.clone(),
        })
        .await?;
        build_network_handle!(listener, connector)
    } else {
        tracing::info!(
            "Building PeerConnector, without TLS, listen_addr: {:?}",
            my_addr
        );
        let listener = BoxTcpServer(
            TcpServer::new(my_addr)
                .await
                .map_err(|e| SetupError::ListenFailed(e.to_string()))?,
        );
        let connector = BoxTcpClient(TcpClient::default());
        build_network_handle!(listener, connector)
    }
}
