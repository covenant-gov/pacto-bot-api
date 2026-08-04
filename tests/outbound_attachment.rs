#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! U9: outbound attachment source, encryption, upload, and rumor contracts.

mod common;

use std::fs;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use nostr::{Keys, TagKind};
use pacto_bot_api::attachment::crypto::{AttachmentKey, decrypt};
use pacto_bot_api::attachment::outbound::{
    AttachmentMetadata, AttachmentSource, OutboundAttachmentProcessor,
};
use pacto_bot_api::errors::DaemonError;
use pacto_bot_api::signer::LocalKey;
use pacto_bot_api::spool::Spool;
use pacto_bot_api::transport::protocol::{
    JsonRpcMessage, MAX_FRAME_BYTES, MAX_INLINE_ATTACHMENT_BYTES,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn signer() -> LocalKey {
    LocalKey::parse(&"02".repeat(32)).expect("valid fixed local key")
}

async fn accepting_server(url: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/upload"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "url": url,
            "sha256": "00".repeat(32),
            "size": 1,
            "type": null,
            "uploaded": 1_700_000_000u64
        })))
        .mount(&server)
        .await;
    server
}

fn processor(
    server: &MockServer,
    max_bytes: u64,
) -> (tempfile::TempDir, Arc<Spool>, OutboundAttachmentProcessor) {
    let dir = common::tempdir().expect("tempdir");
    let spool = Arc::new(Spool::open(dir.path()).expect("open spool"));
    let processor =
        OutboundAttachmentProcessor::new(Arc::clone(&spool), max_bytes, vec![server.uri()]);
    (dir, spool, processor)
}

fn custom_tag<'a>(rumor: &'a nostr::UnsignedEvent, name: &str) -> Option<&'a str> {
    rumor
        .tags
        .iter()
        .find(|tag| tag.kind().as_str() == name)
        .and_then(|tag| tag.content())
}

#[tokio::test]
async fn inline_payload_uploads_ciphertext_and_builds_exact_kind_15_tags() {
    let returned_url = "https://cdn.example/ciphertext";
    let server = accepting_server(returned_url).await;
    let plaintext = b"\x89PNG\r\n\x1a\nsmall image bytes";
    let (_dir, _spool, processor) = processor(&server, 1024);
    let recipient = Keys::generate().public_key();
    let prepared = processor
        .prepare(
            &signer(),
            AttachmentSource::InlineBase64(STANDARD.encode(plaintext)),
            AttachmentMetadata {
                filename: Some("photo.jpg".into()),
                blurhash: Some("LEHV6nWB2yk8pyo0adR*.7kCMdnj".into()),
                dim: Some("640x480".into()),
                reply_to: None,
            },
            Some(recipient),
        )
        .await
        .expect("prepare attachment");

    assert_eq!(prepared.rumor.kind.as_u16(), 15);
    assert_eq!(prepared.rumor.content, returned_url);
    assert_eq!(custom_tag(&prepared.rumor, "file-type"), Some("image/png"));
    assert_eq!(
        custom_tag(&prepared.rumor, "encryption-algorithm"),
        Some("aes-gcm")
    );
    assert_eq!(custom_tag(&prepared.rumor, "filename"), Some("photo.jpg"));
    assert_eq!(custom_tag(&prepared.rumor, "dim"), Some("640x480"));
    assert_eq!(
        prepared
            .rumor
            .tags
            .find(TagKind::p())
            .and_then(|tag| tag.content()),
        Some(recipient.to_hex().as_str())
    );

    let key_hex = custom_tag(&prepared.rumor, "decryption-key").expect("key tag");
    let nonce_hex = custom_tag(&prepared.rumor, "decryption-nonce").expect("nonce tag");
    assert_eq!(key_hex.len(), 64);
    assert_eq!(nonce_hex.len(), 32);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let ciphertext = requests[0].body.as_slice();
    assert_eq!(
        custom_tag(&prepared.rumor, "size"),
        Some(ciphertext.len().to_string().as_str())
    );
    assert_eq!(ciphertext.len(), plaintext.len() + 16);
    let key = AttachmentKey::from_hex(key_hex, nonce_hex).expect("parse tags");
    assert_eq!(
        decrypt(&key, ciphertext).expect("decrypt upload"),
        plaintext
    );
}

