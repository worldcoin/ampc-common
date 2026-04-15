use std::ops::{Add, AddAssign, Div, Mul, MulAssign, Neg, Sub, SubAssign};

use num_traits::{Inv, PrimInt};
use rand::Rng;

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub struct PrimeElement<T: PrimInt> {
    modulus: T,
    value: T,
}

impl<T: PrimInt> PrimeElement<T> {
    pub fn new(value: T, modulus: T) -> Self {
        let mod_value = value % modulus;
        Self {
            modulus,
            value: mod_value,
        }
    }

    pub fn zero(modulus: T) -> Self {
        Self {
            modulus,
            value: T::zero(),
        }
    }

    pub fn one(modulus: T) -> Self {
        Self {
            modulus,
            value: T::one(),
        }
    }

    pub fn is_zero(self) -> bool {
        return self.value == T::zero();
    }

    pub fn rand(rng: &mut impl Rng, modulus: T) -> Self {
        Self::new(
            T::from(rng.gen_range(0..modulus.to_u32().unwrap())).unwrap(),
            modulus,
        )
    }

    pub fn rand_multiplicative(rng: &mut impl Rng, modulus: T) -> Self {
        Self::new(
            T::from(rng.gen_range(1..modulus.to_u32().unwrap())).unwrap(),
            modulus,
        )
    }

    pub fn convert(&self) -> (T, T) {
        (self.value, self.modulus)
    }
}

impl<T: PrimInt> Add for PrimeElement<T> {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        assert!(self.modulus == rhs.modulus);
        PrimeElement::new(self.value + rhs.value, self.modulus)
    }
}

impl<T: PrimInt> Mul for PrimeElement<T> {
    type Output = Self;

    fn mul(self, rhs: Self) -> Self::Output {
        assert!(self.modulus == rhs.modulus);
        PrimeElement::new(self.value * rhs.value, self.modulus)
    }
}

impl<T: PrimInt> Neg for PrimeElement<T> {
    type Output = Self;

    fn neg(self) -> Self::Output {
        PrimeElement::new(self.modulus - self.value, self.modulus)
    }
}

impl<T: PrimInt> Sub for PrimeElement<T> {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        self + (-rhs)
    }
}

impl<T: PrimInt> AddAssign for PrimeElement<T> {
    fn add_assign(&mut self, other: PrimeElement<T>) {
        assert!(self.modulus == other.modulus);
        self.value = (self.value + other.value) % self.modulus
    }
}

impl<T: PrimInt> MulAssign for PrimeElement<T> {
    fn mul_assign(&mut self, other: PrimeElement<T>) {
        assert!(self.modulus == other.modulus);
        self.value = (self.value * other.value) % self.modulus
    }
}

impl<T: PrimInt> SubAssign for PrimeElement<T> {
    fn sub_assign(&mut self, rhs: Self) {
        assert!(self.modulus == rhs.modulus);
        self.value = (self.value + (self.modulus - rhs.value)) % self.modulus
    }
}

impl<T: PrimInt> Inv for PrimeElement<T> {
    type Output = Self;

    fn inv(self) -> Self::Output {
        PrimeElement::new(
            (self
                .value
                .pow((self.modulus - T::from(2).unwrap()).to_u32().unwrap()))
                % self.modulus,
            self.modulus,
        )
    }
}

impl<T: PrimInt> Div for PrimeElement<T> {
    type Output = Self;

    fn div(self, rhs: Self) -> Self::Output {
        self * (rhs.inv())
    }
}
