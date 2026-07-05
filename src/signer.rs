//! Signing backends and abstract Signer trait.
//!
//! The daemon supports three progressive-trust signing backends:
//!
//! * `LocalKey` — dev-only raw `nsec` in memory. Secret bytes are stored in a
//!   [`Zeroizing`] container and cleared when the signer is dropped.
//! * `BunkerLocal` — NIP-46 bunker on the same machine.
//! * `BunkerRemote` — production NIP-46 bunker over `wss://`.
//!
//! Sensitive values (nsec, bunker URI) are never logged.

use std::time::Duration;

use crate::config::SigningConfig;
use crate::errors::DaemonError;
use nostr::NostrSigner;
use nostr::key::{Keys, SecretKey};
use nostr::nips::nip46::NostrConnectURI;
use nostr::secp256k1::{Message, Secp256k1};
use nostr::{Kind, PublicKey, Tag, Timestamp, UnsignedEvent, hashes::Hash as BitcoinHashesHash};
use nostr_connect::client::NostrConnect;
use nostr_sdk::RelayOptions;
use secrecy::ExposeSecret;
#[cfg(test)]
use secrecy::SecretString;
use zeroize::{Zeroize, Zeroizing};

/// Abstract signer used by the daemon to obtain public keys and sign events.
#[async_trait::async_trait]
pub trait Signer: Send + Sync {
    /// Return the signer's public key.
    fn public_key(&self) -> PublicKey;

    /// Sign a serialized event payload, returning the signature hex.
    async fn sign_event(&self, payload: &[u8]) -> Result<String, DaemonError>;

    /// Encrypt content for `public_key` using NIP-44.
    async fn nip44_encrypt(
        &self,
        public_key: &PublicKey,
        content: &str,
    ) -> Result<String, DaemonError>;

    /// Decrypt a NIP-44 payload received from `public_key`.
    async fn nip44_decrypt(
        &self,
        public_key: &PublicKey,
        payload: &str,
    ) -> Result<String, DaemonError>;
}

/// Concrete signer backend selected from configuration.
#[derive(Clone)]
pub enum SignerBackend {
    /// Dev-only local nsec key.
    LocalKey(LocalKey),
    /// Local NIP-46 bunker connection.
    BunkerLocal(BunkerConnection),
    /// Production NIP-46 bunker connection.
    BunkerRemote(BunkerConnection),
}

impl std::fmt::Debug for SignerBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignerBackend::LocalKey(_) => f.debug_struct("LocalKey").finish_non_exhaustive(),
            SignerBackend::BunkerLocal(_) => f.debug_struct("BunkerLocal").finish_non_exhaustive(),
            SignerBackend::BunkerRemote(_) => {
                f.debug_struct("BunkerRemote").finish_non_exhaustive()
            }
        }
    }
}

impl SignerBackend {
    /// Verify that a bunker backend's live public key matches the configured
    /// npub. Local keys always pass.
    pub async fn verify_bunker_public_key(&self) -> Result<(), DaemonError> {
        match self {
            SignerBackend::BunkerLocal(conn) | SignerBackend::BunkerRemote(conn) => {
                conn.verify_public_key().await
            }
            SignerBackend::LocalKey(_) => Ok(()),
        }
    }

    /// Construct the appropriate backend from config, validating the configured
    /// npub against the signer-derived pubkey when possible.
    pub fn from_config(config: &SigningConfig, expected_npub: &str) -> Result<Self, DaemonError> {
        let expected_pubkey = parse_npub(expected_npub)
            .map_err(|e| DaemonError::Config(format!("invalid npub for bot: {e}")))?;

        match config {
            SigningConfig::Nsec { nsec } => {
                let nsec = nsec.expose_secret();
                if nsec.is_empty() {
                    return Err(DaemonError::Config(
                        "nsec backend requires a non-empty key".into(),
                    ));
                }
                let signer = LocalKey::parse(nsec)?;
                if signer.public_key() != expected_pubkey {
                    return Err(DaemonError::Config(
                        "nsec public key does not match configured npub".into(),
                    ));
                }
                Ok(SignerBackend::LocalKey(signer))
            }
            SigningConfig::BunkerLocal { uri } => {
                let conn = BunkerConnection::connect(uri.expose_secret(), &expected_pubkey, false)?;
                Ok(SignerBackend::BunkerLocal(conn))
            }
            SigningConfig::BunkerRemote { uri } => {
                let conn = BunkerConnection::connect(uri.expose_secret(), &expected_pubkey, true)?;
                Ok(SignerBackend::BunkerRemote(conn))
            }
        }
    }
}

