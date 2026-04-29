use crate::network::tcp::{
    configure_tcp_stream, ConnectError, DynStreamConn, Server, TcpStreamConn, TlsError,
    TlsServerConfig, TlsStreamConn,
};
use async_trait::async_trait;
use eyre::Result;
use std::{net::SocketAddr, sync::Arc};
use tokio::net::TcpListener;
use tokio_rustls::rustls::{
    pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer},
    server::WebPkiClientVerifier,
    RootCertStore, ServerConfig,
};
use tokio_rustls::{TlsAcceptor, TlsStream};

pub struct TlsServer {
    listener: TcpListener,
    tls_acceptor: TlsAcceptor,
}

pub struct TcpServer {
    listener: TcpListener,
}

impl TlsServer {
    pub async fn new(own_addr: SocketAddr, cfg: TlsServerConfig) -> Result<Self, TlsError> {
        let server_config = match cfg {
            TlsServerConfig::ServerOnly {
                key_file,
                cert_file,
            } => {
                let certs = CertificateDer::pem_file_iter(cert_file)
                    .map_err(|e| TlsError::CertificateError(e.to_string()))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| TlsError::CertificateError(e.to_string()))?;
                let key = PrivateKeyDer::from_pem_file(key_file)
                    .map_err(|e| TlsError::PrivateKeyError(e.to_string()))?;

                ServerConfig::builder()
                    .with_no_client_auth()
                    .with_single_cert(certs, key)
                    .map_err(|e| TlsError::ConfigError(e.to_string()))?
            }
            TlsServerConfig::Mutual {
                root_certs,
                key_file,
                cert_file,
            } => {
                let certs = CertificateDer::pem_file_iter(cert_file)
                    .map_err(|e| TlsError::CertificateError(e.to_string()))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| TlsError::CertificateError(e.to_string()))?;
                let key = PrivateKeyDer::from_pem_file(key_file)
                    .map_err(|e| TlsError::PrivateKeyError(e.to_string()))?;

                let mut root_cert_store = RootCertStore::empty();
                for root_cert in root_certs {
                    for cert in CertificateDer::pem_file_iter(root_cert)
                        .map_err(|e| TlsError::CertificateError(e.to_string()))?
                    {
                        let cert = cert.map_err(|e| TlsError::CertificateError(e.to_string()))?;
                        root_cert_store
                            .add(cert)
                            .map_err(|e| TlsError::CertificateValidation(e.to_string()))?;
                    }
                }
                let client_verifier =
                    WebPkiClientVerifier::builder(<Arc<RootCertStore>>::from(root_cert_store))
                        .build()
                        .map_err(|e| TlsError::ConfigError(e.to_string()))?;
                ServerConfig::builder()
                    .with_client_cert_verifier(client_verifier)
                    .with_single_cert(certs, key)
                    .map_err(|e| TlsError::ConfigError(e.to_string()))?
            }
        };

        let tls_acceptor = TlsAcceptor::from(Arc::new(server_config));
        let listener = TcpListener::bind(own_addr)
            .await
            .map_err(|e| TlsError::BindFailed(e.to_string()))?;
        Ok(Self {
            listener,
            tls_acceptor,
        })
    }
}

impl TcpServer {
    pub async fn new(own_addr: SocketAddr) -> Result<Self> {
        let listener = TcpListener::bind(own_addr).await?;
        Ok(Self { listener })
    }
}

#[async_trait]
impl Server for TlsServer {
    type Output = TlsStreamConn;
    async fn accept(&self) -> Result<(SocketAddr, Self::Output), ConnectError> {
        let (tcp_stream, peer_addr) = self.listener.accept().await?;
        configure_tcp_stream(&tcp_stream)
            .map_err(|e| ConnectError::TcpConfigFailed(e.to_string()))?;
        let tls_stream = self.tls_acceptor.accept(tcp_stream).await?;
        Ok((peer_addr, TlsStreamConn(TlsStream::Server(tls_stream))))
    }
}

#[async_trait]
impl Server for TcpServer {
    type Output = TcpStreamConn;
    async fn accept(&self) -> Result<(SocketAddr, Self::Output), ConnectError> {
        let (tcp_stream, peer_addr) = self.listener.accept().await?;
        configure_tcp_stream(&tcp_stream)
            .map_err(|e| ConnectError::TcpConfigFailed(e.to_string()))?;
        Ok((peer_addr, TcpStreamConn(tcp_stream)))
    }
}

pub struct BoxTcpServer(pub TcpServer);
#[async_trait]
impl Server for BoxTcpServer {
    type Output = DynStreamConn;
    async fn accept(&self) -> Result<(SocketAddr, Self::Output), ConnectError> {
        let (addr, stream) = self.0.accept().await?;
        Ok((addr, Box::new(stream)))
    }
}

#[allow(dead_code)]
pub struct BoxTlsServer(pub TlsServer);
#[async_trait]
impl Server for BoxTlsServer {
    type Output = DynStreamConn;
    async fn accept(&self) -> Result<(SocketAddr, Self::Output), ConnectError> {
        let (addr, stream) = self.0.accept().await?;
        Ok((addr, Box::new(stream)))
    }
}
