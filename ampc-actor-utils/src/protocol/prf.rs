use crate::protocol::shuffle::Permutation;
use ampc_secret_sharing::shares::{int_ring::IntRing2k, ring_impl::RingElement};
use eyre::Result;
use rand::{distributions::Standard, prelude::Distribution, Rng, RngCore, SeedableRng};

/// Generate a uniformly random u32 in [0, modulus)
fn gen_u32_mod<R: Rng>(rng: &mut R, modulus: u32) -> Result<u32> {
    if modulus == 0 {
        eyre::bail!("modulus must be non-zero");
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

const RNG_BUF_LEN: usize = 1024;
/// A buffered wrapper around PrfRng that generates 1024 bytes at a time
/// to minimize the overhead of frequent small random number generation calls.
#[derive(Clone, Debug)]
pub struct BufferedPrf {
    rng: PrfRng,
    buffer: [u8; RNG_BUF_LEN],
    position: usize,
}

impl BufferedPrf {
    /// Create a new BufferedPrf from an existing PrfRng
    pub fn new(mut rng: PrfRng) -> Self {
        let mut buffer = [0u8; RNG_BUF_LEN];
        rng.fill_bytes(&mut buffer);
        Self {
            rng,
            buffer,
            position: 0,
        }
    }

    /// Refill the buffer with fresh random bytes
    fn refill_buffer(&mut self) {
        self.rng.fill_bytes(&mut self.buffer);
        self.position = 0;
    }

    /// Get the next value of type T from the buffer
    fn gen<T>(&mut self) -> T
    where
        Standard: Distribution<T>,
    {
        let t_size = std::mem::size_of::<T>();

        // Safety check: if type is larger than buffer, fall back to direct generation
        if t_size > self.buffer.len() {
            return self.rng.gen::<T>();
        }

        // Ensure we have enough bytes in the buffer
        if self.position + t_size > self.buffer.len() {
            self.refill_buffer();
        }

        // Safetyy:
        // T is guaranteed to be <= the buffer size
        let result = unsafe { std::ptr::read(self.buffer.as_ptr().add(self.position) as *const T) };

        self.position += t_size;
        result
    }

    /// Get the next `len` bytes from the buffer, refilling if necessary
    fn next_bytes(&mut self, dest: &mut [u8]) {
        self.rng.fill_bytes(dest);
    }
}

impl RngCore for BufferedPrf {
    fn next_u32(&mut self) -> u32 {
        self.gen::<u32>()
    }

    fn next_u64(&mut self) -> u64 {
        self.gen::<u64>()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.next_bytes(dest);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl SeedableRng for BufferedPrf {
    type Seed = <PrfRng as SeedableRng>::Seed;

    fn from_seed(seed: Self::Seed) -> Self {
        Self::new(PrfRng::from_seed(seed))
    }

    fn from_entropy() -> Self {
        Self::new(PrfRng::from_entropy())
    }
}

#[derive(Clone, Debug)]
pub struct Prf {
    pub my_prf: BufferedPrf,
    pub prev_prf: BufferedPrf,
}

impl Default for Prf {
    fn default() -> Self {
        Self {
            my_prf: BufferedPrf::new(PrfRng::from_entropy()),
            prev_prf: BufferedPrf::new(PrfRng::from_entropy()),
        }
    }
}

impl Prf {
    #[cfg(not(feature = "aes_rng_prf"))]
    pub fn new(my_key: PrfSeed, prev_key: PrfSeed) -> Self {
        Self {
            my_prf: BufferedPrf::new(PrfRng::from_seed(Self::expand_seed(my_key))),
            prev_prf: BufferedPrf::new(PrfRng::from_seed(Self::expand_seed(prev_key))),
        }
    }

    #[cfg(not(feature = "aes_rng_prf"))]
    pub fn expand_seed(seed: PrfSeed) -> [u8; 32] {
        use blake3::Hasher;
        let mut h = Hasher::new();
        h.update(&seed);
        let digest = h.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(digest.as_bytes());
        out
    }

    #[cfg(feature = "aes_rng_prf")]
    pub fn new(my_key: PrfSeed, prev_key: PrfSeed) -> Self {
        Self {
            my_prf: BufferedPrf(PrfRng::from_seed(my_key)),
            prev_prf: BufferedPrf(PrfRng::from_seed(prev_key)),
        }
    }

    pub fn get_my_prf(&mut self) -> &mut BufferedPrf {
        &mut self.my_prf
    }

    pub fn get_prev_prf(&mut self) -> &mut BufferedPrf {
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use statrs::distribution::{ChiSquared, ContinuousCDF};

    use super::*;

    // Chi-square test for uniformity with the significance level = 10^-6
    fn chi_squared_test(observed: &[u32], expected: u32) -> Result<bool> {
        if observed.len() < 2 {
            eyre::bail!("Need at least two bins for chi-squared test");
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
}