#[tokio::test]
async fn identical_plaintext_uses_fresh_key_and_nonce() {
    let server = accepting_server("https://cdn.example/blob").await;
    let (_dir, _spool, processor) = processor(&server, 1024);
    let source = || AttachmentSource::InlineBase64(STANDARD.encode(b"same bytes"));
    let first = processor
        .prepare(&signer(), source(), AttachmentMetadata::default(), None)
        .await
        .unwrap();
    let second = processor
        .prepare(&signer(), source(), AttachmentMetadata::default(), None)
        .await
        .unwrap();
    assert_ne!(
        custom_tag(&first.rumor, "decryption-key"),
        custom_tag(&second.rumor, "decryption-key")
    );
    assert_ne!(
        custom_tag(&first.rumor, "decryption-nonce"),
        custom_tag(&second.rumor, "decryption-nonce")
    );
}

#[tokio::test]
async fn spool_source_is_removed_only_after_successful_completion() {
    let server = accepting_server("https://cdn.example/blob").await;
    let (_dir, spool, processor) = processor(&server, 4096);
    let path = spool.outbound_root().join("report.bin");
    fs::write(&path, vec![7u8; 2048]).expect("stage payload");

    let prepared = processor
        .prepare(
            &signer(),
            AttachmentSource::SpoolPath(path.to_string_lossy().into_owned()),
            AttachmentMetadata::default(),
            None,
        )
        .await
        .expect("prepare staged payload");
    assert!(path.exists(), "publication has not completed yet");
    processor.complete(&prepared);
    assert!(!path.exists(), "successful send removes staged source");
}

#[tokio::test]
async fn failed_upload_leaves_spool_source_for_retention_sweep() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/upload"))
        .respond_with(ResponseTemplate::new(415))
        .mount(&server)
        .await;
    let (_dir, spool, processor) = processor(&server, 4096);
    let path = spool.outbound_root().join("retry.bin");
    fs::write(&path, b"retry later").expect("stage payload");

    let result = processor
        .prepare(
            &signer(),
            AttachmentSource::SpoolPath(path.to_string_lossy().into_owned()),
            AttachmentMetadata::default(),
            None,
        )
        .await;
    assert!(matches!(result, Err(DaemonError::BlobUploadFailed { .. })));
    assert!(path.exists());
}

#[test]
fn payload_source_requires_exactly_one_input() {
    assert!(OutboundAttachmentProcessor::source_from_params(None, None).is_err());
    assert!(OutboundAttachmentProcessor::source_from_params(Some("file"), Some("eA==")).is_err());
    assert!(OutboundAttachmentProcessor::source_from_params(Some("file"), None).is_ok());
    assert!(OutboundAttachmentProcessor::source_from_params(None, Some("eA==")).is_ok());
}

#[test]
fn malformed_dimensions_are_invalid_params() {
    let metadata = AttachmentMetadata {
        dim: Some("640 by 480".into()),
        ..Default::default()
    };
    let error = OutboundAttachmentProcessor::validate_metadata(&metadata)
        .expect_err("malformed dimensions fail");
    assert_eq!(error.to_json_rpc_code(), -32602);
}

#[tokio::test]
async fn oversized_payload_is_rejected_before_upload() {
    let server = accepting_server("https://cdn.example/blob").await;
    let (_dir, _spool, processor) = processor(&server, 32);
    let result = processor
        .prepare(
            &signer(),
            AttachmentSource::InlineBase64(STANDARD.encode(vec![0u8; 33])),
            AttachmentMetadata::default(),
            None,
        )
        .await;
    assert!(matches!(
        result,
        Err(DaemonError::AttachmentTooLarge { .. })
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[test]
fn maximum_inline_request_stays_below_unchanged_frame_cap() {
    let encoded = STANDARD.encode(vec![0u8; MAX_INLINE_ATTACHMENT_BYTES]);
    let request = JsonRpcMessage::request(
        1.into(),
        "agent.send_attachment",
        Some(serde_json::json!({
            "bot_id": "frame-bot",
            "recipient": Keys::generate().public_key().to_hex(),
            "inline_base64": encoded,
            "filename": "maximum.bin",
            "blurhash": "LEHV6nWB2yk8pyo0adR*.7kCMdnj",
            "dim": "640x480",
            "reply_to": "00".repeat(32),
        })),
    );
    assert!(serde_json::to_vec(&request).unwrap().len() + 1 < MAX_FRAME_BYTES);

    let too_large = STANDARD.encode(vec![0u8; MAX_INLINE_ATTACHMENT_BYTES + 1]);
    let source = OutboundAttachmentProcessor::source_from_params(None, Some(&too_large)).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime.block_on(accepting_server("https://cdn.example/blob"));
    let (_dir, _spool, processor) = processor(&server, (MAX_INLINE_ATTACHMENT_BYTES + 1) as u64);
    let result =
        runtime.block_on(processor.prepare(&signer(), source, AttachmentMetadata::default(), None));
    assert!(matches!(
        result,
        Err(DaemonError::AttachmentTooLarge { .. })
    ));
}