#[async_trait::async_trait]
impl Signer for SignerBackend {
    fn public_key(&self) -> PublicKey {
        match self {
            SignerBackend::LocalKey(s) => s.public_key(),
            SignerBackend::BunkerLocal(s) | SignerBackend::BunkerRemote(s) => s.public_key(),
        }
    }

    async fn sign_event(&self, payload: &[u8]) -> Result<String, DaemonError> {
        match self {
            SignerBackend::LocalKey(s) => s.sign_event(payload).await,
            SignerBackend::BunkerLocal(s) | SignerBackend::BunkerRemote(s) => {
                s.sign_event(payload).await
            }
        }
    }

    async fn nip44_encrypt(
        &self,
        public_key: &PublicKey,
        content: &str,
    ) -> Result<String, DaemonError> {
        match self {
            SignerBackend::LocalKey(s) => s.nip44_encrypt(public_key, content).await,
            SignerBackend::BunkerLocal(s) | SignerBackend::BunkerRemote(s) => {
                s.nip44_encrypt(public_key, content).await
            }
        }
    }

    async fn nip44_decrypt(
        &self,
        public_key: &PublicKey,
        payload: &str,
    ) -> Result<String, DaemonError> {
        match self {
            SignerBackend::LocalKey(s) => s.nip44_decrypt(public_key, payload).await,
            SignerBackend::BunkerLocal(s) | SignerBackend::BunkerRemote(s) => {
                s.nip44_decrypt(public_key, payload).await
            }
        }
    }
}

/// Dev-only local nsec signer.
pub struct LocalKey {
    /// Cached public key derived from the secret.
    public_key: PublicKey,
    /// Raw secret key bytes. Held in a heap-allocated [`Zeroizing`] container
    /// so the key material is cleared when the signer is dropped and no
    /// unzeroed copy remains on the caller's stack.
    secret_bytes: Box<Zeroizing<[u8; 32]>>,
}

impl Clone for LocalKey {
    fn clone(&self) -> Self {
        Self {
            public_key: self.public_key,
            secret_bytes: self.secret_bytes.clone(),
        }
    }
}

impl std::fmt::Debug for LocalKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalKey")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

impl LocalKey {
    /// Parse a nsec hex or bech32 string into a local signer.
    pub fn parse(nsec: &str) -> Result<Self, DaemonError> {
        // Decode directly into a heap-allocated zeroizing byte buffer. Keeping
        // the secret on the heap means the only live copy is inside the
        // `Zeroizing` container; the temporary stack copies used to derive the
        // public key are zeroed before the function returns.
        let mut secret_bytes = Box::new(Zeroizing::new([0u8; 32]));
        let hex_ok = hex::decode_to_slice(nsec, secret_bytes.as_mut().as_mut()).is_ok();
        if !hex_ok {
            let (_, data) =
                bech32::decode(nsec).map_err(|_| DaemonError::Config("invalid nsec".into()))?;
            if data.len() != 32 {
                return Err(DaemonError::Config("invalid nsec length".into()));
            }
            secret_bytes.as_mut().copy_from_slice(&data);
            // Zeroize the bech32 decode buffer before it is freed.
            let _ = Zeroizing::new(data);
        }

        // Copy the secret to the stack for public-key derivation, then zero all
        // copies that are not the live heap buffer.
        let mut temp = [0u8; 32];
        temp.copy_from_slice(secret_bytes.as_ref().as_ref());
        let secp = Secp256k1::new();
        let mut secret_key = SecretKey::from_slice(&temp)
            .map_err(|e| DaemonError::Config(format!("invalid nsec: {e}")))?;
        let (xonly, _parity) = secret_key.x_only_public_key(&secp);
        secret_key.non_secure_erase();
        temp.zeroize();

        Ok(Self {
            public_key: PublicKey::from(xonly),
            secret_bytes,
        })
    }

    /// Reconstruct a temporary `Keys` from the zeroized secret bytes.
    fn keys(&self) -> Result<Keys, DaemonError> {
        let secret_key = SecretKey::from_slice(self.secret_bytes.as_ref().as_ref())
            .map_err(|e| DaemonError::Nostr(format!("invalid secret key bytes: {e}")))?;
        Ok(Keys::new(secret_key))
    }
}

