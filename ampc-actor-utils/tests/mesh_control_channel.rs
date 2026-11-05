//! Integration tests for the N-party mesh control-plane channel.
//!
//! Builds five `NetworkHandle`s over plain TCP (no TLS) and exercises
//! `MeshControlChannel`'s Role-addressed and barrier APIs. Unlike
//! `ControlChannel` (a 3-party ring: next/prev), `MeshControlChannel`
//! addresses every other party directly by `Role`, so it works for any
//! number of parties.

use ampc_actor_utils::execution::player::Role;
use ampc_actor_utils::network::mpc::{
    build_network_handle, NetworkHandle, NetworkHandleArgs, NetworkValue,
};
use futures::future::join_all;
use tokio_util::sync::CancellationToken;
use tracing_test::traced_test;

const NUM_PARTIES: usize = 5;

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
async fn test_mesh_control_channel_sync() {
    let ports = find_free_ports(NUM_PARTIES);
    let addresses: Vec<String> = ports.iter().map(|p| format!("127.0.0.1:{p}")).collect();

    let shutdown_ct = CancellationToken::new();

    let party_tasks = (0..NUM_PARTIES).map(|party_index| {
        let addresses = addresses.clone();
        let shutdown_ct = shutdown_ct.clone();

        tokio::spawn(async move {
            let mut handle = build_handle(party_index, addresses, shutdown_ct).await;
            let mut mc = handle
                .mesh_control_channel()
                .await
                .expect("mesh_control_channel() failed");

            mc.sync().await.expect("sync() failed");
        })
    });

    let results = join_all(party_tasks).await;
    for result in results {
        result.expect("party task panicked");
    }
}

/// Every party sends every other party a payload tagged with its own index,
/// addressed directly by `Role` (not ring position), and confirms it
/// receives back the sender's own tag from each of them.
#[tokio::test(flavor = "multi_thread")]
#[traced_test]
async fn test_mesh_control_channel_send_recv_by_role() {
    let ports = find_free_ports(NUM_PARTIES);
    let addresses: Vec<String> = ports.iter().map(|p| format!("127.0.0.1:{p}")).collect();

    let shutdown_ct = CancellationToken::new();

    let party_tasks = (0..NUM_PARTIES).map(|party_index| {
        let addresses = addresses.clone();
        let shutdown_ct = shutdown_ct.clone();

        tokio::spawn(async move {
            let mut handle = build_handle(party_index, addresses, shutdown_ct).await;
            let mut mc = handle
                .mesh_control_channel()
                .await
                .expect("mesh_control_channel() failed");

            for other in 0..NUM_PARTIES {
                if other == party_index {
                    continue;
                }
                mc.send(
                    Role::new(other),
                    NetworkValue::Bytes(vec![party_index as u8].into()),
                )
                .await
                .unwrap_or_else(|e| panic!("party {party_index}: send to {other} failed: {e}"));
            }

            for other in 0..NUM_PARTIES {
                if other == party_index {
                    continue;
                }
                let received = mc.recv(Role::new(other)).await.unwrap_or_else(|e| {
                    panic!("party {party_index}: recv from {other} failed: {e}")
                });
                match received {
                    NetworkValue::Bytes(b) => assert_eq!(
                        &b[..],
                        &[other as u8],
                        "party {party_index}: recv from {other} got wrong payload"
                    ),
                    other_variant => panic!(
                        "party {party_index}: unexpected variant {other_variant:?} from {other}"
                    ),
                }
            }
        })
    });

    let results = join_all(party_tasks).await;
    for result in results {
        result.expect("party task panicked");
    }
}
