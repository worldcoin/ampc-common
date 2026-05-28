use ampc_actor_utils::{
    execution::player::Identity,
    network::tcp::{
        connection::{
            accept_loop,
            client::{TcpClient, TlsClient},
            server::{TcpServer, TlsServer},
            ConnectionRequest,
        },
        to_inaddr_any, ConnectionConfig, ConnectionId, Peer, TcpStreamConn, TlsClientConfig,
        TlsServerConfig, TlsStreamConn,
    },
};
use eyre::Result;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, Ia5String, IsCa, KeyPair,
    SanType,
};
use serial_test::serial;
use std::io::Write;
use std::{net::SocketAddr, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
    time::{timeout, Duration},
};
use tokio_util::sync::CancellationToken;
use tracing_test::traced_test;

/// Test utilities for certificate generation
mod cert_utils {
    use tempfile::TempDir;

    use super::*;
    use std::fs;

    pub struct CertificateBundle {
        // don't drop this
        pub _temp_dir: TempDir,
        pub ca_cert_path: String,
        pub server_cert_path: String,
        pub server_key_path: String,
        pub client_cert_path: String,
        pub client_key_path: String,
    }

    impl CertificateBundle {
        pub fn root_certs(&self) -> Vec<String> {
            vec![self.ca_cert_path.clone()]
        }
    }

    /// Common test setup for bidirectional peer tests
    pub struct BidirectionalTestSetup {
        pub certs: CertificateBundle,
        pub addr_a: SocketAddr,
        pub addr_b: SocketAddr,
        pub peer_a_id: Identity,
        pub peer_b_id: Identity,
        pub peer_a: Arc<Peer>,
        pub peer_b: Arc<Peer>,
    }

    /// Initialize common test setup for bidirectional peer tests with ServerOnly TLS
    /// peer_a_name should be alphabetically higher than peer_b_name for proper ordering
    pub async fn get_bidirectional_setup(
        peer_a_name: &str,
        peer_b_name: &str,
    ) -> Result<BidirectionalTestSetup> {
        ampc_actor_utils::network::tcp::init_rustls_crypto_provider();

        let certs = generate_certificates()?;

        // Each peer gets its own port and address
        let port_a = find_free_port().await;
        let port_b = find_free_port().await;
        let addr_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse()?;
        let addr_b: SocketAddr = format!("127.0.0.1:{}", port_b).parse()?;

        let peer_a_id = Identity(peer_a_name.to_string());
        let peer_b_id = Identity(peer_b_name.to_string());

        // peer_a references peer_b's address, and vice versa
        let peer_a = Arc::new(Peer::new(
            peer_b_id.clone(),
            format!("localhost:{}", port_b),
        ));
        let peer_b = Arc::new(Peer::new(
            peer_a_id.clone(),
            format!("localhost:{}", port_a),
        ));

        Ok(BidirectionalTestSetup {
            certs,
            addr_a,
            addr_b,
            peer_a_id,
            peer_b_id,
            peer_a,
            peer_b,
        })
    }

    /// Common test setup for client/server tests
    pub struct ClientServerTestSetup {
        pub certs: CertificateBundle,
        pub addr: SocketAddr,
        pub server_id: Identity,
        pub client_id: Identity,
        pub peer: Arc<Peer>,
    }

    /// Initialize common test setup for client/server tests
    pub async fn get_client_server_setup() -> Result<ClientServerTestSetup> {
        ampc_actor_utils::network::tcp::init_rustls_crypto_provider();

        let certs = generate_certificates()?;
        let port = find_free_port().await;
        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse()?;

        let server_id = Identity("server".to_string());
        let client_id = Identity("client".to_string());
        let peer = Arc::new(Peer::new(server_id.clone(), format!("localhost:{}", port)));

        Ok(ClientServerTestSetup {
            certs,
            addr,
            server_id,
            client_id,
            peer,
        })
    }

    /// Common test setup for plain TCP client/server tests (no TLS)
    pub struct PlainTcpTestSetup {
        pub addr: SocketAddr,
        pub server_id: Identity,
        pub client_id: Identity,
        pub peer: Arc<Peer>,
    }

