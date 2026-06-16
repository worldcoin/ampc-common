use ampc_secret_sharing::{
    shares::{bit::Bit, share::AdditiveShare, vecshare::VecShareAdditive},
    IntRing2k, RingElement,
};
use eyre::{Error, Result};
use rand::Rng;
use rand_distr::{Distribution, Standard};

use crate::protocol::test_utils::create_single_sharing_additive;

// Offline randomness struct
#[derive(Debug, Clone)]
pub struct VecOfflineRandomSharesAdditive3pc<T: IntRing2k> {
    pub r: VecShareAdditive<T>,
    // todo: add r' here?
    pub r_bits: Vec<VecShareAdditive<Bit>>, // r_7, ..., r_0
    pub b_bit: VecShareAdditive<T>,
}

/// Offline randomness generator
/// a. sampling an instance of pre-generated randomness used in the protocol
/// b. Returns the per-party view of precomputed randomness as a struct
pub fn offline_shares_vec_for_role_additive_3pc<T: IntRing2k>(
    batch_size: usize,
    rng: &mut impl Rng,
) -> Result<Vec<VecOfflineRandomSharesAdditive3pc<T>>, Error>
where
    Standard: Distribution<T>,
{
    let mut offline_vec = Vec::with_capacity(3);
    let mut rand_bits_k_p0 = vec![Vec::with_capacity(batch_size); T::K];
    let mut r_batch_p0 = Vec::with_capacity(batch_size);
    let mut b_bit_batch_p0 = Vec::with_capacity(batch_size);

    let mut rand_bits_k_p1 = vec![Vec::with_capacity(batch_size); T::K];
    let mut r_batch_p1 = Vec::with_capacity(batch_size);
    let mut b_bit_batch_p1 = Vec::with_capacity(batch_size);

    let mut rand_bits_k_p2 = vec![Vec::with_capacity(batch_size); T::K];
    let mut r_batch_p2 = Vec::with_capacity(batch_size);
    let mut b_bit_batch_p2 = Vec::with_capacity(batch_size);

    (0..batch_size).for_each(|_| {
        let rand_bits: Vec<bool> = (0..T::K).map(|_| rng.gen_bool(0.5)).collect(); // sample random bits for r
        let total_value = rand_bits // compute r
            .iter()
            .rev()
            .enumerate()
            .fold(T::zero(), |acc, (i, bit)| {
                let multiplier = if i == 0 {
                    T::one()
                } else {
                    (T::one() + T::one()).wrapping_shl((i - 1) as u32)
                };
                acc + (T::from(*bit) * multiplier)
            });
        let total_value_shares = create_single_sharing_additive(rng, total_value); // shares of r
        let b_bit = rng.gen_bool(0.5); // sample b
        let b_bit_shares = create_single_sharing_additive(rng, T::from(b_bit)); // shares of b
        let rand_bit_shares: Vec<(bool, bool)> = rand_bits
            .iter()
            .map(|overall_bit| {
                let first_share = rng.gen_bool(0.5);
                let second_share = !(*overall_bit == first_share);
                (first_share, second_share)
            })
            .collect();
        (0..3).for_each(|party_idx| {
            match party_idx {
                // per role instantiate the struct for each party
                0 => {
                    rand_bit_shares
                        .clone()
                        .iter()
                        .enumerate()
                        .for_each(|(bit_num, (b0, _))| {
                            rand_bits_k_p0[bit_num]
                                .push(AdditiveShare::new(RingElement(Bit::new(*b0))));
                        });
                    r_batch_p0.push(total_value_shares.0);
                    b_bit_batch_p0.push(b_bit_shares.0);
                }
                1 => {
                    rand_bit_shares
                        .clone()
                        .iter()
                        .enumerate()
                        .for_each(|(bit_num, (_, b1))| {
                            rand_bits_k_p1[bit_num]
                                .push(AdditiveShare::new(RingElement(Bit::new(*b1))));
                        });
                    r_batch_p1.push(total_value_shares.1);
                    b_bit_batch_p1.push(b_bit_shares.1);
                }
                2 => {
                    rand_bit_shares
                        .clone()
                        .iter()
                        .enumerate()
                        .for_each(|(bit_num, _)| {
                            rand_bits_k_p2[bit_num].push(AdditiveShare::zero());
                        });
                    r_batch_p2.push(AdditiveShare::zero());
                    b_bit_batch_p2.push(AdditiveShare::zero());
                }
                _ => unreachable!(),
            };
        });
    });

    let r_bits_0: Vec<VecShareAdditive<Bit>> = rand_bits_k_p0
        .into_iter()
        .map(|inner_batch| VecShareAdditive::new_vec(inner_batch))
        .collect();
    let r_0 = VecShareAdditive::new_vec(r_batch_p0);
    let b_bit_0 = VecShareAdditive::new_vec(b_bit_batch_p0);
    let offline_0 = VecOfflineRandomSharesAdditive3pc {
        r_bits: r_bits_0,
        r: r_0,
        b_bit: b_bit_0,
    };
    offline_vec.push(offline_0);

    let r_bits_1: Vec<VecShareAdditive<Bit>> = rand_bits_k_p1
        .into_iter()
        .map(|inner_batch| VecShareAdditive::new_vec(inner_batch))
        .collect();
    let r_1 = VecShareAdditive::new_vec(r_batch_p1);
    let b_bit_1 = VecShareAdditive::new_vec(b_bit_batch_p1);
    let offline_1 = VecOfflineRandomSharesAdditive3pc {
        r_bits: r_bits_1,
        r: r_1,
        b_bit: b_bit_1,
    };
    offline_vec.push(offline_1);

    let r_bits_2: Vec<VecShareAdditive<Bit>> = rand_bits_k_p2
        .into_iter()
        .map(|inner_batch| VecShareAdditive::new_vec(inner_batch))
        .collect();
    let r_2 = VecShareAdditive::new_vec(r_batch_p2);
    let b_bit_2 = VecShareAdditive::new_vec(b_bit_batch_p2);
    let offline_2 = VecOfflineRandomSharesAdditive3pc {
        r_bits: r_bits_2,
        r: r_2,
        b_bit: b_bit_2,
    };
    offline_vec.push(offline_2);

    Ok(offline_vec)
}
