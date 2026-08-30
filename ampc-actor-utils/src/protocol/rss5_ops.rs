// 5-party protocol operations: converting between 5-of-5 and 3-of-3 additive
// sharings. See `FiveToThreeRoles`/`reshare_five_to_three_additive` below.

use crate::execution::player::Role;
use crate::execution::session::{NetworkSession, SessionHandles};
use crate::network::mpc::NetworkInt;
use crate::protocol::prf::{PairwisePrfKeys, FIVE_PARTY_COUNT};
use ampc_secret_sharing::shares::ring_impl::RingElement;
use eyre::{bail, eyre, Result};
use rand::Rng;
use rand_distr::{Distribution, Standard};
use std::collections::BTreeSet;
use tracing::instrument;

/// Role assignment for one round of 5-of-5 -> 3-of-3 additive resharing.
///
/// `recipients[0]` and `recipients[1]` derive their piece of each resharer's
/// share locally from a pairwise PRF key. `recipients[2]` is the one
/// recipient that instead receives its piece over the network from each
/// resharer. `resharers` are the two parties whose 5-of-5 additive share is
/// being split up and distributed to the recipients.
#[derive(Clone, Copy, Debug)]
pub struct FiveToThreeRoles {
    pub recipients: [Role; 3],
    pub resharers: [Role; 2],
}

impl FiveToThreeRoles {
    /// The canonical assignment: P0, P1, P2 as recipients, P3, P4 as
    /// resharers.
    pub fn canonical() -> Self {
        Self {
            recipients: [Role::new(0), Role::new(1), Role::new(2)],
            resharers: [Role::new(3), Role::new(4)],
        }
    }

    /// Validates that `recipients` and `resharers` together are exactly the
    /// five distinct roles of the 5-party configuration.
    fn validate(&self) -> Result<()> {
        let given: BTreeSet<Role> = self
            .recipients
            .iter()
            .chain(self.resharers.iter())
            .copied()
            .collect();
        if given.len() != FIVE_PARTY_COUNT as usize {
            bail!(
                "FiveToThreeRoles must name {FIVE_PARTY_COUNT} distinct roles, got {}: {self:?}",
                given.len()
            );
        }
        let expected: BTreeSet<Role> = (0..FIVE_PARTY_COUNT)
            .map(|i| Role::new(i as usize))
            .collect();
        if given != expected {
            bail!("FiveToThreeRoles does not cover the 5-party role set: {self:?}");
        }
        Ok(())
    }
}

/// Converts a 5-of-5 additive sharing `d = d_0 + ... + d_4` into a 3-of-3
/// additive sharing held by `roles.recipients`, using the PRF-optimized
/// variant of the "Stage 1" resharing protocol: 1 communication round, one
/// message per resharer (batched over all of `shares`), and 2 PRF draws per
/// non-collector party.
///
/// Every one of the 5 parties must call this with its own 5-of-5 additive
/// share of each value in `shares` (all parties pass batches of the same
/// length). The 2 resharer parties get back `Ok(vec![])` — they hold no
/// output share. The 3 recipient parties get back their 3-of-3 additive
/// share of each value, in the same order as `shares`.
///
/// `pairwise` must already be set up (see `setup_pairwise_prf_keys`) for the
/// same session, and this party's `own_role` must appear in
/// `roles.recipients` or `roles.resharers`.
#[instrument(
    level = "trace",
    target = "mpc::network",
    fields(party = ?session.own_role()),
    skip_all
)]
pub async fn reshare_five_to_three_additive<T>(
    session: &mut NetworkSession,
    pairwise: &mut PairwisePrfKeys,
    roles: &FiveToThreeRoles,
    shares: Vec<RingElement<T>>,
) -> Result<Vec<RingElement<T>>>
where
    T: NetworkInt,
    Standard: Distribution<T>,
{
    roles.validate()?;
    if shares.is_empty() {
        bail!("reshare_five_to_three_additive: shares must not be empty");
    }

    let own_role = session.own_role();
    let [r0, r1, r2] = roles.recipients;
    let [s0, s1] = roles.resharers;

    let prf_piece = |pairwise: &mut PairwisePrfKeys, other: Role, len: usize| -> Result<Vec<RingElement<T>>> {
        let rng = pairwise
            .get_mut(other)
            .ok_or_else(|| eyre!("no pairwise PRF key held with {other:?}"))?;
        Ok((0..len).map(|_| rng.gen::<RingElement<T>>()).collect())
    };

    if own_role == s0 || own_role == s1 {
        let piece_r0 = prf_piece(pairwise, r0, shares.len())?;
        let piece_r1 = prf_piece(pairwise, r1, shares.len())?;
        let leftover: Vec<RingElement<T>> = shares
            .into_iter()
            .zip(piece_r0)
            .zip(piece_r1)
            .map(|((d, a), b)| d - a - b)
            .collect();
        session
            .send_to(T::new_network_vec(leftover), &r2)
            .await?;
        Ok(vec![])
    } else if own_role == r0 || own_role == r1 {
        let piece_s0 = prf_piece(pairwise, s0, shares.len())?;
        let piece_s1 = prf_piece(pairwise, s1, shares.len())?;
        Ok(shares
            .into_iter()
            .zip(piece_s0)
            .zip(piece_s1)
            .map(|((d, a), b)| d + a + b)
            .collect())
    } else if own_role == r2 {
        let from_s0 = T::into_vec(session.receive_from(&s0).await?)?;
        let from_s1 = T::into_vec(session.receive_from(&s1).await?)?;
        if from_s0.len() != shares.len() || from_s1.len() != shares.len() {
            bail!(
                "reshare_five_to_three_additive: expected {} elements from each resharer, got {} and {}",
                shares.len(),
                from_s0.len(),
                from_s1.len()
            );
        }
        Ok(shares
            .into_iter()
            .zip(from_s0)
            .zip(from_s1)
            .map(|((d, a), b)| d + a + b)
            .collect())
    } else {
        bail!("own role {own_role:?} is not part of the given FiveToThreeRoles: {roles:?}")
    }
}

