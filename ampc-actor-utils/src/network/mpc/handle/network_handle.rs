use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::execution::scheduler::parallelize;
use crate::network::tcp::{RuntimeTlsConfig, TlsConfig};
use crate::{
    execution::{
        player::{Identity, Role, RoleAssignment},
        session::{NetworkSession, Session},
    },
    network::{
        mpc::{
            handle::{
                config::MpcConfig,
                control_channel::{
                    ControlChannel, MeshControlChannel, TcpControlChannel, TcpMeshControlChannel,
                },
                data::PeerConnections,
                session::TcpSession,
            },
            value::NetworkValue,
            NetworkHandle, Networking,
        },
        tcp::{
            accept_loop, Client, ConnectionConfig, ConnectionId, ConnectionRequest,
            ConnectionState, NetworkConnection, Peer, Server,
        },
    },
    protocol::ops::setup_replicated_prf,
};
use async_trait::async_trait;
use eyre::{bail, eyre, Result};
use futures::future::join_all;
use itertools::Itertools;
use rand::{thread_rng, Rng};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_util::sync::CancellationToken;

pub struct MpcNetworkHandle<T: NetworkConnection + 'static, C: Client<Output = T> + 'static> {
    peers: Vec<Arc<Peer>>,
    my_id: Arc<Identity>,
    connector: Arc<C>,
    conn_cmd_tx: UnboundedSender<ConnectionRequest<T>>,
    shutdown_ct: CancellationToken,
    config: MpcConfig,
    next_session_id: u32,
    party_index: usize,
    role_assignments: Arc<RoleAssignment>,
}

impl<T: NetworkConnection + 'static, C: Client<Output = T> + 'static> Drop
    for MpcNetworkHandle<T, C>
{
    fn drop(&mut self) {
        self.shutdown_ct.cancel();
        tracing::debug!("TcpNetworkHandle dropped");
    }
}

