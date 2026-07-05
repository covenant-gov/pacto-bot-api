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
    use crate::test_support::mock_bunker::MockBunker;
    use crate::test_support::mock_relay::MockRelay;
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

    #[tokio::test]
    async fn accepts_matching_bunker_pubkey() {
        let relay = MockRelay::start().await.expect("mock relay starts");
        let keys = Keys::generate();
        let bunker = MockBunker::new(keys.clone(), vec![relay.url()])
            .await
            .expect("mock bunker starts");

        // Give the signer time to bootstrap and subscribe before the client
        // connects.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let uri = bunker.uri(&relay.url());
        let result =
            verify_bunker_public_key(&uri, &keys.public_key(), Duration::from_secs(5)).await;

        bunker.stop().await;
        relay.stop().await;

        assert!(result.is_ok(), "expected matching bunker pubkey to verify");
    }

    #[tokio::test]
    async fn times_out_when_bunker_does_not_respond() {
        // A relay that accepts the connection but never speaks Nostr will
        // force the NIP-46 handshake to hit the call timeout.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stall relay");
        let addr = listener.local_addr().expect("local addr");

        // Spawn a task that accepts connections and drops them, so the
        // bunker client cannot complete the handshake.
        let stall_handle = tokio::spawn(async move {
            loop {
                let _ = listener.accept().await;
            }
        });

        let keys = Keys::generate();
        let uri = format!(
            "bunker://{}?relay=ws://{}",
            keys.public_key().to_hex(),
            addr
        );
        let err = verify_bunker_public_key(&uri, &keys.public_key(), Duration::from_millis(100))
            .await
            .expect_err("expected timeout error");

        stall_handle.abort();
        let _ = stall_handle.await;

        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("timeout") || msg.contains("deadline") || msg.contains("timed out"),
            "expected timeout error, got: {err}"
        );
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
