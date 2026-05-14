//! Integration tests for the MPC control-plane channel.
//!
//! Builds three `NetworkHandle`s over plain TCP (no TLS) and exercises
//! `ControlChannel`'s directional and barrier APIs.

use ampc_actor_utils::network::mpc::{
    build_network_handle, NetworkHandle, NetworkHandleArgs, NetworkValue,
};
use futures::future::join_all;
use tokio_util::sync::CancellationToken;
use tracing_test::traced_test;

const NUM_PARTIES: usize = 3;

/// Bind `n` listeners simultaneously so the OS assigns `n` distinct free ports,
/// then return those ports. Holding all listeners alive until all ports are
/// collected prevents the OS from handing out the same port twice.
fn find_free_ports(n: usize) -> Vec<u16> {
    let listeners: Vec<std::net::TcpListener> = (0..n)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect()
}

async fn build_handle(
    party_index: usize,
    addresses: Vec<String>,
    shutdown_ct: CancellationToken,
) -> Box<dyn NetworkHandle> {
    build_network_handle(
        NetworkHandleArgs {
            party_index,
            addresses: addresses.clone(),
            outbound_addresses: addresses,
            connection_parallelism: 1,
            request_parallelism: 1,
            sessions_per_request: 1,
            tls: None,
        },
        shutdown_ct,
    )
    .await
    .expect("build_network_handle failed")
}

#[tokio::test(flavor = "multi_thread")]
#[traced_test]
async fn test_control_channel_sync() {
    let ports = find_free_ports(NUM_PARTIES);
    let addresses: Vec<String> = ports.iter().map(|p| format!("127.0.0.1:{p}")).collect();

    let shutdown_ct = CancellationToken::new();

    let party_tasks = (0..NUM_PARTIES).map(|party_index| {
        let addresses = addresses.clone();
        let shutdown_ct = shutdown_ct.clone();

        tokio::spawn(async move {
            let mut handle = build_handle(party_index, addresses, shutdown_ct).await;
            let mut cc = handle
                .control_channel()
                .await
                .expect("control_channel() failed");

            cc.sync().await.expect("sync() failed");
        })
    });

    let results = join_all(party_tasks).await;
    for result in results {
        result.expect("party task panicked");
    }
}

/// Each party sends a payload tagged with its own index to its `next` peer,
/// then receives from `prev`. Confirms that `send_next` and `recv_prev` route
/// to the correct streams: party N's `recv_prev` must observe the payload sent
/// by party (N - 1) mod NUM_PARTIES.
#[tokio::test(flavor = "multi_thread")]
#[traced_test]
async fn test_control_channel_send_next_recv_prev() {
    let ports = find_free_ports(NUM_PARTIES);
    let addresses: Vec<String> = ports.iter().map(|p| format!("127.0.0.1:{p}")).collect();

    let shutdown_ct = CancellationToken::new();

    let party_tasks = (0..NUM_PARTIES).map(|party_index| {
        let addresses = addresses.clone();
        let shutdown_ct = shutdown_ct.clone();

        tokio::spawn(async move {
            let mut handle = build_handle(party_index, addresses, shutdown_ct).await;
            let mut cc = handle
                .control_channel()
                .await
                .expect("control_channel() failed");

            cc.send_next(NetworkValue::Bytes(vec![party_index as u8]))
                .await
                .expect("send_next() failed");

            let received = cc.recv_prev().await.expect("recv_prev() failed");
            let expected = ((party_index + NUM_PARTIES - 1) % NUM_PARTIES) as u8;
            match received {
                NetworkValue::Bytes(b) => assert_eq!(
                    b,
                    vec![expected],
                    "party {party_index}: recv_prev got wrong payload"
                ),
                other => panic!("party {party_index}: unexpected variant {other:?}"),
            }
        })
    });

    let results = join_all(party_tasks).await;
    for result in results {
        result.expect("party task panicked");
    }
}

/// Mirror of the above: each party sends to `prev` and receives from `next`.
/// Party N's `recv_next` must observe the payload sent by party (N + 1) mod
/// NUM_PARTIES.
#[tokio::test(flavor = "multi_thread")]
#[traced_test]
async fn test_control_channel_send_prev_recv_next() {
    let ports = find_free_ports(NUM_PARTIES);
    let addresses: Vec<String> = ports.iter().map(|p| format!("127.0.0.1:{p}")).collect();

    let shutdown_ct = CancellationToken::new();

    let party_tasks = (0..NUM_PARTIES).map(|party_index| {
        let addresses = addresses.clone();
        let shutdown_ct = shutdown_ct.clone();

        tokio::spawn(async move {
            let mut handle = build_handle(party_index, addresses, shutdown_ct).await;
            let mut cc = handle
                .control_channel()
                .await
                .expect("control_channel() failed");

            cc.send_prev(NetworkValue::Bytes(vec![party_index as u8]))
                .await
                .expect("send_prev() failed");

            let received = cc.recv_next().await.expect("recv_next() failed");
            let expected = ((party_index + 1) % NUM_PARTIES) as u8;
            match received {
                NetworkValue::Bytes(b) => assert_eq!(
                    b,
                    vec![expected],
                    "party {party_index}: recv_next got wrong payload"
                ),
                other => panic!("party {party_index}: unexpected variant {other:?}"),
            }
        })
    });

    let results = join_all(party_tasks).await;
    for result in results {
        result.expect("party task panicked");
    }
}
