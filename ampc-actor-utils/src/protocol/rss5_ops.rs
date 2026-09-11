// Convert 5-of-5 and 3-of-3 additive shares.
// See `FiveToThreeRoles`/`reshare_five_to_three_party_additive` below.

use crate::execution::player::Role;
use crate::execution::session::{NetworkSession, SessionHandles};
use crate::network::mpc::NetworkInt;
use crate::protocol::dealer_5pc::{dealer_rss5_batch, dealer_rss5_boolean_batch, ShareType};
use crate::protocol::prf::{orbit5_roles, PairwisePrfKeys, ThresholdPrfKeys};
use ampc_secret_sharing::shares::ring_impl::RingElement;
use ampc_secret_sharing::shares::rss5::{RssShare, ORBIT5_PARTY_COUNT, RSS5_SLOTS_HELD};
use eyre::{bail, eyre, Result};
use num_traits::Zero;
use rand::Rng;
use rand_distr::{Distribution, Standard};
use std::collections::BTreeSet;
use tracing::instrument;

/// Role assignment for one round of 5-of-5 -> 3-of-3 additive resharing.
///
/// `senders[0]` masks its share for `receivers[0]` and `receivers[2]`, then
/// sends the correction to `receivers[1]`. `senders[1]` masks its share for
/// `receivers[0]` and `receivers[1]`, then sends the correction to
/// `receivers[2]`. This distributes the two network messages across two
/// receivers instead of sending both to one receiver.
#[derive(Clone, Copy, Debug)]
pub struct FiveToThreeRoles {
    pub receivers: [Role; 3],
    pub senders: [Role; 2],
}

impl FiveToThreeRoles {
    /// The canonical assignment: P0, P1, P2 as receivers, P3, P4 as
    /// senders.
    pub fn canonical() -> Self {
        Self {
            receivers: [Role::new(0), Role::new(1), Role::new(2)],
            senders: [Role::new(3), Role::new(4)],
        }
    }

    /// Returns the two receivers that derive masks with `sender` and the
    /// receiver to which `sender` sends its correction.
    fn sender_route(&self, sender: Role) -> Option<([Role; 2], Role)> {
        let [r0, r1, r2] = self.receivers;
        let [s0, s1] = self.senders;
        if sender == s0 {
            Some(([r0, r2], r1))
        } else if sender == s1 {
            Some(([r0, r1], r2))
        } else {
            None
        }
    }

    /// Validates that `receivers` and `senders` together are exactly the
    /// five distinct roles of the 5-party configuration.
    fn validate(&self) -> Result<()> {
        let given: BTreeSet<Role> = self
            .receivers
            .iter()
            .chain(self.senders.iter())
            .copied()
            .collect();
        if given.len() != ORBIT5_PARTY_COUNT {
            bail!(
                "FiveToThreeRoles must name {ORBIT5_PARTY_COUNT} distinct roles, got {}: {self:?}",
                given.len()
            );
        }
        let expected: BTreeSet<Role> = (0..ORBIT5_PARTY_COUNT).map(Role::new).collect();
        if given != expected {
            bail!("FiveToThreeRoles does not cover the 5-party role set: {self:?}");
        }
        Ok(())
    }
}

/// Converts a 5-of-5 additive sharing `d = d_0 + ... + d_4` into a 3-of-3
/// additive sharing held by `roles.receivers` using pairwise PRF keys.
///
/// This takes one communication round, with one batched message and two
/// PRF-derived masks per sender share.
///
/// Every one of the 5 parties must call this with its own 5-of-5 additive
/// share of each value in `shares` (all parties pass batches of the same
/// length). The 2 sender parties get back `Ok(vec![])` — they hold no
/// output share. The 3 receiver parties get back their 3-of-3 additive
/// share of each value, in the same order as `shares`.
///
/// The `pairwise` keys must be set up for the same session, and this party's
/// `own_role` must appear in
/// `roles.receivers` or `roles.senders`.

