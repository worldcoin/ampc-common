// Single-dealer sharing and reconstruction for 3-of-5 replicated secret sharing.
//
// An RSS scheme consists of ten pieces, one for each unordered pair {i, j} of parties.
// Each share is replicated to the three parties outside its pair, so every
// party holds six pieces. The dealer derives five random pieces from shared
// threshold-PRF keys, sets the four pieces whose pair contains the dealer to
// zero, and uses the final piece as a correction so that all ten reconstruct
// to the secret. Only that correction is sent over the network.

use crate::execution::player::Role;
use crate::execution::session::{NetworkSession, SessionHandles};
use crate::network::mpc::NetworkInt;
use crate::protocol::prf::{orbit5_roles, PartyPair, ThresholdPrfKeys};
use ampc_secret_sharing::shares::int_ring::IntRing2k;
use ampc_secret_sharing::shares::ring_impl::RingElement;
use ampc_secret_sharing::shares::rss5::{slot_pair, RssShare, ORBIT5_PARTY_COUNT, RSS5_SLOTS_HELD};
use eyre::{bail, eyre, Result, WrapErr};
use num_traits::Zero;
use rand::Rng;
use rand_distr::{Distribution, Standard};
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};

const EMPTY_BATCH_ERROR: &str = "RSS5 batch must not be empty";

/// How the ten unique RSS5 pieces combine to reconstruct a value.
#[derive(Clone, Copy)]
pub enum ShareType {
    /// Addition modulo `2^k`.
    Arithmetic,
    /// Bitwise XOR of packed ring elements.
    Boolean,
}

impl ShareType {
    fn unmask<T: IntRing2k>(self, value: RingElement<T>, mask: RingElement<T>) -> RingElement<T> {
        match self {
            Self::Arithmetic => value - mask,
            Self::Boolean => value ^ mask,
        }
    }

    fn combine<T: IntRing2k>(self, left: RingElement<T>, right: RingElement<T>) -> RingElement<T> {
        match self {
            Self::Arithmetic => left + right,
            Self::Boolean => left ^ right,
        }
    }
}

/// Chooses the piece that carries the correction and the two parties that
/// receive it. The other two non-dealer parties are the correction pair and
/// therefore do not hold that piece.
fn select_correction_pair_and_recipients(dealer: Role) -> Result<(PartyPair, [Role; 2])> {
    if dealer.index() >= ORBIT5_PARTY_COUNT {
        bail!("dealer role {dealer:?} is outside the 5-party role set");
    }

    let others: Vec<Role> = orbit5_roles()
        .into_iter()
        .filter(|role| *role != dealer)
        .collect();
    Ok((PartyPair::new(others[0], others[1]), [others[2], others[3]]))
}

