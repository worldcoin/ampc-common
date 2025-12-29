// Protocol operations for MPC
// This file contains only the non-iris-specific protocol operations

use crate::execution::session::SessionHandles;
use crate::network::value::NetworkInt;
use crate::protocol::binary::{bit_inject, extract_msb_batch, lift, open_bin};
use crate::{
    execution::session::{NetworkSession, Session},
    network::value::NetworkValue,
    protocol::prf::{Prf, PrfSeed},
};
use ampc_secret_sharing::shares::bit::Bit;
use ampc_secret_sharing::shares::share::DistanceShare;
use ampc_secret_sharing::shares::{ring_impl::RingElement, share::Share, IntRing2k, VecShare};
use eyre::{bail, eyre, Result};
use itertools::{izip, Itertools};
use tracing::instrument;

pub(crate) const B_BITS: u64 = 16;
pub(crate) const B: u64 = 1 << B_BITS;

/// Setup the PRF seeds in the replicated protocol.
/// Each party sends to the next party a random seed.
/// At the end, each party will hold two seeds which are the basis of the
/// replicated protocols.
#[instrument(
    level = "trace",
    target = "mpc::network",
    fields(party = ?session.own_role),
    skip_all
)]
pub async fn setup_replicated_prf(session: &mut NetworkSession, my_seed: PrfSeed) -> Result<Prf> {
    // send my_seed to the next party
    session.send_next(NetworkValue::PrfKey(my_seed)).await?;
    // deserializing received seed.
    let other_seed = match session.receive_prev().await {
        Ok(NetworkValue::PrfKey(seed)) => seed,
        _ => bail!("Could not deserialize PrfKey"),
    };
    // creating the two PRFs
    Ok(Prf::new(my_seed, other_seed))
}

/// Setup a shared seed across all three parties.
/// Each party sends their seed to both neighbors and receives from both.
/// The final shared seed is the XOR of all three seeds.
pub async fn setup_shared_seed(session: &mut NetworkSession, my_seed: PrfSeed) -> Result<PrfSeed> {
    let my_msg = NetworkValue::PrfKey(my_seed);

    let decode = |msg| match msg {
        Ok(NetworkValue::PrfKey(seed)) => Ok(seed),
        _ => Err(eyre!("Could not deserialize PrfKey")),
    };

    // Round 1: Send to the next party and receive from the previous party.
    session.send_next(my_msg.clone()).await?;
    let prev_seed = decode(session.receive_prev().await)?;

    // Round 2: Send/receive in the opposite direction.
    session.send_prev(my_msg).await?;
    let next_seed = decode(session.receive_next().await)?;

    let shared_seed = std::array::from_fn(|i| my_seed[i] ^ prev_seed[i] ^ next_seed[i]);
    Ok(shared_seed)
}

/// Convert Galois Ring elements to replicated secret shares (Rep3)
/// This takes a vector of ring elements and converts them to replicated shares
pub async fn galois_ring_to_rep3(
    session: &mut Session,
    items: Vec<RingElement<u16>>,
) -> Result<Vec<Share<u16>>> {
    let network = &mut session.network_session;

    // make sure we mask the input with a zero sharing
    let masked_items: Vec<_> = items
        .iter()
        .map(|x| session.prf.gen_zero_share() + x)
        .collect();

    // sending to the next party
    network
        .send_next(NetworkValue::VecRing16(masked_items.clone()))
        .await?;

    // receiving from previous party
    let shares_b = {
        match network.receive_prev().await {
            Ok(NetworkValue::VecRing16(message)) => Ok(message),
            _ => Err(eyre!("Error in receiving in galois_ring_to_rep3 operation")),
        }
    }?;
    let res: Vec<Share<u16>> = masked_items
        .into_iter()
        .zip(shares_b)
        .map(|(a, b)| Share::new(a, b))
        .collect();
    Ok(res)
}

/// Compares the given distances to zero and reveal the bit "less than zero".
pub async fn lt_zero_and_open_u16(
    session: &mut Session,
    distances: &[Share<u16>],
) -> Result<Vec<bool>> {
    let bits = extract_msb_batch(session, distances).await?;
    open_bin(session, &bits)
        .await
        .map(|v| v.into_iter().map(|x| x.convert()).collect())
}

/// Subtracts a public ring element from a secret-shared ring element in-place.
pub fn sub_pub<T: IntRing2k + NetworkInt>(
    session: &mut Session,
    share: &mut Share<T>,
    rhs: RingElement<T>,
) {
    match session.own_role().index() {
        0 => share.a -= rhs,
        1 => share.b -= rhs,
        2 => {}
        _ => unreachable!(),
    }
}

