//! Single-party gRPC node for the 3-process benchmark — the gRPC analogue of
//! `examples/mpc_node.rs`.
//!
//! Launch this binary THREE times — once per party — each pinned to a disjoint
//! core set with `taskset`. Each process is one party and runs its own gRPC
//! server (in a detached task) plus outbound clients to the other two parties,
//! exactly like the `grpc` networking stack in prod but on one box.
//!
//! Unlike the MPC handle there is no `make_network_sessions()` rendezvous: each
//! party creates sessions `0..sessions` explicitly via `create_session`, and
//! `create_session` blocks (polling `is_session_ready`) until every peer has
//! created the matching session id. Because all three processes create the same
//! id set concurrently, they rendezvous per-session — launch order does not
//! matter within the retry/connect window.
//!
//! Each session then runs R rounds of `send(next); recv(prev)` on the ring and
//! the process prints its own wall-clock — the three should be near-identical.
//!
//! Build:
//!     cargo build --release --features grpc --example grpc_node
//!
//! Run (12-core example: 4 workers per party). See the recipe at the end.

use std::time::{Duration, Instant};

use ampc_actor_utils::execution::local::generate_local_identities;
use ampc_actor_utils::execution::player::{Identity, Role};
use ampc_actor_utils::execution::session::SessionId;
use ampc_actor_utils::network::grpc::{build_network_handle, GrpcNetworkHandleArgs};
use ampc_actor_utils::network::mpc::{NetworkValue, Networking};
use clap::Parser;
use eyre::Result;
use tokio::task::JoinSet;

#[derive(Parser, Debug)]
#[command(about = "One gRPC party for the 3-process ring ping-pong benchmark")]
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

    /// Number of concurrent application-level sessions on this party.
    #[arg(long, default_value_t = 1000)]
    sessions: usize,

    /// Sequential rounds per session.
    #[arg(long, default_value_t = 2000)]
    rounds: usize,

    /// gRPC connections per peer (connection_parallelism). The number of gRPC
    /// streams is derived as `sessions / connections` (one stream per connection),
    /// so keep `sessions` a multiple of `connections`.
    #[arg(long, default_value_t = 4)]
    connections: usize,

    /// Payload bytes per message.
    #[arg(long, default_value_t = 32)]
    payload: usize,

    /// Per-`receive` timeout in seconds.
    #[arg(long, default_value_t = 5)]
    timeout_secs: u64,

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

    let handle = build_network_handle(GrpcNetworkHandleArgs {
        party_index: args.party,
        addresses: args.addrs.clone(),
        outbound_addresses: outbound,
        connection_parallelism: args.connections,
        request_parallelism: args.sessions,
        timeout_duration: Duration::from_secs(args.timeout_secs),
    })
    .await?;

    // Rendezvous: create every session. Each `create_session` returns only once
    // all peers have created the matching id, so this is our connect barrier.
    eprintln!(
        "[party {}] creating {} sessions…",
        args.party, args.sessions
    );
    let mut create_tasks = JoinSet::new();
    for i in 0..args.sessions {
        let handle = handle.clone();
        create_tasks.spawn(async move { handle.create_session(SessionId::from(i as u32)).await });
    }
    let mut sessions = Vec::with_capacity(args.sessions);
    for res in create_tasks.join_all().await {
        sessions.push(res?);
    }
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
    ping_pong(sessions, next_id, prev_id, args.rounds, args.payload).await;
    let elapsed = start.elapsed();

    report(&args, total_msgs, elapsed);

    // Cooldown so peers finish their last rounds before we drop the handle and
    // the process exits (which tears down the streams).
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(handle);
    Ok(())
}

/// One task per session; each runs `rounds` of send(next); recv(prev).
async fn ping_pong<S>(
    sessions: Vec<S>,
    next_id: Identity,
    prev_id: Identity,
    rounds: usize,
    payload: usize,
) where
    S: Networking + Send + 'static,
{
    let mut tasks = JoinSet::new();
    for mut session in sessions.into_iter() {
        let next_id = next_id.clone();
        let prev_id = prev_id.clone();
        let msg = NetworkValue::Bytes(vec![7u8; payload]);
        tasks.spawn(async move {
            for _ in 0..rounds {
                // send is non-blocking (unbounded mpsc → coalesced into the gRPC
                // stream by tonic's h2 task), so all parties enqueue before anyone
                // blocks on recv — no deadlock.
                session.send(msg.clone(), &next_id).await.unwrap();
                let _ = session.receive(&prev_id).await.unwrap();
            }
        });
    }
    tasks.join_all().await;
}

fn report(args: &Args, total_msgs: usize, elapsed: Duration) {
    let secs = elapsed.as_secs_f64();
    let per_round_us = (elapsed.as_nanos() as f64 / args.rounds as f64) / 1000.0;
    println!("──────── grpc_node party {} ────────", args.party);
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

/*  launch recipe (3 pinned processes)  ──────────────────────────────────────────

# Build once
cargo build --release --features grpc --example grpc_node
BIN=./target/release/examples/grpc_node

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

# Tear down netem
sudo tc qdisc del dev lo root

# The three wall-clocks should agree closely; per-party perf.pN.txt gives CPU,
# context-switches, and cache behavior per party (disjoint cores → clean attribution).
──────────────────────────────────────────────────────────────────────────────── */
