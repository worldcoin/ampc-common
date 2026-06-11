use aes_prng::AesRng;
use ampc_secret_sharing::{
    shares::{
        bit::Bit,
        primefield::PrimeElement,
        share::{AdditiveShare, AdditiveSharePrime},
    },
    IntRing2k, ReplicatedShare, RingElement, Role,
};
use eyre::{bail, eyre, Error, Result};
use num_traits::{One, PrimInt, Zero};
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, Standard};
use std::ops::{Neg, SubAssign};
use tracing::instrument;

use crate::protocol::{
    ops::open_ring,
    test_utils::{create_single_sharing_additive, create_single_sharing_replicated},
};
use crate::{
    execution::session::{NetworkSession, Session, SessionHandles},
    network::mpc::{NetworkInt, NetworkValue},
    protocol::{prf::PrfRng, Prf, PrfSeed},
};
// Precomputed offline randomness for extract_msb_rand: a share<T> element 'r', its per-bit boolean
// shares (bit7..bit0), and a shared random bit 'b_bit' embedded as a Share<T> share.

// TODO: generalize r_bits to work for any type T; make it Vec<ReplicatedShare<Bit>> maybe?
pub struct OfflineRandomSharesReplicated<T: IntRing2k> {
    r: ReplicatedShare<T>,
    r_bits: Vec<ReplicatedShare<Bit>>, // r_7, ..., r_0
    b_bit: ReplicatedShare<T>,
}

// sampling an instance of pre-generated randomness used in the protocol for T = u8
/// Returns the per-party view of precomputed randomness for extract_msb_rand.
/// Each party gets its replicated ABY3 share (a,b) of the same global values.
pub fn offline_shares_for_role_replicated<T: IntRing2k>(
    role: &impl Role,
    rng: &mut impl Rng,
) -> Result<OfflineRandomSharesReplicated<T>, Error>
where
    Standard: Distribution<T>,
{
    let rand_bits: Vec<bool> = (0..T::K).map(|_| rng.gen_bool(0.5)).collect();
    let total_value = rand_bits
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
    let total_value_shares = create_single_sharing_replicated(rng, total_value);
    let b_bit = rng.gen_bool(0.5);
    let b_bit_shares = create_single_sharing_replicated(rng, T::from(b_bit));
    let rand_bit_shares = rand_bits.iter().map(move |overall_bit| {
        let first_share = rng.gen_bool(0.5);
        let second_share = rng.gen_bool(0.5);
        let first_second_xor = !(first_share == second_share);
        let third_share = !(*overall_bit == first_second_xor);
        (first_share, second_share, third_share)
    });

    match role.index() {
        // Party 0 holds (a0,a2) for every shared value.
        0 => Ok(OfflineRandomSharesReplicated {
            r: total_value_shares.0,
            r_bits: rand_bit_shares
                .map(|(b0, _, b2)| {
                    ReplicatedShare::new(RingElement(Bit::new(b0)), RingElement(Bit::new(b2)))
                })
                .collect(),
            b_bit: b_bit_shares.0,
        }),
        // Party 1 holds (a1,a0).
        1 => Ok(OfflineRandomSharesReplicated {
            r: total_value_shares.1,
            r_bits: rand_bit_shares
                .map(|(b0, b1, _)| {
                    ReplicatedShare::new(RingElement(Bit::new(b1)), RingElement(Bit::new(b0)))
                })
                .collect(),
            b_bit: b_bit_shares.1,
        }),
        // Party 2 holds (a2,a1).
        2 => Ok(OfflineRandomSharesReplicated {
            r: total_value_shares.2,
            r_bits: rand_bit_shares
                .map(|(_, b1, b2)| {
                    ReplicatedShare::new(RingElement(Bit::new(b2)), RingElement(Bit::new(b1)))
                })
                .collect(),
            b_bit: b_bit_shares.2,
        }),
        _ => bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]"),
    }
}

#[derive(Debug)]
pub struct OfflineRandomSharesAdditive2<T: IntRing2k> {
    r: AdditiveShare<T>,
    r_bits: Vec<AdditiveShare<Bit>>, // r_7, ..., r_0
    b_bit: AdditiveShare<T>,
}

// sampling an instance of pre-generated randomness used in the protocol for T = u8
/// Returns the per-party view of precomputed randomness for extract_msb_rand.
/// Each party gets its additive ABY3 share (a,b) of the same global values.
pub fn offline_shares_for_role_additive2<T: IntRing2k>(
    role: &impl Role,
    rng: &mut impl Rng,
) -> Result<OfflineRandomSharesAdditive2<T>, Error>
where
    Standard: Distribution<T>,
{
    let rand_bits: Vec<bool> = (0..T::K).map(|_| rng.gen_bool(0.5)).collect();
    let total_value = rand_bits
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
    let total_value_shares = create_single_sharing_additive(rng, total_value);
    let b_bit = rng.gen_bool(0.5);
    let b_bit_shares = create_single_sharing_additive(rng, T::from(b_bit));
    let rand_bit_shares = rand_bits.iter().map(move |overall_bit| {
        let first_share = rng.gen_bool(0.5);
        let second_share = !(*overall_bit == first_share);
        (first_share, second_share)
    });

    match role.index() {
        0 => Ok(OfflineRandomSharesAdditive2 {
            r: total_value_shares.0,
            r_bits: rand_bit_shares
                .map(|(b0, _)| AdditiveShare::new(RingElement(Bit::new(b0))))
                .collect(),
            b_bit: b_bit_shares.0,
        }),
        1 => Ok(OfflineRandomSharesAdditive2 {
            r: total_value_shares.1,
            r_bits: rand_bit_shares
                .map(|(_, b1)| AdditiveShare::new(RingElement(Bit::new(b1))))
                .collect(),
            b_bit: b_bit_shares.1,
        }),
        2 => Ok(OfflineRandomSharesAdditive2 {
            r: AdditiveShare::zero(),
            r_bits: rand_bit_shares.map(|_| AdditiveShare::zero()).collect(),
            b_bit: AdditiveShare::zero(),
        }),
        _ => bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]"),
    }
}

