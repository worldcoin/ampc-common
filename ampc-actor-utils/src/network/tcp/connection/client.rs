use crate::network::tcp::{
    configure_tcp_stream, Client, ConnectError, DynStreamConn, TcpStreamConn, TlsClientConfig,
    TlsError, TlsStreamConn,
};
use async_trait::async_trait;
use eyre::Result;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::rustls::{
    pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer, ServerName},
    ClientConfig, RootCertStore,
};
use tokio_rustls::{TlsConnector, TlsStream};

#[derive(Clone)]
pub struct TlsClient {
    tls_connector: TlsConnector,
}

#[derive(Clone, Default)]
pub struct TcpClient {}

impl TlsClient {
    pub async fn new(cfg: TlsClientConfig) -> Result<Self, TlsError> {
        let mut roots = RootCertStore::empty();
        let client_config = match cfg {
            TlsClientConfig::ServerOnly { root_certs } => {
                for root_cert in &root_certs {
                    for cert in CertificateDer::pem_file_iter(root_cert)
                        .map_err(|e| TlsError::CertificateError(e.to_string()))?
                    {
                        let cert = cert.map_err(|e| TlsError::CertificateError(e.to_string()))?;
                        roots
                            .add(cert)
                            .map_err(|e| TlsError::CertificateError(e.to_string()))?;
                    }
                }
                ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth()
            }
            TlsClientConfig::Mutual {
                root_certs,
                key_file,
                cert_file,
            } => {
                for root_cert in &root_certs {
                    for cert in CertificateDer::pem_file_iter(root_cert)
                        .map_err(|e| TlsError::CertificateError(e.to_string()))?
                    {
                        let cert = cert.map_err(|e| TlsError::CertificateError(e.to_string()))?;
                        roots
                            .add(cert)
                            .map_err(|e| TlsError::CertificateError(e.to_string()))?;
                    }
                }
                let certs = CertificateDer::pem_file_iter(&cert_file)
                    .map_err(|e| TlsError::CertificateError(e.to_string()))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| TlsError::CertificateError(e.to_string()))?;
                let key = PrivateKeyDer::from_pem_file(&key_file)
                    .map_err(|e| TlsError::PrivateKeyError(e.to_string()))?;
                ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_client_auth_cert(certs, key)
                    .map_err(|e| TlsError::ConfigError(e.to_string()))?
            }
        };
        let tls_connector = TlsConnector::from(Arc::new(client_config));
        Ok(Self { tls_connector })
    }
}

#[async_trait]
impl Client for TlsClient {
    type Output = TlsStreamConn;
    async fn connect(&self, url: String) -> Result<Self::Output, ConnectError> {
        let hostname = url
            .split(':')
            .next()
            .ok_or_else(|| ConnectError::InvalidInput("Invalid URL: missing hostname".to_string()))?
            .to_string();

        let domain = ServerName::try_from(hostname)
            .map_err(|e| ConnectError::InvalidInput(format!("Invalid server name: {}", e)))?;
        let stream = TcpStream::connect(&url).await?;
        configure_tcp_stream(&stream).map_err(|e| ConnectError::TcpConfigFailed(e.to_string()))?;

        let tls_stream = self.tls_connector.connect(domain, stream).await?;
        Ok(TlsStreamConn(TlsStream::Client(tls_stream)))
    }
}

#[async_trait]
impl Client for TcpClient {
    type Output = TcpStreamConn;
    async fn connect(&self, url: String) -> Result<Self::Output, ConnectError> {
        let stream = TcpStream::connect(&url).await?;
        configure_tcp_stream(&stream).map_err(|e| ConnectError::TcpConfigFailed(e.to_string()))?;
        Ok(TcpStreamConn(stream))
    }
}

#[derive(Clone)]
pub struct BoxTcpClient(pub TcpClient);
#[async_trait]
impl Client for BoxTcpClient {
    type Output = DynStreamConn;
    async fn connect(&self, url: String) -> Result<Self::Output, ConnectError> {
        let stream = self.0.connect(url).await?;
        Ok(Box::new(stream))
    }
}
