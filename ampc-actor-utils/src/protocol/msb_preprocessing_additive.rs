use aes_prng::AesRng;
use ampc_secret_sharing::{
    shares::{
        bit::Bit,
        primefield::PrimeElement,
        share::{AdditiveShare, AdditiveSharePrime},
    },
    IntRing2k, RingElement,
};
use eyre::{Error, Result};
use num_traits::{One, PrimInt, Zero};
use rand::{Rng, SeedableRng};
use std::ops::{Neg, SubAssign};

use crate::{
    execution::session::{Session, SessionHandles},
    network::mpc::NetworkInt,
    protocol::{
        msb_dealer_helpers::{
            bin_to_primefield16, open_additive_share, primefield16_to_bin_one_hot,
            send_binary_shares_to_dealer, send_prime16_shares_to_dealer,
            setup_shared_seed_dealer_model,
        },
        msb_offline_randomness::OfflineRandomSharesAdditive3pc,
        prf::PrfRng,
        Prf, PrfSeed,
    },
};

// MSB extraction protocol
pub async fn extract_msb_rand_additive<T: IntRing2k + NetworkInt, K: PrimInt>(
    session: &mut Session,
    x: AdditiveShare<T>,
    offline: &OfflineRandomSharesAdditive3pc<T>,
    prime_modulus: K,
) -> Result<AdditiveShare<T>, Error> {
    let mut rng = AesRng::from_random_seed();

    // [r']_k = [r]_k - [r_bit]_1 ^ 2^{k - 1} -> todo: move this to struct
    // convert RingElement<Bit> -> Bit -> Bool -> (via from) T
    let v_t: T = T::from(offline.r_bits[0].get_value().convert().convert());
    // safely left-shift by T::K - 1 == bit width - 1 using wrapping_shl
    let scaled_msb_self = RingElement(v_t.wrapping_shl((T::K - 1) as u32));
    let r_prime_self: RingElement<T> = offline.r.get_value() - scaled_msb_self;
    let r_prime_share = AdditiveShare::new(r_prime_self);

    // step 1: c' = (x + r) mod 2^{k - 1}
    // mask input 'x:AdditiveShare<T>' with pre-generated random ring element 'r:AdditiveShare<T>'
    let c_share: AdditiveShare<T> = x + offline.r;
    let c: T = open_additive_share::<T, K>(session, &[c_share]).await?[0];
    let mask: T = T::one() // (100000 - 1) -> 011111 for k = 6 example
        .wrapping_shl((T::K - 1) as u32)
        .wrapping_sub(&T::one());
    let c_prime: T = c & mask;

    // step 2: compute bitLT using c_prime and replicated bits r_bits[7], ..., r_bits[1]
    // a. sample a prf used for masking values by compute parties
    let prf_seed = PrfSeed::from([rng.gen::<u8>(); 16]);
    let bit_lt = bitlt_3pc(
        session,
        offline.r_bits.clone().into_iter().skip(1).collect(),
        c_prime,
        offline.r_bits.len() - 1,
        prf_seed,
        prime_modulus,
    )
    .await?;

    // step 3: [a']_k = 2^{k-1} [u]_1 + c' - [r']_k, [d]_k = [a]_k - [a']_k
    // a. computing scaled 2^{k - 1} * [u]_1
    // convert RingElement<Bit> -> Bit -> Bool -> (via from) T
    let v_t: T = T::from(bit_lt.get_value().convert().convert());
    // safely left-shift by T::K - 1 == bit width - 1 using wrapping_shl
    let scaled_bit_lt_self = RingElement(v_t.wrapping_shl((T::K - 1) as u32));
    let scaled_bit_lt = AdditiveShare::new(scaled_bit_lt_self); // creating additive share type for scaled bit_lt
                                                                // computing [a']_k = 2^{k-1} [u]_1 + c' - [r']_k
    let mut x_prime = scaled_bit_lt;
    x_prime.add_assign_const_role(c_prime, session.own_role());
    x_prime.sub_assign(r_prime_share);
    // computing [d]_k = [a]_k - [a']_k
    let d_share = x - x_prime;

    // step 5: computing MSB using b_bit and d_share
    // 5a. scale b_bit by 2^{k - 1}
    let two_pow_k_minus_1: T = T::one().wrapping_shl((T::K - 1) as u32);
    let mut b_msb_share = offline.b_bit;
    b_msb_share = b_msb_share * two_pow_k_minus_1;
    let e_share = d_share + b_msb_share;
    // e_share: ReplicatedShare<T>
    let e_open: T = open_additive_share::<T, K>(session, &[e_share]).await?[0];
    // MSB as bool
    let e_msb_bool: bool = ((e_open >> (T::K - 1)) & T::one()) == T::one();

    let msb = if e_msb_bool {
        let mut neg_b_bit = -offline.b_bit;
        neg_b_bit.add_assign_const_role(T::one(), session.own_role());
        neg_b_bit
    } else {
        offline.b_bit
    };
    Ok(msb)
}

