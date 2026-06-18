use std::{
    iter::zip,
    ops::{Neg, SubAssign},
};

use aes_prng::AesRng;
use ampc_secret_sharing::{
    shares::{
        bit::Bit,
        primefield::PrimeElement,
        share::{AdditiveShare, AdditiveSharePrime},
        vecshare::{VecShareAdditive, VecShareAdditivePrime},
    },
    IntRing2k, RingElement,
};
use eyre::{Error, Result};
use itertools::Itertools;
use num_traits::{One, PrimInt, Zero};
use rand::{Rng, SeedableRng};

use crate::{
    execution::session::{Session, SessionHandles},
    network::mpc::NetworkInt,
    protocol::{
        batched_msb::batched_preprocessing::VecOfflineRandomSharesAdditive3pc,
        msb_dealer_helpers::{
            bin_to_primefield16, open_additive_share, primefield16_to_bin_one_hot,
            send_binary_shares_to_dealer, send_prime16_shares_to_dealer,
            setup_shared_seed_dealer_model,
        },
        prf::PrfRng,
        Prf, PrfSeed,
    },
};

pub async fn extract_msb_rand_additive_batch<T: IntRing2k + NetworkInt, K: PrimInt>(
    session: &mut Session,
    x: &VecShareAdditive<T>,
    offline: &VecOfflineRandomSharesAdditive3pc<T>,
    prime_modulus: K,
) -> Result<VecShareAdditive<T>, Error> {
    let mut rng = AesRng::from_random_seed();

    // [r']_k = [r]_k - [r_bit]_1 ^ 2^{k - 1} -> todo: move this to struct
    // convert RingElement<Bit> -> Bit -> Bool -> (via from) T
    let v_t = offline.r_bits[0].shares();
    let r_vals = offline.r.shares();
    let mut r_prime_vec = Vec::with_capacity(v_t.len());
    v_t.iter()
        .zip(r_vals.iter())
        .for_each(|(r_bit_share, r_share)| {
            let underlying_value = T::from(r_bit_share.get_value().convert().convert());
            let scaled_value = RingElement(underlying_value.wrapping_shl((T::K - 1) as u32));
            let r_prime: RingElement<T> = r_share.get_value() - scaled_value;
            r_prime_vec.push(AdditiveShare::new(r_prime));
        });
    let r_prime_share = VecShareAdditive::new_vec(r_prime_vec);

    // step 1: c' = (x + r) mod 2^{k - 1}
    // mask input 'x:AdditiveShare<T>' with pre-generated random ring element 'r:AdditiveShare<T>'
    let c_share: VecShareAdditive<T> = x + &offline.r;
    let c_vec: Vec<T> = open_additive_share::<T>(session, c_share.shares()).await?;
    let mask: T = T::one() // (100000 - 1) -> 011111 for k = 6 example
        .wrapping_shl((T::K - 1) as u32)
        .wrapping_sub(&T::one());
    let c_prime: Vec<T> = c_vec.into_iter().map(|c| c & mask).collect();

    // step 2: compute bitLT using c_prime and replicated bits r_bits[7], ..., r_bits[1]
    // a. sample a prf used for masking values by compute parties
    let prf_seed = PrfSeed::from([rng.gen::<u8>(); 16]);
    let bit_lt: VecShareAdditive<Bit> = bitlt_3pc_batched(
        session,
        &offline.r_bits.clone().into_iter().skip(1).collect_vec(),
        &c_prime,
        offline.r_bits.len() - 1,
        prf_seed,
        prime_modulus,
    )
    .await?;

    // step 3: [a']_k = 2^{k-1} [u]_1 + c' - [r']_k, [d]_k = [a]_k - [a']_k
    // a. computing scaled 2^{k - 1} * [u]_1
    // convert RingElement<Bit> -> Bit -> Bool -> (via from) T
    let v_t = bit_lt.shares();
    let mut scaled_bit_lt_vec = Vec::with_capacity(v_t.len());
    v_t.iter().for_each(|bit_lt_share| {
        let underlying_value = T::from(bit_lt_share.get_value().convert().convert());
        let scaled_bit_lt_self = RingElement(underlying_value.wrapping_shl((T::K - 1) as u32));
        // creating additive share type for scaled bit_lt
        // computing [a']_k = 2^{k-1} [u]_1 + c' - [r']_k
        let scaled_bit_lt = AdditiveShare::new(scaled_bit_lt_self);
        scaled_bit_lt_vec.push(scaled_bit_lt);
    });
    let mut x_prime = VecShareAdditive::new_vec(scaled_bit_lt_vec);
    x_prime.add_assign_const_role(&c_prime, session.own_role());
    x_prime.sub_assign(r_prime_share);
    // computing [d]_k = [a]_k - [a']_k
    let d_share = x.clone() - x_prime;

    // step 5: computing MSB using b_bit and d_share
    // 5a. scale b_bit by 2^{k - 1}
    let mut b_msb_share = offline.b_bit.clone();
    let two_pow_k_minus_1: Vec<T> =
        vec![T::one().wrapping_shl((T::K - 1) as u32); b_msb_share.len()];
    b_msb_share = &b_msb_share * two_pow_k_minus_1;
    let e_share = d_share + b_msb_share;
    // e_share: ReplicatedShare<T>
    let e_open_vec: Vec<T> = open_additive_share::<T>(session, e_share.shares()).await?;
    // MSB as bool
    assert_eq!(e_open_vec.len(), offline.b_bit.shares().len());
    let mut msb_vec = Vec::with_capacity(e_open_vec.len());
    for (e_open, b_bit) in zip(e_open_vec.into_iter(), offline.b_bit.shares().iter()) {
        let e_msb_bool: bool = ((e_open >> (T::K - 1)) & T::one()) == T::one();
        let msb = if e_msb_bool {
            let mut neg_b_bit = -b_bit;
            neg_b_bit.add_assign_const_role(T::one(), session.own_role());
            neg_b_bit
        } else {
            b_bit.clone()
        };
        msb_vec.push(msb);
    }

    Ok(VecShareAdditive::new_vec(msb_vec))
}

