pub mod multiplexer;

use crate::{
    execution::{player::Identity, session::SessionId},
    network::{
        mpc::{
            handle::{
                config::MpcConfig,
                data::{InStream, OutStream, OutboundMsg, PeerConnections},
            },
            NetworkValue, Networking,
        },
        tcp::{ConnectionId, ConnectionState, NetworkConnection},
    },
};
use async_trait::async_trait;
use eyre::{bail, ensure, eyre, Result};
use std::collections::{BTreeMap, HashMap};
use tokio::{
    sync::mpsc::{self},
    time::timeout,
};

const STRIPE_MAGIC: &[u8; 8] = b"AMPCSTRP";
const STRIPE_HEADER_BYTES: usize = 32;
const MIN_BYTES_PER_STRIPE: usize = 256 * 1024;

#[derive(Debug)]
struct Fragment {
    sequence: u64,
    total_len: usize,
    index: usize,
    count: usize,
    payload: Vec<u8>,
}

impl Fragment {
    fn encode(
        sequence: u64,
        total_len: usize,
        index: usize,
        count: usize,
        payload: &[u8],
    ) -> Result<NetworkValue> {
        let total_len = u64::try_from(total_len)?;
        let index = u32::try_from(index)?;
        let count = u32::try_from(count)?;
        let mut bytes = Vec::with_capacity(STRIPE_HEADER_BYTES + payload.len());
        bytes.extend_from_slice(STRIPE_MAGIC);
        bytes.extend_from_slice(&sequence.to_le_bytes());
        bytes.extend_from_slice(&total_len.to_le_bytes());
        bytes.extend_from_slice(&index.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(payload);
        Ok(NetworkValue::Bytes(bytes))
    }

    fn decode(value: NetworkValue) -> Result<Self> {
        let NetworkValue::Bytes(bytes) = value else {
            bail!("striped MPC session received an unframed value");
        };
        ensure!(
            bytes.len() >= STRIPE_HEADER_BYTES,
            "striped MPC fragment is shorter than its header"
        );
        ensure!(
            &bytes[..STRIPE_MAGIC.len()] == STRIPE_MAGIC,
            "invalid striped MPC fragment magic"
        );

        let sequence = u64::from_le_bytes(bytes[8..16].try_into()?);
        let total_len = usize::try_from(u64::from_le_bytes(bytes[16..24].try_into()?))?;
        let index = u32::from_le_bytes(bytes[24..28].try_into()?) as usize;
        let count = u32::from_le_bytes(bytes[28..32].try_into()?) as usize;
        ensure!(count > 0, "striped MPC fragment has zero chunks");
        ensure!(index < count, "striped MPC fragment index is out of range");
        ensure!(total_len > 0, "striped MPC fragment has an empty message");

        Ok(Self {
            sequence,
            total_len,
            index,
            count,
            payload: bytes[STRIPE_HEADER_BYTES..].to_vec(),
        })
    }
}

#[derive(Debug)]
struct Assembly {
    total_len: usize,
    chunks: Vec<Option<Vec<u8>>>,
    received: usize,
    received_len: usize,
}

impl Assembly {
    fn new(fragment: &Fragment) -> Self {
        Self {
            total_len: fragment.total_len,
            chunks: vec![None; fragment.count],
            received: 0,
            received_len: 0,
        }
    }

    fn insert(&mut self, fragment: Fragment) -> Result<()> {
        ensure!(
            self.total_len == fragment.total_len && self.chunks.len() == fragment.count,
            "inconsistent striped MPC fragment metadata"
        );
        ensure!(
            self.chunks[fragment.index].is_none(),
            "duplicate striped MPC fragment"
        );
        self.received += 1;
        self.received_len += fragment.payload.len();
        ensure!(
            self.received_len <= self.total_len,
            "striped MPC fragments exceed declared message length"
        );
        self.chunks[fragment.index] = Some(fragment.payload);
        Ok(())
    }

    fn is_complete(&self) -> bool {
        self.received == self.chunks.len()
    }

