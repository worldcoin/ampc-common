//! Measurement harness: runs the same secret-shared vector comparison
//! workload under the 3-party protocol (the production implementation in
//! `ampc-actor-utils`) and the 5-party extension
//! (`ampc_actor_utils::protocol::rss5`), verifies the results against the
//! plaintext computation, and reports the number of bytes transferred per
//! vector comparison. Both the 16-bit pipeline and the 32-bit iris FHD
//! pipeline are measured for both party counts.
//!
//! All parties run in one process over `LocalNetworkingStore` channels
//! wrapped in a byte-counting `Networking` implementation, so the measured
//! bytes are the serialized `NetworkValue` messages the production stack
//! would hand to the transport (excluding TCP/TLS framing).
//!
//! The 3PC binary adder is selected by this crate's `ripple` feature
//! (byte-optimal ripple-carry adder); the default build uses the
//! parallel-prefix adder that `ampc-actor-utils` ships with. The 5PC
//! pipelines have a single (ripple-style) configuration.
//!
//! Usage: `vector-mpc-5pc [db_size] [num_queries]` (defaults: 4096, 2).

use ampc_actor_utils::execution::player::{Identity, Role, RoleAssignment};
use ampc_actor_utils::execution::session::{NetworkSession, Session, SessionHandles, SessionId};
use ampc_actor_utils::network::mpc::{
    local::{LocalNetworking, LocalNetworkingStore},
    NetworkValue, Networking,
};
use ampc_actor_utils::protocol::binary::{extract_msb_batch, open_bin};
use ampc_actor_utils::protocol::fhd_ops::{fhd_greater_than_threshold, translate_threshold_a};
use ampc_actor_utils::protocol::ops::{
    batch_signed_lift_vec, galois_ring_to_rep3, setup_replicated_prf, sub_pub, B,
};
use ampc_actor_utils::protocol::prf::PrfSeed;
use ampc_actor_utils::protocol::rss5;
use ampc_secret_sharing::iris_vector::{IrisVector, IRIS_VECTOR_SIZE};
use ampc_secret_sharing::shares::DistanceShare;
use ampc_secret_sharing::RingElement;
use async_trait::async_trait;
use eyre::Result;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const THRESHOLD: u16 = 1000;
const QUERY_SEED_BASE: u64 = 1_000_000_000;
const FHD_SEED_BASE: u64 = 3_000_000_000;
/// Iris-like fractional-Hamming-distance match threshold.
const FHD_THRESHOLD_RATIO: f64 = 0.375;
/// Iris-like bound on |code_dot| <= mask_dot (12800 mask bits).
const FHD_MASK_MAX: u16 = 12_800;

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

fn fhd_rng(query: usize, index: usize) -> StdRng {
    StdRng::seed_from_u64(FHD_SEED_BASE + (query as u64) * 100_000_000 + index as u64)
}

/// Additive 3-way sharing of a u16 value (mod 2^16).
fn split_additive(value: u16, rng: &mut StdRng) -> [u16; 3] {
    let s0: u16 = rng.gen();
    let s1: u16 = rng.gen();
    [s0, s1, value.wrapping_sub(s0).wrapping_sub(s1)]
}

/// Additive 5-way sharing of a u16 value (mod 2^16).
fn split_additive_5(value: u16, rng: &mut StdRng) -> [u16; 5] {
    let mut shares = [0u16; 5];
    let mut acc = value;
    for s in shares.iter_mut().take(4) {
        let r: u16 = rng.gen();
        *s = r;
        acc = acc.wrapping_sub(r);
    }
    shares[4] = acc;
    shares
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
    setup_messages: u64,
    convert_bytes: u64,
    lift_bytes: u64,
    msb_bytes: u64,
    open_bytes: u64,
    /// Protocol messages only (setup excluded, like the byte phases).
    messages: u64,
    /// Sequential one-way message legs per query (5PC pipelines only).
    legs: u64,
}

impl PartyStats {
    fn after_setup(counters: &Counters) -> Self {
        PartyStats {
            setup_bytes: counters.bytes(),
            setup_messages: counters.messages(),
            ..Default::default()
        }
    }

    fn finish(&mut self, counters: &Counters) {
        self.messages = counters.messages() - self.setup_messages;
    }
}

struct Counters {
    bytes: Arc<AtomicU64>,
    messages: Arc<AtomicU64>,
}

