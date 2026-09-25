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
    batch_len: usize,
    dealer_inputs: Option<Vec<RingElement<T>>>,
    share_type: ShareType,
) -> Result<Vec<RssShare<T>>>
where
    T: NetworkInt,
    Standard: Distribution<T>,
{
    if batch_len == 0 {
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
    match (own_role == dealer, dealer_inputs.as_ref()) {
        (true, Some(inputs)) => {
            if inputs.len() != batch_len {
                bail!(
                    "RSS5 dealer {dealer:?} provided {} inputs, expected {batch_len}",
                    inputs.len()
                );
            }
        }
        (true, None) => bail!("RSS5 dealer {dealer:?} must provide inputs"),
        (false, Some(_)) => bail!("RSS5 non-dealer {own_role:?} must not provide inputs"),
        (false, None) => {}
    }
    let batch_size = batch_len;

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
        let correction = if let Some(mut correction) = dealer_inputs {
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
/// All parties must provide the same nonzero `batch_len`. The dealer must
/// provide `Some(inputs)` with that length; all other parties must pass `None`.
/// Correction recipients validate the received length. Other parties receive
/// no message, so this function cannot verify their batch lengths agree.
pub async fn dealer_rss5_arithmetic<T>(
    session: &mut NetworkSession,
    threshold: &mut ThresholdPrfKeys,
    dealer: Role,
    batch_len: usize,
    dealer_inputs: Option<Vec<RingElement<T>>>,
) -> Result<Vec<RssShare<T>>>
where
    T: NetworkInt,
    Standard: Distribution<T>,
{
    dealer_rss5_batch(
        session,
        threshold,
        dealer,
        batch_len,
        dealer_inputs,
        ShareType::Arithmetic,
    )
    .await
}

/// Deals a non-empty batch of boolean secrets in one network message.
///
/// All parties must provide the same nonzero `batch_len`. The dealer must
/// provide `Some(inputs)` with that length; all other parties must pass `None`.
/// Correction recipients validate the received length. Other parties receive
/// no message, so this function cannot verify their batch lengths agree.
pub async fn dealer_rss5_boolean<T>(
    session: &mut NetworkSession,
    threshold: &mut ThresholdPrfKeys,
    dealer: Role,
    batch_len: usize,
    dealer_inputs: Option<Vec<RingElement<T>>>,
) -> Result<Vec<RssShare<T>>>
where
    T: NetworkInt,
    Standard: Distribution<T>,
{
    dealer_rss5_batch(
        session,
        threshold,
        dealer,
        batch_len,
        dealer_inputs,
        ShareType::Boolean,
    )
    .await
}

/// Deals all five parties' packed Boolean contribution batches in one round.
///
/// Each party supplies its own nonempty `u64` batch, as produced by the local
/// AND operation. Output batch `i` is this party's RSS5 view of dealer `i`'s
/// inputs; callers can XOR the five batches to obtain shares of their XOR.
///
/// All parties must agree on batch length, element/lane ordering, and call
/// order, using threshold PRFs already established for this session. PRF
/// consumption and correction routing exactly match five sequential calls to
/// [`dealer_rss5_boolean`] on `u64` with dealers 0 through 4. In particular, each
/// party prepares its held components for every dealer, not just itself.
///
/// Both outgoing corrections are enqueued before any receive, so the exchange
/// has one logical communication round. This relies on sends not waiting for
/// peer receives, as supported by the local and TCP networking implementations.
/// After an error or cancellation during the exchange, discard the session and
/// PRF state rather than retrying with partially advanced streams.
#[tracing::instrument(level = "trace", target = "mpc::network", skip_all)]
pub async fn dealer_rss5_boolean_all(
    session: &mut NetworkSession,
    threshold: &mut ThresholdPrfKeys,
    local_inputs: Vec<RingElement<u64>>,
) -> Result<[Vec<RssShare<u64>>; ORBIT5_PARTY_COUNT]> {
    let batch_len = local_inputs.len();
    if batch_len == 0 {
        bail!(EMPTY_BATCH_ERROR);
    }
    let own_role = session.own_role();
    if own_role.index() >= ORBIT5_PARTY_COUNT
        || session.role_assignments.len() != ORBIT5_PARTY_COUNT
        || orbit5_roles()
            .iter()
            .any(|role| !session.role_assignments.contains_key(role))
    {
        bail!("all-party RSS5 dealing requires a session with roles 0 through 4");
    }
    if threshold.own_role() != own_role {
        bail!(
            "threshold PRF keys belong to {:?}, but the session belongs to {own_role:?}",
            threshold.own_role()
        );
    }

    let routes = orbit5_roles()
        .into_iter()
        .map(select_correction_pair_and_recipients)
        .collect::<Result<Vec<_>>>()?;
    let mut batches: [Vec<RssShare<u64>>; ORBIT5_PARTY_COUNT] = std::array::from_fn(|_| {
        vec![
            RssShare {
                slots: [RingElement::zero(); RSS5_SLOTS_HELD],
            };
            batch_len
        ]
    });
    let mut correction_slots = [None; ORBIT5_PARTY_COUNT];

    // Preserve the sequential dealer's draw order: dealer, slot, then element.
    // Zero and correction components consume no randomness. Every holder of a
    // PRF follows this schedule, even when it is not the current dealer.
    for dealer in orbit5_roles() {
        let dealer_index = dealer.index();
        let correction_pair = routes[dealer_index].0;
        for slot in 0..RSS5_SLOTS_HELD {
            let (i, j) = slot_pair(own_role.index(), slot);
            let (i, j) = (Role::new(i), Role::new(j));
            let pair = PartyPair::new(i, j);
            if pair == correction_pair {
                correction_slots[dealer_index] = Some(slot);
            } else if !pair.contains(dealer) {
                let rng = threshold.get_mut(i, j).ok_or_else(|| {
                    eyre!("role {own_role:?} has no threshold PRF key for pair ({i:?}, {j:?})")
                })?;
                for share in &mut batches[dealer_index] {
                    share.slots[slot] = rng.gen::<RingElement<u64>>();
                }
            }
        }
    }

    // The dealer holds all five masks for its own contribution. Its correction
    // slot is still zero, so XORing all six slots removes exactly those masks.
    let own_slot = correction_slots[own_role.index()]
        .ok_or_else(|| eyre!("dealer {own_role:?} does not hold its correction component"))?;
    let mut correction = local_inputs;
    for (value, share) in correction.iter_mut().zip(&mut batches[own_role.index()]) {
        for mask in &share.slots {
            *value ^= *mask;
        }
        share.slots[own_slot] = *value;
    }

    // All parties enqueue their own corrections before waiting for a peer.
    let recipients = routes[own_role.index()].1;
    for (recipient, payload) in recipients.into_iter().zip([correction.clone(), correction]) {
        session
            .send_to(u64::new_network_vec(payload), &recipient)
            .await
            .wrap_err_with(|| {
                format!("dealer {own_role:?} failed to send RSS5 correction to {recipient:?}")
            })?;
    }

    for dealer in orbit5_roles() {
        if dealer == own_role {
            continue;
        }
        if let Some(slot) = correction_slots[dealer.index()] {
            let received = session.receive_from(&dealer).await.wrap_err_with(|| {
                format!("{own_role:?} failed to receive RSS5 correction from {dealer:?}")
            })?;
            let correction = u64::into_vec(received).wrap_err_with(|| {
                format!("dealer {dealer:?} sent an invalid RSS5 correction payload")
            })?;
            if correction.len() != batch_len {
                bail!(
                    "expected {batch_len} RSS5 correction elements from {dealer:?}, got {}",
                    correction.len()
                );
            }
            for (share, value) in batches[dealer.index()].iter_mut().zip(correction) {
                share.slots[slot] = value;
            }
        }
    }
    Ok(batches)
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

    mod all_party {
        use super::*;
        use crate::execution::{
            player::Identity,
            session::{NetworkingImpl, SessionId},
        };
        use crate::network::mpc::{NetworkValue, Networking};
        use rand::{rngs::StdRng, SeedableRng};
        use std::sync::{Arc, Mutex};

        fn fixed_keys(role: Role) -> ThresholdPrfKeys {
            let seeds = PartyPair::excluding(role)
                .into_iter()
                .map(|pair| {
                    let (i, j) = pair.parties();
                    let mut seed = [0; 16];
                    seed[0] = i.index() as u8;
                    seed[1] = j.index() as u8;
                    (pair, seed)
                })
                .collect();
            ThresholdPrfKeys::from_seeds(role, seeds).unwrap()
        }

        fn assert_same_prf_state(actual: &mut ThresholdPrfKeys, expected: &mut ThresholdPrfKeys) {
            assert_eq!(actual.own_role(), expected.own_role());
            for pair in PartyPair::excluding(actual.own_role()) {
                let (i, j) = pair.parties();
                assert_eq!(
                    actual.get_mut(i, j).unwrap().gen::<[u64; 4]>(),
                    expected.get_mut(i, j).unwrap().gen::<[u64; 4]>(),
                    "PRF streams diverged for {pair:?}"
                );
            }
        }

        struct RecordedNetwork {
            inner: NetworkingImpl,
            events: Arc<Mutex<Vec<&'static str>>>,
        }

        #[async_trait::async_trait]
        impl Networking for RecordedNetwork {
            async fn send(&mut self, value: NetworkValue, receiver: &Identity) -> Result<()> {
                self.events.lock().unwrap().push("send");
                self.inner.send(value, receiver).await
            }

            async fn receive(&mut self, sender: &Identity) -> Result<NetworkValue> {
                self.events.lock().unwrap().push("receive");
                self.inner.receive(sender).await
            }
        }

        #[tokio::test]
        async fn matches_sequential_dealers_and_preserves_prf_state() {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                let mut rng = StdRng::seed_from_u64(54);
                let cases: Vec<[Vec<RingElement<u64>>; ORBIT5_PARTY_COUNT]> =
                    [1, 2, 63, 64, 65, 129]
                        .into_iter()
                        .map(|len| std::array::from_fn(|_| (0..len).map(|_| rng.gen()).collect()))
                        .collect();
                let runtime = LocalRuntime::new(generate_local_identities_orbit5(), local_seeds())
                    .await
                    .unwrap();
                let mut jobs = JoinSet::new();
                for session in runtime.sessions {
                    let cases = cases.clone();
                    jobs.spawn(async move {
                        let mut session = session.network_session;
                        let role = session.own_role();
                        let events = Arc::new(Mutex::new(Vec::new()));
                        session.networking = Box::new(RecordedNetwork {
                            inner: session.networking,
                            events: Arc::clone(&events),
                        });
                        let mut actual_prf = fixed_keys(role);
                        let mut reference_prf = fixed_keys(role);
                        let mut outputs = Vec::new();
                        for (case, values) in cases.iter().enumerate() {
                            // Interleave u16 dealing, as in Stage 2, without resetting PRFs.
                            let dealer = Role::new(case % ORBIT5_PARTY_COUNT);
                            let input =
                                || (dealer == role).then(|| vec![RingElement(0xa55a_u16); 3]);
                            let expected = dealer_rss5_boolean(
                                &mut session,
                                &mut reference_prf,
                                dealer,
                                3,
                                input(),
                            )
                            .await
                            .unwrap();
                            let actual = dealer_rss5_boolean(
                                &mut session,
                                &mut actual_prf,
                                dealer,
                                3,
                                input(),
                            )
                            .await
                            .unwrap();
                            assert_eq!(actual, expected);

                            let batch_len = values[role.index()].len();
                            let mut expected = Vec::new();
                            for dealer in orbit5_roles() {
                                expected.push(
                                    dealer_rss5_boolean(
                                        &mut session,
                                        &mut reference_prf,
                                        dealer,
                                        batch_len,
                                        (dealer == role).then(|| values[role.index()].clone()),
                                    )
                                    .await
                                    .unwrap(),
                                );
                            }
                            events.lock().unwrap().clear();
                            let actual = dealer_rss5_boolean_all(
                                &mut session,
                                &mut actual_prf,
                                values[role.index()].clone(),
                            )
                            .await
                            .unwrap();
                            assert_eq!(actual.as_slice(), expected.as_slice());
                            assert_same_prf_state(&mut actual_prf, &mut reference_prf);

                            // This catches regressions to awaiting a dealer's correction
                            // before this party has sent both of its own corrections.
                            let events = events.lock().unwrap();
                            assert_eq!(&events[..2], &["send", "send"]);
                            assert!(events[2..].iter().all(|event| *event == "receive"));
                            outputs.push(actual);
                        }
                        (role, outputs)
                    });
                }
                let results = jobs.join_all().await;
                for (case, values) in cases.iter().enumerate() {
                    for dealer in orbit5_roles() {
                        let views: Vec<_> = results
                            .iter()
                            .map(|(role, outputs)| (*role, outputs[case][dealer.index()].clone()))
                            .collect();
                        assert_eq!(
                            rss5_reconstruction(&views, ShareType::Boolean).unwrap(),
                            values[dealer.index()],
                        );
                    }
                }
            })
            .await
            .expect("all-party RSS5 dealing timed out");
        }

        struct FailingNetwork {
            fail_send: bool,
            response: Option<Result<NetworkValue>>,
        }

        #[async_trait::async_trait]
        impl Networking for FailingNetwork {
            async fn send(&mut self, _: NetworkValue, _: &Identity) -> Result<()> {
                if self.fail_send {
                    bail!("test send failure");
                }
                Ok(())
            }

            async fn receive(&mut self, _: &Identity) -> Result<NetworkValue> {
                self.response
                    .take()
                    .unwrap_or_else(|| Err(eyre!("unexpected receive")))
            }
        }

        fn test_session(role: Role, networking: NetworkingImpl) -> NetworkSession {
            NetworkSession {
                own_role: role,
                session_id: SessionId::from(0),
                role_assignments: Arc::new(
                    generate_local_identities_orbit5()
                        .into_iter()
                        .enumerate()
                        .map(|(i, id)| (Role::new(i), id))
                        .collect(),
                ),
                networking,
            }
        }

        #[tokio::test]
        async fn invalid_inputs_do_not_consume_prfs_or_communicate() {
            for (len, owner, role, replace_role, expected) in [
                (0, 0, 0, false, EMPTY_BATCH_ERROR),
                (1, 1, 0, false, "threshold PRF keys belong to"),
                (1, 0, 0, true, "requires a session with roles 0 through 4"),
                (1, 0, 5, false, "requires a session with roles 0 through 4"),
            ] {
                let events = Arc::new(Mutex::new(Vec::new()));
                let mut session = test_session(
                    Role::new(role),
                    Box::new(RecordedNetwork {
                        inner: Box::new(FailingNetwork {
                            fail_send: true,
                            response: None,
                        }),
                        events: Arc::clone(&events),
                    }),
                );
                if replace_role {
                    let roles = Arc::make_mut(&mut session.role_assignments);
                    let identity = roles.remove(&Role::new(4)).unwrap();
                    roles.insert(Role::new(5), identity);
                }
                let mut prf = fixed_keys(Role::new(owner));
                let mut untouched = fixed_keys(Role::new(owner));
                let error =
                    dealer_rss5_boolean_all(&mut session, &mut prf, vec![RingElement(0); len])
                        .await
                        .unwrap_err();
                assert!(
                    error.to_string().contains(expected),
                    "unexpected error: {error}"
                );
                assert!(events.lock().unwrap().is_empty());
                assert_same_prf_state(&mut prf, &mut untouched);
            }
        }

        #[tokio::test]
        async fn correction_errors_are_propagated() {
            for (fail_send, response, expected) in [
                (true, None, "test send failure"),
                (
                    false,
                    Some(Err(eyre!("test receive timeout"))),
                    "test receive timeout",
                ),
                (
                    false,
                    Some(Ok(NetworkValue::PrfKey([0; 16]))),
                    "invalid RSS5 correction payload",
                ),
                (
                    false,
                    Some(Ok(u64::new_network_vec(vec![]))),
                    "expected 1 RSS5 correction elements",
                ),
            ] {
                let role = Role::new(3); // Receives corrections under the existing routing.
                let mut session = test_session(
                    role,
                    Box::new(FailingNetwork {
                        fail_send,
                        response,
                    }),
                );
                let error = dealer_rss5_boolean_all(
                    &mut session,
                    &mut fixed_keys(role),
                    vec![RingElement(42)],
                )
                .await
                .unwrap_err();
                let report = format!("{error:#}");
                assert!(report.contains(expected), "unexpected error: {report}");
                assert!(report.contains("RSS5 correction"));
            }
        }
    }

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
                        0,
                        None,
                    )
                    .await
                    .unwrap_err()
                    .to_string(),
                    "RSS5 batch must not be empty"
                );
                let batch_len = batch.len();
                let scalar = (own_role == dealer).then(|| vec![RingElement(scalar)]);
                let batch =
                    (own_role == dealer).then(|| batch.into_iter().map(RingElement).collect());

                let single_shares =
                    dealer_rss5_arithmetic(&mut network_session, &mut threshold, dealer, 1, scalar)
                        .await
                        .unwrap();
                assert_eq!(single_shares.len(), 1);
                let batch_shares = dealer_rss5_arithmetic(
                    &mut network_session,
                    &mut threshold,
                    dealer,
                    batch_len,
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
                        0,
                        None,
                    )
                    .await
                    .unwrap_err()
                    .to_string(),
                    "RSS5 batch must not be empty"
                );
                let batch_len = batch.len();
                let scalar = (own_role == dealer).then(|| vec![RingElement(scalar)]);
                let batch =
                    (own_role == dealer).then(|| batch.into_iter().map(RingElement).collect());

                let single_shares =
                    dealer_rss5_boolean(&mut network_session, &mut threshold, dealer, 1, scalar)
                        .await
                        .unwrap();
                assert_eq!(single_shares.len(), 1);
                let batch_shares = dealer_rss5_boolean(
                    &mut network_session,
                    &mut threshold,
                    dealer,
                    batch_len,
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
    async fn invalid_dealer_inputs_rejected_before_communication_or_prf_use() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let runtime = LocalRuntime::new(generate_local_identities_orbit5(), local_seeds())
                .await
                .unwrap();
            let mut session = runtime.sessions.into_iter().next().unwrap().network_session;
            let own_role = session.own_role();
            let seeds = || {
                PartyPair::excluding(own_role)
                    .into_iter()
                    .map(|pair| (pair, [0; 16]))
                    .collect()
            };
            let mut threshold = ThresholdPrfKeys::from_seeds(own_role, seeds()).unwrap();
            let mut untouched = ThresholdPrfKeys::from_seeds(own_role, seeds()).unwrap();

            for share_type in [ShareType::Arithmetic, ShareType::Boolean] {
                let cases = [
                    (own_role, 0, None, "RSS5 batch must not be empty"),
                    (own_role, 1, None, "must provide inputs"),
                    (own_role, 1, Some(vec![]), "provided 0 inputs, expected 1"),
                    (
                        own_role,
                        1,
                        Some(vec![RingElement(7_u16); 2]),
                        "provided 2 inputs, expected 1",
                    ),
                    (
                        Role::new(1),
                        1,
                        Some(vec![RingElement(7_u16)]),
                        "must not provide inputs",
                    ),
                    (Role::new(1), 1, Some(vec![]), "must not provide inputs"),
                ];
                for (dealer, batch_len, inputs, expected) in cases {
                    let result = match share_type {
                        ShareType::Arithmetic => {
                            dealer_rss5_arithmetic(
                                &mut session,
                                &mut threshold,
                                dealer,
                                batch_len,
                                inputs,
                            )
                            .await
                        }
                        ShareType::Boolean => {
                            dealer_rss5_boolean(
                                &mut session,
                                &mut threshold,
                                dealer,
                                batch_len,
                                inputs,
                            )
                            .await
                        }
                    };
                    let error = result.unwrap_err().to_string();
                    assert!(error.contains(expected), "unexpected error: {error}");
                }
            }

            // Rejected calls must leave all shared random streams untouched.
            for i in 0..ORBIT5_PARTY_COUNT {
                for j in i + 1..ORBIT5_PARTY_COUNT {
                    if let Some(rng) = threshold.get_mut(Role::new(i), Role::new(j)) {
                        let expected = untouched.get_mut(Role::new(i), Role::new(j)).unwrap();
                        assert_eq!(rng.gen::<u64>(), expected.gen::<u64>());
                    }
                }
            }
        })
        .await
        .expect("invalid dealer inputs must fail without waiting for peers");
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
                        let inputs = (role == dealer)
                            .then(|| values.iter().copied().map(RingElement).collect());
                        let shares = match share_type {
                            Arithmetic => {
                                dealer_rss5_arithmetic(
                                    &mut session,
                                    &mut threshold,
                                    dealer,
                                    values.len(),
                                    inputs,
                                )
                                .await
                            }
                            Boolean => {
                                dealer_rss5_boolean(
                                    &mut session,
                                    &mut threshold,
                                    dealer,
                                    values.len(),
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
