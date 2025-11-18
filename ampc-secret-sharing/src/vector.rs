//! Vector types for secret sharing.
//!
//! This module provides `Vector` and `SecretSharedVector` types that can be used
//! to represent plaintext vectors and their secret-shared counterparts.

use crate::galois::degree4::{basis, GaloisRingElement, ShamirGaloisRingShare};
use rand::{CryptoRng, Rng};
use rand_distr::{Distribution, StandardNormal};
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// A plaintext vector of i8 values that can be secret-shared.
///
/// This is the input type for secret sharing operations.
/// The size is specified via const generics, e.g., `Vector<512>` for face embeddings.
#[derive(Clone)]
pub struct Vector<const SIZE: usize>(pub [i8; SIZE]);

/// A secret-shared vector containing u16 values.
///
/// This represents one share of a secret-shared vector.
/// Each share contains SIZE u16 values.
#[derive(Clone, Debug)]
pub struct SecretSharedVector<const SIZE: usize>(pub [u16; SIZE]);

impl<const SIZE: usize> Default for SecretSharedVector<SIZE> {
    fn default() -> Self {
        SecretSharedVector([0u16; SIZE])
    }
}

impl<const SIZE: usize> Vector<SIZE> {
    /// Create a new Vector from an array of i8 values.
    pub fn new(data: [i8; SIZE]) -> Self {
        Vector(data)
    }

    /// Create a random Vector with values in the range [-8, 7).
    ///
    /// This is useful for testing and database initialization.
    pub fn random<R: CryptoRng + Rng>(rng: &mut R) -> Self {
        let mut vec = [0i8; SIZE];
        for element in &mut vec {
            *element = rng.gen_range(-8..7);
        }
        Vector(vec)
    }

    /// Compute the dot product with another vector.
    pub fn dot(&self, other: &Vector<SIZE>) -> i16 {
        self.0
            .iter()
            .zip(other.0.iter())
            .map(|(&a, &b)| (a as i16) * (b as i16))
            .sum()
    }

    /// Create a random normalized vector.
    ///
    /// This generates a vector with values sampled from a standard normal distribution,
    /// normalized to unit length, and then quantized to i8 values.
    ///
    /// This is useful for testing and generating realistic embedding vectors.
    pub fn random_normalized<R: CryptoRng + Rng>(rng: &mut R) -> Self {
        let mut v: Vec<f64> = (0..SIZE).map(|_| StandardNormal.sample(rng)).collect();
        let norm = (v.iter().map(|x| x * x).sum::<f64>()).sqrt();
        v.iter_mut().for_each(|x| *x /= norm);
        Self::quantize(v)
    }

    /// Quantize a floating-point vector to i8 values.
    ///
    /// This is a private helper method that converts a normalized f64 vector
    /// to an i8 array by scaling values to the range [-7, 7].
    fn quantize(vector: Vec<f64>) -> Self {
        let max = vector.iter().map(|x| x.abs()).fold(f64::MIN, f64::max);
        let vec: [i8; SIZE] = vector
            .iter()
            .map(|&x| (x / max * 7.0).round() as i8)
            .collect::<Vec<i8>>()
            .try_into()
            .unwrap();
        Vector(vec)
    }

    /// Generate a random vector with a dot product within the specified range.
    ///
    /// This method generates a random vector `v2` such that `self.dot(&v2)` is within
    /// the range `[dot + eps.start, dot + eps.end)`.
    ///
    /// # Arguments
    /// * `dot` - Target dot product value
    /// * `eps` - Range for the dot product (relative to `dot`)
    /// * `rng` - Random number generator
    ///
    /// # Panics
    /// Panics if `eps.start >= eps.end`
    ///
    /// # Returns
    /// A random vector with dot product in the specified range
    pub fn random_with_dot<R: CryptoRng + Rng>(
        &self,
        dot: i16,
        eps: Range<i16>,
        rng: &mut R,
    ) -> Self {
        assert!(
            eps.start < eps.end,
            "Invalid range: start must be less than end"
        );

        let dot_float = (dot as f64 / self.dot(self) as f64).clamp(-1.0, 1.0);

        // Dequantize
        let mut v1: Vec<f64> = self.0.iter().map(|&x| x as f64).collect();
        let norm = (v1.iter().map(|x| x * x).sum::<f64>()).sqrt();
        v1.iter_mut().for_each(|x| *x /= norm);

        // Rejection sampling
        loop {
            let mut z: Vec<f64> = (0..SIZE).map(|_| StandardNormal.sample(rng)).collect();
            let dot_z_v1: f64 = z.iter().zip(&v1).map(|(a, b)| a * b).sum();
            z.iter_mut()
                .zip(&v1)
                .for_each(|(z_val, &v_val)| *z_val -= dot_z_v1 * v_val);
            let z_norm = (z.iter().map(|x| x * x).sum::<f64>()).sqrt();
            z.iter_mut().for_each(|x| *x /= z_norm);

            let v2: Vec<f64> = v1
                .iter()
                .zip(z)
                .map(|(&v, z)| dot_float * v + (1.0 - dot_float * dot_float).sqrt() * z)
                .collect();

            let v2 = Self::quantize(v2);
            if self.dot(&v2) >= (dot + eps.start) && self.dot(&v2) < (dot + eps.end) {
                return v2;
            }
        }
    }