    fn assemble(self) -> Result<NetworkValue> {
        ensure!(self.is_complete(), "striped MPC message is incomplete");
        ensure!(
            self.received_len == self.total_len,
            "striped MPC message length mismatch"
        );
        let mut bytes = Vec::with_capacity(self.total_len);
        for chunk in self.chunks {
            bytes.extend(chunk.ok_or_else(|| eyre!("missing striped MPC fragment"))?);
        }
        NetworkValue::deserialize(&bytes)
    }
}

#[derive(Debug)]
struct PeerIo {
    identity: Identity,
    tx: Vec<OutStream>,
    rx: InStream,
    next_send_sequence: u64,
    next_receive_sequence: u64,
    pending: BTreeMap<u64, Assembly>,
}

#[derive(Debug)]
pub struct TcpSession {
    session_id: SessionId,
    // TcpSession is typically used with two peers. A logical peer stream owns
    // all physical connections so one large request can use aggregate bandwidth.
    peers: Vec<PeerIo>,
    config: MpcConfig,
}

impl TcpSession {
    pub fn new(
        session_id: SessionId,
        identities: Vec<Identity>,
        tx: Vec<Vec<OutStream>>,
        rx: Vec<InStream>,
        config: MpcConfig,
    ) -> Self {
        assert_eq!(identities.len(), tx.len());
        assert_eq!(identities.len(), rx.len());
        let peers = identities
            .into_iter()
            .zip(tx)
            .zip(rx)
            .map(|((identity, tx), rx)| PeerIo {
                identity,
                tx,
                rx,
                next_send_sequence: 0,
                next_receive_sequence: 0,
                pending: BTreeMap::new(),
            })
            .collect();
        Self {
            session_id,
            peers,
            config,
        }
    }

    pub fn id(&self) -> SessionId {
        self.session_id
    }

    fn get_peer_mut(&mut self, id: &Identity) -> Option<&mut PeerIo> {
        self.peers.iter_mut().find(|peer| &peer.identity == id)
    }

    fn take_completed(peer: &mut PeerIo) -> Result<Option<NetworkValue>> {
        let sequence = peer.next_receive_sequence;
        let is_complete = peer
            .pending
            .get(&sequence)
            .is_some_and(Assembly::is_complete);
        if !is_complete {
            return Ok(None);
        }
        let assembly = peer
            .pending
            .remove(&sequence)
            .ok_or_else(|| eyre!("completed striped MPC message disappeared"))?;
        peer.next_receive_sequence = peer
            .next_receive_sequence
            .checked_add(1)
            .ok_or_else(|| eyre!("striped MPC receive sequence exhausted"))?;
        assembly.assemble().map(Some)
    }
}

impl Drop for TcpSession {
    fn drop(&mut self) {
        //tracing::debug!("dropping session id {:?}", self.session_id);
    }
}

#[async_trait]
impl Networking for TcpSession {
    async fn send(&mut self, value: NetworkValue, receiver: &Identity) -> Result<()> {
        let session_id = self.session_id;
        let peer = self.get_peer_mut(receiver).ok_or(eyre!(
            "Outgoing stream for {receiver:?} in session {:?} not found",
            session_id
        ))?;
        ensure!(
            !peer.tx.is_empty(),
            "striped MPC session has no connections"
        );

        if peer.tx.len() == 1 {
            peer.tx[0]
                .send((session_id, value))
                .map_err(|e| eyre!(e.to_string()))?;
            return Ok(());
        }

        let serialized = value.to_network();
        let stripe_count = peer
            .tx
            .len()
            .min(serialized.len().div_ceil(MIN_BYTES_PER_STRIPE).max(1));
        let sequence = peer.next_send_sequence;
        peer.next_send_sequence = peer
            .next_send_sequence
            .checked_add(1)
            .ok_or_else(|| eyre!("striped MPC send sequence exhausted"))?;
        let first_connection = usize::try_from(sequence % peer.tx.len() as u64)?;
        let stripe_base_len = serialized.len() / stripe_count;
        let extra_bytes = serialized.len() % stripe_count;

        for index in 0..stripe_count {
            let start = index * stripe_base_len + index.min(extra_bytes);
            let end = start + stripe_base_len + usize::from(index < extra_bytes);
            let payload = &serialized[start..end];
            let connection = (first_connection + index) % peer.tx.len();
            let fragment =
                Fragment::encode(sequence, serialized.len(), index, stripe_count, payload)?;
            peer.tx[connection]
                .send((session_id, fragment))
                .map_err(|e| eyre!(e.to_string()))?;
        }
        Ok(())
    }

