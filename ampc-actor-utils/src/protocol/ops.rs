// Protocol operations for MPC
// This file contains only the non-iris-specific protocol operations

use crate::execution::session::{NetworkSession, Session, SessionHandles};
use crate::network::value::{NetworkInt, NetworkValue};
use crate::protocol::binary::{bit_inject, extract_msb_batch, lift, lift_to_ring48, open_bin};
use crate::protocol::prf::{Prf, PrfSeed};
use ampc_secret_sharing::shares::bit::Bit;
use ampc_secret_sharing::shares::share::DistanceShare;
use ampc_secret_sharing::shares::RingRandFillable;
use ampc_secret_sharing::shares::{
    ring_impl::RingElement, share::Share, IntRing2k, Ring48, VecShare,
};
use eyre::{bail, eyre, Result};
use itertools::{izip, Itertools};
use rand_distr::{Distribution, Standard};
use tracing::instrument;

pub type DistancePair<T> = (DistanceShare<T>, DistanceShare<T>);
pub type IdDistance<T> = (Share<T>, DistanceShare<T>);

pub const B_BITS: u64 = 16;
pub const B: u32 = 1 << B_BITS;

// ---------------------------------------------------------------------------
// Batched replicated multiplication
// ---------------------------------------------------------------------------

/// Executes one round of the ABY3 replicated multiplication protocol.
///
/// The caller provides a closure that computes the local product expression
/// for each of `n` output shares. The function handles PRF zero-share
/// generation, network communication (send_next/receive_prev), and share
/// reconstruction.
///
/// The closure receives the batch index `0..n` and returns a `RingElement<T>`
/// representing the product terms for that slot. Multiple products can be
/// additively combined by simply returning their sum/difference.
pub async fn reshare_products<T, F>(
    session: &mut Session,
    n: usize,
    mut expr: F,
) -> Result<Vec<Share<T>>>
where
    T: NetworkInt + RingRandFillable,
    F: FnMut(usize) -> RingElement<T>,
{
    let (prf_my, prf_prev) = session.prf.gen_rands_batch::<T>(n);

    let round_a: Vec<RingElement<T>> = prf_my
        .0
        .into_iter()
        .zip(prf_prev.0)
        .enumerate()
        .map(|(i, (my_r, prev_r))| (my_r - prev_r) + expr(i))
        .collect();

    let network = &mut session.network_session;
    network
        .send_next(T::new_network_vec(round_a.clone()))
        .await?;
    let round_b: Vec<RingElement<T>> = T::into_vec(network.receive_prev().await?)?;

    Ok(round_a
        .into_iter()
        .zip(round_b)
        .map(|(a, b)| Share::new(a, b))
        .collect())
}

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
    let (prf_my_values, prf_prev_values) = session.prf.gen_rands_batch(items.len());

    // make sure we mask the input with a zero sharing
    let masked_items: Vec<_> = izip!(
        items.into_iter(),
        prf_my_values.0.into_iter(),
        prf_prev_values.0.into_iter()
    )
    .map(|(x, a, b)| {
        let zero_share = a - b; // equivalent to gen_zero_share()
        zero_share + x
    })
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
#[inline]
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

/// Batched conditional MUX via the reshare protocol.
///
/// For each of `n_elements` elements, computes `c * (left - right) + right`
/// across `fields_per_element` independent share-pairs in a single
/// communication round. The caller provides a closure that, given
/// `(element_index, field_index)`, returns `(left, right)`.
#[inline]
async fn reshare_mux<T, F>(
    session: &mut Session,
    n_elements: usize,
    fields_per_element: usize,
    control_bits: &[Share<T>],
    mut get_field: F,
) -> Result<Vec<Share<T>>>
where
    T: NetworkInt + RingRandFillable,
    F: FnMut(usize, usize) -> (Share<T>, Share<T>),
{
    if fields_per_element == 0 {
        bail!("reshare_mux: fields_per_element must be greater than zero");
    }
    if control_bits.len() < n_elements {
        bail!(
            "reshare_mux: control_bits length ({}) is smaller than n_elements ({})",
            control_bits.len(),
            n_elements
        );
    }
    reshare_products(session, n_elements * fields_per_element, |i| {
        let j = i / fields_per_element;
        let f = i % fields_per_element;
        let c = &control_bits[j];
        let (left, right) = get_field(j, f);
        let diff = left - right;
        c.a * diff.a + c.b * diff.a + c.a * diff.b + right.a
    })
    .await
}

