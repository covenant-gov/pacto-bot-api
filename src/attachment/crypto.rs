//! AES-256-GCM attachment encryption matching `pacto-app`'s wire format.
//!
//! `pacto-app` encrypts attachment ciphertext with a **16-byte** AES-GCM
//! nonce rather than the usual 12-byte nonce (KTD8 in
//! `docs/plans/2026-08-03-001-feat-reactions-attachments-parity-plan.md`).
//! Getting this width wrong silently produces files the app cannot open, so
//! `Cipher` is the single place that width is expressed. The byte layout —
//! ciphertext body followed by the 16-byte GCM tag — matches
//! `pacto-app/src-tauri/src/crypto.rs:68-93`.

use aes::Aes256;
use aes_gcm::aead::generic_array::typenum::U16;
use aes_gcm::{AeadInPlace, AesGcm, KeyInit};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::errors::DaemonError;

/// `AesGcm<Aes256, U16>` — 32-byte key, 16-byte nonce, 16-byte tag.
type Cipher = AesGcm<Aes256, U16>;

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 16;
const TAG_LEN: usize = 16;

/// The literal written into the `encryption-algorithm` rumor tag.
pub const ENCRYPTION_ALGORITHM: &str = "aes-gcm";

/// AES-256-GCM key and 16-byte nonce for one attachment.
///
/// Both are held in [`Zeroizing`] so the bytes are cleared when the value is
/// dropped. The manual [`std::fmt::Debug`] impl below emits neither.
pub struct AttachmentKey {
    key: Zeroizing<[u8; KEY_LEN]>,
    nonce: Zeroizing<[u8; NONCE_LEN]>,
}

impl AttachmentKey {
    /// Draw a fresh key and nonce from OS entropy. Never seeded, never reused
    /// across attachments (R21).
    pub fn generate() -> Result<Self, DaemonError> {
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        getrandom::getrandom(key.as_mut())?;
        let mut nonce = Zeroizing::new([0u8; NONCE_LEN]);
        getrandom::getrandom(nonce.as_mut())?;
        Ok(Self { key, nonce })
    }

    /// Parse from the rumor's hex-encoded `decryption-key` / `decryption-nonce` tags.
    pub fn from_hex(key_hex: &str, nonce_hex: &str) -> Result<Self, DaemonError> {
        let key =
            decode_hex_exact::<KEY_LEN>(key_hex).ok_or_else(|| DaemonError::AttachmentInvalid {
                category: "malformed decryption-key tag".to_string(),
            })?;
        let nonce = decode_hex_exact::<NONCE_LEN>(nonce_hex).ok_or_else(|| {
            DaemonError::AttachmentInvalid {
                category: "malformed decryption-nonce tag".to_string(),
            }
        })?;
        Ok(Self { key, nonce })
    }

    /// Lowercase hex of the 32-byte key, for the `decryption-key` rumor tag.
    pub fn key_hex(&self) -> String {
        hex::encode(self.key.as_slice())
    }

    /// Lowercase hex of the 16-byte nonce, for the `decryption-nonce` rumor tag.
    pub fn nonce_hex(&self) -> String {
        hex::encode(self.nonce.as_slice())
    }

    fn cipher(&self) -> Result<Cipher, DaemonError> {
        Cipher::new_from_slice(self.key.as_slice()).map_err(|_| DaemonError::AttachmentInvalid {
            category: "invalid key length".to_string(),
        })
    }
}

impl std::fmt::Debug for AttachmentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentKey").finish_non_exhaustive()
    }
}

/// Decode `s` as hex into a fixed-size buffer, `None` unless it decodes to
/// exactly `N` bytes. Malformed or wrong-length hex is caller error, never a
/// panic. The intermediate decode buffer is zeroized before it is dropped.
fn decode_hex_exact<const N: usize>(s: &str) -> Option<Zeroizing<[u8; N]>> {
    let decoded = Zeroizing::new(hex::decode(s).ok()?);
    if decoded.len() != N {
        return None;
    }
    let mut out = Zeroizing::new([0u8; N]);
    out.copy_from_slice(&decoded);
    Some(out)
}

/// Encrypt, returning ciphertext = sealed body followed by the 16-byte GCM tag.
pub fn encrypt(key: &AttachmentKey, plaintext: &[u8]) -> Result<Vec<u8>, DaemonError> {
    let cipher = key.cipher()?;
    let nonce = aes_gcm::aead::Nonce::<Cipher>::from_slice(key.nonce.as_slice());
    let mut buffer = plaintext.to_vec();
    let tag = cipher
        .encrypt_in_place_detached(nonce, b"", &mut buffer)
        .map_err(|_| DaemonError::AttachmentInvalid {
            category: "encrypt failed".to_string(),
        })?;
    buffer.extend_from_slice(tag.as_slice());
    Ok(buffer)
}

/// Decrypt ciphertext produced by [`encrypt`]. A failed tag check — wrong
/// key, wrong nonce, or a tampered ciphertext or tag — yields
/// `AttachmentInvalid` with no plaintext in the message.
pub fn decrypt(key: &AttachmentKey, ciphertext: &[u8]) -> Result<Vec<u8>, DaemonError> {
    if ciphertext.len() < TAG_LEN {
        return Err(DaemonError::AttachmentInvalid {
            category: "ciphertext too short".to_string(),
        });
    }
    let split_at = ciphertext.len() - TAG_LEN;
    let (body, tag_bytes) = ciphertext.split_at(split_at);
    let cipher = key.cipher()?;
    let nonce = aes_gcm::aead::Nonce::<Cipher>::from_slice(key.nonce.as_slice());
    let tag = aes_gcm::aead::Tag::<Cipher>::from_slice(tag_bytes);
    let mut buffer = body.to_vec();
    cipher
        .decrypt_in_place_detached(nonce, b"", &mut buffer, tag)
        .map_err(|_| DaemonError::AttachmentInvalid {
            category: "decrypt failed".to_string(),
        })?;
    Ok(buffer)
}

/// Lowercase hex SHA-256, used for the `ox` rumor tag and the Blossom `x` auth tag.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}
