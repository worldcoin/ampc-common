use aws_sdk_secretsmanager::{
    error::SdkError, operation::get_secret_value::GetSecretValueError,
    Client as SecretsManagerClient,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use eyre::Result;
use serde::de::DeserializeOwned;
use sodiumoxide::crypto::{
    box_::{PublicKey, SecretKey},
    sealedbox,
};
use std::string::FromUtf8Error;
use thiserror::Error;
use zeroize::Zeroize;

const CURRENT_SECRET_LABEL: &str = "AWSCURRENT";
const PREVIOUS_SECRET_LABEL: &str = "AWSPREVIOUS";

#[allow(clippy::large_enum_variant, clippy::result_large_err)]
#[derive(Error, Debug)]
pub enum SharesDecodingError {
    #[error("Secrets Manager error: {0}")]
    SecretsManagerError(#[from] SdkError<GetSecretValueError>),
    #[error("Secret string not found")]
    SecretStringNotFound,
    #[error("Decoding error: {0}")]
    DecodingError(#[from] base64::DecodeError),
    #[error("Parsing bytes to UTF8 error")]
    DecodedShareParsingToUTF8Error(#[from] FromUtf8Error),
    #[error("Parsing key error")]
    ParsingKeyError,
    #[error("Sealed box open error")]
    SealedBoxOpenError,
    #[error("Previous key not found error")]
    PreviousKeyNotFound,
    #[error("Base64 decoding error")]
    Base64DecodeError,
    #[error(transparent)]
    SerdeError(#[from] serde_json::error::Error),
    #[error("Failed to parse decrypted share: {0}")]
    ShareParsingError(String),
}

pub struct SharesEncryptionKeyPairs {
    current_key_pair: SharesEncryptionKeyPair,
    previous_key_pair: Option<SharesEncryptionKeyPair>,
}

impl Zeroize for SharesEncryptionKeyPairs {
    fn zeroize(&mut self) {
        self.current_key_pair.zeroize();
        if let Some(ref mut prev) = self.previous_key_pair {
            prev.zeroize();
        }
    }
}

impl Drop for SharesEncryptionKeyPairs {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl SharesEncryptionKeyPairs {
    /// Load encryption key pairs from AWS Secrets Manager
    ///
    /// # Arguments
    /// * `client` - AWS Secrets Manager client
    /// * `environment` - Environment name (e.g., "dev", "stage", "prod")
    /// * `service_name` - Service name for the secret path (e.g., "face-ampc", "iris-mpc")
    /// * `party_id` - Party ID (0, 1, or 2)
    pub async fn from_storage(
        client: SecretsManagerClient,
        environment: &str,
        service_name: &str,
        party_id: usize,
    ) -> Result<Self, SharesDecodingError> {
        let current_sk_b64_string = download_private_key_from_asm(
            &client,
            environment,
            service_name,
            &party_id.to_string(),
            CURRENT_SECRET_LABEL,
        )
        .await?;

        #[allow(clippy::manual_unwrap_or_default)]
        let previous_sk_b64_string = match download_private_key_from_asm(
            &client,
            environment,
            service_name,
            &party_id.to_string(),
            PREVIOUS_SECRET_LABEL,
        )
        .await
        {
            Ok(sk) => sk,
            Err(_) => String::new(), // Previous key is optional
        };

        Self::from_b64_private_key_strings(current_sk_b64_string, previous_sk_b64_string)
    }

    #[allow(clippy::result_large_err)]
    pub fn from_b64_private_key_strings(
        current_sk_b64_string: String,
        previous_sk_b64_string: String,
    ) -> Result<Self, SharesDecodingError> {
        let current_key_pair =
            SharesEncryptionKeyPair::from_b64_private_key_string(current_sk_b64_string)?;

        if previous_sk_b64_string.is_empty() {
            return Ok(SharesEncryptionKeyPairs {
                current_key_pair,
                previous_key_pair: None,
            });
        }

        let previous_key_pair =
            SharesEncryptionKeyPair::from_b64_private_key_string(previous_sk_b64_string)?;
        Ok(SharesEncryptionKeyPairs {
            current_key_pair,
            previous_key_pair: Some(previous_key_pair),
        })
    }
}

pub struct SharesEncryptionKeyPair {
    pk: PublicKey,
    sk: SecretKey,
}

impl Zeroize for SharesEncryptionKeyPair {
    fn zeroize(&mut self) {
        self.pk.0.zeroize();
        self.sk.0.zeroize();
    }
}

impl Drop for SharesEncryptionKeyPair {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl SharesEncryptionKeyPair {
    #[allow(clippy::result_large_err)]
    pub fn from_b64_private_key_string(sk: String) -> Result<Self, SharesDecodingError> {
        // Parse the secret string - it might be JSON with a "private-key" field
        let sk_b64 = if sk.trim().starts_with('{') {
            let json: serde_json::Value =
                serde_json::from_str(&sk).map_err(|_| SharesDecodingError::ParsingKeyError)?;
            json.get("private-key")
                .and_then(|v| v.as_str())
                .ok_or(SharesDecodingError::ParsingKeyError)?
                .to_string()
        } else {
            sk
        };

        let sk_bytes = STANDARD
            .decode(sk_b64)
            .map_err(SharesDecodingError::DecodingError)?;

        let sk = SecretKey::from_slice(&sk_bytes).ok_or(SharesDecodingError::ParsingKeyError)?;

        let pk_from_sk = sk.public_key();
        Ok(Self { pk: pk_from_sk, sk })
    }

    #[allow(clippy::result_large_err)]
    pub fn open_sealed_box(&self, code: Vec<u8>) -> Result<Vec<u8>, SharesDecodingError> {
        let decrypted = sealedbox::open(&code, &self.pk, &self.sk);
        match decrypted {
            Ok(bytes) => Ok(bytes),
            Err(_) => Err(SharesDecodingError::SealedBoxOpenError),
        }
    }
}

#[allow(clippy::result_large_err)]
async fn download_private_key_from_asm(
    client: &SecretsManagerClient,
    env: &str,
    service_name: &str,
    node_id: &str,
    version_stage: &str,
) -> Result<String, SharesDecodingError> {
    let private_key_secret_id: String =
        format!("{}/{}/ecdh-private-key-{}", env, service_name, node_id);
    tracing::info!(
        "Downloading private key from Secrets Manager: {} (version: {})",
        private_key_secret_id,
        version_stage
    );

    match client
        .get_secret_value()
        .secret_id(private_key_secret_id)
        .version_stage(version_stage)
        .send()
        .await
    {
        Ok(secret_key_output) => match secret_key_output.secret_string {
            Some(data) => Ok(data),
            None => Err(SharesDecodingError::SecretStringNotFound),
        },
        Err(e) => Err(e.into()),
    }
}

/// Decrypt a base64-encoded encrypted share and deserialize it into a type T
///
/// # Type Parameters
/// * `T` - The type to deserialize the decrypted share into (must implement DeserializeOwned)
///
/// # Arguments
/// * `encrypted_share_b64` - Base64-encoded encrypted share
/// * `key_pairs` - Encryption key pairs (current and optionally previous)
///
/// # Returns
/// The decrypted and deserialized share of type T
#[allow(clippy::result_large_err)]
pub fn decrypt_share<T: DeserializeOwned>(
    encrypted_share_b64: String,
    key_pairs: &SharesEncryptionKeyPairs,
) -> Result<T, SharesDecodingError> {
    // Base64 decode the encrypted share
    let share_bytes = STANDARD
        .decode(encrypted_share_b64.as_bytes())
        .map_err(|_| SharesDecodingError::Base64DecodeError)?;

    let decrypted_bytes = decrypt_binary_share(share_bytes, key_pairs)?;

    // Parse the decrypted JSON string
    let json_string = String::from_utf8(decrypted_bytes)
        .map_err(SharesDecodingError::DecodedShareParsingToUTF8Error)?;

    // Parse the JSON into type T
    let share: T = serde_json::from_str(&json_string).map_err(|e| {
        SharesDecodingError::ShareParsingError(format!("Failed to parse share JSON: {}", e))
    })?;

    Ok(share)
}

/// Decrypt a binary share and return the decrypted bytes
///
/// # Arguments
/// * `share_bytes` - Binary share bytes
/// * `key_pairs` - Encryption key pairs (current and optionally previous)
///
/// # Returns
/// The decrypted binary share
#[allow(clippy::result_large_err)]
pub fn decrypt_binary_share(
    share_bytes: Vec<u8>,
    key_pairs: &SharesEncryptionKeyPairs,
) -> Result<Vec<u8>, SharesDecodingError> {
    if let Ok(decrypted_bytes) = key_pairs
        .current_key_pair
        .open_sealed_box(share_bytes.clone())
    {
        return Ok(decrypted_bytes);
    }

    let previous_key_pair = key_pairs
        .previous_key_pair
        .as_ref()
        .ok_or(SharesDecodingError::PreviousKeyNotFound)?;

    previous_key_pair
        .open_sealed_box(share_bytes)
        .map_err(|_| SharesDecodingError::SealedBoxOpenError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sodiumoxide::crypto::box_;

    #[test]
    fn decrypts_with_current_key() {
        let (current_pk, current_sk) = box_::gen_keypair();
        let current_b64 = STANDARD.encode(current_sk.0);
        let key_pairs =
            SharesEncryptionKeyPairs::from_b64_private_key_strings(current_b64, String::new())
                .expect("failed to build key pairs");
        let plaintext = b"hello binary share".to_vec();
        let ciphertext = sealedbox::seal(&plaintext, &current_pk);
        let decrypted = decrypt_binary_share(ciphertext, &key_pairs).expect("decryption failed");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn falls_back_to_previous_key() {
        let (_, current_sk) = box_::gen_keypair();
        let current_b64 = STANDARD.encode(current_sk.0);
        let (previous_pk, previous_sk) = box_::gen_keypair();
        let previous_b64 = STANDARD.encode(previous_sk.0);
        let key_pairs =
            SharesEncryptionKeyPairs::from_b64_private_key_strings(current_b64, previous_b64)
                .expect("failed to build key pairs");
        let plaintext = b"sealed with the previous key".to_vec();
        let ciphertext = sealedbox::seal(&plaintext, &previous_pk);

        let decrypted = decrypt_binary_share(ciphertext, &key_pairs).expect("decryption failed");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn errors_with_previous_key_not_found_when_no_previous() {
        let (_, current_sk) = box_::gen_keypair();
        let current_b64 = STANDARD.encode(current_sk.0);
        // Seal with an unrelated key so the current key pair cannot open it.
        let (unrelated_pk, _) = box_::gen_keypair();
        let ciphertext = sealedbox::seal(b"unopenable", &unrelated_pk);

        let key_pairs =
            SharesEncryptionKeyPairs::from_b64_private_key_strings(current_b64, String::new())
                .expect("failed to build key pairs");
        let err = decrypt_binary_share(ciphertext, &key_pairs).unwrap_err();
        assert!(matches!(err, SharesDecodingError::PreviousKeyNotFound));
    }

    #[test]
    fn errors_with_sealed_box_open_when_neither_key_matches() {
        let (_, current_sk) = box_::gen_keypair();
        let current_b64 = STANDARD.encode(current_sk.0);
        let (_, previous_sk) = box_::gen_keypair();
        let previous_b64 = STANDARD.encode(previous_sk.0);
        let key_pairs =
            SharesEncryptionKeyPairs::from_b64_private_key_strings(current_b64, previous_b64)
                .expect("failed to build key pairs");
        let (unrelated_pk, _) = box_::gen_keypair();
        let ciphertext = sealedbox::seal(b"unopenable", &unrelated_pk);
        let err = decrypt_binary_share(ciphertext, &key_pairs).unwrap_err();
        assert!(matches!(err, SharesDecodingError::SealedBoxOpenError));
    }
}