/// Setup a shared seed across first two parties in dealer model.
/// Each party (of 1, 2) sends their seed to the other and receives from each other.
/// The final shared seed is the XOR of both seeds.
pub async fn setup_shared_seed_dealer_model(
    session: &mut NetworkSession,
    my_seed: PrfSeed,
) -> Result<PrfSeed> {
    let my_msg = NetworkValue::PrfKey(my_seed);

    let decode = |msg| match msg {
        Ok(NetworkValue::PrfKey(seed)) => Ok(seed),
        _ => Err(eyre!("Could not deserialize PrfKey")),
    };

    let shared_seed = match session.own_role.index() {
        0 => {
            session.send_next(my_msg.clone()).await?;
            let other_seed = decode(session.receive_next().await)?;
            std::array::from_fn(|i| my_seed[i] ^ other_seed[i])
        }
        1 => {
            session.send_prev(my_msg).await?;
            let other_seed = decode(session.receive_prev().await)?;
            std::array::from_fn(|i| my_seed[i] ^ other_seed[i])
        }
        _ => {
            bail!("Cannot deal with roles that have index outside of the set [0, 1]")
        }
    };

    Ok(shared_seed)
}

// TODO: implement the struct OfflineRandomShares with a new function that instantiates a new instance for type T
// can we just instantiate a new instance within the MSB protocol??

