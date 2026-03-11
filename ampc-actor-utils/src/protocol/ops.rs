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
pub const B: u64 = 1 << B_BITS;

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

    // Conditional multiplexing:
    // If control bit is 1, select d1, else select d2.
    // res = c * d1 + (1 - c) * d2 = d2 + c * (d1 - d2);
    // We need to do it for both code_dot and mask_dot.

    // we start with the mult of c and d1-d2
    let (prf_my_values, prf_prev_values) = session.prf.gen_rands_batch(distances.len() * 2);

    let res_a: Vec<RingElement<T>> = izip!(
        distances.iter(),
        control_bits.iter(),
        prf_my_values.0.chunks(2),
        prf_prev_values.0.chunks(2)
    )
    .flat_map(|((d1, d2), c, my_prf, prev_prf)| {
        let code = d1.code_dot - d2.code_dot;
        let mask = d1.mask_dot - d2.mask_dot;
        let code_zero_share = my_prf[0] - prev_prf[0]; // equivalent to gen_zero_share()
        let mask_zero_share = my_prf[1] - prev_prf[1]; // equivalent to gen_zero_share()
        let code_mul_a = code_zero_share + c.a * code.a + c.b * code.a + c.a * code.b;
        let mask_mul_a = mask_zero_share + c.a * mask.a + c.b * mask.a + c.a * mask.b;
        [code_mul_a, mask_mul_a]
    })
    .collect();

    let network = &mut session.network_session;

    let message = if res_a.len() == 1 {
        T::new_network_element(res_a[0])
    } else {
        T::new_network_vec(res_a.clone())
    };
    network.send_next(message).await?;

    let res_b = T::into_vec(network.receive_prev().await?)?;

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
            code_dot: res.code_dot + d2.code_dot,
            mask_dot: res.mask_dot + d2.mask_dot,
        })
        .collect())
}

