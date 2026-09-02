// 3-of-5 replicated secret sharing, the 5-party analogue of the 3-party `Share`
//
// A secret x is split into ten additive shares indexed by the ten unordered
// pairs of distinct parties, there are 4 choose 2 = 6 such pairs per party.
// Each party holds the 6 shares that do not include its own index.

use super::{int_ring::IntRing2k, ring_impl::RingElement};
use num_traits::Zero;
use std::ops::{Add, Mul, Sub};

/// Number of parties in the ORBIT5 (5-party) protocol configuration.
pub const ORBIT5_PARTY_COUNT: usize = 5;

/// Number of shares held by each party: `C(4, 2)`.
pub const RSS5_SLOTS_HELD: usize = 6;

/// The index pair of each locally-held slot, as offsets from the holder's own
/// role. Slot `i` of party `p` is the share indexed by the pair
/// `{p + SLOT_OFFSETS[i].0, p + SLOT_OFFSETS[i].1}` (mod [`ORBIT5_PARTY_COUNT`]).
/// In particular:
/// Role 0: [(1, 2), (1, 3), (1, 4), (2, 3), (2, 4), (3, 4)]
/// Role 1: [(2, 3), (2, 4), (0, 2), (3, 4), (0, 3), (0, 4)]
/// Role 2: [(3, 4), (0, 3), (1, 3), (0, 4), (1, 4), (0, 1)]
/// Role 3: [(0, 4), (1, 4), (2, 4), (0, 1), (0, 2), (1, 2)]
/// Role 4: [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)]
///
/// See Appedix C of "Multi-Party Replicated Secret Sharing over a Ring with
/// Applications to Privacy-Preserving Machine Learning" by Baccarini, Blanton and Yuan,
/// for the mapping below
pub const SLOT_OFFSETS: [(usize, usize); RSS5_SLOTS_HELD] =
    [(1, 2), (1, 3), (1, 4), (2, 3), (2, 4), (3, 4)];

/// The index pair of slot `slot` as held by party `role`, as an
/// ordered pair of absolute role indices.
pub fn slot_pair(role: usize, slot: usize) -> (usize, usize) {
    let (i, j) = SLOT_OFFSETS[slot];
    let (i, j) = (
        (role + i) % ORBIT5_PARTY_COUNT,
        (role + j) % ORBIT5_PARTY_COUNT,
    );
    if i <= j {
        (i, j)
    } else {
        (j, i)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// A 3-of-5 replicated share of a value in a ring.
/// The value is shared among five parties, with each party holding six shares.
/// The shares are represented as an array of [RingElement], where `slots[i]` is
/// the share indexed by the party pair `slot_pair(role, i)` and `role` is the
/// holding party.
///
/// Because the layout is relative to the holder, the same slot index denotes a
/// different share on each party: shares held by different parties must never
/// be combined.
pub struct RssShare<T: IntRing2k + Sized> {
    pub slots: [RingElement<T>; RSS5_SLOTS_HELD],
}

// Implementations of arithmetic operations for RssShare
impl<T: IntRing2k> Add<&Self> for RssShare<T> {
    type Output = Self;

    fn add(self, rhs: &Self) -> Self::Output {
        RssShare {
            slots: std::array::from_fn(|i| self.slots[i] + rhs.slots[i]),
        }
    }
}

impl<T: IntRing2k> Add<Self> for RssShare<T> {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        RssShare {
            slots: std::array::from_fn(|i| self.slots[i] + rhs.slots[i]),
        }
    }
}

impl<T: IntRing2k> Sub<Self> for RssShare<T> {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        RssShare {
            slots: std::array::from_fn(|i| self.slots[i] - rhs.slots[i]),
        }
    }
}

impl<T: IntRing2k> Sub<&Self> for RssShare<T> {
    type Output = Self;

    fn sub(self, rhs: &Self) -> Self::Output {
        RssShare {
            slots: std::array::from_fn(|i| self.slots[i] - rhs.slots[i]),
        }
    }
}

/// Multiplication by a public constant, known to every party.
impl<T: IntRing2k> Mul<T> for RssShare<T> {
    type Output = Self;

    fn mul(self, rhs: T) -> Self::Output {
        RssShare {
            slots: std::array::from_fn(|i| self.slots[i] * rhs),
        }
    }
}

/// Assignment of the 100 cross-terms of a product to the five parties.
///
/// `MUL_ASSIGN[i]` lists the right-hand slots that a party pairs with its own
/// left-hand slot `i`, so party `p` computes
///
/// ```
/// v_p = sum over i of ( a_i * sum over j in MUL_ASSIGN[i] of b_j )
/// ```
///
/// Slots are relative to `p`, so every party evaluates the same
/// expression. 20 terms per party cover all `10 * 10` ordered pairs of
/// share indices exactly once across the 5 parties, and every term assigned
/// to `p` uses only slots that `p` holds.
///
/// Taken from Appendix C of Baccarini, Blanton and Yuan.
const MUL_OPERAND_ASSIGN: [&[usize]; RSS5_SLOTS_HELD] = [
    &[0, 1, 2, 3, 4, 5], // a_0 * (b_0 + b_1 + b_2 + b_3 + b_4 + b_5)
    &[0, 1, 2, 3, 4, 5], // a_1 * (b_0 + b_1 + b_2 + b_3 + b_4 + b_5)
    &[1, 3],             // a_2 * (b_1 + b_3)
    &[0, 2],             // a_3 * (b_0 + b_2)
    &[0, 1],             // a_4 * (b_0 + b_1)
    &[0, 4],             // a_5 * (b_0 + b_4)
];

/// The local part of the multiplication
/// [`RssShare`] again.
impl<T: IntRing2k> Mul<Self> for &RssShare<T> {
    type Output = RingElement<T>;

