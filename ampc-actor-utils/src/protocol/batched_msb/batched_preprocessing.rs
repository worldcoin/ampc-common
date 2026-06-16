use ampc_secret_sharing::{
    shares::{bit::Bit, share::AdditiveShare, vecshare::VecShareAdditive},
    IntRing2k, RingElement, Role,
};
use eyre::{bail, Error, Result};
use rand::Rng;
use rand_distr::{Distribution, Standard};

use crate::protocol::test_utils::create_single_sharing_additive;

// Offline randomness struct
#[derive(Debug)]
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
    role: &impl Role,
    rng: &mut impl Rng,
) -> Result<VecOfflineRandomSharesAdditive3pc<T>, Error>
where
    Standard: Distribution<T>,
{
    let mut rand_bits_k = vec![Vec::with_capacity(batch_size); T::K];
    let mut r_batch = Vec::with_capacity(batch_size);
    let mut b_bit_batch = Vec::with_capacity(batch_size);

    let _: Vec<_> = (0..batch_size)
        .map(|_| {
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
            let rand_bit_shares = rand_bits.iter().map(|overall_bit| {
                let first_share = rng.gen_bool(0.5);
                let second_share = !(*overall_bit == first_share);
                (first_share, second_share)
            });
            let _ = match role.index() {
                // per role instantiate the struct for each party
                0 => {
                    rand_bit_shares.enumerate().for_each(|(bit_num, (b0, _))| {
                        rand_bits_k[bit_num].push(AdditiveShare::new(RingElement(Bit::new(b0))));
                    });
                    r_batch.push(total_value_shares.0);
                    b_bit_batch.push(b_bit_shares.0);
                    Ok::<_, Error>(())
                }
                1 => {
                    rand_bit_shares.enumerate().for_each(|(bit_num, (_, b1))| {
                        rand_bits_k[bit_num].push(AdditiveShare::new(RingElement(Bit::new(b1))));
                    });
                    r_batch.push(total_value_shares.1);
                    b_bit_batch.push(b_bit_shares.1);
                    Ok(())
                }
                2 => {
                    rand_bit_shares.enumerate().for_each(|(bit_num, _)| {
                        rand_bits_k[bit_num].push(AdditiveShare::zero());
                    });
                    r_batch.push(AdditiveShare::zero());
                    b_bit_batch.push(AdditiveShare::zero());
                    Ok(())
                }
                _ => bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]"),
            };
            Ok(())
        })
        .collect();

    let r_bits: Vec<VecShareAdditive<Bit>> = rand_bits_k
        .into_iter()
        .map(|inner_batch| VecShareAdditive::new_vec(inner_batch))
        .collect();

    let r = VecShareAdditive::new_vec(r_batch);
    let b_bit = VecShareAdditive::new_vec(b_bit_batch);

    Ok(VecOfflineRandomSharesAdditive3pc { r_bits, r, b_bit })
}
