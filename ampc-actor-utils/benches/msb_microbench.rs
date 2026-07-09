use std::ops::Neg;

use aes_prng::AesRng;
use ampc_actor_utils::{
    execution::player::Role,
    protocol::{prf::PrfRng, Prf},
};
use ampc_secret_sharing::{
    shares::{
        bit::Bit, primefield::PrimeElement, share::AdditiveSharePrime,
        vecshare::VecShareAdditivePrime,
    },
    RingElement,
};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use itertools::{izip, repeat_n, Itertools};
use rand::{Rng, SeedableRng};

pub fn bench_aes_rng_primefield(c: &mut Criterion) {
    let mut group = c.benchmark_group("msb_microbench");

    let sizes = [1_000_000];

    for &num_shares in sizes.iter() {
        let values: Vec<_> = (0..num_shares)
            .map(|val| RingElement::<Bit>(Bit::new(val % 2 == 0)))
            .collect();

        let prefix_sum_input: Vec<VecShareAdditivePrime<u16>> = (0..16)
            .map(|bit_idx| {
                VecShareAdditivePrime::new_vec(
                    (0..num_shares)
                        .map(|val| {
                            AdditiveSharePrime::new(PrimeElement::<u16>::new(
                                (val + bit_idx) as u16,
                                37,
                            ))
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect();

        group.bench_function(BenchmarkId::new("prefix_sum", num_shares), |b| {
            b.iter(|| {
                std::hint::black_box(prefix_sum(&prefix_sum_input));
            });
        });

        group.bench_function(BenchmarkId::new("entire_primefield_gen", num_shares), |b| {
            b.iter(|| {
                std::hint::black_box(bin_to_primefield16_dealer(&values));
            });
        });

        group.bench_function(BenchmarkId::new("aes_and_mod", num_shares), |b| {
            b.iter(|| {
                std::hint::black_box(aes_and_mod_op(num_shares));
            });
        });

        group.bench_function(BenchmarkId::new("chacha_and_mod", num_shares), |b| {
            b.iter(|| {
                std::hint::black_box(chacha_and_mod_op(num_shares));
            });
        });

        group.bench_function(BenchmarkId::new("just_mod", num_shares), |b| {
            b.iter(|| {
                std::hint::black_box(mod_op(num_shares));
            });
        });
    }

    group.finish();
}

fn prefix_sum(prime_shares_received: &[VecShareAdditivePrime<u16>]) {
    let prime_modulus: u16 = 37;
    let num_in_batch = prime_shares_received[0].len();
    let num_bits = prime_shares_received.len();
    let shared_seed: [u8; 16] = [0; 16];
    let mut rng = PrfRng::from_seed(Prf::expand_seed(shared_seed));

    // Prefix sum
    let mut prefix_sum = Vec::with_capacity(prime_shares_received.len());
    let mut running_sum =
        VecShareAdditivePrime::new_vec(vec![AdditiveSharePrime::zero(prime_modulus); num_in_batch]);
    prime_shares_received.iter().for_each(|share| {
        running_sum += share;
        prefix_sum.push(running_sum.clone());
    });

    // Pairwise sum
    let mut pairwise_sum = Vec::with_capacity(prime_shares_received.len());
    pairwise_sum.push(prefix_sum[0].clone());
    (1..prefix_sum.len()).for_each(|i| {
        pairwise_sum.push(prefix_sum[i - 1].clone() + prefix_sum[i].clone());
    });

    // Subtract 1
    pairwise_sum.iter_mut().for_each(|share| {
        share.add_assign_const_role(
            vec![PrimeElement::<u16>::one(prime_modulus).neg(); num_in_batch],
            Role::new(0),
        );
    });

    // Scale prime shares
    pairwise_sum.iter_mut().for_each(|share| {
        let scalar =
            vec![PrimeElement::<u16>::rand_multiplicative(&mut rng, prime_modulus); num_in_batch];
        *share *= scalar;
    });

    // Shift shares
    let rand_shift = rng.gen_range(0..num_bits);
    let mut shifted_shares = Vec::with_capacity(num_bits);
    (0..pairwise_sum.len()).for_each(|i| {
        shifted_shares.push(pairwise_sum[(i + rand_shift) % pairwise_sum.len()].clone());
    });
}

fn bin_to_primefield16_dealer(
    values: &[RingElement<Bit>],
) -> (
    Vec<AdditiveSharePrime<PrimeElement<u16>>>,
    Vec<AdditiveSharePrime<PrimeElement<u16>>>,
) {
    let mut rng = AesRng::from_entropy();
    let modulus = 37;
    let (shares_0, shares_1): (
        Vec<AdditiveSharePrime<PrimeElement<u16>>>,
        Vec<AdditiveSharePrime<PrimeElement<u16>>>,
    ) = values
        .iter()
        .map(|value| {
            let bit_as_mod19 = PrimeElement::<u16>::new(u8::from(value.convert()) as u16, modulus);
            let rand_mod19_share = PrimeElement::<u16>::rand(&mut rng, modulus);
            let other_share = bit_as_mod19 - rand_mod19_share;
            (
                AdditiveSharePrime::new(other_share),
                AdditiveSharePrime::new(rand_mod19_share),
            )
        })
        .unzip();
    (shares_0, shares_1)
}

fn aes_and_mod_op(num_elements: usize) -> Vec<PrimeElement<u16>> {
    let mut rng = AesRng::from_entropy();
    let modulus = 37;

    let res: Vec<_> = (0..num_elements)
        .map(|_| PrimeElement::<u16>::rand(&mut rng, modulus))
        .collect();

    res
}

fn chacha_and_mod_op(num_elements: usize) -> Vec<PrimeElement<u16>> {
    let mut rng = PrfRng::from_entropy();
    let modulus = 37;

    let res: Vec<_> = (0..num_elements)
        .map(|_| PrimeElement::<u16>::rand(&mut rng, modulus))
        .collect();

    res
}

fn mod_op(num_elements: usize) -> Vec<PrimeElement<u16>> {
    let modulus = 37;

    let res: Vec<_> = (0..num_elements)
        .map(|val| PrimeElement::<u16>::new(val as u16, modulus))
        .collect();

    res
}

criterion_group!(benches, bench_aes_rng_primefield);
criterion_main!(benches);
