use ampc_actor_utils::network::mpc::{
    build_network_handle_with_policy, handle::config::MpcConfig, NetworkHandleArgs, NetworkValue,
    SessionConnectionPolicy,
};
use eyre::Result;
use futures::future::join_all;
use serial_test::serial;
use std::{net::TcpListener, sync::Arc, time::Duration};
use tokio::{sync::Barrier, task::JoinSet};
use tokio_util::sync::CancellationToken;

const NUM_PARTIES: usize = 3;
const NUM_CONNECTIONS: usize = 4;
const NUM_SESSIONS: usize = 9;
const MESSAGES_PER_SESSION: usize = 6;

#[test]
fn affine_policy_assigns_sessions_round_robin() {
    let config = MpcConfig::new_with_policy(
        Duration::from_secs(1),
        3,
        8,
        SessionConnectionPolicy::Affine,
    );

    let assignments = (0..config.num_sessions)
        .map(|session| config.connection_for_session(session))
        .collect::<Vec<_>>();
    assert_eq!(
        assignments,
        vec![
            Some(0),
            Some(1),
            Some(2),
            Some(0),
            Some(1),
            Some(2),
            Some(0),
            Some(1)
        ]
    );
    assert_eq!(
        (0..config.num_connections)
            .map(|connection| config.get_sessions_for_connection(connection))
            .collect::<Vec<_>>(),
        vec![3, 3, 2]
    );
    assert_eq!(config.connection_for_session(config.num_sessions), None);
    assert_eq!(
        config.get_sessions_for_connection(config.num_connections),
        0
    );

    let more_connections = MpcConfig::new_with_policy(
        Duration::from_secs(1),
        5,
        2,
        SessionConnectionPolicy::Affine,
    );
    assert_eq!(more_connections.num_connections, 2);
    assert_eq!(
        (0..more_connections.num_connections)
            .map(|connection| more_connections.get_sessions_for_connection(connection))
            .collect::<Vec<_>>(),
        vec![1, 1]
    );
    assert_eq!(more_connections.get_sessions_for_connection(2), 0);

    let more_striped_connections = MpcConfig::new(Duration::from_secs(1), 5, 2);
    assert_eq!(more_striped_connections.num_connections, 5);
    assert_eq!(more_striped_connections.get_sessions_for_connection(4), 2);

    let striped = MpcConfig::new(Duration::from_secs(1), 3, 8);
    assert_eq!(
        striped.session_connection_policy,
        SessionConnectionPolicy::Striped
    );
    assert_eq!(striped.connection_for_session(0), None);
    assert_eq!(striped.get_sessions_for_connection(0), 8);
    assert_eq!(striped.get_sessions_for_connection(1), 8);
    assert_eq!(striped.get_sessions_for_connection(2), 8);
}

fn reserve_local_addresses(count: usize) -> Result<Vec<String>> {
    // Hold every listener until all addresses are chosen so this test cannot
    // accidentally select the same ephemeral port twice.
    let listeners = (0..count)
        .map(|_| TcpListener::bind("127.0.0.1:0"))
        .collect::<std::io::Result<Vec<_>>>()?;
    let addresses = listeners
        .iter()
        .map(|listener| listener.local_addr().map(|address| address.to_string()))
        .collect::<std::io::Result<Vec<_>>>()?;
    drop(listeners);
    Ok(addresses)
}

fn message(sender: usize, session: usize, sequence: usize) -> NetworkValue {
    // Values larger than the legacy striping threshold ensure an affine
    // session's direct transport path also handles production-sized payloads.
    let len = if sequence % 3 == 2 {
        300_123
    } else {
        32 + sequence
    };
    let mut payload = vec![(sender * 31 + session * 7 + sequence) as u8; len];
    payload[..8].copy_from_slice(&(session as u64).to_le_bytes());
    payload[8..16].copy_from_slice(&(sequence as u64).to_le_bytes());
    payload[16..24].copy_from_slice(&(sender as u64).to_le_bytes());
    NetworkValue::Bytes(payload)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn affine_sessions_preserve_delivery_and_order_across_connections() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("ampc_actor_utils=debug")
        .with_test_writer()
        .try_init();
    let addresses = reserve_local_addresses(NUM_PARTIES)?;
    let shutdown = CancellationToken::new();

    let mut handles = Vec::with_capacity(NUM_PARTIES);
    for party_index in 0..NUM_PARTIES {
        let args = NetworkHandleArgs {
            party_index,
            addresses: addresses.clone(),
            outbound_addresses: addresses.clone(),
            connection_parallelism: NUM_CONNECTIONS,
            request_parallelism: NUM_SESSIONS,
            sessions_per_request: 1,
            tls: None,
        };
        handles.push(
            build_network_handle_with_policy(
                args,
                shutdown.clone(),
                SessionConnectionPolicy::Affine,
            )
            .await?,
        );
    }

    let session_results = join_all(
        handles
            .iter_mut()
            .map(|handle| handle.make_network_sessions()),
    )
    .await;
    let mut sessions_by_party = Vec::with_capacity(NUM_PARTIES);
    let mut session_error_tokens = Vec::with_capacity(NUM_PARTIES);
    for result in session_results {
        let (sessions, error_token) = result?;
        assert_eq!(sessions.len(), NUM_SESSIONS);
        sessions_by_party.push(sessions.into_iter());
        session_error_tokens.push(error_token);
    }

    let mut jobs = JoinSet::new();
    // A connection is owned collectively by all sessions assigned to it. Keep
    // every session alive until delivery has been verified so dropping the
    // first completed task cannot close a shared connection underneath peers.
    let delivery_barrier = Arc::new(Barrier::new(NUM_PARTIES * NUM_SESSIONS));
    for session_index in 0..NUM_SESSIONS {
        for (party_index, party_sessions) in sessions_by_party.iter_mut().enumerate() {
            let mut session = party_sessions
                .next()
                .expect("every party must create every session");
            let delivery_barrier = delivery_barrier.clone();
            jobs.spawn(async move {
                for sequence in 0..MESSAGES_PER_SESSION {
                    session
                        .send_next(message(party_index, session_index, sequence))
                        .await?;
                }

                let previous_party = (party_index + NUM_PARTIES - 1) % NUM_PARTIES;
                for sequence in 0..MESSAGES_PER_SESSION {
                    let received = session.receive_prev().await?;
                    assert_eq!(
                        received,
                        message(previous_party, session_index, sequence),
                        "session {session_index}, party {party_index}, sequence {sequence}"
                    );
                }
                delivery_barrier.wait().await;
                Result::<()>::Ok(())
            });
        }
    }

    while let Some(result) = jobs.join_next().await {
        result??;
    }
    assert!(sessions_by_party.iter().all(|sessions| sessions.len() == 0));
    assert!(session_error_tokens
        .iter()
        .all(|error_token| !error_token.is_cancelled()));

    shutdown.cancel();
    Ok(())
}