async fn dealer_rss5_batch<T>(
    session: &mut NetworkSession,
    threshold: &mut ThresholdPrfKeys,
    dealer: Role,
    dealer_inputs: Vec<RingElement<T>>,
    share_type: ShareType,
) -> Result<Vec<RssShare<T>>>
where
    T: NetworkInt,
    Standard: Distribution<T>,
{
    if dealer_inputs.is_empty() {
        bail!(EMPTY_BATCH_ERROR);
    }

    let own_role = session.own_role();
    if threshold.own_role() != own_role {
        bail!(
            "threshold PRF keys belong to {:?}, but the session belongs to {own_role:?}",
            threshold.own_role()
        );
    }
    let (correction_pair, recipients) = select_correction_pair_and_recipients(dealer)?;
    let batch_size = dealer_inputs.len();

    let mut slots: [Option<Vec<RingElement<T>>>; RSS5_SLOTS_HELD] = std::array::from_fn(|_| None);
    let mut correction_slot = None;

    for (slot, values) in slots.iter_mut().enumerate() {
        let (i, j) = slot_pair(own_role.index(), slot); //global rss_share idx
        let (party_i, party_j) = (Role::new(i), Role::new(j));
        let pair = PartyPair::new(party_i, party_j);

        if pair == correction_pair {
            correction_slot = Some(slot);
        } else if pair.contains(dealer) {
            *values = Some(vec![RingElement::zero(); batch_size]);
        } else {
            let rng = threshold.get_mut(party_i, party_j).ok_or_else(|| {
                eyre!(
                    "role {own_role:?} has no threshold PRF key for pair ({party_i:?}, {party_j:?})"
                )
            })?;
            *values = Some(
                (0..batch_size)
                    .map(|_| rng.gen::<RingElement<T>>())
                    .collect(),
            );
        }
    }

    if let Some(slot) = correction_slot {
        let correction = if own_role == dealer {
            let mut correction = dealer_inputs;
            for masks in slots.iter().flatten() {
                for (value, mask) in correction.iter_mut().zip(masks) {
                    *value = share_type.unmask(*value, *mask);
                }
            }

            session
                .send_to(T::new_network_vec(correction.clone()), &recipients[0])
                .await
                .wrap_err_with(|| {
                    format!(
                        "dealer {dealer:?} failed to send RSS5 correction to {:?}",
                        recipients[0]
                    )
                })?;
            session
                .send_to(T::new_network_vec(correction.clone()), &recipients[1])
                .await
                .wrap_err_with(|| {
                    format!(
                        "dealer {dealer:?} failed to send RSS5 correction to {:?}",
                        recipients[1]
                    )
                })?;
            correction
        } else {
            if !recipients.contains(&own_role) {
                bail!(
                    "invalid correction route: {own_role:?} holds {correction_pair:?} but is not a recipient"
                );
            }
            let received = session.receive_from(&dealer).await.wrap_err_with(|| {
                format!("{own_role:?} failed to receive RSS5 correction from {dealer:?}")
            })?;
            let correction = T::into_vec(received).wrap_err_with(|| {
                format!("dealer {dealer:?} sent an invalid RSS5 correction payload")
            })?;
            if correction.len() != batch_size {
                bail!(
                    "expected {batch_size} RSS5 correction elements from {dealer:?}, got {}",
                    correction.len()
                );
            }
            correction
        };
        slots[slot] = Some(correction);
    }

    let slots: [Vec<RingElement<T>>; RSS5_SLOTS_HELD] = slots
        .into_iter()
        .enumerate()
        .map(|(slot, values)| {
            values.ok_or_else(|| eyre!("RSS5 slot {slot} was not initialized for {own_role:?}"))
        })
        .collect::<Result<Vec<_>>>()?
        .try_into()
        .map_err(|_| eyre!("internal RSS5 slot count mismatch"))?;

    Ok((0..batch_size)
        .map(|index| RssShare {
            slots: std::array::from_fn(|slot| slots[slot][index]),
        })
        .collect())
}

/// Deals a non-empty batch of arithmetic secrets in one network message.
///
/// All parties must provide the same batch length. Only the dealer's inputs
/// are read; other parties may provide a same-length vector of zeros.
/// Correction recipients validate the received length. Other parties receive
/// no message, so this function cannot verify their batch lengths agree.
pub async fn dealer_rss5_arithmetic<T>(
    session: &mut NetworkSession,
    threshold: &mut ThresholdPrfKeys,
    dealer: Role,
    dealer_inputs: Vec<RingElement<T>>,
) -> Result<Vec<RssShare<T>>>
where
    T: NetworkInt,
    Standard: Distribution<T>,
{
    dealer_rss5_batch(
        session,
        threshold,
        dealer,
        dealer_inputs,
        ShareType::Arithmetic,
    )
    .await
}

/// Deals a non-empty batch of boolean secrets in one network message.
///
/// All parties must provide the same batch length. Only the dealer's inputs
/// are read; other parties may provide a same-length vector of zeros.
/// Correction recipients validate the received length. Other parties receive
/// no message, so this function cannot verify their batch lengths agree.
pub async fn dealer_rss5_boolean<T>(
    session: &mut NetworkSession,
    threshold: &mut ThresholdPrfKeys,
    dealer: Role,
    dealer_inputs: Vec<RingElement<T>>,
) -> Result<Vec<RssShare<T>>>
where
    T: NetworkInt,
    Standard: Distribution<T>,
{
    dealer_rss5_batch(
        session,
        threshold,
        dealer,
        dealer_inputs,
        ShareType::Boolean,
    )
    .await
}

