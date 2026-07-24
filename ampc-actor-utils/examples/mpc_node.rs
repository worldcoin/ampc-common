//! Single-party MPC node for the 3-process benchmark (Option A).
//!
//! Launch this binary THREE times — once per party — each pinned to a disjoint core
//! set with `taskset`. Each process is one MPC party, exactly like prod but on one
//! box. `make_network_sessions()` rendezvouses the three processes (it returns only
//! after all connections are established and PRF-validated), so no external barrier
//! is needed and launch order does not matter within the ~retry window.
//!
//! Each process runs its own sessions through R rounds of `send(next); recv(prev)`
//! on the ring and prints its own wall-clock — the three should be near-identical.
//!
//! Build:
//!     cargo build --release --example mpc_node
//!
//! Run (12-core example: 4 cores + 4 workers per party). See the recipe at the end.

use std::time::{Duration, Instant};

use ampc_actor_utils::execution::local::generate_local_identities;
use ampc_actor_utils::execution::player::{Identity, Role};
use ampc_actor_utils::execution::session::NetworkSession;
use ampc_actor_utils::network::mpc::{
    build_network_handle, NetworkHandle, NetworkHandleArgs, NetworkValue, Networking,
};
use clap::Parser;
use eyre::Result;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(about = "One MPC party for the 3-process ring ping-pong benchmark")]
struct Args {
    /// This party's index: 0, 1, or 2.
    #[arg(long)]
    party: usize,

    /// Listen addresses for all three parties, in order.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "127.0.0.1:7000,127.0.0.1:7001,127.0.0.1:7002"
    )]
    addrs: Vec<String>,

    /// Dial (outbound) addresses for all three parties, in order. Defaults to `addrs`.
    #[arg(long, value_delimiter = ',')]
    outbound: Option<Vec<String>>,

    /// Number of concurrent sessions on this party.
    #[arg(long, default_value_t = 1000)]
    sessions: usize,

    /// Sequential rounds per session.
    #[arg(long, default_value_t = 2000)]
    rounds: usize,

    /// TCP connections per peer.
    #[arg(long, default_value_t = 4)]
    connections: usize,

    /// Payload bytes per message.
    #[arg(long, default_value_t = 32)]
    payload: usize,

    /// Tokio worker threads (set ≈ the number of cores you `taskset` this process to).
    #[arg(long, default_value_t = 4)]
    workers: usize,
}

fn main() -> Result<()> {
    let args = Args::parse();
    assert!(args.party < 3, "party must be 0, 1, or 2");
    assert_eq!(args.addrs.len(), 3, "need exactly 3 listen addresses");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(args.workers)
        .enable_all()
        .build()?;
    rt.block_on(run(args))
}

async fn run(args: Args) -> Result<()> {
    let identities = generate_local_identities();
    let outbound = args.outbound.clone().unwrap_or_else(|| args.addrs.clone());

    let shutdown_ct = CancellationToken::new();
    let mut handle: Box<dyn NetworkHandle> = build_network_handle(
        NetworkHandleArgs {
            party_index: args.party,
            addresses: args.addrs.clone(),
            outbound_addresses: outbound,
            connection_parallelism: args.connections,
            request_parallelism: args.sessions,
            sessions_per_request: 1, // num_sessions = request_parallelism * this
            tls: None,
        },
        shutdown_ct.clone(),
    )
    .await?;

    // Rendezvous barrier: returns only once all peers are connected + PRF-validated.
    eprintln!("[party {}] connecting…", args.party);
    let (sessions, _session_ct) = handle.make_network_sessions().await?;
    eprintln!(
        "[party {}] connected: {} sessions ready",
        args.party,
        sessions.len()
    );

    let role = Role::new(args.party);
    let next_id = identities[role.next(3).index()].clone();
    let prev_id = identities[role.prev(3).index()].clone();

    let total_msgs = args.sessions * args.rounds; // this party's sends; recvs equal
    let start = Instant::now();
    let sessions = ping_pong(sessions, next_id, prev_id, args.rounds, args.payload).await;
    let elapsed = start.elapsed();

    report(&args, total_msgs, elapsed);

    // Cooldown so peers finish their last rounds before we tear down. Keep the
    // sessions alive across the sleep: dropping a session drains its outbound
    // channel, which the peer's reader sees as EOF and turns into a mesh-wide
    // err_ct cancellation — killing any peer still mid-benchmark.
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(sessions);
    drop(handle);
    Ok(())
}