#[async_trait]
impl<T: NetworkConnection + 'static, C: Client<Output = T> + 'static> NetworkHandle
    for MpcNetworkHandle<T, C>
{
    async fn make_network_sessions(&mut self) -> Result<(Vec<NetworkSession>, CancellationToken)> {
        let session_err_ct = CancellationToken::new();
        let drop_guard = session_err_ct.clone().drop_guard();
        let connection_state =
            ConnectionState::new(self.shutdown_ct.clone(), session_err_ct.clone());

        // wait for all peers to establish all connections
        let connections = self
            .make_peer_connections(self.config.num_connections, connection_state.clone())
            .await?;

        // calls multiplexer::run() on each TCP/TLS stream
        let mut tcp_sessions = super::session::make_sessions(
            connections,
            connection_state,
            &self.config,
            self.next_session_id,
        )
        .await;

        tokio::select! {
            r = self.validate_sessions(&mut tcp_sessions) =>  r,
            _ = self.shutdown_ct.cancelled() => Err(eyre!("shutdown_ct triggered")),
            _ = session_err_ct.cancelled() => Err(eyre!("session_err_ct triggered")),
        }?;

        let network_sessions: Vec<_> = tcp_sessions
            .into_iter()
            .map(|tcp_session| NetworkSession {
                session_id: tcp_session.id(),
                role_assignments: self.role_assignments.clone(),
                networking: Box::new(tcp_session),
                own_role: Role::new(self.party_index),
            })
            .collect();

        let sessions_per_conn = (0..self.config.num_connections)
            .map(|idx| self.config.get_sessions_for_connection(idx))
            .collect_vec();
        tracing::info!(
            "make_sessions succeeded. starting id: {} sessions per connection: {:?}",
            self.next_session_id,
            sessions_per_conn
        );

        self.next_session_id = self
            .next_session_id
            .saturating_add(self.config.num_sessions);

        // the `session` module assumes that session ids are sequential.
        // if there is not sufficient space to make num_sessions on the next
        // run, reset next_session_id to zero.
        if self.next_session_id >= u32::MAX - self.config.num_sessions {
            self.next_session_id = 0;
        }

        drop_guard.disarm();
        Ok((network_sessions, session_err_ct))
    }

    async fn make_sessions(&mut self) -> Result<(Vec<Session>, CancellationToken)> {
        let (network_sessions, ct) = self.make_network_sessions().await?;
        let drop_guard = ct.clone().drop_guard();

        let mut session_futures = vec![];
        for mut network_session in network_sessions.into_iter() {
            let fut = async move {
                let my_session_seed = thread_rng().gen();
                let prf = setup_replicated_prf(&mut network_session, my_session_seed).await?;
                Ok::<Session, eyre::Report>(Session {
                    network_session,
                    prf,
                })
            };
            session_futures.push(fut);
        }

        let r = parallelize(session_futures.into_iter()).await?;
        drop_guard.disarm();
        Ok((r, ct))
    }

    async fn control_channel(&mut self) -> Result<Box<dyn ControlChannel>> {
        // This is a ring (next/prev) API: it only has a well-defined meaning
        // for a 3-party configuration, where each party has exactly two
        // peers. For other party counts, use `mesh_control_channel` instead.
        if self.peers.len() != 2 {
            bail!(
                "control_channel() requires exactly 2 peers (a 3-party ring), found {}; \
                 use mesh_control_channel() instead",
                self.peers.len()
            );
        }

        let err_ct = CancellationToken::new();
        let drop_guard = err_ct.clone().drop_guard();
        let connection_state = ConnectionState::new(self.shutdown_ct.clone(), err_ct);

        // One connection per peer; peers[0] → conns[0][0], peers[1] → conns[1][0].
        let mut conns = self.make_connections(1, connection_state).await?;
        let stream_to_peer1 = conns
            .pop()
            .and_then(|mut c| c.pop())
            .ok_or_else(|| eyre!("no connection to peer 1"))?;
        let stream_to_peer0 = conns
            .pop()
            .and_then(|mut c| c.pop())
            .ok_or_else(|| eyre!("no connection to peer 0"))?;

        // Resolve which stream is "next" and which is "prev" using role assignments.
        let my_role = Role::new(self.party_index);
        let next_id = self
            .role_assignments
            .get(&my_role.next(self.role_assignments.len() as u8))
            .ok_or_else(|| eyre!("next role not found in role_assignments"))?;

        let (next_stream, prev_stream) = if self.peers[0].id() == next_id {
            (stream_to_peer0, stream_to_peer1)
        } else {
            (stream_to_peer1, stream_to_peer0)
        };

        drop_guard.disarm();
        Ok(Box::new(TcpControlChannel::new(
            next_stream,
            prev_stream,
            self.shutdown_ct.clone(),
        )))
    }

    async fn mesh_control_channel(&mut self) -> Result<Box<dyn MeshControlChannel>> {
        let err_ct = CancellationToken::new();
        let drop_guard = err_ct.clone().drop_guard();
        let connection_state = ConnectionState::new(self.shutdown_ct.clone(), err_ct);

        // One connection per peer.
        let conns = self.make_connections(1, connection_state).await?;
        let mut peer_streams: Vec<(Identity, T)> = self
            .peers
            .iter()
            .zip(conns)
            .map(|(peer, mut conn)| {
                let stream = conn
                    .pop()
                    .ok_or_else(|| eyre!("no connection to peer {:?}", peer.id()))?;
                Ok((peer.id().clone(), stream))
            })
            .collect::<Result<_>>()?;

        // Resolve each peer's Role via role_assignments (forward lookup by
        // .get(), not a scan over the HashMap, since iterating a HashMap's
        // order is not guaranteed to match across parties).
        let num_parties = self.role_assignments.len() as u8;
        let own_role = Role::new(self.party_index);
        let mut streams = BTreeMap::new();
        for idx in 0..num_parties {
            let role = Role::new(idx as usize);
            if role == own_role {
                continue;
            }
            let identity = self
                .role_assignments
                .get(&role)
                .ok_or_else(|| eyre!("role {role:?} not found in role_assignments"))?;
            let pos = peer_streams
                .iter()
                .position(|(id, _)| id == identity)
                .ok_or_else(|| {
                    eyre!("no connection established for role {role:?} ({identity:?})")
                })?;
            let (_, stream) = peer_streams.remove(pos);
            streams.insert(role, stream);
        }

        drop_guard.disarm();
        Ok(Box::new(TcpMeshControlChannel::new(
            streams,
            self.shutdown_ct.clone(),
        )))
    }
}