    /// Initialize common test setup for plain TCP client/server tests (no TLS/certs)
    pub async fn get_plain_tcp_setup() -> Result<PlainTcpTestSetup> {
        let port = find_free_port().await;
        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse()?;

        let server_id = Identity("server".to_string());
        let client_id = Identity("client".to_string());
        let peer = Arc::new(Peer::new(server_id.clone(), format!("localhost:{}", port)));

        Ok(PlainTcpTestSetup {
            addr,
            server_id,
            client_id,
            peer,
        })
    }

    /// Generate a complete certificate bundle: CA + server cert + client cert
    pub fn generate_certificates() -> Result<CertificateBundle> {
        let temp_dir = tempfile::tempdir()?;

        // 1. Generate CA certificate
        let mut ca_params = CertificateParams::default();
        ca_params.distinguished_name = DistinguishedName::new();
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "Test CA");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];

        let ca_key_pair = KeyPair::generate()?;
        let ca_cert = ca_params.self_signed(&ca_key_pair)?;

        let ca_cert_pem = ca_cert.pem();
        let ca_cert_path = temp_dir.path().join("ca.crt");
        {
            use std::io::Write;
            let mut file = fs::File::create(&ca_cert_path)?;
            file.write_all(ca_cert_pem.as_bytes())?;
            file.sync_all()?;
        }

        // 2. Generate server certificate signed by CA
        let mut server_params = CertificateParams::default();
        server_params.distinguished_name = DistinguishedName::new();
        server_params
            .distinguished_name
            .push(DnType::CommonName, "localhost");
        server_params.subject_alt_names = vec![
            SanType::DnsName(Ia5String::try_from("localhost")?),
            SanType::IpAddress("127.0.0.1".parse().unwrap()),
        ];
        server_params.key_usages = vec![
            rcgen::KeyUsagePurpose::DigitalSignature,
            rcgen::KeyUsagePurpose::KeyEncipherment,
        ];

        let server_key_pair = KeyPair::generate()?;
        let server_cert = server_params.signed_by(&server_key_pair, &ca_cert, &ca_key_pair)?;

        let server_cert_pem = server_cert.pem();
        let server_key_pem = server_key_pair.serialize_pem();

        let server_cert_path = temp_dir.path().join("server.crt");
        let server_key_path = temp_dir.path().join("server.key");
        let mut file = fs::File::create(&server_cert_path)?;
        file.write_all(server_cert_pem.as_bytes())?;
        file.sync_all()?;
        let mut file = fs::File::create(&server_key_path)?;
        file.write_all(server_key_pem.as_bytes())?;
        file.sync_all()?;

        // 3. Generate client certificate signed by CA
        let mut client_params = CertificateParams::default();
        client_params.distinguished_name = DistinguishedName::new();
        client_params
            .distinguished_name
            .push(DnType::CommonName, "test-client");
        client_params.key_usages = vec![
            rcgen::KeyUsagePurpose::DigitalSignature,
            rcgen::KeyUsagePurpose::KeyEncipherment,
        ];

        let client_key_pair = KeyPair::generate()?;
        let client_cert = client_params.signed_by(&client_key_pair, &ca_cert, &ca_key_pair)?;

        let client_cert_pem = client_cert.pem();
        let client_key_pem = client_key_pair.serialize_pem();

        let client_cert_path = temp_dir.path().join("client.crt");
        let client_key_path = temp_dir.path().join("client.key");
        let mut file = fs::File::create(&client_cert_path)?;
        file.write_all(client_cert_pem.as_bytes())?;
        file.sync_all()?;
        let mut file = fs::File::create(&client_key_path)?;
        file.write_all(client_key_pem.as_bytes())?;
        file.sync_all()?;

        Ok(CertificateBundle {
            _temp_dir: temp_dir,
            ca_cert_path: ca_cert_path.to_str().unwrap().to_string(),
            server_cert_path: server_cert_path.to_str().unwrap().to_string(),
            server_key_path: server_key_path.to_str().unwrap().to_string(),
            client_cert_path: client_cert_path.to_str().unwrap().to_string(),
            client_key_path: client_key_path.to_str().unwrap().to_string(),
        })
    }

    /// Find a free port for testing
    pub async fn find_free_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    /// Spawn an echo server task that connects with the given config and echoes data back
    pub async fn spawn_echo_server<T: ampc_actor_utils::network::tcp::NetworkConnection>(
        conn_id: ConnectionId,
        identity: Identity,
        config: ConnectionConfig<T>,
        shutdown: CancellationToken,
        message_size: usize,
    ) -> tokio::task::JoinHandle<Result<()>> {
        tokio::spawn(async move {
            let connection_state = ampc_actor_utils::network::tcp::connection::ConnectionState::new(
                shutdown,
                CancellationToken::new(),
            );

            let mut conn: T = timeout(
                TEST_TIMEOUT,
                ampc_actor_utils::network::tcp::connect(
                    conn_id,
                    Arc::new(identity),
                    connection_state,
                    config,
                ),
            )
            .await??;

            let mut buf = vec![0u8; message_size];
            conn.read_exact(&mut buf).await?;
            conn.write_all(&buf).await?;

            Ok(())
        })
    }

    /// Spawn a client task that connects, sends a message, and verifies the echo
    pub async fn spawn_test_client<T: ampc_actor_utils::network::tcp::NetworkConnection>(
        conn_id: ConnectionId,
        identity: Identity,
        config: ConnectionConfig<T>,
        shutdown: CancellationToken,
        message: &'static [u8],
        delay_ms: u64,
    ) -> tokio::task::JoinHandle<Result<()>> {
        tokio::spawn(async move {
            // Give server time to start if needed
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }

            let connection_state = ampc_actor_utils::network::tcp::connection::ConnectionState::new(
                shutdown,
                CancellationToken::new(),
            );

            let mut conn: T = timeout(
                TEST_TIMEOUT,
                ampc_actor_utils::network::tcp::connect(
                    conn_id,
                    Arc::new(identity),
                    connection_state,
                    config,
                ),
            )
            .await??;

            conn.write_all(message).await?;

            let mut buf = vec![0u8; message.len()];
            conn.read_exact(&mut buf).await?;

            if buf != message {
                eyre::bail!("Echo mismatch: expected {:?}, got {:?}", message, buf);
            }

            Ok(())
        })
    }
}

