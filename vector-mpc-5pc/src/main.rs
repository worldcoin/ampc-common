//! Measurement harness: runs the same secret-shared vector comparison
//! workload under the 3-party protocol (the production implementation in
//! `ampc-actor-utils`) and the 5-party extension
//! (`ampc_actor_utils::protocol::rss5`), verifies the results against the
//! plaintext computation, and reports the number of bytes transferred per
//! vector comparison.
//!
//! All parties run in one process over `LocalNetworkingStore` channels
//! wrapped in a byte-counting `Networking` implementation, so the measured
//! bytes are the serialized `NetworkValue` messages the production stack
//! would hand to the transport (excluding TCP/TLS framing).
//!
//! Usage: `vector-mpc-5pc [db_size] [num_queries]` (defaults: 4096, 2).

use ampc_actor_utils::execution::player::{Identity, Role, RoleAssignment};
use ampc_actor_utils::execution::session::{NetworkSession, Session, SessionHandles, SessionId};
use ampc_actor_utils::network::mpc::{
    local::{LocalNetworking, LocalNetworkingStore},
    NetworkValue, Networking,
};
use ampc_actor_utils::protocol::binary::{extract_msb_batch, open_bin};
use ampc_actor_utils::protocol::ops::{galois_ring_to_rep3, setup_replicated_prf, sub_pub};
use ampc_actor_utils::protocol::prf::PrfSeed;
use ampc_actor_utils::protocol::rss5;
use ampc_secret_sharing::iris_vector::{IrisVector, IRIS_VECTOR_SIZE};
use ampc_secret_sharing::RingElement;
use async_trait::async_trait;
use eyre::Result;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const THRESHOLD: u16 = 1000;
const QUERY_SEED_BASE: u64 = 1_000_000_000;

/// Wrapping dot product of two u16 share vectors (as in the reference PoC).
fn udot(a: &[u16], b: &[u16]) -> u16 {
    a.iter()
        .zip(b.iter())
        .map(|(&a, &b)| u16::wrapping_mul(a, b))
        .fold(0_u16, u16::wrapping_add)
}

/// Deterministic per-index RNG so every party derives the same DB/query
/// shares locally (same trick as the reference PoC).
fn db_rng(index: usize) -> StdRng {
    StdRng::seed_from_u64(index as u64)
}

fn query_rng(query: usize) -> StdRng {
    StdRng::seed_from_u64(QUERY_SEED_BASE + query as u64)
}

/// `Networking` wrapper counting the serialized bytes and messages this
/// party hands to the transport.
struct CountingNetworking {
    inner: LocalNetworking,
    bytes_sent: Arc<AtomicU64>,
    messages_sent: Arc<AtomicU64>,
}

#[async_trait]
impl Networking for CountingNetworking {
    async fn send(&mut self, value: NetworkValue, receiver: &Identity) -> Result<()> {
        self.bytes_sent
            .fetch_add(value.byte_len() as u64, Ordering::Relaxed);
        self.messages_sent.fetch_add(1, Ordering::Relaxed);
        self.inner.send(value, receiver).await
    }

    async fn receive(&mut self, sender: &Identity) -> Result<NetworkValue> {
        self.inner.receive(sender).await
    }
}

#[derive(Default, Clone, Copy)]
struct PartyStats {
    setup_bytes: u64,
    convert_bytes: u64,
    msb_bytes: u64,
    open_bytes: u64,
    messages: u64,
}

struct Counters {
    bytes: Arc<AtomicU64>,
    messages: Arc<AtomicU64>,
}

impl Counters {
    fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// Build one counted `NetworkSession` per party over an in-process mesh.
fn counted_network_sessions(num_parties: usize) -> Vec<(NetworkSession, Counters)> {
    let identities: Vec<Identity> = (0..num_parties)
        .map(|i| Identity::from(format!("party{i}")))
        .collect();
    let role_assignments: Arc<RoleAssignment> = Arc::new(
        identities
            .iter()
            .enumerate()
            .map(|(index, id)| (Role::new(index), id.clone()))
            .collect(),
    );
    let store = LocalNetworkingStore::from_host_ids(&identities);
    identities
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let bytes = Arc::new(AtomicU64::new(0));
            let messages = Arc::new(AtomicU64::new(0));
            let networking = CountingNetworking {
                inner: store.get_local_network(id.clone()),
                bytes_sent: Arc::clone(&bytes),
                messages_sent: Arc::clone(&messages),
            };
            let session = NetworkSession {
                session_id: SessionId(0),
                role_assignments: Arc::clone(&role_assignments),
                networking: Box::new(networking),
                own_role: Role::new(i),
            };
            (session, Counters { bytes, messages })
        })
        .collect()
}