pub async fn extract_msb_rand<T: IntRing2k + NetworkInt, K: PrimInt>(
    session: &mut Session,
    x: ReplicatedShare<T>,
    offline: &OfflineRandomSharesReplicated<T>,
    prime_modulus: K,
) -> Result<ReplicatedShare<T>, Error> {
    let mut rng = AesRng::from_random_seed();
    // TODO
    // let prime_modulus_lower_bound = 2 * T::K + 1;
    // get_next_prime(prime_modulus_lower_bound)

    // step 1: [r']_k = [r]_k - [r_bit]_1 ^ 2^{k - 1}
    // convert RingElement<Bit> -> Bit -> Bool -> (via from) T
    let v_t: T = T::from(offline.r_bits[0].get_a().convert().convert());
    // safely left-shift by T::K - 1 == bit width - 1 using wrapping_shl
    let scaled_msb_self = RingElement(v_t.wrapping_shl((T::K - 1) as u32));

    let v_t: T = T::from(offline.r_bits[0].get_b().convert().convert());
    let scaled_msb_prev = RingElement(v_t.wrapping_shl((T::K - 1) as u32));

    let r_prime_self: RingElement<T> = offline.r.get_a() - scaled_msb_self;
    let r_prime_prev: RingElement<T> = offline.r.get_b() - scaled_msb_prev;
    let r_prime_share = ReplicatedShare::new(r_prime_self, r_prime_prev);

    // step 2: c' = (x + r) mod 2^{k - 1}

    // mask input 'x:AdditiveShare<T>' with pre-generated random ring element 'r:AdditiveShare<T>'
    let c_share: ReplicatedShare<T> = x + offline.r;
    let c: T = open_ring(session, std::slice::from_ref(&c_share)).await?[0];
    let mask: T = T::one()
        .wrapping_shl((T::K - 1) as u32)
        .wrapping_sub(&T::one());
    let c_prime: T = c & mask;

    // step 3: compute bitLT using c_prime and replicated bits r_bits[7], ..., r_bits[1]
    // convert the replicated bits to additive shares of bits
    let mut r_bits_additive = Vec::with_capacity(offline.r_bits.len() - 1);
    for rep_bit_share in offline.r_bits.iter().skip(1) {
        r_bits_additive.push(rep_to_add2(session, *rep_bit_share).await?);
    }

    // sample a prf
    let prf_seed = PrfSeed::from([rng.gen::<u8>(); 16]);
    // TODO: compute bitlt using additive shares and prf seed -> output is additive share of bitLT
    let bit_lt_share_add2 = bitlt(
        session,
        r_bits_additive.clone(),
        c_prime,
        r_bits_additive.len(),
        prf_seed,
        prime_modulus,
    )
    .await?;

    // TODO convert the additive share of bitlt back to replicated share of bitlt (for nowww))
    let bit_lt_share = add2_to_rep_binary(session, bit_lt_share_add2).await?;
    // step 4: [a']_k = 2^{k-1} [u]_1 + c' - [r']_k, [d]_k = [a]_k - [a']_k

    // 4a. computing scaled 2^{k - 1} * [u]_1
    // convert RingElement<Bit> -> Bit -> Bool -> (via from) T
    let v_t: T = T::from(bit_lt_share.get_a().convert().convert());
    // safely left-shift by T::K - 1 == bit width - 1 using wrapping_shl
    let scaled_bit_lt_self = RingElement(v_t.wrapping_shl((T::K - 1) as u32));

    let v_t: T = T::from(bit_lt_share.get_b().convert().convert());
    // safely left-shift by T::K - 1 == bit width - 1 using wrapping_shl
    let scaled_bit_lt_prev = RingElement(v_t.wrapping_shl((T::K - 1) as u32));

    let scaled_bit_lt = ReplicatedShare::new(scaled_bit_lt_self, scaled_bit_lt_prev);
    let mut x_prime = scaled_bit_lt;
    x_prime.add_assign_const_role(c_prime, session.own_role());
    x_prime.sub_assign(r_prime_share);

    let d_share = x - x_prime;

    // step 5: computing MSB using b_bit and d_share
    // 5a. scale b_bit by 2^{k - 1}
    let two_pow_k_minus_1: T = T::one().wrapping_shl((T::K - 1) as u32);
    let mut b_msb_share = offline.b_bit;
    b_msb_share = b_msb_share * two_pow_k_minus_1;
    let e_share = d_share + b_msb_share;
    // e_share: ReplicatedShare<T>
    let e_open: T = open_ring(session, &[e_share]).await?[0];
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

pub async fn extract_msb_rand_additive<T: IntRing2k + NetworkInt, K: PrimInt>(
    session: &mut Session,
    x: AdditiveShare<T>,
    offline: &OfflineRandomSharesAdditive2<T>,
    prime_modulus: K,
) -> Result<AdditiveShare<T>, Error> {
    let mut rng = AesRng::from_random_seed();
    // TODO
    // let prime_modulus_lower_bound = 2 * T::K + 1;
    // get_next_prime(prime_modulus_lower_bound)

    // step 1: [r']_k = [r]_k - [r_bit]_1 ^ 2^{k - 1}
    // convert RingElement<Bit> -> Bit -> Bool -> (via from) T
    let v_t: T = T::from(offline.r_bits[0].get_value().convert().convert());
    // safely left-shift by T::K - 1 == bit width - 1 using wrapping_shl
    let scaled_msb_self = RingElement(v_t.wrapping_shl((T::K - 1) as u32));

    let r_prime_self: RingElement<T> = offline.r.get_value() - scaled_msb_self;
    let r_prime_share = AdditiveShare::new(r_prime_self);

    // step 2: c' = (x + r) mod 2^{k - 1}

    // mask input 'x:AdditiveShare<T>' with pre-generated random ring element 'r:AdditiveShare<T>'
    let c_share: AdditiveShare<T> = x + offline.r;
    let c: T = open_additive_share::<T, K>(session, &[c_share]).await?[0];
    let mask: T = T::one()
        .wrapping_shl((T::K - 1) as u32)
        .wrapping_sub(&T::one());
    let c_prime: T = c & mask;

    // step 3: compute bitLT using c_prime and replicated bits r_bits[7], ..., r_bits[1]
    // sample a prf
    let prf_seed = PrfSeed::from([rng.gen::<u8>(); 16]);
    // TODO: compute bitlt using additive shares and prf seed -> output is additive share of bitLT
    let bit_lt_share_add2 = bitlt(
        session,
        offline.r_bits.clone().into_iter().skip(1).collect(),
        c_prime,
        offline.r_bits.len() - 1,
        prf_seed,
        prime_modulus,
    )
    .await?;

    // step 4: [a']_k = 2^{k-1} [u]_1 + c' - [r']_k, [d]_k = [a]_k - [a']_k

    // 4a. computing scaled 2^{k - 1} * [u]_1
    // convert RingElement<Bit> -> Bit -> Bool -> (via from) T
    let v_t: T = T::from(bit_lt_share_add2.get_value().convert().convert());
    // safely left-shift by T::K - 1 == bit width - 1 using wrapping_shl
    let scaled_bit_lt_self = RingElement(v_t.wrapping_shl((T::K - 1) as u32));

    let scaled_bit_lt = AdditiveShare::new(scaled_bit_lt_self);
    let mut x_prime = scaled_bit_lt;
    x_prime.add_assign_const_role(c_prime, session.own_role());
    x_prime.sub_assign(r_prime_share);

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

pub async fn rep_to_add2<T: IntRing2k>(
    session: &mut Session,
    rep_share: ReplicatedShare<T>,
) -> Result<AdditiveShare<T>, Error> {
    let (a, b) = rep_share.get_ab();

    let mut share = AdditiveShare::zero();

    match session.own_role().index() {
        0 => {
            share.value += a + b;
        }
        1 => {
            share.value += a;
        }
        2 => {}
        _ => {
            bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]")
        }
    }

    Ok(share)
}

pub async fn add2_to_rep_binary(
    session: &mut Session,
    share: AdditiveShare<Bit>,
) -> Result<ReplicatedShare<Bit>, Error> {
    let network = &mut session.network_session;

    let randomized_input = match network.own_role.index() {
        0 | 1 | 2 => {
            // Local additive zero-share of bits
            let zero_share = session.prf.gen_binary_zero_share::<Bit>();
            share.value ^ zero_share
        }
        _ => {
            bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]")
        }
    };

    network
        .send_next(NetworkValue::RingElementBit(randomized_input))
        .await?;

    let randomized_input_prev = match network.receive_prev().await {
        Ok(NetworkValue::RingElementBit(value)) => value,
        _ => bail!("Could not deserialize RingElementBit"),
    };

    Ok(ReplicatedShare::new(
        randomized_input,
        randomized_input_prev,
    ))
}