const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const TEST_MESSAGE: &[u8] = b"Hello, TLS!";

/// Test ServerOnly connection with ServerOnly TLS
/// This mimics the workpool Leader/Worker setup
#[serial]
#[traced_test]
#[tokio::test(flavor = "multi_thread")]
async fn test_server_only_connection_with_server_only_tls() -> Result<()> {
    let setup = cert_utils::get_client_server_setup().await?;
    let cert_utils::ClientServerTestSetup {
        certs,
        addr,
        server_id,
        client_id,
        peer,
    } = setup;

    // Create TLS server with ServerOnly auth (no client cert verification)
    let listener = TlsServer::new(
        to_inaddr_any(addr),
        TlsServerConfig::ServerOnly {
            key_file: certs.server_key_path.clone(),
            cert_file: certs.server_cert_path.clone(),
        },
    )
    .await?;

    let shutdown_ct = CancellationToken::new();

    // Spawn accept loop task
    let (conn_req_tx, conn_req_rx) = mpsc::unbounded_channel::<ConnectionRequest<TlsStreamConn>>();
    let accept_task = {
        let shutdown = shutdown_ct.clone();
        tokio::spawn(async move {
            accept_loop(listener, conn_req_rx, shutdown).await;
        })
    };

    // Server-side echo handler
    let server_task = cert_utils::spawn_echo_server(
        ConnectionId::new(0),
        server_id.clone(),
        ConnectionConfig::Server {
            peer_id: client_id.clone(),
            conn_cmd_tx: conn_req_tx.clone(),
        },
        shutdown_ct.clone(),
        TEST_MESSAGE.len(),
    )
    .await;

    // Client connects using TlsClient with ServerOnly auth
    let client = Arc::new(
        TlsClient::new(TlsClientConfig::ServerOnly {
            root_certs: certs.root_certs(),
        })
        .await?,
    );
    let client_task = cert_utils::spawn_test_client(
        ConnectionId::new(0),
        client_id.clone(),
        ConnectionConfig::Client {
            peer: peer.clone(),
            client: client.clone(),
        },
        shutdown_ct.clone(),
        TEST_MESSAGE,
        0,
    )
    .await;

    // Wait for both tasks to complete
    timeout(TEST_TIMEOUT, server_task)
        .await
        .expect("timeout")?
        .expect("failed");
    timeout(TEST_TIMEOUT, client_task)
        .await
        .expect("timeout")?
        .expect("failed");

    // Clean up
    shutdown_ct.cancel();
    timeout(Duration::from_secs(1), accept_task).await.ok();

    Ok(())
}

