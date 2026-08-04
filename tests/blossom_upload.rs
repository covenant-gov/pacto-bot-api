#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! U7: Blossom upload authorization, byte fidelity, and ordered failover.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use nostr::{Event, JsonUtil};
use pacto_bot_api::attachment::blossom::upload;
use pacto_bot_api::attachment::crypto::sha256_hex;
use pacto_bot_api::errors::DaemonError;
use pacto_bot_api::signer::LocalKey;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn signer() -> LocalKey {
    LocalKey::parse(&"01".repeat(32)).expect("valid fixed local key")
}

fn descriptor(url: &str, body: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "url": url,
        "sha256": sha256_hex(body),
        "size": body.len(),
        "type": null,
        "uploaded": 1_700_000_000u64
    })
}

#[tokio::test]
async fn created_response_returns_url_and_sends_exact_authorized_ciphertext() {
    let server = MockServer::start().await;
    let ciphertext = b"ciphertext bytes including \0 binary";
    let returned_url = "https://cdn.example/blob";
    Mock::given(method("PUT"))
        .and(path("/upload"))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(descriptor(returned_url, ciphertext)),
        )
        .mount(&server)
        .await;

    let result = upload(&[server.uri()], &signer(), ciphertext)
        .await
        .expect("upload succeeds");
    assert_eq!(result, returned_url);

    let requests = server
        .received_requests()
        .await
        .expect("request recording enabled");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.body.as_slice(), ciphertext);
    assert_eq!(
        request
            .headers
            .get("content-type")
            .expect("content-type")
            .to_str()
            .expect("header text"),
        "application/octet-stream"
    );

    let authorization = request
        .headers
        .get("authorization")
        .expect("authorization")
        .to_str()
        .expect("header text")
        .strip_prefix("Nostr ")
        .expect("Nostr scheme");
    let event_json = String::from_utf8(STANDARD.decode(authorization).expect("standard base64"))
        .expect("event JSON utf8");
    let event = Event::from_json(event_json).expect("valid signed event");
    assert_eq!(event.kind.as_u16(), 24_242);
    assert!(event.verify().is_ok());

    let action = event
        .tags
        .iter()
        .find(|tag| tag.kind().as_str() == "t")
        .and_then(|tag| tag.content());
    let hash = event
        .tags
        .iter()
        .find(|tag| tag.kind().as_str() == "x")
        .and_then(|tag| tag.content());
    let expiration = event
        .tags
        .iter()
        .find(|tag| tag.kind().as_str() == "expiration")
        .and_then(|tag| tag.content())
        .and_then(|value| value.parse::<u64>().ok());
    assert_eq!(action, Some("upload"));
    assert_eq!(hash, Some(sha256_hex(ciphertext).as_str()));
    assert_eq!(expiration, Some(event.created_at.as_u64() + 300));
}

#[tokio::test]
async fn already_stored_ok_response_returns_url() {
    let server = MockServer::start().await;
    let ciphertext = b"already stored";
    Mock::given(method("PUT"))
        .and(path("/upload"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(descriptor("https://cdn.example/existing", ciphertext)),
        )
        .mount(&server)
        .await;

    let result = upload(&[server.uri()], &signer(), ciphertext)
        .await
        .expect("200 accepted");
    assert_eq!(result, "https://cdn.example/existing");
}

#[tokio::test]
async fn unsupported_first_host_falls_over_to_second() {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    let ciphertext = b"failover ciphertext";
    Mock::given(method("PUT"))
        .and(path("/upload"))
        .respond_with(
            ResponseTemplate::new(415).insert_header("X-Reason", "ciphertext unsupported"),
        )
        .mount(&first)
        .await;
    Mock::given(method("PUT"))
        .and(path("/upload"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(descriptor("https://cdn.example/fallback", ciphertext)),
        )
        .mount(&second)
        .await;

    let result = upload(&[first.uri(), second.uri()], &signer(), ciphertext)
        .await
        .expect("second host succeeds");
    assert_eq!(result, "https://cdn.example/fallback");
    assert_eq!(first.received_requests().await.unwrap().len(), 1);
    assert_eq!(second.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn malformed_or_failed_hosts_return_safe_distinct_error() {
    let malformed = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/upload"))
        .respond_with(ResponseTemplate::new(201).set_body_string("not json"))
        .mount(&malformed)
        .await;

    let error = upload(&[malformed.uri()], &signer(), b"secret-free ciphertext")
        .await
        .expect_err("malformed descriptor fails");
    assert!(matches!(error, DaemonError::BlobUploadFailed { .. }));
    let message = error.to_string();
    assert!(!message.contains("secret-free ciphertext"));
    assert!(!message.contains(&"01".repeat(32)));
}