/// One task per session; each runs `rounds` of send(next); recv(prev).
///
/// Returns the sessions so the caller can keep them alive until after the
/// cooldown — dropping a session early drains its outbound channel and signals
/// EOF to the peer still receiving from us, tearing down the whole mesh.
async fn ping_pong(
    sessions: Vec<NetworkSession>,
    next_id: Identity,
    prev_id: Identity,
    rounds: usize,
    payload: usize,
) -> Vec<NetworkSession> {
    let mut tasks = JoinSet::new();
    for mut session in sessions.into_iter() {
        let next_id = next_id.clone();
        let prev_id = prev_id.clone();
        let msg = NetworkValue::Bytes(vec![7u8; payload]);
        tasks.spawn(async move {
            for _ in 0..rounds {
                // send is non-blocking (unbounded mpsc → coalesced by the mux task),
                // so all parties enqueue before anyone blocks on recv — no deadlock.
                if session.networking.send(msg.clone(), &next_id).await.is_err() {
                    break;
                }
                // A peer finishing first closes its send half; the resulting recv
                // error is benign end-of-benchmark teardown, not a reason to abort.
                if session.networking.receive(&prev_id).await.is_err() {
                    break;
                }
            }
            session
        });
    }
    tasks.join_all().await
}

fn report(args: &Args, total_msgs: usize, elapsed: Duration) {
    let secs = elapsed.as_secs_f64();
    let per_round_us = (elapsed.as_nanos() as f64 / args.rounds as f64) / 1000.0;
    println!("──────── mpc_node party {} ────────", args.party);
    println!(
        "sessions={} rounds={} connections={} payload={}B workers={}",
        args.sessions, args.rounds, args.connections, args.payload, args.workers
    );
    println!("wall-clock:        {:.3} s", secs);
    println!("this party msgs:   {} (sends; recvs equal)", total_msgs);
    println!(
        "throughput:        {:.2} M msg/s (this party, one direction)",
        total_msgs as f64 / secs / 1e6
    );
    println!(
        "per-round latency: {:.2} µs  (wall / rounds, all sessions overlapped)",
        per_round_us
    );
    println!("────────────────────────────────────");
}

/*  launch recipe (Option A: 3 pinned processes)  ───────────────────────────────

# Build once
cargo build --release --example mpc_node
BIN=./target/release/examples/mpc_node

# (optional) realistic RTT + loss on loopback. netem on `lo` delays BOTH directions,
# so RTT ≈ 2 × delay.
sudo tc qdisc add dev lo root netem delay 500us 100us distribution normal loss 0.05%

# Launch 3 parties pinned to DISJOINT core sets. Match --workers to the core count
# (≈ 1 worker thread per core — do NOT oversubscribe; this is async, non-blocking I/O).
# Adjust core ranges to your machine (this assumes ≥12 cores).
taskset -c 0-3  perf stat -d -o perf.p0.txt $BIN --party 0 --workers 4 --sessions 1000 --rounds 2000 &
taskset -c 4-7  perf stat -d -o perf.p1.txt $BIN --party 1 --workers 4 --sessions 1000 --rounds 2000 &
taskset -c 8-11 perf stat -d -o perf.p2.txt $BIN --party 2 --workers 4 --sessions 1000 --rounds 2000 &
wait

taskset -c 0-3   $BIN --party 0 --workers 4 --sessions 1000 --rounds 2000 &
taskset -c 4-7   $BIN --party 1 --workers 4 --sessions 1000 --rounds 2000 &
taskset -c 8-11  $BIN --party 2 --workers 4 --sessions 1000 --rounds 2000 &
wait


# Tear down netem
sudo tc qdisc del dev lo root

# The three wall-clocks should agree closely; per-party perf.pN.txt gives CPU,
# context-switches, and cache behavior per party (disjoint cores → clean attribution).

# RTT sweep (run the 3-process block above once per delay to build the latency curve):
#   for d in 0us 250us 500us 1ms 2ms; do
#     sudo tc qdisc replace dev lo root netem delay $d
#     <launch the 3 processes, collect wall-clock>
#   done
#   sudo tc qdisc del dev lo root
──────────────────────────────────────────────────────────────────────────────── */