pub async fn bin_to_primefield16(
    session: &mut Session,
    values: Vec<RingElement<Bit>>,
    modulus: u16,
) -> Result<Vec<AdditiveSharePrime<PrimeElement<u16>>>, Error> {
    let network = &mut session.network_session;
    let shares = match network.own_role.index() {
        0 => {
            let share_from_previous = network
                .receive_prev()
                .await
                .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;
            if values.len() == 1 {
                match share_from_previous {
                    NetworkValue::PrimeElement16(message) => {
                        Ok(vec![AdditiveSharePrime::new(message)])
                    }
                    _ => Err(eyre!("Wrong value type is received in open_bin operation")),
                }
            } else {
                match NetworkValue::vec_from_network(share_from_previous) {
                    Ok(v) => {
                        if matches!(v[0], NetworkValue::PrimeElement16(_)) {
                            Ok(v.into_iter()
                                .map(|x| match x {
                                    NetworkValue::PrimeElement16(message) => {
                                        AdditiveSharePrime::new(message)
                                    }
                                    _ => unreachable!(),
                                })
                                .collect())
                        } else {
                            Err(eyre!("Wrong value type is received in open_bin operation"))
                        }
                    }
                    Err(e) => Err(eyre!("Error in receiving in open_bin operation: {}", e)),
                }
            }?
        }
        1 => {
            let share_from_next = network
                .receive_next()
                .await
                .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;
            if values.len() == 1 {
                match share_from_next {
                    NetworkValue::PrimeElement16(message) => {
                        Ok(vec![AdditiveSharePrime::new(message)])
                    }
                    _ => Err(eyre!("Wrong value type is received in open_bin operation")),
                }
            } else {
                match NetworkValue::vec_from_network(share_from_next) {
                    Ok(v) => {
                        if matches!(v[0], NetworkValue::PrimeElement16(_)) {
                            Ok(v.into_iter()
                                .map(|x| match x {
                                    NetworkValue::PrimeElement16(message) => {
                                        AdditiveSharePrime::new(message)
                                    }
                                    _ => unreachable!(),
                                })
                                .collect())
                        } else {
                            Err(eyre!("Wrong value type is received in open_bin operation"))
                        }
                    }
                    Err(e) => Err(eyre!("Error in receiving in open_bin operation: {}", e)),
                }
            }?
        }
        2 => {
            let mut rng = AesRng::from_entropy();
            let (shares_0, shares_1): (
                Vec<AdditiveSharePrime<PrimeElement<u16>>>,
                Vec<AdditiveSharePrime<PrimeElement<u16>>>,
            ) = values
                .iter()
                .map(|value| {
                    let bit_as_mod19 =
                        PrimeElement::<u16>::new(u8::from(value.convert()) as u16, modulus);
                    let rand_mod19_share = PrimeElement::<u16>::rand(&mut rng, modulus);
                    let other_share = bit_as_mod19 - rand_mod19_share;
                    (
                        AdditiveSharePrime::new(other_share),
                        AdditiveSharePrime::new(rand_mod19_share),
                    )
                })
                .unzip();
            let message_next = if shares_0.len() == 1 {
                NetworkValue::PrimeElement16(shares_0[0].value)
            } else {
                let values = shares_0
                    .iter()
                    .map(|x| NetworkValue::PrimeElement16(x.value))
                    .collect::<Vec<_>>();
                NetworkValue::vec_to_network(values)
            };
            network.send_next(message_next).await?;
            let message_prev = if shares_1.len() == 1 {
                NetworkValue::PrimeElement16(shares_1[0].value)
            } else {
                let values = shares_1
                    .iter()
                    .map(|x| NetworkValue::PrimeElement16(x.value))
                    .collect::<Vec<_>>();
                NetworkValue::vec_to_network(values)
            };
            network.send_prev(message_prev).await?;
            vec![]
        }
        _ => bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]"),
    };
    Ok(shares)
}

pub async fn primefield16_to_bin_one_hot(
    session: &mut Session,
    values: Vec<PrimeElement<u16>>,
) -> Result<Vec<AdditiveShare<Bit>>, Error> {
    let network = &mut session.network_session;
    let shares = match network.own_role.index() {
        0 => {
            let share_from_previous = network
                .receive_prev()
                .await
                .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;
            if values.len() == 1 {
                match share_from_previous {
                    NetworkValue::RingElementBit(message) => Ok(vec![AdditiveShare::new(message)]),
                    _ => Err(eyre!("Wrong value type is received in open_bin operation")),
                }
            } else {
                match NetworkValue::vec_from_network(share_from_previous) {
                    Ok(v) => {
                        if matches!(v[0], NetworkValue::RingElementBit(_)) {
                            Ok(v.into_iter()
                                .map(|x| match x {
                                    NetworkValue::RingElementBit(message) => {
                                        AdditiveShare::new(message)
                                    }
                                    _ => unreachable!(),
                                })
                                .collect())
                        } else {
                            Err(eyre!("Wrong value type is received in open_bin operation"))
                        }
                    }
                    Err(e) => Err(eyre!("Error in receiving in open_bin operation: {}", e)),
                }
            }?
        }
        1 => {
            let share_from_next = network
                .receive_next()
                .await
                .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;
            if values.len() == 1 {
                match share_from_next {
                    NetworkValue::RingElementBit(message) => Ok(vec![AdditiveShare::new(message)]),
                    _ => Err(eyre!("Wrong value type is received in open_bin operation")),
                }
            } else {
                match NetworkValue::vec_from_network(share_from_next) {
                    Ok(v) => {
                        if matches!(v[0], NetworkValue::RingElementBit(_)) {
                            Ok(v.into_iter()
                                .map(|x| match x {
                                    NetworkValue::RingElementBit(message) => {
                                        AdditiveShare::new(message)
                                    }
                                    _ => unreachable!(),
                                })
                                .collect())
                        } else {
                            Err(eyre!("Wrong value type is received in open_bin operation"))
                        }
                    }
                    Err(e) => Err(eyre!("Error in receiving in open_bin operation: {}", e)),
                }
            }?
        }
        2 => {
            let mut rng = AesRng::from_entropy();
            let (shares_0, shares_1): (Vec<AdditiveShare<Bit>>, Vec<AdditiveShare<Bit>>) = values
                .iter()
                .map(|value| {
                    let rand_bit_as_bool = rng.gen_range(0..=1) != 0;
                    let rand_bit = RingElement(Bit::from(rand_bit_as_bool));
                    let other_share = if value.is_zero() {
                        RingElement(Bit::one()) - rand_bit
                    } else {
                        RingElement(Bit::zero()) - rand_bit
                    };
                    (
                        AdditiveShare::new(other_share),
                        AdditiveShare::new(rand_bit),
                    )
                })
                .unzip();
            let message_next = if shares_0.len() == 1 {
                NetworkValue::RingElementBit(shares_0[0].value)
            } else {
                let values = shares_0
                    .iter()
                    .map(|x| NetworkValue::RingElementBit(x.value))
                    .collect::<Vec<_>>();
                NetworkValue::vec_to_network(values)
            };
            network.send_next(message_next).await?;
            let message_prev = if shares_1.len() == 1 {
                NetworkValue::RingElementBit(shares_1[0].value)
            } else {
                let values = shares_1
                    .iter()
                    .map(|x| NetworkValue::RingElementBit(x.value))
                    .collect::<Vec<_>>();
                NetworkValue::vec_to_network(values)
            };
            network.send_prev(message_prev).await?;
            vec![]
        }
        _ => bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]"),
    };
    Ok(shares)
}

