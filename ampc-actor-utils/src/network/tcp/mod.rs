//! Shared TCP/TLS infrastructure for both MPC and workpool networking
//!
//! This module provides common connection handling, client/server implementations,
//! and stream wrappers that can be used by both MPC and workpool modules.

pub mod config;
pub mod connection;
pub mod streams;
pub mod types;

use crate::execution::player::Identity;
use secrecy::SecretString;
use serde::{Deserialize, Deserializer, Serialize};
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Once};
use thiserror::Error;
use tokio::sync::mpsc::UnboundedSender;

// Re-export commonly used types
pub use config::configure_tcp_stream;
pub use connection::{accept_loop, connect, ConnectionRequest, ConnectionState};
pub use streams::{
    Client, ConnectError, DynStreamConn, NetworkConnection, Server, TcpStreamConn, TlsStreamConn,
};
pub use types::{ConnectionId, Peer};

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
    /// Note that the Server will trust the peers to correctly self-identify
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

/// TLS configuration for a client. Used by the workpool networking
/// stack and is used internally by the MPC networking stack.
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
    /// only the server is authenticated, with PEM contents given inline
    /// rather than as paths (e.g. certs sourced from a secret manager /
    /// env var, which have no filesystem representation)
    ServerOnlyPem {
        /// the root certs for the server (PEM contents)
        root_certs_pem: Vec<String>,
    },
    /// both the client and server are authenticated, with PEM contents
    /// given inline rather than as paths
    MutualPem {
        /// the root certs for the server (PEM contents)
        root_certs_pem: Vec<String>,
        /// the client key (PEM content)
        key_pem: String,
        /// the client cert (PEM content)
        cert_pem: String,
    },
}

/// TLS configuration for a server. Used by the workpool
/// networking stack and is used internally by the MPC
/// networking stack.
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
    /// PEM contents given inline rather than as paths (e.g. certs sourced
    /// from a secret manager / env var, which have no filesystem
    /// representation)
    ServerOnlyPem {
        /// the server key (PEM content)
        key_pem: String,
        /// the server cert (PEM content)
        cert_pem: String,
    },
    /// PEM contents given inline rather than as paths
    MutualPem {
        /// the client certs (PEM contents)
        root_certs_pem: Vec<String>,
        /// the server key (PEM content)
        key_pem: String,
        /// the server cert (PEM content)
        cert_pem: String,
    },
}

/// How to interpret `TlsConfig`'s key/cert material.
///
/// `File` (the default) is the original, backwards-compatible behavior:
/// `private_key` / `leaf_cert` / `root_certs` are filesystem paths. `Pem`
/// is for values that are already PEM contents (e.g. sourced from a secret
/// manager / env var, which have no filesystem representation) — in this
/// mode the private key is read from `private_key_pem` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum TlsSource {
    #[default]
    File,
    Pem,
}

fn parse_secret_string(s: &str) -> Result<SecretString, Infallible> {
    Ok(SecretString::from(s.to_string()))
}

/// TLS configuration for secure network communication. This gets passed
/// to build_network_handle() for the MPC networking stack.
/// It is also used to deserialize inputs from a yaml file.
#[derive(Debug, Clone, Serialize, Deserialize, clap::Args)]
#[group(requires_all = ["private_key", "leaf_cert", "root_certs"])]
pub struct TlsConfig {
    /// Selects whether the key/cert material below is file paths (using
    /// `private_key`) or inline PEM contents (using `private_key_pem`
    /// instead). Defaults to `File` so existing configs and callers keep
    /// working unchanged.
    #[arg(required = false, value_enum, default_value_t = TlsSource::File)]
    #[serde(default)]
    pub source: TlsSource,

    /// Path to the private key file. Used when `source` is `File` (the
    /// default).
    #[arg(required = false)]
    #[serde(default)]
    pub private_key: Option<String>,

