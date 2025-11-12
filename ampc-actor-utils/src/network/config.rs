/// Network configuration types
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    #[serde(default)]
    pub private_key: Option<String>,

    #[serde(default)]
    pub leaf_cert: Option<String>,

    #[serde(default, deserialize_with = "deserialize_yaml_json_string")]
    pub root_certs: Vec<String>,
}

fn deserialize_yaml_json_string<'de, D>(deserializer: D) -> eyre::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: String = Deserialize::deserialize(deserializer)?;
    serde_json::from_str(&value).map_err(serde::de::Error::custom)
}
