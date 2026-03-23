use aes_prng::AesRng;
use ampc_actor_utils::execution::local::LocalRuntime;
use ampc_actor_utils::protocol::ops::{
    conditionally_select_distance, conditionally_swap_distances, DistancePair,
};
use ampc_actor_utils::protocol::test_utils::create_array_sharing;
use ampc_secret_sharing::shares::bit::Bit;
use ampc_secret_sharing::shares::share::DistanceShare;
use ampc_secret_sharing::Share;
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use itertools::Itertools;
use rand::SeedableRng;
use tokio::task::JoinSet;

fn bench_select_distance(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("conditionally_select_distance");
    let sizes = [256, 1024, 4096];

    for &n in &sizes {
        // Create secret-shared distances and control bits.
        // Each distance pair has code_dot and mask_dot (4 values per pair),
        // plus one control bit per pair.
        let mut rng = AesRng::seed_from_u64(42);
        let vals: Vec<u32> = (0..(4 * n + n) as u32).collect();
        let shares = create_array_sharing(&mut rng, &vals);

        group.bench_function(BenchmarkId::new("e2e", n), |b| {
            b.iter_batched(
                || {
                    let sessions = rt
                        .block_on(LocalRuntime::mock_sessions_with_channel())
                        .unwrap();
                    sessions.into_iter().enumerate().map(|(i, session)| {
                        let s = shares.of_party(i).clone();
                        let distances: Vec<DistancePair<u32>> = (0..n)
                            .map(|j| {
                                (
                                    DistanceShare::new(s[j], s[n + j]),
                                    DistanceShare::new(s[2 * n + j], s[3 * n + j]),
                                )
                            })
                            .collect();
                        let control_bits: Vec<Share<u32>> = (0..n).map(|j| s[4 * n + j]).collect();
                        (session, distances, control_bits)
                    }).collect::<Vec<_>>()
                },
                |party_inputs| {
                    rt.block_on(async {
                        let mut jobs = JoinSet::new();

                        for (session, distances, control_bits) in party_inputs.into_iter() {
                            jobs.spawn(async move {
                                let mut session = session.lock().await;

                                conditionally_select_distance(
                                    &mut session,
                                    &distances,
                                    &control_bits,
                                )
                                .await
                                .unwrap()
                            });
                        }
                        jobs.join_all().await
                    })
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

fn bench_swap_distances(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("conditionally_swap_distances");
    let sizes = [128, 512, 2048];

    for &num_pairs in &sizes {
        let list_size = num_pairs * 2;
        let mut rng = AesRng::seed_from_u64(43);

        // Each list entry: (id, code_dot, mask_dot) = 3 values per entry
        let vals: Vec<u32> = (0..(3 * list_size) as u32).collect();
        let shares = create_array_sharing(&mut rng, &vals);

        let bit_vals: Vec<Bit> = (0..num_pairs).map(|i| Bit::new(i % 2 == 0)).collect();
        let bit_shares = create_array_sharing(&mut rng, &bit_vals);

        let indices: Vec<(usize, usize)> = (0..list_size).tuples().collect();

        group.bench_function(BenchmarkId::new("e2e", num_pairs), |b| {
            b.iter_batched(
                || {
                    rt.block_on(LocalRuntime::mock_sessions_with_channel())
                        .unwrap()
                },
                |sessions| {
                    rt.block_on(async {
                        let mut jobs = JoinSet::new();

                        for (i, session) in sessions.into_iter().enumerate() {
                            let s = shares.of_party(i).clone();
                            let bs = bit_shares.of_party(i).clone();
                            let indices = indices.clone();
                            jobs.spawn(async move {
                                let mut session = session.lock().await;
                                let list: Vec<(Share<u32>, DistanceShare<u32>)> = (0..list_size)
                                    .map(|j| {
                                        (
                                            s[j],
                                            DistanceShare::new(
                                                s[list_size + j],
                                                s[2 * list_size + j],
                                            ),
                                        )
                                    })
                                    .collect();

                                conditionally_swap_distances(&mut session, bs, &list, &indices)
                                    .await
                                    .unwrap()
                            });
                        }
                        jobs.join_all().await
                    })
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_select_distance, bench_swap_distances);
criterion_main!(benches);