/// Conditionally selects the distance shares based on control bits.
/// If the control bit is 1, it selects the first distance share (d1),
/// otherwise it selects the second distance share (d2).
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn conditionally_select_distance<T>(
    session: &mut Session,
    distances: &[DistancePair<T>],
    control_bits: &[Share<T>],
) -> Result<Vec<DistanceShare<T>>>
where
    T: NetworkInt + RingRandFillable,
{
    if distances.len() != control_bits.len() {
        bail!("Number of distances must match number of control bits");
    }

    // res = c * (d1 - d2) + d2, computed for both code_dot and mask_dot.
    let products = reshare_mux(session, distances.len(), 2, control_bits, |j, f| {
        let (d1, d2) = &distances[j];
        if f == 0 {
            (d1.code_dot, d2.code_dot)
        } else {
            (d1.mask_dot, d2.mask_dot)
        }
    })
    .await?;

    Ok(products
        .into_iter()
        .tuples()
        .map(|(code, mask)| DistanceShare::new(code, mask))
        .collect())
}

#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn conditionally_select_distances_with_plain_ids<T>(
    session: &mut Session,
    left_distances: Vec<(u32, DistanceShare<T>)>,
    right_distances: Vec<(u32, DistanceShare<T>)>,
    control_bits: Vec<Share<T>>,
) -> Result<Vec<IdDistance<T>>>
where
    T: NetworkInt + RingRandFillable + From<u32>,
{
    let n = left_distances.len();
    if n == 0 {
        bail!("Distances must not be empty");
    }
    if n != control_bits.len() {
        bail!("Number of distances must match number of control bits");
    }
    if n != right_distances.len() {
        bail!("Left and right distances must have the same length");
    }

    let distances = reshare_mux(session, n, 2, &control_bits, |j, f| {
        let left = &left_distances[j].1;
        let right = &right_distances[j].1;
        if f == 0 {
            (left.code_dot, right.code_dot)
        } else {
            (left.mask_dot, right.mask_dot)
        }
    })
    .await?
    .into_iter()
    .tuples()
    .map(|(code, mask)| DistanceShare::new(code, mask));

    // Select ids first: c * (left_id - right_id) + right_id
    let ids = izip!(left_distances, right_distances, control_bits).map(|(left, right, c)| {
        let (left_id, _) = left;
        let (right_id, _) = right;
        let diff = left_id.wrapping_sub(right_id);
        let mut res = c * RingElement(T::from(diff));
        res.add_assign_const_role(T::from(right_id), session.own_role());
        res
    });

    Ok(izip!(ids, distances).collect())
}

#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn conditionally_select_distances_with_shared_ids<T>(
    session: &mut Session,
    left_distances: Vec<IdDistance<T>>,
    right_distances: Vec<IdDistance<T>>,
    control_bits: Vec<Share<T>>,
) -> Result<Vec<IdDistance<T>>>
where
    T: NetworkInt + RingRandFillable,
{
    if left_distances.is_empty() {
        bail!("Distances must not be empty");
    }
    let n = left_distances.len();
    if n != control_bits.len() {
        bail!("Number of distances must match number of control bits");
    }
    if n != right_distances.len() {
        bail!("Left and right distances must have the same length");
    }

    let flat_res = reshare_mux(session, n, 3, &control_bits, |j, f| {
        let (left_id, left_dist) = &left_distances[j];
        let (right_id, right_dist) = &right_distances[j];
        match f {
            0 => (*left_id, *right_id),
            1 => (left_dist.code_dot, right_dist.code_dot),
            2 => (left_dist.mask_dot, right_dist.mask_dot),
            _ => unreachable!(),
        }
    })
    .await?;

    Ok(flat_res
        .into_iter()
        .tuples()
        .map(|(id, code, mask)| (id, DistanceShare::new(code, mask)))
        .collect())
}

