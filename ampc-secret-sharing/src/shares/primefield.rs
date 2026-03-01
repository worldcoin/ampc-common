use std::ops::{Add, AddAssign, Div, Mul, MulAssign, Neg, Sub, SubAssign};

use num_traits::{Inv, One, Zero};
use rand::{Rng, RngCore};

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub struct Mod19(u16);

impl Mod19 {
    pub fn new(value: u16) -> Self {
        Self(value % 19)
    }

    pub fn rand(rng: &mut impl Rng) -> Self {
        Self::new(rng.gen_range(0..19))
    }

    pub fn rand_multiplicative(rng: &mut impl Rng) -> Self {
        Self::new(rng.gen_range(1..19))
    }

    pub fn convert(&self) -> u16 {
        self.0
    }
}

impl One for Mod19 {
    fn one() -> Self {
        Self(1)
    }
}

impl Add for Mod19 {
    type Output = Mod19;

    fn add(self, other: Mod19) -> Self::Output {
        Mod19((self.0 + other.0) % 19)
    }
}

impl Mul for Mod19 {
    type Output = Mod19;

    fn mul(self, other: Mod19) -> Self::Output {
        Mod19((self.0 * other.0) % 19)
    }
}

impl Neg for Mod19 {
    type Output = Mod19;

    fn neg(self) -> Self::Output {
        Mod19(19 - self.0)
    }
}

impl Sub for Mod19 {
    type Output = Mod19;

    fn sub(self, rhs: Self) -> Self::Output {
        self + (-rhs)
    }
}

impl AddAssign for Mod19 {
    fn add_assign(&mut self, other: Mod19) {
        self.0 = (self.0 + other.0) % 19
    }
}

impl MulAssign for Mod19 {
    fn mul_assign(&mut self, other: Mod19) {
        self.0 = (self.0 * other.0) % 19
    }
}

impl SubAssign for Mod19 {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 = (self.0 + (19 - rhs.0)) % 19
    }
}

impl Inv for Mod19 {
    type Output = Mod19;

    fn inv(self) -> Self::Output {
        Mod19((self.0.pow(17)) % 19)
    }
}

impl Div for Mod19 {
    type Output = Mod19;

    fn div(self, rhs: Self) -> Self::Output {
        self * (rhs.inv())
    }
}

impl Zero for Mod19 {
    fn zero() -> Self {
        Mod19::new(0)
    }

    fn is_zero(&self) -> bool {
        self.0 % 19 == 0
    }
}
