use std::ops::{Add, Div, Mul};

use num_traits::Inv;

pub struct Mod19(u8);

impl Mod19 {
    fn new(value: u8) -> Self {
        Self(value % 19)
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
