/// Network configuration types
use serde::{Deserialize, Deserializer, Serialize};

/// TLS configuration for secure network communication.
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
