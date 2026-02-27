use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use anyhow::{anyhow, Result};

const NONCE_LEN: usize = 12;

pub fn encrypt(plaintext: &str, key_hex: &str) -> Result<String> {
    let key_bytes = hex::decode(key_hex)
        .map_err(|_| anyhow!("ENCRYPTION_KEY must be 64 hex characters (32 bytes)"))?;
    if key_bytes.len() != 32 {
        return Err(anyhow!("ENCRYPTION_KEY must be 32 bytes (64 hex chars)"));
    }

    let cipher = Aes256Gcm::new_from_slice(&key_bytes)
        .map_err(|e| anyhow!("Failed to create cipher: {}", e))?;

    let nonce_bytes: [u8; NONCE_LEN] = rand::random();
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| anyhow!("Encryption failed: {}", e))?;

    let mut combined = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);

    Ok(hex::encode(combined))
}

pub fn decrypt(encrypted_hex: &str, key_hex: &str) -> Result<String> {
    let key_bytes = hex::decode(key_hex)
        .map_err(|_| anyhow!("ENCRYPTION_KEY must be 64 hex characters (32 bytes)"))?;
    if key_bytes.len() != 32 {
        return Err(anyhow!("ENCRYPTION_KEY must be 32 bytes (64 hex chars)"));
    }

    let combined = hex::decode(encrypted_hex)
        .map_err(|_| anyhow!("Invalid encrypted data (not hex)"))?;

    if combined.len() < NONCE_LEN + 1 {
        return Err(anyhow!("Encrypted data too short"));
    }

    let (nonce_bytes, ciphertext) = combined.split_at(NONCE_LEN);
    let nonce = Nonce::from_slice(nonce_bytes);

    let cipher = Aes256Gcm::new_from_slice(&key_bytes)
        .map_err(|e| anyhow!("Failed to create cipher: {}", e))?;

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow!("Decryption failed (wrong key or corrupted data)"))?;

    String::from_utf8(plaintext).map_err(|_| anyhow!("Decrypted data is not valid UTF-8"))
}

/// Returns plaintext if no encryption key is set, or decrypts if key is present.
/// Also handles the migration case where data might be stored as plaintext even
/// though a key is now configured (UFVKs start with "uview" or "utest").
pub fn decrypt_or_plaintext(data: &str, key_hex: &str) -> Result<String> {
    if key_hex.is_empty() {
        return Ok(data.to_string());
    }

    if data.starts_with("uview") || data.starts_with("utest") {
        return Ok(data.to_string());
    }

    decrypt(data, key_hex)
}

/// Decrypt a webhook secret, handling migration from plaintext.
/// Plaintext webhook secrets start with "whsec_".
pub fn decrypt_webhook_secret(data: &str, key_hex: &str) -> Result<String> {
    if key_hex.is_empty() || data.starts_with("whsec_") {
        return Ok(data.to_string());
    }
    decrypt(data, key_hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = "a".repeat(64);
        let plaintext = "uviewtest1somefakeufvkdata";
        let encrypted = encrypt(plaintext, &key).unwrap();
        assert_ne!(encrypted, plaintext);
        let decrypted = decrypt(&encrypted, &key).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_decrypt_or_plaintext_no_key() {
        let result = decrypt_or_plaintext("uviewtest1abc", "").unwrap();
        assert_eq!(result, "uviewtest1abc");
    }

    #[test]
    fn test_decrypt_or_plaintext_plaintext_ufvk() {
        let key = "b".repeat(64);
        let result = decrypt_or_plaintext("uviewtest1abc", &key).unwrap();
        assert_eq!(result, "uviewtest1abc");
    }
}