impl<T: NetworkConnection + 'static, C: Client<Output = T> + 'static> MpcNetworkHandle<T, C> {
    #[allow(clippy::too_many_arguments)]
    pub async fn new<I, S>(
        my_id: Identity,
        peers: I,
        connector: C,
        listener: S,
        config: MpcConfig,
        shutdown_ct: CancellationToken,
        party_index: usize,
        role_assignments: Arc<RoleAssignment>,
        tls_cfg: Option<TlsConfig>, // needed by the accept_loop to validate peers
    ) -> Result<Self>
    where
        I: Iterator<Item = (Identity, String)>,
        S: Server<Output = T> + 'static,
    {
        let peers: Vec<_> = peers.collect();
        let identities: Vec<_> = peers.iter().map(|(id, _address)| id.0.clone()).collect();
        let tls = build_runtime_tls_config(tls_cfg, &identities, party_index)?;

        let my_id = Arc::new(my_id);
        assert!(
            !peers.is_empty(),
            "expected at least 1 peer identity to build a NetworkHandle"
        );
        let peers: Vec<Arc<Peer>> = peers.into_iter().map(|p| Arc::new(p.into())).collect();

        // use the shutdown_ct to cancel anything spawned by the NetworkHandle. But don't want this to affect the calling code.
        // Hence the child_token()
        let shutdown_ct = shutdown_ct.child_token();
        let (conn_cmd_tx, conn_cmd_rx) = mpsc::unbounded_channel::<ConnectionRequest<T>>();

        // be sure not to make more than one network handle...
        tokio::spawn(accept_loop(listener, conn_cmd_rx, shutdown_ct.clone(), tls));

        Ok(Self {
            my_id,
            peers,
            connector: Arc::new(connector),
            config,
            conn_cmd_tx,
            shutdown_ct,
            next_session_id: 0,
            party_index,
            role_assignments,
        })
    }

    // associates the connections with an Identity
    async fn make_peer_connections(
        &self,
        conns_per_peer: u32,
        connection_state: ConnectionState,
    ) -> Result<PeerConnections<T>> {
        let conns = self
            .make_connections(conns_per_peer, connection_state)
            .await?;
        Ok(PeerConnections::new(self.peers.clone(), conns))
    }

    // returns the connections for each peer, grouped in the same order as
    // `self.peers` (i.e. result[i] holds the connections to peers[i]).
    // when returned, the handshakes have successfully completed
    async fn make_connections(
        &self,
        conns_per_peer: u32,
        connection_state: ConnectionState,
    ) -> Result<Vec<Vec<T>>> {
        assert!(!self.peers.is_empty());
        let mut connect_futures = Vec::with_capacity(conns_per_peer as usize * self.peers.len());

        for peer in self.peers.iter() {
            for idx in 0..conns_per_peer {
                let connection_id = ConnectionId::new(idx);
                let fut = crate::network::tcp::connect(
                    connection_id,
                    self.my_id.clone(),
                    connection_state.clone(),
                    ConnectionConfig::Bidirectional {
                        peer: peer.clone(),
                        client: self.connector.clone(),
                        conn_cmd_tx: self.conn_cmd_tx.clone(),
                    },
                );
                connect_futures.push(fut);
            }
        }
        let results: Vec<T> = join_all(connect_futures)
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;

        // results is ordered [peer0_conn0..peer0_connN, peer1_conn0..peer1_connN, ...];
        // regroup it into one Vec<T> per peer, in peer order.
        let conns_per_peer = conns_per_peer as usize;
        let mut results = results.into_iter();
        let per_peer: Vec<Vec<T>> = (0..self.peers.len())
            .map(|_| results.by_ref().take(conns_per_peer).collect())
            .collect();

        Ok(per_peer)
    }

    async fn validate_sessions(&self, sessions: &mut [TcpSession]) -> Result<()> {
        // make sure all the sessions are working
        for (idx, session) in sessions.iter_mut().enumerate() {
            // turn the session id into a byte array.
            let mut id_arr = [0u8; 16];
            let idx_bytes = idx.to_le_bytes();
            let len = std::cmp::min(id_arr.len(), idx_bytes.len());
            id_arr[..len].copy_from_slice(&idx_bytes[..len]);

            let prf = NetworkValue::PrfKey(id_arr);
            for peer in &self.peers {
                session.send(prf.clone(), peer.id()).await?;
            }
            for peer in &self.peers {
                let r = session.receive(peer.id()).await?;
                match r {
                    NetworkValue::PrfKey(arr) => {
                        assert_eq!(id_arr, arr);
                    }
                    _ => bail!("invalid msg received in validate_sessions()"),
                }
            }
        }
        Ok(())
    }
}

