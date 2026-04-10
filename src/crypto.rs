use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
use rand::Rng;
use serde::{Deserialize, Serialize};

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

const VERSION_V1: u8 = 1;
const VERSION_V2: u8 = 2;

#[derive(Serialize, Deserialize)]
struct EncryptedData {
    version: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    salt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nonce: Option<String>,
    data: String,
}

fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; KEY_LEN], String> {
    let argon2 = Argon2::default();
    let salt_string = SaltString::encode_b64(salt).map_err(|e| format!("Invalid salt: {}", e))?;

    let hash = argon2
        .hash_password(password.as_bytes(), &salt_string)
        .map_err(|e| format!("Hash failed: {}", e))?;

    let hash_bytes = hash.hash.ok_or("No hash output")?;
    let hash_slice = hash_bytes.as_bytes();

    let mut key = [0u8; KEY_LEN];
    let len = hash_slice.len().min(KEY_LEN);
    key[..len].copy_from_slice(&hash_slice[..len]);
    Ok(key)
}

#[allow(dead_code)]
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();

    for chunk in data.chunks(3) {
        let mut n = (chunk[0] as u32) << 16;
        if chunk.len() > 1 {
            n |= (chunk[1] as u32) << 8;
        }
        if chunk.len() > 2 {
            n |= chunk[2] as u32;
        }

        result.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        result.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(ALPHABET[(n & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

pub fn encrypt(plaintext: &str, password: &str) -> String {
    let mut salt = [0u8; SALT_LEN];
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut salt);
    rand::rng().fill_bytes(&mut nonce_bytes);

    let key = derive_key(password, &salt).expect("key derivation failed");
    let cipher = Aes256Gcm::new_from_slice(&key).expect("valid key size");
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .expect("encryption failed");

    let encrypted = EncryptedData {
        version: VERSION_V2,
        salt: Some(hex::encode(salt)),
        nonce: Some(hex::encode(nonce_bytes)),
        data: hex::encode(ciphertext),
    };

    serde_json::to_string(&encrypted).expect("serialization failed")
}

pub fn decrypt(encrypted_json: &str, password: &str) -> Result<String, String> {
    let encrypted: EncryptedData =
        serde_json::from_str(encrypted_json).map_err(|e| format!("Invalid JSON: {}", e))?;

    match encrypted.version {
        VERSION_V1 => Err("V1 format detected - plain text migration not supported".to_string()),
        VERSION_V2 => {
            let salt = encrypted.salt.ok_or("Missing salt")?;
            let nonce = encrypted.nonce.ok_or("Missing nonce")?;
            let ciphertext =
                hex::decode(&encrypted.data).map_err(|e| format!("Invalid hex: {}", e))?;

            let salt_bytes = hex::decode(&salt).map_err(|e| format!("Invalid salt hex: {}", e))?;
            let nonce_bytes =
                hex::decode(&nonce).map_err(|e| format!("Invalid nonce hex: {}", e))?;

            let key = derive_key(password, &salt_bytes)?;
            let cipher =
                Aes256Gcm::new_from_slice(&key).map_err(|e| format!("Invalid key: {}", e))?;
            let nonce = Nonce::from_slice(&nonce_bytes);

            let plaintext = cipher
                .decrypt(nonce, ciphertext.as_ref())
                .map_err(|_| "Decryption failed - wrong password?")?;

            String::from_utf8(plaintext).map_err(|e| format!("Invalid UTF-8: {}", e))
        }
        _ => Err(format!("Unknown version: {}", encrypted.version)),
    }
}

pub fn is_encrypted(data: &str) -> bool {
    if let Ok(encrypted) = serde_json::from_str::<EncryptedData>(data) {
        encrypted.version == VERSION_V2
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let plaintext = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let password = "test_password_123";

        let encrypted = encrypt(plaintext, password);
        let decrypted = decrypt(&encrypted, password).expect("decryption failed");

        assert_eq!(plaintext, decrypted);
    }

    #[test]
    fn test_wrong_password_fails() {
        let plaintext = "test seed phrase";
        let password = "correct_password";
        let wrong_password = "wrong_password";

        let encrypted = encrypt(plaintext, password);
        let result = decrypt(&encrypted, wrong_password);

        assert!(result.is_err());
    }

    #[test]
    fn test_is_encrypted() {
        let plaintext = "abandon abandon abandon";
        let password = "test_password";

        let encrypted = encrypt(plaintext, password);

        assert!(is_encrypted(&encrypted));
        assert!(!is_encrypted(plaintext));
    }

    #[test]
    fn test_serialization_format() {
        let plaintext = "test seed";
        let password = "test_password";

        let encrypted = encrypt(plaintext, password);

        let data: EncryptedData = serde_json::from_str(&encrypted).expect("valid JSON");
        assert_eq!(data.version, VERSION_V2);
        assert!(data.salt.is_some());
        assert!(data.nonce.is_some());
        assert!(!data.data.is_empty());
    }

    #[test]
    fn test_different_salts_produce_different_output() {
        let plaintext = "same seed";
        let password = "same_password";

        let encrypted1 = encrypt(plaintext, password);
        let encrypted2 = encrypt(plaintext, password);

        let data1: EncryptedData = serde_json::from_str(&encrypted1).unwrap();
        let data2: EncryptedData = serde_json::from_str(&encrypted2).unwrap();

        assert_ne!(data1.salt, data2.salt);
        assert_ne!(data1.nonce, data2.nonce);
        assert_ne!(data1.data, data2.data);
    }

    #[test]
    fn test_plain_text_not_detected_as_encrypted() {
        let plain_mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

        assert!(!is_encrypted(plain_mnemonic));

        let quoted = format!("\"{}\"", plain_mnemonic);
        assert!(!is_encrypted(&quoted));

        let with_newline = format!("{}\n", plain_mnemonic);
        assert!(!is_encrypted(&with_newline));
    }

    #[test]
    fn test_decrypt_rejects_plain_text() {
        let plain_mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let result = decrypt(plain_mnemonic, "any_password");

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("Invalid JSON") || err.contains("V1 format"));
    }
}
