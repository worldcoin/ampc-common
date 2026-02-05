use super::{
    int_ring::IntRing2k,
    ring_impl::RingElement,
    share::Share,
    vecshare::{SliceShare, VecShare},
};

/// Flat, bit-lane-major storage for bit-transposed packed shares.
///
/// Logically represents `num_bits` bit lanes, each containing `chunk_count`
/// shares of type T. Stored as a single contiguous allocation.
///
/// Layout: `data[bit_index * chunk_count + chunk_index]`
#[derive(Clone, Debug)]
pub struct TransposedPack<T: IntRing2k> {
    data: Vec<Share<T>>,
    num_bits: usize,
    chunk_count: usize,
}

impl<T: IntRing2k> TransposedPack<T> {
    /// Create a new TransposedPack filled with default shares.
    pub fn new(num_bits: usize, chunk_count: usize) -> Self {
        Self {
            data: vec![Share::default(); num_bits * chunk_count],
            num_bits,
            chunk_count,
        }
    }

    /// Construct from a pre-built flat data vector with known dimensions.
    pub fn from_flat(data: Vec<Share<T>>, num_bits: usize, chunk_count: usize) -> Self {
        debug_assert_eq!(data.len(), num_bits * chunk_count);
        Self {
            data,
            num_bits,
            chunk_count,
        }
    }

    pub fn num_bits(&self) -> usize {
        self.num_bits
    }

    pub fn chunk_count(&self) -> usize {
        self.chunk_count
    }

    /// Access a single bit lane as a slice.
    pub fn lane(&self, bit: usize) -> &[Share<T>] {
        let start = bit * self.chunk_count;
        &self.data[start..start + self.chunk_count]
    }

    /// Access a single bit lane as a mutable slice.
    pub fn lane_mut(&mut self, bit: usize) -> &mut [Share<T>] {
        let start = bit * self.chunk_count;
        &mut self.data[start..start + self.chunk_count]
    }

    /// Access a single bit lane as a SliceShare (for passing to and_many etc.).
    pub fn lane_as_slice(&self, bit: usize) -> SliceShare<'_, T> {
        SliceShare::from_slice(self.lane(bit))
    }

    /// Pop the last bit lane, returning it as a VecShare.
    /// Decrements num_bits and truncates the backing storage.
    pub fn pop_lane(&mut self) -> Option<VecShare<T>> {
        if self.num_bits == 0 {
            return None;
        }
        self.num_bits -= 1;
        let start = self.num_bits * self.chunk_count;
        let lane_data = self.data[start..].to_vec();
        self.data.truncate(start);
        Some(VecShare::new_vec(lane_data))
    }

    /// Convert to Vec<VecShare<T>> for code that needs lane-level ownership
    /// (e.g. the prefix tree in binary_add_2_get_msb).
    pub fn into_lanes(self) -> Vec<VecShare<T>> {
        self.data
            .chunks_exact(self.chunk_count)
            .map(|chunk| VecShare::new_vec(chunk.to_vec()))
            .collect()
    }

    /// XOR-assign element-wise with another TransposedPack.
    pub fn xor_assign(&mut self, other: &Self) {
        debug_assert_eq!(self.num_bits, other.num_bits);
        debug_assert_eq!(self.chunk_count, other.chunk_count);
        for (a, b) in self.data.iter_mut().zip(other.data.iter()) {
            *a ^= b;
        }
    }

    /// XOR element-wise, producing a new TransposedPack.
    pub fn xor(&self, other: &Self) -> Self {
        debug_assert_eq!(self.num_bits, other.num_bits);
        debug_assert_eq!(self.chunk_count, other.chunk_count);
        let data = self
            .data
            .iter()
            .zip(other.data.iter())
            .map(|(a, b)| a ^ b)
            .collect();
        Self {
            data,
            num_bits: self.num_bits,
            chunk_count: self.chunk_count,
        }
    }

    /// XOR a single lane in-place with a SliceShare (e.g. for carry propagation).
    pub fn xor_lane_assign(&mut self, bit: usize, other: SliceShare<'_, T>) {
        debug_assert_eq!(self.chunk_count, other.len());
        for (a, b) in self.lane_mut(bit).iter_mut().zip(other.iter()) {
            *a ^= b;
        }
    }

    /// Iterate over all elements in flat (lane-major) order.
    pub fn iter(&self) -> impl Iterator<Item = &Share<T>> {
        self.data.iter()
    }

    /// Consume self and return the flat backing data.
    pub fn into_flat_data(self) -> Vec<Share<T>> {
        self.data
    }
}

