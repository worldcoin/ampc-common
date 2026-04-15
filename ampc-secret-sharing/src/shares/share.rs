use crate::shares::primefield::PrimeElement;

use super::{int_ring::IntRing2k, ring_impl::RingElement};
use itertools::{izip, Itertools};
use num_traits::{PrimInt, Zero};
use serde::{Deserialize, Serialize};
use std::ops::{
    Add, AddAssign, BitAnd, BitXor, BitXorAssign, Mul, MulAssign, Neg, Not, Shl, Shr, Sub,
    SubAssign,
};

/// Trait for representing a party role in the MPC protocol.
/// This allows the secret sharing code to work with different networking implementations.
pub trait Role {
    /// Returns the zero-indexed role index (0, 1, or 2)
    fn index(&self) -> usize;
}

#[derive(
    Clone, Copy, Debug, PartialEq, Default, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash,
)]
#[serde(bound = "")]
/// A replicated share of a value in a ring.
/// The value is shared among three parties, with each party holding two shares.
/// The shares are represented as a pair of [RingElement], where `a` is the share held by party i and `b` is the share held by party i-1 (mod 3).
pub struct ReplicatedShare<T: IntRing2k + Sized> {
    pub a: RingElement<T>,
    pub b: RingElement<T>,
}

impl<T: IntRing2k> ReplicatedShare<T> {
    pub fn new(a: RingElement<T>, b: RingElement<T>) -> Self {
        Self { a, b }
    }

    pub fn from_const<R: Role>(value: T, role: R) -> Self {
        let mut res = Self::zero();
        res.add_assign_const_role(value, role);
        res
    }

    pub fn add_assign_const_role<R: Role>(&mut self, other: T, role: R) {
        match role.index() {
            0 => self.a += RingElement(other),
            1 => self.b += RingElement(other),
            2 => {}
            _ => unimplemented!(),
        }
    }

    pub fn get_a(self) -> RingElement<T> {
        self.a
    }

    pub fn get_b(self) -> RingElement<T> {
        self.b
    }

    pub fn get_ab(self) -> (RingElement<T>, RingElement<T>) {
        (self.a, self.b)
    }

    pub fn get_ab_ref(&self) -> (RingElement<T>, RingElement<T>) {
        (self.a, self.b)
    }

    pub fn iter_from_iter_ab(
        a: impl Iterator<Item = RingElement<T>>,
        b: impl Iterator<Item = RingElement<T>>,
    ) -> impl Iterator<Item = ReplicatedShare<T>> {
        a.zip(b).map(|(a_, b_)| ReplicatedShare::new(a_, b_))
    }
}

impl<T: IntRing2k> Add<&Self> for ReplicatedShare<T> {
    type Output = Self;

    fn add(self, rhs: &Self) -> Self::Output {
        ReplicatedShare {
            a: self.a + rhs.a,
            b: self.b + rhs.b,
        }
    }
}

impl<T: IntRing2k> Add<Self> for ReplicatedShare<T> {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        ReplicatedShare {
            a: self.a + rhs.a,
            b: self.b + rhs.b,
        }
    }
}

impl<T: IntRing2k> Sub<Self> for ReplicatedShare<T> {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        ReplicatedShare {
            a: self.a - rhs.a,
            b: self.b - rhs.b,
        }
    }
}

impl<T: IntRing2k> Sub<&Self> for ReplicatedShare<T> {
    type Output = Self;

    fn sub(self, rhs: &Self) -> Self::Output {
        ReplicatedShare {
            a: self.a - rhs.a,
            b: self.b - rhs.b,
        }
    }
}

impl<T: IntRing2k> AddAssign<Self> for ReplicatedShare<T> {
    fn add_assign(&mut self, rhs: Self) {
        self.a += rhs.a;
        self.b += rhs.b;
    }
}

impl<T: IntRing2k> AddAssign<&Self> for ReplicatedShare<T> {
    fn add_assign(&mut self, rhs: &Self) {
        self.a += rhs.a;
        self.b += rhs.b;
    }
}

impl<T: IntRing2k> SubAssign for ReplicatedShare<T> {
    fn sub_assign(&mut self, rhs: Self) {
        self.a -= rhs.a;
        self.b -= rhs.b;
    }
}

impl<T: IntRing2k> SubAssign<&Self> for ReplicatedShare<T> {
    fn sub_assign(&mut self, rhs: &Self) {
        self.a -= rhs.a;
        self.b -= rhs.b;
    }
}

impl<T: IntRing2k> Mul<RingElement<T>> for ReplicatedShare<T> {
    type Output = Self;

    fn mul(self, rhs: RingElement<T>) -> Self::Output {
        ReplicatedShare {
            a: self.a * rhs,
            b: self.b * rhs,
        }
    }
}

impl<T: IntRing2k> Mul<T> for ReplicatedShare<T> {
    type Output = Self;

    fn mul(self, rhs: T) -> Self::Output {
        self * RingElement(rhs)
    }
}

impl<T: IntRing2k> Mul<T> for &ReplicatedShare<T> {
    type Output = ReplicatedShare<T>;