// BitLT protocol
pub async fn bitlt_3pc_batched<T: IntRing2k + NetworkInt, K: PrimInt>(
    session: &mut Session,
    shares: &Vec<VecShareAdditive<Bit>>,
    public_value_vec: &[T],
    public_value_num_bits: usize,
    prf_seed: PrfSeed,
    prime_modulus: K,
) -> Result<VecShareAdditive<Bit>> {
    let num_in_batch = shares[0].len();
    // Scale the public value to avoid leakage to dealer if the private and public values are equal.
    // I.e., scaled = 2 * public_value + 1
    let mut scaled_public_value_bits =
        vec![Vec::<bool>::with_capacity(num_in_batch); public_value_num_bits + 1];
    public_value_vec.iter().for_each(|public_value| {
        (0..public_value_num_bits)
            .rev()
            .map(|i| ((*public_value >> i) & T::one()) == T::one())
            .chain(std::iter::once(true))
            .enumerate()
            .for_each(|(idx, elem)| scaled_public_value_bits[idx].push(elem))
    });

    let mut scaled_shares = shares.clone();
    // Scale private value to avoid leakage to dealer if the private and public values are equal
    // I.e., scaled = 2 * shares
    let zero_vecshare = VecShareAdditive::new_vec(vec![
        AdditiveShare::new(RingElement(Bit::zero()));
        num_in_batch
    ]);
    scaled_shares.push(zero_vecshare);
    let num_bits = scaled_shares.len();
    let mut rng_rand_bits = if session.own_role().index() == 0 || session.own_role().index() == 1 {
        let shared_seed =
            setup_shared_seed_dealer_model(&mut session.network_session, prf_seed).await?;
        let mut rng = PrfRng::from_seed(Prf::expand_seed(shared_seed));

        assert_eq!(scaled_public_value_bits.len(), scaled_shares.len());

        // XOR shares by the scaled public value for comparison
        scaled_shares
            .iter_mut()
            .zip(scaled_public_value_bits.iter())
            .for_each(|(share, public_val)| {
                let public_val_bit: Vec<Bit> = public_val
                    .iter()
                    .map(|pub_bool| Bit::from(*pub_bool))
                    .collect();
                share.add_assign_const_role(&public_val_bit, session.own_role());
            });

        // Mask the share by adding random value generated by PRF
        let rand_bits: Vec<Vec<Bit>> = scaled_shares
            .iter_mut()
            .map(|share| {
                let rand_bit: Vec<Bit> = (0..num_in_batch).map(|_| rng.gen::<Bit>()).collect();
                share.add_assign_const_role(&rand_bit, session.own_role());
                rand_bit
            })
            .collect();

        Some((rng, rand_bits))
    } else {
        None
    };

    // Communication round 1: Send shares to dealer to convert to prime field
    let mut dealer_shares = Vec::with_capacity(scaled_shares.len());
    for share in scaled_shares {
        let dealer_share = send_binary_shares_to_dealer(session, share.shares()).await?;
        dealer_shares.push(dealer_share);
    }
    // Communication round 2: Receive prime field shares from dealer
    let mut prime_shares_received = Vec::with_capacity(dealer_shares.len());
    for dealer_share in dealer_shares {
        let prime_share =
            bin_to_primefield16(session, dealer_share, prime_modulus.to_u16().unwrap()).await?;
        prime_shares_received.push(VecShareAdditivePrime::new_vec(prime_share));
    }

    let (rand_shift, shifted_shares) = if let Some((rng, rand_bits)) = &mut rng_rand_bits {
        // Unmask prime shares
        rand_bits
            .iter()
            .zip(prime_shares_received.iter_mut())
            .for_each(|(bit_vec, share_vec)| {
                bit_vec
                    .iter()
                    .zip(share_vec.shares_mut())
                    .for_each(|(bit, share)| {
                        if bit.convert() {
                            *share = share.neg();
                            share.add_assign_const_role(
                                PrimeElement::<u16>::one(prime_modulus.to_u16().unwrap()),
                                session.own_role(),
                            );
                        }
                    })
            });
        // Prefix sum
        let mut prefix_sum = Vec::with_capacity(prime_shares_received.len());
        let mut running_sum = VecShareAdditivePrime::new_vec(vec![
            AdditiveSharePrime::zero(
                prime_modulus.to_u16().unwrap()
            );
            num_in_batch
        ]);
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
                vec![PrimeElement::<u16>::one(prime_modulus.to_u16().unwrap()).neg(); num_in_batch],
                session.own_role(),
            );
        });

        // Scale prime shares
        pairwise_sum.iter_mut().for_each(|share| {
            let scalar =
                vec![
                    PrimeElement::<u16>::rand_multiplicative(rng, prime_modulus.to_u16().unwrap());
                    num_in_batch
                ];
            *share *= scalar;
        });

        // Shift shares
        let rand_shift = rng.gen_range(0..num_bits);
        let mut shifted_shares = Vec::with_capacity(num_bits);
        (0..pairwise_sum.len()).for_each(|i| {
            shifted_shares.push(pairwise_sum[(i + rand_shift) % pairwise_sum.len()].clone());
        });
        (Some(rand_shift), shifted_shares)
    } else {
        (None, vec![VecShareAdditivePrime::new_vec(vec![]); num_bits])
    };
    // Communication round 3: Send prime shares to dealer to convert to binary
    let mut dealer_values = Vec::with_capacity(shifted_shares.len());
    for shifted_share in shifted_shares {
        let dealer_value = send_prime16_shares_to_dealer(session, shifted_share.shares()).await?;
        dealer_values.push(dealer_value);
    }

    // Communication round 4: Receive binary shares of one hot vector from dealer
    let mut one_hot_shifted_shares = vec![Vec::with_capacity(num_in_batch); dealer_values.len()];

    let len_bits = dealer_values.len();
    for i in 0..num_in_batch {
        let mut transposed_vec = Vec::with_capacity(len_bits);
        for dealer_vec in dealer_values.iter() {
            if dealer_vec.len() == num_in_batch {
                transposed_vec.push(dealer_vec[i]);
            }
        }
        let one_hot_shifted_share = primefield16_to_bin_one_hot(session, transposed_vec).await?;
        for (idx, share) in one_hot_shifted_share.into_iter().enumerate() {
            one_hot_shifted_shares[idx].push(share);
        }
    }

    if let Some(rand_shift) = rand_shift {
        let mut one_hot_shares = Vec::with_capacity(one_hot_shifted_shares.len());
        // Un-shift the one-hot vector shares
        (0..one_hot_shifted_shares.len()).for_each(|i| {
            if rand_shift <= i {
                one_hot_shares.push(one_hot_shifted_shares[i - rand_shift].clone());
            } else {
                one_hot_shares.push(
                    one_hot_shifted_shares[i + (one_hot_shifted_shares.len() - rand_shift)].clone(),
                );
            }
        });
        // Get the dot product against the public value
        let mut dot_product_share =
            VecShareAdditive::new_vec(vec![AdditiveShare::<Bit>::zero(); num_in_batch]);
        one_hot_shares
            .iter()
            .zip(scaled_public_value_bits.iter())
            .for_each(|(share, bool_share)| {
                let bits: Vec<Bit> = bool_share.iter().map(|share| Bit::from(*share)).collect();
                let vecshare = VecShareAdditive::new_vec(share.to_vec());
                dot_product_share += vecshare * bits;
            });

        // Add 1 because this is the opposite of what we want
        dot_product_share
            .add_assign_const_role(&vec![Bit::one(); num_in_batch], session.own_role());
        Ok(dot_product_share)
    }
    // Return dummy zero share if dealer
    else {
        Ok(VecShareAdditive::new_vec(vec![
            AdditiveShare::<Bit>::zero();
            num_in_batch
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::extract_msb_rand_additive_batch;
    use crate::execution::local::LocalRuntime;
    use crate::protocol::batched_msb::batched_preprocessing::offline_shares_vec_for_role_additive_3pc;
    use crate::protocol::{
        msb_dealer_helpers::open_additive_share, test_utils::create_array_sharing_additive,
    };
    use aes_prng::AesRng;
    use ampc_secret_sharing::shares::vecshare::VecShareAdditive;
    use eyre::Result;
    use rand::Rng;
    use tokio::task::JoinSet;

    async fn test_extract_msb_batch_rand_u32_additive() -> Result<()> {
        let modulus = 37; // primefield modulus
        let mut rng = AesRng::from_random_seed();
        let mut offline_rng = AesRng::from_random_seed();
        let len = 100usize; // test size

        // Random cleartext values + expected MSB bits
        let ints: Vec<u16> = (0..len).map(|_| rng.gen::<u16>()).collect();
        let expected: Vec<u16> = ints.iter().map(|x| (*x >> 15) & 1).collect();

        // Generate shares for each party and set up sessions
        let shares = create_array_sharing_additive(&mut rng, &ints);
        let sessions = LocalRuntime::mock_sessions_with_channel().await?;
        let mut jobs = JoinSet::new();
        // pick up the pre-generated randomness
        let offline_all_parties = offline_shares_vec_for_role_additive_3pc(len, &mut offline_rng)?;
        for (i, session) in sessions.into_iter().enumerate() {
            let session = session.clone();
            let shares_i = VecShareAdditive::new_vec(shares.of_party(i).clone());
            let offline = offline_all_parties[i].clone();

            jobs.spawn(async move {
                let mut session = session.lock().await;

                // Run extract_msb_rand for each shared input
                let out =
                    extract_msb_rand_additive_batch(&mut session, &shares_i, &offline, modulus)
                        .await?;

                // Open result bits
                open_additive_share::<u16>(&mut session, &out.shares()).await
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
    async fn test_extract_msb_batch_rand_additive() -> Result<()> {
        test_extract_msb_batch_rand_u32_additive().await
    }
}