#[instrument(
    level = "trace",
    target = "mpc::network",
    fields(party = ?session.own_role()),
    skip_all
)]
pub async fn reshare_five_to_three_party_additive<T>(
    session: &mut NetworkSession,
    pairwise: &mut PairwisePrfKeys,
    roles: &FiveToThreeRoles,
    shares: Vec<RingElement<T>>,
) -> Result<Vec<RingElement<T>>>
where
    T: NetworkInt,
    Standard: Distribution<T>,
{
    debug_assert!(
        roles.validate().is_ok(),
        "invalid FiveToThreeRoles: {roles:?}"
    );
    if shares.is_empty() {
        bail!("reshare_5to3_party_additive: shares must not be empty");
    }

    let own_role = session.own_role();
    let [r0, r1, r2] = roles.receivers;
    let [s0, s1] = roles.senders;

    // prf_piece is a helper that given a pairwise instance (tied to own role)
    // and a role of other party + len of vector, returns a vector of random elements of that length
    let prf_piece =
        |pairwise: &mut PairwisePrfKeys, other: Role, len: usize| -> Result<Vec<RingElement<T>>> {
            let rng = pairwise
                .get_mut(other)
                .ok_or_else(|| eyre!("no pairwise PRF key held with {other:?}"))?;
            Ok((0..len).map(|_| rng.gen::<RingElement<T>>()).collect())
        };

    if let Some((masked_receivers, correction_receiver)) = roles.sender_route(own_role) {
        let mask_0 = prf_piece(pairwise, masked_receivers[0], shares.len())?;
        let mask_1 = prf_piece(pairwise, masked_receivers[1], shares.len())?;
        let correction: Vec<RingElement<T>> = shares
            .into_iter()
            .zip(mask_0)
            .zip(mask_1)
            .map(|((d, a), b)| d - a - b)
            .collect();
        session
            .send_to(T::new_network_vec(correction), &correction_receiver)
            .await?;
        Ok(vec![])
    } else if own_role == r0 {
        let mask_s0 = prf_piece(pairwise, s0, shares.len())?;
        let mask_s1 = prf_piece(pairwise, s1, shares.len())?;
        Ok(shares
            .into_iter()
            .zip(mask_s0)
            .zip(mask_s1)
            .map(|((d, a), b)| d + a + b)
            .collect())
    } else if own_role == r1 || own_role == r2 {
        let (masked_sender, correction_sender) = if own_role == r1 { (s1, s0) } else { (s0, s1) };
        let mask = prf_piece(pairwise, masked_sender, shares.len())?;
        let correction = T::into_vec(session.receive_from(&correction_sender).await?)?;
        if correction.len() != shares.len() {
            bail!(
                "reshare_five_to_three_party_additive: expected {} elements from correction sender {correction_sender:?}, got {}",
                shares.len(),
                correction.len()
            );
        }
        Ok(shares
            .into_iter()
            .zip(mask)
            .zip(correction)
            .map(|((d, a), b)| d + a + b)
            .collect())
    } else {
        bail!("own role {own_role:?} is not part of the given FiveToThreeRoles: {roles:?}")
    }
}

/// Convenience wrapper over [`reshare_five_to_three_party_additive`] hardcoding the
/// canonical P0, P1, P2 (recipients) / P3, P4 (resharers) role split.
pub async fn reshare_five_to_three_additive_canonical<T>(
    session: &mut NetworkSession,
    pairwise: &mut PairwisePrfKeys,
    shares: Vec<RingElement<T>>,
) -> Result<Vec<RingElement<T>>>
where
    T: NetworkInt,
    Standard: Distribution<T>,
{
    reshare_five_to_three_party_additive(session, pairwise, &FiveToThreeRoles::canonical(), shares)
        .await
}

