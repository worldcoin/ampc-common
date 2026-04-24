//! Shared TCP/TLS infrastructure for both MPC and workpool networking
//!
//! This module provides common connection handling, client/server implementations,
//! and stream wrappers that can be used by both MPC and workpool modules.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Once};

pub mod config;
pub mod connection;
pub mod streams;
pub mod types;

// Re-export commonly used types
pub use config::{configure_tcp_stream, TlsConfig};
pub use connection::{accept_loop, connect, ConnectionRequest, ConnectionState};
pub use streams::{
    Client, ConnectError, DynStreamConn, NetworkConnection, Server, TcpStreamConn, TlsStreamConn,
};
use tokio::sync::mpsc::UnboundedSender;
pub use types::{ConnectionId, Peer};

use crate::execution::player::Identity;

/// specifies how the connection will be initiated between two parties
pub enum ConnectionConfig<T: NetworkConnection + 'static> {
    /// The given party will listen for incoming connections and will either
    /// wait for a peer to initiate a connection or initiate the connection
    /// themself, depending on who has the greater peer id
    ///
    /// Assumes that both parties are configured as Bidirectional
    Bidirectional {
        peer: Arc<Peer>,
        client: Arc<dyn Client<Output = T>>,
        conn_cmd_tx: UnboundedSender<ConnectionRequest<T>>,
    },
    /// The given party will listen for connections from this peer_id.
    /// Assumes the other party is configured as Client
    Server {
        peer_id: Identity,
        conn_cmd_tx: UnboundedSender<ConnectionRequest<T>>,
    },
    /// The given party will initiate a connection to this peer.
    /// Assumes the other party is configured as Server
    Client {
        peer: Arc<Peer>,
        client: Arc<dyn Client<Output = T>>,
    },
}

/// tls configuration for a client
pub enum TlsClientConfig {
    /// only the server is authenticated
    ServerOnly {
        /// the root certs for the server
        root_certs: Vec<String>,
    },
    /// both the client and server are authenticated
    Mutual {
        /// the root certs for the server
        root_certs: Vec<String>,
        /// the client key
        key_file: String,
        /// the client cert
        cert_file: String,
    },
}

/// tls configuration for a server
pub enum TlsServerConfig {
    ServerOnly {
        /// the server key
        key_file: String,
        /// the server cert
        cert_file: String,
    },
    Mutual {
        /// the client certs
        root_certs: Vec<String>,
        /// the server key
        key_file: String,
        /// the server cert
        cert_file: String,
    },
}

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