fn validate_reconstruction_roles<T>(shares: &[(Role, T)]) -> Result<()> {
    if shares.len() != ORBIT5_PARTY_COUNT {
        bail!(
            "RSS5 reconstruction requires {ORBIT5_PARTY_COUNT} party views, got {}",
            shares.len()
        );
    }

    let actual: BTreeSet<Role> = shares.iter().map(|(role, _)| *role).collect();
    let expected: BTreeSet<Role> = orbit5_roles().into_iter().collect();
    if actual != expected {
        bail!("RSS5 reconstruction requires exactly roles {expected:?}, got {actual:?}");
    }
    Ok(())
}

fn rss5_reconstruction_impl<T: IntRing2k>(
    shares: &[(Role, Vec<RssShare<T>>)],
    share_type: ShareType,
) -> Result<Vec<RingElement<T>>> {
    if shares.is_empty() {
        bail!(EMPTY_BATCH_ERROR);
    }
    validate_reconstruction_roles(shares)?;

    let batch_size = shares.first().map_or(0, |(_, batch)| batch.len());
    for (role, batch) in shares {
        if batch.is_empty() {
            bail!(EMPTY_BATCH_ERROR);
        }
        if batch.len() != batch_size {
            bail!(
                "RSS5 reconstruction expected {batch_size} shares from {role:?}, got {}",
                batch.len()
            );
        }
    }

    let mut pieces: BTreeMap<(usize, usize), Vec<RingElement<T>>> = BTreeMap::new();
    for (role, batch) in shares {
        for slot in 0..RSS5_SLOTS_HELD {
            let pair = slot_pair(role.index(), slot);
            let values: Vec<RingElement<T>> = batch.iter().map(|share| share.slots[slot]).collect();
            match pieces.entry(pair) {
                Entry::Vacant(entry) => {
                    entry.insert(values);
                }
                Entry::Occupied(entry) => {
                    if let Some(index) = entry
                        .get()
                        .iter()
                        .zip(&values)
                        .position(|(left, right)| left != right)
                    {
                        bail!("RSS5 parties disagree on piece {pair:?} at batch index {index}");
                    }
                }
            }
        }
    }
    if pieces.len() != 10 {
        bail!("RSS5 reconstruction found {} of 10 pieces", pieces.len());
    }

    let mut secrets = vec![RingElement::zero(); batch_size];
    for values in pieces.values() {
        for (secret, value) in secrets.iter_mut().zip(values) {
            *secret = share_type.combine(*secret, *value);
        }
    }
    Ok(secrets)
}

