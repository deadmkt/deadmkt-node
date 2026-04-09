// deadmkt-keystore: AES-256-GCM encrypted Ed25519 key storage
// with Argon2id KDF. Supports encrypted, insecure, and env modes.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use argon2::Argon2;
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

// =========================================================================
// Error types
// =========================================================================

#[derive(Debug, Error)]
pub enum KeystoreError {
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("decryption failed (wrong password?)")]
    DecryptionFailed,

    #[error("invalid key data: {0}")]
    InvalidKeyData(String),

    #[error("hex decode error: {0}")]
    HexError(#[from] hex::FromHexError),

    #[error("environment variable DEADMKT_KEYSTORE_PASSWORD not set")]
    EnvVarNotSet,

    #[error("KDF error: {0}")]
    KdfError(String),
}

// =========================================================================
// Keystore mode detection
// =========================================================================

#[derive(Debug, PartialEq, Eq)]
pub enum KeystoreMode {
    Encrypted,
    Insecure,
    Missing,
}

/// Detect keystore mode from file contents.
pub fn detect_keystore_mode(path: &Path) -> Result<KeystoreMode, KeystoreError> {
    if !path.exists() {
        return Ok(KeystoreMode::Missing);
    }
    let contents = std::fs::read_to_string(path)?;
    let json: serde_json::Value = serde_json::from_str(&contents)?;
    if json.get("mode").and_then(|v| v.as_str()) == Some("insecure") {
        Ok(KeystoreMode::Insecure)
    } else {
        Ok(KeystoreMode::Encrypted)
    }
}

// =========================================================================
// Keystore file format
// =========================================================================

#[derive(Serialize, Deserialize)]
struct EncryptedKeystore {
    salt: String,       // hex, 32 bytes
    nonce: String,      // hex, 12 bytes
    ciphertext: String, // hex
    public_key: String, // hex, 32 bytes
}

#[derive(Serialize, Deserialize)]
struct InsecureKeystore {
    mode: String,       // "insecure"
    secret_key: String, // hex, 32 bytes
    public_key: String, // hex, 32 bytes
}

// =========================================================================
// KDF: Argon2id
// =========================================================================

/// Derive a 256-bit encryption key from password + salt using Argon2id.
/// Params: memory=64MB, iterations=3, parallelism=1.
pub fn derive_key(password: &[u8], salt: &[u8]) -> Result<[u8; 32], KeystoreError> {
    let params = argon2::Params::new(65536, 3, 1, Some(32))
        .map_err(|e| KeystoreError::KdfError(e.to_string()))?;
    let argon2 = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);

    let mut key = [0u8; 32];
    argon2.hash_password_into(password, salt, &mut key)
        .map_err(|e| KeystoreError::KdfError(e.to_string()))?;
    Ok(key)
}

// =========================================================================
// Save/Load encrypted keystore
// =========================================================================

pub fn save_keystore(
    path: &Path,
    secret: &SigningKey,
    public: &VerifyingKey,
    password: &str,
) -> Result<(), KeystoreError> {
    let mut salt = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut salt);

    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);

    let key = derive_key(password.as_bytes(), &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| KeystoreError::InvalidKeyData(e.to_string()))?;
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, secret.as_bytes().as_ref())
        .map_err(|_| KeystoreError::DecryptionFailed)?;

    let keystore = EncryptedKeystore {
        salt: hex::encode(salt),
        nonce: hex::encode(nonce_bytes),
        ciphertext: hex::encode(ciphertext),
        public_key: hex::encode(public.as_bytes()),
    };

    let json = serde_json::to_string_pretty(&keystore)?;
    std::fs::write(path, json)?;
    Ok(())
}

pub fn load_keystore(
    path: &Path,
    password: &str,
) -> Result<(SigningKey, VerifyingKey), KeystoreError> {
    let contents = std::fs::read_to_string(path)?;
    let keystore: EncryptedKeystore = serde_json::from_str(&contents)?;

    let salt = hex::decode(&keystore.salt)?;
    let nonce_bytes = hex::decode(&keystore.nonce)?;
    let ciphertext = hex::decode(&keystore.ciphertext)?;

    let key = derive_key(password.as_bytes(), &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| KeystoreError::InvalidKeyData(e.to_string()))?;
    let nonce = Nonce::from_slice(&nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, ciphertext.as_ref())
        .map_err(|_| KeystoreError::DecryptionFailed)?;

    if plaintext.len() != 32 {
        return Err(KeystoreError::InvalidKeyData(
            format!("expected 32 bytes, got {}", plaintext.len()),
        ));
    }

    let secret_bytes: [u8; 32] = plaintext.try_into().unwrap();
    let secret = SigningKey::from_bytes(&secret_bytes);
    let public = secret.verifying_key();

    Ok((secret, public))
}

// =========================================================================
// Insecure mode
// =========================================================================

pub fn save_keystore_insecure(
    path: &Path,
    secret: &SigningKey,
    public: &VerifyingKey,
) -> Result<(), KeystoreError> {
    let keystore = InsecureKeystore {
        mode: "insecure".into(),
        secret_key: hex::encode(secret.as_bytes()),
        public_key: hex::encode(public.as_bytes()),
    };
    let json = serde_json::to_string_pretty(&keystore)?;
    std::fs::write(path, json)?;
    Ok(())
}