// tls_cfg.root_certs has all 3 parties root certs but NetworkHandleArgs.peers may only have 2 identites.
// need to use party_idx to filter out root_certs so that it matches the length of peers.
fn build_runtime_tls_config(
    tls_cfg: Option<TlsConfig>,
    peers: &[String],
    party_idx: usize,
) -> Result<Option<RuntimeTlsConfig>> {
    let Some(tls_cfg) = tls_cfg else {
        return Ok(None);
    };
    let mut cert_load_errors: Vec<String> = Vec::new();
    let mut peer_certs = HashMap::new();

    let mut root_certs = tls_cfg.root_certs.clone();
    if party_idx >= root_certs.len() {
        bail!("party idx mismatch with root certs");
    }
    root_certs.remove(party_idx);
    if root_certs.len() != peers.len() {
        bail!("mismatch between root certs and peer ids");
    }
    for (peer_id, root_cert_path) in peers.iter().zip(root_certs.into_iter()) {
        match CertificateDer::pem_file_iter(&root_cert_path) {
            Err(e) => {
                let err_msg = format!("failed to open cert file for peer {}: {}", peer_id, e);
                tracing::error!("{}", err_msg);
                cert_load_errors.push(err_msg);
            }
            Ok(mut iter) => match iter.next() {
                None => {
                    let err_msg =
                        format!("cert file for peer {} contains no certificates", peer_id);
                    tracing::error!("{}", err_msg);
                    cert_load_errors.push(err_msg);
                }
                Some(Err(e)) => {
                    let err_msg = format!("failed to parse cert for peer {}: {}", peer_id, e);
                    tracing::error!("{}", err_msg);
                    cert_load_errors.push(err_msg);
                }
                Some(Ok(cert)) => {
                    peer_certs.insert(peer_id.clone(), cert.as_ref().to_vec());
                }
            },
        }
    }

    if peer_certs.keys().len() != peers.len() {
        bail!(
            "failed to load certs for at least one peer. errors: {:?}",
            cert_load_errors
        );
    }

    Ok(Some(RuntimeTlsConfig {
        validate_peer_ids: true, // if we ever want to use ServerOnly TLS, this could be changed to !peer_certs.is_empty()
        peer_certs,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;

    use eyre::Result;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::task::JoinSet;
    use tokio_util::sync::CancellationToken;
    use tracing_test::traced_test;

    use crate::execution::local::generate_local_identities_n;
    use crate::network::tcp::ConnectionState;

    /// Establishes `num_parties` TCP handles (each connected to every other
    /// party) and has every party exchange raw bytes with every peer over
    /// every connection, to verify the N-peer connection-establishment path
    /// (not just the 3-party ring case).
    async fn run_tcp_network_handle_test(num_parties: u8) -> Result<()> {
        const CONNECTIONS_PER_PEER: u32 = 2;

        let identities = generate_local_identities_n(num_parties as usize);
        let handles = get_local_mpc_handles(identities, CONNECTIONS_PER_PEER as usize, 1).await?;
        let cs = ConnectionState::new(CancellationToken::new(), CancellationToken::new());

        // for each handle (party), one Vec<TcpStreamConn> per peer, in the
        // same order as that party's `self.peers` (i.e. global index order,
        // skipping its own index).
        let connections = futures::future::join_all(
            handles
                .iter()
                .map(|h| h.make_connections(CONNECTIONS_PER_PEER, cs.clone())),
        )
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;

        let mut jobs = JoinSet::new();
        let peer_ids: Vec<u8> = (0..num_parties).collect();

        tracing::debug!("connections created. sending data");

        for (peer_idx, per_peer_conns) in connections.into_iter().enumerate() {
            let other_ids: Vec<u8> = peer_ids
                .iter()
                .copied()
                .filter(|id| *id != peer_idx as u8)
                .collect();
            assert_eq!(per_peer_conns.len(), other_ids.len());

            for (other_id, conns) in other_ids.into_iter().zip(per_peer_conns) {
                for (conn_idx, mut c) in conns.into_iter().enumerate() {
                    // send the neighbour's own id, marked with the
                    // connection index; the neighbour does the same in
                    // reverse, so what comes back is this party's own id.
                    let data = [other_id, conn_idx as u8];
                    jobs.spawn(async move {
                        c.write_all(&data).await.unwrap();
                        c.flush().await.unwrap();

                        let mut recv_data = [0u8; 2];
                        c.read_exact(&mut recv_data).await.unwrap();
                        assert_eq!(&recv_data, &[peer_idx as u8, conn_idx as u8]);
                    });
                }
            }
        }

        jobs.join_all().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[traced_test]
    async fn test_tcp_network_handle() -> Result<()> {
        run_tcp_network_handle_test(3).await
    }

    #[tokio::test(flavor = "multi_thread")]
    #[traced_test]
    async fn test_tcp_network_handle_five_parties() -> Result<()> {
        run_tcp_network_handle_test(5).await
    }
}
