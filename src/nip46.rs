//! Minimal NIP-46 (Nostr Connect) client for bunker verification.
//!
//! This module performs a live `get_public_key` request against a NIP-46
//! bunker over its declared relay. It is intentionally scoped to verification:
//! full signing/encryption through the bunker remains future work.

use std::time::Duration;

use nostr::PublicKey;
use nostr::key::Keys;
use nostr::nips::nip46::NostrConnectURI;
use nostr_connect::client::NostrConnect;

use crate::errors::DaemonError;

/// Verify that a NIP-46 bunker returns the expected public key.
///
/// Generates an ephemeral client key, connects to the relays declared in the
/// bunker URI, and performs the NIP-46 handshake. Returns `Ok(())` only when
/// the bunker's live public key matches `expected_pubkey`. Returns a
/// [`DaemonError::Bunker`] on connection failure, timeout, or mismatch.
pub async fn verify_bunker_public_key(
    bunker_uri: &str,
    expected_pubkey: &PublicKey,
    call_timeout: Duration,
) -> Result<(), DaemonError> {
    let uri = NostrConnectURI::parse(bunker_uri)
        .map_err(|e| DaemonError::Bunker(format!("invalid bunker URI: {e}")))?;

    if !uri.is_bunker() {
        return Err(DaemonError::Bunker("not a bunker URI".into()));
    }

    let app_keys = Keys::generate();
    let connect = NostrConnect::new(uri, app_keys, call_timeout, None)
        .map_err(|e| DaemonError::Bunker(format!("failed to create NIP-46 client: {e}")))?;

    // `bunker_uri` bootstraps the connection, sends `connect`, and returns
    // the URI annotated with the remote signer's live public key.
    let live_uri = connect
        .bunker_uri()
        .await
        .map_err(|e| DaemonError::Bunker(format!("bunker handshake failed: {e}")))?;

    connect.shutdown().await;

    let live_pubkey = live_uri
        .remote_signer_public_key()
        .ok_or_else(|| DaemonError::Bunker("bunker URI missing remote signer pubkey".into()))?;

    if live_pubkey != expected_pubkey {
        return Err(DaemonError::Bunker(
            "bunker public key does not match configured npub".into(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_non_bunker_uri() {
        let keys = Keys::generate();
        // `NostrConnectURI::parse` only accepts bunker URIs; a nostrconnect URI
        // is rejected at parse time.
        let uri = format!(
            "nostrconnect://{}?relay=ws://127.0.0.1:4242",
            keys.public_key().to_hex()
        );
        let expected = keys.public_key();
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(verify_bunker_public_key(
                &uri,
                &expected,
                Duration::from_secs(1),
            ))
            .unwrap_err();
        assert!(err.to_string().contains("invalid bunker URI"));
    }

    #[test]
    fn rejects_invalid_uri() {
        let keys = Keys::generate();
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(verify_bunker_public_key(
                "not-a-uri",
                &keys.public_key(),
                Duration::from_secs(1),
            ))
            .unwrap_err();
        assert!(err.to_string().contains("invalid bunker URI"));
    }
}