/// Checks all five replicated views and reconstructs a batch in input order.
///
/// Provide exactly one view for each role 0 through 4, in any order, with
/// equal, non-empty batch lengths. A single value uses a batch of length one.
/// Empty inputs, mismatched lengths, and inconsistent replicated pieces return
/// an error.
///
/// `share_type` must match the dealer: arithmetic sums pieces modulo `2^k`,
/// while boolean XORs packed ring elements without bit decomposition.
/// This is a local operation over already collected shares, with no network IO.
pub fn rss5_reconstruction<T: IntRing2k>(
    shares: &[(Role, Vec<RssShare<T>>)],
    share_type: ShareType,
) -> Result<Vec<RingElement<T>>> {
    rss5_reconstruction_impl(shares, share_type)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::local::{generate_local_identities_orbit5, LocalRuntime};
    use crate::protocol::ops::setup_threshold_prf_keys;
    use tokio::task::JoinSet;

    fn local_seeds() -> Vec<[u8; 16]> {
        (0..ORBIT5_PARTY_COUNT)
            .map(|index| {
                let mut seed = [0_u8; 16];
                seed[0] = index as u8;
                seed
            })
            .collect()
    }

    async fn run_arithmetic(
        dealer: Role,
        scalar: u16,
        batch: Vec<u16>,
    ) -> (
        Vec<(Role, Vec<RssShare<u16>>)>,
        Vec<(Role, Vec<RssShare<u16>>)>,
    ) {
        let identities = generate_local_identities_orbit5();
        let runtime = LocalRuntime::new(identities, local_seeds()).await.unwrap();
        let mut jobs = JoinSet::new();

        for session in runtime.sessions {
            let batch = batch.clone();
            jobs.spawn(async move {
                let mut network_session = session.network_session;
                let own_role = network_session.own_role();
                let mut threshold = setup_threshold_prf_keys(&mut network_session)
                    .await
                    .unwrap();
                assert_eq!(
                    dealer_rss5_arithmetic::<u16>(
                        &mut network_session,
                        &mut threshold,
                        dealer,
                        vec![],
                    )
                    .await
                    .unwrap_err()
                    .to_string(),
                    "RSS5 batch must not be empty"
                );
                let scalar = RingElement(if own_role == dealer { scalar } else { 0 });
                let batch = batch
                    .into_iter()
                    .map(|value| RingElement(if own_role == dealer { value } else { 0 }))
                    .collect();

                let single_shares = dealer_rss5_arithmetic(
                    &mut network_session,
                    &mut threshold,
                    dealer,
                    vec![scalar],
                )
                .await
                .unwrap();
                assert_eq!(single_shares.len(), 1);
                let batch_shares = dealer_rss5_arithmetic(
                    &mut network_session,
                    &mut threshold,
                    dealer,
                    batch,
                )
                .await
                .unwrap();
                (own_role, single_shares, batch_shares)
            });
        }

        let results = jobs.join_all().await;
        let scalar = results
            .iter()
            .map(|(role, shares, _)| (*role, shares.clone()))
            .collect();
        let batch = results
            .into_iter()
            .map(|(role, _, shares)| (role, shares))
            .collect();
        (scalar, batch)
    }

    type BooleanShares = RssShare<u16>;
    type BooleanBatchShares = Vec<BooleanShares>;

    async fn run_boolean(
        dealer: Role,
        scalar: u16,
        batch: Vec<u16>,
    ) -> (
        Vec<(Role, BooleanBatchShares)>,
        Vec<(Role, BooleanBatchShares)>,
    ) {
        let identities = generate_local_identities_orbit5();
        let runtime = LocalRuntime::new(identities, local_seeds()).await.unwrap();
        let mut jobs = JoinSet::new();

        for session in runtime.sessions {
            let batch = batch.clone();
            jobs.spawn(async move {
                let mut network_session = session.network_session;
                let own_role = network_session.own_role();
                let mut threshold = setup_threshold_prf_keys(&mut network_session)
                    .await
                    .unwrap();
                assert_eq!(
                    dealer_rss5_boolean::<u16>(
                        &mut network_session,
                        &mut threshold,
                        dealer,
                        vec![],
                    )
                    .await
                    .unwrap_err()
                    .to_string(),
                    "RSS5 batch must not be empty"
                );
                let scalar = RingElement(if own_role == dealer { scalar } else { 0 });
                let batch = batch
                    .into_iter()
                    .map(|value| RingElement(if own_role == dealer { value } else { 0 }))
                    .collect();

                let single_shares = dealer_rss5_boolean(
                    &mut network_session,
                    &mut threshold,
                    dealer,
                    vec![scalar],
                )
                .await
                .unwrap();
                assert_eq!(single_shares.len(), 1);
                let batch_shares =
                    dealer_rss5_boolean(&mut network_session, &mut threshold, dealer, batch)
                        .await
                        .unwrap();
                (own_role, single_shares, batch_shares)
            });
        }

        let results = jobs.join_all().await;
        let scalar = results
            .iter()
            .map(|(role, shares, _)| (*role, shares.clone()))
            .collect();
        let batch = results
            .into_iter()
            .map(|(role, _, shares)| (role, shares))
            .collect();
        (scalar, batch)
    }

    fn check_reconstruction_batch_validation(
        shares: &[(Role, Vec<RssShare<u16>>)],
        share_type: ShareType,
    ) {
        let empty_batches: Vec<_> = shares.iter().map(|(role, _)| (*role, vec![])).collect();
        for empty in [&[][..], empty_batches.as_slice()] {
            assert_eq!(
                rss5_reconstruction::<u16>(empty, share_type)
                    .unwrap_err()
                    .to_string(),
                "RSS5 batch must not be empty"
            );
        }

        let mut mismatched = shares.to_vec();
        mismatched[1].1.pop();
        let error = rss5_reconstruction(&mismatched, share_type).unwrap_err();
        assert!(error.to_string().contains("expected 4 shares"));
        assert!(error.to_string().contains("got 3"));
    }

    #[tokio::test]
    async fn arithmetic_single_element_and_batch_round_trip() {
        let expected_batch = vec![0, 1, u16::MAX, 12_345];
        let (scalar, batch) = run_arithmetic(Role::new(0), 42, expected_batch.clone()).await;

        assert_eq!(
            rss5_reconstruction(&scalar, ShareType::Arithmetic).unwrap(),
            vec![RingElement(42)]
        );
        assert_eq!(
            rss5_reconstruction(&batch, ShareType::Arithmetic).unwrap(),
            expected_batch
                .into_iter()
                .map(RingElement)
                .collect::<Vec<_>>()
        );
        check_reconstruction_batch_validation(&batch, ShareType::Arithmetic);
    }

    #[tokio::test]
    async fn boolean_single_element_and_batch_round_trip() {
        let expected_batch = vec![0, 0xffff, 0x1234, 0xaaaa];
        let (scalar, batch) = run_boolean(Role::new(3), 0xa55a, expected_batch.clone()).await;

        assert_eq!(
            rss5_reconstruction(&scalar, ShareType::Boolean).unwrap(),
            vec![RingElement(0xa55a)]
        );
        assert_eq!(
            rss5_reconstruction(&batch, ShareType::Boolean).unwrap(),
            expected_batch
                .into_iter()
                .map(RingElement)
                .collect::<Vec<_>>()
        );
        check_reconstruction_batch_validation(&batch, ShareType::Boolean);
    }

    #[tokio::test]
    //stress test that PRF outputs remain synchronized across batch size, dealer.
    async fn dealer_rss5_many_instances() {
        use ShareType::{Arithmetic, Boolean};

        // Vary batch sizes for the same dealer, then rotate through the
        // remaining dealers. Alternate arithmetic and boolean throughout.
        let rounds: [(usize, ShareType, &[u16]); 7] = [
            (0, Arithmetic, &[42]),
            (0, Boolean, &[0, 1, 2, 3, 0x1234, 0xaaaa, u16::MAX]),
            (0, Arithmetic, &[u16::MAX, 7]),
            (1, Boolean, &[0xa55a, 0, 1]),
            (2, Arithmetic, &[12345]),
            (3, Boolean, &[0, 1, 2, 3, 0x1234, 0xaaaa, u16::MAX]),
            (4, Arithmetic, &[0, u16::MAX]),
        ];

        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let runtime = LocalRuntime::new(generate_local_identities_orbit5(), local_seeds())
                .await
                .unwrap();
            let mut jobs = JoinSet::new();

            for session in runtime.sessions {
                jobs.spawn(async move {
                    let mut session = session.network_session;
                    let role = session.own_role();
                    // Set up PRFs ONCE and reuse the same session and mutable keys
                    // for every call. Resetting keys would hide synchronization bugs.
                    let mut threshold = setup_threshold_prf_keys(&mut session).await.unwrap();
                    let mut outputs = Vec::new();
                    for (dealer, share_type, values) in rounds {
                        let dealer = Role::new(dealer);
                        // All parties use equal batch sizes within each round;
                        // only the dealer supplies the actual values.
                        let inputs = values
                            .iter()
                            .map(|value| RingElement(if role == dealer { *value } else { 0 }))
                            .collect();
                        let shares = match share_type {
                            Arithmetic => {
                                dealer_rss5_arithmetic(
                                    &mut session,
                                    &mut threshold,
                                    dealer,
                                    inputs,
                                )
                                .await
                            }
                            Boolean => {
                                dealer_rss5_boolean(
                                    &mut session,
                                    &mut threshold,
                                    dealer,
                                    inputs,
                                )
                                .await
                            }
                        }
                        .unwrap();
                        assert_eq!(shares.len(), values.len());
                        outputs.push(shares);
                    }
                    (role, outputs)
                });
            }

            let results = jobs.join_all().await;
            for (index, (dealer, share_type, values)) in rounds.into_iter().enumerate() {
                let shares: Vec<_> = results
                    .iter()
                    .map(|(role, outputs)| (*role, outputs[index].clone()))
                    .collect();
                // Reconstruction checks that all replicated pieces still agree:
                // this verifies PRF synchronization as well as the plaintext result.
                assert_eq!(
                    rss5_reconstruction(&shares, share_type).unwrap(),
                    values.iter().copied().map(RingElement).collect::<Vec<_>>(),
                    "round {index}, dealer {dealer}"
                );
            }
        })
        .await
        .expect("RSS5 synchronization test timed out");
    }
}