#[cfg(test)]
impl LocalKey {
    /// Return the raw secret key bytes.
    ///
    /// This helper is test-only and is stripped from release builds.
    pub fn secret_bytes(&self) -> &[u8] {
        self.secret_bytes.as_ref().as_ref()
    }
}

#[async_trait::async_trait]
impl Signer for LocalKey {
    fn public_key(&self) -> PublicKey {
        self.public_key
    }

    async fn sign_event(&self, payload: &[u8]) -> Result<String, DaemonError> {
        // The local key signs the SHA-256 hash of the serialized event payload.
        let hash: nostr::hashes::sha256::Hash = BitcoinHashesHash::hash(payload);
        let message = Message::from_digest(*hash.as_byte_array());
        let keys = self.keys()?;
        let sig = keys.sign_schnorr(&message);
        Ok(sig.to_string())
    }

    async fn nip44_encrypt(
        &self,
        public_key: &PublicKey,
        content: &str,
    ) -> Result<String, DaemonError> {
        let keys = self.keys()?;
        let signer: &dyn nostr::NostrSigner = &keys;
        signer
            .nip44_encrypt(public_key, content)
            .await
            .map_err(|e| DaemonError::Nostr(format!("NIP-44 encryption failed: {e}")))
    }

    async fn nip44_decrypt(
        &self,
        public_key: &PublicKey,
        payload: &str,
    ) -> Result<String, DaemonError> {
        let keys = self.keys()?;
        let signer: &dyn nostr::NostrSigner = &keys;
        signer
            .nip44_decrypt(public_key, payload)
            .await
            .map_err(|e| DaemonError::Nostr(format!("NIP-44 decryption failed: {e}")))
    }
}

/// NIP-46 bunker connection details.
#[derive(Clone)]
pub struct BunkerConnection {
    /// Parsed bunker URI metadata.
    #[allow(dead_code)]
    uri: NostrConnectURI,
    /// Expected public key for this bot identity.
    expected_pubkey: PublicKey,
    /// Live NIP-46 client used to send signing/encryption requests.
    client: NostrConnect,
}

impl std::fmt::Debug for BunkerConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BunkerConnection")
            .field("expected_pubkey", &self.expected_pubkey.to_hex())
            .finish_non_exhaustive()
    }
}

/// Build the default relay options used for NIP-46 bunker connections.
///
/// Auto-reconnection is enabled so that transient relay drops do not
/// permanently terminate the bunker's signing session.
fn default_bunker_relay_opts() -> RelayOptions {
    RelayOptions::default()
        .notification_channel_size(4096)
        .reconnect(true)
}

impl BunkerConnection {
    /// Parse a bunker URI and verify it declares the expected bot pubkey.
    pub fn connect(
        uri: &str,
        expected_pubkey: &PublicKey,
        require_wss: bool,
    ) -> Result<Self, DaemonError> {
        if uri.is_empty() {
            return Err(DaemonError::Bunker("empty bunker URI".into()));
        }

        let parsed = NostrConnectURI::parse(uri)
            .map_err(|e| DaemonError::Bunker(format!("invalid bunker URI: {e}")))?;

        if !parsed.is_bunker() {
            return Err(DaemonError::Bunker("not a bunker URI".into()));
        }

        if require_wss {
            let relays = parsed.relays();
            if relays.iter().any(|r| r.as_str().starts_with("ws://")) {
                return Err(DaemonError::Bunker(
                    "bunker_remote must use wss:// relays".into(),
                ));
            }
        }

        // The pubkey declared in the URI is not trusted on its own; live
        // verification against the bunker's reported pubkey happens during
        // ClientManager startup.
        if parsed.remote_signer_public_key().is_none() {
            return Err(DaemonError::Bunker(
                "bunker URI missing remote signer pubkey".into(),
            ));
        }

        let app_keys = Keys::generate();
        let opts = default_bunker_relay_opts();
        let timeout_secs: u64 = std::env::var("PACTO_BUNKER_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        let client = NostrConnect::new(
            parsed.clone(),
            app_keys,
            Duration::from_secs(timeout_secs),
            Some(opts),
        )
        .map_err(|e| DaemonError::Bunker(format!("failed to create NIP-46 client: {e}")))?;

        Ok(Self {
            uri: parsed,
            expected_pubkey: *expected_pubkey,
            client,
        })
    }