/// For each of the given distance shares returns `true` if it's a share of a non-negative value.
pub async fn gte_zero_and_open_u16(
    session: &mut Session,
    distances: &[Share<u16>],
) -> Result<Vec<bool>> {
    let bits = extract_msb_batch(session, distances).await?;

    // MSB is `1` is `distance < 0`.
    // MSB is `0` if `distance >= 0`.
    // Open the binary shares and negate the value to return `true` if and only if `distance >=0`.
    open_bin(session, &bits)
        .await
        .map(|v| v.into_iter().map(|x| !x.convert()).collect())
}

/// Open ring shares to reveal the secret value
/// This is a helper function for opening shares
#[allow(dead_code)]
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn open_ring<T: IntRing2k + crate::network::value::NetworkInt>(
    session: &mut Session,
    shares: &[Share<T>],
) -> Result<Vec<T>> {
    let network = &mut session.network_session;
    let message = if shares.len() == 1 {
        T::new_network_element(shares[0].b)
    } else {
        let shares_b = shares.iter().map(|x| x.b).collect::<Vec<_>>();
        T::new_network_vec(shares_b)
    };

    network.send_next(message).await?;

    // receiving from previous party
    let c = network
        .receive_prev()
        .await
        .and_then(|v| T::into_vec(v))
        .map_err(|e| eyre!("Error in receiving in open operation: {}", e))?;

    // ADD shares with the received shares
    shares
        .iter()
        .zip(c.iter())
        .map(|(s, c)| Ok((s.a + s.b + c).convert()))
        .collect::<Result<Vec<_>>>()
}

#[instrument(level = "trace", target = "searcher::network", skip_all)]
/// Same as [open_ring], but for non-replicated shares. Due to the share being non-replicated,
/// each party needs to send its entire share to the next and previous party.
pub async fn open_ring_element_broadcast<T: IntRing2k + NetworkInt>(
    session: &mut Session,
    shares: &[RingElement<T>],
) -> Result<Vec<T>> {
    let network = &mut session.network_session;
    let message = if shares.len() == 1 {
        T::new_network_element(shares[0])
    } else {
        T::new_network_vec(shares.to_vec())
    };

    network.send_next(message.clone()).await?;
    network.send_prev(message).await?;

    // receiving from previous party
    let b = network
        .receive_prev()
        .await
        .and_then(|v| T::into_vec(v))
        .map_err(|e| eyre!("Error in receiving in open operation: {}", e))?;
    let c = network
        .receive_next()
        .await
        .and_then(|v| T::into_vec(v))
        .map_err(|e| eyre!("Error in receiving in open operation: {}", e))?;

    // ADD shares with the received shares
    izip!(shares.iter(), b.iter(), c.iter())
        .map(|(a, b, c)| Ok((*a + *b + *c).convert()))
        .collect::<Result<Vec<_>>>()
}

/// Conditionally selects the distance shares based on control bits.
/// If the control bit is 1, it selects the first distance share (d1),
/// otherwise it selects the second distance share (d2).
/// Assumes that the input shares are originally 16-bit and lifted to u32.
#[instrument(level = "trace", target = "searcher::network", skip_all)]
async fn conditionally_select_distance(
    session: &mut Session,
    distances: &[(DistanceShare<u32>, DistanceShare<u32>)],
    control_bits: &[Share<u32>],
) -> Result<Vec<DistanceShare<u32>>> {
    if distances.len() != control_bits.len() {
        bail!("Number of distances must match number of control bits");
    }

    // Conditional multiplexing:
    // If control bit is 1, select d1, else select d2.
    // res = c * d1 + (1 - c) * d2 = d2 + c * (d1 - d2);
    // We need to do it for both code_dot and mask_dot.

    // we start with the mult of c and d1-d2
    // Compute differences component-wise to avoid intermediate Share allocation
    let res_a: Vec<RingElement<u32>> = distances
        .iter()
        .zip(control_bits.iter())
        .flat_map(|((d1, d2), c)| {
            let code_a = d1.code_dot.a - d2.code_dot.a;
            let code_b = d1.code_dot.b - d2.code_dot.b;
            let mask_a = d1.mask_dot.a - d2.mask_dot.a;
            let mask_b = d1.mask_dot.b - d2.mask_dot.b;
            let code_mul_a =
                session.prf.gen_zero_share() + c.a * code_a + c.b * code_a + c.a * code_b;
            let mask_mul_a =
                session.prf.gen_zero_share() + c.a * mask_a + c.b * mask_a + c.a * mask_b;
            [code_mul_a, mask_mul_a]
        })
        .collect();

    let network = &mut session.network_session;

    let message = if res_a.len() == 1 {
        NetworkValue::RingElement32(res_a[0])
    } else {
        NetworkValue::VecRing32(res_a.clone())
    };
    network.send_next(message).await?;

    let res_b = match network.receive_prev().await {
        Ok(NetworkValue::RingElement32(element)) => vec![element],
        Ok(NetworkValue::VecRing32(elements)) => elements,
        _ => bail!("Could not deserialize RingElement32"),
    };

    // finally compute the result by adding the d2 shares
    Ok(izip!(res_a.into_iter(), res_b.into_iter())
        // combine a and b part into shares
        .map(|(a, b)| Share::new(a, b))
        // combine the code and mask parts into DistanceShare
        .tuples()
        .map(|(code, mask)| DistanceShare {
            code_dot: code,
            mask_dot: mask,
        })
        // add the d2 shares
        .zip(distances.iter())
        .map(|(res, (_, d2))| DistanceShare {
            code_dot: res.code_dot + &d2.code_dot,
            mask_dot: res.mask_dot + &d2.mask_dot,
        })
        .collect())
}

