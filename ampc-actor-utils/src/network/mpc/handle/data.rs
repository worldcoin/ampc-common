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
    peers: [Arc<Peer>; 2],
    c0: Vec<T>,
    c1: Vec<T>,
}

impl<T: NetworkConnection + 'static> PeerConnections<T> {
    pub fn new(peers: [Arc<Peer>; 2], c0: Vec<T>, c1: Vec<T>) -> Self {
        Self { peers, c0, c1 }
    }

    pub fn peer_ids(&self) -> Vec<Identity> {
        self.peers.iter().map(|peer| peer.id().clone()).collect()
    }
}

impl<T: NetworkConnection + 'static> IntoIterator for PeerConnections<T> {
    type Item = (Identity, Vec<T>);
    type IntoIter = std::vec::IntoIter<(Identity, Vec<T>)>;

    fn into_iter(self) -> Self::IntoIter {
        vec![
            (self.peers[0].id().clone(), self.c0),
            (self.peers[1].id().clone(), self.c1),
        ]
        .into_iter()
    }
}
