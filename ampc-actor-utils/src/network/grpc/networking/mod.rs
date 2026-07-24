use crate::{
    execution::{
        local::{generate_local_identities, get_free_local_addresses},
        player::Identity,
        session::{SessionId, StreamId},
    },
    proto_generated::party_node::{
        party_node_client::PartyNodeClient, party_node_server::PartyNodeServer, SendRequests,
    },
};
use backon::{ExponentialBuilder, Retryable};
use eyre::{bail, eyre, Result};
use futures::future::JoinAll;
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    time::Duration,
};
use tonic::{
    transport::{Channel, Endpoint, Server},
    Streaming,
};

use super::handle::GrpcHandle;
use super::{GrpcConfig, InStream, InStreams, OutStream, OutStreams};

mod stream_manager;
use stream_manager::StreamManager;

/// Large HTTP-2 flow-control window. The MPC data plane keeps ~1 message per
/// session in flight, so backpressure is provided by the OS socket buffer; we
/// size the window generously so HTTP-2 flow control never engages (see the
/// "large flow-control windows" note in the idealized gRPC design).
pub(crate) const GRPC_WINDOW_SIZE: u32 = 16 * 1024 * 1024;

// WARNING: this implementation assumes that messages for a specific player
// within one session are sent in order and consecutively. Don't send messages
// to the same player in parallel within the same session. Use batching instead.
pub struct GrpcNetworking {
    party_id: Identity,
    // other party id -> client to call that party
    clients: HashMap<Identity, Vec<PartyNodeClient<Channel>>>,
    // session_id -> incoming streams
    inbound_sessions: HashMap<SessionId, InStreams>,
    // sessions in use
    // TODO: deletion logic
    active_sessions: HashSet<SessionId>,
    // creates outbound gRPC streams and multiplexes sessions over them
    sm: StreamManager,

    config: GrpcConfig,
}

impl GrpcNetworking {
    pub fn new(party_id: Identity, config: GrpcConfig) -> Self {
        GrpcNetworking {
            party_id,
            clients: HashMap::new(),
            inbound_sessions: HashMap::new(),
            active_sessions: HashSet::new(),
            sm: StreamManager::new(config.clone()),
            config,
        }
    }

    pub fn party_id(&self) -> Identity {
        self.party_id.clone()
    }

    pub fn config(&self) -> GrpcConfig {
        self.config.clone()
    }

    // TODO: from config?
    fn backoff(&self) -> ExponentialBuilder {
        ExponentialBuilder::new()
            .with_min_delay(std::time::Duration::from_millis(500))
            .with_factor(1.1)
            .with_max_delay(std::time::Duration::from_secs(5))
            .with_max_times(27) // about 60 seconds overall delay
    }

    pub async fn connect_to_party(&mut self, party_id: Identity, address: &str) -> Result<()> {
        if self.clients.contains_key(&party_id) {
            bail!(
                "{:?} has already connected to {:?}",
                self.party_id,
                party_id
            );
        }
        // Configure a generous HTTP-2 flow-control window so it never throttles the
        // 1000-session fan-in (see GRPC_WINDOW_SIZE).
        let endpoint = Endpoint::from_shared(address.to_string())?
            .initial_stream_window_size(Some(GRPC_WINDOW_SIZE))
            .initial_connection_window_size(Some(GRPC_WINDOW_SIZE));
        let clients = (0..self.config.connection_parallelism.max(1))
            .map(|_| {
                let endpoint = endpoint.clone();
                (move || {
                    let endpoint = endpoint.clone();
                    async move {
                        Ok::<_, tonic::transport::Error>(PartyNodeClient::new(
                            endpoint.connect().await?,
                        ))
                    }
                })
                .retry(self.backoff())
                .sleep(tokio::time::sleep)
            })
            .map(tokio::spawn)
            .collect::<JoinAll<_>>()
            .await
            .into_iter()
            .collect::<Result<Result<Vec<PartyNodeClient<_>>, _>, _>>()??;
        tracing::trace!(
            "{:?} connected to {:?} at address {:?}",
            self.party_id,
            party_id,
            address
        );
        self.clients.insert(party_id.clone(), clients);
        Ok(())
    }

    // adds a session to a stream, and creates a new stream if needed
    pub async fn create_outgoing_streams(&mut self, session_id: SessionId) -> Result<OutStreams> {
        self.sm
            .add_session(&self.party_id, &self.clients, session_id)
    }

    pub fn is_session_ready(&self, session_id: SessionId) -> bool {
        let n_senders = match self.inbound_sessions.get(&session_id) {
            None => 0,
            Some(q) => q.len(),
        };

        n_senders == self.clients.len()
    }

    pub async fn obtain_incoming_streams(&mut self, session_id: SessionId) -> Result<InStreams> {
        self.active_sessions.insert(session_id);
        self.inbound_sessions
            .remove(&session_id)
            .ok_or(eyre!(format!(
                "{session_id:?} hasn't been added to message queues"
            )))
    }
}

