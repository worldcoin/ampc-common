use aes_prng::AesRng;
use criterion::{criterion_group, criterion_main, Criterion};
use rand::Rng;

/// Total number of dot products to compute per benchmark iteration.
const N: usize = 1_000_000;
/// Length of each vector, for face and DI we want 512, for IRIS it is 12800
const VEC_LEN: usize = 512;

/// Wrapping dot product of two `u16` vectors.
pub fn udot(a: &[u16], b: &[u16]) -> u16 {
    a.iter()
        .zip(b.iter())
        .map(|(&a, &b)| u16::wrapping_mul(a, b))
        .fold(0_u16, u16::wrapping_add)
}

fn udot_bench(c: &mut Criterion) {
    let mut rng = AesRng::from_random_seed();

    // Pre-generate a random DB of size N
    let db: Vec<Vec<u16>> = (0..N)
        .map(|_| (0..VEC_LEN).map(|_| rng.gen::<u16>()).collect())
        .collect();

    let query: Vec<u16> = (0..VEC_LEN).map(|_| rng.gen::<u16>()).collect();

    c.bench_function(
        &format!("{N} dot products. Element size: {VEC_LEN})"),
        |x| {
            // One query passing through the whole DB: compute one dot product

            x.iter(|| {
                db.iter()
                    .map(|db_vec| udot(&query, db_vec))
                    .collect::<Vec<u16>>()
            })
        },
    );
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(200);
    targets = udot_bench
}
criterion_main!(benches);
