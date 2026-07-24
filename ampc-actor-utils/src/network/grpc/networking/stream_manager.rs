use crate::{
    execution::{
        player::Identity,
        session::{SessionId, StreamId},
    },
    proto_generated::party_node::{party_node_client::PartyNodeClient, SendRequest, SendRequests},
};
use eyre::{eyre, Result};
use futures::Stream;
use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tonic::{metadata::AsciiMetadataValue, transport::Channel, Request, Status};

use super::super::{GrpcConfig, OutStream, OutStreams};

/// Maximum coalesced payload per batch. gRPC caps a message at 4 MiB; stay well
/// under that.
const MAX_COALESCED_PAYLOAD: usize = 1 << 21;

/// A `Stream` that coalesces many ready per-session messages into a single
/// `SendRequests` batch. It is handed directly to tonic and polled inside the
/// HTTP-2 connection task, so coalescing happens with no intermediate egress
/// channel or dedicated task (see the idealized gRPC design).
struct CoalescingStream {
    rx: UnboundedReceiver<SendRequest>,
    stream_parallelism: usize,
}

impl Stream for CoalescingStream {
    type Item = SendRequests;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.rx.poll_recv(cx) {
            Poll::Ready(Some(first)) => {
                let mut payload_len = first.data.len();
                let mut requests = vec![first];
                // Drain messages that are already queued into the same batch.
                while requests.len() != this.stream_parallelism {
                    match this.rx.poll_recv(cx) {
                        Poll::Ready(Some(msg)) => {
                            payload_len += msg.data.len();
                            requests.push(msg);
                            if payload_len >= MAX_COALESCED_PAYLOAD {
                                break;
                            }
                        }
                        // Nothing more ready (or the channel closed): flush now.
                        _ => break,
                    }
                }
                Poll::Ready(Some(SendRequests { requests }))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Default)]
pub struct StreamManager {
    established_sessions: HashSet<SessionId>,
    established_streams: HashSet<StreamId>,
    stream_channels: HashMap<Identity, HashMap<StreamId, OutStream>>,
    config: GrpcConfig,
}

impl StreamManager {
    pub fn new(config: GrpcConfig) -> Self {
        Self {
            config,
            ..Default::default()
        }
    }

    // many tasks may try to create sessions at the same time.
    // assume that the tasks will be given the correct session ids (no duplicates with range from [0..n_sessions))
    pub fn add_session(
        &mut self,
        party_id: &Identity,
        clients: &HashMap<Identity, Vec<PartyNodeClient<Channel>>>,
        session_id: SessionId,
    ) -> Result<OutStreams> {
        if !self.established_sessions.insert(session_id) {
            return Err(eyre!(
                "{:?} has already been created by {:?}",
                session_id,
                party_id
            ));
        }

        let stream_id = StreamId::from(session_id.0 / self.config.stream_parallelism as u32);
        if self.established_streams.insert(stream_id) {
            self.add_stream(party_id.clone(), clients, stream_id)?;
        }

        let mut out_streams = HashMap::new();
        for (client_id, stream_map) in self.stream_channels.iter() {
            let tx = stream_map
                .get(&stream_id)
                .ok_or(eyre!(
                    "failed to get stream id {} for {:?}",
                    stream_id.0,
                    client_id
                ))?
                .clone();
            out_streams.insert(client_id.clone(), tx);
        }

        Ok(out_streams)
    }

    fn add_stream(
        &mut self,
        party_id: Identity,
        clients: &HashMap<Identity, Vec<PartyNodeClient<Channel>>>,
        stream_id: StreamId,
    ) -> Result<()> {
        tracing::debug!(
            "{:?} is adding a stream to {} clients",
            party_id,
            clients.len()
        );

        let stream_parallelism = self.config.stream_parallelism;
        for (client_id, clients) in clients.iter() {
            let round_robin = (stream_id.0 as usize) % clients.len();
            let mut client = clients[round_robin].clone();

            let (hawk_tx, hawk_rx) = mpsc::unbounded_channel::<SendRequest>();
            // The coalescing stream is handed straight to tonic; it batches ready
            // messages inside the h2 task with no extra egress channel or task.
            let coalescing_stream = CoalescingStream {
                rx: hawk_rx,
                stream_parallelism,
            };
            let mut request = Request::new(coalescing_stream);
            request.metadata_mut().insert(
                "sender_id",
                AsciiMetadataValue::from_str(&party_id.0)
                    .map_err(|e| eyre!("Failed to convert Sender ID to ASCII: {e}"))?,
            );
            request.metadata_mut().insert(
                "stream_id",
                AsciiMetadataValue::from_str(&stream_id.0.to_string())
                    .map_err(|e| eyre!("Failed to convert Stream ID to ASCII: {e}"))?,
            );

            tokio::spawn(async move {
                let _response = client.start_message_stream(request).await?;
                Ok::<_, Status>(())
            });

            self.stream_channels
                .entry(client_id.clone())
                .or_default()
                .insert(stream_id, hawk_tx);
        }
        Ok(())
    }
}