    async fn receive(&mut self, sender: &Identity) -> Result<NetworkValue> {
        let session_id = self.session_id;
        let timeout_duration = self.config.timeout_duration;
        let max_fragments = self.config.num_connections as usize;
        let peer = self.get_peer_mut(sender).ok_or(eyre!(
            "Incoming stream for {sender:?} in session {:?} not found",
            session_id
        ))?;
        if peer.tx.len() == 1 {
            return match timeout(timeout_duration, peer.rx.recv()).await {
                Ok(res) => res.ok_or_else(|| eyre!("No message received")),
                Err(_) => Err(eyre!(
                    "Timeout while waiting for message from {sender:?} in {:?}",
                    session_id
                )),
            };
        }

        match timeout(timeout_duration, async {
            loop {
                if let Some(value) = Self::take_completed(peer)? {
                    return Ok(value);
                }
                let value = peer
                    .rx
                    .recv()
                    .await
                    .ok_or_else(|| eyre!("No message received"))?;
                let fragment = Fragment::decode(value)?;
                ensure!(
                    fragment.count <= max_fragments,
                    "striped MPC message exceeds configured connection count"
                );
                ensure!(
                    fragment.sequence >= peer.next_receive_sequence,
                    "received a stale striped MPC message"
                );
                peer.pending
                    .entry(fragment.sequence)
                    .or_insert_with(|| Assembly::new(&fragment))
                    .insert(fragment)?;
            }
        })
        .await
        {
            Ok(res) => res,
            Err(_) => Err(eyre!(
                "Timeout while waiting for striped message from {sender:?} in {:?}",
                session_id
            )),
        }
    }
}

#[derive(Default)]
pub struct SessionChannels {
    pub outbound_tx: HashMap<Identity, HashMap<ConnectionId, mpsc::UnboundedSender<OutboundMsg>>>,
    pub outbound_rx: HashMap<Identity, HashMap<ConnectionId, mpsc::UnboundedReceiver<OutboundMsg>>>,
    pub inbound_tx: HashMap<Identity, HashMap<SessionId, mpsc::UnboundedSender<NetworkValue>>>,
    pub inbound_rx: HashMap<Identity, HashMap<SessionId, mpsc::UnboundedReceiver<NetworkValue>>>,
}

pub async fn make_sessions<T: NetworkConnection + 'static>(
    connections: PeerConnections<T>,
    connection_state: ConnectionState,
    config: &MpcConfig,
    next_session_id: u32,
) -> Vec<TcpSession> {
    let sc = make_channels(connections.peer_ids(), config, next_session_id);
    make_sessions_inner(connections, connection_state, config, next_session_id, sc).await
}

fn make_channels(
    peer_ids: Vec<Identity>,
    config: &MpcConfig,
    next_session_id: u32,
) -> SessionChannels {
    let mut sc = SessionChannels::default();

    for peer_id in peer_ids {
        let mut outbound_tx = HashMap::new();
        let mut outbound_rx = HashMap::new();
        let mut inbound_tx = HashMap::new();
        let mut inbound_rx = HashMap::new();

        for connection_id in (0..config.num_connections).map(ConnectionId::from) {
            let (tx, rx) = mpsc::unbounded_channel::<OutboundMsg>();
            outbound_tx.insert(connection_id, tx);
            outbound_rx.insert(connection_id, rx);
        }

        for session_id in
            (next_session_id..next_session_id + config.num_sessions).map(SessionId::from)
        {
            let (tx, rx) = mpsc::unbounded_channel::<NetworkValue>();
            inbound_tx.insert(session_id, tx);
            inbound_rx.insert(session_id, rx);
        }

        sc.outbound_tx.insert(peer_id.clone(), outbound_tx);
        sc.outbound_rx.insert(peer_id.clone(), outbound_rx);
        sc.inbound_tx.insert(peer_id.clone(), inbound_tx);
        sc.inbound_rx.insert(peer_id.clone(), inbound_rx);
    }
    sc
}