    /// Inline PEM content of the private key. Used when `source` is `Pem`.
    /// Confidential: wrapped so it never gets printed via Debug/logged, and
    /// is skipped by Serialize (e.g. accidentally re-serializing this
    /// config to JSON logs). Defaults to absent, like every other TLS field
    /// here.
    #[arg(required = false, value_parser = parse_secret_string)]
    #[serde(default, skip_serializing)]
    pub private_key_pem: Option<SecretString>,

    #[arg(required = false)]
    #[serde(default)]
    pub leaf_cert: Option<String>,

    #[serde(default, deserialize_with = "deserialize_yaml_json_string")]
    pub root_certs: Vec<String>,
}

// used when constructing a worker or leader handle
#[derive(Error, Debug)]
pub enum SetupError {
    #[error("configuration error: {0}")]
    BadConfig(String),
    #[error("parse error: {0}")]
    InvalidAddress(String),
    #[error("error in TCP stack: {0}")]
    ListenFailed(String),
    #[error("Failed to bind listener: {0}")]
    BindFailed(String),
}

/// Error type for TLS configuration and setup
#[derive(Error, Debug)]
pub enum TlsError {
    #[error("Failed to load or parse certificate: {0}")]
    CertificateError(String),

    #[error("Failed to validate certificate: {0}")]
    CertificateValidation(String),

    #[error("Failed to load or parse private key: {0}")]
    PrivateKeyError(String),

    #[error("Failed to configure TLS: {0}")]
    ConfigError(String),
}

impl From<TlsError> for SetupError {
    fn from(err: TlsError) -> Self {
        SetupError::BadConfig(err.to_string())
    }
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

pub fn deserialize_yaml_json_string<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: String = Deserialize::deserialize(deserializer)?;
    serde_json::from_str(&value).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIVATE_KEY_PEM_MARKER: &str = "-----BEGIN PRIVATE KEY-----super-secret-material";

    fn sample_config_file() -> serde_json::Value {
        serde_json::json!({
            "private_key": "/etc/mesh/key.pem",
            "leaf_cert": "-----BEGIN CERTIFICATE-----leaf",
            "root_certs": "[\"-----BEGIN CERTIFICATE-----root\"]",
        })
    }

    fn sample_config_pem() -> serde_json::Value {
        serde_json::json!({
            "source": "pem",
            "private_key_pem": PRIVATE_KEY_PEM_MARKER,
            "leaf_cert": "-----BEGIN CERTIFICATE-----leaf",
            "root_certs": "[\"-----BEGIN CERTIFICATE-----root\"]",
        })
    }

    #[test]
    fn private_key_pem_is_never_printed_via_debug() {
        let config: TlsConfig = serde_json::from_value(sample_config_pem()).unwrap();
        let debug_output = format!("{config:?}");
        assert!(!debug_output.contains(PRIVATE_KEY_PEM_MARKER));
        assert!(debug_output.contains("REDACTED"));
        // leaf_cert / root_certs are public, so they're still visible.
        assert!(debug_output.contains("leaf_cert"));
    }

    #[test]
    fn private_key_pem_is_never_printed_via_serialize() {
        let config: TlsConfig = serde_json::from_value(sample_config_pem()).unwrap();
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(!serialized.contains(PRIVATE_KEY_PEM_MARKER));
    }

    #[test]
    fn source_defaults_to_file_when_omitted() {
        // Existing configs / env vars that predate `source` must keep working
        // unchanged, which means defaulting to the original file-path behavior.
        let config: TlsConfig = serde_json::from_value(sample_config_file()).unwrap();
        assert_eq!(config.source, TlsSource::File);
        assert_eq!(config.private_key.as_deref(), Some("/etc/mesh/key.pem"));
        assert!(config.private_key_pem.is_none());
    }

    #[test]
    fn private_key_pem_defaults_to_none_when_omitted() {
        let config: TlsConfig = serde_json::from_value(sample_config_file()).unwrap();
        assert!(config.private_key_pem.is_none());
    }

    #[test]
    fn source_pem_round_trips() {
        let config: TlsConfig = serde_json::from_value(sample_config_pem()).unwrap();
        assert_eq!(config.source, TlsSource::Pem);
    }
}
