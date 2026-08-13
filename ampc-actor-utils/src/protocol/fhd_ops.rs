use ampc_secret_sharing::{
    shares::{bit::Bit, DistanceShare, VecShare},
    RingElement, Share,
};
use eyre::Result;
use tracing::instrument;

use crate::{
    execution::session::Session,
    protocol::{
        binary::{
            bit_inject, extract_anon_stats_msb_batch, extract_anon_stats_msb_batch_from_components,
            extract_msb_batch, extract_msb_batch_three_way, lift, mul_lift_2k_to_32, open_bin,
        },
        ops::{
            conditionally_select_distance, galois_ring_to_rep3_components, reshare_products,
            DistancePair, B,
        },
    },
};

pub type FhdDotSharePair = (Vec<Share<u16>>, Vec<Share<u16>>);

/// Refreshed Rep3 dot products retained by the fused exact-scan threshold
/// path. Scalar [`Share`] values are materialized only for public candidate
/// indices after the dense anonymous-statistics comparison has completed.
#[derive(Debug)]
pub struct FusedFhdDotShares {
    local: Vec<RingElement<u16>>,
    previous: Vec<RingElement<u16>>,
}

impl FusedFhdDotShares {
    /// Number of interleaved `(code, mask)` comparisons retained.
    pub fn len(&self) -> usize {
        self.local.len() / 2
    }

    pub fn is_empty(&self) -> bool {
        self.local.is_empty()
    }

    /// Reconstruct selected scalar Rep3 code and mask shares. Indices address
    /// comparisons, and output order (including duplicate indices) is
    /// preserved.
    pub fn select(&self, indices: &[usize]) -> Result<FhdDotSharePair> {
        let mut codes = Vec::with_capacity(indices.len());
        let mut masks = Vec::with_capacity(indices.len());
        for &index in indices {
            eyre::ensure!(
                index < self.len(),
                "dot-product index {index} is out of bounds for {} comparisons",
                self.len()
            );
            let offset = index * 2;
            codes.push(Share::new(self.local[offset], self.previous[offset]));
            masks.push(Share::new(
                self.local[offset + 1],
                self.previous[offset + 1],
            ));
        }
        Ok((codes, masks))
    }
}

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
#[instrument(level = "trace", target = "searcher::network", skip_all)]
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

/// Lift only the mask-dot shares needed by the direct FHD threshold protocol.
///
/// Unlike the generic distance path, exact linear scan never needs a lifted
/// distance or an oblivious minimum. The code dot is multiplied by `2^16`, so
/// it can be lifted locally by shifting; only the mask dot requires the MPC
/// carry-correcting lift.
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn lift_fhd_mask_dots(
    session: &mut Session,
    mask_dots: &[Share<u16>],
) -> Result<Vec<Share<u32>>> {
    Ok(lift(session, VecShare::new_vec(mask_dots.to_vec()))
        .await?
        .inner())
}

/// Compare raw replicated `u16` dot products to an FHD threshold.
///
/// This is the CPU counterpart of the GPU threshold-ring `lift_mul_sub`
/// protocol. All inputs are processed in one batch. `mask_dots` must have
/// already been lifted with [`lift_fhd_mask_dots`].
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn fhd_greater_than_threshold_pre_lifted_masks(
    session: &mut Session,
    code_dots: &[Share<u16>],
    mask_dots: &[Share<u32>],
    threshold_ratio: f64,
) -> Result<Vec<Share<Bit>>> {
    eyre::ensure!(
        code_dots.len() == mask_dots.len(),
        "code and mask dot batches must have equal lengths"
    );
    let a = translate_threshold_a(threshold_ratio);
    let diffs = code_dots
        .iter()
        .zip(mask_dots)
        .map(|(code_dot, mask_dot)| mul_lift_2k_to_32::<16>(code_dot) - *mask_dot * a)
        .collect::<Vec<_>>();
    extract_msb_batch_three_way(session, &diffs).await
}

/// Refresh local interleaved Galois-ring dot contributions into Rep3 and run
/// the fixed anonymous-statistics threshold without materializing the dense
/// scalar Rep3 batch.
///
/// The refresh is exactly the one used by `galois_ring_to_rep3`: it consumes
/// the same PRF values, sends one `VecRing16` to the next party, and receives
/// the previous party's refreshed components. The returned holder can later
/// materialize only the publicly selected candidate code and mask shares.
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn fhd_greater_than_anon_stats_from_galois(
    session: &mut Session,
    interleaved_dots: Vec<RingElement<u16>>,
) -> Result<(Vec<Share<Bit>>, FusedFhdDotShares)> {
    eyre::ensure!(
        interleaved_dots.len().is_multiple_of(2),
        "anonymous-threshold input must contain interleaved code/mask pairs"
    );
    let (local, previous) = galois_ring_to_rep3_components(session, interleaved_dots).await?;
    let bits = extract_anon_stats_msb_batch_from_components(session, &local, &previous).await?;
    Ok((bits, FusedFhdDotShares { local, previous }))
}

