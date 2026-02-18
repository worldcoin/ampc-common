//! Secret sharing utilities for AMPC (Asymmetric Multi-Party Computation)
//!
//! This module provides common secret sharing functionality that can be used
//! by both face-ampc and iris-mpc implementations.
#![deny(
    clippy::iter_over_hash_type,
    reason = "In MPC protocols, this can be dangerous as the iteration order is not guaranteed to be in sync between the parties due to HashMap randomization."
)]

pub mod face_vector;
pub mod galois;
pub mod id;
pub mod shares;

pub use face_vector::{FaceSecretSharedVector, FaceVector, FACE_VECTOR_SIZE};
pub use galois::degree4::{basis, GaloisRingElement, ShamirGaloisRingShare};
pub use id::PartyID;
pub use shares::{IntRing2k, RingElement, Role, Share};