/// Test ServerOnly connection with Mutual TLS
/// Server requires client certificate, client presents it
#[serial]
#[traced_test]
#[tokio::test(flavor = "multi_thread")]
async fn test_server_only_connection_with_mutual_tls() -> Result<()> {
    let setup = cert_utils::get_client_server_setup().await?;
    let cert_utils::ClientServerTestSetup {
        certs,
        addr,
        server_id,
        client_id,
        peer,
    } = setup;

    // Create TLS server with Mutual auth (requires and verifies client cert)
    let listener = TlsServer::new(
        to_inaddr_any(addr),
        TlsServerConfig::Mutual {
            root_certs: certs.root_certs(),
            key_file: certs.server_key_path.clone(),
            cert_file: certs.server_cert_path.clone(),
        },
    )
    .await?;

    let shutdown_ct = CancellationToken::new();

    // Spawn accept loop task
    let (conn_req_tx, conn_req_rx) = mpsc::unbounded_channel::<ConnectionRequest<TlsStreamConn>>();
    let accept_task = {
        let shutdown = shutdown_ct.clone();
        tokio::spawn(async move {
            accept_loop(listener, conn_req_rx, shutdown).await;
        })
    };

    // Server-side echo handler
    let server_task = cert_utils::spawn_echo_server(
        ConnectionId::new(0),
        server_id.clone(),
        ConnectionConfig::Server {
            peer_id: client_id.clone(),
            conn_cmd_tx: conn_req_tx.clone(),
        },
        shutdown_ct.clone(),
        TEST_MESSAGE.len(),
    )
    .await;

    // Client connects using TlsClient with Mutual auth (presents client cert)
    let client = Arc::new(
        TlsClient::new(TlsClientConfig::Mutual {
            root_certs: certs.root_certs(),
            key_file: certs.client_key_path.clone(),
            cert_file: certs.client_cert_path.clone(),
        })
        .await?,
    );
    let client_task = cert_utils::spawn_test_client(
        ConnectionId::new(0),
        client_id.clone(),
        ConnectionConfig::Client {
            peer: peer.clone(),
            client: client.clone(),
        },
        shutdown_ct.clone(),
        TEST_MESSAGE,
        0,
    )
    .await;

    // Wait for both tasks to complete
    timeout(TEST_TIMEOUT, server_task)
        .await
        .expect("timeout")?
        .expect("failed");
    timeout(TEST_TIMEOUT, client_task)
        .await
        .expect("timeout")?
        .expect("failed");

    // Clean up
    shutdown_ct.cancel();
    timeout(Duration::from_secs(1), accept_task).await.ok();

    Ok(())
}