    fn mul(self, rhs: Self) -> Self::Output {
        let mut acc = RingElement::zero();
        for (i, rhs_slots) in MUL_OPERAND_ASSIGN.iter().enumerate() {
            let mut rhs_sum = RingElement::zero();
            for &j in rhs_slots.iter() {
                rhs_sum += rhs.slots[j];
            }
            acc += self.slots[i] * rhs_sum;
        }
        acc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shares::bit::Bit;

    use aes_prng::AesRng;
    use rand::{Rng, SeedableRng};
    use rand_distr::{Distribution, Standard};
    use std::collections::HashMap;

    /// The ten unordered pairs of distinct parties, i.e. every share index.
    fn all_pairs() -> Vec<(usize, usize)> {
        (0..ORBIT5_PARTY_COUNT)
            .flat_map(|i| (i + 1..ORBIT5_PARTY_COUNT).map(move |j| (i, j)))
            .collect()
    }

    /// Deal a fresh sharing of `value` and return all five parties' views.
    fn get_shares<T: IntRing2k>(rng: &mut impl Rng, value: T) -> [RssShare<T>; ORBIT5_PARTY_COUNT]
    where
        Standard: Distribution<T>,
    {
        let pairs = all_pairs();
        let (last, rest) = pairs.split_last().unwrap();

        // Nine uniform shares, and a tenth that makes the ten sum to the secret.
        let mut slots: HashMap<(usize, usize), RingElement<T>> = rest
            .iter()
            .map(|pair| (*pair, RingElement(rng.gen::<T>())))
            .collect();
        let sum = slots
            .values()
            .fold(RingElement::zero(), |acc, share| acc + *share);
        slots.insert(*last, RingElement(value) - sum);

        // Hand each party the six shares whose index pair excludes it.
        std::array::from_fn(|role| RssShare {
            slots: std::array::from_fn(|slot| slots[&slot_pair(role, slot)]),
        })
    }

    /// Reconstruct a secret from all five views, checking on the way that the
    /// parties agree on the shares they replicate and that every one of the ten
    /// shares is held by somebody.
    fn reconstruct_shares<T: IntRing2k>(
        shares: &[RssShare<T>; ORBIT5_PARTY_COUNT],
    ) -> RingElement<T> {
        let mut slots: HashMap<(usize, usize), RingElement<T>> = HashMap::new();
        for (role, share) in shares.iter().enumerate() {
            for (slot, value) in share.slots.iter().enumerate() {
                let pair = slot_pair(role, slot);
                if let Some(previous) = slots.insert(pair, *value) {
                    assert_eq!(
                        previous, *value,
                        "parties disagree on the share for {pair:?}"
                    );
                }
            }
        }
        assert_eq!(slots.len(), all_pairs().len(), "not every share is held");

        slots
            .values()
            .fold(RingElement::zero(), |acc, share| acc + *share)
    }

    /// Reconstruct from the 5-of-5 additive sharing that the local halves of a
    /// multiplication produce.
    fn reconstruct_mul_shares<T: IntRing2k>(
        parts: [RingElement<T>; ORBIT5_PARTY_COUNT],
    ) -> RingElement<T> {
        parts
            .iter()
            .fold(RingElement::zero(), |acc, part| acc + *part)
    }

    /// [`MUL_OPERAND_ASSIGN`] must partition the cross-terms: each of the 100 ordered
    /// pairs of share indices assigned to exactly one party, and no party
    /// assigned a term over a share it does not hold.
    #[test]
    fn mul_assignment_partitions_all_cross_terms() {
        let mut assigned_to: HashMap<((usize, usize), (usize, usize)), usize> = HashMap::new();

        for role in 0..ORBIT5_PARTY_COUNT {
            for (i, rhs_slots) in MUL_OPERAND_ASSIGN.iter().enumerate() {
                for &j in rhs_slots.iter() {
                    let (lhs, rhs) = (slot_pair(role, i), slot_pair(role, j));

                    // A party can only multiply shares it holds, i.e. shares
                    // whose index pair excludes it.
                    for pair in [lhs, rhs] {
                        assert!(
                            pair.0 != role && pair.1 != role,
                            "party {role} is assigned a term over {pair:?}, which it does not hold"
                        );
                    }

                    if let Some(other) = assigned_to.insert((lhs, rhs), role) {
                        panic!("term {lhs:?} * {rhs:?} assigned to both {other} and {role}");
                    }
                }
            }
        }

        let pairs = all_pairs();
        assert_eq!(
            assigned_to.len(),
            pairs.len() * pairs.len(),
            "some cross-terms are unassigned"
        );
    }

    #[test]
    fn mul_matches_plain_multiplication() {
        mul_test::<Bit>();
        mul_test::<u16>();
        mul_test::<u64>();
    }
    fn mul_test<T: IntRing2k>()
    where
        Standard: Distribution<T>,
    {
        let mut rng = AesRng::from_entropy();

        for _ in 0..10000 {
            let a_t: T = rng.gen();
            let b_t: T = rng.gen();

            // split a_t and b_t into shares for all five parties
            let a = get_shares(&mut rng, a_t);
            let b = get_shares(&mut rng, b_t);

            // Dealing and reconstruction agree before any arithmetic happens.
            assert_eq!(reconstruct_shares(&a), RingElement(a_t));

            // Multiplication
            let expected_mul = RingElement(a_t.wrapping_mul(&b_t));
            let c: [RingElement<T>; ORBIT5_PARTY_COUNT] = std::array::from_fn(|i| &a[i] * &b[i]);
            assert_eq!(reconstruct_mul_shares(c), expected_mul);
        }
    }
}
