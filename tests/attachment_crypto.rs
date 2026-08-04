#![allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]
//! U3: attachment crypto and media primitives
//! (docs/plans/2026-08-03-001-feat-reactions-attachments-parity-plan.md,
//! "U3. Attachment crypto and media primitives").
//!
//! `tests/fixtures/attachment/aes_gcm_vector.json` is the cross-implementation
//! fixture required by that unit's test scenarios. The plan asks for a
//! ciphertext fixture produced by pacto-app's own encrypt path. A pacto-app
//! checkout exists on this development machine but its working tree carries
//! uncommitted changes from concurrent work on another plan, so it was read
//! only (to confirm `src-tauri/src/crypto.rs:68-93` matches the byte layout
//! exercised here) and not built, to avoid disturbing that concurrent work.
//! The committed fixture is instead produced by an INDEPENDENT AES-256-GCM
//! implementation — Python's `cryptography` package (OpenSSL-backed), not
//! the `aes-gcm` Rust crate under test — so the round trip below is a
//! genuine cross-implementation check rather than a same-crate proof. The
//! fixture JSON's `_provenance` field carries the exact generation command.

use std::path::PathBuf;

use pacto_bot_api::attachment::crypto::{AttachmentKey, decrypt, encrypt, sha256_hex};
use pacto_bot_api::attachment::mime::extension_for_mime;
use pacto_bot_api::errors::DaemonError;

/// The independent-implementation cross-check vector for this unit.
struct Fixture {
    key_hex: String,
    nonce_hex: String,
    plaintext: Vec<u8>,
    ciphertext: Vec<u8>,
}

fn load_fixture() -> Fixture {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("attachment")
        .join("aes_gcm_vector.json");
    let raw = std::fs::read_to_string(&path).expect("read fixture");
    let json: serde_json::Value = serde_json::from_str(&raw).expect("parse fixture json");
    let field = |name: &str| -> String {
        json[name]
            .as_str()
            .unwrap_or_else(|| panic!("fixture missing string field {name}"))
            .to_string()
    };
    Fixture {
        key_hex: field("key_hex"),
        nonce_hex: field("nonce_hex"),
        plaintext: hex::decode(field("plaintext_hex")).expect("decode plaintext hex"),
        ciphertext: hex::decode(field("ciphertext_hex")).expect("decode ciphertext hex"),
    }
}

/// Encrypt then decrypt round-trips an arbitrary payload.
#[test]
fn round_trip_arbitrary_payload() {
    let key = AttachmentKey::generate().expect("generate key");
    let plaintext = b"an arbitrary attachment payload, not tied to any fixture".to_vec();

    let ciphertext = encrypt(&key, &plaintext).expect("encrypt");
    let recovered = decrypt(&key, &ciphertext).expect("decrypt");

    assert_eq!(recovered, plaintext);
}

/// A ciphertext fixture produced by an independent implementation, committed
/// alongside its key and nonce, decrypts to the expected plaintext. This is
/// the cross-implementation check; a same-implementation round trip cannot
/// catch a layout divergence.
#[test]
fn external_fixture_decrypts_to_expected_plaintext() {
    let fixture = load_fixture();
    let key = AttachmentKey::from_hex(&fixture.key_hex, &fixture.nonce_hex)
        .expect("parse fixture key/nonce");

    let recovered = decrypt(&key, &fixture.ciphertext).expect("decrypt fixture ciphertext");

    assert_eq!(recovered, fixture.plaintext);
}

/// Encrypting the fixture's plaintext under the same key and nonce
/// reproduces the committed ciphertext byte for byte.
#[test]
fn reencrypting_fixture_plaintext_reproduces_ciphertext_byte_for_byte() {
    let fixture = load_fixture();
    let key = AttachmentKey::from_hex(&fixture.key_hex, &fixture.nonce_hex)
        .expect("parse fixture key/nonce");

    let ciphertext = encrypt(&key, &fixture.plaintext).expect("re-encrypt fixture plaintext");

    assert_eq!(ciphertext, fixture.ciphertext);
}

