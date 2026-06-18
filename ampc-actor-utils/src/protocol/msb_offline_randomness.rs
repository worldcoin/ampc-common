use ampc_secret_sharing::{
    shares::{bit::Bit, share::AdditiveShare},
    IntRing2k, RingElement,
};
use eyre::{Error, Result};
use itertools::Itertools;
use rand::Rng;
use rand_distr::{Distribution, Standard};
use serde::{Deserialize, Serialize};

use crate::protocol::test_utils::create_single_sharing_additive;

// Offline randomness struct
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct OfflineRandomSharesAdditive3pc<T: IntRing2k> {
    pub r: AdditiveShare<T>,
    // todo: add r' here?
    pub r_bits: Vec<AdditiveShare<Bit>>, // r_7, ..., r_0
    pub b_bit: AdditiveShare<T>,
}

/// Offline randomness generator
/// a. sampling an instance of pre-generated randomness used in the protocol
/// b. Returns the per-party view of precomputed randomness as a struct
pub fn offline_shares_for_role_additive_3pc<T: IntRing2k>(
    rng: &mut impl Rng,
) -> Result<Vec<OfflineRandomSharesAdditive3pc<T>>, Error>
where
    Standard: Distribution<T>,
{
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
    let rand_bit_shares = rand_bits
        .iter()
        .map(move |overall_bit| {
            let first_share = rng.gen_bool(0.5);
            let second_share = !(*overall_bit == first_share);
            (first_share, second_share)
        })
        .collect_vec();

    let mut offline_shares = Vec::with_capacity(3);
    (0..3).for_each(|party_idx| {
        let offline_struct = match party_idx {
            // per role instantiate the struct for each party
            0 => OfflineRandomSharesAdditive3pc {
                r: total_value_shares.0,
                r_bits: rand_bit_shares
                    .iter()
                    .map(|(b0, _)| AdditiveShare::new(RingElement(Bit::new(*b0))))
                    .collect(),
                b_bit: b_bit_shares.0,
            },
            1 => OfflineRandomSharesAdditive3pc {
                r: total_value_shares.1,
                r_bits: rand_bit_shares
                    .iter()
                    .map(|(_, b1)| AdditiveShare::new(RingElement(Bit::new(*b1))))
                    .collect(),
                b_bit: b_bit_shares.1,
            },
            2 => OfflineRandomSharesAdditive3pc {
                r: AdditiveShare::<T>::zero(),
                r_bits: rand_bit_shares
                    .iter()
                    .map(|_| AdditiveShare::zero())
                    .collect(),
                b_bit: AdditiveShare::zero(),
            },
            _ => unreachable!(),
        };
        offline_shares.push(offline_struct);
    });
    Ok(offline_shares)
}