#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn open_additive_share<T: IntRing2k + NetworkInt, K: PrimInt>(
    session: &mut Session,
    share: &[AdditiveShare<T>],
) -> Result<Vec<T>, Error> {
    let network = &mut session.network_session;
    let message = if share.len() == 1 {
        T::new_network_element(share[0].value)
    } else {
        T::new_network_vec(
            share
                .iter()
                .map(|additive_share| additive_share.value)
                .collect(),
        )
    };

    network.send_next(message.clone()).await?;
    network.send_prev(message).await?;

    // Receiving share from previous party
    let share_from_previous = 
        // receiving from previous party
        network
            .receive_prev()
            .await
            .and_then(|v| T::into_vec(v))
            .map_err(|e| eyre!("Error in receiving in open operation: {}", e))?;

    // Receiving share from next party
    let share_from_next = {
        // receiving from previous party
        network
            .receive_next()
            .await
            .and_then(|v| T::into_vec(v))
            .map_err(|e| eyre!("Error in receiving in open operation: {}", e))?
    };

    share.iter().zip(share_from_previous.iter()).zip(share_from_next.iter()).map(
        |((share_a, share_b), share_c)| {
            Ok((share_a.value + share_b + share_c).convert())
        }
    ).collect::<Result<Vec<_>>>()
}

#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn open_additive_share_u8(
    session: &mut Session,
    share: &AdditiveShare<u8>,
) -> Result<RingElement<u8>, Error> {
    let network = &mut session.network_session;
    let message = NetworkValue::RingElement8(share.value);

    network.send_next(message.clone()).await?;
    network.send_prev(message).await?;

    // Receiving share from previous party
    let share_from_previous = {
        let prev_share = network
            .receive_prev()
            .await
            .map_err(|e| eyre!("Error in receiving in open_u8 operation: {}", e))?;

        match prev_share {
            NetworkValue::RingElement8(message) => Ok(message),
            _ => Err(eyre!("Wrong value type is received in open_u8 operation")),
        }
    }?;

    // Receiving share from next party
    let share_from_next = {
        let next_share = network
            .receive_next()
            .await
            .map_err(|e| eyre!("Error in receiving in open_u8 operation: {}", e))?;

        match next_share {
            NetworkValue::RingElement8(message) => Ok(message),
            _ => Err(eyre!("Wrong value type is received in open_u8 operation")),
        }
    }?;

    Ok(share.value + share_from_previous + share_from_next)
}

#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn open_additive_share_bit(
    session: &mut Session,
    share: &AdditiveShare<Bit>,
) -> Result<RingElement<Bit>, Error> {
    let network = &mut session.network_session;
    let message = NetworkValue::RingElementBit(share.value);

    network.send_next(message.clone()).await?;
    network.send_prev(message).await?;

    // Receiving share from previous party
    let share_from_previous = {
        let prev_share = network
            .receive_prev()
            .await
            .map_err(|e| eyre!("Error in receiving in open_Bit operation: {}", e))?;

        match prev_share {
            NetworkValue::RingElementBit(message) => Ok(message),
            _ => Err(eyre!("Wrong value type is received in open_Bit operation")),
        }
    }?;

    // Receiving share from next party
    let share_from_next = {
        let next_share = network
            .receive_next()
            .await
            .map_err(|e| eyre!("Error in receiving in open_Bit operation: {}", e))?;

        match next_share {
            NetworkValue::RingElementBit(message) => Ok(message),
            _ => Err(eyre!("Wrong value type is received in open_Bit operation")),
        }
    }?;

    Ok(share.value + share_from_previous + share_from_next)
}