/// Conditionally swaps the distance shares based on control bits.
/// Given the ith pair of indices (i1, i2), the function does the following.
/// If the control bit is 0, it swaps tuples (id, distance share) with index i1 and i2,
/// otherwise it does nothing.
/// The vector ids are in plaintext and propagated in secret shared form.
///
/// Note: `indices` must be pairwise-disjoint. Swaps are computed against the original
/// list snapshot and applied afterwards, so overlapping index pairs will produce
/// incorrect results.
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn conditionally_swap_distances_plain_ids<T>(
    session: &mut Session,
    swap_when_zero_bits: Vec<Share<Bit>>,
    list: &[(u32, DistanceShare<T>)],
    indices: &[(usize, usize)],
) -> Result<Vec<IdDistance<T>>>
where
    T: IntRing2k + NetworkInt + RingRandFillable + From<u32>,
    Standard: Distribution<T>,
{
    if swap_when_zero_bits.len() != indices.len() {
        bail!("swap bits and indices must have the same length");
    }

    let list_len = list.len();
    for (idx1, idx2) in indices.iter() {
        if *idx1 >= list_len || *idx2 >= list_len {
            bail!(
                "index out of bounds in swap_indices: ({}, {}) for list of length {}",
                idx1,
                idx2,
                list_len
            );
        }
    }

    let role = session.own_role();
    // Convert vector ids into trivial shares (promoted to ring T)
    let mut swapped_list = list
        .iter()
        .map(|(id, d)| {
            let shared_index = Share::from_const(T::from(*id), role);
            (shared_index, *d)
        })
        .collect::<Vec<_>>();
    // Lift swap bits to T shares
    let swap_bits_lifted: Vec<Share<T>> =
        bit_inject(session, VecShare::<Bit>::new_vec(swap_when_zero_bits))
            .await?
            .inner();

    let distances_to_swap = indices
        .iter()
        .map(|(idx1, idx2)| (list[*idx1].1, list[*idx2].1))
        .collect::<Vec<_>>();
    // Select the first distance in each pair based on the control bits
    let first_distances =
        conditionally_select_distance(session, &distances_to_swap, &swap_bits_lifted).await?;
    // Select the second distance in each pair as sum of both distances minus the first selected distance
    let second_distances = distances_to_swap
        .into_iter()
        .zip(first_distances.iter())
        .map(|(d_pair, first_d)| {
            DistanceShare::new(
                d_pair.0.code_dot + d_pair.1.code_dot - first_d.code_dot,
                d_pair.0.mask_dot + d_pair.1.mask_dot - first_d.mask_dot,
            )
        })
        .collect::<Vec<_>>();

    for (bit, (idx1, idx2), first_d, second_d) in izip!(
        swap_bits_lifted.iter(),
        indices.iter(),
        first_distances,
        second_distances
    ) {
        let id1 = list[*idx1].0;
        let id2 = list[*idx2].0;
        // Local computation: IDs are public so no reshare needed.
        // first_id = bit * (id1 - id2) + id2
        let mut first_id = *bit * RingElement(T::from(id1.wrapping_sub(id2)));
        first_id.add_assign_const_role(T::from(id2), role);
        // second_id = (id1 + id2) - first_id
        let mut second_id = -first_id;
        second_id.add_assign_const_role(T::from(id1.wrapping_add(id2)), role);
        swapped_list[*idx1] = (first_id, first_d);
        swapped_list[*idx2] = (second_id, second_d);
    }
    Ok(swapped_list)
}