async fn make_sessions_inner<T: NetworkConnection + 'static>(
    connections: PeerConnections<T>,
    connection_state: ConnectionState,
    config: &MpcConfig,
    next_session_id: u32,
    mut sc: SessionChannels,
) -> Vec<TcpSession> {
    let num_connections = config.num_connections;
    let num_sessions = config.num_sessions;

    // save a copy of peer_ids for session creation
    let peer_ids = connections.peer_ids();

    // spawn the forwarders
    for (peer_id, mut conns) in connections.into_iter() {
        for (idx, connection) in conns.drain(..).enumerate() {
            let connection_id = ConnectionId::from(idx as u32);
            let outbound_rx = sc
                .outbound_rx
                .get_mut(&peer_id)
                .unwrap()
                .remove(&connection_id)
                .unwrap();

            let inbound_forwarder = sc.inbound_tx.get(&peer_id).cloned().unwrap();
            let cs = connection_state.clone();

            tokio::spawn(multiplexer::run(
                connection,
                num_sessions,
                cs,
                inbound_forwarder,
                outbound_rx,
            ));
        }
    }

    // create the sessions
    let mut sessions = vec![];
    for (idx, session_id) in (next_session_id..next_session_id + num_sessions)
        .map(SessionId::from)
        .enumerate()
    {
        let mut tx = Vec::with_capacity(peer_ids.len());
        let mut rx = Vec::with_capacity(peer_ids.len());

        for peer_id in &peer_ids {
            // Rotate the starting connection per session for small messages;
            // large messages use this entire vector concurrently.
            let first_connection = idx as u32 % num_connections;
            let outbound_tx = (0..num_connections)
                .map(|offset| ConnectionId::from((first_connection + offset) % num_connections))
                .map(|connection_id| {
                    sc.outbound_tx
                        .get(peer_id)
                        .unwrap()
                        .get(&connection_id)
                        .cloned()
                        .unwrap()
                })
                .collect();
            tx.push(outbound_tx);
            let inbound_rx = sc
                .inbound_rx
                .get_mut(peer_id)
                .unwrap()
                .remove(&session_id)
                .unwrap();
            rx.push(inbound_rx);
        }

        // Create the TcpSession for this stream
        let session = TcpSession::new(session_id, peer_ids.clone(), tx, rx, config.clone());
        sessions.push(session);
    }

    sessions
}

#[cfg(test)]
mod tests {
    use super::*;
    use ampc_secret_sharing::shares::ring_impl::RingElement;
    use std::time::Duration;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

    fn test_session(
        connections: usize,
    ) -> (
        TcpSession,
        Identity,
        Vec<UnboundedReceiver<OutboundMsg>>,
        UnboundedSender<NetworkValue>,
    ) {
        let identity = Identity::from("peer");
        let mut tx = Vec::with_capacity(connections);
        let mut outbound_rx = Vec::with_capacity(connections);
        for _ in 0..connections {
            let (connection_tx, connection_rx) = mpsc::unbounded_channel();
            tx.push(connection_tx);
            outbound_rx.push(connection_rx);
        }
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let config = MpcConfig::new(Duration::from_secs(1), connections, 1);
        let session = TcpSession::new(
            SessionId::from(7),
            vec![identity.clone()],
            vec![tx],
            vec![inbound_rx],
            config,
        );
        (session, identity, outbound_rx, inbound_tx)
    }

    fn drain_fragments(
        outbound: &mut [UnboundedReceiver<OutboundMsg>],
    ) -> Vec<(u64, NetworkValue)> {
        let mut fragments = Vec::new();
        for connection in outbound {
            while let Ok((session_id, value)) = connection.try_recv() {
                assert_eq!(session_id, SessionId::from(7));
                let sequence = Fragment::decode(value.clone()).unwrap().sequence;
                fragments.push((sequence, value));
            }
        }
        fragments
    }

    #[tokio::test]
    async fn one_session_uses_all_connections_for_a_large_value() -> Result<()> {
        let (mut session, peer, mut outbound, _inbound) = test_session(4);
        let value = NetworkValue::VecRing16(vec![RingElement(42); 1_000_000]);
        session.send(value, &peer).await?;

        let fragments = drain_fragments(&mut outbound);
        assert_eq!(fragments.len(), 4);
        assert!(outbound.iter().all(UnboundedReceiver::is_empty));
        Ok(())
    }

    #[tokio::test]
    async fn out_of_order_stripes_preserve_logical_message_order() -> Result<()> {
        let (mut session, peer, mut outbound, inbound) = test_session(4);
        let first = NetworkValue::Bytes(vec![1; 1024 * 1024]);
        let second = NetworkValue::Bytes(vec![2; 1024 * 1024]);
        session.send(first.clone(), &peer).await?;
        session.send(second.clone(), &peer).await?;

        let fragments = drain_fragments(&mut outbound);
        assert_eq!(fragments.len(), 8);
        for (_, value) in fragments
            .iter()
            .filter(|(sequence, _)| *sequence == 1)
            .rev()
        {
            inbound.send(value.clone())?;
        }
        for (_, value) in fragments
            .iter()
            .filter(|(sequence, _)| *sequence == 0)
            .rev()
        {
            inbound.send(value.clone())?;
        }

        assert_eq!(session.receive(&peer).await?, first);
        assert_eq!(session.receive(&peer).await?, second);
        Ok(())
    }

    #[test]
    fn connection_count_is_not_clamped_to_session_count() {
        let config = MpcConfig::new(Duration::from_secs(1), 16, 1);
        assert_eq!(config.num_connections, 16);
        assert_eq!(config.get_sessions_for_connection(15), 1);
    }
}