/// A ciphertext produced with a 16-byte nonce fails to decrypt under a
/// 12-byte-nonce cipher, proving the nonce width `attachment::crypto` uses
/// is genuinely 16 bytes and not the more common 12.
#[test]
fn sixteen_byte_nonce_ciphertext_fails_under_twelve_byte_nonce_cipher() {
    use aes_gcm::Aes256Gcm;
    use aes_gcm::aead::{AeadInPlace, KeyInit};

    let key = AttachmentKey::generate().expect("generate key");
    let plaintext = b"nonce width proof payload".to_vec();
    let ciphertext = encrypt(&key, &plaintext).expect("encrypt");

    let key_bytes = hex::decode(key.key_hex()).expect("decode key hex");
    let nonce_bytes = hex::decode(key.nonce_hex()).expect("decode nonce hex");
    assert_eq!(
        nonce_bytes.len(),
        16,
        "our nonce must genuinely be 16 bytes"
    );

    // Build a standard 12-byte-nonce AES-256-GCM cipher directly (not via
    // attachment::crypto) with the same key, using only the first 12 bytes
    // of our 16-byte nonce.
    let cipher12 = Aes256Gcm::new_from_slice(&key_bytes).expect("build 12-byte-nonce cipher");
    let nonce12 = aes_gcm::aead::Nonce::<Aes256Gcm>::from_slice(&nonce_bytes[..12]);
    let tag_start = ciphertext.len() - 16;
    let (body, tag_bytes) = ciphertext.split_at(tag_start);
    let tag12 = aes_gcm::aead::Tag::<Aes256Gcm>::from_slice(tag_bytes);
    let mut buffer = body.to_vec();

    let result = cipher12.decrypt_in_place_detached(nonce12, b"", &mut buffer, tag12);

    assert!(
        result.is_err(),
        "a 12-byte-nonce cipher must not decrypt ciphertext produced under a 16-byte nonce"
    );
}

/// Ciphertext length equals plaintext length plus exactly 16.
#[test]
fn ciphertext_length_is_plaintext_length_plus_sixteen() {
    let key = AttachmentKey::generate().expect("generate key");
    for len in [0usize, 1, 17, 4096] {
        let plaintext = vec![0xABu8; len];
        let ciphertext = encrypt(&key, &plaintext).expect("encrypt");
        assert_eq!(ciphertext.len(), plaintext.len() + 16);
    }
}

/// Flipping one ciphertext byte fails decryption with `AttachmentInvalid`.
#[test]
fn flipping_ciphertext_byte_fails_with_attachment_invalid() {
    let key = AttachmentKey::generate().expect("generate key");
    let plaintext = b"payload whose body will be tampered with".to_vec();
    let mut ciphertext = encrypt(&key, &plaintext).expect("encrypt");

    // Flip a byte in the sealed body, well before the trailing 16-byte tag.
    ciphertext[0] ^= 0xFF;

    let err = decrypt(&key, &ciphertext).expect_err("tampered ciphertext must fail to decrypt");
    assert!(matches!(err, DaemonError::AttachmentInvalid { .. }));
}

/// Flipping one authentication-tag byte fails decryption with
/// `AttachmentInvalid`.
#[test]
fn flipping_tag_byte_fails_with_attachment_invalid() {
    let key = AttachmentKey::generate().expect("generate key");
    let plaintext = b"payload whose tag will be tampered with".to_vec();
    let mut ciphertext = encrypt(&key, &plaintext).expect("encrypt");

    let last = ciphertext.len() - 1;
    ciphertext[last] ^= 0xFF;

    let err = decrypt(&key, &ciphertext).expect_err("tampered tag must fail to decrypt");
    assert!(matches!(err, DaemonError::AttachmentInvalid { .. }));
}