/// Test Bidirectional connection with ServerOnly TLS
/// Both peers can initiate/accept, TLS is server-only (no client certs)
#[serial]
#[traced_test]
#[tokio::test(flavor = "multi_thread")]
async fn test_bidirectional_connection_with_server_only_tls() -> Result<()> {
    let setup = cert_utils::get_bidirectional_setup("peer_b", "peer_a").await?;
    let cert_utils::BidirectionalTestSetup {
        certs,
        peer_a_id,
        peer_b_id,
        peer_a,
        peer_b,
        addr_a,
        addr_b,
    } = setup;

    let shutdown_ct = CancellationToken::new();

    // Create separate TLS server listeners for each peer with ServerOnly auth
    let listener_a = TlsServer::new(
        to_inaddr_any(addr_a),
        TlsServerConfig::ServerOnly {
            key_file: certs.server_key_path.clone(),
            cert_file: certs.server_cert_path.clone(),
        },
    )
    .await?;

    let listener_b = TlsServer::new(
        to_inaddr_any(addr_b),
        TlsServerConfig::ServerOnly {
            key_file: certs.server_key_path.clone(),
            cert_file: certs.server_cert_path.clone(),
        },
    )
    .await?;

    // Create separate TLS clients for each peer
    let client_a = Arc::new(
        TlsClient::new(TlsClientConfig::ServerOnly {
            root_certs: certs.root_certs(),
        })
        .await?,
    );

    let client_b = Arc::new(
        TlsClient::new(TlsClientConfig::ServerOnly {
            root_certs: certs.root_certs(),
        })
        .await?,
    );

    // Spawn accept loop for peer_a
    let (conn_req_tx_a, conn_req_rx_a) =
        mpsc::unbounded_channel::<ConnectionRequest<TlsStreamConn>>();
    let accept_task_a = {
        let shutdown = shutdown_ct.clone();
        tokio::spawn(async move {
            accept_loop(listener_a, conn_req_rx_a, shutdown).await;
        })
    };

    // Spawn accept loop for peer_b
    let (conn_req_tx_b, conn_req_rx_b) =
        mpsc::unbounded_channel::<ConnectionRequest<TlsStreamConn>>();
    let accept_task_b = {
        let shutdown = shutdown_ct.clone();
        tokio::spawn(async move {
            accept_loop(listener_b, conn_req_rx_b, shutdown).await;
        })
    };

    // peer_b_id is "peer_a" (lower ID) - will accept connection
    let peer_b_acceptor = cert_utils::spawn_echo_server(
        ConnectionId::new(0),
        peer_b_id.clone(),
        ConnectionConfig::Bidirectional {
            peer: peer_b.clone(), // peer_b describes remote "peer_b"
            client: client_b,
            conn_cmd_tx: conn_req_tx_b.clone(),
        },
        shutdown_ct.clone(),
        TEST_MESSAGE.len(),
    )
    .await;

    // peer_a_id is "peer_b" (higher ID) - will connect
    let peer_a_connector = cert_utils::spawn_test_client(
        ConnectionId::new(0),
        peer_a_id.clone(),
        ConnectionConfig::Bidirectional {
            peer: peer_a.clone(), // peer_a describes remote "peer_a"
            client: client_a,
            conn_cmd_tx: conn_req_tx_a.clone(),
        },
        shutdown_ct.clone(),
        TEST_MESSAGE,
        200, // Give peer_b time to start accepting
    )
    .await;

    // Wait for both peer tasks to complete
    timeout(TEST_TIMEOUT, peer_b_acceptor)
        .await
        .expect("timeout")?
        .expect("failed");
    timeout(TEST_TIMEOUT, peer_a_connector)
        .await
        .expect("timeout")?
        .expect("failed");

    // Clean up
    shutdown_ct.cancel();
    timeout(Duration::from_secs(1), accept_task_a).await.ok();
    timeout(Duration::from_secs(1), accept_task_b).await.ok();

    Ok(())
}