/// Computes the `A` term of the threshold comparison based on the formula `A = ((1. - 2. * t) * B)`.
pub fn translate_threshold_a(t: f64) -> u32 {
    assert!(
        (0. ..=1.).contains(&t),
        "Threshold must be in the range [0, 1]"
    );
    ((1. - 2. * t) * (B as f64)) as u32
}

/// Computes the cross product of distances shares represented as a fraction (code_dist, mask_dist).
/// The cross product is computed as (d2.code_dist * d1.mask_dist - d1.code_dist * d2.mask_dist) and the result is shared.
///
/// Assumes that the input shares are originally 16-bit and lifted to u32.
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub(crate) async fn cross_mul(
    session: &mut Session,
    distances: &[(DistanceShare<u32>, DistanceShare<u32>)],
) -> Result<Vec<Share<u32>>> {
    let res_a: Vec<RingElement<u32>> = distances
        .iter()
        .map(|(d1, d2)| {
            session.prf.gen_zero_share() + &d2.code_dot * &d1.mask_dot - &d1.code_dot * &d2.mask_dot
        })
        .collect();

    let network = &mut session.network_session;

    let message = if res_a.len() == 1 {
        NetworkValue::RingElement32(res_a[0])
    } else {
        NetworkValue::VecRing32(res_a.clone())
    };
    network.send_next(message).await?;

    let res_b = match network.receive_prev().await {
        Ok(NetworkValue::RingElement32(element)) => vec![element],
        Ok(NetworkValue::VecRing32(elements)) => elements,
        _ => bail!("Could not deserialize RingElement32"),
    };
    Ok(izip!(res_a.into_iter(), res_b.into_iter())
        .map(|(a, b)| Share::new(a, b))
        .collect())
}

/// For every pair of distance shares (d1, d2), this computes the secret-shared bit d2 < d1 .
///
/// The less-than operator is implemented in 2 steps:
///
/// 1. d2.code_dot * d1.mask_dot - d1.code_dot * d2.mask_dot is computed, which is a numerator of the fraction difference d2.code_dot / d2.mask_dot - d1.code_dot / d1.mask_dot.
/// 2. The most significant bit of the result is extracted.
///
/// Input values are assumed to be 16-bit shares that have been lifted to 32 bits.
pub(crate) async fn oblivious_cross_compare(
    session: &mut Session,
    distances: &[(DistanceShare<u32>, DistanceShare<u32>)],
) -> Result<Vec<Share<Bit>>> {
    // d2.code_dot * d1.mask_dot - d1.code_dot * d2.mask_dot
    let diff = cross_mul(session, distances).await?;
    // Compute the MSB of the above
    extract_msb_batch(session, &diff).await
}

/// For every pair of distance shares (d1, d2), this computes the secret-shared bit d2 < d1 and lift it to u32 shares.
///
/// The less-than operator is implemented in 2 steps:
///
/// 1. d2.code_dot * d1.mask_dot - d1.code_dot * d2.mask_dot is computed, which is a numerator of the fraction difference d2.code_dot / d2.mask_dot - d1.code_dot / d1.mask_dot.
/// 2. The most significant bit of the result is extracted.
///
/// Input values are assumed to be 16-bit shares that have been lifted to 32 bits.
pub(crate) async fn oblivious_cross_compare_lifted(
    session: &mut Session,
    distances: &[(DistanceShare<u32>, DistanceShare<u32>)],
) -> Result<Vec<Share<u32>>> {
    // compute the secret-shared bits d1 < d2
    let bits = oblivious_cross_compare(session, distances).await?;
    // inject bits to T shares
    Ok(bit_inject(session, VecShare { shares: bits })
        .await?
        .inner())
}

/// For every pair of distance shares (d1, d2), this computes the bit d2 < d1 uses it to return the lower of the two distances.
///
/// Input values are assumed to be 16-bit shares that have been lifted to 32 bits.
pub async fn min_of_pair_batch(
    session: &mut Session,
    distances: &[(DistanceShare<u32>, DistanceShare<u32>)],
) -> Result<Vec<DistanceShare<u32>>> {
    // compute the secret-shared bits d1 < d2
    let bits = oblivious_cross_compare_lifted(session, distances).await?;

    conditionally_select_distance(session, distances, bits.as_slice()).await
}

