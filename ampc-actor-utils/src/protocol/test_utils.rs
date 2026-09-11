use ampc_secret_sharing::{IntRing2k, RingElement, Share};
use num_traits::Zero;
use rand::{Rng, RngCore};
use rand_distr::{Distribution, Standard};

#[cfg(test)]
pub(crate) mod rss5_boolean {
    use ampc_secret_sharing::shares::rss5::{slot_pair, RssShare};
    use ampc_secret_sharing::RingElement;
    use rand::Rng;
    use std::collections::BTreeMap;

    pub fn share(rng: &mut impl Rng, values: &[u64]) -> [Vec<RssShare<u64>>; 5] {
        let mut result: [Vec<RssShare<u64>>; 5] = std::array::from_fn(|_| Vec::new());
        for &value in values {
            // Nine random components and one correction, indexed by excluded pair.
            let mut components = BTreeMap::new();
            let mut correction = value;
            for i in 0..5 {
                for j in i + 1..5 {
                    let component = if (i, j) == (3, 4) {
                        correction
                    } else {
                        rng.gen()
                    };
                    correction ^= component;
                    components.insert((i, j), component);
                }
            }
            for (role, shares) in result.iter_mut().enumerate() {
                shares.push(RssShare {
                    slots: std::array::from_fn(|slot| {
                        RingElement(components[&slot_pair(role, slot)])
                    }),
                });
            }
        }
        result
    }

    pub fn reconstruct(parties: [&[RssShare<u64>]; 5]) -> Vec<u64> {
        let len = parties[0].len();
        assert!(parties.iter().all(|shares| shares.len() == len));
        (0..len)
            .map(|index| {
                let mut components = BTreeMap::new();
                for (role, shares) in parties.iter().enumerate() {
                    for (slot, component) in shares[index].slots.iter().enumerate() {
                        let entry = components
                            .entry(slot_pair(role, slot))
                            .or_insert((component.0, 0));
                        assert_eq!(entry.0, component.0, "inconsistent RSS replicas");
                        entry.1 += 1;
                    }
                }
                assert_eq!(components.len(), 10);
                // XOR each distinct component once, after checking all three replicas.
                components.values().fold(0, |acc, &(value, count)| {
                    assert_eq!(count, 3);
                    acc ^ value
                })
            })
            .collect()
    }
}

fn create_single_sharing<R: RngCore, T: IntRing2k>(
    rng: &mut R,
    input: T,
) -> (Share<T>, Share<T>, Share<T>)
where
    Standard: Distribution<T>,
{
    let a = RingElement(rng.gen::<T>());
    let b = RingElement(rng.gen::<T>());
    let c = RingElement(input) - a - b;

    let share1 = Share::new(a, c);
    let share2 = Share::new(b, a);
    let share3 = Share::new(c, b);
    (share1, share2, share3)
}
pub struct LocalShares1D<T: IntRing2k> {
    pub p0: Vec<Share<T>>,
    pub p1: Vec<Share<T>>,
    pub p2: Vec<Share<T>>,
}

impl<T: IntRing2k> LocalShares1D<T> {
    pub fn of_party(&self, party_id: usize) -> &Vec<Share<T>> {
        match party_id {
            0 => &self.p0,
            1 => &self.p1,
            2 => &self.p2,
            _ => panic!("Invalid party id"),
        }
    }
}

pub fn create_array_sharing<R: RngCore, T: IntRing2k>(
    rng: &mut R,
    input: &Vec<T>,
) -> LocalShares1D<T>
where
    Standard: Distribution<T>,
{
    let mut player0 = Vec::new();
    let mut player1 = Vec::new();
    let mut player2 = Vec::new();

    for entry in input {
        let (a, b, c) = create_single_sharing(rng, *entry);
        player0.push(a);
        player1.push(b);
        player2.push(c);
    }
    LocalShares1D {
        p0: player0,
        p1: player1,
        p2: player2,
    }
}

/// Splits `input` into a 5-of-5 additive sharing: five uniformly random
/// ring elements that sum (mod 2^k) to `input`.
pub fn create_single_sharing_additive_5party<R: RngCore, T: IntRing2k>(
    rng: &mut R,
    input: T,
) -> [RingElement<T>; 5]
where
    Standard: Distribution<T>,
{
    let a = RingElement(rng.gen::<T>());
    let b = RingElement(rng.gen::<T>());
    let c = RingElement(rng.gen::<T>());
    let d = RingElement(rng.gen::<T>());
    let e = RingElement(input) - a - b - c - d;
    [a, b, c, d, e]
}

pub struct LocalShares1DAdditive5<T: IntRing2k> {
    shares: [Vec<RingElement<T>>; 5],
}

impl<T: IntRing2k> LocalShares1DAdditive5<T> {
    pub fn of_party(&self, party_id: usize) -> &Vec<RingElement<T>> {
        &self.shares[party_id]
    }
}

pub fn create_array_sharing_additive_5party<R: RngCore, T: IntRing2k>(
    rng: &mut R,
    input: &[T],
) -> LocalShares1DAdditive5<T>
where
    Standard: Distribution<T>,
{
    let mut shares: [Vec<RingElement<T>>; 5] = std::array::from_fn(|_| Vec::new());
    for entry in input {
        let split = create_single_sharing_additive_5party(rng, *entry);
        for (party_shares, share) in shares.iter_mut().zip(split) {
            party_shares.push(share);
        }
    }
    LocalShares1DAdditive5 { shares }
}

/// Locally reconstructs the plaintext value from an n-of-n additive sharing
/// held together (e.g. gathered in a test). Works for any party count, so
/// the same helper covers both a 3-of-3 and a 5-of-5 additive sharing.
pub fn reconstruct_additive_shares<T: IntRing2k>(shares: &[RingElement<T>]) -> T {
    shares
        .iter()
        .fold(RingElement::zero(), |acc, share| acc + *share)
        .convert()
}
