use crate::execution::session::NetworkSession;
use crate::protocol::{prf::ThresholdPrfKeys, rss5_ops::and_many};
use ampc_secret_sharing::shares::{
    ring_impl::RingElement,
    rss5::{RssShare, RSS5_SLOTS_HELD},
};
use eyre::{ensure, Result};

/// Reduce three Boolean-shared 16-bit summands to A and B with the same sum mod 2^16.
/// Preprocess each summand's batch with
/// [`transpose_rss5_u16`]
/// It produces 16 bit planes, LSB first: plane `j` holds shares of bit `j` across
/// comparisons, with each u64 component packing up to 64 comparisons.
/// Both outputs use the same layout.
/// All parties must agree on the batch layout and threshold PRF call order.
#[tracing::instrument(level = "trace", target = "mpc::network", skip_all)]
#[allow(clippy::type_complexity)]
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
        for ((&x, &y), &z) in inputs[0][bit]
            .iter()
            .zip(&inputs[1][bit])
            .zip(&inputs[2][bit])
        {
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

/// Return packed Boolean RSS5 shares of the MSB of A + B modulo 2^16.
/// Inputs A and B come from `full_adder_reduce`, which preserves the layout from
/// [`transpose_rss5_u16`](ampc_secret_sharing::shares::vecshare_bittranspose::transpose_rss5_u16).
/// Plane `j` contains shares of bit `j` across N comparisons: ceil(N / 64)
/// `RssShare<u64>` values, each component packing that bit from up to 64 comparisons.
/// Planes are LSB first; B's bit 0 must be zero. The output is one packed MSB plane.
#[tracing::instrument(level = "trace", target = "mpc::network", skip_all)]
pub async fn binary_add_2_get_msb(
    session: &mut NetworkSession,
    threshold: &mut ThresholdPrfKeys,
    a: &[Vec<RssShare<u64>>; 16],
    b: &[Vec<RssShare<u64>>; 16],
) -> Result<Vec<RssShare<u64>>> {
    let packed_len = a[0].len();
    ensure!(
        a.iter().chain(b).all(|plane| plane.len() == packed_len),
        "PPA input planes must have equal lengths"
    );
    if packed_len == 0 {
        return Ok(Vec::new());
    }

    // Bit 0 cannot generate a carry; bit 15 is used only in the final XOR.
    // p_j = A_j XOR B_j: bit j propagates a carry when exactly one input is 1.
    // The vectors p and g start at bit 1, so vector index 0 corresponds to bit 1.
    let mut p: Vec<Vec<RssShare<u64>>> = a[1..15]
        .iter()
        .zip(&b[1..15])
        .map(|(a, b)| transposed_pack_xor(a, b))
        .collect::<Result<_>>()?;
    // Each operand pair borrows the two planes to AND for one bit position.
    let operands: Vec<_> = a[1..15]
        .iter()
        .zip(&b[1..15])
        .map(|(a, b)| (a.as_slice(), b.as_slice()))
        .collect();
    // g_j = A_j AND B_j: both bits being 1 generates a carry into bit j + 1.
    // Compute these planes for bits 1..14 in one batched AND call.
    let mut g = transposed_pack_and(session, threshold, &operands).await?;

    // Blocks stay ordered from least to most significant: 14 -> 7 -> 4 -> 2 -> 1.
    while g.len() > 1 {
        let mut operands = Vec::new();
        for low in (0..g.len() - 1).step_by(2) {
            let high = low + 1;
            // G = g_high XOR (g_low AND p_high).
            operands.push((g[low].as_slice(), p[high].as_slice()));
            // The least-significant block never needs to propagate an incoming carry.
            if low > 0 {
                operands.push((p[low].as_slice(), p[high].as_slice()));
            }
        }
        // Batch both generation and propagation ANDs for this entire tree level.
        let mut products = transposed_pack_and(session, threshold, &operands)
            .await?
            .into_iter();
        let mut next_g = Vec::new();
        let mut next_p = Vec::new();
        for low in (0..g.len() - 1).step_by(2) {
            let high = low + 1;
            let carry = products
                .next()
                .ok_or_else(|| eyre::eyre!("missing carry AND"))?;
            // Finish each generation plane locally using its higher block's g.
            next_g.push(transposed_pack_xor(&g[high], &carry)?);
            // Keep an unused placeholder at index 0 so p and g use the same indices.
            next_p.push(if low == 0 {
                Vec::new()
            } else {
                products
                    .next()
                    .ok_or_else(|| eyre::eyre!("missing propagation AND"))?
            });
        }
        // An odd number of blocks leaves the highest block unpaired at this level.
        if g.len() % 2 == 1 {
            next_g.push(g.pop().unwrap());
            next_p.push(p.pop().unwrap());
        }
        g = next_g;
        p = next_p;
    }

    // g[0] is now the carry from bits 1..14 into the most-significant bit.
    let msb_p = transposed_pack_xor(&a[15], &b[15])?;
    transposed_pack_xor(&msb_p, &g[0])
}

/// XOR two equally sized planes locally, preserving the packed comparison order.
fn transposed_pack_xor(lhs: &[RssShare<u64>], rhs: &[RssShare<u64>]) -> Result<Vec<RssShare<u64>>> {
    ensure!(lhs.len() == rhs.len(), "XOR planes must have equal lengths");
    // The share operator XORs matching components without unpacking their bits.
    Ok(lhs.iter().zip(rhs).map(|(a, b)| *a ^ *b).collect())
}

// Pending helper: batch all plane pairs through and_many and preserve their order.
async fn transposed_pack_and(
    _session: &mut NetworkSession,
    _threshold: &mut ThresholdPrfKeys,
    _operands: &[(&[RssShare<u64>], &[RssShare<u64>])],
) -> Result<Vec<Vec<RssShare<u64>>>> {
    eyre::bail!("RSS5 transposed_pack_and is not implemented yet")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{
        local::{generate_local_identities_orbit5, LocalRuntime},
        player::Role,
        session::SessionHandles,
    };
    use crate::protocol::{
        ops::setup_threshold_prf_keys,
        rss5_ops::{dealer_three_party_additive_as_boolean_rss5_batches, FiveToThreeRoles},
        test_utils::rss5_boolean::reconstruct,
    };
    use aes_prng::AesRng;
    use ampc_secret_sharing::shares::vecshare_bittranspose::transpose_rss5_u16;
    use rand::{Rng, SeedableRng};
    use tokio::task::JoinSet;

    #[tokio::test]
    async fn test_rss5_full_adder_reduce() {
        const K: usize = 10000;
        check_rss5_full_adder_reduce(K).await;
    }

    async fn check_rss5_full_adder_reduce(k: usize) {
        // The dealer stage rejects empty batches; empty adder inputs are tested separately below.
        assert!(k > 0, "Stage 2 requires a nonempty batch");

        // Fix the seed so failures can be reproduced with the same plaintext inputs.
        let mut rng = AesRng::seed_from_u64(53);

        // Test a partial packed word, a full word, a word boundary, and the requested batch size.
        // Each comparison contains three random u16 summands [d1, d2, d3].
        let cases: Vec<Vec<[u16; 3]>> = [1, 64, 65, k]
            .into_iter()
            .map(|len| (0..len).map(|_| rng.gen()).collect())
            .collect();

        // Create five connected local party sessions with deterministic per-party RNG seeds.
        let runtime = LocalRuntime::new(
            generate_local_identities_orbit5(),
            (0..5).map(|i| [i; 16]).collect(),
        )
        .await
        .unwrap();

        // Collect the concurrent tasks that simulate the five independent parties.
        let mut jobs = JoinSet::new();

        // Each session runs only its own party's side of the protocol.
        for session in runtime.sessions {
            // Copy the test fixtures into this task; only its assigned summand enters the protocol.
            let cases = cases.clone();

            // Run parties concurrently
            jobs.spawn(async move {
                // Find this party's role
                let mut network = session.network_session;
                let own_role = network.own_role();

                // Establish shared PRF streams once and reuse their advancing state across cases.
                let mut threshold = setup_threshold_prf_keys(&mut network).await.unwrap();

                // Represent three summands, each with 16 bit planes and zero packed words.
                let empty: [[Vec<RssShare<u64>>; 16]; 3] =
                    std::array::from_fn(|_| std::array::from_fn(|_| Vec::new()));

                // Call the adder directly to exercise its empty-input behavior.
                let (a, b) = full_adder_reduce(&mut network, &mut threshold, &empty)
                    .await
                    .unwrap();

                // Both output summands must have empty vectors in every bit plane.
                assert!(a.iter().chain(&b).all(Vec::is_empty));

                // Start with empty planes, then make one plane inconsistent with the others.
                let mut malformed = empty;

                // Add one packed share only to bit 15 of the third summand.
                malformed[2][15].push(RssShare {
                    slots: [RingElement(0); RSS5_SLOTS_HELD],
                });

                // Mismatched plane lengths must be rejected before interactive AND work begins.
                assert!(full_adder_reduce(&mut network, &mut threshold, &malformed)
                    .await
                    .is_err());

                // Save this party's output shares for later reconstruction by the test
                let mut outputs = Vec::new();

                // Process every batch using the same session and PRF streams.
                for (case, values) in cases.into_iter().enumerate() {
                    // Rotate the Stage 2 dealers between batches, retaining PRF state.
                    let roles = FiveToThreeRoles {
                        // These three roles hold d1, d2, and d3 and act as dealers in Stage 2.
                        receivers: std::array::from_fn(|i| Role::new((i + case) % 5)),
                        // The remaining two roles hold no additive summand at this stage.
                        senders: std::array::from_fn(|i| Role::new((i + 3 + case) % 5)),
                    };

                    // Select this party's summand column; non-holders get an empty input vector.
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

                    // Share each of the three additive summands separately as Boolean RSS5.
                    // All five parties participate and receive their own local share of each batch.
                    let summands = dealer_three_party_additive_as_boolean_rss5_batches(
                        &mut network,
                        &mut threshold,
                        &roles,
                        additive,
                        values.len(),
                    )
                    .await
                    .unwrap();

                    // Stage 2 already stores all 16 shared bits in each u16 component.
                    // Transpose each batch into 16 planes, packing one bit from up to 64 comparisons.
                    let inputs = summands.map(|summand| transpose_rss5_u16(&summand));

                    // Reduce d1, d2, d3 to two shared words A and B with the same sum modulo 2^16.
                    let (a, b) = full_adder_reduce(&mut network, &mut threshold, &inputs)
                        .await
                        .unwrap();

                    // Every output plane needs one u64 share per group of up to 64 comparisons.
                    assert!(a
                        .iter()
                        .chain(&b)
                        .all(|plane| plane.len() == values.len().div_ceil(64)));

                    // B is the carry shifted left by one bit, so its lowest plane is locally zero.
                    assert!(b[0]
                        .iter()
                        .all(|share| share.slots.iter().all(|component| component.0 == 0)));

                    // Keep this case's two output sharings in the same order as the input cases.
                    outputs.push((a, b));
                }
                // Label the party's results so completion order does not affect reconstruction.
                (own_role.index(), outputs)
            });
        }
        // Wait for all parties, failing if communication stalls or the test takes too long.
        let mut results = tokio::time::timeout(std::time::Duration::from_secs(30), jobs.join_all())
            .await
            .expect("full-adder protocol timed out");
        // Put party views in role order, as required by the reconstruction helper.
        results.sort_by_key(|(role, _)| *role);

        // Reconstruct and check each batch against the original plaintext test fixtures.
        for (case, values) in cases.iter().enumerate() {
            // Combine all five party views of each A plane into plaintext packed u64 words.
            let a: [Vec<u64>; 16] = std::array::from_fn(|bit| {
                reconstruct(std::array::from_fn(|role| {
                    results[role].1[case].0[bit].as_slice()
                }))
            });
            // Reconstruct B in the same bit-plane layout as A.
            let b: [Vec<u64>; 16] = std::array::from_fn(|bit| {
                reconstruct(std::array::from_fn(|role| {
                    results[role].1[case].1[bit].as_slice()
                }))
            });

            // Include unused lanes in the final packed word to check zero padding as well.
            for comparison in 0..values.len().div_ceil(64) * 64 {
                // Recover one comparison's u16 from its lane across all 16 bit planes.
                let unpack = |planes: &[Vec<u64>; 16]| {
                    // Read planes from MSB to LSB, shifting the accumulated word before each bit.
                    planes.iter().rev().fold(0u16, |acc, plane| {
                        // comparison / 64 selects the packed word; comparison % 64 selects its lane.
                        (acc << 1) | ((plane[comparison / 64] >> (comparison % 64)) & 1) as u16
                    })
                };

                // Extract this comparison's reconstructed sum-without-carry word.
                let actual_a = unpack(&a);

                // Extract this comparison's reconstructed shifted-carry word.
                let actual_b = unpack(&b);

                // Padding lanes represent three zero inputs rather than a real comparison.
                let [x, y, z] = values.get(comparison).copied().unwrap_or([0; 3]);

                // A must contain the XOR of the three input bits at every position.
                assert_eq!(actual_a, x ^ y ^ z);
                // Use majority as an independent carry formula, shifted left with overflow discarded.
                assert_eq!(actual_b, ((x & y) | (x & z) | (y & z)).wrapping_shl(1));
                // Finally verify A + B equals the original arithmetic sum modulo 2^16.
                assert_eq!(
                    actual_a.wrapping_add(actual_b),
                    (u32::from(x) + u32::from(y) + u32::from(z)) as u16
                );
            }
        }
    }
}