#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn send_binary_shares_to_dealer(
    session: &mut Session,
    shares: &Vec<AdditiveShare<Bit>>,
) -> Result<Vec<RingElement<Bit>>, Error> {
    let network = &mut session.network_session;
    let message = if shares.len() == 1 {
        NetworkValue::RingElementBit(shares[0].value)
    } else {
        // TODO: could be optimized by packing bits
        let bits = shares
            .iter()
            .map(|x| NetworkValue::RingElementBit(x.value))
            .collect::<Vec<_>>();
        NetworkValue::vec_to_network(bits)
    };

    let values_received = match network.own_role.index() {
        0 => {
            network.send_prev(message.clone()).await?;
            vec![]
        }
        1 => {
            network.send_next(message.clone()).await?;
            vec![]
        }
        2 => {
            let share_from_previous = network
                .receive_prev()
                .await
                .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;
            let values_from_previous = if shares.len() == 1 {
                match share_from_previous {
                    NetworkValue::RingElementBit(message) => Ok(vec![message]),
                    _ => Err(eyre!("Wrong value type is received in open_bin operation")),
                }
            } else {
                match NetworkValue::vec_from_network(share_from_previous) {
                    Ok(v) => {
                        if matches!(v[0], NetworkValue::RingElementBit(_)) {
                            Ok(v.into_iter()
                                .map(|x| match x {
                                    NetworkValue::RingElementBit(message) => message,
                                    _ => unreachable!(),
                                })
                                .collect())
                        } else {
                            Err(eyre!("Wrong value type is received in open_bin operation"))
                        }
                    }
                    Err(e) => Err(eyre!("Error in receiving in open_bin operation: {}", e)),
                }
            }?;
            let share_from_next = network
                .receive_next()
                .await
                .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;
            let values_from_next = if shares.len() == 1 {
                match share_from_next {
                    NetworkValue::RingElementBit(message) => Ok(vec![message]),
                    _ => Err(eyre!("Wrong value type is received in open_bin operation")),
                }
            } else {
                match NetworkValue::vec_from_network(share_from_next) {
                    Ok(v) => {
                        if matches!(v[0], NetworkValue::RingElementBit(_)) {
                            Ok(v.into_iter()
                                .map(|x| match x {
                                    NetworkValue::RingElementBit(message) => message,
                                    _ => unreachable!(),
                                })
                                .collect())
                        } else {
                            Err(eyre!("Wrong value type is received in open_bin operation"))
                        }
                    }
                    Err(e) => Err(eyre!("Error in receiving in open_bin operation: {}", e)),
                }
            }?;

            values_from_previous
                .iter()
                .zip(values_from_next.iter())
                .map(|(prev, next)| *prev ^ next)
                .collect()
        }
        _ => {
            bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]")
        }
    };
    Ok(values_received)
}

#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn send_prime16_shares_to_dealer(
    session: &mut Session,
    shares: &Vec<AdditiveSharePrime<PrimeElement<u16>>>,
) -> Result<Vec<PrimeElement<u16>>, Error> {
    let network = &mut session.network_session;
    let message = if shares.len() == 1 {
        NetworkValue::PrimeElement16(shares[0].value)
    } else {
        // TODO: could be optimized by packing bits
        let bits = shares
            .iter()
            .map(|x| NetworkValue::PrimeElement16(x.value))
            .collect::<Vec<_>>();
        NetworkValue::vec_to_network(bits)
    };
    let values_received = match network.own_role.index() {
        0 => {
            network.send_prev(message.clone()).await?;
            vec![]
        }
        1 => {
            network.send_next(message.clone()).await?;
            vec![]
        }
        2 => {
            let share_from_previous = network
                .receive_prev()
                .await
                .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;
            let values_from_previous = if shares.len() == 1 {
                match share_from_previous {
                    NetworkValue::PrimeElement16(message) => Ok(vec![message]),
                    _ => Err(eyre!("Wrong value type is received in open_bin operation")),
                }
            } else {
                match NetworkValue::vec_from_network(share_from_previous) {
                    Ok(v) => {
                        if matches!(v[0], NetworkValue::PrimeElement16(_)) {
                            Ok(v.into_iter()
                                .map(|x| match x {
                                    NetworkValue::PrimeElement16(message) => message,
                                    _ => unreachable!(),
                                })
                                .collect())
                        } else {
                            Err(eyre!("Wrong value type is received in open_bin operation"))
                        }
                    }
                    Err(e) => Err(eyre!("Error in receiving in open_bin operation: {}", e)),
                }
            }?;
            let share_from_next = network
                .receive_next()
                .await
                .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;
            let values_from_next = if shares.len() == 1 {
                match share_from_next {
                    NetworkValue::PrimeElement16(message) => Ok(vec![message]),
                    _ => Err(eyre!("Wrong value type is received in open_bin operation")),
                }
            } else {
                match NetworkValue::vec_from_network(share_from_next) {
                    Ok(v) => {
                        if matches!(v[0], NetworkValue::PrimeElement16(_)) {
                            Ok(v.into_iter()
                                .map(|x| match x {
                                    NetworkValue::PrimeElement16(message) => message,
                                    _ => unreachable!(),
                                })
                                .collect())
                        } else {
                            Err(eyre!("Wrong value type is received in open_bin operation"))
                        }
                    }
                    Err(e) => Err(eyre!("Error in receiving in open_bin operation: {}", e)),
                }
            }?;
            values_from_previous
                .iter()
                .zip(values_from_next.iter())
                .map(|(prev, next)| *prev + *next)
                .collect()
        }
        _ => {
            bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]")
        }
    };
    Ok(values_received)
}

