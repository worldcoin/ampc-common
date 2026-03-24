use crate::execution::player::Identity;
use async_trait::async_trait;
use eyre::Result;

pub mod handle;
pub mod local;
mod value;

// Re-export commonly used types for convenience
pub use handle::{build_network_handle, NetworkHandle, NetworkHandleArgs};
pub use local::LocalNetworkingStore;
pub use value::{NetworkInt, NetworkValue, StateChecksum};

/// Requirements for networking.
#[async_trait]
pub trait Networking {
    async fn send(&mut self, value: NetworkValue, receiver: &Identity) -> Result<()>;

    async fn receive(&mut self, sender: &Identity) -> Result<NetworkValue>;
}

#[derive(Clone)]
pub enum NetworkType {
    Local,
    Tcp {
        connection_parallelism: usize,
        request_parallelism: usize,
    },
}

impl NetworkType {
    pub fn default_connection_parallelism() -> usize {
        1
    }

    pub fn default_request_parallelism() -> usize {
        1
    }

    pub fn default_tcp() -> Self {
        Self::Tcp {
            connection_parallelism: Self::default_connection_parallelism(),
            request_parallelism: Self::default_request_parallelism(),
        }
    }
}
