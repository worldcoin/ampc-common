use crate::{execution::player::Identity, network::mpc::NetworkValue};
use eyre::Result;
use std::{collections::HashMap, time::Duration};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tonic::Status;

// `pub(crate)` so the tonic-generated client/server (in `crate::proto_generated`)
// can reach `RawCodec`/the message types via the `codec_path` and `extern_path`
// settings in `build.rs`.
pub(crate) mod codec;
mod control_channel;
mod handle;
pub(crate) mod messages;
mod networking;
mod session;

use self::messages::SendRequest;

#[allow(unused_imports)]
pub use handle::*;
pub use networking::*;

type TonicResult<T> = Result<T, Status>;

fn err_to_status(e: eyre::Error) -> Status {
    Status::internal(e.to_string())
}

// Egress: session -> coalescing stream -> codec. Carries values unserialized.
type OutStream = UnboundedSender<SendRequest>;
type OutStreams = HashMap<Identity, OutStream>;
// Ingress: fanout -> session. The codec has already deserialized, so a decoded
// `NetworkValue` (no session tag needed — the fanout demuxed by session id).
type InStream = UnboundedReceiver<NetworkValue>;
type InStreams = HashMap<Identity, InStream>;
// Sender half feeding a session's inbound queue (paired with `InStream`).
type InboundSender = UnboundedSender<NetworkValue>;

#[derive(Default, Clone, Debug)]
pub struct GrpcConfig {
    pub timeout_duration: Duration,
    // number of gRPC connections to create
    pub connection_parallelism: usize,
    // total number of application-level sessions across all streams
    pub request_parallelism: usize,
}