    fn mul(self, rhs: T) -> Self::Output {
        ReplicatedShare {
            a: self.a * rhs,
            b: self.b * rhs,
        }
    }
}

impl<T: IntRing2k> MulAssign<T> for ReplicatedShare<T> {
    fn mul_assign(&mut self, rhs: T) {
        self.a *= rhs;
        self.b *= rhs;
    }
}

/// This is only the local part of the multiplication (so without randomness and
/// without communication)!
impl<T: IntRing2k> Mul<Self> for &ReplicatedShare<T> {
    type Output = RingElement<T>;

    fn mul(self, rhs: Self) -> Self::Output {
        self.a * rhs.a + self.b * rhs.a + self.a * rhs.b
    }
}

impl<T: IntRing2k> BitXor<Self> for &ReplicatedShare<T> {
    type Output = ReplicatedShare<T>;

    fn bitxor(self, rhs: Self) -> Self::Output {
        ReplicatedShare {
            a: self.a ^ rhs.a,
            b: self.b ^ rhs.b,
        }
    }
}

impl<T: IntRing2k> BitXor<Self> for ReplicatedShare<T> {
    type Output = Self;

    fn bitxor(self, rhs: Self) -> Self::Output {
        ReplicatedShare {
            a: self.a ^ rhs.a,
            b: self.b ^ rhs.b,
        }
    }
}

impl<T: IntRing2k> BitXor<&Self> for ReplicatedShare<T> {
    type Output = Self;

    fn bitxor(self, rhs: &Self) -> Self::Output {
        ReplicatedShare {
            a: self.a ^ rhs.a,
            b: self.b ^ rhs.b,
        }
    }
}

impl<T: IntRing2k> BitXorAssign<&Self> for ReplicatedShare<T> {
    fn bitxor_assign(&mut self, rhs: &Self) {
        self.a ^= rhs.a;
        self.b ^= rhs.b;
    }
}

impl<T: IntRing2k> BitXorAssign<Self> for ReplicatedShare<T> {
    fn bitxor_assign(&mut self, rhs: Self) {
        self.a ^= rhs.a;
        self.b ^= rhs.b;
    }
}

/// This is only the local part of the AND (so without randomness and without
/// communication)!
impl<T: IntRing2k> BitAnd<Self> for &ReplicatedShare<T> {
    type Output = RingElement<T>;

    fn bitand(self, rhs: Self) -> Self::Output {
        (self.a & rhs.a) ^ (self.b & rhs.a) ^ (self.a & rhs.b)
    }
}

impl<T: IntRing2k> BitAnd<&RingElement<T>> for &ReplicatedShare<T> {
    type Output = ReplicatedShare<T>;

    fn bitand(self, rhs: &RingElement<T>) -> Self::Output {
        ReplicatedShare {
            a: self.a & rhs,
            b: self.b & rhs,
        }
    }
}

impl<T: IntRing2k> BitAnd<T> for ReplicatedShare<T> {
    type Output = ReplicatedShare<T>;

    fn bitand(self, rhs: T) -> Self::Output {
        ReplicatedShare {
            a: self.a & rhs,
            b: self.b & rhs,
        }
    }
}

impl<T: IntRing2k> Zero for ReplicatedShare<T> {
    fn zero() -> Self {
        Self {
            a: RingElement::zero(),
            b: RingElement::zero(),
        }
    }

    fn is_zero(&self) -> bool {
        self.a.is_zero() && self.b.is_zero()
    }
}

impl<T: IntRing2k> Neg for ReplicatedShare<T> {
    type Output = Self;

    fn neg(self) -> Self::Output {
        Self {
            a: -self.a,
            b: -self.b,
        }
    }
}

impl<T: IntRing2k> Neg for &ReplicatedShare<T> {
    type Output = ReplicatedShare<T>;

    fn neg(self) -> Self::Output {
        ReplicatedShare {
            a: -self.a,
            b: -self.b,
        }
    }
}

// WARNING: This only works because there are three additive shares.
// NOT(b) = NOT(b_0 XOR b_1 XOR b_2)
// = b_0 XOR b_1 XOR b_2 XOR 1
// = b_0 XOR 1 XOR b_1 XOR 1 XOR b_2 XOR 1
// = NOT(b_0) XOR NOT(b_1) XOR NOT(b_2)
impl<T: IntRing2k> Not for &ReplicatedShare<T> {
    type Output = ReplicatedShare<T>;

    fn not(self) -> Self::Output {
        ReplicatedShare {
            a: !self.a,
            b: !self.b,
        }
    }
}

impl<T: IntRing2k> Shr<u32> for &ReplicatedShare<T> {
    type Output = ReplicatedShare<T>;

    fn shr(self, rhs: u32) -> Self::Output {
        ReplicatedShare {
            a: self.a >> rhs,
            b: self.b >> rhs,
        }
    }
}

impl<T: IntRing2k> Shl<u32> for ReplicatedShare<T> {
    type Output = Self;

    fn shl(self, rhs: u32) -> Self::Output {
        Self {
            a: self.a << rhs,
            b: self.b << rhs,
        }
    }
}

