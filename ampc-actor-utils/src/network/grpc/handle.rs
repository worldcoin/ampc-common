use crate::{
    execution::{
        local::generate_local_identities,
        player::{Identity, Role, RoleAssignment},
        scheduler::parallelize,
        session::{NetworkSession, Session, SessionId, StreamId},
    },
    network::mpc::{handle::control_channel::ControlChannel, NetworkHandle, NetworkValue},
    proto_generated::party_node::party_node_server::PartyNode,
    protocol::ops::setup_replicated_prf,
};
use eyre::{bail, eyre, Result};
use rand::{thread_rng, Rng};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, Mutex},
    time::sleep,
};
use tokio_util::sync::CancellationToken;
use tonic::{async_trait, Request, Response, Status, Streaming};

use super::messages::{SendRequests, SendResponse};
use super::networking::GrpcNetworking;
use super::session::GrpcSession;
use super::{GrpcConfig, InStream, InStreams, InboundSender, OutStreams, TonicResult};

struct ConnectToPartyTask {
    party_id: Identity,
    address: String,
}

enum GrpcTask {
    ConnectToParty(ConnectToPartyTask),
    CreateOutgoingStreams(SessionId),
    ObtainIncomingStreams(SessionId),
    IsSessionReady(SessionId),
}

enum MessageResult {
    Empty,
    IsSessionReady(bool),
    OutgoingStreams(OutStreams),
    IncomingStreams(InStreams),
}

struct MessageJob {
    task: GrpcTask,
    return_channel: oneshot::Sender<Result<MessageResult>>,
}

// Concurrency handler for networking operations
#[derive(Clone)]
pub struct GrpcHandle {
    grpc: Arc<Mutex<GrpcNetworking>>,
    job_queue: mpsc::Sender<MessageJob>,
    party_id: Identity,
    config: GrpcConfig,
    /// Guards the one-shot `NetworkHandle::make_network_sessions`. Shared across
    /// clones so a cloned handle cannot re-create the same session ids.
    sessions_created: Arc<AtomicBool>,
}

