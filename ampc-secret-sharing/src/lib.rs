//! Secret sharing utilities for AMPC (Asymmetric Multi-Party Computation)
//!
//! This module provides common secret sharing functionality that can be used
//! by both face-ampc and iris-mpc implementations.

pub mod galois;
pub mod id;
pub mod shares;
pub mod vector;

pub use galois::degree4::{basis, GaloisRingElement, ShamirGaloisRingShare};
pub use id::PartyID;
pub use shares::{IntRing2k, RingElement, Role, Share};
pub use vector::{SecretSharedVector, Vector};