    /// Create secret shares from this vector.
    ///
    /// This function creates 3 secret shares, each containing SIZE u16 values.
    /// The vector is processed in chunks of 4, where each chunk is converted to
    /// a GaloisRingElement and then secret-shared.
    ///
    /// # Errors
    /// Returns an error if SIZE is not divisible by 4
    pub fn secret_share<R: CryptoRng + Rng>(
        &self,
        rng: &mut R,
    ) -> eyre::Result<[SecretSharedVector<SIZE>; 3]> {
        #[allow(clippy::manual_is_multiple_of)]
        if SIZE % 4 != 0 {
            return Err(eyre::eyre!(
                "Vector size must be divisible by 4, got {}",
                SIZE
            ));
        }

        let mut shares = [
            SecretSharedVector::default(),
            SecretSharedVector::default(),
            SecretSharedVector::default(),
        ];

        for i in (0..SIZE).step_by(4) {
            let element = GaloisRingElement::<basis::A>::from_coefs([
                self.0[i] as u16,
                self.0[i + 1] as u16,
                self.0[i + 2] as u16,
                self.0[i + 3] as u16,
            ]);
            let element = element.to_monomial();
            let share = ShamirGaloisRingShare::encode_3_mat(&element.coefs, rng);
            for j in 0..3 {
                shares[j].0[i] = share[j].y.coefs[0];
                shares[j].0[i + 1] = share[j].y.coefs[1];
                shares[j].0[i + 2] = share[j].y.coefs[2];
                shares[j].0[i + 3] = share[j].y.coefs[3];
            }
        }

        Ok(shares)
    }
}

impl<const SIZE: usize> SecretSharedVector<SIZE> {
    /// Get the underlying array of u16 values.
    pub fn as_array(&self) -> &[u16; SIZE] {
        &self.0
    }

    /// Get a mutable reference to the underlying array.
    pub fn as_array_mut(&mut self) -> &mut [u16; SIZE] {
        &mut self.0
    }

    /// Convert the share to a Vec<u16>.
    pub fn to_vec(&self) -> Vec<u16> {
        self.0.to_vec()
    }

    /// Multiply this share by Lagrange coefficients for the given party ID.
    ///
    /// This is used in the reconstruction process.
    pub fn multiply_lagrange_coeffs(&mut self, id: usize) {
        let lagrange_coeffs = ShamirGaloisRingShare::deg_2_lagrange_polys_at_zero();
        for i in (0..self.0.len()).step_by(4) {
            let element = GaloisRingElement::<basis::Monomial>::from_coefs([
                self.0[i],
                self.0[i + 1],
                self.0[i + 2],
                self.0[i + 3],
            ]);
            let element: GaloisRingElement<basis::Monomial> = element * lagrange_coeffs[id - 1];
            let element = element.to_basis_B();
            self.0[i] = element.coefs[0];
            self.0[i + 1] = element.coefs[1];
            self.0[i + 2] = element.coefs[2];
            self.0[i + 3] = element.coefs[3];
        }
    }
}

// Manual Serialize/Deserialize implementation for large arrays
impl<const SIZE: usize> Serialize for SecretSharedVector<SIZE> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0[..].serialize(serializer)
    }
}

impl<'de, const SIZE: usize> Deserialize<'de> for SecretSharedVector<SIZE> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let vec: Vec<u16> = Deserialize::deserialize(deserializer)?;
        if vec.len() != SIZE {
            return Err(serde::de::Error::custom(format!(
                "Expected {} elements, got {}",
                SIZE,
                vec.len()
            )));
        }
        let mut arr = [0u16; SIZE];
        arr.copy_from_slice(&vec);
        Ok(SecretSharedVector(arr))
    }
}

#[cfg(test)]
mod tests {
    use super::Vector;
    use rand::thread_rng;

    #[test]
    fn test_random_normalized_with_dot() {
        for _ in 0..100 {
            let mut rng = thread_rng();
            let v1 = Vector::<512>::random_normalized(&mut rng);
            let v2 = v1.random_with_dot(1000, -10..10, &mut rng);
            let dot = v1.dot(&v2);
            assert!((990..1010).contains(&dot));
        }
    }
}
