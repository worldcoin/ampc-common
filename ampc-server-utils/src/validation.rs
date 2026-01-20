//! Share hash validation utilities.
//!
//! This module provides generic functions for validating the integrity of
//! decrypted shares by comparing their SHA256 hashes.

use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Errors that can occur during share validation.
#[derive(Error, Debug)]
pub enum SharesValidationError {
    /// The computed hash of the share does not match the expected hash.
    #[error("Share hash mismatch: expected={expected}, computed={computed}")]
    HashMismatch { expected: String, computed: String },

    /// Failed to serialize the share to JSON for hashing.
    #[error("Failed to serialize share for validation: {0}")]
    SerializationError(#[from] serde_json::error::Error),
}

/// Compute SHA256 hash of data and return it as a hex string.
pub fn sha256_as_hex_string<T: AsRef<[u8]>>(data: T) -> String {
    let hash = Sha256::digest(data.as_ref());
    hex::encode(hash)
}

/// Validate that the hash of a share matches the expected hash.
///
/// This function serializes the share to JSON, computes its SHA256 hash,
/// and compares it against the expected hash. This is used to verify the
/// integrity of shares received from external services.
///
/// # Type Parameters
/// * `T` - The share type, must implement `Serialize`
///
/// # Arguments
/// * `expected_hash` - The SHA256 hash (hex string) provided by the sender
/// * `share` - The decrypted share struct to validate
///
/// # Returns
/// * `Ok(())` if the hash matches
/// * `Err(SharesValidationError)` if the hash doesn't match or serialization fails
pub fn validate_share_hash<T: Serialize>(
    expected_hash: &str,
    share: &T,
) -> Result<(), SharesValidationError> {
    let stringified_share = serde_json::to_string(share)?;

    let computed_hash = sha256_as_hex_string(stringified_share.as_bytes());

    if expected_hash != computed_hash {
        tracing::error!(
            "Share hash mismatch: expected={}, computed={}",
            expected_hash,
            computed_hash
        );
        return Err(SharesValidationError::HashMismatch {
            expected: expected_hash.to_string(),
            computed: computed_hash,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
    struct TestShare {
        version: String,
        data: String,
    }

    #[test]
    fn test_sha256_as_hex_string() {
        // Test with known input/output
        let data = b"hello world";
        let hash = sha256_as_hex_string(data);
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_validate_share_hash_success() {
        let share = TestShare {
            version: "1.0".to_string(),
            data: "test_data".to_string(),
        };

        // Compute the expected hash
        let json_str = serde_json::to_string(&share).unwrap();
        let expected_hash = sha256_as_hex_string(json_str.as_bytes());

        let result = validate_share_hash(&expected_hash, &share);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_share_hash_mismatch() {
        let share = TestShare {
            version: "1.0".to_string(),
            data: "test_data".to_string(),
        };

        let wrong_hash = "0000000000000000000000000000000000000000000000000000000000000000";

        let result = validate_share_hash(wrong_hash, &share);
        assert!(result.is_err());

        match result {
            Err(SharesValidationError::HashMismatch { expected, computed }) => {
                assert_eq!(expected, wrong_hash);
                assert!(!computed.is_empty());
            }
            _ => panic!("Expected HashMismatch error"),
        }
    }
}