impl<T: IntRing2k> Shl<u32> for &ReplicatedShare<T> {
    type Output = ReplicatedShare<T>;

    fn shl(self, rhs: u32) -> Self::Output {
        ReplicatedShare {
            a: self.a << rhs,
            b: self.b << rhs,
        }
    }
}

/// Additive share of a relative Hamming distance.
/// The distance is represented as a pair of shares `(code_dot, mask_dot)`, where
/// - `code_dot` is the number of matching unmasked iris bits minus the number of non-matching unmasked iris bits,
/// - `mask_dot` is the number of common unmasked bits.
///
/// The greater the ratio `code_dot / mask_dot`, the more similar the irises are.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct DistanceShare<T: IntRing2k> {
    pub code_dot: ReplicatedShare<T>,
    pub mask_dot: ReplicatedShare<T>,
}

impl<T> DistanceShare<T>
where
    T: IntRing2k,
{
    pub fn new(code_dot: ReplicatedShare<T>, mask_dot: ReplicatedShare<T>) -> Self {
        DistanceShare { code_dot, mask_dot }
    }
}

impl<T: IntRing2k> Add<&Self> for DistanceShare<T> {
    type Output = Self;

    fn add(self, rhs: &Self) -> Self::Output {
        DistanceShare {
            code_dot: self.code_dot + rhs.code_dot,
            mask_dot: self.mask_dot + rhs.mask_dot,
        }
    }
}

impl<T: IntRing2k> Add<Self> for DistanceShare<T> {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        DistanceShare {
            code_dot: self.code_dot + rhs.code_dot,
            mask_dot: self.mask_dot + rhs.mask_dot,
        }
    }
}

impl<T: IntRing2k> AddAssign<&Self> for DistanceShare<T> {
    fn add_assign(&mut self, rhs: &Self) {
        self.code_dot += &rhs.code_dot;
        self.mask_dot += &rhs.mask_dot;
    }
}

/// Reconstructs a vector of DistanceShare from replicated shares
/// Used in iris-mpc protocol operations
pub fn reconstruct_distance_vector(
    a: super::ring_impl::VecRingElement<u32>,
    b: super::ring_impl::VecRingElement<u32>,
) -> Vec<DistanceShare<u32>> {
    izip!(a.0, b.0)
        .map(|(a, b)| ReplicatedShare::new(a, b))
        .tuples()
        .map(|(code_dot, mask_dot)| DistanceShare::new(code_dot, mask_dot))
        .collect_vec()
}

pub fn reconstruct_id_distance_vector(
    a: super::ring_impl::VecRingElement<u32>,
    b: super::ring_impl::VecRingElement<u32>,
) -> Vec<(ReplicatedShare<u32>, DistanceShare<u32>)> {
    izip!(a.0, b.0)
        .map(|(a, b)| ReplicatedShare::new(a, b))
        .tuples()
        .map(|(id, code_dot, mask_dot)| {
            let dist_share = DistanceShare::new(code_dot, mask_dot);
            (id, dist_share)
        })
        .collect_vec()
}

#[derive(
    Clone, Copy, Debug, PartialEq, Default, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash,
)]
#[serde(bound = "")]
/// An additive share of a value in the ring.
/// The value is shared among two parties, with each party holding one share.
/// The sum of the shares is the secret.
pub struct AdditiveShare<T: IntRing2k + Sized> {
    pub value: RingElement<T>,
}

impl<T: IntRing2k> AdditiveShare<T> {
    pub fn new(value: RingElement<T>) -> Self {
        Self { value }
    }

    pub fn zero() -> Self {
        Self {
            value: RingElement::<T>::zero(),
        }
    }

    pub fn from_const<R: Role>(value: T, role: R) -> Self {
        let mut res = Self::zero();
        res.add_assign_const_role(value, role);
        res
    }

    pub fn add_assign_const_role<R: Role>(&mut self, other: T, role: R) {
        match role.index() {
            0 => self.value += RingElement(other),
            1 => {}
            2 => {}
            _ => unimplemented!(),
        }
    }

    pub fn get_value(self) -> RingElement<T> {
        self.value
    }

    pub fn iter_from_iter_value(
        value_iter: impl Iterator<Item = RingElement<T>>,
    ) -> impl Iterator<Item = AdditiveShare<T>> {
        value_iter.map(|value| AdditiveShare::new(value))
    }
}

impl<T: IntRing2k> Add<&Self> for AdditiveShare<T> {
    type Output = Self;

    fn add(self, rhs: &Self) -> Self::Output {
        AdditiveShare {
            value: self.value + rhs.value,
        }
    }
}

impl<T: IntRing2k> Add<Self> for AdditiveShare<T> {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        AdditiveShare {
            value: self.value + rhs.value,
        }
    }
}

impl<T: IntRing2k> Sub<Self> for AdditiveShare<T> {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        AdditiveShare {
            value: self.value - rhs.value,
        }
    }
}

impl<T: IntRing2k> Sub<&Self> for AdditiveShare<T> {
    type Output = Self;

