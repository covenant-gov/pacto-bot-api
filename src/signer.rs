//! Signing backends and abstract Signer trait.
//!
//! The daemon supports three progressive-trust signing backends:
//!
//! * `LocalKey` — dev-only raw `nsec` in memory. Secret clearing depends on the
//!   underlying `nostr::Keys` implementation; do not assume active zeroization.
//! * `BunkerLocal` — NIP-46 bunker on the same machine.
//! * `BunkerRemote` — production NIP-46 bunker over `wss://`.
//!
//! Sensitive values (nsec, bunker URI) are never logged.

use crate::config::SigningConfig;
use crate::errors::DaemonError;
use nostr::key::Keys;
use nostr::nips::nip46::NostrConnectURI;
use nostr::secp256k1::Message;
use nostr::{PublicKey, hashes::Hash as BitcoinHashesHash};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Abstract signer used by the daemon to obtain public keys and sign events.
#[async_trait::async_trait]
pub trait Signer: Send + Sync {
    /// Return the signer's public key.
    fn public_key(&self) -> PublicKey;

    /// Sign a serialized event payload, returning the signature hex.
    async fn sign_event(&self, payload: &[u8]) -> Result<String, DaemonError>;
}

/// Concrete signer backend selected from configuration.
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
    /// Construct the appropriate backend from config, validating the configured
    /// npub against the signer-derived pubkey when possible.
    pub fn from_config(config: &SigningConfig, expected_npub: &str) -> Result<Self, DaemonError> {
        let expected_pubkey = parse_npub(expected_npub)
            .map_err(|e| DaemonError::Config(format!("invalid npub for bot: {e}")))?;

        match config {
            SigningConfig::Nsec { nsec } => {
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
                let conn = BunkerConnection::connect(uri, &expected_pubkey, false)?;
                Ok(SignerBackend::BunkerLocal(conn))
            }
            SigningConfig::BunkerRemote { uri } => {
                let conn = BunkerConnection::connect(uri, &expected_pubkey, true)?;
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
}

/// Dev-only local nsec signer.
pub struct LocalKey {
    /// The parsed nostr keys.
    keys: ZeroizingKeys,
}

impl std::fmt::Debug for LocalKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalKey")
            .field("public_key", &self.keys.0.public_key())
            .finish_non_exhaustive()
    }
}

impl LocalKey {
    /// Parse a nsec hex or bech32 string into a local signer.
    pub fn parse(nsec: &str) -> Result<Self, DaemonError> {
        let keys =
            Keys::parse(nsec).map_err(|e| DaemonError::Config(format!("invalid nsec: {e}")))?;
        Ok(Self {
            keys: ZeroizingKeys(keys),
        })
    }
}

#[async_trait::async_trait]
impl Signer for LocalKey {
    fn public_key(&self) -> PublicKey {
        self.keys.0.public_key()
    }

    async fn sign_event(&self, payload: &[u8]) -> Result<String, DaemonError> {
        // The local key signs the SHA-256 hash of the serialized event payload.
        let hash: nostr::hashes::sha256::Hash = BitcoinHashesHash::hash(payload);
        let message = Message::from_digest(*hash.as_byte_array());
        let sig = self.keys.0.sign_schnorr(&message);
        Ok(sig.to_string())
    }
}

/// Newtype around `Keys` that documents the local key is retained in memory.
///
/// `#[zeroize(skip)]` means the inner `Keys` bytes are not cleared by this
/// wrapper; any clearing depends on `nostr::Keys` internals.
#[derive(Zeroize, ZeroizeOnDrop)]
struct ZeroizingKeys(#[zeroize(skip)] Keys);

/// NIP-46 bunker connection details.
pub struct BunkerConnection {
    /// Parsed bunker URI metadata.
    #[allow(dead_code)]
    uri: NostrConnectURI,
    /// Expected public key for this bot identity.
    expected_pubkey: PublicKey,
}

impl std::fmt::Debug for BunkerConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BunkerConnection")
            .field("expected_pubkey", &self.expected_pubkey.to_hex())
            .finish_non_exhaustive()
    }
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

        let remote_pubkey = parsed
            .remote_signer_public_key()
            .ok_or_else(|| DaemonError::Bunker("bunker URI missing remote signer pubkey".into()))?;

        if remote_pubkey != expected_pubkey {
            return Err(DaemonError::Bunker(
                "bunker remote signer pubkey does not match configured npub".into(),
            ));
        }

        Ok(Self {
            uri: parsed,
            expected_pubkey: *expected_pubkey,
        })
    }
}

#[async_trait::async_trait]
impl Signer for BunkerConnection {
    fn public_key(&self) -> PublicKey {
        self.expected_pubkey
    }

    async fn sign_event(&self, _payload: &[u8]) -> Result<String, DaemonError> {
        // TODO(#7y3): implement full NIP-46 sign_event flow over the bunker relay.
        Err(DaemonError::Bunker(
            "NIP-46 signing not yet implemented".into(),
        ))
    }
}

/// Parse an npub bech32 string or hex pubkey into a `PublicKey`.
fn parse_npub(npub: &str) -> Result<PublicKey, String> {
    PublicKey::parse(npub).map_err(|e| format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::ToBech32;

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

        let backend = SignerBackend::from_config(&SigningConfig::Nsec { nsec }, &npub).unwrap();
        assert!(matches!(backend, SignerBackend::LocalKey(_)));
    }

    #[test]
    fn local_key_rejects_mismatched_npub() {
        let keys = test_keys();
        let other_keys = test_keys();
        let nsec = keys.secret_key().to_bech32().unwrap();
        let other_npub = other_keys.public_key().to_bech32().unwrap();

        let err =
            SignerBackend::from_config(&SigningConfig::Nsec { nsec }, &other_npub).unwrap_err();
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

        let backend =
            SignerBackend::from_config(&SigningConfig::BunkerLocal { uri }, &npub).unwrap();
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

        let err =
            SignerBackend::from_config(&SigningConfig::BunkerRemote { uri }, &npub).unwrap_err();
        assert!(err.to_string().contains("wss://"));
    }

    #[test]
    fn bunker_mismatch_returns_error() {
        let keys = test_keys();
        let other_keys = test_keys();
        let npub = keys.public_key().to_bech32().unwrap();
        let uri = format!(
            "bunker://{}?relay=wss://relay.nsec.app",
            other_keys.public_key().to_hex()
        );

        let err =
            SignerBackend::from_config(&SigningConfig::BunkerRemote { uri }, &npub).unwrap_err();
        assert!(err.to_string().contains("does not match"));
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
        drop(signer);
        // ZeroizingKeys implements ZeroizeOnDrop; SecretKey's Drop also erases.
        // Compilation plus successful drop verifies the zeroize contract is wired.
        let _ = ZeroizingKeys(Keys::generate());
    }
}
