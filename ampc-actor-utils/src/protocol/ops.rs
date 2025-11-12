// Protocol operations for MPC
// This file contains only the non-iris-specific protocol operations

use crate::execution::session::SessionHandles;
use crate::network::value::NetworkInt;
use crate::protocol::binary::{extract_msb_u16_batch, open_bin};
use crate::{
    execution::session::{NetworkSession, Session},
    network::value::NetworkValue,
    protocol::prf::{Prf, PrfSeed},
};
use ampc_secret_sharing::shares::{ring_impl::RingElement, share::Share, IntRing2k};
use eyre::{bail, eyre, Result};
use tracing::instrument;

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

/// Compares the given distance to a threshold and reveal the bit "less than or equal".
pub async fn lte_threshold_and_open_u16(
    session: &mut Session,
    distances: &[Share<u16>],
) -> Result<Vec<bool>> {
    let bits = extract_msb_u16_batch(session, distances).await?;
    open_bin(session, &bits)
        .await
        .map(|v| v.into_iter().map(|x| x.convert()).collect())
}

/// Open ring shares to reveal the secret value
/// This is a helper function for opening shares
#[allow(dead_code)]
async fn open_ring<T: IntRing2k + crate::network::value::NetworkInt>(
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
