use eyre::Result;
use serde::{Deserialize, Deserializer, Serialize};
use socket2::{SockRef, TcpKeepalive};
use std::time::Duration;
use tokio::net::TcpStream;

/// Set no_delay and keepalive on a TCP stream
pub fn configure_tcp_stream(stream: &TcpStream) -> Result<()> {
    let params = TcpKeepalive::new()
        // idle time before keepalives get sent. NGINX default is 60 seconds. want to be less than that.
        .with_time(Duration::from_secs(30))
        // how often to send keepalives
        .with_interval(Duration::from_secs(30))
        // how many unanswered probes before the connection is closed
        .with_retries(4);
    let socket_ref = SockRef::from(stream);
    socket_ref.set_tcp_nodelay(true)?;
    socket_ref.set_tcp_keepalive(&params)?;
    Ok(())
}

/// TLS configuration for secure network communication.
/// Used to deserialize inputs from a yaml file
#[derive(Debug, Clone, Serialize, Deserialize, clap::Args)]
#[group(requires_all = ["private_key", "leaf_cert", "root_certs"])]
pub struct TlsConfig {
    #[arg(required = false)]
    #[serde(default)]
    pub private_key: Option<String>,

    #[arg(required = false)]
    #[serde(default)]
    pub leaf_cert: Option<String>,

    #[serde(default, deserialize_with = "deserialize_yaml_json_string")]
    pub root_certs: Vec<String>,
}

pub fn deserialize_yaml_json_string<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: String = Deserialize::deserialize(deserializer)?;
    serde_json::from_str(&value).map_err(serde::de::Error::custom)
}
