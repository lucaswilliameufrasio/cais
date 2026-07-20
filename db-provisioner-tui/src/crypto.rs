use anyhow::{Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use aws_lc_rs::{aead, rand::SecureRandom, rand::SystemRandom};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use zeroize::Zeroizing;

use crate::models::{EncryptedValue, KdfConfig};

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const PASSWORD_CHECK_VALUE: &[u8] = b"db-provisioner-tui:ok";

pub fn default_kdf_config() -> Result<KdfConfig> {
    Ok(KdfConfig {
        salt: random_bytes(16)?,
        memory_kib: 65_536,
        iterations: 3,
        parallelism: 1,
    })
}

pub fn derive_key(password: &str, config: &KdfConfig) -> Result<Zeroizing<Vec<u8>>> {
    let params = Params::new(
        config.memory_kib,
        config.iterations,
        config.parallelism,
        Some(KEY_LEN),
    )
    .map_err(|error| anyhow::anyhow!("invalid Argon2 parameters: {error}"))?;

    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new(vec![0_u8; KEY_LEN]);
    argon2
        .hash_password_into(password.as_bytes(), &config.salt, key.as_mut_slice())
        .map_err(|error| anyhow::anyhow!("failed to derive encryption key: {error}"))?;
    Ok(key)
}

pub fn encrypt(key: &[u8], plaintext: &[u8]) -> Result<EncryptedValue> {
    let unbound_key =
        aead::UnboundKey::new(&aead::AES_256_GCM, key).context("failed to create AEAD key")?;
    let sealing_key = aead::LessSafeKey::new(unbound_key);
    let nonce_bytes = random_bytes(NONCE_LEN)?;
    let nonce =
        aead::Nonce::try_assume_unique_for_key(&nonce_bytes).context("failed to create nonce")?;
    let mut in_out = plaintext.to_vec();
    sealing_key
        .seal_in_place_append_tag(nonce, aead::Aad::empty(), &mut in_out)
        .context("failed to encrypt secret")?;

    Ok(EncryptedValue {
        ciphertext: in_out,
        nonce: nonce_bytes,
    })
}

pub fn decrypt(key: &[u8], encrypted: &EncryptedValue) -> Result<Zeroizing<Vec<u8>>> {
    let unbound_key =
        aead::UnboundKey::new(&aead::AES_256_GCM, key).context("failed to create AEAD key")?;
    let opening_key = aead::LessSafeKey::new(unbound_key);
    let nonce = aead::Nonce::try_assume_unique_for_key(&encrypted.nonce)
        .context("failed to create nonce")?;
    let mut in_out = Zeroizing::new(encrypted.ciphertext.clone());
    let plaintext = opening_key
        .open_in_place(nonce, aead::Aad::empty(), in_out.as_mut_slice())
        .context("failed to decrypt secret")?;
    let len = plaintext.len();
    in_out.truncate(len);
    Ok(in_out)
}

pub fn build_password_check(key: &[u8]) -> Result<EncryptedValue> {
    encrypt(key, PASSWORD_CHECK_VALUE)
}

pub fn verify_password_check(key: &[u8], encrypted: &EncryptedValue) -> Result<()> {
    let plaintext = decrypt(key, encrypted)?;
    if plaintext.as_slice() == PASSWORD_CHECK_VALUE {
        Ok(())
    } else {
        anyhow::bail!("invalid master password")
    }
}

pub fn random_bytes(len: usize) -> Result<Vec<u8>> {
    let rng = SystemRandom::new();
    let mut output = vec![0_u8; len];
    rng.fill(&mut output)
        .context("failed to generate secure random bytes")?;
    Ok(output)
}

pub fn generate_password() -> Result<String> {
    let bytes = random_bytes(24)?;
    Ok(STANDARD.encode(bytes).trim_end_matches('=').to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        build_password_check, decrypt, default_kdf_config, derive_key, encrypt,
        verify_password_check,
    };

    #[test]
    fn round_trips_encrypted_value() {
        let config = default_kdf_config().expect("config");
        let key = derive_key("secret", &config).expect("key");
        let encrypted = encrypt(key.as_slice(), b"hello").expect("encrypt");
        let plaintext = decrypt(key.as_slice(), &encrypted).expect("decrypt");
        assert_eq!(plaintext.as_slice(), b"hello");
    }

    #[test]
    fn verifies_password_check() {
        let config = default_kdf_config().expect("config");
        let key = derive_key("secret", &config).expect("key");
        let check = build_password_check(key.as_slice()).expect("check");
        assert!(verify_password_check(key.as_slice(), &check).is_ok());
    }
}