impl GrpcConfig {
    /// Number of application-level sessions multiplexed onto a single gRPC
    /// stream. Derived so there is exactly one stream per connection:
    /// `request_parallelism / connection_parallelism`. Keep `request_parallelism`
    /// a multiple of `connection_parallelism` for a clean 1:1 mapping.
    pub fn stream_parallelism(&self) -> usize {
        (self.request_parallelism / self.connection_parallelism.max(1)).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::{session::GrpcSession, *};
    use crate::{
        execution::{local::generate_local_identities, player::Role, session::SessionId},
        network::mpc::{
            handle::control_channel::ControlChannel, NetworkHandle, NetworkType, NetworkValue,
            Networking,
        },
    };
    use futures::future::join_all;
    use rand::Rng;
    use tokio::{task::JoinSet, time::sleep};
    use tracing_test::traced_test;

    // can only send NetworkValue over the network. PrfKey is easy to make so this is used here.
    fn get_prf() -> NetworkValue {
        let mut rng = rand::thread_rng();
        let mut key = [0u8; 16];
        rng.fill(&mut key);
        NetworkValue::PrfKey(key)
    }

    async fn create_session_helper(
        session_id: SessionId,
        players: &[GrpcHandle],
    ) -> Result<Vec<GrpcSession>> {
        let mut jobs = vec![];
        for player in players.iter() {
            let player = player.clone();
            let task = tokio::spawn(async move {
                tracing::trace!(
                    "Player {:?} is creating session {:?}",
                    player.party_id(),
                    session_id
                );
                player.create_session(session_id).await.unwrap()
            });
            jobs.push(task);
        }
        join_all(jobs)
            .await
            .into_iter()
            .map(|r| r.map_err(eyre::Report::new))
            .collect::<Result<Vec<_>>>()
    }

    #[tokio::test(flavor = "multi_thread")]
    #[traced_test]
    async fn test_grpc_comms_correct() -> Result<()> {
        let identities = generate_local_identities();
        let players = setup_local_grpc_networking(
            identities.clone(),
            NetworkType::default_connection_parallelism(),
            NetworkType::default_request_parallelism(),
        )
        .await?;

        let mut jobs = JoinSet::new();

        // Simple session with one message sent from one party to another
        {
            let players = players.clone();

            let session_id = SessionId::from(0);

            jobs.spawn(async move {
                let mut players = create_session_helper(session_id, &players).await.unwrap();

                // we don't need the last player here
                players.pop();

                let mut bob = players.pop().unwrap();
                let mut alice = players.pop().unwrap();

                // Send a message from the first party to the second party
                let message = get_prf();
                let message_copy = message.clone();

                let task1 = tokio::spawn(async move {
                    alice.send(message.clone(), &"bob".into()).await.unwrap();
                });
                let task2 = tokio::spawn(async move {
                    let received_message = bob.receive(&"alice".into()).await.unwrap();
                    assert_eq!(message_copy, received_message);
                });
                let _ = tokio::try_join!(task1, task2).unwrap();
            });
        }

        // Multiple parties sending messages to each other
        let all_parties_talk = |identities: Vec<Identity>, sessions: Vec<GrpcSession>| async move {
            let mut tasks = JoinSet::new();
            let message_to_next = get_prf();
            let message_to_prev = get_prf();
            for (player_id, session) in sessions.into_iter().enumerate() {
                let role = Role::new(player_id);
                let next = role.next(3).index();
                let prev = role.prev(3).index();

                let next_id = identities[next].clone();
                let prev_id = identities[prev].clone();

                let mut session = session;
                let msg_next = message_to_next.clone();
                let msg_prev = message_to_prev.clone();
                tasks.spawn(async move {
                    // Sending
                    session.send(msg_next.clone(), &next_id).await.unwrap();
                    session.send(msg_prev.clone(), &prev_id).await.unwrap();

                    // Receiving
                    let received_message_from_prev = session.receive(&prev_id).await.unwrap();
                    assert_eq!(received_message_from_prev, msg_next);
                    let received_message_from_next = session.receive(&next_id).await.unwrap();
                    assert_eq!(received_message_from_next, msg_prev);
                });
            }
            tasks.join_all().await;
        };

        // Each party sending and receiving messages to each other
        {
            let players = players.clone();
            let identities = identities.clone();
            jobs.spawn(async move {
                let session_id = SessionId::from(1);

                let players = create_session_helper(session_id, &players).await.unwrap();

                // Test that parties can send and receive messages
                all_parties_talk(identities, players).await;
            });
        }

        // Parties create a session asynchronously
        {
            let players = players.clone();
            let session_id = SessionId::from(2);
            // Session is consecutively created
            let sessions = {
                let mut jobs = vec![];
                for (i, player) in players.iter().enumerate() {
                    let player = player.clone();
                    let task = tokio::spawn(async move {
                        tracing::trace!(
                            "Player {:?} is creating session {:?}",
                            player.party_id(),
                            session_id
                        );
                        sleep(Duration::from_millis(200 * i as u64)).await;
                        player.create_session(session_id).await.unwrap()
                    });
                    jobs.push(task);
                }
                join_all(jobs)
                    .await
                    .into_iter()
                    .map(|r| r.map_err(eyre::Report::new))
                    .collect::<Result<Vec<_>>>()?
            };

            // Test that parties can send and receive messages
            all_parties_talk(identities, sessions).await;
        }

        jobs.join_all().await;

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[traced_test]
    async fn test_grpc_comms_fail() -> Result<()> {
        let parties = generate_local_identities();

        let players = setup_local_grpc_networking(
            parties.clone(),
            NetworkType::default_connection_parallelism(),
            NetworkType::default_request_parallelism(),
        )
        .await?;

        let mut jobs = JoinSet::new();

        {
            // Send to a non-existing party
            let players = players.clone();
            jobs.spawn(async move {
                let session_id = SessionId::from(0);
                let mut sessions = create_session_helper(session_id, &players).await.unwrap();

                let message = get_prf();
                let res = sessions[0]
                    .send(message.clone(), &Identity::from("eve"))
                    .await;
                assert_eq!(
                    "Outgoing stream for Identity(\"eve\") in SessionId(0) not found",
                    res.unwrap_err().to_string()
                );
            });
        }

        {
            // Receive from a wrong party
            let players = players.clone();
            jobs.spawn(async move {
                let session_id = SessionId::from(1);
                let mut sessions = create_session_helper(session_id, &players).await.unwrap();

                let res = sessions[0].receive(&Identity::from("eve")).await;
                assert_eq!(
                    res.unwrap_err().to_string(),
                    "Incoming stream for Identity(\"eve\") in SessionId(1) not found"
                );
            });
        }

        {
            // Send to itself
            let players = players.clone();
            jobs.spawn(async move {
                let session_id = SessionId::from(2);
                let mut sessions = create_session_helper(session_id, &players).await.unwrap();

                let message = get_prf();
                let res = sessions[0]
                    .send(message.clone(), &Identity::from("alice"))
                    .await;
                assert_eq!(
                    res.unwrap_err().to_string(),
                    "Outgoing stream for Identity(\"alice\") in SessionId(2) not found",
                );
            });
        }

        {
            // Add the same session
            let players = players.clone();
            jobs.spawn(async move {
                let session_id = SessionId::from(3);
                let _ = create_session_helper(session_id, &players).await.unwrap();

                let alice = players[0].clone();

                let res = alice.create_session(session_id).await;

                assert_eq!(
                    res.unwrap_err().to_string(),
                    "SessionId(3) has already been created by Identity(\"alice\")"
                );
            });
        }

        {
            // Receive from a party that didn't send a message (timeout error)
            let players = players.clone();
            jobs.spawn(async move {
                let session_id = SessionId::from(4);
                let mut sessions = create_session_helper(session_id, &players).await.unwrap();

                let res = sessions[0].receive(&Identity::from("bob")).await;
                assert_eq!(
                    res.unwrap_err().to_string(),
                    "Identity(\"alice\"): Timeout while waiting for message from \
                     Identity(\"bob\") in SessionId(4)"
                );
            });
        }

        jobs.join_all().await;

        Ok(())
    }

    /// Every party opens a control channel, barriers on `sync()`, then sends a
    /// payload tagged with its own role to `next` and reads from `prev`. Proves
    /// that the control channel lands on a stream of its own (the data plane has
    /// already claimed ids `0..request_parallelism`, spread over two streams)
    /// and that next/prev are wired to the right peers.
    #[tokio::test(flavor = "multi_thread")]
    #[traced_test]
    async fn test_grpc_control_channel() -> Result<()> {
        let identities = generate_local_identities();
        let num_parties = identities.len();
        let players = setup_local_grpc_networking(identities.clone(), 2, 4).await?;

        // Claim the data-plane session ids first, so the control channel has to
        // pick ids that don't collide with them.
        {
            let tasks = players
                .iter()
                .map(|player| {
                    let mut player = player.clone();
                    tokio::spawn(async move { player.make_network_sessions().await })
                })
                .collect::<Vec<_>>();
            for task in tasks {
                task.await??;
            }
        }

        let tasks = players
            .iter()
            .enumerate()
            .map(|(party_index, player)| {
                let mut player = player.clone();
                tokio::spawn(async move {
                    let mut cc = player.control_channel().await?;
                    cc.sync().await?;

                    cc.send_next(NetworkValue::Bytes(vec![party_index as u8]))
                        .await?;
                    let received = cc.recv_prev().await?;

                    let expected = ((party_index + num_parties - 1) % num_parties) as u8;
                    match received {
                        NetworkValue::Bytes(b) => assert_eq!(b, vec![expected]),
                        other => panic!("party {party_index}: unexpected variant {other:?}"),
                    }
                    Ok::<(), eyre::Report>(())
                })
            })
            .collect::<Vec<_>>();

        for task in tasks {
            task.await??;
        }

        Ok(())
    }
}
