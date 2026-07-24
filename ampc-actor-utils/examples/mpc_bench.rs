//! MPC networking stack benchmark: 1000-session ring ping-pong.
//!
//! Drives the raw-TCP multiplexer with the production access pattern: N concurrent
//! sessions, each running R sequential rounds of `send(next); recv(prev)` on the
//! 3-party ring. This exercises exactly the cross-session coalescing path that
//! dominates real search wall-clock.
//!
//! Everything is loopback, so ABSOLUTE wall-clock is NOT representative of prod
//! (loopback RTT ≈ µs). Its value is (a) isolating per-message CPU/scheduling
//! overhead when run bare, and (b) reproducing the latency-bound + flow-control
//! regime when run under `tc netem` (see the recipe at the bottom).
//!
//! Build (release, mandatory for meaningful numbers):
//!     cargo build --release --example mpc_bench
//!
//! Run (env-configurable; defaults in `Cfg::from_env`). Run 2–3× and take the
//! median — the first run pays cold-cache / first-touch costs:
//!     SESSIONS=1000 ROUNDS=2000 CONNECTIONS=4 PAYLOAD=32 \
//!         ./target/release/examples/mpc_bench

use std::time::{Duration, Instant};

use ampc_actor_utils::execution::local::generate_local_identities;
use ampc_actor_utils::execution::player::{Identity, Role};
use ampc_actor_utils::execution::session::NetworkSession;
use ampc_actor_utils::network::mpc::handle::testing::setup_local_mpc_networking;
use ampc_actor_utils::network::mpc::{NetworkValue, Networking};
use eyre::Result;
use tokio::task::JoinSet;

struct Cfg {
    sessions: usize,    // logical sessions per party (== request_parallelism)
    rounds: usize,      // sequential rounds per session
    connections: usize, // TCP connections per peer
    payload: usize,     // payload bytes per message
    workers: usize,     // tokio worker threads
}

impl Cfg {
    fn from_env() -> Self {
        let env = |k: &str, d: usize| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        Cfg {
            sessions: env("SESSIONS", 1000),
            rounds: env("ROUNDS", 2000),
            connections: env("CONNECTIONS", 4),
            payload: env("PAYLOAD", 32),
            workers: env(
                "WORKERS",
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(8),
            ),
        }
    }
}

fn main() -> Result<()> {
    let cfg = Cfg::from_env();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cfg.workers)
        .enable_all()
        .build()?;
    rt.block_on(run(cfg))
}

async fn run(cfg: Cfg) -> Result<()> {
    let identities = generate_local_identities();

    // Establishes + validates all connections and returns sessions[party][session_idx].
    // Keep `_handles` alive: dropping a handle cancels its shutdown token and tears
    // down the multiplexer tasks.
    let (_handles, sessions) =
        setup_local_mpc_networking(identities.clone(), cfg.connections, cfg.sessions).await?;

    let total_msgs = 3 * cfg.sessions * cfg.rounds; // sends; recvs are equal
    let start = Instant::now();
    ping_pong(&identities, sessions, cfg.rounds, cfg.payload).await;
    let elapsed = start.elapsed();

    report(&cfg, total_msgs, elapsed);
    Ok(())
}

/// Spawn one task per (party, session); each runs `rounds` of send(next); recv(prev).
async fn ping_pong(
    identities: &[Identity],
    sessions: Vec<Vec<NetworkSession>>,
    rounds: usize,
    payload: usize,
) {
    let mut tasks = JoinSet::new();
    for (party, party_sessions) in sessions.into_iter().enumerate() {
        let role = Role::new(party);
        let next_id = identities[role.next(3).index()].clone();
        let prev_id = identities[role.prev(3).index()].clone();
        for mut session in party_sessions.into_iter() {
            let next_id = next_id.clone();
            let prev_id = prev_id.clone();
            let msg = NetworkValue::Bytes(vec![7u8; payload]);
            tasks.spawn(async move {
                for _ in 0..rounds {
                    // send is non-blocking (unbounded mpsc → coalesced by the mux
                    // task), so every party enqueues before anyone blocks on recv —
                    // no deadlock despite send-before-recv ordering.
                    session
                        .networking
                        .send(msg.clone(), &next_id)
                        .await
                        .unwrap();
                    let _ = session.networking.receive(&prev_id).await.unwrap();
                }
            });
        }
    }
    tasks.join_all().await;
}

fn report(cfg: &Cfg, total_msgs: usize, elapsed: Duration) {
    let secs = elapsed.as_secs_f64();
    let per_round_us = (elapsed.as_nanos() as f64 / cfg.rounds as f64) / 1000.0;
    println!("──────── mpc_bench ────────");
    println!(
        "sessions={} rounds={} connections={} payload={}B workers={}",
        cfg.sessions, cfg.rounds, cfg.connections, cfg.payload, cfg.workers
    );
    println!("wall-clock:        {:.3} s", secs);
    println!("total messages:    {} (sends; recvs equal)", total_msgs);
    println!(
        "throughput:        {:.2} M msg/s",
        total_msgs as f64 / secs / 1e6
    );
    println!(
        "per-round latency: {:.2} µs  (wall / rounds, all sessions overlapped)",
        per_round_us
    );
    println!("────────────────────────────");
}

/*  perf + netem recipe  ────────────────────────────────────────────────────────

# 1. Build once
cargo build --release --example mpc_bench

# 2. (optional) inject realistic RTT + loss on loopback.
#    netem on `lo` delays BOTH directions, so RTT ≈ 2 × delay. 500us → ~1ms RTT.
sudo tc qdisc add dev lo root netem delay 500us 100us distribution normal loss 0.05%

# 3. Measure CPU / context-switches / cache with perf; wall-clock printed by the bench
SESSIONS=1000 ROUNDS=2000 CONNECTIONS=4 PAYLOAD=32 \
  perf stat -d -d ./target/release/examples/mpc_bench

# CPU profile / flamegraph of where time goes in the stack:
#   perf record -g ./target/release/examples/mpc_bench && perf report

# 4. Tear down netem
sudo tc qdisc del dev lo root

# Sweep RTT to build a latency-bound curve (the interesting plot):
for d in 0us 250us 500us 1ms 2ms; do
  sudo tc qdisc replace dev lo root netem delay $d 2>/dev/null
  echo "delay=$d (RTT≈2x)"; ./target/release/examples/mpc_bench
done
sudo tc qdisc del dev lo root

# To see the stack's internal coalescing metrics (flush_reason, batch sizes), build
# with --features networking_metrics AND install a metrics recorder in main()
# (e.g. metrics_exporter_prometheus). Without a recorder the metrics! macros are no-ops.
──────────────────────────────────────────────────────────────────────────────── */