    fn sub(self, rhs: &Self) -> Self::Output {
        AdditiveShare {
            value: self.value - rhs.value,
        }
    }
}

impl<T: IntRing2k> AddAssign<Self> for AdditiveShare<T> {
    fn add_assign(&mut self, rhs: Self) {
        self.value += rhs.value;
    }
}

impl<T: IntRing2k> AddAssign<&Self> for AdditiveShare<T> {
    fn add_assign(&mut self, rhs: &Self) {
        self.value += rhs.value;
    }
}

impl<T: IntRing2k> SubAssign for AdditiveShare<T> {
    fn sub_assign(&mut self, rhs: Self) {
        self.value -= rhs.value;
    }
}

impl<T: IntRing2k> SubAssign<&Self> for AdditiveShare<T> {
    fn sub_assign(&mut self, rhs: &Self) {
        self.value -= rhs.value;
    }
}

impl<T: IntRing2k> Mul<RingElement<T>> for AdditiveShare<T> {
    type Output = Self;

    fn mul(self, rhs: RingElement<T>) -> Self::Output {
        AdditiveShare {
            value: self.value * rhs,
        }
    }
}

impl<T: IntRing2k> Mul<T> for AdditiveShare<T> {
    type Output = Self;

    fn mul(self, rhs: T) -> Self::Output {
        self * RingElement(rhs)
    }
}

impl<T: IntRing2k> Mul<T> for &AdditiveShare<T> {
    type Output = AdditiveShare<T>;

    fn mul(self, rhs: T) -> Self::Output {
        AdditiveShare {
            value: self.value * rhs,
        }
    }
}

impl<T: IntRing2k> MulAssign<T> for AdditiveShare<T> {
    fn mul_assign(&mut self, rhs: T) {
        self.value *= rhs;
    }
}

impl<T: IntRing2k> BitXor<Self> for &AdditiveShare<T> {
    type Output = AdditiveShare<T>;

    fn bitxor(self, rhs: Self) -> Self::Output {
        AdditiveShare {
            value: self.value ^ rhs.value,
        }
    }
}

impl<T: IntRing2k> BitXor<Self> for AdditiveShare<T> {
    type Output = Self;

    fn bitxor(self, rhs: Self) -> Self::Output {
        AdditiveShare {
            value: self.value ^ rhs.value,
        }
    }
}

impl<T: IntRing2k> BitXor<&Self> for AdditiveShare<T> {
    type Output = Self;

    fn bitxor(self, rhs: &Self) -> Self::Output {
        AdditiveShare {
            value: self.value ^ rhs.value,
        }
    }
}

impl<T: IntRing2k> BitXorAssign<&Self> for AdditiveShare<T> {
    fn bitxor_assign(&mut self, rhs: &Self) {
        self.value ^= rhs.value;
    }
}

impl<T: IntRing2k> BitXorAssign<Self> for AdditiveShare<T> {
    fn bitxor_assign(&mut self, rhs: Self) {
        self.value ^= rhs.value;
    }
}

impl<T: IntRing2k> BitAnd<&RingElement<T>> for &AdditiveShare<T> {
    type Output = AdditiveShare<T>;

    fn bitand(self, rhs: &RingElement<T>) -> Self::Output {
        AdditiveShare {
            value: self.value & rhs,
        }
    }
}

impl<T: IntRing2k> BitAnd<T> for AdditiveShare<T> {
    type Output = AdditiveShare<T>;

    fn bitand(self, rhs: T) -> Self::Output {
        AdditiveShare {
            value: self.value & rhs,
        }
    }
}

impl<T: IntRing2k> Zero for AdditiveShare<T> {
    fn zero() -> Self {
        Self {
            value: RingElement::zero(),
        }
    }

    fn is_zero(&self) -> bool {
        self.value.is_zero()
    }
}

impl<T: IntRing2k> Neg for AdditiveShare<T> {
    type Output = Self;

    fn neg(self) -> Self::Output {
        Self { value: -self.value }
    }
}

impl<T: IntRing2k> Neg for &AdditiveShare<T> {
    type Output = AdditiveShare<T>;

    fn neg(self) -> Self::Output {
        AdditiveShare { value: -self.value }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct AdditiveSharePrime<PrimeElement> {
    pub value: PrimeElement,
}

impl<T: PrimInt> AdditiveSharePrime<PrimeElement<T>> {
    pub fn new(value: PrimeElement<T>) -> Self {
        Self { value }
    }

    pub fn from_const<R: Role>(value: PrimeElement<T>, role: R, modulus: T) -> Self {
        let mut res = Self::zero(modulus);
        res.add_assign_const_role(value, role);
        res
    }

    pub fn add_assign_const_role<R: Role>(&mut self, other: PrimeElement<T>, role: R) {
        match role.index() {
            0 => self.value += other,
            1 => {}
            2 => {}
            _ => unimplemented!(),
        }
    }

    pub fn get_value(self) -> PrimeElement<T> {
        self.value
    }

    pub fn iter_from_iter_value(
        value_iter: impl Iterator<Item = PrimeElement<T>>,
    ) -> impl Iterator<Item = AdditiveSharePrime<PrimeElement<T>>> {
        value_iter.map(|value| AdditiveSharePrime::new(value))
    }

    pub fn zero(modulus: T) -> Self {
        Self {
            value: PrimeElement::<T>::zero(modulus),
        }
    }

    pub fn is_zero(&self) -> bool {
        self.value.is_zero()
    }
}

impl<T: PrimInt> Add<&Self> for AdditiveSharePrime<PrimeElement<T>> {
    type Output = Self;

    fn add(self, rhs: &Self) -> Self::Output {
        AdditiveSharePrime {
            value: self.value + rhs.value,
        }
    }
}

impl<T: PrimInt> Add<Self> for AdditiveSharePrime<PrimeElement<T>> {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        AdditiveSharePrime {
            value: self.value + rhs.value,
        }
    }
}

impl<T: PrimInt> Sub<Self> for AdditiveSharePrime<PrimeElement<T>> {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        AdditiveSharePrime {
            value: self.value - rhs.value,
        }
    }
}

