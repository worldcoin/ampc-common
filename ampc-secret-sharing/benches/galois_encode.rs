use ampc_secret_sharing::{basis, GaloisRingElement, ShamirGaloisRingShare};
use criterion::{criterion_group, criterion_main, Criterion};
use rand::{rngs::StdRng, SeedableRng};
use std::hint::black_box;

fn bench_galois_encode(c: &mut Criterion) {
    let input = GaloisRingElement::<basis::Monomial>::from_coefs([0x1234, 0x5678, 0x9abc, 0xdef0]);
    let mut group = c.benchmark_group("shamir_galois_encode");

    group.bench_function("encode_3", |b| {
        let mut rng = StdRng::seed_from_u64(0);
        b.iter(|| black_box(ShamirGaloisRingShare::encode_3(black_box(&input), &mut rng)));
    });

    group.bench_function("encode_3_mat", |b| {
        let mut rng = StdRng::seed_from_u64(0);
        b.iter(|| {
            black_box(ShamirGaloisRingShare::encode_3_mat(
                black_box(&input.coefs),
                &mut rng,
            ))
        });
    });

    group.bench_function("encode_5", |b| {
        let mut rng = StdRng::seed_from_u64(0);
        b.iter(|| black_box(ShamirGaloisRingShare::encode_5(black_box(&input), &mut rng)));
    });

    group.bench_function("encode_5_mat", |b| {
        let mut rng = StdRng::seed_from_u64(0);
        b.iter(|| {
            black_box(ShamirGaloisRingShare::encode_5_mat(
                black_box(&input.coefs),
                &mut rng,
            ))
        });
    });

    group.finish();
}

criterion_group!(benches, bench_galois_encode);
criterion_main!(benches);
