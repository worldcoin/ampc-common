//! 48-bit ring element stored in a `u64`.
//!
//! All arithmetic wraps modulo 2^48. The inner `u64` is always kept
//! masked to the low 48 bits so that bit-slicing / MSB extraction
//! operates on exactly 48 bit-planes.

use num_traits::{
    AsPrimitive, One, WrappingAdd, WrappingMul, WrappingNeg, WrappingShl, WrappingShr, WrappingSub,
    Zero,
};
use rand::{
    distributions::{Distribution, Standard},
    Rng,
};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, BitXor, BitXorAssign, Neg, Not, Rem},
};

use super::int_ring::IntRing2k;

const MASK_48: u64 = (1u64 << 48) - 1;

/// A 48-bit unsigned ring element, stored in the low 48 bits of a `u64`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct Ring48(pub u64);

// SAFETY: Ring48 is repr(transparent) over u64 which satisfies both traits.
unsafe impl bytemuck::Zeroable for Ring48 {}
unsafe impl bytemuck::NoUninit for Ring48 {}
unsafe impl bytemuck::AnyBitPattern for Ring48 {}

impl Ring48 {
    pub const MASK: u64 = MASK_48;

    #[inline(always)]
    pub fn masked(v: u64) -> Self {
        Ring48(v & MASK_48)
    }
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

impl fmt::Display for Ring48 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// From / Into conversions
// ---------------------------------------------------------------------------

impl From<bool> for Ring48 {
    #[inline]
    fn from(b: bool) -> Self {
        Ring48(b as u64)
    }
}

impl From<u8> for Ring48 {
    #[inline]
    fn from(v: u8) -> Self {
        Ring48(v as u64)
    }
}

impl From<u16> for Ring48 {
    #[inline]
    fn from(v: u16) -> Self {
        Ring48(v as u64)
    }
}

impl From<u32> for Ring48 {
    #[inline]
    fn from(v: u32) -> Self {
        Ring48(v as u64)
    }
}

impl From<Ring48> for u128 {
    #[inline]
    fn from(v: Ring48) -> u128 {
        v.0 as u128
    }
}

// ---------------------------------------------------------------------------
// std::ops arithmetic (required by Zero/One traits)
// ---------------------------------------------------------------------------

#[allow(clippy::suspicious_arithmetic_impl)]
impl std::ops::Add for Ring48 {
    type Output = Self;
    #[inline(always)]
    fn add(self, rhs: Self) -> Self {
        Ring48((self.0.wrapping_add(rhs.0)) & MASK_48)
    }
}

#[allow(clippy::suspicious_arithmetic_impl)]
impl std::ops::Sub for Ring48 {
    type Output = Self;
    #[inline(always)]
    fn sub(self, rhs: Self) -> Self {
        Ring48((self.0.wrapping_sub(rhs.0)) & MASK_48)
    }
}

#[allow(clippy::suspicious_arithmetic_impl)]
impl std::ops::Mul for Ring48 {
    type Output = Self;
    #[inline(always)]
    fn mul(self, rhs: Self) -> Self {
        Ring48((self.0.wrapping_mul(rhs.0)) & MASK_48)
    }
}

// ---------------------------------------------------------------------------
// Wrapping arithmetic (num-traits)
// ---------------------------------------------------------------------------

impl WrappingAdd for Ring48 {
    #[inline(always)]
    fn wrapping_add(&self, rhs: &Self) -> Self {
        Ring48((self.0.wrapping_add(rhs.0)) & MASK_48)
    }
}

impl WrappingSub for Ring48 {
    #[inline(always)]
    fn wrapping_sub(&self, rhs: &Self) -> Self {
        Ring48((self.0.wrapping_sub(rhs.0)) & MASK_48)
    }
}

impl WrappingMul for Ring48 {
    #[inline(always)]
    fn wrapping_mul(&self, rhs: &Self) -> Self {
        Ring48((self.0.wrapping_mul(rhs.0)) & MASK_48)
    }
}

impl WrappingNeg for Ring48 {
    #[inline(always)]
    fn wrapping_neg(&self) -> Self {
        Ring48((self.0.wrapping_neg()) & MASK_48)
    }
}

impl WrappingShl for Ring48 {
    #[inline(always)]
    fn wrapping_shl(&self, rhs: u32) -> Self {
        Ring48((self.0.wrapping_shl(rhs)) & MASK_48)
    }
}

impl WrappingShr for Ring48 {
    #[inline(always)]
    fn wrapping_shr(&self, rhs: u32) -> Self {
        // No mask needed: shifting right can only reduce bits.
        Ring48(self.0.wrapping_shr(rhs))
    }
}

// ---------------------------------------------------------------------------
// Bitwise ops
// ---------------------------------------------------------------------------

impl Not for Ring48 {
    type Output = Self;
    #[inline(always)]
    fn not(self) -> Self {
        Ring48((!self.0) & MASK_48)
    }
}

impl BitXor for Ring48 {
    type Output = Self;
    #[inline(always)]
    fn bitxor(self, rhs: Self) -> Self {
        Ring48(self.0 ^ rhs.0)
    }
}

impl BitXorAssign for Ring48 {
    #[inline(always)]
    fn bitxor_assign(&mut self, rhs: Self) {
        self.0 ^= rhs.0;
    }
}

impl BitAnd for Ring48 {
    type Output = Self;
    #[inline(always)]
    fn bitand(self, rhs: Self) -> Self {
        Ring48(self.0 & rhs.0)
    }
}

impl BitAndAssign for Ring48 {
    #[inline(always)]
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl BitOr for Ring48 {
    type Output = Self;
    #[inline(always)]
    fn bitor(self, rhs: Self) -> Self {
        Ring48(self.0 | rhs.0)
    }
}

impl BitOrAssign for Ring48 {
    #[inline(always)]
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

// ---------------------------------------------------------------------------
// Zero / One
// ---------------------------------------------------------------------------

impl Zero for Ring48 {
    #[inline]
    fn zero() -> Self {
        Ring48(0)
    }
    #[inline]
    fn is_zero(&self) -> bool {
        self.0 == 0
    }
}

impl One for Ring48 {
    #[inline]
    fn one() -> Self {
        Ring48(1)
    }
}

// ---------------------------------------------------------------------------
// Rem
// ---------------------------------------------------------------------------

impl Rem for Ring48 {
    type Output = Self;
    #[inline]
    fn rem(self, rhs: Self) -> Self {
        Ring48(self.0 % rhs.0)
    }
}

// ---------------------------------------------------------------------------
// Shr / Shl via u32 (needed for RingElement<T> impls)
// These are NOT the wrapping variants – they are used by RingElement shift ops.
// ---------------------------------------------------------------------------

impl std::ops::Shr<usize> for Ring48 {
    type Output = Self;
    #[inline(always)]
    fn shr(self, rhs: usize) -> Self {
        Ring48(self.0 >> rhs)
    }
}

impl std::ops::Shl<usize> for Ring48 {
    type Output = Self;
    #[inline(always)]
    fn shl(self, rhs: usize) -> Self {
        Ring48((self.0 << rhs) & MASK_48)
    }
}

// ---------------------------------------------------------------------------
// Signed type for IntRing2k
// ---------------------------------------------------------------------------

/// Signed interpretation of a 48-bit value.
/// Uses bit 47 as the sign bit.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Signed48(pub i64);

impl Neg for Signed48 {
    type Output = Self;
    #[inline]
    fn neg(self) -> Self {
        Signed48(-self.0)
    }
}

impl From<bool> for Signed48 {
    #[inline]
    fn from(b: bool) -> Self {
        Signed48(b as i64)
    }
}

impl AsPrimitive<Ring48> for Signed48 {
    #[inline]
    fn as_(self) -> Ring48 {
        Ring48((self.0 as u64) & MASK_48)
    }
}

// ---------------------------------------------------------------------------
// IntRing2k
// ---------------------------------------------------------------------------

impl IntRing2k for Ring48 {
    type Signed = Signed48;
    const K: usize = 48;
    const BYTES: usize = 6;
}

impl super::ring_impl::RingRandFillable for Ring48 {
    #[inline]
    fn fill_vec_ring<R: Rng + ?Sized>(
        slice: &mut [super::ring_impl::RingElement<Self>],
        rng: &mut R,
    ) -> Result<(), rand::Error> {
        // Fast path: bulk fill as u64, then mask each element to 48 bits.
        // SAFETY: RingElement<Ring48> is repr(transparent) over Ring48 which is
        // repr(transparent) over u64, so the slice has the same memory layout as [u64].
        let len = slice.len();
        let raw = unsafe { std::slice::from_raw_parts_mut(slice.as_mut_ptr() as *mut u64, len) };
        rng.try_fill(raw)?;
        for elem in slice.iter_mut() {
            elem.0 .0 &= MASK_48;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Random generation
// ---------------------------------------------------------------------------

impl Distribution<Ring48> for Standard {
    #[inline]
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> Ring48 {
        Ring48(rng.gen::<u64>() & MASK_48)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring48_wrapping_add() {
        let a = Ring48(MASK_48);
        let b = Ring48(1);
        assert_eq!(a.wrapping_add(&b), Ring48(0));
    }

    #[test]
    fn test_ring48_wrapping_sub() {
        let a = Ring48(0);
        let b = Ring48(1);
        assert_eq!(a.wrapping_sub(&b), Ring48(MASK_48));
    }

    #[test]
    fn test_ring48_wrapping_mul() {
        let a = Ring48(1u64 << 47);
        let b = Ring48(2);
        assert_eq!(a.wrapping_mul(&b), Ring48(0));
    }

    #[test]
    fn test_ring48_wrapping_neg() {
        let a = Ring48(1);
        assert_eq!(a.wrapping_neg(), Ring48(MASK_48));
    }

    #[test]
    fn test_ring48_not() {
        let a = Ring48(0);
        assert_eq!(!a, Ring48(MASK_48));
    }

    #[test]
    fn test_ring48_from_bool() {
        assert_eq!(Ring48::from(true), Ring48(1));
        assert_eq!(Ring48::from(false), Ring48(0));
    }

    #[test]
    fn test_ring48_into_u128() {
        let a = Ring48(12345);
        let b: u128 = a.into();
        assert_eq!(b, 12345u128);
    }

    #[test]
    fn test_ring48_intring2k() {
        assert_eq!(Ring48::K, 48);
        assert_eq!(Ring48::BYTES, 6);
    }

    #[test]
    fn test_ring48_random_masked() {
        let mut rng = rand::thread_rng();
        for _ in 0..100 {
            let v: Ring48 = rng.gen();
            assert_eq!(v.0, v.0 & MASK_48, "Random Ring48 should be masked");
        }
    }
}
