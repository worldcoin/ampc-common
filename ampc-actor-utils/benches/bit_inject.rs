use ampc_actor_utils::protocol::Prf;
use ampc_secret_sharing::{
    shares::{VecRingElement, VecShare},
    IntRing2k, RingElement, Share,
};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use itertools::{izip, repeat_n, Itertools};
use num_traits::Zero;

pub fn bench_bit_inject(c: &mut Criterion) {
    let mut group = c.benchmark_group("bit_inject");
    let mut prf = Prf::default();
    let sizes = [256, 512, 1024, 2048];

    for &num_shares in sizes.iter() {
        let (mine, prev) = prf.gen_rands_batch(num_shares);
        let input1 = prf.gen_rands_mine(num_shares);
        let input2 = prf.gen_rands_mine(num_shares);
        group.bench_function(BenchmarkId::new("regular", num_shares), |b| {
            b.iter(|| {
                std::hint::black_box(test_bit_inject(
                    input1.clone(),
                    input2.clone(),
                    mine.clone(),
                    prev.clone(),
                ));
            });
        });
        group.bench_function(BenchmarkId::new("skip_zip", num_shares), |b| {
            b.iter(|| {
                std::hint::black_box(test_bit_inject2(
                    input1.clone(),
                    input2.clone(),
                    mine.clone(),
                    prev.clone(),
                ));
            });
        });
    }

    group.finish();
}

fn test_bit_inject(
    input1: VecRingElement<u32>,
    input2: VecRingElement<u32>,
    mine: VecRingElement<u32>,
    theirs: VecRingElement<u32>,
) -> VecShare<u32> {
    let len = input1.len();

    // Pack shares
    // By the end of Round 3, Party 1 holds the following shares:
    // - s1 = (x, r_01) of [b_0 XOR b_1]
    let s1 = Share::iter_from_iter_ab(input1.into_iter(), mine.into_iter());
    // - s2 = (0, 0) of [b_2] (we can ignore this shares as they are zero)
    // - s3 = (0, y) of [r_01 * b_2]
    let s3 = Share::iter_from_iter_ab(zero_iter(len), input2.into_iter());
    // - s4 = (r_12, 0) of [x * b_2]
    let s4 = Share::iter_from_iter_ab(theirs.into_iter(), zero_iter(len));

    // Local computation of the final shares:
    // [b_0 XOR b_1 XOR b_2] = [b_0 XOR b_1] + [b_2] - 2 * [(b_0 XOR b_1 ) * b_2]
    // = [b_0 XOR b_1] + [b_2] - 2 * ([r_01 * b_2] + [x * b_2])
    // = s1 - 2 * (s3 + s4)
    VecShare::new_vec(
        izip!(s1, s3, s4)
            .map(|(s1, s3, s4)| {
                let sum34 = s3 + s4;
                s1 - sum34 - sum34
            })
            .collect_vec(),
    )
}

fn test_bit_inject2(
    input1: VecRingElement<u32>,
    input2: VecRingElement<u32>,
    mine: VecRingElement<u32>,
    theirs: VecRingElement<u32>,
) -> VecShare<u32> {
    VecShare::new_vec(
        izip!(
            input1.into_iter(),
            mine.into_iter(),
            input2.into_iter(),
            theirs.into_iter()
        )
        .map(|(s1a, s1b, s3b, s4a)| {
            // Local computation of the final shares:
            // [b_0 XOR b_1 XOR b_2] = [b_0 XOR b_1] + [b_2] - 2 * [(b_0 XOR b_1 ) * b_2]
            // = [b_0 XOR b_1] + [b_2] - 2 * ([r_01 * b_2] + [x * b_2])
            // = s1 - 2 * (s3 + s4)

            let s1 = Share::new(s1a, s1b);
            let s3 = Share::new(RingElement::zero(), s3b);
            let s4 = Share::new(s4a, RingElement::zero());
            let sum34 = s3 + s4;
            s1 - sum34 - sum34
        })
        .collect_vec(),
    )
}

fn zero_iter<T: IntRing2k>(len: usize) -> impl Iterator<Item = RingElement<T>> {
    repeat_n(RingElement::<T>::zero(), len)
}

criterion_group!(benches, bench_bit_inject);
criterion_main!(benches);