impl GrpcHandle {
    pub async fn new(grpc: GrpcNetworking) -> Result<Self> {
        let party_id = grpc.party_id();
        let config = grpc.config();
        let grpc = Arc::new(Mutex::new(grpc));
        let (tx, rx) = tokio::sync::mpsc::channel::<MessageJob>(1);

        // Loop to handle incoming tasks from job queue
        {
            let grpc = grpc.clone();
            tokio::spawn(async move {
                let mut rx = rx;
                while let Some(job) = rx.recv().await {
                    match job.task {
                        GrpcTask::CreateOutgoingStreams(session_id) => {
                            let mut grpc = grpc.lock().await;
                            let job_result = grpc
                                .create_outgoing_streams(session_id)
                                .await
                                .map(MessageResult::OutgoingStreams);
                            let _ = job.return_channel.send(job_result);
                        }
                        GrpcTask::IsSessionReady(session_id) => {
                            let grpc = grpc.lock().await;
                            let job_result = Ok(MessageResult::IsSessionReady(
                                grpc.is_session_ready(session_id),
                            ));
                            let _ = job.return_channel.send(job_result);
                        }
                        GrpcTask::ConnectToParty(task) => {
                            let mut grpc = grpc.lock().await;
                            let job_result = grpc
                                .connect_to_party(task.party_id, &task.address)
                                .await
                                .map(|_| MessageResult::Empty);
                            let _ = job.return_channel.send(job_result);
                        }
                        GrpcTask::ObtainIncomingStreams(session_id) => {
                            let mut grpc = grpc.lock().await;
                            let job_result = grpc
                                .obtain_incoming_streams(session_id)
                                .await
                                .map(MessageResult::IncomingStreams);
                            let _ = job.return_channel.send(job_result);
                        }
                    }
                }
            });
        }

        Ok(GrpcHandle {
            grpc,
            party_id,
            job_queue: tx,
            config,
            sessions_created: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn party_id(&self) -> Identity {
        self.party_id.clone()
    }

    // Send a task to the job queue and wait for the result
    async fn submit(&self, task: GrpcTask) -> Result<MessageResult> {
        let (tx, rx) = oneshot::channel();
        let job = MessageJob {
            task,
            return_channel: tx,
        };
        self.job_queue.send(job).await?;
        rx.await?
    }
}

// Server implementation
#[async_trait]
impl PartyNode for GrpcHandle {
    async fn start_message_stream(
        &self,
        request: Request<Streaming<SendRequests>>,
    ) -> TonicResult<Response<SendResponse>> {
        let sender_id: Identity = request
            .metadata()
            .get("sender_id")
            .ok_or(Status::unauthenticated("Sender ID not found"))?
            .to_str()
            .map_err(|_| Status::unauthenticated("Sender ID is not a string"))?
            .to_string()
            .into();
        let stream_id: u32 = request
            .metadata()
            .get("stream_id")
            .ok_or(Status::not_found("Stream ID not found"))?
            .to_str()
            .map_err(|_| Status::not_found("Stream ID malformed"))?
            .parse()
            .map_err(|_| Status::invalid_argument("Stream ID is not a u32 number"))?;
        let stream_id = StreamId::from(stream_id);

        let incoming_stream = request.into_inner();

        tracing::debug!(
            "Player {:?} is starting message stream with player {:?} in stream {:?}",
            self.party_id,
            sender_id,
            stream_id.0
        );

        // create channels for the sessions
        let mut inbound_forwarder: HashMap<u32, InboundSender> = HashMap::new();
        let mut inbound_sessions: HashMap<SessionId, InStream> = HashMap::new();
        let start_id = stream_id.0 * self.config.stream_parallelism() as u32;
        for session_id in start_id..start_id + self.config.stream_parallelism() as u32 {
            let (hawk_tx, hawk_rx) = mpsc::unbounded_channel::<NetworkValue>();
            inbound_forwarder.insert(session_id, hawk_tx);
            inbound_sessions.insert(SessionId::from(session_id), hawk_rx);
        }

        let grpc = self.grpc.clone();
        let sender_id_clone = sender_id.clone();
        tokio::spawn(async move {
            let mut grpc = grpc.lock().await;
            let _ = grpc
                .start_message_stream(
                    sender_id,
                    stream_id,
                    incoming_stream,
                    inbound_forwarder,
                    inbound_sessions,
                )
                .await;
        });

        tracing::debug!(
            "Player {:?} has started message stream with player {:?} in stream {:?}",
            self.party_id,
            sender_id_clone,
            stream_id.0
        );

        Ok(Response::new(SendResponse {}))
    }
}

// Connection and session management
impl GrpcHandle {
    pub async fn connect_to_party(&self, party_id: Identity, address: &str) -> Result<()> {
        let task = ConnectToPartyTask {
            party_id,
            address: address.to_string(),
        };
        let task = GrpcTask::ConnectToParty(task);
        let _ = self.submit(task).await?;
        Ok(())
    }

    pub async fn create_session(&self, session_id: SessionId) -> Result<GrpcSession> {
        // Create outgoing streams and ask other parties to send incoming streams
        let task = GrpcTask::CreateOutgoingStreams(session_id);
        let res = self.submit(task).await?;
        let outstreams = match res {
            MessageResult::OutgoingStreams(streams) => Ok(streams),
            _ => Err(eyre!("Wrong result type while creating outgoing streams")),
        }?;

        // Wait for incoming streams to be created and sent by other parties
        self.wait_for_session(session_id).await?;

        // Fetch incoming streams from GrpcNetworking
        let task = GrpcTask::ObtainIncomingStreams(session_id);
        let res = self.submit(task).await?;
        let instreams = match res {
            MessageResult::IncomingStreams(streams) => Ok(streams),
            _ => Err(eyre!("Wrong result type while creating incoming streams")),
        }?;

        Ok(GrpcSession {
            session_id,
            own_identity: self.party_id.clone(),
            out_streams: outstreams,
            in_streams: instreams,
            config: self.config.clone(),
        })
    }

    // This function should be called after all parties have called `create_session`
    pub async fn wait_for_session(&self, session_id: SessionId) -> Result<()> {
        while matches!(
            self.submit(GrpcTask::IsSessionReady(session_id)).await?,
            MessageResult::IsSessionReady(false)
        ) {
            tracing::debug!(
                "Player {:?} is waiting for session {:?} to be ready",
                self.party_id,
                session_id
            );
            sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    }
}

/// gRPC session ids are structural: the server maps an incoming stream to the
/// session id range `stream_id * stream_parallelism .. + stream_parallelism`, so
/// the session ids a party may create are fixed at `0..request_parallelism`. That
/// makes `make_network_sessions` one-shot — a second call would re-create ids that
/// already exist and block forever waiting on peers, so it returns an error
/// instead. Reconnecting means building a new handle.
///
/// Unlike the MPC handle there is no session error token: each handle is also its
/// own gRPC server living in a detached task, so nothing is cancelled when the
/// sessions are dropped. The returned `CancellationToken` is inert.
#[async_trait]
impl NetworkHandle for GrpcHandle {
    async fn make_network_sessions(&mut self) -> Result<(Vec<NetworkSession>, CancellationToken)> {
        if self.sessions_created.swap(true, Ordering::SeqCst) {
            bail!("make_network_sessions may only be called once per GrpcHandle");
        }

        let identities = generate_local_identities();
        let own_role = identities
            .iter()
            .position(|id| *id == self.party_id)
            .map(Role::new)
            .ok_or_else(|| eyre!("{:?} is not one of the local identities", self.party_id))?;
        let role_assignments: Arc<RoleAssignment> = Arc::new(
            identities
                .iter()
                .enumerate()
                .map(|(idx, id)| (Role::new(idx), id.clone()))
                .collect(),
        );

        // Every party creates the same id set concurrently; `create_session`
        // returns only once all peers have created the matching id, so this is
        // the connect rendezvous.
        let num_sessions = self.config.request_parallelism;
        let mut tasks = Vec::with_capacity(num_sessions);
        for idx in 0..num_sessions as u32 {
            let handle = self.clone();
            tasks.push(tokio::spawn(async move {
                handle.create_session(SessionId::from(idx)).await
            }));
        }

        let mut network_sessions = Vec::with_capacity(num_sessions);
        for task in tasks {
            let session = task.await??;
            network_sessions.push(NetworkSession {
                session_id: session.session_id,
                role_assignments: role_assignments.clone(),
                networking: Box::new(session),
                own_role,
            });
        }

        tracing::info!(
            "make_network_sessions succeeded for {:?}: {} sessions",
            self.party_id,
            network_sessions.len()
        );

        Ok((network_sessions, CancellationToken::new()))
    }

    async fn make_sessions(&mut self) -> Result<(Vec<Session>, CancellationToken)> {
        let (network_sessions, ct) = self.make_network_sessions().await?;

        let mut session_futures = vec![];
        for mut network_session in network_sessions.into_iter() {
            session_futures.push(async move {
                let my_session_seed = thread_rng().gen();
                let prf = setup_replicated_prf(&mut network_session, my_session_seed).await?;
                Ok::<Session, eyre::Report>(Session {
                    network_session,
                    prf,
                })
            });
        }

        let sessions = parallelize(session_futures.into_iter()).await?;
        Ok((sessions, ct))
    }

    async fn control_channel(&mut self) -> Result<Box<dyn ControlChannel>> {
        bail!("control_channel is not implemented for the gRPC network handle")
    }
}