impl<T: PrimInt> Sub<&Self> for AdditiveSharePrime<PrimeElement<T>> {
    type Output = Self;

    fn sub(self, rhs: &Self) -> Self::Output {
        AdditiveSharePrime {
            value: self.value - rhs.value,
        }
    }
}

impl<T: PrimInt> AddAssign<Self> for AdditiveSharePrime<PrimeElement<T>> {
    fn add_assign(&mut self, rhs: Self) {
        self.value += rhs.value;
    }
}

impl<T: PrimInt> AddAssign<&Self> for AdditiveSharePrime<PrimeElement<T>> {
    fn add_assign(&mut self, rhs: &Self) {
        self.value += rhs.value;
    }
}

impl<T: PrimInt> SubAssign for AdditiveSharePrime<PrimeElement<T>> {
    fn sub_assign(&mut self, rhs: Self) {
        self.value -= rhs.value;
    }
}

impl<T: PrimInt> SubAssign<&Self> for AdditiveSharePrime<PrimeElement<T>> {
    fn sub_assign(&mut self, rhs: &Self) {
        self.value -= rhs.value;
    }
}

impl<T: PrimInt> Mul<PrimeElement<T>> for AdditiveSharePrime<PrimeElement<T>> {
    type Output = Self;

    fn mul(self, rhs: PrimeElement<T>) -> Self::Output {
        AdditiveSharePrime {
            value: self.value * rhs,
        }
    }
}

impl<T: PrimInt> Mul<PrimeElement<T>> for &AdditiveSharePrime<PrimeElement<T>> {
    type Output = AdditiveSharePrime<PrimeElement<T>>;

    fn mul(self, rhs: PrimeElement<T>) -> Self::Output {
        AdditiveSharePrime {
            value: self.value * rhs,
        }
    }
}

impl<T: PrimInt> MulAssign<PrimeElement<T>> for AdditiveSharePrime<PrimeElement<T>> {
    fn mul_assign(&mut self, rhs: PrimeElement<T>) {
        self.value *= rhs;
    }
}

impl<T: PrimInt> Neg for AdditiveSharePrime<PrimeElement<T>> {
    type Output = Self;

    fn neg(self) -> Self::Output {
        Self { value: -self.value }
    }
}

impl<T: PrimInt> Neg for &AdditiveSharePrime<PrimeElement<T>> {
    type Output = AdditiveSharePrime<PrimeElement<T>>;

