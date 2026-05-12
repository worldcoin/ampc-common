//! Integration test for the MPC control-plane channel.
//!
//! Builds three `NetworkHandle`s over plain TCP (no TLS), calls
//! `control_channel()` on each, and then runs a three-party `sync()` barrier
//! to confirm that all parties can complete the handshake successfully.

use ampc_actor_utils::network::mpc::{build_network_handle, NetworkHandle, NetworkHandleArgs};
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
            let mut handle: Box<dyn NetworkHandle> = build_network_handle(
                NetworkHandleArgs {
                    party_index,
                    addresses: addresses.clone(),
                    outbound_addresses: addresses.clone(),
                    connection_parallelism: 1,
                    request_parallelism: 1,
                    sessions_per_request: 1,
                    tls: None,
                },
                shutdown_ct,
            )
            .await
            .expect("build_network_handle failed");

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