/// Test Bidirectional connection with Mutual TLS
/// This mimics the MPC setup with full mutual authentication
#[serial]
#[traced_test]
#[tokio::test(flavor = "multi_thread")]
async fn test_bidirectional_connection_with_mutual_tls() -> Result<()> {
    let setup = cert_utils::get_bidirectional_setup("peer_a", "peer_z").await?;
    let cert_utils::BidirectionalTestSetup {
        certs,
        peer_a_id,
        peer_b_id,
        peer_a,
        peer_b,
        addr_a,
        addr_b,
    } = setup;

    let shutdown_ct = CancellationToken::new();

    // Create separate TLS server listeners for each peer with ServerOnly auth
    let listener_a = TlsServer::new(
        to_inaddr_any(addr_a),
        TlsServerConfig::Mutual {
            root_certs: certs.root_certs(),
            key_file: certs.server_key_path.clone(),
            cert_file: certs.server_cert_path.clone(),
        },
    )
    .await?;

    let listener_b = TlsServer::new(
        to_inaddr_any(addr_b),
        TlsServerConfig::Mutual {
            root_certs: certs.root_certs(),
            key_file: certs.server_key_path.clone(),
            cert_file: certs.server_cert_path.clone(),
        },
    )
    .await?;

    // Create separate TLS clients for each peer
    let client_a = Arc::new(
        TlsClient::new(TlsClientConfig::Mutual {
            root_certs: certs.root_certs(),
            key_file: certs.client_key_path.clone(),
            cert_file: certs.client_cert_path.clone(),
        })
        .await?,
    );

    let client_b = Arc::new(
        TlsClient::new(TlsClientConfig::Mutual {
            root_certs: certs.root_certs(),
            key_file: certs.client_key_path.clone(),
            cert_file: certs.client_cert_path.clone(),
        })
        .await?,
    );

    // Spawn accept loop for peer_a
    let (conn_req_tx_a, conn_req_rx_a) =
        mpsc::unbounded_channel::<ConnectionRequest<TlsStreamConn>>();
    let accept_task_a = {
        let shutdown = shutdown_ct.clone();
        tokio::spawn(async move {
            accept_loop(listener_a, conn_req_rx_a, shutdown).await;
        })
    };

    // Spawn accept loop for peer_b
    let (conn_req_tx_b, conn_req_rx_b) =
        mpsc::unbounded_channel::<ConnectionRequest<TlsStreamConn>>();
    let accept_task_b = {
        let shutdown = shutdown_ct.clone();
        tokio::spawn(async move {
            accept_loop(listener_b, conn_req_rx_b, shutdown).await;
        })
    };

    // peer_b (higher ID "peer_z") - will connect
    let peer_b_connector = cert_utils::spawn_test_client(
        ConnectionId::new(0),
        peer_b_id.clone(),
        ConnectionConfig::Bidirectional {
            peer: peer_b.clone(), // peer_b describes remote peer "peer_a"
            client: client_b,
            conn_cmd_tx: conn_req_tx_b.clone(),
        },
        shutdown_ct.clone(),
        TEST_MESSAGE,
        200, // Give peer_a time to start accepting
    )
    .await;

    // peer_a (lower ID "peer_a") - will accept connection
    let peer_a_acceptor = cert_utils::spawn_echo_server(
        ConnectionId::new(0),
        peer_a_id.clone(),
        ConnectionConfig::Bidirectional {
            peer: peer_a.clone(), // peer_a describes remote peer "peer_z"
            client: client_a,
            conn_cmd_tx: conn_req_tx_a.clone(),
        },
        shutdown_ct.clone(),
        TEST_MESSAGE.len(),
    )
    .await;

    // Wait for both peer tasks to complete
    timeout(TEST_TIMEOUT, peer_a_acceptor)
        .await
        .expect("timeout")?
        .expect("failed");
    timeout(TEST_TIMEOUT, peer_b_connector)
        .await
        .expect("timeout")?
        .expect("failed");

    // Clean up
    shutdown_ct.cancel();
    timeout(Duration::from_secs(1), accept_task_a).await.ok();
    timeout(Duration::from_secs(1), accept_task_b).await.ok();

    Ok(())
}

