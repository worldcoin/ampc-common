use ampc_secret_sharing::{
    shares::{bit::Bit, share::AdditiveShare},
    IntRing2k, ReplicatedShare, RingElement, Role,
};
use eyre::{bail, eyre, Error};
use num_traits::Zero;
use tracing::instrument;

use crate::{
    execution::session::{Session, SessionHandles},
    network::value::{NetworkInt, NetworkValue},
};
use crate::protocol::ops::open_ring;
// Precomputed offline randomness for extract_msb_rand: a share<T> element 'r', its per-bit boolean
// shares (bit7..bit0), and a shared random bit 'b_bit' embedded as a Share<T> share.
pub struct OfflineRandomShares<T: IntRing2k> {
    r: ReplicatedShare<T>,
    r_bits: [ReplicatedShare<Bit>; 8], // r_7, ..., r_0
    b_bit: ReplicatedShare<T>,
}

// sampling an instance of pre-generated randomness used in the protocol for T = u8
/// Returns the per-party view of precomputed randomness for extract_msb_rand.
/// Each party gets its replicated ABY3 share (a,b) of the same global values.
fn offline_shares_for_role(role: &impl Role) -> Result<OfflineRandomShares<u8>, Error> {
    use ampc_secret_sharing::RingElement;

    let elem_r0 = RingElement(57u8);
    let elem_r1 = RingElement(200u8);
    let elem_r2 = RingElement(181u8);

    let rb_0 = RingElement(34u8);
    let rb_1 = RingElement(79u8);
    let rb_2 = RingElement(144u8);

    // Boolean shares per bit (bit7..bit0), as additive mod-2 triplets (b0,b1,b2).
    let bit_triplets: [(u8, u8, u8); 8] = [
        (0, 1, 0), // bit7
        (1, 1, 0), // bit6
        (1, 0, 0), // bit5
        (0, 0, 1), // bit4
        (1, 0, 1), // bit3
        (0, 1, 0), // bit2
        (1, 1, 1), // bit1
        (0, 1, 1), // bit0
    ];

    match role.index() {
        // Party 0 holds (a0,a2) for every shared value.
        0 => Ok(OfflineRandomShares {
            r: ReplicatedShare::new(elem_r0, elem_r2),
            r_bits: bit_triplets.map(|(b0, _, b2)| {
                ReplicatedShare::new(
                    RingElement(Bit::new(b0 == 1)),
                    RingElement(Bit::new(b2 == 1)),
                )
            }),
            b_bit: ReplicatedShare::new(rb_0, rb_2),
        }),
        // Party 1 holds (a1,a0).
        1 => Ok(OfflineRandomShares {
            r: ReplicatedShare::new(elem_r1, elem_r0),
            r_bits: bit_triplets.map(|(_, b1, b0)| {
                ReplicatedShare::new(
                    RingElement(Bit::new(b1 == 1)),
                    RingElement(Bit::new(b0 == 1)),
                )
            }),
            b_bit: ReplicatedShare::new(rb_1, rb_0),
        }),
        // Party 2 holds (a2,a1).
        2 => Ok(OfflineRandomShares {
            r: ReplicatedShare::new(elem_r2, elem_r1),
            r_bits: bit_triplets.map(|(b2, b1, _)| {
                ReplicatedShare::new(
                    RingElement(Bit::new(b2 == 1)),
                    RingElement(Bit::new(b1 == 1)),
                )
            }),
            b_bit: ReplicatedShare::new(rb_2, rb_1),
        }),
        _ => bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]"),
    }
}