/// Conditionally selects equally-sized slices of input shares based on control bits.
/// If the control bit is 1, it selects the left value shares; otherwise, it selects the right value share.
async fn select_shared_slices_by_bits<T>(
    session: &mut Session,
    left_values: &[Share<T>],
    right_values: &[Share<T>],
    control_bits: &[Share<T>],
    slice_size: usize,
) -> Result<Vec<Share<T>>>
where
    T: IntRing2k + NetworkInt,
    Standard: Distribution<T>,
{
    if left_values.len() != right_values.len() {
        bail!("Left and right values must have the same length");
    }
    if !left_values.len().is_multiple_of(slice_size) {
        bail!("Left and right values length must be multiple of slice size");
    }
    if control_bits.len() != left_values.len() / slice_size {
        bail!("Number of control bits must match number of slices");
    }

    // Conditional multiplexing:
    // If control bit is 1, select left_value, else select right_value.
    // res = c * (left_value - right_value) + right_value
    // Compute c * (left_value - right_value)
    let res_a: Vec<RingElement<T>> = izip!(
        left_values.chunks(slice_size),
        right_values.chunks(slice_size),
        control_bits.iter()
    )
    .flat_map(|(left_chunk, right_chunk, c)| {
        left_chunk
            .iter()
            .zip(right_chunk.iter())
            .map(|(left, right)| {
                let diff = *left - *right;
                session.prf.gen_zero_share::<T>() + c.a * diff.a + c.b * diff.a + c.a * diff.b
            })
            .collect::<Vec<_>>()
    })
    .collect();

    let network = &mut session.network_session;

    network.send_next(T::new_network_vec(res_a.clone())).await?;

    let res_b: Vec<RingElement<T>> = T::into_vec(network.receive_prev().await?)?;

    // Pack networking messages into shares and
    // compute the result by adding the right shares
    Ok(izip!(res_a, res_b)
        .map(|(a, b)| Share::new(a, b))
        .zip(right_values.iter())
        .map(|(res, right)| res + right)
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
    T: IntRing2k + NetworkInt + From<u32>,
    Standard: Distribution<T>,
{
    if left_distances.len() != control_bits.len() {
        eyre::bail!("Number of distances must match number of control bits");
    }
    if left_distances.len() != right_distances.len() {
        eyre::bail!("Left and right distances must have the same length");
    }
    if left_distances.is_empty() {
        eyre::bail!("Distances must not be empty");
    }

    // Now select distances
    let (left_ids, left_dist): (Vec<_>, Vec<_>) = left_distances.into_iter().unzip();
    let (right_ids, right_dist): (Vec<_>, Vec<_>) = right_distances.into_iter().unzip();
    let left_dist = left_dist
        .into_iter()
        .flat_map(|d| [d.code_dot, d.mask_dot])
        .collect::<Vec<_>>();
    let right_dist = right_dist
        .into_iter()
        .flat_map(|d| [d.code_dot, d.mask_dot])
        .collect::<Vec<_>>();

    let distances =
        select_shared_slices_by_bits(session, &left_dist, &right_dist, &control_bits, 2)
            .await?
            .into_iter()
            .tuples()
            .map(|(code_dot, mask_dot)| DistanceShare::new(code_dot, mask_dot));

    // Select ids first: c * (left_id - right_id) + right_id
    let ids = izip!(left_ids, right_ids, control_bits).map(|(left_id, right_id, c)| {
        let diff = left_id.wrapping_sub(right_id);
        let mut res = c * RingElement(T::from(diff));
        res.add_assign_const_role(T::from(right_id), session.own_role());
        res
    });

    Ok(izip!(ids, distances)
        .map(|(id, distance)| (id, distance))
        .collect::<Vec<_>>())
}

#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn conditionally_select_distances_with_shared_ids<T>(
    session: &mut Session,
    left_distances: Vec<IdDistance<T>>,
    right_distances: Vec<IdDistance<T>>,
    control_bits: Vec<Share<T>>,
) -> Result<Vec<IdDistance<T>>>
where
    T: IntRing2k + NetworkInt,
    Standard: Distribution<T>,
{
    if left_distances.len() != control_bits.len() {
        eyre::bail!("Number of distances must match number of control bits");
    }
    if left_distances.len() != right_distances.len() {
        eyre::bail!("Left and right distances must have the same length");
    }
    if left_distances.is_empty() {
        eyre::bail!("Distances must not be empty");
    }

    let left_dist = left_distances
        .into_iter()
        .flat_map(|(id, d)| [id, d.code_dot, d.mask_dot])
        .collect::<Vec<_>>();
    let right_dist = right_distances
        .into_iter()
        .flat_map(|(id, d)| [id, d.code_dot, d.mask_dot])
        .collect::<Vec<_>>();
    let distances =
        select_shared_slices_by_bits(session, &left_dist, &right_dist, &control_bits, 3)
            .await?
            .into_iter()
            .tuples()
            .map(|(id, code_dot, mask_dot)| (id, DistanceShare::new(code_dot, mask_dot)))
            .collect::<Vec<_>>();

    Ok(distances)
}

/// Conditionally swaps the distance shares based on control bits.
/// Given the ith pair of indices (i1, i2), the function does the following.
/// If the control bit is 0, it swaps tuples (id, distance share) with index i1 and i2,
/// otherwise it does nothing.
/// The vector ids are in plaintext and propagated in secret shared form.
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn conditionally_swap_distances_plain_ids<T>(
    session: &mut Session,
    swap_bits: Vec<Share<Bit>>,
    list: &[(u32, DistanceShare<T>)],
    indices: &[(usize, usize)],
) -> Result<Vec<IdDistance<T>>>
where
    T: IntRing2k + NetworkInt + RingRandFillable + From<u32>,
    Standard: Distribution<T>,
{
    if swap_bits.len() != indices.len() {
        eyre::bail!("swap bits and indices must have the same length");
    }
    let role = session.own_role();
    // Convert vector ids into trivial shares (promoted to ring T)
    let mut encrypted_list = list
        .iter()
        .map(|(id, d)| {
            let shared_index = Share::from_const(T::from(*id), role);
            (shared_index, *d)
        })
        .collect::<Vec<_>>();
    // Lift swap bits to T shares
    let swap_bits_lifted: Vec<Share<T>> = bit_inject(session, VecShare::<Bit>::new_vec(swap_bits))
        .await?
        .inner();

    let distances_to_swap = indices
        .iter()
        .filter_map(|(idx1, idx2)| match (list.get(*idx1), list.get(*idx2)) {
            (Some((_, d1)), Some((_, d2))) => Some((*d1, *d2)),
            _ => None,
        })
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
        let mut not_bit = -bit;
        not_bit.add_assign_const_role(T::from(1u32), role);
        let id1 = T::from(list[*idx1].0);
        let id2 = T::from(list[*idx2].0);
        // Only propagate index and skip version id.
        // This computation is local as indices are public.
        let first_id = bit * id1 + not_bit * id2;
        let second_id = bit * id2 + not_bit * id1;
        encrypted_list[*idx1] = (first_id, first_d);
        encrypted_list[*idx2] = (second_id, second_d);
    }
    Ok(encrypted_list)
}

