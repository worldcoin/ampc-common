use std::{net::SocketAddr, sync::Arc};

use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::{
    execution::player::Identity,
    network::tcp::{
        self, Client, ConnectionRequest, NetworkConnection, Server, SetupError, TlsConfig,
    },
};

pub trait Connector {
    pub fn connect(&self) -> Box<dyn NetworkConnection>;
}

pub struct PeerConnector<T: NetworkConnection + 'static, C: Client<Output = T> + 'static> {
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

impl<T: NetworkConnection + 'static, C: Client<Output = T> + 'static> PeerConnector<T, C> {
    pub fn new<S>(connector: C, listener: S, shutdown_ct: CancellationToken) -> Self
    where
        S: Server<Output = T> + 'static,
    {
        todo!()
    }
}

// todo: implement Connector for PeerConnector

pub async fn build_peer_connector<
    T: NetworkConnection + 'static,
    C: Client<Output = T> + 'static,
>(
    args: PeerConnectorArgs,
    shutdown_ct: CancellationToken,
) -> Result<Box<dyn Connector>, SetupError> {
    tcp::init_rustls_crypto_provider();

    let my_addr = tcp::to_inaddr_any(args.address.parse::<SocketAddr>()?);

    macro_rules! build_network_handle {
        ($listener:expr, $connector:expr) => {
            Ok(Box::new(
                PeerConnector::new(
                    my_identity,
                    peers,
                    $connector,
                    $listener,
                    tcp_config,
                    shutdown_ct,
                    my_index,
                    role_assignments,
                )
                .await?,
            ))
        };
    }

    if let Some(tls) = args.tls.as_ref() {
        tracing::info!(
            "Building PeerConnector, with TLS, from configs: {:?} {:?}",
            tcp_config,
            tls,
        );

        let root_certs = tls.clone().root_certs;

        tracing::info!("Running in full app TLS mode.");
        if tls.private_key.is_none() || tls.leaf_cert.is_none() {
            return Err(eyre::eyre!(
                "TLS configuration is required for this operation"
            ));
        }
        let private_key = tls
            .private_key
            .as_ref()
            .ok_or(eyre::eyre!("Private key is required for TLS"))?;

        let leaf_cert = tls
            .leaf_cert
            .as_ref()
            .ok_or(eyre::eyre!("Leaf certificate is required for TLS"))?;

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
            "Building PeerConnector, without TLS, from config: {:?}, listen_addr: {:?}",
            tcp_config,
            my_addr
        );
        let listener = BoxTcpServer(TcpServer::new(my_addr).await?);
        let connector = BoxTcpClient(TcpClient::default());
        build_network_handle!(listener, connector)
    }
}
