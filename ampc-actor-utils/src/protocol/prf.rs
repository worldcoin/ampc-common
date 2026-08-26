use crate::execution::player::Role;
use crate::protocol::shuffle::Permutation;
use ampc_secret_sharing::shares::{
    int_ring::IntRing2k,
    ring_impl::{RingElement, RingRandFillable, VecRingElement},
};
use eyre::{bail, Result};
use rand::{distributions::Standard, prelude::Distribution, Rng, SeedableRng};
use std::collections::{BTreeMap, BTreeSet};

/// Generate a uniformly random u32 in [0, modulus)
fn gen_u32_mod(rng: &mut PrfRng, modulus: u32) -> Result<u32> {
    if modulus == 0 {
        bail!("modulus must be non-zero");
    }
    let modulus_64 = modulus as u64;
    // Rejection sampling to avoid modulo bias
    // The rejection bound is the largest multiple of modulus that fits in u64 - 1.
    // In this case, the probability of rejection is 2^64 % modulus / 2^64 < 2^(-32).
    let rejection_bound = u64::MAX - (u64::MAX % modulus_64 + 1) % modulus_64;
    loop {
        let v = rng.gen::<u64>();
        if v <= rejection_bound {
            return Ok((v % modulus_64) as u32);
        }
    }
}

#[cfg(not(feature = "aes_rng_prf"))]
type PrfRng = rand_chacha::ChaCha8Rng;

#[cfg(feature = "aes_rng_prf")]
type PrfRng = aes_prng::AesRng;

pub type PrfSeed = [u8; 16];

#[derive(Clone, Debug)]
pub struct Prf {
    pub my_prf: PrfRng,
    pub prev_prf: PrfRng,
}

impl Default for Prf {
    fn default() -> Self {
        Self {
            my_prf: PrfRng::from_entropy(),
            prev_prf: PrfRng::from_entropy(),
        }
    }
}

impl Prf {
    pub fn new(my_key: PrfSeed, prev_key: PrfSeed) -> Self {
        Self {
            my_prf: seed_to_rng(my_key),
            prev_prf: seed_to_rng(prev_key),
        }
    }

    #[cfg(not(feature = "aes_rng_prf"))]
    fn expand_seed(seed: PrfSeed) -> [u8; 32] {
        use blake3::Hasher;
        let mut h = Hasher::new();
        h.update(&seed);
        let digest = h.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(digest.as_bytes());
        out
    }

    #[inline(always)]
    pub fn get_my_prf(&mut self) -> &mut PrfRng {
        &mut self.my_prf
    }

    #[inline(always)]
    pub fn get_prev_prf(&mut self) -> &mut PrfRng {
        &mut self.prev_prf
    }

    pub fn gen_seed() -> PrfSeed {
        let mut rng = PrfRng::from_entropy();
        rng.gen::<PrfSeed>()
    }

    pub fn gen_rands<T>(&mut self) -> (T, T)
    where
        Standard: Distribution<T>,
    {
        let a = self.my_prf.gen::<T>();
        let b = self.prev_prf.gen::<T>();
        (a, b)
    }

    #[inline(always)]
    pub fn gen_rands_mine<T>(&mut self, len: usize) -> VecRingElement<T>
    where
        T: RingRandFillable,
    {
        let mut mine = VecRingElement(vec![RingElement::<T>::default(); len]);
        self.get_my_prf().fill(&mut mine);
        mine
    }

    #[inline(always)]
    pub fn gen_rands_prev<T>(&mut self, len: usize) -> VecRingElement<T>
    where
        T: RingRandFillable,
    {
        let mut prev = VecRingElement(vec![RingElement::<T>::default(); len]);
        self.get_prev_prf().fill(&mut prev);
        prev
    }

    // returns the ring elements corresponding to (mine, prev). can be used to create zero shares (mine - prev) or binary shares (mine ^ prev)
    #[inline(always)]
    pub fn gen_rands_batch<T>(&mut self, len: usize) -> (VecRingElement<T>, VecRingElement<T>)
    where
        T: RingRandFillable,
    {
        let mine = self.gen_rands_mine(len);
        let prev = self.gen_rands_prev(len);
        (mine, prev)
    }

    pub fn gen_zero_share<T: IntRing2k>(&mut self) -> RingElement<T>
    where
        Standard: Distribution<T>,
    {
        let (a, b) = self.gen_rands::<RingElement<T>>();
        a - b
    }