    /// Query the bunker's live public key via the NIP-46 `get_public_key`
    /// handshake (`bunker_uri`).
    pub async fn get_public_key(&self) -> Result<PublicKey, DaemonError> {
        let live_uri = self
            .client
            .bunker_uri()
            .await
            .map_err(|e| DaemonError::Bunker(format!("bunker handshake failed: {e}")))?;

        let live_pubkey = live_uri
            .remote_signer_public_key()
            .ok_or_else(|| DaemonError::Bunker("bunker URI missing remote signer pubkey".into()))?;

        Ok(*live_pubkey)
    }

    /// Verify that the bunker's live public key matches the configured npub.
    pub async fn verify_public_key(&self) -> Result<(), DaemonError> {
        let live = self.get_public_key().await?;
        if live != self.expected_pubkey {
            return Err(DaemonError::Config(
                "configured npub does not match live bunker public key".into(),
            ));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Signer for BunkerConnection {
    fn public_key(&self) -> PublicKey {
        self.expected_pubkey
    }

    async fn sign_event(&self, payload: &[u8]) -> Result<String, DaemonError> {
        let unsigned = unsigned_event_from_payload(payload)?;
        let expected_id = unsigned.id.ok_or_else(|| {
            DaemonError::Bunker("NIP-46 sign_event payload missing event id".into())
        })?;

        let event = self
            .client
            .sign_event(unsigned)
            .await
            .map_err(|e| DaemonError::Bunker(format!("NIP-46 sign_event failed: {e}")))?;

        if event.id != expected_id {
            return Err(DaemonError::Bunker(
                "bunker returned event id mismatch".into(),
            ));
        }

        event
            .verify()
            .map_err(|e| DaemonError::Bunker(format!("bunker signature invalid: {e}")))?;

        Ok(event.sig.to_string())
    }

    async fn nip44_encrypt(
        &self,
        public_key: &PublicKey,
        content: &str,
    ) -> Result<String, DaemonError> {
        self.client
            .nip44_encrypt(public_key, content)
            .await
            .map_err(|e| DaemonError::Bunker(format!("NIP-46 nip44_encrypt failed: {e}")))
    }

    async fn nip44_decrypt(
        &self,
        public_key: &PublicKey,
        payload: &str,
    ) -> Result<String, DaemonError> {
        self.client
            .nip44_decrypt(public_key, payload)
            .await
            .map_err(|e| DaemonError::Bunker(format!("NIP-46 nip44_decrypt failed: {e}")))
    }
}

/// Reconstruct an [`UnsignedEvent`] from the canonical event-id preimage that
/// the daemon produces for signing.
fn unsigned_event_from_payload(payload: &[u8]) -> Result<UnsignedEvent, DaemonError> {
    let (_, pubkey, created_at, kind, tags, content): (
        u8,
        PublicKey,
        Timestamp,
        Kind,
        Vec<Tag>,
        String,
    ) = serde_json::from_slice(payload)
        .map_err(|e| DaemonError::Nostr(format!("invalid event signing payload: {e}")))?;

    let mut unsigned = UnsignedEvent::new(pubkey, created_at, kind, tags, content);
    unsigned.ensure_id();
    Ok(unsigned)
}

/// Parse an npub bech32 string or hex pubkey into a `PublicKey`.
fn parse_npub(npub: &str) -> Result<PublicKey, String> {
    PublicKey::parse(npub).map_err(|e| format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::ToBech32;
    use serde_json::json;

    fn test_keys() -> Keys {
        Keys::generate()
    }

    #[test]
    fn local_key_parses_nsec_and_matches_npub() {
        let keys = test_keys();
        let nsec = keys.secret_key().to_bech32().unwrap();
        let npub = keys.public_key().to_bech32().unwrap();

        let signer = LocalKey::parse(&nsec).unwrap();
        assert_eq!(signer.public_key(), keys.public_key());

        let backend = SignerBackend::from_config(
            &SigningConfig::Nsec {
                nsec: SecretString::new(nsec.into()),
            },
            &npub,
        )
        .unwrap();
        assert!(matches!(backend, SignerBackend::LocalKey(_)));
    }

    #[test]
    fn local_key_rejects_mismatched_npub() {
        let keys = test_keys();
        let other_keys = test_keys();
        let nsec = keys.secret_key().to_bech32().unwrap();
        let other_npub = other_keys.public_key().to_bech32().unwrap();

        let err = SignerBackend::from_config(
            &SigningConfig::Nsec {
                nsec: SecretString::new(nsec.into()),
            },
            &other_npub,
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not match"));
    }

    #[test]
    fn bunker_local_accepts_matching_pubkey() {
        let keys = test_keys();
        let npub = keys.public_key().to_bech32().unwrap();
        let uri = format!(
            "bunker://{}?relay=ws://127.0.0.1:4848",
            keys.public_key().to_hex()
        );

        let backend = SignerBackend::from_config(
            &SigningConfig::BunkerLocal {
                uri: SecretString::new(uri.into()),
            },
            &npub,
        )
        .unwrap();
        assert!(matches!(backend, SignerBackend::BunkerLocal(_)));
    }

    #[test]
    fn bunker_remote_rejects_ws_relay() {
        let keys = test_keys();
        let npub = keys.public_key().to_bech32().unwrap();
        let uri = format!(
            "bunker://{}?relay=ws://relay.nsec.app",
            keys.public_key().to_hex()
        );

        let err = SignerBackend::from_config(
            &SigningConfig::BunkerRemote {
                uri: SecretString::new(uri.into()),
            },
            &npub,
        )
        .unwrap_err();
        assert!(err.to_string().contains("wss://"));
    }

    #[test]
    fn bunker_mismatch_uri_is_accepted_for_live_verification() {
        let keys = test_keys();
        let other_keys = test_keys();
        let npub = keys.public_key().to_bech32().unwrap();
        let uri = format!(
            "bunker://{}?relay=wss://relay.nsec.app",
            other_keys.public_key().to_hex()
        );

        // The URI-declared pubkey is no longer trusted at parse time; live
        // verification happens during ClientManager startup.
        let backend = SignerBackend::from_config(
            &SigningConfig::BunkerRemote {
                uri: SecretString::new(uri.into()),
            },
            &npub,
        )
        .unwrap();
        assert!(matches!(backend, SignerBackend::BunkerRemote(_)));
        assert_eq!(backend.public_key(), keys.public_key());
    }

    #[test]
    fn bunker_remote_accepts_matching_pubkey() {
        let keys = test_keys();
        let npub = keys.public_key().to_bech32().unwrap();
        let uri = format!(
            "bunker://{}?relay=wss://relay.nsec.app",
            keys.public_key().to_hex()
        );

        let backend = SignerBackend::from_config(
            &SigningConfig::BunkerRemote {
                uri: SecretString::new(uri.into()),
            },
            &npub,
        )
        .unwrap();
        assert!(matches!(backend, SignerBackend::BunkerRemote(_)));
    }

    #[test]
    fn bunker_relay_options_enable_reconnect() {
        let opts = default_bunker_relay_opts();
        // RelayOptions does not expose the reconnect field directly, but its
        // Debug representation includes it. This guards against accidentally
        // disabling reconnection for bunker sessions.
        let debug = format!("{:?}", opts);
        assert!(
            debug.contains("reconnect: true"),
            "bunker relay options should enable reconnect: {debug}"
        );
    }

    #[test]
    fn unsigned_event_from_payload_roundtrips() {
        let keys = test_keys();
        let mut unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from(1_700_000_000_u64),
            Kind::TextNote,
            Vec::new(),
            "payload roundtrip",
        );
        unsigned.ensure_id();
        let expected_id = unsigned.id.unwrap();

        let payload = serde_json::to_vec(&json!([
            0,
            unsigned.pubkey,
            unsigned.created_at,
            unsigned.kind,
            unsigned.tags,
            unsigned.content
        ]))
        .unwrap();

        let reconstructed = unsigned_event_from_payload(&payload).unwrap();
        assert_eq!(reconstructed.id, Some(expected_id));
        assert_eq!(reconstructed.pubkey, keys.public_key());
        assert_eq!(reconstructed.kind, Kind::TextNote);
        assert_eq!(reconstructed.content, "payload roundtrip");
    }

    #[test]
    fn debug_does_not_leak_nsec() {
        let keys = test_keys();
        let nsec = keys.secret_key().to_bech32().unwrap();
        let signer = LocalKey::parse(&nsec).unwrap();
        let debug = format!("{:?}", signer);
        assert!(!debug.contains(&nsec));
        assert!(!debug.contains(&keys.secret_key().to_secret_hex()));
    }

    #[test]
    fn zeroize_clears_local_key_secret() {
        let keys = test_keys();
        let nsec = keys.secret_key().to_bech32().unwrap();
        let signer = LocalKey::parse(&nsec).unwrap();
        assert_ne!(signer.secret_bytes(), &[0u8; 32]);
        drop(signer);
        // The secret bytes live in a Zeroizing container that is cleared on
        // drop. The memory-scan integration test verifies no secret bytes
        // remain in the process address space after the signer is dropped.
    }
}