/// Conditionally swaps the distance shares based on control bits.
/// Given the ith pair of indices (i1, i2), the function does the following.
/// If the ith control bit is 0, it swaps tuples (0-indexed vector id, distance share) with index i1 and i2,
/// otherwise it does nothing.
/// Assumes that the input shares are originally 16-bit and lifted to T.
/// The vector ids are 0-indexed and given in secret shared form.
///
/// Note: `indices` must be pairwise-disjoint. Swaps are computed against the original
/// list snapshot and applied afterwards, so overlapping index pairs will produce
/// incorrect results.
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn conditionally_swap_distances<T>(
    session: &mut Session,
    swap_when_zero_bits: Vec<Share<Bit>>,
    list: &[IdDistance<T>],
    indices: &[(usize, usize)],
) -> Result<Vec<IdDistance<T>>>
where
    T: NetworkInt + RingRandFillable,
    Standard: Distribution<T>,
{
    if swap_when_zero_bits.len() != indices.len() {
        bail!("swap bits and indices must have the same length");
    }
    let list_len = list.len();
    for (idx1, idx2) in indices.iter() {
        if *idx1 >= list_len || *idx2 >= list_len {
            bail!(
                "index out of bounds in swap_indices: ({}, {}) for list of length {}",
                idx1,
                idx2,
                list_len
            );
        }
    }
    // Lift bits to T shares
    let swap_bits_lifted: Vec<Share<T>> =
        bit_inject(session, VecShare::<Bit>::new_vec(swap_when_zero_bits))
            .await?
            .inner();

    let selected = reshare_mux(session, indices.len(), 3, &swap_bits_lifted, |j, f| {
        let (left_id, left_dist) = &list[indices[j].0];
        let (right_id, right_dist) = &list[indices[j].1];
        match f {
            0 => (*left_id, *right_id),
            1 => (left_dist.code_dot, right_dist.code_dot),
            2 => (left_dist.mask_dot, right_dist.mask_dot),
            _ => unreachable!(),
        }
    })
    .await?;

    // Derive both swap elements: first = selected, second = d1 + d2 - selected.
    let mut swapped_list = list.to_vec();
    for (chunk, (idx1, idx2)) in selected.chunks(3).zip(indices) {
        let first_id = chunk[0];
        let first_dist = DistanceShare::new(chunk[1], chunk[2]);

        let (id1, dist1) = &list[*idx1];
        let (id2, dist2) = &list[*idx2];
        let second_id = *id1 + *id2 - first_id;
        let second_dist = DistanceShare::new(
            dist1.code_dot + dist2.code_dot - first_dist.code_dot,
            dist1.mask_dot + dist2.mask_dot - first_dist.mask_dot,
        );

        swapped_list[*idx1] = (first_id, first_dist);
        swapped_list[*idx2] = (second_id, second_dist);
    }

    Ok(swapped_list)
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

/// Signed lift from u16 to Ring48. Same logic as batch_signed_lift but
/// targets 48-bit ring arithmetic.
pub async fn batch_signed_lift_ring48(
    session: &mut Session,
    mut pre_lift: VecShare<u16>,
) -> Result<VecShare<Ring48>> {
    // Compute (v + 2^{15}) % 2^{16}, to make values positive.
    for v in pre_lift.iter_mut() {
        v.add_assign_const_role(1_u16 << 15, session.own_role());
    }
    let mut lifted_values = lift_to_ring48(session, pre_lift).await?;
    // Now we got shares of d1' over 2^48 such that d1' = (d1'_1 + d1'_2 + d1'_3) %
    // 2^{16} = d1. Next we subtract the 2^15 term we've added previously to
    // get signed shares over 2^{48}.
    for v in lifted_values.iter_mut() {
        v.add_assign_const_role(
            Ring48::masked((1_u64 << 48) - (1_u64 << 15)),
            session.own_role(),
        );
    }
    Ok(lifted_values)
}

/// Wrapper over batch_signed_lift_ring48 that lifts a vector (Vec) of 16-bit
/// shares to a vector (Vec) of Ring48 shares.
pub async fn batch_signed_lift_vec_ring48(
    session: &mut Session,
    pre_lift: Vec<Share<u16>>,
) -> Result<Vec<Share<Ring48>>> {
    let pre_lift = VecShare::new_vec(pre_lift);
    Ok(batch_signed_lift_ring48(session, pre_lift).await?.inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::local::{generate_local_identities, LocalRuntime};
    use crate::protocol::test_utils::create_array_sharing;
    use aes_prng::AesRng;
    use rand::RngCore;
    use rand::SeedableRng;
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

    #[tokio::test]
    async fn test_conditionally_select_distances_with_plain_ids() {
        let mut rng = AesRng::seed_from_u64(44_u64);

        // Two distance pairs with plaintext ids
        // When control bit = 1, select left; when 0, select right.
        let left_ids: Vec<u32> = vec![10, 20];
        let right_ids: Vec<u32> = vec![30, 40];
        let left_code: Vec<u32> = vec![100, 200];
        let left_mask: Vec<u32> = vec![500, 600];
        let right_code: Vec<u32> = vec![300, 400];
        let right_mask: Vec<u32> = vec![700, 800];
        // control bits: [1, 0] -> select left for first, right for second
        let control_vals: Vec<u32> = vec![1, 0];

        // Share all values
        let all_vals: Vec<u32> = [
            &left_code[..],
            &left_mask[..],
            &right_code[..],
            &right_mask[..],
            &control_vals[..],
        ]
        .concat();
        let shares = create_array_sharing(&mut rng, &all_vals);

        let sessions = LocalRuntime::mock_sessions_with_channel().await.unwrap();
        let mut jobs = JoinSet::new();

        for (i, session) in sessions.into_iter().enumerate() {
            let session = session.clone();
            let shares_i = shares.of_party(i).clone();
            let left_ids = left_ids.clone();
            let right_ids = right_ids.clone();
            jobs.spawn(async move {
                let mut session = session.lock().await;
                let n = 2usize;
                let left_dist: Vec<(u32, DistanceShare<u32>)> = (0..n)
                    .map(|j| {
                        (
                            left_ids[j],
                            DistanceShare::new(shares_i[j], shares_i[n + j]),
                        )
                    })
                    .collect();
                let right_dist: Vec<(u32, DistanceShare<u32>)> = (0..n)
                    .map(|j| {
                        (
                            right_ids[j],
                            DistanceShare::new(shares_i[2 * n + j], shares_i[3 * n + j]),
                        )
                    })
                    .collect();
                let control_bits: Vec<Share<u32>> = (0..n).map(|j| shares_i[4 * n + j]).collect();

                let result = conditionally_select_distances_with_plain_ids(
                    &mut session,
                    left_dist,
                    right_dist,
                    control_bits,
                )
                .await
                .unwrap();

                // Open id, code_dot, mask_dot for each result
                let ids: Vec<Share<u32>> = result.iter().map(|(id, _)| *id).collect();
                let codes: Vec<Share<u32>> = result.iter().map(|(_, d)| d.code_dot).collect();
                let masks: Vec<Share<u32>> = result.iter().map(|(_, d)| d.mask_dot).collect();

                let opened_ids = open_ring(&mut session, &ids).await.unwrap();
                let opened_codes = open_ring(&mut session, &codes).await.unwrap();
                let opened_masks = open_ring(&mut session, &masks).await.unwrap();
                (opened_ids, opened_codes, opened_masks)
            });
        }

        let results: Vec<(Vec<u32>, Vec<u32>, Vec<u32>)> = jobs.join_all().await;

        // All parties agree
        assert_eq!(results[0], results[1]);
        assert_eq!(results[1], results[2]);

        let (ids, codes, masks) = &results[0];
        // control bit 1 -> left selected: id=10, code=100, mask=500
        assert_eq!(ids[0], 10);
        assert_eq!(codes[0], 100);
        assert_eq!(masks[0], 500);
        // control bit 0 -> right selected: id=40, code=400, mask=800
        assert_eq!(ids[1], 40);
        assert_eq!(codes[1], 400);
        assert_eq!(masks[1], 800);
    }

    #[tokio::test]
    async fn test_conditionally_select_distances_with_shared_ids() {
        let mut rng = AesRng::seed_from_u64(45_u64);

        // (id, code_dot, mask_dot) for left and right, plus control bits
        // left:  [(10, 100, 500), (20, 200, 600)]
        // right: [(30, 300, 700), (40, 400, 800)]
        // control: [0, 1] -> select right for first, left for second
        let vals: Vec<u32> = vec![
            10, 100, 500, // left[0]: id, code, mask
            20, 200, 600, // left[1]
            30, 300, 700, // right[0]
            40, 400, 800, // right[1]
            0, 1, // control bits
        ];
        let shares = create_array_sharing(&mut rng, &vals);

        let sessions = LocalRuntime::mock_sessions_with_channel().await.unwrap();
        let mut jobs = JoinSet::new();

        for (i, session) in sessions.into_iter().enumerate() {
            let session = session.clone();
            let s = shares.of_party(i).clone();
            jobs.spawn(async move {
                let mut session = session.lock().await;
                let left: Vec<IdDistance<u32>> = vec![
                    (s[0], DistanceShare::new(s[1], s[2])),
                    (s[3], DistanceShare::new(s[4], s[5])),
                ];
                let right: Vec<IdDistance<u32>> = vec![
                    (s[6], DistanceShare::new(s[7], s[8])),
                    (s[9], DistanceShare::new(s[10], s[11])),
                ];
                let control = vec![s[12], s[13]];

                let result = conditionally_select_distances_with_shared_ids(
                    &mut session,
                    left,
                    right,
                    control,
                )
                .await
                .unwrap();

                let ids: Vec<Share<u32>> = result.iter().map(|(id, _)| *id).collect();
                let codes: Vec<Share<u32>> = result.iter().map(|(_, d)| d.code_dot).collect();
                let masks: Vec<Share<u32>> = result.iter().map(|(_, d)| d.mask_dot).collect();

                let opened_ids = open_ring(&mut session, &ids).await.unwrap();
                let opened_codes = open_ring(&mut session, &codes).await.unwrap();
                let opened_masks = open_ring(&mut session, &masks).await.unwrap();
                (opened_ids, opened_codes, opened_masks)
            });
        }

        let results: Vec<(Vec<u32>, Vec<u32>, Vec<u32>)> = jobs.join_all().await;

        assert_eq!(results[0], results[1]);
        assert_eq!(results[1], results[2]);

        let (ids, codes, masks) = &results[0];
        // control=0 -> right: id=30, code=300, mask=700
        assert_eq!(ids[0], 30);
        assert_eq!(codes[0], 300);
        assert_eq!(masks[0], 700);
        // control=1 -> left: id=20, code=200, mask=600
        assert_eq!(ids[1], 20);
        assert_eq!(codes[1], 200);
        assert_eq!(masks[1], 600);
    }

    #[tokio::test]
    async fn test_conditionally_swap_distances_plain_ids() {
        let mut rng = AesRng::seed_from_u64(46_u64);

        // List: [(10, (100, 500)), (20, (200, 600)), (30, (300, 700)), (40, (400, 800))]
        // Non-overlapping swap indices: [(0, 1), (2, 3)]
        // Swap-when-zero bits: [0, 1]
        //   bit=0 for (0,1) -> swap positions 0 and 1
        //   bit=1 for (2,3) -> no swap
        let list_ids: Vec<u32> = vec![10, 20, 30, 40];
        let code_vals: Vec<u32> = vec![100, 200, 300, 400];
        let mask_vals: Vec<u32> = vec![500, 600, 700, 800];

        let all_vals: Vec<u32> = [&code_vals[..], &mask_vals[..]].concat();
        let shares = create_array_sharing(&mut rng, &all_vals);

        let bit_vals: Vec<Bit> = vec![Bit::new(false), Bit::new(true)];
        let bit_shares = create_array_sharing(&mut rng, &bit_vals);

        let sessions = LocalRuntime::mock_sessions_with_channel().await.unwrap();
        let mut jobs = JoinSet::new();

        for (i, session) in sessions.into_iter().enumerate() {
            let session = session.clone();
            let s = shares.of_party(i).clone();
            let bs = bit_shares.of_party(i).clone();
            let list_ids = list_ids.clone();
            jobs.spawn(async move {
                let mut session = session.lock().await;
                let n = 4usize;
                let list: Vec<(u32, DistanceShare<u32>)> = (0..n)
                    .map(|j| (list_ids[j], DistanceShare::new(s[j], s[n + j])))
                    .collect();
                let indices = vec![(0usize, 1usize), (2usize, 3usize)];

                let result =
                    conditionally_swap_distances_plain_ids(&mut session, bs, &list, &indices)
                        .await
                        .unwrap();

                let ids: Vec<Share<u32>> = result.iter().map(|(id, _)| *id).collect();
                let codes: Vec<Share<u32>> = result.iter().map(|(_, d)| d.code_dot).collect();
                let masks: Vec<Share<u32>> = result.iter().map(|(_, d)| d.mask_dot).collect();

                let opened_ids = open_ring(&mut session, &ids).await.unwrap();
                let opened_codes = open_ring(&mut session, &codes).await.unwrap();
                let opened_masks = open_ring(&mut session, &masks).await.unwrap();
                (opened_ids, opened_codes, opened_masks)
            });
        }

        let results: Vec<(Vec<u32>, Vec<u32>, Vec<u32>)> = jobs.join_all().await;

        assert_eq!(results[0], results[1]);
        assert_eq!(results[1], results[2]);

        let (ids, codes, masks) = &results[0];
        // bit=0 for (0,1) -> swap positions 0 and 1
        assert_eq!(ids[0], 20);
        assert_eq!(codes[0], 200);
        assert_eq!(masks[0], 600);
        assert_eq!(ids[1], 10);
        assert_eq!(codes[1], 100);
        assert_eq!(masks[1], 500);
        // bit=1 for (2,3) -> no swap
        assert_eq!(ids[2], 30);
        assert_eq!(codes[2], 300);
        assert_eq!(masks[2], 700);
        assert_eq!(ids[3], 40);
        assert_eq!(codes[3], 400);
        assert_eq!(masks[3], 800);
    }

    #[tokio::test]
    async fn test_conditionally_swap_distances() {
        let mut rng = AesRng::seed_from_u64(47_u64);

        // List: [(id=10, code=100, mask=500), (id=20, code=200, mask=600), (id=30, code=300, mask=700)]
        // Swap indices: [(0, 2)]
        // Swap-when-zero bits: [0] -> swap positions 0 and 2
        let vals: Vec<u32> = vec![
            10, 100, 500, // item 0
            20, 200, 600, // item 1
            30, 300, 700, // item 2
        ];
        let shares = create_array_sharing(&mut rng, &vals);
        let bit_vals: Vec<Bit> = vec![Bit::new(false)];
        let bit_shares = create_array_sharing(&mut rng, &bit_vals);

        let sessions = LocalRuntime::mock_sessions_with_channel().await.unwrap();
        let mut jobs = JoinSet::new();

        for (i, session) in sessions.into_iter().enumerate() {
            let session = session.clone();
            let s = shares.of_party(i).clone();
            let bs = bit_shares.of_party(i).clone();
            jobs.spawn(async move {
                let mut session = session.lock().await;
                let list: Vec<IdDistance<u32>> = vec![
                    (s[0], DistanceShare::new(s[1], s[2])),
                    (s[3], DistanceShare::new(s[4], s[5])),
                    (s[6], DistanceShare::new(s[7], s[8])),
                ];
                let indices = vec![(0usize, 2usize)];

                let result = conditionally_swap_distances(&mut session, bs, &list, &indices)
                    .await
                    .unwrap();

                let ids: Vec<Share<u32>> = result.iter().map(|(id, _)| *id).collect();
                let codes: Vec<Share<u32>> = result.iter().map(|(_, d)| d.code_dot).collect();
                let masks: Vec<Share<u32>> = result.iter().map(|(_, d)| d.mask_dot).collect();

                let opened_ids = open_ring(&mut session, &ids).await.unwrap();
                let opened_codes = open_ring(&mut session, &codes).await.unwrap();
                let opened_masks = open_ring(&mut session, &masks).await.unwrap();
                (opened_ids, opened_codes, opened_masks)
            });
        }

        let results: Vec<(Vec<u32>, Vec<u32>, Vec<u32>)> = jobs.join_all().await;

        assert_eq!(results[0], results[1]);
        assert_eq!(results[1], results[2]);

        let (ids, codes, masks) = &results[0];
        // bit=0 -> swap positions 0 and 2
        // pos0 = old pos2: id=30, code=300, mask=700
        // pos1 = unchanged: id=20, code=200, mask=600
        // pos2 = old pos0: id=10, code=100, mask=500
        assert_eq!(ids[0], 30);
        assert_eq!(codes[0], 300);
        assert_eq!(masks[0], 700);
        assert_eq!(ids[1], 20);
        assert_eq!(codes[1], 200);
        assert_eq!(masks[1], 600);
        assert_eq!(ids[2], 10);
        assert_eq!(codes[2], 100);
        assert_eq!(masks[2], 500);
    }
}