    pub fn gen_binary_zero_share<T: IntRing2k>(&mut self) -> RingElement<T>
    where
        Standard: Distribution<T>,
    {
        let (a, b) = self.gen_rands::<RingElement<T>>();
        a ^ b
    }

    // Generates shared random u32 in [0, modulus)
    fn gen_u32_mod(&mut self, modulus: u32) -> Result<(u32, u32)> {
        let a = gen_u32_mod(&mut self.my_prf, modulus)?;
        let b = gen_u32_mod(&mut self.prev_prf, modulus)?;
        Ok((a, b))
    }

    pub fn gen_permutation(&mut self, size: u32) -> Result<Permutation> {
        let mut perm_a: Vec<u32> = (0..size).collect();
        let mut perm_b: Vec<u32> = (0..size).collect();
        for i in 1..size {
            let (j_a, j_b) = self.gen_u32_mod(i + 1)?;
            perm_a.swap(i as usize, j_a as usize);
            perm_b.swap(i as usize, j_b as usize);
        }
        Ok((perm_a, perm_b))
    }
}

#[inline]
fn seed_to_rng(seed: PrfSeed) -> PrfRng {
    #[cfg(not(feature = "aes_rng_prf"))]
    {
        PrfRng::from_seed(Prf::expand_seed(seed))
    }
    #[cfg(feature = "aes_rng_prf")]
    {
        PrfRng::from_seed(seed)
    }
}

/// Number of parties in the 5-party protocol configuration used by
/// [`ThresholdPrfKeys`] and [`PairwisePrfKeys`].
pub const FIVE_PARTY_COUNT: u8 = 5;

fn five_party_roles() -> [Role; FIVE_PARTY_COUNT as usize] {
    std::array::from_fn(Role::new)
}

/// An unordered, canonically-ordered pair of two distinct parties.
///
/// `PartyPair::new(a, b) == PartyPair::new(b, a)`, so it can be used as a
/// map key representing the pair `{a, b}` regardless of which order the two
/// roles are known in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PartyPair(Role, Role);

impl PartyPair {
    pub fn new(a: Role, b: Role) -> Self {
        assert_ne!(a, b, "a PartyPair requires two distinct roles");
        if a.index() <= b.index() {
            Self(a, b)
        } else {
            Self(b, a)
        }
    }

    pub fn contains(&self, role: Role) -> bool {
        self.0 == role || self.1 == role
    }

    pub fn parties(&self) -> (Role, Role) {
        (self.0, self.1)
    }

    /// All unordered pairs among the five parties that do not contain
    /// `own_role`. There are `C(4, 2) = 6` of them.
    pub fn excluding(own_role: Role) -> Vec<PartyPair> {
        let roles = five_party_roles();
        let mut pairs = Vec::with_capacity(6);
        for (idx, &a) in roles.iter().enumerate() {
            if a == own_role {
                continue;
            }
            for &b in &roles[idx + 1..] {
                if b == own_role {
                    continue;
                }
                pairs.push(PartyPair::new(a, b));
            }
        }
        pairs
    }
}

/// A `(3, 5)` shared-PRF key configuration for a 5-party protocol.
///
/// For every unordered pair of parties `{i, j}` there is one key `k_{i,j}`,
/// known only to the three parties *not* in `{i, j}`. Each party therefore
/// owns keys for the `C(4, 2) = 6` pairs that exclude it. This is a
/// generalization, to five parties, of the same trick that the replicated
/// (3-party) [`Prf`] above is built on: a value only the "other" parties can
/// predict is exactly what is needed to mask/rerandomize shares that those
/// parties, and not the excluded pair, must be able to jointly verify or
/// reconstruct.
///
/// Construct one with [`from_seeds`](Self::from_seeds) once the underlying
/// seeds have been agreed with the other owners of each key (see
/// `setup_threshold_prf_keys` in `protocol::ops`).
#[derive(Debug)]
pub struct ThresholdPrfKeys {
    own_role: Role,
    keys: BTreeMap<PartyPair, PrfRng>,
}