/// Convenience wrapper over [`reshare_five_to_three_additive`] hardcoding the
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
    reshare_five_to_three_additive(session, pairwise, &FiveToThreeRoles::canonical(), shares).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::local::{generate_local_identities_n, LocalRuntime};
    use crate::protocol::ops::setup_pairwise_prf_keys;
    use crate::protocol::test_utils::{create_array_sharing_additive_5party, reconstruct_additive_shares};
    use aes_prng::AesRng;
    use rand::SeedableRng;
    use tokio::task::JoinSet;

    async fn run_five_to_three(
        roles: FiveToThreeRoles,
        per_party_shares: [Vec<RingElement<u16>>; 5],
    ) -> Vec<(Role, Vec<RingElement<u16>>)> {
        let identities = generate_local_identities_n(FIVE_PARTY_COUNT as usize);
        let mut seeds = Vec::new();
        for i in 0..FIVE_PARTY_COUNT {
            let mut seed = [0_u8; 16];
            seed[0] = i;
            seeds.push(seed);
        }
        let runtime = LocalRuntime::new(identities, seeds).await.unwrap();

        let mut jobs = JoinSet::new();
        for (session, my_shares) in runtime.sessions.into_iter().zip(per_party_shares) {
            jobs.spawn(async move {
                let mut network_session = session.network_session;
                let mut pairwise = setup_pairwise_prf_keys(&mut network_session).await.unwrap();
                let own_role = network_session.own_role();
                let result = reshare_five_to_three_additive(
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

    fn split_into_5(rng: &mut AesRng, values: &[u16]) -> [Vec<RingElement<u16>>; 5] {
        let shares = create_array_sharing_additive_5party(rng, values);
        std::array::from_fn(|i| shares.of_party(i).clone())
    }

    /// Reconstructs the plaintext values from the 3 recipients' 3-of-3
    /// additive shares (the 2 resharers hold no output share).
    fn reconstruct(recipient_shares: &[(Role, Vec<RingElement<u16>>)]) -> Vec<u16> {
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
        let values: Vec<u16> = vec![42, 1000, 65535, 0];
        let per_party_shares = split_into_5(&mut rng, &values);
        let roles = FiveToThreeRoles::canonical();

        let results = run_five_to_three(roles, per_party_shares).await;

        for (role, shares) in &results {
            if roles.resharers.contains(role) {
                assert!(shares.is_empty(), "resharer {role:?} should hold no output share");
            } else {
                assert_eq!(shares.len(), values.len());
            }
        }

        let reconstructed = reconstruct(&results);
        assert_eq!(reconstructed, values);
    }

    #[tokio::test]
    async fn test_reshare_five_to_three_additive_rotated_roles() {
        let mut rng = AesRng::seed_from_u64(49);
        let values: Vec<u16> = vec![7, 12345];
        let per_party_shares = split_into_5(&mut rng, &values);
        // Rotate: P2,P3,P4 as recipients, P0,P1 as resharers.
        let roles = FiveToThreeRoles {
            recipients: [Role::new(2), Role::new(3), Role::new(4)],
            resharers: [Role::new(0), Role::new(1)],
        };

        let results = run_five_to_three(roles, per_party_shares).await;
        let reconstructed = reconstruct(&results);
        assert_eq!(reconstructed, values);
    }

    #[test]
    fn test_invalid_role_assignment_rejected() {
        // Overlapping recipient/resharer role.
        let roles = FiveToThreeRoles {
            recipients: [Role::new(0), Role::new(1), Role::new(2)],
            resharers: [Role::new(2), Role::new(3)],
        };
        assert!(roles.validate().is_err());

        // Too few distinct roles (only covers 4 of 5).
        let roles = FiveToThreeRoles {
            recipients: [Role::new(0), Role::new(0), Role::new(1)],
            resharers: [Role::new(2), Role::new(3)],
        };
        assert!(roles.validate().is_err());

        assert!(FiveToThreeRoles::canonical().validate().is_ok());
    }
}