async fn party_3(
    mut network_session: NetworkSession,
    counters: Counters,
    db_size: usize,
    queries: Vec<Vec<bool>>,
) -> Result<PartyStats> {
    let party = network_session.own_role().index();
    let seed: PrfSeed = rand::thread_rng().gen();
    let prf = setup_replicated_prf(&mut network_session, seed).await?;
    let mut sess = Session {
        network_session,
        prf,
    };
    let mut stats = PartyStats {
        setup_bytes: counters.bytes(),
        ..Default::default()
    };

    let db: Vec<[u16; IRIS_VECTOR_SIZE]> = (0..db_size)
        .map(|i| {
            let mut rng = db_rng(i);
            let vector = IrisVector::random(&mut rng);
            vector.secret_share(&mut rng).unwrap()[party].0
        })
        .collect();

    for (q, expected) in queries.iter().enumerate() {
        let mut rng = query_rng(q);
        let vector = IrisVector::random(&mut rng);
        let mut query_share = vector.secret_share(&mut rng)?[party].clone();
        query_share.multiply_lagrange_coeffs(party + 1);

        let distances: Vec<RingElement<u16>> = db
            .iter()
            .map(|d| RingElement(udot(&query_share.0, d)))
            .collect();

        let before = counters.bytes();
        let mut rep = galois_ring_to_rep3(&mut sess, distances).await?;
        for share in rep.iter_mut() {
            sub_pub(&mut sess, share, RingElement(THRESHOLD));
        }
        let after_convert = counters.bytes();
        let msb = extract_msb_batch(&mut sess, &rep).await?;
        let after_msb = counters.bytes();
        let bits: Vec<bool> = open_bin(&mut sess, &msb)
            .await?
            .into_iter()
            .map(|b| b.convert())
            .collect();
        let after_open = counters.bytes();

        stats.convert_bytes += after_convert - before;
        stats.msb_bytes += after_msb - after_convert;
        stats.open_bytes += after_open - after_msb;

        assert_eq!(
            &bits, expected,
            "3PC party {party} query {q}: result mismatch"
        );
    }
    stats.messages = counters.messages.load(Ordering::Relaxed);
    Ok(stats)
}

async fn party_5(
    network_session: NetworkSession,
    counters: Counters,
    db_size: usize,
    queries: Vec<Vec<bool>>,
) -> Result<PartyStats> {
    let party = network_session.own_role().index();
    let seed: PrfSeed = rand::thread_rng().gen();
    let mut sess = rss5::setup_rss5_session(network_session, seed).await?;
    let mut stats = PartyStats {
        setup_bytes: counters.bytes(),
        ..Default::default()
    };

    let db: Vec<[u16; IRIS_VECTOR_SIZE]> = (0..db_size)
        .map(|i| {
            let mut rng = db_rng(i);
            let vector = IrisVector::random(&mut rng);
            vector.secret_share_5(&mut rng).unwrap()[party].0
        })
        .collect();

    for (q, expected) in queries.iter().enumerate() {
        let mut rng = query_rng(q);
        let vector = IrisVector::random(&mut rng);
        let mut query_share = vector.secret_share_5(&mut rng)?[party].clone();
        query_share.multiply_lagrange_coeffs_5(party + 1);

        let distances: Vec<RingElement<u16>> = db
            .iter()
            .map(|d| RingElement(udot(&query_share.0, d)))
            .collect();

        // "convert" covers turning the additive shares into shared adder
        // inputs: redistribution 5 -> 3 plus bit-plane input sharing (the
        // threshold subtraction in between is local).
        let before = counters.bytes();
        let mut input = rss5::redistribute(&mut sess, &distances).await?;
        rss5::sub_pub(&sess, &mut input, RingElement(THRESHOLD));
        let shared = rss5::share_adder_inputs(&mut sess, &input).await?;
        let after_convert = counters.bytes();
        let msb = rss5::extract_msb(&mut sess, shared).await?;
        let after_msb = counters.bytes();
        let bits = rss5::open_bit_plane(&mut sess, &msb, db_size).await?;
        let after_open = counters.bytes();

        stats.convert_bytes += after_convert - before;
        stats.msb_bytes += after_msb - after_convert;
        stats.open_bytes += after_open - after_msb;

        assert_eq!(
            &bits, expected,
            "5PC party {party} query {q}: result mismatch"
        );
    }
    stats.messages = counters.messages.load(Ordering::Relaxed);
    Ok(stats)
}

