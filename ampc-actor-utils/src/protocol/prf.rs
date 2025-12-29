use crate::protocol::shuffle::Permutation;
use ampc_secret_sharing::shares::{
    int_ring::IntRing2k,
    ring_impl::{RingElement, VecRingElement},
};
use eyre::Result;
use rand::{distributions::Standard, prelude::Distribution, Rng, RngCore, SeedableRng};

/// Generate a uniformly random u32 in [0, modulus)
fn gen_u32_mod(rng: &mut PrfRng, modulus: u32) -> Result<u32> {
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
    #[cfg(not(feature = "aes_rng_prf"))]
    pub fn new(my_key: PrfSeed, prev_key: PrfSeed) -> Self {
        Self {
            my_prf: PrfRng::from_seed(Self::expand_seed(my_key)),
            prev_prf: PrfRng::from_seed(Self::expand_seed(prev_key)),
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

    #[cfg(feature = "aes_rng_prf")]
    pub fn new(my_key: PrfSeed, prev_key: PrfSeed) -> Self {
        Self {
            my_prf: PrfRng::from_seed(my_key),
            prev_prf: PrfRng::from_seed(prev_key),
        }
    }

    pub fn get_my_prf(&mut self) -> &mut PrfRng {
        &mut self.my_prf
    }

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

    /// Generates `n` zero shares in batch. Uses fill_bytes for maximum performance.
    pub fn gen_zero_shares_batch<T: IntRing2k>(&mut self, n: usize) -> VecRingElement<T>
    where
        Standard: Distribution<T>,
    {
        VecRingElement(self.gen_zero_shares_fast(n))
    }

    /// Fills an existing slice with zero shares. Use this with thread_local buffers
    /// to avoid repeated allocations in hot paths.
    #[inline]
    pub fn fill_zero_shares<T: IntRing2k>(&mut self, dest: &mut [RingElement<T>])
    where
        Standard: Distribution<T>,
    {
        for elem in dest.iter_mut() {
            let a: RingElement<T> = self.my_prf.gen::<RingElement<T>>();
            let b: RingElement<T> = self.prev_prf.gen::<RingElement<T>>();
            *elem = a - b;
        }
    }

    /// Generates `n` binary zero shares in batch. Uses fill_bytes for maximum performance.
    pub fn gen_binary_zero_shares_batch<T: IntRing2k>(&mut self, n: usize) -> VecRingElement<T>
    where
        Standard: Distribution<T>,
    {
        VecRingElement(self.gen_binary_zero_shares_fast(n))
    }

    /// Fills an existing slice with binary zero shares. Use this with thread_local buffers
    /// to avoid repeated allocations in hot paths.
    #[inline]
    pub fn fill_binary_zero_shares<T: IntRing2k>(&mut self, dest: &mut [RingElement<T>])
    where
        Standard: Distribution<T>,
    {
        for elem in dest.iter_mut() {
            let a: RingElement<T> = self.my_prf.gen::<RingElement<T>>();
            let b: RingElement<T> = self.prev_prf.gen::<RingElement<T>>();
            *elem = a ^ b;
        }
    }

    /// Generates `n` random RingElements from my_prf. Uses fill_bytes for maximum performance.
    pub fn gen_my_rands_batch<T: IntRing2k>(&mut self, n: usize) -> VecRingElement<T>
    where
        Standard: Distribution<T>,
    {
        VecRingElement(self.gen_my_rands_fast(n))
    }

    /// Fills an existing slice with random RingElements from my_prf.
    #[inline]
    pub fn fill_my_rands<T: IntRing2k>(&mut self, dest: &mut [RingElement<T>])
    where
        Standard: Distribution<T>,
    {
        for elem in dest.iter_mut() {
            *elem = self.my_prf.gen::<RingElement<T>>();
        }
    }

    /// Generates `n` random RingElements from prev_prf. Uses fill_bytes for maximum performance.
    pub fn gen_prev_rands_batch<T: IntRing2k>(&mut self, n: usize) -> VecRingElement<T>
    where
        Standard: Distribution<T>,
    {
        VecRingElement(self.gen_prev_rands_fast(n))
    }

    /// Fills an existing slice with random RingElements from prev_prf.
    #[inline]
    pub fn fill_prev_rands<T: IntRing2k>(&mut self, dest: &mut [RingElement<T>])
    where
        Standard: Distribution<T>,
    {
        for elem in dest.iter_mut() {
            *elem = self.prev_prf.gen::<RingElement<T>>();
        }
    }

    /// Ultra-fast zero share generation using fill_bytes.
    /// Bypasses the Distribution trait entirely for maximum performance.
    /// Uses chunked processing to minimize memory bandwidth.
    ///
    /// # Safety
    /// This uses unsafe pointer casts. Safe because RingElement<T> is #[repr(transparent)]
    /// and T is a primitive integer type with no padding.
    #[inline]
    pub fn gen_zero_shares_fast<T: IntRing2k>(&mut self, n: usize) -> Vec<RingElement<T>> {
        if n == 0 {
            return Vec::new();
        }
        let elem_size = std::mem::size_of::<T>();
        let byte_len = n * elem_size;

        // Allocate result buffer
        let mut result: Vec<RingElement<T>> = vec![RingElement(T::default()); n];

        // Process in chunks to keep temp buffer in L1 cache
        const CHUNK_BYTES: usize = 4096; // 4KB fits comfortably in L1
        let chunk_elems = CHUNK_BYTES / elem_size;
        let mut temp: Vec<T> = vec![T::default(); chunk_elems.max(1)];

        for chunk in result.chunks_mut(chunk_elems) {
            let chunk_len = chunk.len();
            let chunk_byte_len = chunk_len * elem_size;

            // Fill chunk with my_prf
            let chunk_bytes: &mut [u8] =
                unsafe { std::slice::from_raw_parts_mut(chunk.as_mut_ptr() as *mut u8, chunk_byte_len) };
            self.my_prf.fill_bytes(chunk_bytes);

            // Fill temp with prev_prf
            let temp_bytes: &mut [u8] =
                unsafe { std::slice::from_raw_parts_mut(temp.as_mut_ptr() as *mut u8, chunk_byte_len) };
            self.prev_prf.fill_bytes(temp_bytes);

            // Subtract in place
            for (r, t) in chunk.iter_mut().zip(temp[..chunk_len].iter()) {
                r.0 = r.0.wrapping_sub(t);
            }
        }

        result
    }

    /// Ultra-fast binary zero share generation using fill_bytes.
    /// Bypasses the Distribution trait entirely for maximum performance.
    /// Uses chunked processing to minimize memory bandwidth.
    #[inline]
    pub fn gen_binary_zero_shares_fast<T: IntRing2k>(&mut self, n: usize) -> Vec<RingElement<T>> {
        if n == 0 {
            return Vec::new();
        }
        let byte_len = n * std::mem::size_of::<T>();

        // Allocate result buffer
        let mut result: Vec<RingElement<T>> = vec![RingElement(T::default()); n];
        let result_bytes: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(result.as_mut_ptr() as *mut u8, byte_len) };

        // Process in chunks to keep temp buffer in L1 cache
        // ChaCha generates 64 bytes at a time, so use a multiple of that
        const CHUNK_SIZE: usize = 4096; // 4KB fits comfortably in L1
        let mut temp = [0u8; CHUNK_SIZE];

        for chunk in result_bytes.chunks_mut(CHUNK_SIZE) {
            let chunk_len = chunk.len();
            // Fill chunk with my_prf
            self.my_prf.fill_bytes(chunk);
            // Fill temp with prev_prf and XOR immediately
            self.prev_prf.fill_bytes(&mut temp[..chunk_len]);
            // XOR in place - compiler will auto-vectorize this
            for (r, t) in chunk.iter_mut().zip(temp[..chunk_len].iter()) {
                *r ^= *t;
            }
        }

        result
    }

    /// Ultra-fast random generation from my_prf using fill_bytes.
    #[inline]
    pub fn gen_my_rands_fast<T: IntRing2k>(&mut self, n: usize) -> Vec<RingElement<T>> {
        if n == 0 {
            return Vec::new();
        }
        let byte_len = n * std::mem::size_of::<T>();
        let mut result: Vec<RingElement<T>> = vec![RingElement(T::default()); n];
        let result_bytes: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(result.as_mut_ptr() as *mut u8, byte_len) };
        self.my_prf.fill_bytes(result_bytes);
        result
    }

    /// Ultra-fast random generation from prev_prf using fill_bytes.
    #[inline]
    pub fn gen_prev_rands_fast<T: IntRing2k>(&mut self, n: usize) -> Vec<RingElement<T>> {
        if n == 0 {
            return Vec::new();
        }
        let byte_len = n * std::mem::size_of::<T>();
        let mut result: Vec<RingElement<T>> = vec![RingElement(T::default()); n];
        let result_bytes: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(result.as_mut_ptr() as *mut u8, byte_len) };
        self.prev_prf.fill_bytes(result_bytes);
        result
    }

    /// Fill existing buffer with zero shares using fill_bytes (no allocation).
    /// Use with thread_local pre-allocated buffers for hot paths.
    #[inline]
    pub fn fill_zero_shares_fast<T: IntRing2k>(
        &mut self,
        dest: &mut [RingElement<T>],
        temp: &mut [T],
    ) {
        debug_assert_eq!(dest.len(), temp.len());
        if dest.is_empty() {
            return;
        }
        let byte_len = dest.len() * std::mem::size_of::<T>();

        // Fill dest with bytes from my_prf
        let dest_bytes: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(dest.as_mut_ptr() as *mut u8, byte_len) };
        self.my_prf.fill_bytes(dest_bytes);

        // Fill temp with bytes from prev_prf
        let temp_bytes: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(temp.as_mut_ptr() as *mut u8, byte_len) };
        self.prev_prf.fill_bytes(temp_bytes);

        // Compute difference in place
        for (d, t) in dest.iter_mut().zip(temp.iter()) {
            d.0 = d.0.wrapping_sub(t);
        }
    }

    /// Fill existing buffer with binary zero shares using fill_bytes (no allocation).
    #[inline]
    pub fn fill_binary_zero_shares_fast<T: IntRing2k>(
        &mut self,
        dest: &mut [RingElement<T>],
        temp: &mut [T],
    ) {
        debug_assert_eq!(dest.len(), temp.len());
        if dest.is_empty() {
            return;
        }
        let byte_len = dest.len() * std::mem::size_of::<T>();

        // Fill dest with bytes from my_prf
        let dest_bytes: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(dest.as_mut_ptr() as *mut u8, byte_len) };
        self.my_prf.fill_bytes(dest_bytes);

        // Fill temp with bytes from prev_prf
        let temp_bytes: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(temp.as_mut_ptr() as *mut u8, byte_len) };
        self.prev_prf.fill_bytes(temp_bytes);

        // Compute XOR in place
        for (d, t) in dest.iter_mut().zip(temp.iter()) {
            d.0 = d.0 ^ *t;
        }
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