pub async fn bitlt<T: IntRing2k + NetworkInt, K: PrimInt>(
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
    use crate::protocol::msb_preprocessing::{
        add2_to_rep_binary, bitlt, extract_msb_rand_additive, offline_shares_for_role_additive2,
        open_additive_share, open_additive_share_bit, open_additive_share_u8, rep_to_add2,
    };
    use crate::protocol::ops::open_ring;
    use crate::protocol::test_utils::{
        create_array_sharing_additive, create_single_sharing_additive,
        create_single_sharing_replicated,
    };
    use crate::protocol::PrfSeed;
    use crate::{
        execution::{local::LocalRuntime, session::SessionHandles},
        protocol::{
            binary::open_bin,
            msb_preprocessing::{extract_msb_rand, offline_shares_for_role_replicated},
            test_utils::create_array_sharing_replicated,
        },
    };
    use aes_prng::AesRng;
    use ampc_secret_sharing::shares::share::AdditiveShare;
    use ampc_secret_sharing::shares::vecshare::VecShareAdditive;
    use ampc_secret_sharing::shares::{bit::Bit, VecShareReplicated};
    use ampc_secret_sharing::RingElement;
    use eyre::{bail, Error, Result};
    use rand::{Rng, SeedableRng};
    use rand_distr::{Distribution, Standard};
    use tokio::task::JoinSet;

    async fn test_extract_msb_rand_u8() -> Result<()> {
        let modulus = 19;
        let mut rng = AesRng::from_random_seed();
        let offline_rng = AesRng::from_random_seed();
        let len = 4usize;

        // Random cleartext values + expected MSB bits
        let ints: Vec<u8> = (0..len).map(|_| rng.gen::<u8>()).collect();
        //let ints: Vec<u8> = vec![241u8, 128u8, 34u8, 255u8, 11u8];

        let expected: Vec<u8> = ints.iter().map(|x| (*x >> 7) & 1).collect();

        println!(
            "Cleartext values: {:?} Expected Values: {:?}",
            ints, expected
        );
        // Secret-share inputs across 3 parties
        let shares = create_array_sharing_replicated(&mut rng, &ints);

        let sessions = LocalRuntime::mock_sessions_with_channel().await?;
        let mut jobs = JoinSet::new();

        for (i, session) in sessions.into_iter().enumerate() {
            let session = session.clone();
            let shares_i = VecShareReplicated::new_vec(shares.of_party(i).clone());
            let mut offline_rng = offline_rng.clone();

            jobs.spawn(async move {
                let mut session = session.lock().await;

                // pick up the pre-generated randomness
                let offline =
                    offline_shares_for_role_replicated(&session.own_role(), &mut offline_rng)?;

                // Run extract_msb_rand for each shared input
                let mut out = Vec::with_capacity(shares_i.len());
                for x in shares_i.shares().iter().cloned() {
                    out.push(extract_msb_rand::<u8, u8>(&mut session, x, &offline, modulus).await?);
                }

                // Open result bits
                open_ring(&mut session, &out).await
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
    async fn test_extract_msb_rand() -> Result<()> {
        test_extract_msb_rand_u8().await
    }

    async fn test_extract_msb_rand_u32() -> Result<()> {
        let modulus = 67;
        let mut rng = AesRng::from_random_seed();
        let offline_rng = AesRng::from_random_seed();
        let len = 100usize;

        // Random cleartext values + expected MSB bits
        let ints: Vec<u32> = (0..len).map(|_| rng.gen::<u32>()).collect();
        //let ints: Vec<u8> = vec![241u8, 128u8, 34u8, 255u8, 11u8];

        let expected: Vec<u32> = ints.iter().map(|x| (*x >> 31) & 1).collect();

        println!(
            "Cleartext values: {:?} Expected Values: {:?}",
            ints, expected
        );
        // Secret-share inputs across 3 parties
        let shares = create_array_sharing_replicated(&mut rng, &ints);

        let sessions = LocalRuntime::mock_sessions_with_channel().await?;
        let mut jobs = JoinSet::new();

        for (i, session) in sessions.into_iter().enumerate() {
            let session = session.clone();
            let shares_i = VecShareReplicated::new_vec(shares.of_party(i).clone());
            let mut offline_rng = offline_rng.clone();

            jobs.spawn(async move {
                let mut session = session.lock().await;

                // pick up the pre-generated randomness
                let offline =
                    offline_shares_for_role_replicated(&session.own_role(), &mut offline_rng)?;

                // Run extract_msb_rand for each shared input
                let mut out = Vec::with_capacity(shares_i.len());
                for x in shares_i.shares().iter().cloned() {
                    out.push(
                        extract_msb_rand::<u32, u16>(&mut session, x, &offline, modulus).await?,
                    );
                }

                // Open result bits
                open_ring(&mut session, &out).await
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
    async fn test_extract_msb_rand_2() -> Result<()> {
        test_extract_msb_rand_u32().await
    }

    async fn test_extract_msb_rand_u32_additive() -> Result<()> {
        let modulus = 67;
        let mut rng = AesRng::from_random_seed();
        let offline_rng = AesRng::from_random_seed();
        let len = 100usize;

        // Random cleartext values + expected MSB bits
        let ints: Vec<u32> = (0..len).map(|_| rng.gen::<u32>()).collect();
        //let ints: Vec<u8> = vec![241u8, 128u8, 34u8, 255u8, 11u8];

        let expected: Vec<u32> = ints.iter().map(|x| (*x >> 31) & 1).collect();

        println!(
            "Cleartext values: {:?} Expected Values: {:?}",
            ints, expected
        );
        // Secret-share inputs across 3 parties
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
                    offline_shares_for_role_additive2(&session.own_role(), &mut offline_rng)?;

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

    async fn test_rep_to_add2_u8() -> Result<()>
    where
        Standard: Distribution<u8>,
    {
        let mut rng = AesRng::from_entropy();
        let sessions = LocalRuntime::mock_sessions_with_channel().await?;
        let mut jobs = JoinSet::new();
        let value = rng.gen::<u8>();
        let expected = RingElement(value);
        let shares = create_single_sharing_replicated::<AesRng, u8>(&mut rng, value);

        for session in sessions.into_iter() {
            let session = session.clone();

            jobs.spawn(async move {
                let mut session = session.lock().await;
                let shares_i = match session.own_role().index() {
                    0 => shares.0,
                    1 => shares.1,
                    2 => shares.2,
                    _ => {
                        bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]")
                    }
                };

                let out = rep_to_add2::<u8>(&mut session, shares_i).await?;

                // Open result bits
                open_additive_share_u8(&mut session, &out).await
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
    async fn test_rep_to_add2() -> Result<()> {
        test_rep_to_add2_u8().await
    }

    async fn test_add2_to_rep_binary() -> Result<()> {
        let mut rng = AesRng::from_entropy();
        let sessions = LocalRuntime::mock_sessions_with_channel().await?;
        let mut jobs = JoinSet::new();

        let value = Bit::new(rng.gen::<bool>());
        let expected = value;

        // Two-party additive sharing of the bit; dealer/party 2 gets zero.
        let shares = create_single_sharing_additive::<AesRng, Bit>(&mut rng, value);

        for session in sessions.into_iter() {
            let session = session.clone();

            jobs.spawn(async move {
                let mut session = session.lock().await;

                let share_i = match session.own_role().index() {
                    0 => shares.0,
                    1 => shares.1,
                    2 => AdditiveShare::zero(),
                    _ => {
                        bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]")
                    }
                };

                let out = add2_to_rep_binary(&mut session, share_i).await?;

                // Open replicated bit share
                let opened = open_bin(&mut session, std::slice::from_ref(&out)).await?;
                Ok::<Bit, Error>(opened[0])
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
    async fn test_add2_to_rep() -> Result<()> {
        test_add2_to_rep_binary().await
    }

    async fn test_bitlt_u8() -> Result<()>
    where
        Standard: Distribution<u8>,
    {
        let modulus = 19;
        let mut rng = AesRng::from_entropy();
        let sessions = LocalRuntime::mock_sessions_with_channel().await?;
        let mut jobs = JoinSet::new();
        let private_values: Vec<Bit> = (0..8).map(|_| rng.gen::<Bit>()).collect();
        let private_value = private_values
            .iter()
            .rev()
            .enumerate()
            .fold(0_u8, |acc, (index, elem)| {
                acc + (elem.convert() as u8) * (2_u8.pow(index as u32))
            });
        let shares: (Vec<AdditiveShare<Bit>>, Vec<AdditiveShare<Bit>>) = private_values
            .iter()
            .map(|value| create_single_sharing_additive::<AesRng, Bit>(&mut rng, *value))
            .unzip();

        let public_value = rng.gen::<u8>();
        let expected = public_value < private_value;

        for session in sessions.into_iter() {
            let session = session.clone();
            let shares = shares.clone();
            jobs.spawn(async move {
                let mut rng = AesRng::from_entropy();
                let mut session = session.lock().await;
                let shares_i = match session.own_role().index() {
                    0 => shares.0,
                    1 => shares.1,
                    2 => vec![AdditiveShare::<Bit>::zero(); 8],
                    _ => {
                        bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]")
                    }
                };
                let prf_seed = PrfSeed::from([rng.gen::<u8>(); 16]);

                let out = bitlt(
                    &mut session,
                    shares_i.clone(),
                    public_value,
                    8,
                    prf_seed,
                    modulus,
                )
                .await?;

                // Open result bits
                open_additive_share_bit(&mut session, &out).await
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
        assert_eq!(opened[0].convert().convert(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn test_bitlt() -> Result<()> {
        test_bitlt_u8().await
    }

    async fn test_bitlt_u32() -> Result<()>
    where
        Standard: Distribution<u8>,
    {
        let modulus = 67;
        let mut rng = AesRng::from_entropy();
        let sessions = LocalRuntime::mock_sessions_with_channel().await?;
        let mut jobs = JoinSet::new();
        let private_values: Vec<Bit> = (0..32).map(|_| rng.gen::<Bit>()).collect();
        let private_value = private_values
            .iter()
            .rev()
            .enumerate()
            .fold(0_u32, |acc, (index, elem)| {
                acc + (elem.convert() as u32) * (2_u32.pow(index as u32))
            });
        let shares: (Vec<AdditiveShare<Bit>>, Vec<AdditiveShare<Bit>>) = private_values
            .iter()
            .map(|value| create_single_sharing_additive::<AesRng, Bit>(&mut rng, *value))
            .unzip();

        let public_value = rng.gen::<u32>();
        let expected = public_value < private_value;

        for session in sessions.into_iter() {
            let session = session.clone();
            let shares = shares.clone();
            jobs.spawn(async move {
                let mut rng = AesRng::from_entropy();
                let mut session = session.lock().await;
                let shares_i = match session.own_role().index() {
                    0 => shares.0,
                    1 => shares.1,
                    2 => vec![AdditiveShare::<Bit>::zero(); 8],
                    _ => {
                        bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]")
                    }
                };
                let prf_seed = PrfSeed::from([rng.gen::<u8>(); 16]);

                let out = bitlt(
                    &mut session,
                    shares_i.clone(),
                    public_value,
                    32,
                    prf_seed,
                    modulus,
                )
                .await?;

                // Open result bits
                open_additive_share_bit(&mut session, &out).await
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
        assert_eq!(opened[0].convert().convert(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn test_bitlt_2() -> Result<()> {
        test_bitlt_u32().await
    }
}
