use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key as ChaKey, Nonce};
use rand::RngCore;
use thiserror::Error;
use zeroize::Zeroize;

#[derive(Clone)]
pub struct Key([u8; 32]);

impl Key {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() != 32 {
            return Err(CryptoError::BadKey);
        }
        let mut k = [0u8; 32];
        k.copy_from_slice(bytes);
        Ok(Self(k))
    }

    pub fn random() -> Self {
        let mut k = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut k);
        Self(k)
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("bad key")]
    BadKey,
    #[error("encrypt failed")]
    Encrypt,
    #[error("decrypt failed")]
    Decrypt,
}

/// Envelope: 12-byte nonce || ciphertext+tag.
pub fn encrypt(key: &Key, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let cipher = ChaCha20Poly1305::new(ChaKey::from_slice(&key.0));
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| CryptoError::Encrypt)?;
    let mut out = Vec::with_capacity(12 + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn decrypt(key: &Key, blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if blob.len() < 13 {
        return Err(CryptoError::Decrypt);
    }
    let cipher = ChaCha20Poly1305::new(ChaKey::from_slice(&key.0));
    let nonce = Nonce::from_slice(&blob[..12]);
    cipher
        .decrypt(nonce, &blob[12..])
        .map_err(|_| CryptoError::Decrypt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let key = Key::from_bytes([3u8; 32]);
        let ct = encrypt(&key, b"hello profiles").unwrap();
        assert_ne!(&ct[12..], b"hello profiles");
        assert_eq!(decrypt(&key, &ct).unwrap(), b"hello profiles");
        assert!(decrypt(&Key::from_bytes([9u8; 32]), &ct).is_err());
    }
}