async fn run_3pc(db_size: usize, expected: &[Vec<bool>]) -> Result<Vec<PartyStats>> {
    let mut handles = Vec::new();
    for (session, counters) in counted_network_sessions(3) {
        let queries = expected.to_vec();
        handles.push(tokio::spawn(party_3(session, counters, db_size, queries)));
    }
    let mut stats = Vec::new();
    for h in handles {
        stats.push(h.await??);
    }
    Ok(stats)
}

async fn run_5pc(db_size: usize, expected: &[Vec<bool>]) -> Result<Vec<PartyStats>> {
    let mut handles = Vec::new();
    for (session, counters) in counted_network_sessions(5) {
        let queries = expected.to_vec();
        handles.push(tokio::spawn(party_5(session, counters, db_size, queries)));
    }
    let mut stats = Vec::new();
    for h in handles {
        stats.push(h.await??);
    }
    Ok(stats)
}

fn report(label: &str, corruptions: usize, stats: &[PartyStats], comparisons: u64) -> f64 {
    let n = stats.len();
    let sum = |f: fn(&PartyStats) -> u64| stats.iter().map(f).sum::<u64>();
    let convert = sum(|s| s.convert_bytes);
    let msb = sum(|s| s.msb_bytes);
    let open = sum(|s| s.open_bytes);
    let setup = sum(|s| s.setup_bytes);
    let messages = sum(|s| s.messages);
    let total = convert + msb + open;
    let per_cmp = total as f64 / comparisons as f64;

    println!("== {label} ({n} parties, semi-honest, tolerates {corruptions} corruption(s)) ==");
    println!("  results verified against plaintext: OK");
    println!("  bytes sent per comparison (sum over all parties): {per_cmp:.3}");
    println!(
        "    share adder inputs: {:.3}   msb extraction: {:.3}   open: {:.3}",
        convert as f64 / comparisons as f64,
        msb as f64 / comparisons as f64,
        open as f64 / comparisons as f64,
    );
    println!(
        "  per party average: {:.3} bytes/comparison",
        per_cmp / n as f64
    );
    println!(
        "  total protocol bytes: {total} ({messages} messages), one-time setup: {setup} bytes"
    );
    println!();
    per_cmp
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let db_size: usize = args
        .get(1)
        .map(|s| s.parse().expect("db_size must be an integer"))
        .unwrap_or(4096);
    let num_queries: usize = args
        .get(2)
        .map(|s| s.parse().expect("num_queries must be an integer"))
        .unwrap_or(2);
    assert!(
        db_size % 64 == 0,
        "db_size must be a multiple of 64 (bit-packing granularity)"
    );

    println!(
        "vector comparison via MPC: {IRIS_VECTOR_SIZE}-dim int4 vectors, \
         db_size={db_size}, queries={num_queries}, threshold={THRESHOLD}"
    );
    println!();

    // Plaintext reference results.
    let expected: Vec<Vec<bool>> = (0..num_queries)
        .map(|q| {
            let query = IrisVector::random(&mut query_rng(q));
            (0..db_size)
                .map(|i| {
                    let db_vec = IrisVector::random(&mut db_rng(i));
                    db_vec.dot(&query) < THRESHOLD as i16
                })
                .collect()
        })
        .collect();

    let comparisons = (db_size * num_queries) as u64;

    let stats3 = run_3pc(db_size, &expected).await?;
    let bytes3 = report("3PC baseline (ampc-actor-utils)", 1, &stats3, comparisons);

    let stats5 = run_5pc(db_size, &expected).await?;
    let bytes5 = report("5PC extension (rss5)", 2, &stats5, comparisons);

    println!(
        "5PC / 3PC communication ratio: {:.2}x ({:.3} vs {:.3} bytes per comparison)",
        bytes5 / bytes3,
        bytes5,
        bytes3
    );
    Ok(())
}
