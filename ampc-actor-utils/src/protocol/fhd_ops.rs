use ampc_secret_sharing::{
    shares::{bit::Bit, DistanceShare, VecShare},
    RingElement, Share,
};
use eyre::{bail, Result};
use itertools::izip;
use tracing::instrument;

use crate::{
    execution::session::Session,
    network::value::NetworkValue,
    protocol::{
        binary::{bit_inject, extract_msb_batch, open_bin},
        ops::{conditionally_select_distance, DistancePair, B},
    },
};

/// Computes the `A` term of the threshold comparison based on the formula `A = ((1. - 2. * t) * B)`.
#[inline]
pub fn translate_threshold_a(t: f64) -> u32 {
    assert!(
        (0. ..=1.).contains(&t),
        "Threshold must be in the range [0, 1]"
    );
    ((1. - 2. * t) * (B as f64)) as u32
}

/// Compares the distance between two iris pairs to a threshold.
///
/// - Takes as input two code and mask dot products between two irises,
///   i.e., code_dist = <iris1.code, iris2.code> and mask_dist = <iris1.mask, iris2.mask>,
///   already lifted to 32 bits if they are originally 16-bit.
/// - Multiplies with threshold constants B = 2^16 and A = ((1. - 2. * threshold_ratio) * B).
/// - Compares mask_dist * A > code_dist * B.
/// - This corresponds to "distance > threshold", that is NOT match.
pub async fn fhd_greater_than_threshold(
    session: &mut Session,
    distances: &[DistanceShare<u32>],
    threshold_ratio: f64,
) -> Result<Vec<Share<Bit>>> {
    let a = translate_threshold_a(threshold_ratio);
    let diffs: Vec<Share<u32>> = distances
        .iter()
        .map(|d| {
            let x = d.mask_dot * a;
            let y = d.code_dot * B;
            y - x
        })
        .collect();

    extract_msb_batch(session, &diffs).await
}