impl ThresholdPrfKeys {
    /// Build the key set from one already-agreed seed per owned pair.
    ///
    /// Fails unless `seeds` contains exactly the six pairs returned by
    /// [`PartyPair::excluding(own_role)`](PartyPair::excluding).
    pub fn from_seeds(own_role: Role, seeds: BTreeMap<PartyPair, PrfSeed>) -> Result<Self> {
        let expected: BTreeSet<PartyPair> = PartyPair::excluding(own_role).into_iter().collect();
        let actual: BTreeSet<PartyPair> = seeds.keys().copied().collect();
        if actual != expected {
            bail!(
                "threshold PRF key set for role {own_role:?} does not match the expected \
                 3-of-5 key set: expected {expected:?}, got {actual:?}"
            );
        }
        let keys = seeds
            .into_iter()
            .map(|(pair, seed)| (pair, seed_to_rng(seed)))
            .collect();
        Ok(Self { own_role, keys })
    }

    pub fn own_role(&self) -> Role {
        self.own_role
    }

    /// The PRF for key `k_{i,j}`. Returns `None` if this party does not own
    /// that key (i.e. `own_role` is `i` or `j`) or if `i == j`.
    pub fn get_mut(&mut self, i: Role, j: Role) -> Option<&mut PrfRng> {
        if i == j {
            return None;
        }
        self.keys.get_mut(&PartyPair::new(i, j))
    }

    /// The excluded pairs whose key this party holds.
    pub fn owned_pairs(&self) -> impl Iterator<Item = PartyPair> + '_ {
        self.keys.keys().copied()
    }
}

/// Pairwise shared-PRF keys: for every unordered pair of parties `{i, j}`,
/// a key `\lambda_{i,j}` known only to `i` and `j` themselves. Each party
/// owns one such key per other party.
///
/// Construct one with [`from_seeds`](Self::from_seeds) once the underlying
/// seed has been agreed with the other party of each pair (see
/// `setup_pairwise_prf_keys` in `protocol::ops`).
#[derive(Debug)]
pub struct PairwisePrfKeys {
    own_role: Role,
    keys: BTreeMap<Role, PrfRng>,
}

impl PairwisePrfKeys {
    /// Build the key set from one already-agreed seed per other party.
    ///
    /// Fails unless `seeds` contains exactly one entry per other party in
    /// the 5-party configuration (i.e. `0..FIVE_PARTY_COUNT`, excluding
    /// `own_role`).
    pub fn from_seeds(own_role: Role, seeds: BTreeMap<Role, PrfSeed>) -> Result<Self> {
        let expected: BTreeSet<Role> = five_party_roles()
            .into_iter()
            .filter(|role| *role != own_role)
            .collect();
        let actual: BTreeSet<Role> = seeds.keys().copied().collect();
        if actual != expected {
            bail!(
                "pairwise PRF key set for role {own_role:?} does not match the expected \
                 key set: expected {expected:?}, got {actual:?}"
            );
        }
        let keys = seeds
            .into_iter()
            .map(|(role, seed)| (role, seed_to_rng(seed)))
            .collect();
        Ok(Self { own_role, keys })
    }

    pub fn own_role(&self) -> Role {
        self.own_role
    }

    /// The PRF for key `\lambda_{own_role, other}`. Returns `None` if
    /// `other` is not a party this key set was built for.
    pub fn get_mut(&mut self, other: Role) -> Option<&mut PrfRng> {
        self.keys.get_mut(&other)
    }

    /// The other parties this party shares a pairwise key with.
    pub fn parties(&self) -> impl Iterator<Item = Role> + '_ {
        self.keys.keys().copied()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use statrs::distribution::{ChiSquared, ContinuousCDF};

    use super::*;

    // Chi-square test for uniformity with the significance level = 10^-6
    fn chi_squared_test(observed: &[u32], expected: u32) -> Result<bool> {
        if observed.len() < 2 {
            bail!("Need at least two bins for chi-squared test");
        }
        let degrees_of_freedom = observed.len() - 1;
        let expected_f = expected as f64;
        let chi2: f64 = observed
            .iter()
            .map(|o| {
                let diff = *o as f64 - expected_f;
                diff * diff / expected_f
            })
            .sum();

        // Significance level
        let alpha = 1e-6;
        let chi_squared_dist = ChiSquared::new(degrees_of_freedom as f64)?;
        let critical_value = chi_squared_dist.inverse_cdf(1.0 - alpha);

        Ok(chi2 < critical_value)
    }

