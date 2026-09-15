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

#[derive(Clone, Copy)]
enum ShareType {
    Arithmetic,
    Boolean,
}

impl ShareType {
    fn remove_mask<T: IntRing2k>(
        self,
        value: RingElement<T>,
        mask: RingElement<T>,
    ) -> RingElement<T> {
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
fn correction_route(dealer: Role) -> Result<(PartyPair, [Role; 2])> {
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
    shares: Vec<RingElement<T>>,
    share_type: ShareType,
) -> Result<Vec<RssShare<T>>>
where
    T: NetworkInt,
    Standard: Distribution<T>,
{
    if shares.is_empty() {
        bail!("dealer RSS5 batch must not be empty");
    }

    let own_role = session.own_role();
    if threshold.own_role() != own_role {
        bail!(
            "threshold PRF keys belong to {:?}, but the session belongs to {own_role:?}",
            threshold.own_role()
        );
    }
    let (correction_pair, recipients) = correction_route(dealer)?;
    let len = shares.len();

    let mut slots: [Option<Vec<RingElement<T>>>; RSS5_SLOTS_HELD] = std::array::from_fn(|_| None);
    let mut correction_slot = None;

    for (slot, values) in slots.iter_mut().enumerate() {
        let (i, j) = slot_pair(own_role.index(), slot);
        let (party_i, party_j) = (Role::new(i), Role::new(j));
        let pair = PartyPair::new(party_i, party_j);

        if pair == correction_pair {
            correction_slot = Some(slot);
        } else if pair.contains(dealer) {
            *values = Some(vec![RingElement::zero(); len]);
        } else {
            let rng = threshold.get_mut(party_i, party_j).ok_or_else(|| {
                eyre!(
                    "role {own_role:?} has no threshold PRF key for pair ({party_i:?}, {party_j:?})"
                )
            })?;
            *values = Some((0..len).map(|_| rng.gen::<RingElement<T>>()).collect());
        }
    }

    if let Some(slot) = correction_slot {
        let correction = if own_role == dealer {
            let mut correction = shares;
            for masks in slots.iter().flatten() {
                for (value, mask) in correction.iter_mut().zip(masks) {
                    *value = share_type.remove_mask(*value, *mask);
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
            if correction.len() != len {
                bail!(
                    "expected {len} RSS5 correction elements from {dealer:?}, got {}",
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

    Ok((0..len)
        .map(|index| RssShare {
            slots: std::array::from_fn(|slot| slots[slot][index]),
        })
        .collect())
}

/// Deals one arithmetic secret.
///
/// All parties call this with the same `dealer`. Only the dealer's `share` is
/// read; other parties may pass zero.
pub async fn dealer_rss5_arithmetic<T>(
    session: &mut NetworkSession,
    threshold: &mut ThresholdPrfKeys,
    dealer: Role,
    share: RingElement<T>,
) -> Result<RssShare<T>>
where
    T: NetworkInt,
    Standard: Distribution<T>,
{
    dealer_rss5_batch(
        session,
        threshold,
        dealer,
        vec![share],
        ShareType::Arithmetic,
    )
    .await?
    .pop()
    .ok_or_else(|| eyre!("arithmetic RSS5 dealer returned no scalar share"))
}

/// Deals a non-empty batch of arithmetic secrets in one network message.
///
/// All parties must provide the same batch length. Only the dealer's shares
/// are read; other parties may provide a same-length vector of zeros.
pub async fn dealer_rss5_arithmetic_batch<T>(
    session: &mut NetworkSession,
    threshold: &mut ThresholdPrfKeys,
    dealer: Role,
    shares: Vec<RingElement<T>>,
) -> Result<Vec<RssShare<T>>>
where
    T: NetworkInt,
    Standard: Distribution<T>,
{
    dealer_rss5_batch(session, threshold, dealer, shares, ShareType::Arithmetic).await
}

/// Deals one boolean secret.
/// Equivalent to decomposing `secret` into bits and the dealer sharing each bit.
///
/// All parties call this with the same `dealer`. Only the dealer's `share` is
/// read; other parties may pass zero.
pub async fn dealer_rss5_boolean<T>(
    session: &mut NetworkSession,
    threshold: &mut ThresholdPrfKeys,
    dealer: Role,
    secret: RingElement<T>,
) -> Result<RssShare<T>>
where
    T: NetworkInt,
    Standard: Distribution<T>,
{
    dealer_rss5_batch(session, threshold, dealer, vec![secret], ShareType::Boolean)
        .await?
        .pop()
        .ok_or_else(|| eyre!("boolean RSS5 dealer returned no scalar share"))
}

/// Deals a non-empty batch of boolean secrets in one network message.
///
/// All parties must provide the same batch length. Only the dealer's shares
/// are read; other parties may provide a same-length vector of zeros.
pub async fn dealer_rss5_boolean_batch<T>(
    session: &mut NetworkSession,
    threshold: &mut ThresholdPrfKeys,
    dealer: Role,
    shares: Vec<RingElement<T>>,
) -> Result<Vec<RssShare<T>>>
where
    T: NetworkInt,
    Standard: Distribution<T>,
{
    dealer_rss5_batch(session, threshold, dealer, shares, ShareType::Boolean).await
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

fn reconstruct_rss5<T: IntRing2k>(
    shares: &[(Role, Vec<RssShare<T>>)],
    share_type: ShareType,
) -> Result<Vec<RingElement<T>>> {
    validate_reconstruction_roles(shares)?;

    let len = shares.first().map_or(0, |(_, batch)| batch.len());
    for (role, batch) in shares {
        if batch.len() != len {
            bail!(
                "RSS5 reconstruction expected {len} shares from {role:?}, got {}",
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

    let mut secrets = vec![RingElement::zero(); len];
    for values in pieces.values() {
        for (secret, value) in secrets.iter_mut().zip(values) {
            *secret = share_type.combine(*secret, *value);
        }
    }
    Ok(secrets)
}

/// Checks all five replicated views and reconstructs one arithmetic secret.
pub fn reconstruct_arithmetic<T: IntRing2k>(
    shares: &[(Role, RssShare<T>)],
) -> Result<RingElement<T>> {
    let batched: Vec<(Role, Vec<RssShare<T>>)> = shares
        .iter()
        .map(|(role, share)| (*role, vec![*share]))
        .collect();
    reconstruct_rss5(&batched, ShareType::Arithmetic)?
        .pop()
        .ok_or_else(|| eyre!("arithmetic RSS5 reconstruction returned no scalar"))
}

/// Checks all five replicated views and reconstructs an arithmetic batch.
pub fn reconstruct_arithmetic_batch<T: IntRing2k>(
    shares: &[(Role, Vec<RssShare<T>>)],
) -> Result<Vec<RingElement<T>>> {
    reconstruct_rss5(shares, ShareType::Arithmetic)
}

/// Checks all five replicated views and reconstructs one boolean secret.
pub fn reconstruct_boolean<T: IntRing2k>(shares: &[(Role, RssShare<T>)]) -> Result<RingElement<T>> {
    let batched: Vec<(Role, Vec<RssShare<T>>)> = shares
        .iter()
        .map(|(role, share)| (*role, vec![*share]))
        .collect();
    reconstruct_rss5(&batched, ShareType::Boolean)?
        .pop()
        .ok_or_else(|| eyre!("boolean RSS5 reconstruction returned no scalar"))
}

/// Checks all five replicated views and reconstructs a boolean batch.
pub fn reconstruct_boolean_batch<T: IntRing2k>(
    shares: &[(Role, Vec<RssShare<T>>)],
) -> Result<Vec<RingElement<T>>> {
    reconstruct_rss5(shares, ShareType::Boolean)
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
    ) -> (Vec<(Role, RssShare<u16>)>, Vec<(Role, Vec<RssShare<u16>>)>) {
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
                let scalar = RingElement(if own_role == dealer { scalar } else { 0 });
                let batch = batch
                    .into_iter()
                    .map(|value| RingElement(if own_role == dealer { value } else { 0 }))
                    .collect();

                let scalar_share =
                    dealer_rss5_arithmetic(&mut network_session, &mut threshold, dealer, scalar)
                        .await
                        .unwrap();
                let batch_shares = dealer_rss5_arithmetic_batch(
                    &mut network_session,
                    &mut threshold,
                    dealer,
                    batch,
                )
                .await
                .unwrap();
                (own_role, scalar_share, batch_shares)
            });
        }

        let results = jobs.join_all().await;
        let scalar = results
            .iter()
            .map(|(role, share, _)| (*role, *share))
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
    ) -> (Vec<(Role, BooleanShares)>, Vec<(Role, BooleanBatchShares)>) {
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
                let scalar = RingElement(if own_role == dealer { scalar } else { 0 });
                let batch = batch
                    .into_iter()
                    .map(|value| RingElement(if own_role == dealer { value } else { 0 }))
                    .collect();

                let scalar_shares =
                    dealer_rss5_boolean(&mut network_session, &mut threshold, dealer, scalar)
                        .await
                        .unwrap();
                let batch_shares =
                    dealer_rss5_boolean_batch(&mut network_session, &mut threshold, dealer, batch)
                        .await
                        .unwrap();
                (own_role, scalar_shares, batch_shares)
            });
        }

        let results = jobs.join_all().await;
        let scalar = results
            .iter()
            .map(|(role, shares, _)| (*role, *shares))
            .collect();
        let batch = results
            .into_iter()
            .map(|(role, _, shares)| (role, shares))
            .collect();
        (scalar, batch)
    }

    #[tokio::test]
    async fn arithmetic_scalar_and_batch_round_trip() {
        let expected_batch = vec![0, 1, u16::MAX, 12_345];
        let (scalar, batch) = run_arithmetic(Role::new(0), 42, expected_batch.clone()).await;

        assert_eq!(reconstruct_arithmetic(&scalar).unwrap(), RingElement(42));
        assert_eq!(
            reconstruct_arithmetic_batch(&batch).unwrap(),
            expected_batch
                .into_iter()
                .map(RingElement)
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn boolean_scalar_and_batch_round_trip() {
        let expected_batch = vec![0, 0xffff, 0x1234, 0xaaaa];
        let (scalar, batch) = run_boolean(Role::new(3), 0xa55a, expected_batch.clone()).await;

        assert_eq!(reconstruct_boolean(&scalar).unwrap(), RingElement(0xa55a));
        assert_eq!(
            reconstruct_boolean_batch(&batch).unwrap(),
            expected_batch
                .into_iter()
                .map(RingElement)
                .collect::<Vec<_>>()
        );
    }
}