// Server implementation
impl GrpcNetworking {
    pub async fn start_message_stream(
        &mut self,
        sender_id: Identity,
        stream_id: StreamId,
        mut stream: Streaming<SendRequests>,
        session_forwarder: HashMap<u32, OutStream>,
        mut inbound_sessions: HashMap<SessionId, InStream>,
    ) -> Result<()> {
        if sender_id == self.party_id {
            bail!("Sender ID coincides with receiver ID: {:?}", sender_id);
        }

        for (session_id, stream) in inbound_sessions.drain() {
            if self
                .inbound_sessions
                .entry(session_id)
                .or_default()
                .insert(sender_id.clone(), stream)
                .is_some()
            {
                tracing::error!(
                    "duplicate session id {} on stream {} from sender {:?}",
                    session_id.0,
                    stream_id.0,
                    sender_id
                );
            }
        }

        // logging here to avoid a clone.
        tracing::debug!(
            "{:?} has added incoming stream  {:?} from {:?}",
            self.party_id,
            stream_id,
            sender_id
        );

        // Direct-fanout ingress: read the tonic `Streaming` straight off the h2
        // driver and dispatch each message to its per-session channel. No
        // intermediate relay task/channel (see the idealized gRPC design).
        tokio::spawn(async move {
            loop {
                match stream.message().await {
                    Ok(Some(msg)) => {
                        for request in msg.requests {
                            let session_id = request.session_id;
                            if let Some(tx) = session_forwarder.get(&session_id) {
                                if let Err(e) = tx.send(request) {
                                    tracing::error!(
                                        "Failed to forward message for session {:?}: {:?}",
                                        session_id,
                                        e
                                    );
                                }
                            } else {
                                tracing::error!(
                                    "{:?} sent message with invalid session id {:?} on stream {:?}",
                                    sender_id,
                                    session_id,
                                    stream_id
                                );
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::error!(
                            "inbound stream {:?} from {:?} errored: {:?}",
                            stream_id,
                            sender_id,
                            e
                        );
                        break;
                    }
                }
            }
        });

        Ok(())
    }
}

pub async fn setup_local_grpc_networking(
    parties: Vec<Identity>,
    connection_parallelism: usize,
    request_parallelism: usize,
) -> Result<Vec<GrpcHandle>> {
    let config = GrpcConfig {
        timeout_duration: Duration::from_secs(5),
        connection_parallelism,
        request_parallelism,
    };

    let nets = parties
        .iter()
        .map(|party| GrpcNetworking::new(party.clone(), config.clone()))
        .collect::<Vec<GrpcNetworking>>();

    // Create handles consecutively to preserve the order of players
    let mut players = Vec::with_capacity(nets.len());
    for net in nets {
        players.push(GrpcHandle::new(net).await?);
    }

    let addresses = get_free_local_addresses(players.len()).await?;

    let players_addresses = players
        .iter()
        .cloned()
        .zip(addresses.iter().cloned())
        .collect::<Vec<_>>();

    // Initialize servers
    for (player, addr) in &players_addresses {
        let player = player.clone();
        let socket = addr.parse().unwrap();
        tokio::spawn(async move {
            Server::builder()
                .initial_stream_window_size(Some(GRPC_WINDOW_SIZE))
                .initial_connection_window_size(Some(GRPC_WINDOW_SIZE))
                .add_service(PartyNodeServer::new(player))
                .serve(socket)
                .await
                .unwrap();
        });
    }

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Connect to each other
    for (player, addr) in &players_addresses {
        for (other_player, other_addr) in &players_addresses.clone() {
            if addr != other_addr {
                let other_addr = format!("http://{}", other_addr);
                player
                    .connect_to_party(other_player.party_id(), &other_addr)
                    .await
                    .unwrap();
            }
        }
    }

    tracing::debug!("Players connected to each other");

    Ok(players)
}

/// Arguments for [`build_network_handle`].
pub struct GrpcNetworkHandleArgs {
    pub party_index: usize,
    /// Listen address for every party, indexed by party index (`host:port`).
    pub addresses: Vec<String>,
    /// Dial address for every party, indexed by party index (`host:port`).
    /// Separate from `addresses` so a proxy can be inserted between parties.
    pub outbound_addresses: Vec<String>,
    /// Number of gRPC connections to open to each peer.
    pub connection_parallelism: usize,
    /// Total number of application-level sessions. The number of gRPC streams is
    /// derived as `request_parallelism / connection_parallelism` (one stream per
    /// connection).
    pub request_parallelism: usize,
    /// How long `receive` waits for a message before timing out.
    pub timeout_duration: Duration,
}

/// Build a gRPC network handle for a single party: start its server and connect
/// to every peer. This is the gRPC analogue of the MPC `build_network_handle`.
pub async fn build_network_handle(args: GrpcNetworkHandleArgs) -> Result<GrpcHandle> {
    let identities = generate_local_identities();

    let config = GrpcConfig {
        timeout_duration: args.timeout_duration,
        connection_parallelism: args.connection_parallelism,
        request_parallelism: args.request_parallelism,
    };

    let my_index = args.party_index;
    let my_identity = identities[my_index].clone();

    let net = GrpcNetworking::new(my_identity.clone(), config);
    let handle = GrpcHandle::new(net).await?;

    // Start this party's gRPC server. Each handle is also its own gRPC server,
    // living in a detached task.
    let socket: SocketAddr = args.addresses[my_index].parse()?;
    let server_handle = handle.clone();
    let server_identity = my_identity.clone();
    tokio::spawn(async move {
        if let Err(e) = Server::builder()
            .initial_stream_window_size(Some(GRPC_WINDOW_SIZE))
            .initial_connection_window_size(Some(GRPC_WINDOW_SIZE))
            .add_service(PartyNodeServer::new(server_handle))
            .serve(socket)
            .await
        {
            tracing::error!("gRPC server for {:?} exited: {:?}", server_identity, e);
        }
    });

    // Connect to every other party.
    for (idx, (identity, address)) in identities
        .iter()
        .zip(args.outbound_addresses.iter())
        .enumerate()
    {
        if idx == my_index {
            continue;
        }
        let address = format!("http://{address}");
        handle.connect_to_party(identity.clone(), &address).await?;
    }

    Ok(handle)
}