/// Dense exact-scan comparison for the fixed anonymous-statistics threshold.
///
/// This uses the 18-bit direct circuit rather than lifting every mask into the
/// 32-bit arithmetic ring. Strict thresholds remain on the generic path since
/// their public multiplier is not a power of two.
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn fhd_greater_than_anon_stats_threshold(
    session: &mut Session,
    code_dots: &[Share<u16>],
    mask_dots: &[Share<u16>],
) -> Result<Vec<Share<Bit>>> {
    extract_anon_stats_msb_batch(session, code_dots, mask_dots).await
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
    reshare_products(session, distances.len(), |i| {
        let (d1, d2) = distances[i];
        d2.code_dot * d1.mask_dot - d1.code_dot * d2.mask_dot
    })
    .await
}

/// For every pair of distance fraction shares (d1, d2), this computes the secret-shared bit d2 < d1.
///
/// The less-than operator is implemented in 2 steps:
///
/// 1. d2.code_dot * d1.mask_dot - d1.code_dot * d2.mask_dot is computed, which is a numerator of the fraction difference d2.code_dot / d2.mask_dot - d1.code_dot / d1.mask_dot.
/// 2. The most significant bit of the result is extracted.
#[instrument(level = "trace", target = "searcher::network", skip_all)]
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
#[instrument(level = "trace", target = "searcher::network", skip_all)]
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
#[instrument(level = "trace", target = "searcher::network", skip_all)]
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
#[instrument(level = "trace", target = "searcher::network", skip_all)]
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
    use crate::network::mpc::NetworkValue;
    use crate::{
        constants::MATCH_THRESHOLD_RATIO,
        execution::{
            local::{generate_local_identities, LocalRuntime},
            session::SessionHandles,
        },
        protocol::{
            ops::{batch_signed_lift_vec, galois_ring_to_rep3, open_ring},
            test_utils::create_array_sharing,
        },
    };

    use super::*;

    use aes_prng::AesRng;
    use ampc_secret_sharing::RingElement;
    use eyre::{bail, Result};
    use rand::{Rng, SeedableRng};
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
            let shares_i = flat_shares.of_party(i).clone();
            let n = test_cases.len();
            jobs.spawn(async move {
                let mut session = session.lock().await;
                let lifted = batch_signed_lift_vec(&mut session, shares_i.clone())
                    .await
                    .unwrap();
                let distances: Vec<DistanceShare<u32>> = (0..n)
                    .map(|j| DistanceShare::new(lifted[2 * j], lifted[2 * j + 1]))
                    .collect();
                let bits =
                    fhd_greater_than_threshold(&mut session, &distances, MATCH_THRESHOLD_RATIO)
                        .await
                        .unwrap();
                let generic = open_bin(&mut session, &bits)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|x| x.convert())
                    .collect::<Vec<bool>>();

                let code_dots = shares_i.iter().step_by(2).copied().collect::<Vec<_>>();
                let mask_dots = shares_i
                    .iter()
                    .skip(1)
                    .step_by(2)
                    .copied()
                    .collect::<Vec<_>>();
                let lifted_masks = lift_fhd_mask_dots(&mut session, &mask_dots).await.unwrap();
                let direct_bits = fhd_greater_than_threshold_pre_lifted_masks(
                    &mut session,
                    &code_dots,
                    &lifted_masks,
                    MATCH_THRESHOLD_RATIO,
                )
                .await
                .unwrap();
                let direct = open_bin(&mut session, &direct_bits)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|x| x.convert())
                    .collect::<Vec<bool>>();
                let anon_bits =
                    fhd_greater_than_anon_stats_threshold(&mut session, &code_dots, &mask_dots)
                        .await
                        .unwrap();
                let anon_direct = open_bin(&mut session, &anon_bits)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|x| x.convert())
                    .collect::<Vec<bool>>();
                (generic, direct, anon_direct)
            });
        }

        let results: Vec<(Vec<bool>, Vec<bool>, Vec<bool>)> = jobs.join_all().await;

        // All parties should agree
        assert_eq!(results[0], results[1]);
        assert_eq!(results[1], results[2]);
        assert_eq!(results[0].0, results[0].1);
        assert_eq!(results[0].0, results[0].2);

        // Check against plaintext reference
        for (i, (cd, md, expected)) in test_cases.into_iter().enumerate() {
            let ref_cd = if cd > (1 << 15) {
                cd as i64 - (1 << 16)
            } else {
                cd as i64
            };
            let reference = reference_fhd_greater_than_threshold(ref_cd, md as i64);
            assert_eq!(
                results[0].0[i], reference,
                "Reference FHD threshold mismatch for (cd={}, md={}): got {}, expected {}",
                cd, md, results[0].0[i], reference
            );
            assert_eq!(
                results[0].0[i], expected,
                "FHD threshold mismatch for (cd={}, md={}): got {}, expected {}",
                cd, md, results[0].0[i], expected
            );
        }
    }

    #[tokio::test]
    async fn direct_anon_threshold_matches_lifted_reference_randomized() {
        const N: usize = 4_097;
        let mut rng = AesRng::seed_from_u64(0x18_375_u64);
        let flat_values = (0..2 * N).map(|_| rng.gen::<u16>()).collect::<Vec<_>>();
        let flat_shares = create_array_sharing(&mut rng, &flat_values);
        let sessions = LocalRuntime::mock_sessions_with_channel().await.unwrap();
        let mut jobs = JoinSet::new();

        for (party, session) in sessions.into_iter().enumerate() {
            let session = session.clone();
            let shares = flat_shares.of_party(party).clone();
            jobs.spawn(async move {
                let mut session = session.lock().await;
                let codes = shares.iter().step_by(2).copied().collect::<Vec<_>>();
                let masks = shares
                    .iter()
                    .skip(1)
                    .step_by(2)
                    .copied()
                    .collect::<Vec<_>>();

                let lifted_masks = lift_fhd_mask_dots(&mut session, &masks).await.unwrap();
                let reference = fhd_greater_than_threshold_pre_lifted_masks(
                    &mut session,
                    &codes,
                    &lifted_masks,
                    0.375,
                )
                .await
                .unwrap();
                let reference = open_bin(&mut session, &reference).await.unwrap();

                let direct = fhd_greater_than_anon_stats_threshold(&mut session, &codes, &masks)
                    .await
                    .unwrap();
                let direct = open_bin(&mut session, &direct).await.unwrap();
                (reference, direct)
            });
        }

        let results = jobs.join_all().await;
        assert_eq!(results[0], results[1]);
        assert_eq!(results[1], results[2]);
        assert_eq!(results[0].0, results[0].1);
    }

    #[tokio::test]
    async fn fused_galois_anon_threshold_matches_dense_rep3_and_selects_candidates() {
        const N: usize = 257;
        let mut rng = AesRng::seed_from_u64(0xf053_d375_u64);
        let mut values = vec![
            125_u16,
            500_u16, // exact 0.375 threshold
            124,
            500, // immediately above threshold
            u16::MAX,
            200, // negative signed code dot
            10,
            0, // zero mask
        ];
        values.extend((values.len()..2 * N).map(|_| rng.gen::<u16>()));
        let additive_shares = create_array_sharing(&mut rng, &values);
        let selected_indices = vec![0, 1, 63, 64, N - 1, 64];
        let expected_selected_codes = selected_indices
            .iter()
            .map(|&index| values[index * 2])
            .collect::<Vec<_>>();
        let expected_selected_masks = selected_indices
            .iter()
            .map(|&index| values[index * 2 + 1])
            .collect::<Vec<_>>();

        let sessions = LocalRuntime::mock_sessions_with_channel().await.unwrap();
        let mut jobs = JoinSet::new();
        for (party, session) in sessions.into_iter().enumerate() {
            let session = session.clone();
            let local_dots = additive_shares
                .of_party(party)
                .iter()
                .map(|share| share.a)
                .collect::<Vec<_>>();
            let selected_indices = selected_indices.clone();
            jobs.spawn(async move {
                let mut session = session.lock().await;

                let dense = galois_ring_to_rep3(&mut session, local_dots.clone())
                    .await
                    .unwrap();
                let dense_codes = dense.iter().step_by(2).copied().collect::<Vec<_>>();
                let dense_masks = dense.iter().skip(1).step_by(2).copied().collect::<Vec<_>>();
                let dense_bits =
                    fhd_greater_than_anon_stats_threshold(&mut session, &dense_codes, &dense_masks)
                        .await
                        .unwrap();
                let dense_open = open_bin(&mut session, &dense_bits).await.unwrap();

                let (fused_bits, retained) =
                    fhd_greater_than_anon_stats_from_galois(&mut session, local_dots)
                        .await
                        .unwrap();
                assert_eq!(retained.len(), N);
                assert!(!retained.is_empty());
                let fused_open = open_bin(&mut session, &fused_bits).await.unwrap();

                let (selected_codes, selected_masks) = retained.select(&selected_indices).unwrap();
                let selected_codes = open_ring(&mut session, &selected_codes).await.unwrap();
                let selected_masks = open_ring(&mut session, &selected_masks).await.unwrap();
                (dense_open, fused_open, selected_codes, selected_masks)
            });
        }

        let results = jobs.join_all().await;
        assert_eq!(results[0], results[1]);
        assert_eq!(results[1], results[2]);
        assert_eq!(results[0].0, results[0].1);
        assert_eq!(results[0].2, expected_selected_codes);
        assert_eq!(results[0].3, expected_selected_masks);
    }
}
