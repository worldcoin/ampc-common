use crate::{
    execution::{player::Identity, session::SessionId},
    network::{mpc::NetworkValue, tcp::NetworkConnection},
};
use std::sync::Arc;
use tokio::sync::mpsc;

// Re-export shared tcp types
pub use crate::network::tcp::Peer;

// session multiplexing over a socket requires a SessionId
pub type OutboundMsg = (SessionId, NetworkValue);
pub type OutStream = mpsc::UnboundedSender<OutboundMsg>;
pub type InStream = mpsc::UnboundedReceiver<NetworkValue>;

pub struct PeerConnections<T: NetworkConnection + 'static> {
    peers: Vec<Arc<Peer>>,
    // conns[i] holds the connections established with peers[i]
    conns: Vec<Vec<T>>,
}

impl<T: NetworkConnection + 'static> PeerConnections<T> {
    pub fn new(peers: Vec<Arc<Peer>>, conns: Vec<Vec<T>>) -> Self {
        assert_eq!(
            peers.len(),
            conns.len(),
            "expected one connection group per peer"
        );
        Self { peers, conns }
    }

    pub fn peer_ids(&self) -> Vec<Identity> {
        self.peers.iter().map(|peer| peer.id().clone()).collect()
    }
}

impl<T: NetworkConnection + 'static> IntoIterator for PeerConnections<T> {
    type Item = (Identity, Vec<T>);
    type IntoIter = std::vec::IntoIter<(Identity, Vec<T>)>;

    fn into_iter(self) -> Self::IntoIter {
        self.peers
            .into_iter()
            .map(|peer| peer.id().clone())
            .zip(self.conns)
            .collect::<Vec<_>>()
            .into_iter()
    }
}
