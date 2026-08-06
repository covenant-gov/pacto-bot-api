#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! U5: inbound attachment parsing, verification, and spool handoff.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use nostr::{Keys, Kind, Tag, Timestamp, UnsignedEvent};
use pacto_bot_api::attachment::crypto::{AttachmentKey, encrypt, sha256_hex};
use pacto_bot_api::attachment::inbound::{
    BlobFetcher, HardenedBlobFetcher, InboundAttachmentProcessor,
};
use pacto_bot_api::errors::DaemonError;
use pacto_bot_api::spool::{INBOUND_RETENTION, Spool};

#[derive(Debug)]
struct StaticFetcher {
    body: Vec<u8>,
    calls: AtomicUsize,
}

impl StaticFetcher {
    fn new(body: Vec<u8>) -> Self {
        Self {
            body,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl BlobFetcher for StaticFetcher {
    async fn fetch(&self, _url: &str, max_bytes: u64) -> Result<Vec<u8>, DaemonError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.body.len() as u64 > max_bytes {
            return Err(DaemonError::AttachmentTooLarge { limit: max_bytes });
        }
        Ok(self.body.clone())
    }
}

fn tag(name: &str, value: impl Into<String>) -> Tag {
    Tag::parse([name.to_string(), value.into()]).expect("valid custom tag")
}

fn encrypted_rumor(
    plaintext: &[u8],
    ox: Option<String>,
    extra_tags: impl IntoIterator<Item = Tag>,
) -> (UnsignedEvent, Vec<u8>, String, String) {
    let key = AttachmentKey::generate().expect("generate key");
    let key_hex = key.key_hex();
    let nonce_hex = key.nonce_hex();
    let ciphertext = encrypt(&key, plaintext).expect("encrypt fixture");
    let mut tags = vec![
        tag("file-type", "image/png"),
        tag("size", ciphertext.len().to_string()),
        tag("decryption-key", key_hex.clone()),
        tag("decryption-nonce", nonce_hex.clone()),
    ];
    if let Some(ox) = ox {
        tags.push(tag("ox", ox));
    }
    tags.extend(extra_tags);
    let rumor = UnsignedEvent::new(
        Keys::generate().public_key(),
        Timestamp::now(),
        Kind::Custom(15),
        tags,
        "https://blobs.example/ciphertext",
    );
    (rumor, ciphertext, key_hex, nonce_hex)
}

fn processor(
    max_bytes: u64,
    fetcher: Arc<StaticFetcher>,
) -> (tempfile::TempDir, Arc<Spool>, InboundAttachmentProcessor) {
    let dir = common::tempdir().expect("tempdir");
    let spool = Arc::new(Spool::open(dir.path()).expect("open spool"));
    let processor = InboundAttachmentProcessor::with_fetcher(
        Arc::clone(&spool),
        max_bytes,
        Duration::from_secs(86_400),
        fetcher,
    );
    (dir, spool, processor)
}

#[tokio::test]
async fn verified_attachment_is_spooled_with_metadata_and_owner_only_mode() {
    let plaintext = b"\x89PNG\r\n\x1a\nverified image bytes";
    let ox = sha256_hex(plaintext);
    let (rumor, ciphertext, _, _) = encrypted_rumor(
        plaintext,
        Some(ox.clone()),
        [
            tag("filename", "../../sender-name.png"),
            tag("blurhash", "LEHV6nWB2yk8pyo0adR*.7kCMdnj"),
            tag("dim", "640x480"),
        ],
    );
    let fetcher = Arc::new(StaticFetcher::new(ciphertext));
    let (_dir, spool, processor) = processor(1024, fetcher);

    let before = Timestamp::now().as_secs();
    let payload = processor
        .process_rumor(&rumor)
        .await
        .expect("process rumor");
    let after = Timestamp::now().as_secs();

    assert_eq!(fs::read(&payload.path).expect("read spool file"), plaintext);
    assert_eq!(payload.mime_type, "image/png");
    assert_eq!(payload.size, plaintext.len() as u64);
    assert_eq!(payload.ox.as_deref(), Some(ox.as_str()));
    assert_eq!(payload.filename.as_deref(), Some("../../sender-name.png"));
    assert_eq!(
        payload.blurhash.as_deref(),
        Some("LEHV6nWB2yk8pyo0adR*.7kCMdnj")
    );
    assert_eq!(payload.dim.as_deref(), Some("640x480"));
    assert!(payload.path.ends_with(".png"));
    assert!(std::path::Path::new(&payload.path).starts_with(spool.inbound_root()));
    assert_eq!(
        fs::metadata(&payload.path)
            .expect("stat spool file")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(payload.expires_at >= before + INBOUND_RETENTION.as_secs());
    assert!(payload.expires_at <= after + INBOUND_RETENTION.as_secs());
}

#[tokio::test]
async fn missing_required_tags_are_rejected_before_fetch() {
    let plaintext = b"payload";
    for missing in ["file-type", "decryption-key", "decryption-nonce"] {
        let (mut rumor, ciphertext, _, _) =
            encrypted_rumor(plaintext, Some(sha256_hex(plaintext)), []);
        let retained: Vec<Tag> = rumor
            .tags
            .iter()
            .filter(|item| item.kind().as_str() != missing)
            .cloned()
            .collect();
        rumor.tags = nostr::Tags::from_list(retained);
        let fetcher = Arc::new(StaticFetcher::new(ciphertext));
        let (_dir, spool, processor) = processor(1024, Arc::clone(&fetcher));

        let error = processor
            .process_rumor(&rumor)
            .await
            .expect_err("missing tag must fail");
        assert!(matches!(error, DaemonError::AttachmentInvalid { .. }));
        assert_eq!(fetcher.calls(), 0);
        assert_eq!(spool.inbound_entry_count(), 0);
    }
}

#[tokio::test]
async fn hash_mismatch_is_rejected_without_leaving_a_spool_file() {
    let plaintext = b"verified bytes must differ";
    let (rumor, ciphertext, key_hex, nonce_hex) =
        encrypted_rumor(plaintext, Some("00".repeat(32)), []);
    let fetcher = Arc::new(StaticFetcher::new(ciphertext));
    let (_dir, spool, processor) = processor(1024, fetcher);

    let error = processor
        .process_rumor(&rumor)
        .await
        .expect_err("hash mismatch must fail");
    let message = error.to_string();
    assert!(matches!(error, DaemonError::AttachmentInvalid { .. }));
    assert!(!message.contains(&key_hex));
    assert!(!message.contains(&nonce_hex));
    assert!(!message.contains("verified bytes"));
    assert_eq!(spool.inbound_entry_count(), 0);
}

#[tokio::test]
async fn absent_ox_is_allowed() {
    let plaintext = b"authenticated by GCM";
    let (rumor, ciphertext, _, _) = encrypted_rumor(plaintext, None, []);
    let fetcher = Arc::new(StaticFetcher::new(ciphertext));
    let (_dir, _spool, processor) = processor(1024, fetcher);

    let payload = processor
        .process_rumor(&rumor)
        .await
        .expect("process rumor");
    assert_eq!(payload.ox, None);
    assert_eq!(fs::read(payload.path).expect("read file"), plaintext);
}

#[tokio::test]
async fn reported_oversize_is_rejected_without_fetch() {
    let plaintext = b"small";
    let (mut rumor, ciphertext, _, _) = encrypted_rumor(plaintext, None, []);
    let retained: Vec<Tag> = rumor
        .tags
        .iter()
        .filter(|item| item.kind().as_str() != "size")
        .cloned()
        .chain([tag("size", "1041")])
        .collect();
    rumor.tags = nostr::Tags::from_list(retained);
    let fetcher = Arc::new(StaticFetcher::new(ciphertext));
    let (_dir, spool, processor) = processor(1024, Arc::clone(&fetcher));

    assert!(matches!(
        processor.process_rumor(&rumor).await,
        Err(DaemonError::AttachmentTooLarge { .. })
    ));
    assert_eq!(fetcher.calls(), 0);
    assert_eq!(spool.inbound_entry_count(), 0);
}

#[tokio::test]
async fn actual_oversize_is_cut_off_without_spooling() {
    let plaintext = vec![7u8; 1025];
    let (mut rumor, ciphertext, _, _) = encrypted_rumor(&plaintext, None, []);
    let retained: Vec<Tag> = rumor
        .tags
        .iter()
        .filter(|item| item.kind().as_str() != "size")
        .cloned()
        .chain([tag("size", "32")])
        .collect();
    rumor.tags = nostr::Tags::from_list(retained);
    let fetcher = Arc::new(StaticFetcher::new(ciphertext));
    let (_dir, spool, processor) = processor(1024, fetcher);

    assert!(matches!(
        processor.process_rumor(&rumor).await,
        Err(DaemonError::AttachmentTooLarge { .. })
    ));
    assert_eq!(spool.inbound_entry_count(), 0);
}

#[tokio::test]
async fn hardened_fetcher_rejects_http_before_request() {
    let fetcher = HardenedBlobFetcher::with_timeout(Duration::from_millis(50));
    let error = fetcher
        .fetch("http://127.0.0.1:9/blob", 1024)
        .await
        .expect_err("http must be rejected");
    assert!(matches!(error, DaemonError::AttachmentInvalid { .. }));
}

#[tokio::test]
async fn hardened_fetcher_rejects_loopback_resolution() {
    let fetcher = HardenedBlobFetcher::with_timeout(Duration::from_millis(50));
    let error = fetcher
        .fetch("https://localhost:9/blob", 1024)
        .await
        .expect_err("loopback must be rejected");
    assert!(matches!(error, DaemonError::AttachmentInvalid { .. }));
}