/// Computes the cross product of distances shares represented as a fraction (code_dist, mask_dist).
/// The cross product is computed as (d2.code_dist * d1.mask_dist - d1.code_dist * d2.mask_dist) and the result is shared.
///
/// Assumes that the input shares are originally 16-bit and lifted to u32.
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn cross_mul(
    session: &mut Session,
    distances: &[DistancePair<u32>],
) -> Result<Vec<Share<u32>>> {
    let (prf_my_values, prf_prev_values) = session.prf.gen_rands_batch(distances.len());
    let res_a: Vec<RingElement<u32>> = izip!(
        distances.iter(),
        prf_my_values.0.into_iter(),
        prf_prev_values.0.into_iter()
    )
    .map(|(&(d1, d2), a, b)| {
        let zero_share = a - b; // equivalent to gen_zero_share()
        zero_share + d2.code_dot * d1.mask_dot - d1.code_dot * d2.mask_dot
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

/// For every pair of distance fraction shares (d1, d2), this computes the secret-shared bit d2 < d1.
///
/// The less-than operator is implemented in 2 steps:
///
/// 1. d2.code_dot * d1.mask_dot - d1.code_dot * d2.mask_dot is computed, which is a numerator of the fraction difference d2.code_dot / d2.mask_dot - d1.code_dot / d1.mask_dot.
/// 2. The most significant bit of the result is extracted.
async fn oblivious_cross_compare(
    session: &mut Session,
    distances: &[DistancePair<u32>],
) -> Result<Vec<Share<Bit>>> {
    let diff = cross_mul(session, distances).await?;
    extract_msb_batch(session, &diff).await
}

/// For every pair of distance fraction shares (d1, d2), this computes the secret-shared bit d2 < d1 and open it.
///
/// The less-than operator is implemented in 2 steps:
///
/// 1. d2.code_dot * d1.mask_dot - d1.code_dot * d2.mask_dot is computed, which is a numerator of the fraction difference d2.code_dot / d2.mask_dot - d1.code_dot / d1.mask_dot.
/// 2. The most significant bit of the result is extracted.
pub async fn cross_compare(
    session: &mut Session,
    distances: &[DistancePair<u32>],
) -> Result<Vec<bool>> {
    let bits = oblivious_cross_compare(session, distances).await?;
    let opened_b = open_bin(session, &bits).await?;
    opened_b.into_iter().map(|x| Ok(x.convert())).collect()
}

/// For every pair of distance fraction shares (d1, d2), this computes the secret-shared bit d2 < d1 and lift it to u32 shares.
///
/// The less-than operator is implemented in 2 steps:
///
/// 1. d2.code_dot * d1.mask_dot - d1.code_dot * d2.mask_dot is computed, which is a numerator of the fraction difference d2.code_dot / d2.mask_dot - d1.code_dot / d1.mask_dot.
/// 2. The most significant bit of the result is extracted.
///
/// Input values are assumed to be 16-bit shares that have been lifted to 32 bits.
pub async fn oblivious_cross_compare_lifted(
    session: &mut Session,
    distances: &[DistancePair<u32>],
) -> Result<Vec<Share<u32>>> {
    // compute the secret-shared bits d2 < d1
    let bits = oblivious_cross_compare(session, distances).await?;
    // inject bits to T shares
    Ok(bit_inject(session, VecShare { shares: bits })
        .await?
        .inner())
}

/// For every pair of distance fraction shares (d1, d2), this computes the bit d2 < d1 uses it to return the lower of the two distances.
///
/// Input values are assumed to be 16-bit shares that have been lifted to 32 bits.
pub async fn min_of_pair_batch(
    session: &mut Session,
    distances: &[DistancePair<u32>],
) -> Result<Vec<DistanceShare<u32>>> {
    // compute the secret-shared bits d2 < d1
    let bits = oblivious_cross_compare_lifted(session, distances).await?;

    conditionally_select_distance(session, distances, bits.as_slice()).await
}

#[cfg(test)]
mod tests {
    use crate::{
        constants::MATCH_THRESHOLD_RATIO,
        execution::{
            local::{generate_local_identities, LocalRuntime},
            session::SessionHandles,
        },
        protocol::{ops::batch_signed_lift_vec, test_utils::create_array_sharing},
    };

    use super::*;

    use aes_prng::AesRng;
    use eyre::{bail, Result};
    use rand::SeedableRng;
    use std::{collections::HashMap, sync::Arc};
    use tokio::{sync::Mutex, task::JoinSet};
    use tracing::instrument;

    #[instrument(level = "trace", target = "searcher::network", skip_all)]
    async fn open_single(session: &mut Session, x: Share<u32>) -> Result<RingElement<u32>> {
        let network = &mut session.network_session;
        network.send_next(NetworkValue::RingElement32(x.b)).await?;
        let missing_share = match network.receive_prev().await {
            Ok(NetworkValue::RingElement32(element)) => element,
            _ => bail!("Could not deserialize RingElement32"),
        };
        let (a, b) = x.get_ab();
        Ok(a + b + missing_share)
    }

    #[tokio::test]
    async fn test_replicated_cross_mul_lift() {
        let mut rng = AesRng::seed_from_u64(0_u64);
        let four_items = vec![1, 2, 3, 4];

        let four_shares = create_array_sharing(&mut rng, &four_items);

        let num_parties = 3;
        let identities = generate_local_identities();

        let four_share_map = HashMap::from([
            (identities[0].clone(), four_shares.p0),
            (identities[1].clone(), four_shares.p1),
            (identities[2].clone(), four_shares.p2),
        ]);

        let mut seeds = Vec::new();
        for i in 0..num_parties {
            let mut seed = [0_u8; 16];
            seed[0] = i;
            seeds.push(seed);
        }
        let runtime = LocalRuntime::new(identities.clone(), seeds.clone())
            .await
            .unwrap();

        let sessions: Vec<Arc<Mutex<Session>>> = runtime
            .sessions
            .into_iter()
            .map(|s| Arc::new(Mutex::new(s)))
            .collect();

        let mut jobs = JoinSet::new();
        for session in sessions {
            let session_lock = session.lock().await;
            let four_shares = four_share_map
                .get(&session_lock.own_identity())
                .unwrap()
                .clone();
            let session = session.clone();
            jobs.spawn(async move {
                let mut session = session.lock().await;
                let four_shares = batch_signed_lift_vec(&mut session, four_shares)
                    .await
                    .unwrap();
                let out_shared = cross_mul(
                    &mut session,
                    &[(
                        DistanceShare {
                            code_dot: four_shares[0],
                            mask_dot: four_shares[1],
                        },
                        DistanceShare {
                            code_dot: four_shares[2],
                            mask_dot: four_shares[3],
                        },
                    )],
                )
                .await
                .unwrap()[0];

                open_single(&mut session, out_shared).await.unwrap()
            });
        }
        // check first party output is equal to the expected result.
        let t = jobs.join_next().await.unwrap().unwrap();
        assert_eq!(t, RingElement(2));
    }

    /// Reference plaintext FHD: (mask_dot - code_dot) / (2 * mask_dot).
    /// `code_dot` here is the raw dot product which can be negative (represented
    /// as wrapping u16).
    fn reference_fhd_greater_than_threshold(cd: i64, md: i64) -> bool {
        if md == 0 {
            return false; // maximal distance -> treat as no match
        }
        let fhd = (md as f64 - cd as f64) / (2.0 * md as f64);
        fhd > MATCH_THRESHOLD_RATIO
    }

    #[tokio::test]
    async fn test_fhd_greater_than_threshold() {
        let mut rng = AesRng::seed_from_u64(44_u64);

        // Test with known values of `(code_dot, mask_dot)` and their expected
        // FHD threshold comparison result.
        // FHD = (md - cd) / (2*md), threshold = 0.375.
        // Expected boolean: FHD > threshold (i.e. NOT a match).
        let test_cases: Vec<(u16, u16, bool)> = vec![
            // cd=100, md=500 -> FHD = 400/1000 = 0.4 > 0.375 -> true
            (100, 500, true),
            // cd=200, md=500 -> FHD = 300/1000 = 0.3 < 0.375 -> false
            (200, 500, false),
            // cd=125, md=500 -> FHD = 375/1000 = 0.375, not strictly greater -> false
            (125, 500, false),
            // cd=124, md=500 -> FHD = 376/1000 = 0.376 > 0.375 -> true
            (124, 500, true),
            // Large mask dot, well below threshold
            (3000, 4000, false), // FHD = 1000/8000 = 0.125
            // Large mask dot, well above threshold
            (100, 4000, true), // FHD = 3900/8000 = 0.4875
            // Negative code_dot (wrapping u16): cd = -1 -> 0xFFFF
            (u16::MAX, 200, true), // cd=-1, md=200 -> FHD = 201/400 = 0.5025
            // cd=0, md=100 -> FHD = 100/200 = 0.5 > 0.375 -> true
            (0, 100, true),
            // cd very close to md -> small FHD
            (490, 500, false), // FHD = 10/1000 = 0.01
            // md = 0 -> treat as no match
            (10, 0, false),
        ];

        let flat_values: Vec<u16> = test_cases
            .iter()
            .flat_map(|(cd, md, _)| [*cd, *md])
            .collect();
        let flat_shares = create_array_sharing(&mut rng, &flat_values);

        let sessions = LocalRuntime::mock_sessions_with_channel().await.unwrap();
        let mut jobs = JoinSet::new();

        for (i, session) in sessions.into_iter().enumerate() {
            let session = session.clone();
            let shares_i = match i {
                0 => flat_shares.p0.clone(),
                1 => flat_shares.p1.clone(),
                2 => flat_shares.p2.clone(),
                _ => unreachable!(),
            };
            let n = test_cases.len();
            jobs.spawn(async move {
                let mut session = session.lock().await;
                let lifted = batch_signed_lift_vec(&mut session, shares_i).await.unwrap();
                let distances: Vec<DistanceShare<u32>> = (0..n)
                    .map(|j| DistanceShare::new(lifted[2 * j], lifted[2 * j + 1]))
                    .collect();
                let bits =
                    fhd_greater_than_threshold(&mut session, &distances, MATCH_THRESHOLD_RATIO)
                        .await
                        .unwrap();
                let opened = open_bin(&mut session, &bits).await.unwrap();
                opened
                    .into_iter()
                    .map(|x| x.convert())
                    .collect::<Vec<bool>>()
            });
        }

        let results: Vec<Vec<bool>> = jobs.join_all().await;

        // All parties should agree
        assert_eq!(results[0], results[1]);
        assert_eq!(results[1], results[2]);

        // Check against plaintext reference
        for (i, (cd, md, expected)) in test_cases.into_iter().enumerate() {
            let ref_cd = if cd > (1 << 15) {
                cd as i64 - (1 << 16)
            } else {
                cd as i64
            };
            let reference = reference_fhd_greater_than_threshold(ref_cd, md as i64);
            assert_eq!(
                results[0][i], reference,
                "Reference FHD threshold mismatch for (cd={}, md={}): got {}, expected {}",
                cd, md, results[0][i], reference
            );
            assert_eq!(
                results[0][i], expected,
                "FHD threshold mismatch for (cd={}, md={}): got {}, expected {}",
                cd, md, results[0][i], expected
            );
        }
    }
}