impl Counters {
    fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    fn messages(&self) -> u64 {
        self.messages.load(Ordering::Relaxed)
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
    let mut stats = PartyStats::after_setup(&counters);

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
    stats.finish(&counters);
    Ok(stats)
}

/// The production iris-mpc threshold comparison (the 32-bit lifted FHD
/// pipeline that `iris-mpc-cpu` imports from `ampc-actor-utils`): rep3-share
/// the (code, mask) dot products, lift both to u32 with
/// `batch_signed_lift_vec`, multiply by the public threshold constants and
/// extract the sign (`fhd_greater_than_threshold`), then open. Synthetic
/// in-range dot values stand in for the Galois-ring dot products; message
/// sizes are identical.
async fn party_3_fhd(
    mut network_session: NetworkSession,
    counters: Counters,
    db_size: usize,
    num_queries: usize,
) -> Result<PartyStats> {
    let party = network_session.own_role().index();
    let seed: PrfSeed = rand::thread_rng().gen();
    let prf = setup_replicated_prf(&mut network_session, seed).await?;
    let mut sess = Session {
        network_session,
        prf,
    };
    let mut stats = PartyStats::after_setup(&counters);

    let a = translate_threshold_a(FHD_THRESHOLD_RATIO);
    for q in 0..num_queries {
        // Interleaved [code, mask] additive shares, as the production
        // pipeline lifts them in one batch.
        let mut adds = Vec::with_capacity(2 * db_size);
        let mut expected = Vec::with_capacity(db_size);
        for i in 0..db_size {
            let mut rng = fhd_rng(q, i);
            let mask: u16 = rng.gen_range(1..=FHD_MASK_MAX);
            let code: i16 = rng.gen_range(-(mask as i32)..=mask as i32) as i16;
            let code_shares = split_additive(code as u16, &mut rng);
            let mask_shares = split_additive(mask, &mut rng);
            adds.push(RingElement(code_shares[party]));
            adds.push(RingElement(mask_shares[party]));
            // fhd_greater_than_threshold opens the sign of
            // code_dot * B - mask_dot * A (1 = NOT a match).
            expected.push((code as i64) * (B as i64) < (mask as i64) * (a as i64));
        }

        let before = counters.bytes();
        let rep = galois_ring_to_rep3(&mut sess, adds).await?;
        let after_convert = counters.bytes();
        let lifted = batch_signed_lift_vec(&mut sess, rep).await?;
        let after_lift = counters.bytes();
        let distances: Vec<DistanceShare<u32>> = lifted
            .chunks(2)
            .map(|pair| DistanceShare::new(pair[0], pair[1]))
            .collect();
        let msb = fhd_greater_than_threshold(&mut sess, &distances, FHD_THRESHOLD_RATIO).await?;
        let after_msb = counters.bytes();
        let bits: Vec<bool> = open_bin(&mut sess, &msb)
            .await?
            .into_iter()
            .map(|b| b.convert())
            .collect();
        let after_open = counters.bytes();

        stats.convert_bytes += after_convert - before;
        stats.lift_bytes += after_lift - after_convert;
        stats.msb_bytes += after_msb - after_lift;
        stats.open_bytes += after_open - after_msb;

        assert_eq!(
            &bits, &expected,
            "3PC FHD party {party} query {q}: result mismatch"
        );
    }
    stats.finish(&counters);
    Ok(stats)
}

async fn party_5(
    network_session: NetworkSession,
    counters: Counters,
    db_size: usize,
    queries: Vec<Vec<bool>>,
    adder: rss5::AdderKind,
    reshare: rss5::ReshareMode,
) -> Result<PartyStats> {
    let party = network_session.own_role().index();
    let seed: PrfSeed = rand::thread_rng().gen();
    let mut sess = rss5::setup_rss5_session(network_session, seed).await?;
    sess.set_reshare_mode(reshare);
    let mut stats = PartyStats::after_setup(&counters);

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
        let msb = rss5::extract_msb_with(&mut sess, shared, adder).await?;
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
    stats.legs = sess.legs() / queries.len().max(1) as u64;
    stats.finish(&counters);
    Ok(stats)
}

/// The 5-party FHD threshold comparison (`rss5`): redistribute the code and
/// mask dot products to 3 clear addends each, input-share the local 32-plane
/// difference addends, compute the mask addends' wrap bits (the counterpart
/// of the 3PC lift), sum with wrap correction and open the sign. Same
/// synthetic workload and phase boundaries as [`party_3_fhd`].
async fn party_5_fhd(
    network_session: NetworkSession,
    counters: Counters,
    db_size: usize,
    num_queries: usize,
) -> Result<PartyStats> {
    let party = network_session.own_role().index();
    let seed: PrfSeed = rand::thread_rng().gen();
    let mut sess = rss5::setup_rss5_session(network_session, seed).await?;
    let mut stats = PartyStats::after_setup(&counters);

    let a = translate_threshold_a(FHD_THRESHOLD_RATIO);
    for q in 0..num_queries {
        let mut code_adds = Vec::with_capacity(db_size);
        let mut mask_adds = Vec::with_capacity(db_size);
        let mut expected = Vec::with_capacity(db_size);
        for i in 0..db_size {
            let mut rng = fhd_rng(q, i);
            let mask: u16 = rng.gen_range(1..=FHD_MASK_MAX);
            let code: i16 = rng.gen_range(-(mask as i32)..=mask as i32) as i16;
            let code_shares = split_additive_5(code as u16, &mut rng);
            let mask_shares = split_additive_5(mask, &mut rng);
            code_adds.push(RingElement(code_shares[party]));
            mask_adds.push(RingElement(mask_shares[party]));
            expected.push((code as i64) * (B as i64) < (mask as i64) * (a as i64));
        }

        let before = counters.bytes();
        let code_in = rss5::redistribute(&mut sess, &code_adds).await?;
        let mask_in = rss5::redistribute(&mut sess, &mask_adds).await?;
        let diff =
            rss5::fhd_diff_inputs(&mut sess, &code_in, &mask_in, FHD_THRESHOLD_RATIO).await?;
        let after_convert = counters.bytes();
        let wraps = rss5::addend_wraps(&mut sess, &mask_in).await?;
        let after_lift = counters.bytes();
        let msb = rss5::fhd_extract_msb(&mut sess, diff, wraps).await?;
        let after_msb = counters.bytes();
        let bits = rss5::open_bit_plane(&mut sess, &msb, db_size).await?;
        let after_open = counters.bytes();

        stats.convert_bytes += after_convert - before;
        stats.lift_bytes += after_lift - after_convert;
        stats.msb_bytes += after_msb - after_lift;
        stats.open_bytes += after_open - after_msb;

        assert_eq!(
            &bits, &expected,
            "5PC FHD party {party} query {q}: result mismatch"
        );
    }
    stats.legs = sess.legs() / num_queries.max(1) as u64;
    stats.finish(&counters);
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

async fn run_3pc_fhd(db_size: usize, num_queries: usize) -> Result<Vec<PartyStats>> {
    let mut handles = Vec::new();
    for (session, counters) in counted_network_sessions(3) {
        handles.push(tokio::spawn(party_3_fhd(
            session,
            counters,
            db_size,
            num_queries,
        )));
    }
    let mut stats = Vec::new();
    for h in handles {
        stats.push(h.await??);
    }
    Ok(stats)
}

async fn run_5pc(
    db_size: usize,
    expected: &[Vec<bool>],
    adder: rss5::AdderKind,
    reshare: rss5::ReshareMode,
) -> Result<Vec<PartyStats>> {
    let mut handles = Vec::new();
    for (session, counters) in counted_network_sessions(5) {
        let queries = expected.to_vec();
        handles.push(tokio::spawn(party_5(
            session, counters, db_size, queries, adder, reshare,
        )));
    }
    let mut stats = Vec::new();
    for h in handles {
        stats.push(h.await??);
    }
    Ok(stats)
}

async fn run_5pc_fhd(db_size: usize, num_queries: usize) -> Result<Vec<PartyStats>> {
    let mut handles = Vec::new();
    for (session, counters) in counted_network_sessions(5) {
        handles.push(tokio::spawn(party_5_fhd(
            session,
            counters,
            db_size,
            num_queries,
        )));
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
    let lift = sum(|s| s.lift_bytes);
    let msb = sum(|s| s.msb_bytes);
    let open = sum(|s| s.open_bytes);
    let setup = sum(|s| s.setup_bytes);
    let setup_messages = sum(|s| s.setup_messages);
    let messages = sum(|s| s.messages);
    let total = convert + lift + msb + open;
    let per_cmp = total as f64 / comparisons as f64;

    println!("== {label} ({n} parties, semi-honest, tolerates {corruptions} corruption(s)) ==");
    println!("  results verified against plaintext: OK");
    println!("  bytes sent per comparison (sum over all parties): {per_cmp:.3}");
    let lift_part = if lift > 0 {
        format!("   lift to u32: {:.3}", lift as f64 / comparisons as f64)
    } else {
        String::new()
    };
    println!(
        "    share inputs: {:.3}{}   msb extraction: {:.3}   open: {:.3}",
        convert as f64 / comparisons as f64,
        lift_part,
        msb as f64 / comparisons as f64,
        open as f64 / comparisons as f64,
    );
    println!(
        "  per party average: {:.3} bytes/comparison",
        per_cmp / n as f64
    );
    let per_party: Vec<String> = stats
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let party_total = s.convert_bytes + s.lift_bytes + s.msb_bytes + s.open_bytes;
            format!("P{i}: {:.3}", party_total as f64 / comparisons as f64)
        })
        .collect();
    println!("  per party breakdown: {}", per_party.join("  "));
    if stats[0].legs > 0 {
        println!("  sequential message legs per query: {}", stats[0].legs);
    }
    println!(
        "  total protocol bytes: {total} ({messages} messages), \
         one-time setup: {setup} bytes ({setup_messages} messages)"
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

    let adder = if cfg!(feature = "ripple") {
        "ripple-carry (byte-optimal)"
    } else {
        "parallel-prefix (crate default)"
    };
    println!(
        "vector comparison via MPC: {IRIS_VECTOR_SIZE}-dim int4 vectors, \
         db_size={db_size}, queries={num_queries}, threshold={THRESHOLD}, \
         3PC adder: {adder}"
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

    let stats5 = run_5pc(
        db_size,
        &expected,
        rss5::AdderKind::Ripple,
        rss5::ReshareMode::Redistribute,
    )
    .await?;
    let bytes5 = report("5PC extension (rss5)", 2, &stats5, comparisons);

    println!(
        "5PC / 3PC communication ratio: {:.2}x ({:.3} vs {:.3} bytes per comparison)",
        bytes5 / bytes3,
        bytes5,
        bytes3
    );
    println!();

    // Bytes-vs-rounds matrix over the adder and resharing variants.
    println!("== 5PC 16-bit adder/resharing variants (bytes vs critical-path legs) ==");
    for (adder, reshare, label) in [
        (
            rss5::AdderKind::Ripple,
            rss5::ReshareMode::Redistribute,
            "ripple + redistribute-reshare (byte-optimal)",
        ),
        (
            rss5::AdderKind::Ripple,
            rss5::ReshareMode::Direct,
            "ripple + direct-reshare",
        ),
        (
            rss5::AdderKind::PrefixTree,
            rss5::ReshareMode::Redistribute,
            "prefix + redistribute-reshare",
        ),
        (
            rss5::AdderKind::PrefixTree,
            rss5::ReshareMode::Direct,
            "prefix + direct-reshare (round-optimal)",
        ),
    ] {
        let stats = run_5pc(db_size, &expected, adder, reshare).await?;
        let total: u64 = stats
            .iter()
            .map(|s| s.convert_bytes + s.lift_bytes + s.msb_bytes + s.open_bytes)
            .sum();
        println!(
            "  {label:<46} {:>7.3} B/comparison, {:>2} legs/query (verified: OK)",
            total as f64 / comparisons as f64,
            stats[0].legs,
        );
    }
    println!();

    let stats_fhd = run_3pc_fhd(db_size, num_queries).await?;
    let bytes_fhd = report(
        "3PC iris FHD pipeline (32-bit lifted, as used by iris-mpc)",
        1,
        &stats_fhd,
        comparisons,
    );
    println!(
        "32-bit FHD / 16-bit 3PC ratio: {:.2}x ({:.3} vs {:.3} bytes per comparison)",
        bytes_fhd / bytes3,
        bytes_fhd,
        bytes3
    );
    println!();

    let stats_fhd5 = run_5pc_fhd(db_size, num_queries).await?;
    let bytes_fhd5 = report(
        "5PC iris FHD pipeline (32-bit, rss5)",
        2,
        &stats_fhd5,
        comparisons,
    );
    println!(
        "5PC / 3PC FHD communication ratio: {:.2}x ({:.3} vs {:.3} bytes per comparison)",
        bytes_fhd5 / bytes_fhd,
        bytes_fhd5,
        bytes_fhd
    );
    Ok(())
}