pub fn load_keystore_insecure(
    path: &Path,
) -> Result<(SigningKey, VerifyingKey), KeystoreError> {
    let contents = std::fs::read_to_string(path)?;
    let keystore: InsecureKeystore = serde_json::from_str(&contents)?;

    let secret_bytes = hex::decode(&keystore.secret_key)?;
    if secret_bytes.len() != 32 {
        return Err(KeystoreError::InvalidKeyData(
            format!("expected 32 bytes, got {}", secret_bytes.len()),
        ));
    }

    let secret = SigningKey::from_bytes(secret_bytes.as_slice().try_into().unwrap());
    let public = secret.verifying_key();
    Ok((secret, public))
}

// =========================================================================
// Env mode
// =========================================================================

pub fn load_keystore_from_env(
    path: &Path,
) -> Result<(SigningKey, VerifyingKey), KeystoreError> {
    let password = std::env::var("DEADMKT_KEYSTORE_PASSWORD")
        .map_err(|_| KeystoreError::EnvVarNotSet)?;
    load_keystore(path, &password)
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use tempfile::tempdir;

    fn gen_keypair() -> (SigningKey, VerifyingKey) {
        let secret = SigningKey::generate(&mut OsRng);
        let public = secret.verifying_key();
        (secret, public)
    }

    // T_KS_01
    #[test]
    fn test_keystore_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("keystore.json");

        let (secret, public) = gen_keypair();
        let password = "test-password-123";

        save_keystore(&path, &secret, &public, password).unwrap();
        assert!(path.exists());

        let (loaded_secret, loaded_public) = load_keystore(&path, password).unwrap();
        assert_eq!(secret.as_bytes(), loaded_secret.as_bytes());
        assert_eq!(public.as_bytes(), loaded_public.as_bytes());
    }

    // T_KS_02
    #[test]
    fn test_keystore_wrong_password() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("keystore.json");

        let (secret, public) = gen_keypair();
        save_keystore(&path, &secret, &public, "correct-password").unwrap();

        let result = load_keystore(&path, "wrong-password");
        assert!(matches!(result, Err(KeystoreError::DecryptionFailed)));
    }

    // T_KS_03
    #[test]
    fn test_keystore_file_format() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("keystore.json");

        let (secret, public) = gen_keypair();
        save_keystore(&path, &secret, &public, "password").unwrap();

        let contents: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        assert!(contents.get("salt").is_some());
        assert!(contents.get("nonce").is_some());
        assert!(contents.get("ciphertext").is_some());
        assert!(contents.get("public_key").is_some());

        // Salt: 32 bytes = 64 hex chars
        assert_eq!(contents["salt"].as_str().unwrap().len(), 64);
        // Nonce: 12 bytes = 24 hex chars
        assert_eq!(contents["nonce"].as_str().unwrap().len(), 24);
    }

    // T_KS_04
    #[test]
    fn test_argon2id_deterministic() {
        let password = b"test-password";
        let salt = [0u8; 32];

        let key1 = derive_key(password, &salt).unwrap();
        let key2 = derive_key(password, &salt).unwrap();
        assert_eq!(key1, key2);
        assert_eq!(key1.len(), 32);
    }

    // T_KS_05
    #[test]
    fn test_insecure_mode() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("keystore.json");

        let (secret, public) = gen_keypair();
        save_keystore_insecure(&path, &secret, &public).unwrap();

        let contents: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(contents["mode"].as_str().unwrap(), "insecure");

        let (loaded_secret, _) = load_keystore_insecure(&path).unwrap();
        assert_eq!(secret.as_bytes(), loaded_secret.as_bytes());
    }

    // T_KS_06
    #[test]
    fn test_env_mode() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("keystore.json");
        let password = "env-mode-password-456";

        let (secret, public) = gen_keypair();
        save_keystore(&path, &secret, &public, password).unwrap();

        // Set env var
        std::env::set_var("DEADMKT_KEYSTORE_PASSWORD", password);

        let (loaded_secret, loaded_public) = load_keystore_from_env(&path).unwrap();
        assert_eq!(secret.as_bytes(), loaded_secret.as_bytes());
        assert_eq!(public.as_bytes(), loaded_public.as_bytes());

        // Missing env var
        std::env::remove_var("DEADMKT_KEYSTORE_PASSWORD");
        let result = load_keystore_from_env(&path);
        assert!(matches!(result, Err(KeystoreError::EnvVarNotSet)));
    }

    // T_KS_07
    #[test]
    fn test_keystore_subsequent_boot() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("keystore.json");
        let password = "boot-password";

        let (secret, public) = gen_keypair();
        save_keystore(&path, &secret, &public, password).unwrap();

        assert!(path.exists());
        let mode = detect_keystore_mode(&path).unwrap();
        assert_eq!(mode, KeystoreMode::Encrypted);

        let (loaded_secret, _) = load_keystore(&path, password).unwrap();
        assert_eq!(secret.as_bytes(), loaded_secret.as_bytes());
    }

    // Extra: detect missing
    #[test]
    fn test_detect_missing_keystore() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        assert_eq!(detect_keystore_mode(&path).unwrap(), KeystoreMode::Missing);
    }

    // Extra: detect insecure mode
    #[test]
    fn test_detect_insecure_mode() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("keystore.json");

        let (secret, public) = gen_keypair();
        save_keystore_insecure(&path, &secret, &public).unwrap();

        assert_eq!(detect_keystore_mode(&path).unwrap(), KeystoreMode::Insecure);
    }
}
