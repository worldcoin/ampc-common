use std::sync::Arc;

use aes_prng::AesRng;
use ampc_actor_utils::{
    execution::{local::LocalRuntime, session::Session},
    protocol::{fhd_ops::cross_compare, ops::batch_signed_lift_vec},
};
use ampc_secret_sharing::{shares::share::DistanceShare, IntRing2k, RingElement, Share};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use rand::{Rng, RngCore, SeedableRng};
use rand_distr::{Distribution, Standard};
use tokio::{sync::Mutex, task::JoinSet};

criterion_group!(
    networking,
    bench_is_match_batch_tcp,
    bench_is_match_batch_grpc,
);
criterion_main!(networking);

const STREAM_PARALLELISM: usize = 16;

pub fn create_random_sharing<R, ShareRing>(rng: &mut R, input: ShareRing) -> Vec<Share<ShareRing>>
where
    R: RngCore,
    ShareRing: IntRing2k + std::fmt::Display,
    Standard: Distribution<ShareRing>,
{
    let val = RingElement(input);
    let a = RingElement(rng.gen());
    let b = RingElement(rng.gen());
    let c = val - a - b;

    let share1 = Share::new(a, c);
    let share2 = Share::new(b, a);
    let share3 = Share::new(c, b);

    vec![share1, share2, share3]
}

async fn run_jobs(
    num_iterations: usize,
    sessions: &[Arc<Mutex<Session>>],
    d1: Vec<Share<u16>>,
    d2: Vec<Share<u16>>,
    t1: Vec<Share<u16>>,
    t2: Vec<Share<u16>>,
) {
    let mut jobs = JoinSet::new();
    for (index, player_session) in sessions.iter().enumerate() {
        // each vec of shares is of length 3 - per 3PC. the sessions were created
        // so that if split by 3, each chunk has the same session id, and each idx
        // corresponds to a party.
        let d1i = d1[index % 3].clone();
        let d2i = d2[index % 3].clone();
        let t1i = t1[index % 3].clone();
        let t2i = t2[index % 3].clone();
        let player_session = player_session.clone();
        jobs.spawn(async move {
            let mut player_session = player_session.lock().await;
            for _ in 0..num_iterations {
                let ds_and_ts = batch_signed_lift_vec(
                    &mut player_session,
                    vec![d1i.clone(), d2i.clone(), t1i.clone(), t2i.clone()],
                )
                .await
                .unwrap();
                cross_compare(
                    &mut player_session,
                    &[(
                        DistanceShare::new(ds_and_ts[0].clone(), ds_and_ts[1].clone()),
                        DistanceShare::new(ds_and_ts[2].clone(), ds_and_ts[3].clone()),
                    )],
                )
                .await
                .unwrap();
            }
        });
    }
    let _outputs = black_box(jobs.join_all().await);
}

fn bench_is_match_batch_tcp(c: &mut Criterion) {
    let mut group = c.benchmark_group("is_match_batch_tcp");
    group.sample_size(10);

    // Pin to 3 worker threads (one per party) so the 3PC parties actually
    // contend for cores. This is the point of the benchmark: measure behaviour
    // when the CPU is saturated rather than letting Tokio fan out across all
    // logical cores and hide the contention.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()
        .unwrap();

    #[allow(clippy::single_element_loop)]
    for (nj, rp) in [(1024, STREAM_PARALLELISM)] {
        {
            let cp = 1;
            let mut rng = AesRng::seed_from_u64(0_u64);
            let d1 = create_random_sharing(&mut rng, 10_u16);
            let d2 = create_random_sharing(&mut rng, 10_u16);
            let t1 = create_random_sharing(&mut rng, 10_u16);
            let t2 = create_random_sharing(&mut rng, 10_u16);

            let sessions = rt
                .block_on(async move { LocalRuntime::mock_sessions_with_tcp(cp, rp).await })
                .unwrap();

            let num_parties = 3;
            assert_eq!(sessions.len(), rp * num_parties);

            group.bench_function(
                BenchmarkId::new("local", format!("cp: {}, rp: {}, nj: {}", cp, rp, nj)),
                |b| {
                    b.iter(|| {
                        let (d1, d2, t1, t2) = (d1.clone(), d2.clone(), t1.clone(), t2.clone());
                        let sessions = &sessions;
                        rt.block_on(async move {
                            for _ in 0..nj / rp {
                                run_jobs(
                                    1,
                                    sessions,
                                    d1.clone(),
                                    d2.clone(),
                                    t1.clone(),
                                    t2.clone(),
                                )
                                .await;
                            }
                        });
                    })
                },
            );
        }
    }
}

fn bench_is_match_batch_grpc(c: &mut Criterion) {
    let mut group = c.benchmark_group("is_match_batch");
    group.sample_size(10);

    // Pin to 3 worker threads (one per party) so the 3PC parties actually
    // contend for cores. This is the point of the benchmark: measure behaviour
    // when the CPU is saturated rather than letting Tokio fan out across all
    // logical cores and hide the contention.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()
        .unwrap();

    #[allow(clippy::single_element_loop)]
    for nj in [1024] {
        #[allow(clippy::single_element_loop)]
        for (cp, rp) in [
            (STREAM_PARALLELISM, STREAM_PARALLELISM),
            (1, STREAM_PARALLELISM),
        ] {
            let mut rng = AesRng::seed_from_u64(0_u64);
            let d1 = create_random_sharing(&mut rng, 10_u16);
            let d2 = create_random_sharing(&mut rng, 10_u16);
            let t1 = create_random_sharing(&mut rng, 10_u16);
            let t2 = create_random_sharing(&mut rng, 10_u16);

            // `sp` (stream parallelism) is derived internally in ampc-common as
            // `request_parallelism / connection_parallelism`, so it is not passed
            // explicitly; it is retained in the loop/label for documentation.
            let sessions = rt
                .block_on(async move { LocalRuntime::mock_sessions_with_grpc(cp, rp).await })
                .unwrap();

            let num_parties = 3;
            assert_eq!(sessions.len(), rp * num_parties);

            group.bench_function(
                BenchmarkId::new("local", format!("cp: {}, rp: {}, nj: {}", cp, rp, nj)),
                |b| {
                    b.iter(|| {
                        let (d1, d2, t1, t2) = (d1.clone(), d2.clone(), t1.clone(), t2.clone());
                        let sessions = &sessions;
                        rt.block_on(async move {
                            for _ in 0..nj / rp {
                                run_jobs(
                                    1,
                                    sessions,
                                    d1.clone(),
                                    d2.clone(),
                                    t1.clone(),
                                    t2.clone(),
                                )
                                .await;
                            }
                        });
                    })
                },
            );
        }
    }
}