    #[test]
    fn test_gen_u32_mod() -> Result<()> {
        let mut prf = Prf::default();

        // Expected count for values in each bin
        let expected = 1000;

        let mut helper = |modulus: u32| -> Result<()> {
            let mut counters_a = vec![0_u32; modulus as usize];
            let mut counters_b = vec![0_u32; modulus as usize];
            let num_samples = modulus * expected;
            for _ in 0..num_samples {
                let (v_a, v_b) = prf.gen_u32_mod(modulus)?;
                counters_a[v_a as usize] += 1;
                counters_b[v_b as usize] += 1;
            }

            assert!(chi_squared_test(&counters_a, expected)?);
            assert!(chi_squared_test(&counters_b, expected)?);

            Ok(())
        };
        helper(2)?;
        helper(7)?;
        helper(101)?;

        Ok(())
    }

    #[test]
    fn test_gen_permutation() -> Result<()> {
        let mut prf = Prf::default();
        // Expected count for each permutation
        let expected = 100;

        let mut helper = |size: u32| -> Result<()> {
            let num_bins: u32 = (2..=size).product();
            let num_samples = num_bins * expected / 2;

            let mut perm_stats = HashMap::new();
            for _ in 0..num_samples {
                let perm = prf.gen_permutation(size)?;
                *perm_stats.entry(perm.0).or_insert(0_u32) += 1;
                *perm_stats.entry(perm.1).or_insert(0_u32) += 1;
            }

            // Check that all permutations have been generated.
            assert_eq!(perm_stats.len() as u32, num_bins);

            let counters: Vec<u32> = perm_stats.values().cloned().collect();
            assert!(chi_squared_test(&counters, expected)?);

            Ok(())
        };
        helper(2)?;
        helper(4)?;
        helper(5)?;

        Ok(())
    }

    #[test]
    fn test_party_pair_excluding_gives_six_disjoint_pairs() {
        for own in 0..FIVE_PARTY_COUNT {
            let own_role = Role::new(own as usize);
            let pairs = PartyPair::excluding(own_role);
            assert_eq!(pairs.len(), 6);
            assert!(pairs.iter().all(|pair| !pair.contains(own_role)));

            let unique: std::collections::HashSet<_> = pairs.iter().collect();
            assert_eq!(unique.len(), 6, "pairs must be pairwise distinct");
        }

        // Every one of the 10 possible pairs is owned by exactly 3 of the 5 parties.
        let mut owner_counts: HashMap<PartyPair, u32> = HashMap::new();
        for own in 0..FIVE_PARTY_COUNT {
            for pair in PartyPair::excluding(Role::new(own as usize)) {
                *owner_counts.entry(pair).or_insert(0) += 1;
            }
        }
        assert_eq!(owner_counts.len(), 10);
        assert!(owner_counts.values().all(|&count| count == 3));
    }

    #[test]
    fn test_threshold_prf_keys_from_seeds_rejects_wrong_key_set() {
        let own_role = Role::new(0);
        // Missing key set (empty) should be rejected.
        assert!(ThresholdPrfKeys::from_seeds(own_role, BTreeMap::new()).is_err());

        // A key set that includes a pair containing own_role should be rejected.
        let mut seeds: BTreeMap<PartyPair, PrfSeed> = PartyPair::excluding(Role::new(1))
            .into_iter()
            .map(|pair| (pair, [0u8; 16]))
            .collect();
        assert!(ThresholdPrfKeys::from_seeds(own_role, seeds.clone()).is_err());

        // The correct key set is accepted.
        seeds = PartyPair::excluding(own_role)
            .into_iter()
            .map(|pair| (pair, [0u8; 16]))
            .collect();
        assert!(ThresholdPrfKeys::from_seeds(own_role, seeds).is_ok());
    }

    #[test]
    fn test_pairwise_prf_keys_from_seeds_rejects_wrong_key_set() {
        let own_role = Role::new(0);
        assert!(PairwisePrfKeys::from_seeds(own_role, BTreeMap::new()).is_err());

        // Including a key for `own_role` itself should be rejected.
        let mut seeds: BTreeMap<Role, PrfSeed> = (0..FIVE_PARTY_COUNT)
            .map(|i| (Role::new(i as usize), [0u8; 16]))
            .collect();
        assert!(PairwisePrfKeys::from_seeds(own_role, seeds.clone()).is_err());

        seeds.remove(&own_role);
        assert!(PairwisePrfKeys::from_seeds(own_role, seeds).is_ok());
    }
}
