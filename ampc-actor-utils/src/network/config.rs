/// Network configuration types
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    pub private_key: Option<String>,
    pub leaf_cert: Option<String>,
    pub root_certs: Vec<String>,
}