// BitLT protocol
pub async fn bitlt_3pc<T: IntRing2k + NetworkInt, K: PrimInt>(
    session: &mut Session,
    shares: Vec<AdditiveShare<Bit>>,
    public_value: T,
    public_value_bits: usize,
    prf_seed: PrfSeed,
    prime_modulus: K,
) -> Result<AdditiveShare<Bit>> {
    // Scale the public value to avoid leakage to dealer if the private and public values are equal.
    // I.e., scaled = 2 * public_value + 1
    let scaled_public_value_bits: Vec<bool> = (0..public_value_bits)
        .rev()
        .map(|i| ((public_value >> i) & T::one()) == T::one())
        .chain(std::iter::once(true))
        .collect();

    let mut scaled_shares = shares.clone();
    let mut rng_rand_bits = if session.own_role().index() == 0 || session.own_role().index() == 1 {
        // Set up shared PRF between parties 1 and 2
        let shared_seed =
            setup_shared_seed_dealer_model(&mut session.network_session, prf_seed).await?;
        let mut rng = PrfRng::from_seed(Prf::expand_seed(shared_seed));
        // Scale private value to avoid leakage to dealer if the private and public values are equal
        // I.e., scaled = 2 * shares
        scaled_shares.push(AdditiveShare::new(RingElement(Bit::zero())));
        assert_eq!(scaled_public_value_bits.len(), scaled_shares.len());

        // XOR shares by the scaled public value for comparison
        scaled_shares
            .iter_mut()
            .zip(scaled_public_value_bits.iter())
            .for_each(|(share, public_val)| {
                let public_val_bit = Bit::from(*public_val);
                share.add_assign_const_role(public_val_bit, session.own_role());
            });

        // Mask the share by adding random value generated by PRF
        let rand_bits: Vec<Bit> = scaled_shares
            .iter_mut()
            .map(|share| {
                let rand_bit = rng.gen::<Bit>();
                share.add_assign_const_role(rand_bit, session.own_role());
                rand_bit
            })
            .collect();

        Some((rng, rand_bits))
    } else {
        None
    };

    // Communication round 1: Send shares to dealer to convert to prime field
    let dealer_shares = send_binary_shares_to_dealer(session, &scaled_shares).await?;
    // Communication round 2: Receive prime field shares from dealer
    let mut prime_shares_received =
        bin_to_primefield16(session, dealer_shares, prime_modulus.to_u16().unwrap()).await?;

    let (rand_shift, shifted_shares) = if let Some((rng, rand_bits)) = &mut rng_rand_bits {
        // Unmask prime shares
        rand_bits
            .iter()
            .zip(prime_shares_received.iter_mut())
            .for_each(|(bit, share)| {
                if bit.convert() {
                    *share = share.neg();
                    share.add_assign_const_role(
                        PrimeElement::<u16>::one(prime_modulus.to_u16().unwrap()),
                        session.own_role(),
                    );
                }
            });
        // Prefix sum
        let mut prefix_sum = Vec::with_capacity(prime_shares_received.len());
        let mut running_sum = AdditiveSharePrime::zero(prime_modulus.to_u16().unwrap());
        prime_shares_received.iter().for_each(|share| {
            running_sum += share;
            prefix_sum.push(running_sum);
        });

        // Pairwise sum
        let mut pairwise_sum = Vec::with_capacity(prime_shares_received.len());
        pairwise_sum.push(prefix_sum[0]);
        (1..prefix_sum.len()).for_each(|i| {
            pairwise_sum.push(prefix_sum[i - 1] + prefix_sum[i]);
        });

        // Subtract 1
        pairwise_sum.iter_mut().for_each(|share| {
            share.add_assign_const_role(
                PrimeElement::<u16>::one(prime_modulus.to_u16().unwrap()).neg(),
                session.own_role(),
            );
        });

        // Scale prime shares
        pairwise_sum.iter_mut().for_each(|share| {
            let scalar =
                PrimeElement::<u16>::rand_multiplicative(rng, prime_modulus.to_u16().unwrap());
            *share *= scalar;
        });

        // Shift shares
        let rand_shift = rng.gen_range(0..shares.len());
        let mut shifted_shares = Vec::with_capacity(shares.len());
        (0..pairwise_sum.len()).for_each(|i| {
            shifted_shares.push(pairwise_sum[(i + rand_shift) % pairwise_sum.len()]);
        });
        (Some(rand_shift), shifted_shares)
    } else {
        (None, vec![])
    };

    // Communication round 3: Send prime shares to dealer to convert to binary
    let dealer_values = send_prime16_shares_to_dealer(session, &shifted_shares).await?;
    // Communication round 4: Receive binary shares of one hot vector from dealer
    let one_hot_shifted_shares = primefield16_to_bin_one_hot(session, dealer_values).await?;

    if let Some(rand_shift) = rand_shift {
        let mut one_hot_shares = Vec::with_capacity(one_hot_shifted_shares.len());
        // Un-shift the one-hot vector shares
        (0..one_hot_shifted_shares.len()).for_each(|i| {
            if rand_shift <= i {
                one_hot_shares.push(one_hot_shifted_shares[i - rand_shift]);
            } else {
                one_hot_shares
                    .push(one_hot_shifted_shares[i + (one_hot_shifted_shares.len() - rand_shift)]);
            }
        });
        // Get the dot product against the public value
        let mut dot_product_share = AdditiveShare::<Bit>::zero();
        one_hot_shares
            .iter()
            .zip(scaled_public_value_bits.iter())
            .for_each(|(share, bit)| {
                dot_product_share += share * Bit::from(*bit);
            });

        // Add 1 because this is the opposite of what we want
        dot_product_share.add_assign_const_role(Bit::one(), session.own_role());
        Ok(dot_product_share)
    }
    // Return dummy zero share if dealer
    else {
        Ok(AdditiveShare::<Bit>::zero())
    }
}

