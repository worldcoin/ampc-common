//! Configuration for AMPC coordination server

use eyre::Result;
use serde::{Deserialize, Deserializer, Serialize};

/// Configuration for server coordination
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCoordinationConfig {
    /// Party/node ID (0, 1, or 2)
    #[serde(default)]
    pub party_id: usize,

    /// Hostnames of all nodes
    #[serde(
        default = "default_node_hostnames",
        deserialize_with = "deserialize_yaml_json_string"
    )]
    pub node_hostnames: Vec<String>,

    /// Healthcheck ports for all nodes (the port for this server is healthcheck_ports[party_id])
    #[serde(
        default = "default_healthcheck_ports",
        deserialize_with = "deserialize_yaml_json_string"
    )]
    pub healthcheck_ports: Vec<String>,

    /// Image name/version identifier
    #[serde(default)]
    pub image_name: String,

    /// Heartbeat interval in seconds
    #[serde(default = "default_heartbeat_interval_secs")]
    pub heartbeat_interval_secs: u64,

    /// Initial retries for heartbeat during startup
    #[serde(default = "default_heartbeat_initial_retries")]
    pub heartbeat_initial_retries: u64,

    /// HTTP query retry delay in milliseconds
    #[serde(default = "default_http_query_retry_delay_ms")]
    pub http_query_retry_delay_ms: u64,

    /// Startup sync timeout in seconds
    #[serde(default = "default_startup_sync_timeout_secs")]
    pub startup_sync_timeout_secs: u64,
}

fn default_node_hostnames() -> Vec<String> {
    vec!["localhost".to_string(); 3]
}

fn default_healthcheck_ports() -> Vec<String> {
    vec!["3000".to_string(); 3]
}

fn default_heartbeat_interval_secs() -> u64 {
    2
}

fn default_heartbeat_initial_retries() -> u64 {
    10
}

fn default_http_query_retry_delay_ms() -> u64 {
    5000
}

fn default_startup_sync_timeout_secs() -> u64 {
    300
}

fn deserialize_yaml_json_string<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: String = Deserialize::deserialize(deserializer)?;
    serde_json::from_str(&value).map_err(serde::de::Error::custom)
}

impl ServerCoordinationConfig {
    /// Get the healthcheck port for this server (from healthcheck_ports[party_id])
    pub fn get_own_healthcheck_port(&self) -> Result<usize> {
        self.healthcheck_ports
            .get(self.party_id)
            .ok_or_else(|| {
                eyre::eyre!(
                    "party_id {} is out of bounds for healthcheck_ports",
                    self.party_id
                )
            })?
            .parse()
            .map_err(|e| eyre::eyre!("Failed to parse healthcheck port: {}", e))
    }
}
