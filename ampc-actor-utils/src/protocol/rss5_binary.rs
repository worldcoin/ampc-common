use crate::execution::session::NetworkSession;
use crate::protocol::{prf::ThresholdPrfKeys, rss5_ops::and_many};
use ampc_secret_sharing::shares::{
    ring_impl::RingElement,
    rss5::{RssShare, RSS5_SLOTS_HELD},
};
use eyre::{ensure, Result};

/// Reduce three Boolean-shared 16-bit summands to A and B with the same sum mod 2^16.
/// Inputs and outputs are 16 bit planes, LSB first, with 64 comparisons per element.
/// All parties must agree on the batch layout and threshold PRF call order.
#[tracing::instrument(level = "trace", target = "mpc::network", skip_all)]
pub async fn full_adder_reduce(
    session: &mut NetworkSession,
    threshold: &mut ThresholdPrfKeys,
    inputs: &[[Vec<RssShare<u64>>; 16]; 3],
) -> Result<([Vec<RssShare<u64>>; 16], [Vec<RssShare<u64>>; 16])> {
    let packed_len = inputs[0][0].len();
    // Check every plane before indexing or entering the interactive AND protocol.
    for (summand, planes) in inputs.iter().enumerate() {
        for (bit, plane) in planes.iter().enumerate() {
            ensure!(
                plane.len() == packed_len,
                "summand {summand}, bit {bit}: expected {packed_len} packed elements, got {}",
                plane.len()
            );
        }
    }

    let mut a: [Vec<RssShare<u64>>; 16] = std::array::from_fn(|_| Vec::with_capacity(packed_len));
    let mut and_lhs = Vec::with_capacity(15 * packed_len);
    let mut and_rhs = Vec::with_capacity(15 * packed_len);
    for (bit, plane) in a.iter_mut().enumerate() {
        for group in 0..packed_len {
            let x = inputs[0][bit][group];
            let y = inputs[1][bit][group];
            let z = inputs[2][bit][group];
            let x_xor_y = x ^ y;

            // A = x XOR y XOR z, computed locally for all 16 bits.
            plane.push(x_xor_y ^ z);
            if bit < 15 {
                // Carry = x XOR ((x XOR y) AND (z XOR x)).
                // Carry bit 15 is discarded by the later shift modulo 2^16.
                and_lhs.push(x_xor_y);
                and_rhs.push(z ^ x);
            }
        }
    }

    // Batch all 15 carry planes into one call. Its current dealer implementation
    // is sequential, so this is not yet a single communication round.
    let products = and_many(session, threshold, &and_lhs, &and_rhs).await?;
    ensure!(
        products.len() == and_lhs.len(),
        "unexpected AND output length"
    );

    let mut b: [Vec<RssShare<u64>>; 16] = std::array::from_fn(|_| {
        vec![
            RssShare {
                slots: [RingElement(0u64); RSS5_SLOTS_HELD],
            };
            packed_len
        ]
    });
    // B = 2 * carry: move planes up one position and leave B[0] zero.
    // Shifting the packed u64 values would mix different comparisons.
    for bit in 0..15 {
        for group in 0..packed_len {
            b[bit + 1][group] = inputs[0][bit][group] ^ products[bit * packed_len + group];
        }
    }

    Ok((a, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{
        local::{generate_local_identities_n, LocalRuntime},
        player::Role,
        session::SessionHandles,
    };
    use crate::protocol::{
        ops::setup_threshold_prf_keys,
        rss5_ops::{dealer_three_party_additive_as_boolean_rss5_batches, FiveToThreeRoles},
        test_utils::rss5_boolean::reconstruct,
    };
    use aes_prng::AesRng;
    use ampc_secret_sharing::shares::vecshare_bittranspose::{
        compact_rss5_u16_bits, transpose_rss5_u16,
    };
    use rand::{Rng, SeedableRng};
    use tokio::task::JoinSet;

    #[tokio::test]
    async fn test_rss5_full_adder_reduce() {
        const K: usize = 10000;
        check_rss5_full_adder_reduce(K).await;
    }

    async fn check_rss5_full_adder_reduce(k: usize) {
        assert!(k > 0, "Stage 2 requires a nonempty batch");
        let mut rng = AesRng::seed_from_u64(53);
        // Each comparison has three independently random u16 summands.
        let cases: Vec<Vec<[u16; 3]>> = [1, 64, 65, k]
            .into_iter()
            .map(|len| (0..len).map(|_| rng.gen()).collect())
            .collect();
        let runtime = LocalRuntime::new(
            generate_local_identities_n(5),
            (0..5).map(|i| [i; 16]).collect(),
        )
        .await
        .unwrap();
        let mut jobs = JoinSet::new();
        for session in runtime.sessions {
            let cases = cases.clone();
            jobs.spawn(async move {
                let mut network = session.network_session;
                let own_role = network.own_role();
                let mut threshold = setup_threshold_prf_keys(&mut network).await.unwrap();

                let empty: [[Vec<RssShare<u64>>; 16]; 3] =
                    std::array::from_fn(|_| std::array::from_fn(|_| Vec::new()));
                let (a, b) = full_adder_reduce(&mut network, &mut threshold, &empty)
                    .await
                    .unwrap();
                assert!(a.iter().chain(&b).all(Vec::is_empty));
                let mut malformed = empty;
                malformed[2][15].push(RssShare {
                    slots: [RingElement(0); RSS5_SLOTS_HELD],
                });
                assert!(full_adder_reduce(&mut network, &mut threshold, &malformed)
                    .await
                    .is_err());

                let mut outputs = Vec::new();
                for (case, values) in cases.into_iter().enumerate() {
                    // Rotate the Stage 2 dealers between batches, retaining PRF state.
                    let roles = FiveToThreeRoles {
                        receivers: std::array::from_fn(|i| Role::new((i + case) % 5)),
                        senders: std::array::from_fn(|i| Role::new((i + 3 + case) % 5)),
                    };
                    let additive = roles
                        .receivers
                        .iter()
                        .position(|role| *role == own_role)
                        .map(|summand| {
                            values
                                .iter()
                                .map(|value| RingElement(value[summand]))
                                .collect()
                        })
                        .unwrap_or_default();
                    let bits = dealer_three_party_additive_as_boolean_rss5_batches(
                        &mut network,
                        &mut threshold,
                        &roles,
                        additive,
                        values.len(),
                    )
                    .await
                    .unwrap();
                    let inputs = bits.map(|summand| {
                        transpose_rss5_u16(&compact_rss5_u16_bits(&summand).unwrap())
                    });
                    let (a, b) = full_adder_reduce(&mut network, &mut threshold, &inputs)
                        .await
                        .unwrap();
                    assert!(a
                        .iter()
                        .chain(&b)
                        .all(|plane| plane.len() == values.len().div_ceil(64)));
                    assert!(b[0]
                        .iter()
                        .all(|share| share.slots.iter().all(|component| component.0 == 0)));
                    outputs.push((a, b));
                }
                (own_role.index(), outputs)
            });
        }
        let mut results = tokio::time::timeout(std::time::Duration::from_secs(30), jobs.join_all())
            .await
            .expect("full-adder protocol timed out");
        results.sort_by_key(|(role, _)| *role);
        for (case, values) in cases.iter().enumerate() {
            let a: [Vec<u64>; 16] = std::array::from_fn(|bit| {
                reconstruct(std::array::from_fn(|role| {
                    results[role].1[case].0[bit].as_slice()
                }))
            });
            let b: [Vec<u64>; 16] = std::array::from_fn(|bit| {
                reconstruct(std::array::from_fn(|role| {
                    results[role].1[case].1[bit].as_slice()
                }))
            });
            for comparison in 0..values.len().div_ceil(64) * 64 {
                let unpack = |planes: &[Vec<u64>; 16]| {
                    planes.iter().rev().fold(0u16, |acc, plane| {
                        (acc << 1) | ((plane[comparison / 64] >> (comparison % 64)) & 1) as u16
                    })
                };
                let actual_a = unpack(&a);
                let actual_b = unpack(&b);
                let [x, y, z] = values.get(comparison).copied().unwrap_or([0; 3]);
                // Use the majority formula as an independent reference for carry.
                assert_eq!(actual_a, x ^ y ^ z);
                assert_eq!(actual_b, ((x & y) | (x & z) | (y & z)).wrapping_shl(1));
                assert_eq!(
                    actual_a.wrapping_add(actual_b),
                    (u32::from(x) + u32::from(y) + u32::from(z)) as u16
                );
            }
        }
    }
}