/// Test plain TCP with ClientOnly connection (no TLS)
#[serial]
#[traced_test]
#[tokio::test(flavor = "multi_thread")]
async fn test_client_only_connection_plain_tcp() -> Result<()> {
    let setup = cert_utils::get_plain_tcp_setup().await?;
    let cert_utils::PlainTcpTestSetup {
        addr,
        server_id,
        client_id,
        peer,
    } = setup;

    // Create plain TCP server and client
    let listener = TcpServer::new(to_inaddr_any(addr)).await?;
    let client = Arc::new(TcpClient::default());

    let shutdown_ct = CancellationToken::new();

    // Spawn accept loop
    let (conn_req_tx, conn_req_rx) = mpsc::unbounded_channel::<ConnectionRequest<TcpStreamConn>>();
    let accept_task = {
        let shutdown = shutdown_ct.clone();
        tokio::spawn(async move {
            accept_loop(listener, conn_req_rx, shutdown).await;
        })
    };

    // Spawn server echo task
    let server_task = cert_utils::spawn_echo_server(
        ConnectionId::new(0),
        server_id.clone(),
        ConnectionConfig::Server {
            peer_id: client_id.clone(),
            conn_cmd_tx: conn_req_tx.clone(),
        },
        shutdown_ct.clone(),
        TEST_MESSAGE.len(),
    )
    .await;

    // Client connects using TcpClient
    let client_task = cert_utils::spawn_test_client(
        ConnectionId::new(0),
        client_id.clone(),
        ConnectionConfig::Client {
            peer: peer.clone(),
            client: client.clone(),
        },
        shutdown_ct.clone(),
        TEST_MESSAGE,
        0,
    )
    .await;

    timeout(TEST_TIMEOUT, server_task)
        .await
        .expect("timeout")?
        .expect("failed");
    timeout(TEST_TIMEOUT, client_task)
        .await
        .expect("timeout")?
        .expect("failed");

    shutdown_ct.cancel();
    timeout(Duration::from_secs(1), accept_task).await.ok();
    Ok(())
}

/// Negative test: Client without cert trying to connect to Mutual TLS server should fail
#[serial]
#[traced_test]
#[tokio::test(flavor = "multi_thread")]
async fn test_mutual_tls_rejects_client_without_cert() -> Result<()> {
    let setup = cert_utils::get_client_server_setup().await?;
    let cert_utils::ClientServerTestSetup {
        certs,
        addr,
        server_id,
        client_id,
        peer,
    } = setup;

    // note that if you change this to TlsServerAuth::Server and comment out root_certs, then
    // the connection should succeed (and the test would then fail)
    //
    // Server requires Mutual TLS (client cert)
    let listener = TlsServer::new(
        to_inaddr_any(addr),
        TlsServerConfig::Mutual {
            root_certs: certs.root_certs(),
            key_file: certs.server_key_path.clone(),
            cert_file: certs.server_cert_path.clone(),
        },
    )
    .await?;

    let shutdown_ct = CancellationToken::new();

    let (conn_req_tx, conn_req_rx) = mpsc::unbounded_channel::<ConnectionRequest<TlsStreamConn>>();
    let accept_task = {
        let shutdown = shutdown_ct.clone();
        tokio::spawn(async move {
            accept_loop(listener, conn_req_rx, shutdown).await;
        })
    };

    let server_task = cert_utils::spawn_echo_server(
        ConnectionId::new(0),
        server_id.clone(),
        ConnectionConfig::Server {
            peer_id: client_id.clone(),
            conn_cmd_tx: conn_req_tx.clone(),
        },
        shutdown_ct.clone(),
        TEST_MESSAGE.len(),
    )
    .await;

    // Client only has ServerOnly auth (no client cert) - should fail
    let client = Arc::new(
        TlsClient::new(TlsClientConfig::ServerOnly {
            root_certs: certs.root_certs(),
        })
        .await?,
    );
    let client_task = cert_utils::spawn_test_client(
        ConnectionId::new(0),
        client_id.clone(),
        ConnectionConfig::Client {
            peer: peer.clone(),
            client: client.clone(),
        },
        shutdown_ct.clone(),
        TEST_MESSAGE,
        200,
    )
    .await;

    let client_result = timeout(TEST_TIMEOUT, client_task).await;
    assert!(client_result.is_err(), "connection should time out");

    shutdown_ct.cancel();
    timeout(Duration::from_secs(1), accept_task).await.ok();
    timeout(Duration::from_secs(1), server_task).await.ok();
    Ok(())
}