#[cfg(test)]
mod tests {
    use super::extract_msb_rand_additive;
    use crate::execution::{local::LocalRuntime, session::SessionHandles};
    use crate::protocol::{
        msb_dealer_helpers::open_additive_share,
        msb_offline_randomness::offline_shares_for_role_additive_3pc,
        test_utils::create_array_sharing_additive,
    };
    use aes_prng::AesRng;
    use ampc_secret_sharing::shares::vecshare::VecShareAdditive;
    use eyre::Result;
    use rand::Rng;
    use tokio::task::JoinSet;

    async fn test_extract_msb_rand_u32_additive() -> Result<()> {
        let modulus = 67; // primefield modulus
        let mut rng = AesRng::from_random_seed();
        let offline_rng = AesRng::from_random_seed();
        let len = 100usize; // test size

        // Random cleartext values + expected MSB bits
        let ints: Vec<u32> = (0..len).map(|_| rng.gen::<u32>()).collect();
        let expected: Vec<u32> = ints.iter().map(|x| (*x >> 31) & 1).collect();

        // Generate shares for each party and set up sessions
        let shares = create_array_sharing_additive(&mut rng, &ints);
        let sessions = LocalRuntime::mock_sessions_with_channel().await?;
        let mut jobs = JoinSet::new();

        for (i, session) in sessions.into_iter().enumerate() {
            let session = session.clone();
            let shares_i = VecShareAdditive::new_vec(shares.of_party(i).clone());
            let mut offline_rng = offline_rng.clone();

            jobs.spawn(async move {
                let mut session = session.lock().await;

                // pick up the pre-generated randomness
                let offline =
                    offline_shares_for_role_additive_3pc(&session.own_role(), &mut offline_rng)?;

                // Run extract_msb_rand for each shared input
                let mut out = Vec::with_capacity(shares_i.len());
                for x in shares_i.shares().iter().cloned() {
                    out.push(
                        extract_msb_rand_additive::<u32, u16>(&mut session, x, &offline, modulus)
                            .await?,
                    );
                }

                // Open result bits
                open_additive_share::<u32, u16>(&mut session, &out).await
            });
        }

        let opened = jobs
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(opened.len(), 3);
        assert_eq!(opened[0], opened[1]);
        assert_eq!(opened[1], opened[2]);
        assert_eq!(opened[0], expected);

        Ok(())
    }

    #[tokio::test]
    async fn test_extract_msb_rand_additive() -> Result<()> {
        test_extract_msb_rand_u32_additive().await
    }
}
