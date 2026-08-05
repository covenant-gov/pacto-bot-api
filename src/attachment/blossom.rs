//! Blossom ciphertext upload with BUD-01 authorization and ordered failover.

use std::str::FromStr;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use nostr::hashes::sha256::Hash as Sha256Hash;
use nostr::{EventBuilder, JsonUtil, Timestamp};
use nostr_blossom::bud01::{
    BlossomAuthorization, BlossomAuthorizationScope, BlossomAuthorizationVerb,
    BlossomBuilderExtension,
};
use nostr_blossom::bud02::BlobDescriptor;

use crate::attachment::crypto::sha256_hex;
use crate::errors::DaemonError;
use crate::nostr::sign_unsigned_event;
use crate::signer::Signer;

const AUTHORIZATION_TTL: Duration = Duration::from_secs(300);
const X_REASON: &str = "x-reason";
/// Per-host deadline for a single upload attempt. Bounds each try so an
/// unresponsive host costs one timeout instead of stalling the failover walk.
const UPLOAD_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Upload ciphertext to the first configured Blossom host that accepts it.
///
/// A fresh kind:24242 authorization is signed for each attempt. Neither the
/// ciphertext nor any attachment decryption material is included in errors.
pub async fn upload(
    servers: &[String],
    signer: &dyn Signer,
    ciphertext: &[u8],
) -> Result<String, DaemonError> {
    if servers.is_empty() {
        return Err(failed("no Blossom hosts configured"));
    }

    let hash_hex = sha256_hex(ciphertext);
    let hash = Sha256Hash::from_str(&hash_hex)
        .map_err(|_| failed("ciphertext hash construction failed"))?;
    // Every attempt needs its own deadline, otherwise a host that completes the
    // TCP handshake and then never responds blocks `send()` forever and the
    // ordered failover below never reaches the next host at all -- defeating
    // the whole point of accepting a list. The inbound fetcher bounds itself
    // the same way (`src/attachment/inbound.rs`).
    let client = reqwest::Client::builder()
        .timeout(UPLOAD_ATTEMPT_TIMEOUT)
        .build()
        .map_err(|_| failed("Blossom client setup failed"))?;
    let mut last_reason = "all Blossom hosts failed".to_string();

    for (index, server) in servers.iter().enumerate() {
        let upload_url =
            match reqwest::Url::parse(&format!("{}/upload", server.trim_end_matches('/'))) {
                Ok(url) => url,
                Err(_) => {
                    last_reason = format!("host {} has an invalid upload URL", index + 1);
                    continue;
                }
            };

        let created_at = Timestamp::now();
        let authorization = BlossomAuthorization::new(
            "Blossom upload authorization".to_string(),
            created_at + AUTHORIZATION_TTL,
            BlossomAuthorizationVerb::Upload,
            BlossomAuthorizationScope::BlobSha256Hashes(vec![hash]),
        );
        let unsigned = EventBuilder::blossom_auth(authorization)
            .custom_created_at(created_at)
            .build(signer.public_key());
        let event = sign_unsigned_event(signer, unsigned).await?;
        let encoded = STANDARD.encode(event.as_json());

        let response = match client
            .put(upload_url)
            .header(reqwest::header::AUTHORIZATION, format!("Nostr {encoded}"))
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(ciphertext.to_vec())
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => {
                last_reason = format!("host {} request failed", index + 1);
                continue;
            }
        };

        if response.status() != reqwest::StatusCode::OK
            && response.status() != reqwest::StatusCode::CREATED
        {
            let status = response.status().as_u16();
            let reason = response
                .headers()
                .get(X_REASON)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.chars().take(256).collect::<String>())
                .filter(|value| !value.is_empty());
            last_reason = match reason {
                Some(reason) => format!("host {} returned HTTP {status}: {reason}", index + 1),
                None => format!("host {} returned HTTP {status}", index + 1),
            };
            continue;
        }

        let descriptor: BlobDescriptor = match response.json().await {
            Ok(descriptor) => descriptor,
            Err(_) => {
                last_reason = format!("host {} returned a malformed descriptor", index + 1);
                continue;
            }
        };
        return Ok(descriptor.url.to_string());
    }

    Err(failed(last_reason))
}

fn failed(reason: impl Into<String>) -> DaemonError {
    DaemonError::BlobUploadFailed {
        reason: reason.into(),
    }
}