/// Lifts a share of a vector (VecShare) of 16-bit values to a share of a vector
/// (VecShare) of 32-bit values.
pub async fn batch_signed_lift(
    session: &mut Session,
    mut pre_lift: VecShare<u16>,
) -> Result<VecShare<u32>> {
    // Compute (v + 2^{15}) % 2^{16}, to make values positive.
    for v in pre_lift.iter_mut() {
        v.add_assign_const_role(1_u16 << 15, session.own_role());
    }
    let mut lifted_values = lift(session, pre_lift).await?;
    // Now we got shares of d1' over 2^32 such that d1' = (d1'_1 + d1'_2 + d1'_3) %
    // 2^{16} = d1 Next we subtract the 2^15 term we've added previously to
    // get signed shares over 2^{32}
    for v in lifted_values.iter_mut() {
        v.add_assign_const_role(((1_u64 << 32) - (1_u64 << 15)) as u32, session.own_role());
    }
    Ok(lifted_values)
}

/// Wrapper over batch_signed_lift that lifts a vector (Vec) of 16-bit shares to
/// a vector (Vec) of 32-bit shares.
pub async fn batch_signed_lift_vec(
    session: &mut Session,
    pre_lift: Vec<Share<u16>>,
) -> Result<Vec<Share<u32>>> {
    let pre_lift = VecShare::new_vec(pre_lift);
    Ok(batch_signed_lift(session, pre_lift).await?.inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::local::{generate_local_identities, LocalRuntime};
    use rand::RngCore;
    use tokio::task::JoinSet;

    #[tokio::test]
    async fn test_setup_replicated_prf() {
        let num_parties = 3;
        let identities = generate_local_identities();
        let mut seeds = Vec::new();
        for i in 0..num_parties {
            let mut seed = [0_u8; 16];
            seed[0] = i;
            seeds.push(seed);
        }
        let mut runtime = LocalRuntime::new(identities.clone(), seeds.clone())
            .await
            .unwrap();

        // Check whether parties have sent/received the correct seeds.
        // P0: [seed_0, seed_2]
        // P1: [seed_1, seed_0]
        // P2: [seed_2, seed_1]

        // Alice
        let prf0 = &mut runtime.sessions[0].prf;
        assert_eq!(
            prf0.get_my_prf().next_u64(),
            Prf::new(seeds[0], seeds[2]).get_my_prf().next_u64()
        );
        assert_eq!(
            prf0.get_prev_prf().next_u64(),
            Prf::new(seeds[0], seeds[2]).get_prev_prf().next_u64()
        );

        // Bob
        let prf1 = &mut runtime.sessions[1].prf;
        assert_eq!(
            prf1.get_my_prf().next_u64(),
            Prf::new(seeds[1], seeds[0]).get_my_prf().next_u64()
        );
        assert_eq!(
            prf1.get_prev_prf().next_u64(),
            Prf::new(seeds[1], seeds[0]).get_prev_prf().next_u64()
        );

        // Charlie
        let prf2 = &mut runtime.sessions[2].prf;
        assert_eq!(
            prf2.get_my_prf().next_u64(),
            Prf::new(seeds[2], seeds[1]).get_my_prf().next_u64()
        );
        assert_eq!(
            prf2.get_prev_prf().next_u64(),
            Prf::new(seeds[2], seeds[1]).get_prev_prf().next_u64()
        );
    }

    #[tokio::test]
    async fn test_setup_shared_seed() {
        let mut seeds = Vec::new();
        for i in 0..3 {
            let mut seed = [0_u8; 16];
            seed[0] = i;
            seeds.push(seed);
        }

        let sessions = LocalRuntime::mock_sessions_with_channel().await.unwrap();
        let mut jobs = JoinSet::new();

        for (i, session) in sessions.iter().enumerate() {
            let session = session.clone();
            let my_seed = seeds[i];
            jobs.spawn(async move {
                let mut session = session.lock().await;
                setup_shared_seed(&mut session.network_session, my_seed)
                    .await
                    .unwrap()
            });
        }

        let mut results = Vec::new();
        while let Some(res) = jobs.join_next().await {
            results.push(res.unwrap());
        }

        // All parties should compute the same shared seed
        assert_eq!(results[0], results[1]);
        assert_eq!(results[1], results[2]);

        // The shared seed should be XOR of all three seeds
        let expected: PrfSeed = std::array::from_fn(|i| seeds[0][i] ^ seeds[1][i] ^ seeds[2][i]);
        assert_eq!(results[0], expected);
    }
}
