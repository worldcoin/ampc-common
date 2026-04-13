use crate::{
    execution::{player::Identity, session::SessionId},
    network::{
        mpc::NetworkValue,
        tcp::{ConnectionState, NetworkConnection},
    },
};
use eyre::{bail, Result};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

    pub async fn sync(&mut self, connection_state: ConnectionState) -> Result<()> {
        let all_conns = self.c0.iter_mut().chain(self.c1.iter_mut());
        let err_ct = connection_state.err_ct();
        let shutdown_ct = connection_state.shutdown_ct();
        tokio::select! {
            _ = err_ct.cancelled() => bail!("connection cancelled"),
            _ = shutdown_ct.cancelled() => bail!("shutdown_triggered"),
            r = futures::future::join_all(all_conns.map(send_and_receive)) => r
                .into_iter()
                .collect::<Result<Vec<_>, _>>().map(|_| ())
        }
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

// ensure all peers are connected to each other.
async fn send_and_receive<T: NetworkConnection>(conn: &mut T) -> Result<()> {
    let snd_buf: [u8; 3] = [2, b'o', b'k'];
    let mut rcv_buf = [0_u8; 3];
    conn.write_all(&snd_buf).await?;
    conn.flush().await?;
    conn.read_exact(&mut rcv_buf).await?;
    if rcv_buf != snd_buf {
        bail!("ok failed");
    }
    Ok(())
}