/// Deals each component of a 3-of-3 additive sharing as boolean RSS5 shares.
///
/// All five parties call this after [`reshare_five_to_three_party_additive`].
/// The three roles in `roles.receivers` each deal their additive share with
/// [`dealer_rss5_boolean_batch`]. The three returned batches follow the order
/// of `roles.receivers`; batch `i` reconstructs to that receiver's additive
/// shares. These components must be combined with a boolean addition protocol
/// when a boolean sharing of their arithmetic sum is needed. `batch_len` is
/// required because the two resharer roles hold no additive output and
/// therefore cannot infer the batch length.
///
/// Receiver roles must pass their `batch_len` additive shares. Sender roles
/// must pass an empty vector. The returned outer vector follows the input
/// order; each inner vector contains `T::K` bit shares, least-significant bit
/// first.
pub async fn dealer_three_party_additive_as_boolean_rss5_batches<T>(
    session: &mut NetworkSession,
    threshold: &mut ThresholdPrfKeys,
    roles: &FiveToThreeRoles,
    additive_shares: Vec<RingElement<T>>,
    batch_len: usize,
) -> Result<[Vec<Vec<RssShare<T>>>; 3]>
where
    T: NetworkInt,
    Standard: Distribution<T>,
{
    roles.validate()?;
    if batch_len == 0 {
        bail!("3-party additive-to-boolean RSS5 batch must not be empty");
    }

    let own_role = session.own_role();
    if threshold.own_role() != own_role {
        bail!(
            "threshold PRF keys belong to {:?}, but the session belongs to {own_role:?}",
            threshold.own_role()
        );
    }
    if roles.receivers.contains(&own_role) {
        if additive_shares.len() != batch_len {
            bail!(
                "3-party additive receiver {own_role:?} provided {} shares, expected {batch_len}",
                additive_shares.len()
            );
        }
    } else if roles.senders.contains(&own_role) {
        if !additive_shares.is_empty() {
            bail!(
                "3-party additive sender {own_role:?} must provide no shares, got {}",
                additive_shares.len()
            );
        }
    } else {
        bail!("own role {own_role:?} is not part of the given FiveToThreeRoles: {roles:?}");
    }

    let zero_batch = || vec![RingElement::zero(); batch_len];
    let mut dealer_batches = Vec::with_capacity(roles.receivers.len());
    for dealer in roles.receivers {
        let dealer_input = if own_role == dealer {
            additive_shares.clone()
        } else {
            zero_batch()
        };
        let dealer_batch =
            dealer_rss5_boolean_batch(session, threshold, dealer, dealer_input).await?;
        if dealer_batch.len() != batch_len {
            bail!(
                "boolean dealer {dealer:?} produced {} values, expected {batch_len}",
                dealer_batch.len()
            );
        }
        dealer_batches.push(dealer_batch);
    }

    dealer_batches
        .try_into()
        .map_err(|_| eyre!("3-party additive-to-boolean RSS5 expected three dealer batches"))
}

