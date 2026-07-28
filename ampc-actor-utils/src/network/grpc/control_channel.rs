use eyre::Result;
use tonic::async_trait;

use crate::{
    execution::player::Identity,
    network::mpc::{handle::control_channel::ControlChannel, NetworkValue, Networking},
};

use super::session::GrpcSession;

/// [`ControlChannel`] implementation over a dedicated [`GrpcSession`].
///
/// The session is created by
/// [`crate::network::mpc::NetworkHandle::control_channel`] on a session id past
/// the range the data plane reserves, so it lands on its own gRPC stream and
/// never shares a coalescing buffer with data-plane traffic.
///
/// Two behaviours differ from
/// [`crate::network::mpc::handle::control_channel::TcpControlChannel`]:
///
/// - A send hands the value to the stream's egress channel rather than blocking
///   until the bytes are flushed, because gRPC gives no per-message flush ack.
///   Ordering still holds — the stream is dedicated to this channel and tonic
///   polls it in order — and `recv_*` still blocks on a real delivery.
/// - A receive gives up after `GrpcConfig::timeout_duration` instead of blocking
///   indefinitely, since that timeout is baked into `GrpcSession::receive`.
pub(crate) struct GrpcControlChannel {
    session: GrpcSession,
    next_id: Identity,
    prev_id: Identity,
}

impl GrpcControlChannel {
    pub(super) fn new(session: GrpcSession, next_id: Identity, prev_id: Identity) -> Self {
        Self {
            session,
            next_id,
            prev_id,
        }
    }
}

#[async_trait]
impl ControlChannel for GrpcControlChannel {
    async fn send_next(&mut self, value: NetworkValue) -> Result<()> {
        self.session.send(value, &self.next_id).await
    }

    async fn send_prev(&mut self, value: NetworkValue) -> Result<()> {
        self.session.send(value, &self.prev_id).await
    }

    async fn recv_next(&mut self) -> Result<NetworkValue> {
        self.session.receive(&self.next_id).await
    }

    async fn recv_prev(&mut self) -> Result<NetworkValue> {
        self.session.receive(&self.prev_id).await
    }
}