/// Conditionally swaps the distance shares based on control bits.
/// Given the ith pair of indices (i1, i2), the function does the following.
/// If the ith control bit is 0, it swaps tuples (0-indexed vector id, distance share) with index i1 and i2,
/// otherwise it does nothing.
/// Assumes that the input shares are originally 16-bit and lifted to T.
/// The vector ids are 0-indexed and given in secret shared form.
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn conditionally_swap_distances<T>(
    session: &mut Session,
    swap_bits: Vec<Share<Bit>>,
    list: &[IdDistance<T>],
    indices: &[(usize, usize)],
) -> Result<Vec<IdDistance<T>>>
where
    T: IntRing2k + NetworkInt + RingRandFillable,
    Standard: Distribution<T>,
{
    if swap_bits.len() != indices.len() {
        return Err(eyre!("swap bits and indices must have the same length"));
    }
    // Lift bits to T shares
    let swap_bits_lifted: Vec<Share<T>> = bit_inject(session, VecShare::<Bit>::new_vec(swap_bits))
        .await?
        .inner();

    // A helper closure to compute the difference of two input shares and prepare the a part of the product of this difference and the control bit.
    let mut mul_share_a = |x: Share<T>, y: Share<T>, sb: &Share<T>| -> RingElement<T> {
        let diff = x - y;
        session.prf.gen_zero_share::<T>() + sb.a * diff.a + sb.b * diff.a + sb.a * diff.b
    };

    // Conditional swapping:
    // If control bit c is 1, return (d1, d2); otherwise, (d2, d1), which can be computed as:
    // - first tuple element = c * (d1 - d2) + d2;
    // - second tuple element = d1 - c * (d1 - d2).
    // We need to do it for ids, code_dot and mask_dot.

    // Compute c * (d1-d2)
    let res_a: Vec<RingElement<T>> = indices
        .iter()
        .zip(swap_bits_lifted.iter())
        .flat_map(|((idx1, idx2), sb)| {
            let (id1, d1) = &list[*idx1];
            let (id2, d2) = &list[*idx2];

            let id = mul_share_a(*id1, *id2, sb);
            let code_dot_a = mul_share_a(d1.code_dot, d2.code_dot, sb);
            let mask_dot_a = mul_share_a(d1.mask_dot, d2.mask_dot, sb);
            [id, code_dot_a, mask_dot_a]
        })
        .collect();

    let network = &mut session.network_session;

    network.send_next(T::new_network_vec(res_a.clone())).await?;

    let res_b: Vec<RingElement<T>> = T::into_vec(network.receive_prev().await?)?;

    // Finally compute the swapped tuples.
    let swapped_distances = izip!(res_a, res_b)
        // combine a and b part into shares
        .map(|(a, b)| Share::new(a, b))
        // combine the code and mask parts into DistanceShare
        .tuples()
        .map(|(id, code, mask)| {
            (
                id,
                DistanceShare {
                    code_dot: code,
                    mask_dot: mask,
                },
            )
        })
        .zip(indices.iter())
        .map(|((res_id, res_dist), (idx1, idx2))| {
            let (id1, dist1) = &list[*idx1];
            let (id2, dist2) = &list[*idx2];
            // first tuple element = c * (d1 - d2) + d2
            // second tuple element = d1 - c * (d1 - d2)
            let first_id = res_id + *id2;
            let second_id = *id1 - res_id;
            let first_distance = DistanceShare {
                code_dot: res_dist.code_dot + dist2.code_dot,
                mask_dot: res_dist.mask_dot + dist2.mask_dot,
            };
            let second_distance = DistanceShare {
                code_dot: dist1.code_dot - res_dist.code_dot,
                mask_dot: dist1.mask_dot - res_dist.mask_dot,
            };
            ((first_id, first_distance), (second_id, second_distance))
        })
        .collect::<Vec<_>>();

    // Update the input list with the swapped tuples.
    let mut swapped_list = list.to_vec();
    for (((id1, d1), (id2, d2)), (idx1, idx2)) in swapped_distances.into_iter().zip(indices) {
        swapped_list[*idx1] = (id1, d1);
        swapped_list[*idx2] = (id2, d2);
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