impl<T: IntRing2k> IntoIterator for TransposedPack<T> {
    type Item = Share<T>;
    type IntoIter = std::vec::IntoIter<Share<T>>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.into_iter()
    }
}

pub trait Transpose64 {
    fn transpose_pack_u64(self) -> TransposedPack<u64>;
}

pub trait Transpose128 {
    fn transpose_pack_u128(self) -> Vec<VecShare<u128>>;
}

impl VecShare<u16> {
    fn share64_from_share16s(
        a: &Share<u16>,
        b: &Share<u16>,
        c: &Share<u16>,
        d: &Share<u16>,
    ) -> Share<u64> {
        let a_ = (a.a.0 as u64)
            | ((b.a.0 as u64) << 16)
            | ((c.a.0 as u64) << 32)
            | ((d.a.0 as u64) << 48);
        let b_ = (a.b.0 as u64)
            | ((b.b.0 as u64) << 16)
            | ((c.b.0 as u64) << 32)
            | ((d.b.0 as u64) << 48);

        Share {
            a: RingElement(a_),
            b: RingElement(b_),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn share128_from_share16s(
        a: &Share<u16>,
        b: &Share<u16>,
        c: &Share<u16>,
        d: &Share<u16>,
        e: &Share<u16>,
        f: &Share<u16>,
        g: &Share<u16>,
        h: &Share<u16>,
    ) -> Share<u128> {
        let a_ = (a.a.0 as u128)
            | ((b.a.0 as u128) << 16)
            | ((c.a.0 as u128) << 32)
            | ((d.a.0 as u128) << 48)
            | ((e.a.0 as u128) << 64)
            | ((f.a.0 as u128) << 80)
            | ((g.a.0 as u128) << 96)
            | ((h.a.0 as u128) << 112);
        let b_ = (a.b.0 as u128)
            | ((b.b.0 as u128) << 16)
            | ((c.b.0 as u128) << 32)
            | ((d.b.0 as u128) << 48)
            | ((e.b.0 as u128) << 64)
            | ((f.b.0 as u128) << 80)
            | ((g.b.0 as u128) << 96)
            | ((h.b.0 as u128) << 112);

        Share {
            a: RingElement(a_),
            b: RingElement(b_),
        }
    }

    fn share_transpose16x128(a: &[Share<u16>; 128]) -> [Share<u128>; 16] {
        let mut j: u32;
        let mut k: usize;
        let mut m: u128;
        let mut t: Share<u128>;

        let mut res = core::array::from_fn(|_| Share::default());

        // pack results into Share128 datatypes
        for (i, bb) in res.iter_mut().enumerate() {
            *bb = Self::share128_from_share16s(
                &a[i],
                &a[i + 16],
                &a[i + 32],
                &a[i + 48],
                &a[i + 64],
                &a[i + 80],
                &a[i + 96],
                &a[i + 112],
            );
        }

        // version of 128x128 transpose that only does the swaps needed for 16 bits
        m = 0x00ff00ff00ff00ff00ff00ff00ff00ff;
        j = 8;
        while j != 0 {
            k = 0;
            while k < 16 {
                t = ((&res[k] >> j) ^ res[k + j as usize]) & m;
                res[k + j as usize] ^= &t;
                res[k] ^= t << j;
                k = (k + j as usize + 1) & !(j as usize);
            }
            j >>= 1;
            m = m ^ (m << j);
        }

        res
    }

    fn share_transpose16x64(a: &[Share<u16>; 64]) -> [Share<u64>; 16] {
        let mut j: u32;
        let mut k: usize;
        let mut m: u64;
        let mut t: Share<u64>;

        let mut res = core::array::from_fn(|_| Share::default());

        // pack results into Share64 datatypes
        for (i, bb) in res.iter_mut().enumerate() {
            *bb = Self::share64_from_share16s(&a[i], &a[16 + i], &a[32 + i], &a[48 + i]);
        }

        // version of 64x64 transpose that only does the swaps needed for 16 bits
        m = 0x00ff00ff00ff00ff;
        j = 8;
        while j != 0 {
            k = 0;
            while k < 16 {
                t = ((&res[k] >> j) ^ res[k + j as usize]) & m;
                res[k + j as usize] ^= &t;
                res[k] ^= t << j;
                k = (k + j as usize + 1) & !(j as usize);
            }
            j >>= 1;
            m = m ^ (m << j);
        }

        res
    }
}

impl Transpose64 for VecShare<u16> {
    fn transpose_pack_u64(mut self) -> TransposedPack<u64> {
        let chunk_count = self.shares.len().div_ceil(64);
        self.shares.resize(chunk_count * 64, Share::default());

        let mut result = TransposedPack::new(16, chunk_count);

        for (j, x) in self.shares.chunks_exact(64).enumerate() {
            let trans = Self::share_transpose16x64(x.try_into().unwrap());
            for (bit, src) in trans.into_iter().enumerate() {
                result.lane_mut(bit)[j] = src;
            }
        }
        result
    }
}

impl Transpose128 for VecShare<u16> {
    fn transpose_pack_u128(mut self) -> Vec<VecShare<u128>> {
        // Pad to multiple of 128
        let len = self.shares.len().div_ceil(128);
        self.shares.resize(len * 128, Share::default());

        let mut res = (0..16)
            .map(|_| VecShare::new_vec(vec![Share::default(); len]))
            .collect::<Vec<_>>();

        for (j, x) in self.shares.chunks_exact(128).enumerate() {
            let trans = Self::share_transpose16x128(x.try_into().unwrap());
            for (src, des) in trans.into_iter().zip(res.iter_mut()) {
                des.shares[j] = src;
            }
        }
        debug_assert_eq!(res.len(), 16);
        res
    }
}

impl VecShare<u32> {
    fn share64_from_share32s(a: &Share<u32>, b: &Share<u32>) -> Share<u64> {
        let a_ = (a.a.0 as u64) | ((b.a.0 as u64) << 32);
        let b_ = (a.b.0 as u64) | ((b.b.0 as u64) << 32);

        Share {
            a: RingElement(a_),
            b: RingElement(b_),
        }
    }

    fn share128_from_share32s(
        a: &Share<u32>,
        b: &Share<u32>,
        c: &Share<u32>,
        d: &Share<u32>,
    ) -> Share<u128> {
        let a_ = (a.a.0 as u128)
            | ((b.a.0 as u128) << 32)
            | ((c.a.0 as u128) << 64)
            | ((d.a.0 as u128) << 96);
        let b_ = (a.b.0 as u128)
            | ((b.b.0 as u128) << 32)
            | ((c.b.0 as u128) << 64)
            | ((d.b.0 as u128) << 96);

        Share {
            a: RingElement(a_),
            b: RingElement(b_),
        }
    }

    fn share_transpose32x128(a: &[Share<u32>; 128]) -> [Share<u128>; 32] {
        let mut j: u32;
        let mut k: usize;
        let mut m: u128;
        let mut t: Share<u128>;

        let mut res = core::array::from_fn(|_| Share::default());

        // pack results into Share128 datatypes
        for (i, bb) in res.iter_mut().enumerate() {
            *bb = Self::share128_from_share32s(&a[i], &a[32 + i], &a[64 + i], &a[96 + i]);
        }

        // version of 128x128 transpose that only does the swaps needed for 32 bits
        m = 0x0000ffff0000ffff0000ffff0000ffff;
        j = 16;
        while j != 0 {
            k = 0;
            while k < 32 {
                t = ((&res[k] >> j) ^ res[k + j as usize]) & m;
                res[k + j as usize] ^= &t;
                res[k] ^= t << j;
                k = (k + j as usize + 1) & !(j as usize);
            }
            j >>= 1;
            m = m ^ (m << j);
        }

        res
    }

    fn share_transpose32x64(a: &[Share<u32>; 64]) -> [Share<u64>; 32] {
        let mut j: u32;
        let mut k: usize;
        let mut m: u64;
        let mut t: Share<u64>;

        let mut res = core::array::from_fn(|_| Share::default());

        // pack results into Share64 datatypes
        for (i, bb) in res.iter_mut().enumerate() {
            *bb = Self::share64_from_share32s(&a[i], &a[32 + i]);
        }

        // version of 64x64 transpose that only does the swaps needed for 32 bits
        m = 0x0000ffff0000ffff;
        j = 16;
        while j != 0 {
            k = 0;
            while k < 32 {
                t = ((&res[k] >> j) ^ res[k + j as usize]) & m;
                res[k + j as usize] ^= &t;
                res[k] ^= t << j;
                k = (k + j as usize + 1) & !(j as usize);
            }
            j >>= 1;
            m = m ^ (m << j);
        }

        res
    }
}

impl Transpose64 for VecShare<u32> {
    /// Transposes `u32` shares into slices of bits and packs them into `u64` shares.
    /// The result has 32 bit lanes, each of length ceil(input_len / 64).
    fn transpose_pack_u64(mut self) -> TransposedPack<u64> {
        let chunk_count = self.shares.len().div_ceil(64);
        self.shares.resize(chunk_count * 64, Share::default());

        let mut result = TransposedPack::new(32, chunk_count);

        for (j, x) in self.shares.chunks_exact(64).enumerate() {
            let trans = Self::share_transpose32x64(x.try_into().unwrap());
            for (bit, src) in trans.into_iter().enumerate() {
                result.lane_mut(bit)[j] = src;
            }
        }
        result
    }
}

impl Transpose128 for VecShare<u32> {
    /// Transposes `u32` shares into slices of bits and packs them into `u128` shares.
    /// The result is a vector of `VecShare<u128>` with 32 elements corresponding to each bit.
    /// The length of each `VecShare<u128>` is ceil(length of self / 128).
    fn transpose_pack_u128(mut self) -> Vec<VecShare<u128>> {
        // Pad to multiple of 128
        let len = self.shares.len().div_ceil(128);
        self.shares.resize(len * 128, Share::default());

        let mut res = (0..32)
            .map(|_| VecShare::new_vec(vec![Share::default(); len]))
            .collect::<Vec<_>>();

        for (j, x) in self.shares.chunks_exact(128).enumerate() {
            let trans = Self::share_transpose32x128(x.try_into().unwrap());
            for (src, des) in trans.into_iter().zip(res.iter_mut()) {
                des.shares[j] = src;
            }
        }
        debug_assert_eq!(res.len(), 32);
        res
    }
}

impl VecShare<u64> {
    fn share128_from_share64s(a: &Share<u64>, b: &Share<u64>) -> Share<u128> {
        let a_ = (a.a.0 as u128) | ((b.a.0 as u128) << 64);
        let b_ = (a.b.0 as u128) | ((b.b.0 as u128) << 64);

        Share {
            a: RingElement(a_),
            b: RingElement(b_),
        }
    }

    fn share_transpose64x128(a: &[Share<u64>; 128]) -> [Share<u128>; 64] {
        let mut j: u32;
        let mut k: usize;
        let mut m: u128;
        let mut t: Share<u128>;

        let mut res = core::array::from_fn(|_| Share::default());

        // pack results into Share128 datatypes
        for (i, bb) in res.iter_mut().enumerate() {
            *bb = Self::share128_from_share64s(&a[i], &a[i + 64]);
        }

        // version of 128x128 transpose that only does the swaps needed for 64 bits
        m = 0x00000000ffffffff00000000ffffffff;
        j = 32;
        while j != 0 {
            k = 0;
            while k < 64 {
                t = ((&res[k] >> j) ^ res[k + j as usize]) & m;
                res[k + j as usize] ^= &t;
                res[k] ^= t << j;
                k = (k + j as usize + 1) & !(j as usize);
            }
            j >>= 1;
            m = m ^ (m << j);
        }

        res
    }

    fn share_transpose64x64(a: &mut [Share<u64>; 64]) {
        let mut j: u32;
        let mut k: usize;
        let mut m: u64;
        let mut t: Share<u64>;

        m = 0x00000000ffffffff;
        j = 32;
        while j != 0 {
            k = 0;
            while k < 64 {
                t = ((&a[k] >> j) ^ a[k + j as usize]) & m;
                a[k + j as usize] ^= &t;
                a[k] ^= t << j;
                k = (k + j as usize + 1) & !(j as usize);
            }
            j >>= 1;
            m = m ^ (m << j);
        }
    }
}

impl Transpose64 for VecShare<u64> {
    fn transpose_pack_u64(mut self) -> TransposedPack<u64> {
        let chunk_count = self.shares.len().div_ceil(64);
        self.shares.resize(chunk_count * 64, Share::default());

        let mut result = TransposedPack::new(64, chunk_count);

        for (j, x) in self.shares.chunks_exact_mut(64).enumerate() {
            Self::share_transpose64x64(x.try_into().unwrap());
            for (bit, src) in x.iter().enumerate() {
                result.lane_mut(bit)[j] = *src;
            }
        }
        result
    }
}

impl Transpose128 for VecShare<u64> {
    fn transpose_pack_u128(mut self) -> Vec<VecShare<u128>> {
        // Pad to multiple of 128
        let len = self.shares.len().div_ceil(128);
        self.shares.resize(len * 128, Share::default());

        let mut res = (0..64)
            .map(|_| VecShare::new_vec(vec![Share::default(); len]))
            .collect::<Vec<_>>();

        for (j, x) in self.shares.chunks_exact(128).enumerate() {
            let trans = Self::share_transpose64x128(x.try_into().unwrap());
            for (src, des) in trans.into_iter().zip(res.iter_mut()) {
                des.shares[j] = src;
            }
        }
        debug_assert_eq!(res.len(), 64);
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shares::{vecshare::VecShare, IntRing2k};
    use rand::Rng;

    fn check_transposed_flat<T: IntRing2k, U: IntRing2k>(
        transposed: &TransposedPack<T>,
        original: &VecShare<U>,
    ) {
        assert_eq!(transposed.num_bits(), U::K);
        let expected_bitslice_len = original.len().div_ceil(T::K);
        assert_eq!(transposed.chunk_count(), expected_bitslice_len);

        for (i_share, share) in original.iter().enumerate() {
            for i_bit in 0..transposed.num_bits() {
                let expected_a_bit = share.a.get_bit_as_bit(i_bit);
                let expected_b_bit = share.b.get_bit_as_bit(i_bit);
                let transposed_share_batch = i_share / T::K;
                let transposed_bit_index = i_share % T::K;
                let lane = transposed.lane(i_bit);
                let transposed_a_bit =
                    lane[transposed_share_batch].a.get_bit_as_bit(transposed_bit_index);
                let transposed_b_bit =
                    lane[transposed_share_batch].b.get_bit_as_bit(transposed_bit_index);
                assert_eq!(expected_a_bit, transposed_a_bit);
                assert_eq!(expected_b_bit, transposed_b_bit);
            }
        }
    }

    fn check_transposed_128<T: IntRing2k, U: IntRing2k>(
        transposed: Vec<VecShare<T>>,
        original: VecShare<U>,
    ) {
        assert_eq!(transposed.len(), U::K);
        let expected_bitslice_len = original.len().div_ceil(T::K);
        for slice in transposed.iter() {
            assert_eq!(slice.shares.len(), expected_bitslice_len);
        }

        for (i_share, share) in original.into_iter().enumerate() {
            for (i_bit, transposed_slice) in transposed.iter().enumerate() {
                let expected_a_bit = share.a.get_bit_as_bit(i_bit);
                let expected_b_bit = share.b.get_bit_as_bit(i_bit);
                let transposed_share_batch = i_share / T::K;
                let transposed_bit_index = i_share % T::K;
                let transposed_a_bit = transposed_slice.shares[transposed_share_batch]
                    .a
                    .get_bit_as_bit(transposed_bit_index);
                let transposed_b_bit = transposed_slice.shares[transposed_share_batch]
                    .b
                    .get_bit_as_bit(transposed_bit_index);
                assert_eq!(expected_a_bit, transposed_a_bit);
                assert_eq!(expected_b_bit, transposed_b_bit);
            }
        }
    }

    #[test]
    fn test_u16_transpose_pack_u64() {
        let mut rng = rand::thread_rng();
        let shares: Vec<Share<u16>> = (0..100).map(|_| Share::new(rng.gen(), rng.gen())).collect();
        let vec_share = VecShare { shares };
        let transposed = vec_share.clone().transpose_pack_u64();

        check_transposed_flat(&transposed, &vec_share);
    }

    #[test]
    fn test_u32_transpose_pack_u64() {
        let mut rng = rand::thread_rng();
        let shares: Vec<Share<u32>> = (0..128).map(|_| Share::new(rng.gen(), rng.gen())).collect();
        let vec_share = VecShare { shares };
        let transposed = vec_share.clone().transpose_pack_u64();

        check_transposed_flat(&transposed, &vec_share);
    }

    #[test]
    fn test_u64_transpose_pack_u64() {
        let mut rng = rand::thread_rng();
        let shares: Vec<Share<u64>> = (0..129).map(|_| Share::new(rng.gen(), rng.gen())).collect();
        let vec_share = VecShare { shares };
        let transposed = vec_share.clone().transpose_pack_u64();

        check_transposed_flat(&transposed, &vec_share);
    }

    #[test]
    fn test_u16_transpose_pack_u128() {
        let mut rng = rand::thread_rng();
        let shares: Vec<Share<u16>> = (0..200).map(|_| Share::new(rng.gen(), rng.gen())).collect();
        let vec_share = VecShare { shares };
        let transposed = vec_share.clone().transpose_pack_u128();

        check_transposed_128(transposed, vec_share);
    }

    #[test]
    fn test_u32_transpose_pack_u128() {
        let mut rng = rand::thread_rng();
        let shares: Vec<Share<u32>> = (0..256).map(|_| Share::new(rng.gen(), rng.gen())).collect();
        let vec_share = VecShare { shares };
        let transposed = vec_share.clone().transpose_pack_u128();

        check_transposed_128(transposed, vec_share);
    }

    #[test]
    fn test_u64_transpose_pack_u128() {
        let mut rng = rand::thread_rng();
        let shares: Vec<Share<u64>> = (0..257).map(|_| Share::new(rng.gen(), rng.gen())).collect();
        let vec_share = VecShare { shares };
        let transposed = vec_share.clone().transpose_pack_u128();

        check_transposed_128(transposed, vec_share);
    }

    #[test]
    fn test_into_lanes_roundtrip() {
        let mut rng = rand::thread_rng();
        let shares: Vec<Share<u16>> = (0..200).map(|_| Share::new(rng.gen(), rng.gen())).collect();
        let vec_share = VecShare { shares };
        let transposed = vec_share.clone().transpose_pack_u64();
        let chunk_count = transposed.chunk_count();

        // into_lanes and back should preserve data
        let lanes = transposed.clone().into_lanes();
        assert_eq!(lanes.len(), 16);
        for (bit, lane) in lanes.iter().enumerate() {
            assert_eq!(lane.shares.as_slice(), transposed.lane(bit));
        }

        // pop_lane should give the last lane
        let mut transposed2 = transposed.clone();
        let last = transposed2.pop_lane().unwrap();
        assert_eq!(last.len(), chunk_count);
        assert_eq!(last.shares.as_slice(), transposed.lane(15));
        assert_eq!(transposed2.num_bits(), 15);
    }
}
