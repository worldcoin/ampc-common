/// Network configuration types
use serde::{Deserialize, Deserializer, Serialize};

/// TLS configuration for secure network communication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    #[serde(default)]
    pub private_key: Option<String>,

    #[serde(default)]
    pub leaf_cert: Option<String>,

    #[serde(default, deserialize_with = "deserialize_yaml_json_string")]
    pub root_certs: Vec<String>,
}

/// Deserialize a YAML/JSON string into a vector of strings
/// This is used for configuration fields that are stored as JSON strings in YAML
pub fn deserialize_yaml_json_string<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: String = Deserialize::deserialize(deserializer)?;
    serde_json::from_str(&value).map_err(serde::de::Error::custom)
}