    fn neg(self) -> Self::Output {
        AdditiveSharePrime { value: -self.value }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shares::bit::Bit;

    use aes_prng::AesRng;
    use itertools::izip;
    use rand::{Rng, SeedableRng};
    use rand_distr::{Distribution, Standard};
    use std::fmt::Debug;

    // Simple Role implementation for tests
    struct TestRole(usize);
    impl Role for TestRole {
        fn index(&self) -> usize {
            self.0
        }
    }

    fn get_shares<T: IntRing2k>(value: T, bitwise: bool) -> Vec<ReplicatedShare<T>>
    where
        Standard: Distribution<T>,
    {
        let mut rng = AesRng::from_entropy();
        let b = RingElement(rng.gen());
        let c = RingElement(rng.gen());
        let a = if bitwise {
            RingElement(value) ^ b ^ c
        } else {
            RingElement(value) - b - c
        };
        vec![
            ReplicatedShare::new(a, c),
            ReplicatedShare::new(b, a),
            ReplicatedShare::new(c, b),
        ]
    }

    fn reconstruct_shares<T: IntRing2k>(shares: Vec<ReplicatedShare<T>>) -> RingElement<T> {
        shares[0].a + shares[1].a + shares[2].a
    }

    fn reconstruct_bit_shares<T: IntRing2k>(shares: Vec<ReplicatedShare<T>>) -> RingElement<T> {
        shares[0].a ^ shares[1].a ^ shares[2].a
    }

    fn reconstruct_mul_shares<T: IntRing2k>(shares: Vec<RingElement<T>>) -> RingElement<T> {
        shares[0] + shares[1] + shares[2]
    }

    fn reconstruct_mul_bit_shares<T: IntRing2k>(shares: Vec<RingElement<T>>) -> RingElement<T> {
        shares[0] ^ shares[1] ^ shares[2]
    }

    fn arithmetic_test<T: IntRing2k>()
    where
        Standard: Distribution<T>,
    {
        let mut rng = AesRng::from_entropy();
        let a_t: T = rng.gen();
        let b_t: T = rng.gen();

        let a = get_shares(a_t, false);
        let b = get_shares(b_t, false);

        // Addition
        let expected_add = RingElement(a_t.wrapping_add(&b_t));
        let mut c = izip!(a.clone(), b.clone())
            .map(|(a, b)| a + b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_shares(c), expected_add);

        c = izip!(a.clone(), b.iter())
            .map(|(a, b)| a + b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_shares(c), expected_add);

        c = a.clone();
        c.iter_mut().zip(b.iter()).for_each(|(a, b)| *a += b);
        assert_eq!(reconstruct_shares(c), expected_add);

        c = a.clone();
        c.iter_mut()
            .zip(b.iter().cloned())
            .for_each(|(a, b)| *a += b);
        assert_eq!(reconstruct_shares(c), expected_add);

        // Addition with a constant
        c = a.clone();
        c.iter_mut()
            .enumerate()
            .for_each(|(i, a)| a.add_assign_const_role(T::one(), TestRole(i)));
        assert_eq!(
            reconstruct_shares(c),
            RingElement(a_t.wrapping_add(&T::one()))
        );

        // Subtraction
        let expected_sub = RingElement(a_t.wrapping_sub(&b_t));
        let mut c = izip!(a.clone(), b.clone())
            .map(|(a, b)| a - b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_shares(c), expected_sub);

        c = izip!(a.clone(), b.iter())
            .map(|(a, b)| a - b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_shares(c), expected_sub);

        c = a.clone();
        c.iter_mut().zip(b.iter()).for_each(|(a, b)| *a -= b);
        assert_eq!(reconstruct_shares(c), expected_sub);

        c = a.clone();
        c.iter_mut()
            .zip(b.iter().cloned())
            .for_each(|(a, b)| *a -= b);
        assert_eq!(reconstruct_shares(c), expected_sub);

        // Multiplication
        let expected_mul = RingElement(a_t.wrapping_mul(&b_t));
        let c = izip!(a.iter(), b.iter())
            .map(|(a, b)| a * b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_mul_shares(c), expected_mul);

        // Multiplication with a constant
        let mut c = a.iter().map(|a| a * b_t).collect::<Vec<_>>();
        assert_eq!(reconstruct_shares(c), expected_mul);
        c = a.iter().cloned().map(|a| a * b_t).collect::<Vec<_>>();
        assert_eq!(reconstruct_shares(c), expected_mul);
        c = a
            .iter()
            .cloned()
            .map(|a| a * RingElement(b_t))
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_shares(c), expected_mul);
        c = a.clone();
        c.iter_mut().for_each(|a| *a *= b_t);
        assert_eq!(reconstruct_shares(c), expected_mul);

        // Negation
        let expected_neg = -RingElement(a_t);
        let mut c = a.iter().map(|a| -a).collect::<Vec<_>>();
        assert_eq!(reconstruct_shares(c), expected_neg);
        c = a.iter().cloned().map(|a| -a).collect::<Vec<_>>();
        assert_eq!(reconstruct_shares(c), expected_neg);

        let a = get_shares(a_t, true);
        let b = get_shares(b_t, true);

        // XOR
        let expected_xor = RingElement(a_t ^ b_t);
        let mut c = izip!(a.clone(), b.clone())
            .map(|(a, b)| a ^ b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_bit_shares(c), expected_xor);

        c = izip!(a.clone(), b.iter())
            .map(|(a, b)| a ^ b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_bit_shares(c), expected_xor);

        c = izip!(a.iter(), b.iter())
            .map(|(a, b)| a ^ b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_bit_shares(c), expected_xor);

        c = a.clone();
        c.iter_mut().zip(b.iter()).for_each(|(a, b)| *a ^= b);
        assert_eq!(reconstruct_bit_shares(c), expected_xor);

        c = a.clone();
        c.iter_mut()
            .zip(b.iter().cloned())
            .for_each(|(a, b)| *a ^= b);
        assert_eq!(reconstruct_bit_shares(c), expected_xor);

        // AND
        let expected_and = RingElement(a_t & b_t);
        let c = izip!(a.iter(), b.iter())
            .map(|(a, b)| a & b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_mul_bit_shares(c), expected_and);
        let mut c = a.iter().cloned().map(|a| a & b_t).collect::<Vec<_>>();
        assert_eq!(reconstruct_bit_shares(c), expected_and);
        c = a.iter().map(|a| a & &RingElement(b_t)).collect::<Vec<_>>();
        assert_eq!(reconstruct_bit_shares(c), expected_and);

        // NOT
        let expected_not = RingElement(!a_t);
        c = a.iter().map(|a| !a).collect::<Vec<_>>();
        assert_eq!(reconstruct_bit_shares(c), expected_not);

        // Shift
        let expected_shl = RingElement(a_t << 1);
        let mut c = a.iter().cloned().map(|a| a << 1).collect::<Vec<_>>();
        assert_eq!(reconstruct_bit_shares(c), expected_shl);
        let expected_shr = RingElement(a_t >> 1);
        c = a.iter().map(|a| a >> 1).collect::<Vec<_>>();
        assert_eq!(reconstruct_bit_shares(c), expected_shr);
    }

