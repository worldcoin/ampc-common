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

    /// Payload bytes per message (the "small" size for non-fixed distributions).
    #[arg(long, default_value_t = 32)]
    payload: usize,

    /// Payload size distribution across a session's messages.
    #[arg(long, value_enum, default_value_t = Dist::Fixed)]
    dist: Dist,

    /// Large/burst payload bytes (upper size for `bimodal` and `uniform`).
    #[arg(long, default_value_t = 4096)]
    large: usize,

    /// Fraction of messages that are `--large` bytes (bimodal only).
    #[arg(long, default_value_t = 0.05)]
    large_frac: f64,

    /// Per-`receive` timeout in seconds.
    #[arg(long, default_value_t = 5)]
    timeout_secs: u64,

    /// Tokio worker threads (set ≈ the number of cores you `taskset` this process to).
    #[arg(long, default_value_t = 4)]
    workers: usize,
}

/// Payload size distribution across a session's messages.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum Dist {
    /// Every message is exactly `--payload` bytes.
    Fixed,
    /// Mostly `--payload` bytes; a `--large-frac` fraction are `--large` bytes.
    /// The bimodal burst pattern that stresses HTTP-2 flow-control windows.
    Bimodal,
    /// Uniform random size in `[--payload, --large]` bytes.
    Uniform,
}

/// Cheap deterministic per-session RNG (xorshift64) — reproducible, no deps.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next_f64(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Draw the next message size (bytes) for the chosen distribution.
fn next_size(rng: &mut Rng, dist: Dist, small: usize, large: usize, large_frac: f64) -> usize {
    match dist {
        Dist::Fixed => small,
        Dist::Bimodal => {
            if rng.next_f64() < large_frac {
                large
            } else {
                small
            }
        }
        Dist::Uniform => {
            let (lo, hi) = (small.min(large), small.max(large));
            lo + (rng.next_f64() * (hi - lo) as f64) as usize
        }
    }
}

/// Wire header size in bytes. Kept small so it fits the default 32B payload.
const HDR: usize = 16;
/// Magic marking a well-formed benchmark message (catches gross corruption).
const MAGIC: u32 = 0xA11C_E500;
/// Body fill byte; the receiver checks every post-header byte equals this.
const FILL: u8 = 7;

/// Write a self-describing header into `buf` and return the send length.
/// Layout (little-endian): magic u32 | session_id u32 | round u32 | len u32.
/// The body (bytes `HDR..`) is left as the caller's `FILL` pre-fill.
fn encode_msg(buf: &mut [u8], session_id: u32, round: u32, n: usize) -> usize {
    let n = n.max(HDR);
    buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    buf[4..8].copy_from_slice(&session_id.to_le_bytes());
    buf[8..12].copy_from_slice(&round.to_le_bytes());
    buf[12..16].copy_from_slice(&(n as u32).to_le_bytes());
    n
}

/// Validate a received message against the session and round it must carry.
/// Panics on any mismatch: truncation, corruption, drop, reorder, or misroute.
fn check_msg(bytes: &[u8], session_id: u32, round: u32) {
    assert!(
        bytes.len() >= HDR,
        "session {session_id} round {round}: {} bytes < header — truncated",
        bytes.len()
    );
    let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    assert_eq!(
        magic, MAGIC,
        "session {session_id} round {round}: bad magic — corrupted"
    );
    let got_session = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    assert_eq!(
        got_session, session_id,
        "round {round}: message for session {got_session} arrived on \
         session {session_id} — misrouted"
    );
    let got_round = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    assert_eq!(
        got_round, round,
        "session {session_id}: expected round {round}, got {got_round} — \
         dropped or reordered"
    );
    let declared = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    assert_eq!(
        declared,
        bytes.len(),
        "session {session_id} round {round}: declared {declared} bytes, got {} \
         — truncated or padded",
        bytes.len()
    );
    assert!(
        bytes[HDR..].iter().all(|&b| b == FILL),
        "session {session_id} round {round}: body corrupted"
    );
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
        create_tasks.spawn(async move {
            handle
                .create_session(SessionId::from(i as u32))
                .await
                .map(|s| (i as u32, s))
        });
    }
    // Tag each session with its id so the receiver can validate every message.
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
    ping_pong(
        sessions,
        next_id,
        prev_id,
        args.rounds,
        args.payload,
        args.dist,
        args.large,
        args.large_frac,
    )
    .await;
    let elapsed = start.elapsed();

    report(&args, total_msgs, elapsed);

    // Cooldown so peers finish their last rounds before we drop the handle and
    // the process exits (which tears down the streams).
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(handle);
    Ok(())
}