/// Decrypting under the wrong key fails rather than returning garbage.
#[test]
fn wrong_key_fails_rather_than_returning_garbage() {
    let right_key = AttachmentKey::generate().expect("generate right key");
    let plaintext = b"payload encrypted under the right key".to_vec();
    let ciphertext = encrypt(&right_key, &plaintext).expect("encrypt");

    // A different key, same nonce, so only the key material differs.
    let wrong_key_material = AttachmentKey::generate().expect("generate wrong key material");
    let wrong_key = AttachmentKey::from_hex(&wrong_key_material.key_hex(), &right_key.nonce_hex())
        .expect("build wrong-key AttachmentKey");

    let err = decrypt(&wrong_key, &ciphertext).expect_err("wrong key must fail to decrypt");
    assert!(matches!(err, DaemonError::AttachmentInvalid { .. }));
}

/// Two encryptions of identical plaintext produce different keys and
/// different nonces (fresh OS entropy every call, never reused).
#[test]
fn two_encryptions_of_identical_plaintext_produce_different_keys_and_nonces() {
    let key_a = AttachmentKey::generate().expect("generate key a");
    let key_b = AttachmentKey::generate().expect("generate key b");

    assert_ne!(key_a.key_hex(), key_b.key_hex());
    assert_ne!(key_a.nonce_hex(), key_b.nonce_hex());
}

/// Malformed hex for the key or the nonce returns `AttachmentInvalid`
/// rather than panicking.
#[test]
fn malformed_key_and_nonce_hex_return_attachment_invalid_without_panicking() {
    let valid_key = AttachmentKey::generate().expect("generate valid key");

    // Malformed key hex: not valid hex at all.
    let err = AttachmentKey::from_hex("not-hex-at-all", &valid_key.nonce_hex())
        .expect_err("non-hex key must be rejected");
    assert!(matches!(err, DaemonError::AttachmentInvalid { .. }));

    // Malformed key hex: valid hex but wrong length (16 bytes, not 32).
    let err = AttachmentKey::from_hex(&valid_key.nonce_hex(), &valid_key.nonce_hex())
        .expect_err("wrong-length key hex must be rejected");
    assert!(matches!(err, DaemonError::AttachmentInvalid { .. }));

    // Malformed nonce hex: not valid hex at all.
    let err = AttachmentKey::from_hex(&valid_key.key_hex(), "also-not-hex")
        .expect_err("non-hex nonce must be rejected");
    assert!(matches!(err, DaemonError::AttachmentInvalid { .. }));

    // Malformed nonce hex: valid hex but wrong length (32 bytes, not 16).
    let err = AttachmentKey::from_hex(&valid_key.key_hex(), &valid_key.key_hex())
        .expect_err("wrong-length nonce hex must be rejected");
    assert!(matches!(err, DaemonError::AttachmentInvalid { .. }));
}

/// `sha256_hex` matches the published SHA-256 vector for `"abc"`.
#[test]
fn sha256_hex_matches_known_vector() {
    let digest = sha256_hex(b"abc");
    assert_eq!(
        digest,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

/// `extension_for_mime` returns `png` for `image/png`, `pdf` for
/// `application/pdf`, and `bin` for an unrecognized type.
#[test]
fn extension_for_mime_returns_expected_values() {
    assert_eq!(extension_for_mime("image/png"), "png");
    assert_eq!(extension_for_mime("application/pdf"), "pdf");
    assert_eq!(extension_for_mime("application/x-not-a-real-mime"), "bin");
}

/// The `Debug` rendering of the key and nonce carrier contains no hex of the
/// key material.
#[test]
fn debug_rendering_hides_key_material() {
    let key = AttachmentKey::generate().expect("generate key");
    let key_hex = key.key_hex();
    let nonce_hex = key.nonce_hex();

    let rendered = format!("{key:?}");

    assert!(!rendered.contains(&key_hex));
    assert!(!rendered.contains(&nonce_hex));
}