    fn identity_test<T: IntRing2k>() {
        let a: ReplicatedShare<T> = ReplicatedShare::zero();
        assert_eq!(a.a, RingElement::zero());
        assert_eq!(a.b, RingElement::zero());
        assert!(a.is_zero());
    }

    macro_rules! test_impl {
        ($([$ty:ty,$fn:ident]),*) => ($(
            #[test]
            fn $fn() {
                arithmetic_test::<$ty>();
                identity_test::<$ty>();
            }
        )*)
    }

    test_impl! {
        [Bit, bit_test],
        [u8, u8_test],
        [u16, u16_test],
        [u32, u32_test],
        [u64, u64_test],
        [u128, u128_test]
    }

    fn get_additive_shares<T: IntRing2k>(value: T) -> Vec<AdditiveShare<T>>
    where
        Standard: Distribution<T>,
    {
        let mut rng = AesRng::from_entropy();
        let next = RingElement(rng.gen());
        let first = RingElement(value) - next;
        vec![AdditiveShare::new(first), AdditiveShare::new(next)]
    }

    fn reconstruct_additive_shares<T: IntRing2k>(shares: Vec<AdditiveShare<T>>) -> RingElement<T> {
        shares[0].value + shares[1].value
    }

    fn arithmetic_test_additive<T: IntRing2k>()
    where
        Standard: Distribution<T>,
    {
        let mut rng = AesRng::from_entropy();
        let a_t: T = rng.gen();
        let b_t: T = rng.gen();

        let a = get_additive_shares(a_t);
        let b = get_additive_shares(b_t);

        // Addition
        let expected_add = RingElement(a_t.wrapping_add(&b_t));
        let mut c = izip!(a.clone(), b.clone())
            .map(|(a, b)| a + b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares(c), expected_add);

        c = izip!(a.clone(), b.iter())
            .map(|(a, b)| a + b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares(c), expected_add);

        c = a.clone();
        c.iter_mut().zip(b.iter()).for_each(|(a, b)| *a += b);
        assert_eq!(reconstruct_additive_shares(c), expected_add);

        c = a.clone();
        c.iter_mut()
            .zip(b.iter().cloned())
            .for_each(|(a, b)| *a += b);
        assert_eq!(reconstruct_additive_shares(c), expected_add);

        // Addition with a constant
        c = a.clone();
        c.iter_mut()
            .enumerate()
            .for_each(|(i, a)| a.add_assign_const_role(T::one(), TestRole(i)));
        assert_eq!(
            reconstruct_additive_shares(c),
            RingElement(a_t.wrapping_add(&T::one()))
        );

        // Subtraction
        let expected_sub = RingElement(a_t.wrapping_sub(&b_t));
        let mut c = izip!(a.clone(), b.clone())
            .map(|(a, b)| a - b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares(c), expected_sub);

        c = izip!(a.clone(), b.iter())
            .map(|(a, b)| a - b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares(c), expected_sub);

        c = a.clone();
        c.iter_mut().zip(b.iter()).for_each(|(a, b)| *a -= b);
        assert_eq!(reconstruct_additive_shares(c), expected_sub);

        c = a.clone();
        c.iter_mut()
            .zip(b.iter().cloned())
            .for_each(|(a, b)| *a -= b);
        assert_eq!(reconstruct_additive_shares(c), expected_sub);

        // Multiplication
        let expected_mul = RingElement(a_t.wrapping_mul(&b_t));

        // Multiplication with a constant
        let mut c = a.iter().map(|a| a * b_t).collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares(c), expected_mul);
        c = a.iter().cloned().map(|a| a * b_t).collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares(c), expected_mul);
        c = a
            .iter()
            .cloned()
            .map(|a| a * RingElement(b_t))
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares(c), expected_mul);
        c = a.clone();
        c.iter_mut().for_each(|a| *a *= b_t);
        assert_eq!(reconstruct_additive_shares(c), expected_mul);

        // Negation
        let expected_neg = -RingElement(a_t);
        let mut c = a.iter().map(|a| -a).collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares(c), expected_neg);
        c = a.iter().cloned().map(|a| -a).collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares(c), expected_neg);
    }

    fn identity_test_additive<T: IntRing2k>() {
        let a: AdditiveShare<T> = AdditiveShare::zero();
        assert_eq!(a.value, RingElement::zero());
        assert!(a.is_zero());
    }

    macro_rules! test_impl_additive {
        ($([$ty:ty,$fn:ident]),*) => ($(
            #[test]
            fn $fn() {
                arithmetic_test_additive::<$ty>();
                identity_test_additive::<$ty>();
            }
        )*)
    }

    test_impl_additive! {
        [u8, u8_additive_test]
    }

    fn get_additive_shares_prime<T: PrimInt>(
        value: T,
        modulus: T,
    ) -> Vec<AdditiveSharePrime<PrimeElement<T>>> {
        let mut rng = AesRng::from_entropy();
        let next = PrimeElement::<T>::rand(&mut rng, modulus);
        let first = PrimeElement::<T>::new(value, modulus) - next;
        vec![
            AdditiveSharePrime::new(first),
            AdditiveSharePrime::new(next),
        ]
    }

    fn reconstruct_additive_shares_prime<T: PrimInt>(
        shares: Vec<AdditiveSharePrime<PrimeElement<T>>>,
    ) -> PrimeElement<T> {
        shares[0].value + shares[1].value
    }

    fn arithmetic_test_additive_prime<T: PrimInt + Debug>(modulus: T) {
        let mut rng = AesRng::from_entropy();
        let a_t = T::from(rng.gen_range(0..(modulus.to_u32().unwrap()))).unwrap();
        let b_t = T::from(rng.gen_range(0..(modulus.to_u32().unwrap()))).unwrap();

        let a = get_additive_shares_prime(a_t, modulus);
        let b = get_additive_shares_prime(b_t, modulus);

        // Addition
        let expected_add = PrimeElement::<T>::new((a_t + b_t) % modulus, modulus);
        let mut c = izip!(a.clone(), b.clone())
            .map(|(a, b)| a + b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares_prime(c), expected_add);

        c = izip!(a.clone(), b.iter())
            .map(|(a, b)| a + b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares_prime(c), expected_add);

        c = a.clone();
        c.iter_mut().zip(b.iter()).for_each(|(a, b)| *a += b);
        assert_eq!(reconstruct_additive_shares_prime(c), expected_add);

        c = a.clone();
        c.iter_mut()
            .zip(b.iter().cloned())
            .for_each(|(a, b)| *a += b);
        assert_eq!(reconstruct_additive_shares_prime(c), expected_add);

        // Addition with a constant
        c = a.clone();
        c.iter_mut()
            .enumerate()
            .for_each(|(i, a)| a.add_assign_const_role(PrimeElement::one(modulus), TestRole(i)));
        assert_eq!(
            reconstruct_additive_shares_prime(c),
            PrimeElement::<T>::new((a_t + T::one()) % modulus, modulus)
        );

        // Subtraction
        let expected_sub =
            PrimeElement::<T>::new(a_t, modulus) - PrimeElement::<T>::new(b_t, modulus);
        let mut c = izip!(a.clone(), b.clone())
            .map(|(a, b)| a - b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares_prime(c), expected_sub);

        c = izip!(a.clone(), b.iter())
            .map(|(a, b)| a - b)
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares_prime(c), expected_sub);

        c = a.clone();
        c.iter_mut().zip(b.iter()).for_each(|(a, b)| *a -= b);
        assert_eq!(reconstruct_additive_shares_prime(c), expected_sub);

        c = a.clone();
        c.iter_mut()
            .zip(b.iter().cloned())
            .for_each(|(a, b)| *a -= b);
        assert_eq!(reconstruct_additive_shares_prime(c), expected_sub);

        // Multiplication
        let expected_mul =
            PrimeElement::<T>::new(a_t, modulus) * PrimeElement::<T>::new(b_t, modulus);

        // Multiplication with a constant
        let mut c = a
            .iter()
            .map(|a| a * PrimeElement::<T>::new(b_t, modulus))
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares_prime(c), expected_mul);
        c = a
            .iter()
            .cloned()
            .map(|a| a * PrimeElement::<T>::new(b_t, modulus))
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares_prime(c), expected_mul);
        c = a
            .iter()
            .cloned()
            .map(|a| a * PrimeElement::<T>::new(b_t, modulus))
            .collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares_prime(c), expected_mul);
        c = a.clone();
        c.iter_mut()
            .for_each(|a| *a *= PrimeElement::<T>::new(b_t, modulus));
        assert_eq!(reconstruct_additive_shares_prime(c), expected_mul);

        // Negation
        let expected_neg = -PrimeElement::new(a_t, modulus);
        let mut c = a.iter().map(|a| -a).collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares_prime(c), expected_neg);
        c = a.iter().cloned().map(|a| -a).collect::<Vec<_>>();
        assert_eq!(reconstruct_additive_shares_prime(c), expected_neg);
    }

    fn identity_test_additive_prime<T: PrimInt + Debug>(modulus: T) {
        let a: AdditiveSharePrime<PrimeElement<T>> = AdditiveSharePrime::zero(modulus);
        assert_eq!(a.value, PrimeElement::<T>::zero(modulus));
        assert!(a.is_zero());
    }

    macro_rules! test_impl_additive_prime_19 {
        ($([$ty:ty,$fn:ident]),*) => ($(
            #[test]
            fn $fn() {
                arithmetic_test_additive_prime(19);
                identity_test_additive_prime(19);
            }
        )*)
    }

    test_impl_additive_prime_19! {
        [u8, u8_additive_test_prime]
    }
}
