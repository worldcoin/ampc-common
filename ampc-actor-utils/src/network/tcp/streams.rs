use async_trait::async_trait;
use eyre::Result;
use std::net::SocketAddr;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::Error as RustlsError;
use tokio_rustls::TlsStream;

pub trait MaybeHasTlsCert {
    fn maybe_tls_cert(&self) -> Option<&[u8]>;
}

/// Trait for network connections that can be closed
#[async_trait]
pub trait NetworkConnection:
    MaybeHasTlsCert + AsyncRead + AsyncWrite + Send + Sync + Unpin
{
    async fn close(&mut self);
}

/// Trait for establishing outbound connections
#[async_trait]
pub trait Client: Send + Sync {
    type Output: NetworkConnection;
    async fn connect(&self, url: String) -> Result<Self::Output, ConnectError>;
}

/// TLS mode exposed by a server
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    None,
    ServerOnly,
    Mutual,
}

/// Trait for accepting incoming connections
#[async_trait]
pub trait Server: Send {
    type Output: NetworkConnection;
    async fn accept(&self) -> Result<(SocketAddr, Self::Output), ConnectError>;
    fn tls_mode(&self) -> TlsMode {
        TlsMode::None
    }
}

/// Error type for network connection operations
#[derive(Error, Debug)]
pub enum ConnectError {
    /// TLS certificate validation failed (e.g., untrusted CA, expired cert)
    #[error("TLS certificate error: {0}")]
    TlsCertificateError(String),

    /// Other TLS/protocol errors
    #[error("TLS error: {0}")]
    TlsError(String),

    #[error("TCP configuration failed: {0}")]
    TcpConfigFailed(String),

    /// Invalid peer address, etc
    #[error("Invalid input: {0}")]
    InvalidInput(String),

    /// Handshake protocol error
    /// note that this can also happen if the
    /// peer is not ready and does not respond.
    /// This should be treated as a transient error.
    #[error("Handshake failed: {0}")]
    HandshakeError(String),

    /// IO error during connection
    #[error("IO error: {0}")]
    IoError(std::io::Error),

    /// Channel/internal error
    #[error("{0}")]
    Other(String),
}

impl From<RustlsError> for ConnectError {
    fn from(e: RustlsError) -> Self {
        match &e {
            RustlsError::InvalidCertificate(reason) => {
                ConnectError::TlsCertificateError(format!("{:?}", reason))
            }
            RustlsError::NoCertificatesPresented => {
                ConnectError::TlsCertificateError("No certificates presented".into())
            }
            _ => ConnectError::TlsError(e.to_string()),
        }
    }
}

impl From<std::io::Error> for ConnectError {
    fn from(e: std::io::Error) -> Self {
        // Check if this wraps a RustlsError
        if let Some(rustls_err) = e
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<RustlsError>())
        {
            return match rustls_err {
                RustlsError::InvalidCertificate(reason) => {
                    ConnectError::TlsCertificateError(format!("{:?}", reason))
                }
                RustlsError::NoCertificatesPresented => {
                    ConnectError::TlsCertificateError("No certificates presented".into())
                }
                _ => ConnectError::TlsError(rustls_err.to_string()),
            };
        }
        // Plain IO error (TCP connection refused, timeout, etc.)
        ConnectError::IoError(e)
    }
}

impl From<eyre::Report> for ConnectError {
    fn from(e: eyre::Report) -> Self {
        ConnectError::Other(e.to_string())
    }
}

impl From<tokio::sync::oneshot::error::RecvError> for ConnectError {
    fn from(e: tokio::sync::oneshot::error::RecvError) -> Self {
        ConnectError::Other(e.to_string())
    }
}

//
// Stream wrapper types
//

pub struct TcpStreamConn(pub TcpStream);
pub struct TlsStreamConn(pub TlsStream<TcpStream>);

/// Dynamic stream type for mixed connectors and listeners
pub type DynStreamConn = Box<dyn NetworkConnection>;

impl MaybeHasTlsCert for DynStreamConn {
    fn maybe_tls_cert(&self) -> Option<&[u8]> {
        (**self).maybe_tls_cert()
    }
}

#[async_trait]
impl NetworkConnection for DynStreamConn {
    async fn close(&mut self) {
        (**self).close().await;
    }
}

impl AsyncRead for TcpStreamConn {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for TcpStreamConn {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

impl MaybeHasTlsCert for TcpStreamConn {
    fn maybe_tls_cert(&self) -> Option<&[u8]> {
        None
    }
}

#[async_trait]
impl NetworkConnection for TcpStreamConn {
    async fn close(&mut self) {
        let _ = self.0.shutdown().await;
    }
}

impl AsyncRead for TlsStreamConn {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for TlsStreamConn {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

impl MaybeHasTlsCert for TlsStreamConn {
    fn maybe_tls_cert(&self) -> Option<&[u8]> {
        let (_, tls_session) = self.0.get_ref();
        tls_session
            .peer_certificates()
            .and_then(|certs| certs.first().map(|c| c.as_ref()))
    }
}

#[async_trait]
impl NetworkConnection for TlsStreamConn {
    async fn close(&mut self) {
        let _ = self.0.shutdown().await;
    }
}