pub async fn extract_msb_rand<T: IntRing2k + NetworkInt>(
    session: &mut Session,
    x: ReplicatedShare<T>,
    offline: &OfflineRandomShares<T>,
) -> Result<ReplicatedShare<Bit>, Error> {

    let (r_self, r_prev) = offline.r.get_ab();
    let (b_self, b_prev) = offline.b_bit.get_ab();
    let (r_msb_self, r_msb_prev) = offline.r_bits[0].get_ab();

    // for testing purposes, printing the role and corresponding shares of OfflineRandomShares
    // println!(
    //     "extract_msb_rand role={:?} r=(a={:?}, b={:?}) b_bit=(a={:?}, b={:?}) r_bit7=(a={:?}, b={:?})",
    //     session.own_role(), r_self, r_prev, b_self, b_prev, msb_self, msb_prev
    // );

    // step 1: [r']_k = [r]_k - [r_bit]_1 ^ 2^{k - 1}

    // convert RingElement<Bit> -> Bit -> Bool -> (via from) T
    let v_t: T = T::from(r_msb_self.convert().convert());  
    // safely left-shift by T::K - 1 == bit width - 1 using wrapping_shl 
    let scaled_msb_self = RingElement(v_t.wrapping_shl((T::K - 1) as u32)); 

    let v_t: T = T::from(r_msb_prev.convert().convert());
    let scaled_msb_prev = RingElement(v_t.wrapping_shl((T::K - 1) as u32));

    // let scaled_msb = ReplicatedShare::new(scaled_msb_self, scaled_msb_prev);
    // println!(
    //     "msb scaled: self={:?} prev={:?}",
    //     scaled_msb.get_a(), scaled_msb.get_b()
    // );

    let r_prime_self: RingElement<T> = offline.r.get_a() - scaled_msb_self;
    let r_prime_prev: RingElement<T> = offline.r.get_b() - scaled_msb_prev;
    let r_prime = ReplicatedShare::new(r_prime_self, r_prime_prev);

    // println!(
    //     "computed r': self={:?} prev={:?}",
    //     r_prime.get_a(),
    //     r_prime.get_b()
    // );

    // step 2: c' = (x + r) mod 2^{k - 1}
    
    // mask input 'x:AdditiveShare<T>' with pre-generated random ring element 'r:AdditiveShare<T>'
    let c_share: ReplicatedShare<T> = x + offline.r; 
    let c: T = open_ring(session, std::slice::from_ref(&c_share)).await?[0];

    println!(
        "computed x, c: x_self={:?} x_prev={:?} c={:?}",
        x.get_a(), x.get_b(), c
    );

    //returning a dummy value for now
    let one = ReplicatedShare::from_const(Bit::new(true), session.own_role());
    Ok(one)
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

#[cfg(test)]
mod tests {
    use aes_prng::AesRng;
    use ampc_secret_sharing::shares::{bit::Bit, VecShare};
    use ampc_secret_sharing::{IntRing2k, ReplicatedShare, RingElement};
    use eyre::{bail, Result};
    use num_traits::Zero;
    use rand::{Rng, SeedableRng};
    use rand_distr::{Distribution, Standard};
    use tokio::task::JoinSet;

    use crate::execution::player::Role;
    use crate::protocol::msb_preprocessing::{open_additive_share_u8, rep_to_add2};
    use crate::protocol::test_utils::{
        create_single_sharing_additive, create_single_sharing_replicated,
    };
    use crate::{
        execution::{local::LocalRuntime, session::SessionHandles},
        protocol::{
            binary::open_bin,
            msb_preprocessing::{extract_msb_rand, offline_shares_for_role},
            test_utils::create_array_sharing,
        },
    };

    async fn test_extract_msb_rand_u8() -> Result<()> {
        let mut rng = AesRng::from_random_seed();
        let len = 1usize;

        // Random cleartext values + expected MSB bits
        let ints: Vec<u8> = (0..len).map(|_| rng.gen::<u8>()).collect();
        let expected: Vec<Bit> = ints
            .iter()
            .map(|x| {
                let msb = *x >> (u8::K - 1);
                if msb.is_zero() {
                    false.into()
                } else {
                    true.into()
                }
            })
            .collect();

        // Secret-share inputs across 3 parties
        let shares = create_array_sharing(&mut rng, &ints);

        let sessions = LocalRuntime::mock_sessions_with_channel().await?;
        let mut jobs = JoinSet::new();

        for (i, session) in sessions.into_iter().enumerate() {
            let session = session.clone();
            let shares_i = VecShare::new_vec(shares.of_party(i).clone());

            jobs.spawn(async move {
                let mut session = session.lock().await;

                // pick up the pre-generated randomness
                let offline = offline_shares_for_role(&session.own_role())?;

                // Run extract_msb_rand for each shared input
                let mut out = Vec::with_capacity(shares_i.len());
                for x in shares_i.shares().iter().cloned() {
                    out.push(extract_msb_rand::<u8>(&mut session, x, &offline).await?);
                }

                // Open result bits
                open_bin(&mut session, &out).await
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
}
