use async_trait::async_trait;
use eyre::Result;
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsStream;

/// Trait for network connections that can be closed
#[async_trait]
pub trait NetworkConnection: AsyncRead + AsyncWrite + Send + Sync + Unpin {
    async fn close(&mut self);
}

/// Trait for establishing outbound connections
#[async_trait]
pub trait Client: Send + Sync {
    type Output: NetworkConnection;
    async fn connect(&self, url: String) -> Result<Self::Output>;
}

/// Trait for accepting incoming connections
#[async_trait]
pub trait Server: Send {
    type Output: NetworkConnection;
    async fn accept(&self) -> Result<(SocketAddr, Self::Output)>;
}

//
// Stream wrapper types
//

pub struct TcpStreamConn(pub TcpStream);
pub struct TlsStreamConn(pub TlsStream<TcpStream>);

/// Dynamic stream type for mixed connectors and listeners
pub type DynStreamConn = Box<dyn NetworkConnection>;

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

#[async_trait]
impl NetworkConnection for TlsStreamConn {
    async fn close(&mut self) {
        let _ = self.0.shutdown().await;
    }
}