/// One task per session; each runs `rounds` of send(next); recv(prev). Every
/// received message is validated against the session id and round it must carry
/// (see `check_msg`), so a dropped, reordered, misrouted, truncated, or
/// corrupted message panics the benchmark instead of passing silently.
#[allow(clippy::too_many_arguments)]
async fn ping_pong<S>(
    sessions: Vec<(u32, S)>,
    next_id: Identity,
    prev_id: Identity,
    rounds: usize,
    payload: usize,
    dist: Dist,
    large: usize,
    large_frac: f64,
) where
    S: Networking + Send + 'static,
{
    let mut tasks = JoinSet::new();
    for (session_id, mut session) in sessions.into_iter() {
        let next_id = next_id.clone();
        let prev_id = prev_id.clone();
        // Scratch buffer sized to the largest payload we might send; the body is
        // pre-filled with FILL and each round rewrites only the header, so payload
        // generation never dominates the hot path.
        let mut buf = vec![FILL; payload.max(large + HDR).max(HDR)];
        let mut rng = Rng::new(session_id as u64);
        tasks.spawn(async move {
            for round in 0..rounds {
                // send is non-blocking (unbounded mpsc → coalesced into the gRPC
                // stream by tonic's h2 task), so all parties enqueue before anyone
                // blocks on recv — no deadlock.
                let n = next_size(&mut rng, dist, payload, large, large_frac);
                let len = encode_msg(&mut buf, session_id, round as u32, n);
                let msg = NetworkValue::Bytes(buf[..len].to_vec());
                session.send(msg, &next_id).await.unwrap();
                match session.receive(&prev_id).await {
                    Ok(NetworkValue::Bytes(b)) => check_msg(&b, session_id, round as u32),
                    Ok(_) => {
                        panic!("session {session_id} round {round}: expected NetworkValue::Bytes")
                    }
                    Err(e) => panic!(
                        "session {session_id} round {round}: recv failed ({e}) — dropped \
                         message or peer gone"
                    ),
                }
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
    let dist_desc = match args.dist {
        Dist::Fixed => format!("fixed {}B", args.payload),
        Dist::Bimodal => format!(
            "bimodal {}B/{}B frac={}",
            args.payload, args.large, args.large_frac
        ),
        Dist::Uniform => format!("uniform {}..{}B", args.payload, args.large),
    };
    println!("distribution:      {}", dist_desc);
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

taskset -c 0-3   $BIN --party 0 --workers 4 --sessions 1000 --rounds 2000 &
taskset -c 4-7   $BIN --party 1 --workers 4 --sessions 1000 --rounds 2000 &
taskset -c 8-11  $BIN --party 2 --workers 4 --sessions 1000 --rounds 2000 &
wait

# Bimodal payloads to stress HTTP-2 flow-control windows: mostly 32B, 5% 16KB
# bursts. Add `--dist uniform` for a uniform 32B..16KB spread instead.
DIST="--dist bimodal --payload 32 --large 16384 --large-frac 0.05"
taskset -c 0-3   $BIN --party 0 --workers 4 --sessions 100 --rounds 2000 $DIST &
taskset -c 4-7   $BIN --party 1 --workers 4 --sessions 100 --rounds 2000 $DIST &
taskset -c 8-11  $BIN --party 2 --workers 4 --sessions 100 --rounds 2000 $DIST &
wait


# Tear down netem
sudo tc qdisc del dev lo root

# The three wall-clocks should agree closely; per-party perf.pN.txt gives CPU,
# context-switches, and cache behavior per party (disjoint cores → clean attribution).
──────────────────────────────────────────────────────────────────────────────── */
