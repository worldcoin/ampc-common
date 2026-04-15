//! Shared TCP/TLS infrastructure for both MPC and workpool networking
//!
//! This module provides common connection handling, client/server implementations,
//! and stream wrappers that can be used by both MPC and workpool modules.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Once;

pub mod config;
pub mod connection;
pub mod streams;
pub mod types;

// Re-export commonly used types
pub use config::{configure_tcp_stream, TlsConfig};
pub use connection::{accept_loop, connect, ConnectionConfig, ConnectionRequest, ConnectionState};
pub use streams::{Client, DynStreamConn, NetworkConnection, Server, TcpStreamConn, TlsStreamConn};
pub use types::{ConnectionId, Peer};

// allow initialization of TLS from possibly multiple modules, while ensuring that the provider is only installed once
pub fn init_rustls_crypto_provider() {
    static INSTALL_CRYPTO_PROVIDER: Once = Once::new();
    INSTALL_CRYPTO_PROVIDER.call_once(|| {
        if tokio_rustls::rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .is_err()
        {
            tracing::error!("failed to install CryptoProvider for rustls");
        }
    });
}

// convert a socket address to use the "any" IP address, which allows servers to listen on all interfaces
pub fn to_inaddr_any(mut socket: SocketAddr) -> SocketAddr {
    if socket.is_ipv4() {
        socket.set_ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    } else {
        socket.set_ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    }
    socket
}