/// Computes ANDs on batches of Boolean RSS shares, with 64 bits packed per element.
/// All five parties must use the same batch length and call order with matching
/// session-specific threshold PRF streams. Empty batches require no communication.
pub async fn and_many(
    session: &mut NetworkSession,
    threshold: &mut ThresholdPrfKeys,
    lhs: &[RssShare<u64>],
    rhs: &[RssShare<u64>],
) -> Result<Vec<RssShare<u64>>> {
    if lhs.len() != rhs.len() {
        bail!("AND input batches must have equal lengths");
    }
    if lhs.is_empty() {
        return Ok(Vec::new());
    }

    // Each party computes its assigned cross-terms for every packed element.
    let local: Vec<RingElement<u64>> = lhs.iter().zip(rhs).map(|(a, b)| a & b).collect();
    let own_role = session.own_role();
    let mut result = vec![
        RssShare {
            slots: [RingElement::zero(); RSS5_SLOTS_HELD],
        };
        lhs.len()
    ];

    // Complete one dealer call at a time, in a common order. This reuses the
    // existing API; it does not yet schedule all five dealers in one round.
    for dealer in orbit5_roles() {
        let dealer_input = if dealer == own_role {
            local.clone()
        } else {
            vec![RingElement::zero(); lhs.len()]
        };
        let shared =
            dealer_rss5_batch(session, threshold, dealer, dealer_input, ShareType::Boolean).await?;
        if shared.len() != lhs.len() {
            bail!(
                "Boolean dealer {dealer:?} returned {} AND contributions, expected {}",
                shared.len(),
                lhs.len()
            );
        }

        // XOR the five sharings to obtain RSS shares of the AND result.
        for (output, contribution) in result.iter_mut().zip(shared) {
            *output ^= contribution;
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::local::{generate_local_identities_n, LocalRuntime};
    use crate::protocol::dealer_5pc::reconstruct_boolean_batch;
    use crate::protocol::ops::{setup_pairwise_prf_keys, setup_threshold_prf_keys};
    use crate::protocol::test_utils::{
        create_array_sharing_additive_5party, reconstruct_additive_shares,
    };
    use aes_prng::AesRng;
    use rand::SeedableRng;
    use tokio::task::JoinSet;

    async fn test_reshare_5to3_additive(
        roles: FiveToThreeRoles,
        per_party_shares: [Vec<RingElement<u16>>; 5],
    ) -> Vec<(Role, Vec<RingElement<u16>>)> {
        let identities = generate_local_identities_n(ORBIT5_PARTY_COUNT);
        let mut seeds = Vec::new();

        // this test assigns deterministic seeds to each party
        // g3: potentially update the seeds to be from rng?
        for i in 0..ORBIT5_PARTY_COUNT {
            let mut seed = [0_u8; 16];
            seed[0] = i as u8;
            seeds.push(seed);
        }
        let runtime = LocalRuntime::new(identities, seeds).await.unwrap();

        let mut jobs = JoinSet::new();
        for (session, my_shares) in runtime.sessions.into_iter().zip(per_party_shares) {
            jobs.spawn(async move {
                let mut network_session = session.network_session;
                let mut pairwise = setup_pairwise_prf_keys(&mut network_session).await.unwrap();
                let own_role = network_session.own_role();
                let result = reshare_five_to_three_party_additive(
                    &mut network_session,
                    &mut pairwise,
                    &roles,
                    my_shares,
                )
                .await
                .unwrap();
                (own_role, result)
            });
        }
        jobs.join_all().await
    }

    fn create_additive_shares(rng: &mut AesRng, values: &[u16]) -> [Vec<RingElement<u16>>; 5] {
        let shares = create_array_sharing_additive_5party(rng, values);
        std::array::from_fn(|i| shares.of_party(i).clone())
    }

    /// Reconstructs the plaintext values from the 3 recipients' 3-of-3
    /// additive shares
    fn reconstruct_three_party_additive_shares(
        recipient_shares: &[(Role, Vec<RingElement<u16>>)],
    ) -> Vec<u16> {
        let recipient_columns: Vec<&Vec<RingElement<u16>>> = recipient_shares
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(_, v)| v)
            .collect();
        let len = recipient_columns.first().map(|v| v.len()).unwrap_or(0);
        (0..len)
            .map(|i| {
                let column: Vec<RingElement<u16>> =
                    recipient_columns.iter().map(|v| v[i]).collect();
                reconstruct_additive_shares(&column)
            })
            .collect()
    }

    #[tokio::test]
    async fn test_reshare_five_to_three_additive_canonical() {
        let mut rng = AesRng::seed_from_u64(48);
        let values: Vec<u16> = vec![42, 1000, 65535, 11, 0, 12345];
        let per_party_shares = create_additive_shares(&mut rng, &values);
        let roles = FiveToThreeRoles::canonical();

        let results = test_reshare_5to3_additive(roles, per_party_shares).await;

        for (role, shares) in &results {
            if roles.senders.contains(role) {
                assert!(
                    shares.is_empty(),
                    "sender {role:?} should hold no output share"
                );
            } else {
                assert_eq!(shares.len(), values.len());
            }
        }

        let reconstructed = reconstruct_three_party_additive_shares(&results);
        assert_eq!(reconstructed, values);
    }

    #[tokio::test]
    async fn test_reshare_five_to_three_additive_rotated_roles() {
        let mut rng = AesRng::seed_from_u64(49);
        let values: Vec<u16> = vec![7, 12345];
        let per_party_shares = create_additive_shares(&mut rng, &values);
        // Rotate: P2,P3,P4 as recipients, P0,P1 as resharers.
        let roles = FiveToThreeRoles {
            receivers: [Role::new(2), Role::new(3), Role::new(4)],
            senders: [Role::new(0), Role::new(1)],
        };

        let results = test_reshare_5to3_additive(roles, per_party_shares).await;
        let reconstructed = reconstruct_three_party_additive_shares(&results);
        assert_eq!(reconstructed, values);
    }

    #[tokio::test]
    async fn three_party_additive_reshare_as_boolean_rss5() {
        let mut rng = AesRng::seed_from_u64(50);
        // Five batch values are dealt by each of the three dealers.
        let values: Vec<u16> = vec![0, 1, 42, 0x1234, u16::MAX];
        let batch_len = values.len();
        let per_party_shares = create_additive_shares(&mut rng, &values);
        let roles = FiveToThreeRoles::canonical();
        let identities = generate_local_identities_n(ORBIT5_PARTY_COUNT);
        let seeds = (0..ORBIT5_PARTY_COUNT)
            .map(|index| {
                let mut seed = [0_u8; 16];
                seed[0] = index as u8;
                seed
            })
            .collect();
        let runtime = LocalRuntime::new(identities, seeds).await.unwrap();
        let mut jobs = JoinSet::new();

        for (session, shares) in runtime.sessions.into_iter().zip(per_party_shares) {
            jobs.spawn(async move {
                let mut network_session = session.network_session;
                let own_role = network_session.own_role();
                let mut pairwise = setup_pairwise_prf_keys(&mut network_session).await.unwrap();
                let additive_shares = reshare_five_to_three_party_additive(
                    &mut network_session,
                    &mut pairwise,
                    &roles,
                    shares,
                )
                .await
                .unwrap();
                let mut threshold = setup_threshold_prf_keys(&mut network_session)
                    .await
                    .unwrap();
                let expected_additive_shares = additive_shares.clone();
                let boolean_batches = dealer_three_party_additive_as_boolean_rss5_batches(
                    &mut network_session,
                    &mut threshold,
                    &roles,
                    additive_shares,
                    batch_len,
                )
                .await
                .unwrap();
                (own_role, expected_additive_shares, boolean_batches)
            });
        }

        let results = jobs.join_all().await;
        for (dealer_index, dealer) in roles.receivers.into_iter().enumerate() {
            let boolean_batch = results
                .iter()
                .map(|(role, _, batches)| (*role, batches[dealer_index].clone()))
                .collect::<Vec<_>>();
            let expected = results
                .iter()
                .find(|(role, _, _)| *role == dealer)
                .map(|(_, shares, _)| shares)
                .unwrap();
            assert_eq!(
                reconstruct_boolean_batch(&boolean_batch).unwrap(),
                *expected
            );
        }
    }

    #[test]
    fn test_correction_routes_are_distributed() {
        let roles = FiveToThreeRoles::canonical();
        let [r0, r1, r2] = roles.receivers;
        let [s0, s1] = roles.senders;

        assert_eq!(roles.sender_route(s0), Some(([r0, r2], r1)));
        assert_eq!(roles.sender_route(s1), Some(([r0, r1], r2)));
        assert_eq!(roles.sender_route(r0), None);
    }

    #[test]
    fn test_invalid_role_assignment_rejected() {
        // Overlapping recipient/resharer role.
        let roles = FiveToThreeRoles {
            receivers: [Role::new(0), Role::new(1), Role::new(2)],
            senders: [Role::new(2), Role::new(3)],
        };
        assert!(roles.validate().is_err());

        // Too few distinct roles (only covers 4 of 5).
        let roles = FiveToThreeRoles {
            receivers: [Role::new(0), Role::new(0), Role::new(1)],
            senders: [Role::new(2), Role::new(3)],
        };
        assert!(roles.validate().is_err());

        assert!(FiveToThreeRoles::canonical().validate().is_ok());
    }
}
