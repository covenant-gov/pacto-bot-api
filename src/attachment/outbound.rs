//! Outbound attachment source validation, encryption, upload, and rumor build.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use nostr::{EventBuilder, EventId, Kind, PublicKey, Tag, TagKind, UnsignedEvent};
use tracing::warn;

use crate::attachment::blossom;
use crate::attachment::crypto::{AttachmentKey, ENCRYPTION_ALGORITHM, encrypt, sha256_hex};
use crate::attachment::mime::sniff_mime;
use crate::errors::{DaemonError, JsonRpcError};
use crate::signer::Signer;
use crate::spool::Spool;
use crate::transport::protocol::MAX_INLINE_ATTACHMENT_BYTES;

/// Exactly one JSON-RPC attachment source.
#[derive(Debug, Clone)]
pub enum AttachmentSource {
    InlineBase64(String),
    SpoolPath(String),
}

/// Optional sender metadata passed through to the kind:15 rumor.
#[derive(Debug, Clone, Default)]
pub struct AttachmentMetadata {
    pub filename: Option<String>,
    pub blurhash: Option<String>,
    pub dim: Option<String>,
    pub reply_to: Option<String>,
}

/// A fully uploaded kind:15 rumor, ready for gift-wrap or MLS publication.
#[derive(Debug)]
pub struct PreparedAttachment {
    pub rumor: UnsignedEvent,
    cleanup_path: Option<PathBuf>,
}

/// Shared outbound attachment services configured at daemon startup.
#[derive(Debug, Clone)]
pub struct OutboundAttachmentProcessor {
    spool: Arc<Spool>,
    max_plaintext_bytes: u64,
    blob_servers: Vec<String>,
}

impl OutboundAttachmentProcessor {
    pub fn new(spool: Arc<Spool>, max_plaintext_bytes: u64, blob_servers: Vec<String>) -> Self {
        Self {
            spool,
            max_plaintext_bytes,
            blob_servers,
        }
    }

    pub fn spool_dir(&self) -> &std::path::Path {
        self.spool.outbound_root()
    }

    /// Validate and select exactly one source from JSON-RPC params.
    pub fn source_from_params(
        spool_path: Option<&str>,
        inline_base64: Option<&str>,
    ) -> Result<AttachmentSource, DaemonError> {
        match (spool_path, inline_base64) {
            (Some(path), None) if !path.is_empty() => {
                Ok(AttachmentSource::SpoolPath(path.to_string()))
            }
            (None, Some(encoded)) if !encoded.is_empty() => {
                Ok(AttachmentSource::InlineBase64(encoded.to_string()))
            }
            _ => Err(invalid_params(
                "exactly one of spool_path or inline_base64 is required",
            )),
        }
    }

    /// Validate optional passthrough metadata before any read, encryption, or upload.
    pub fn validate_metadata(metadata: &AttachmentMetadata) -> Result<(), DaemonError> {
        if let Some(dim) = &metadata.dim {
            let Some((width, height)) = dim.split_once('x') else {
                return Err(invalid_params("dim must have WIDTHxHEIGHT form"));
            };
            let valid = width.parse::<u32>().is_ok()
                && height.parse::<u32>().is_ok()
                && !width.is_empty()
                && !height.is_empty();
            if !valid {
                return Err(invalid_params("dim must have WIDTHxHEIGHT form"));
            }
        }
        Ok(())
    }

    /// Load and cap plaintext, encrypt it under fresh material, upload the
    /// ciphertext, and construct the exact kind:15 rumor the app consumes.
    pub async fn prepare(
        &self,
        signer: &dyn Signer,
        source: AttachmentSource,
        metadata: AttachmentMetadata,
        recipient: Option<PublicKey>,
    ) -> Result<PreparedAttachment, DaemonError> {
        Self::validate_metadata(&metadata)?;
        let (plaintext, cleanup_path) = self.load_source(source).await?;
        if plaintext.len() as u64 > self.max_plaintext_bytes {
            return Err(DaemonError::AttachmentTooLarge {
                limit: self.max_plaintext_bytes,
            });
        }

        let mime_type = sniff_mime(&plaintext).to_string();
        let ox = sha256_hex(&plaintext);
        let key = AttachmentKey::generate()?;
        let ciphertext = encrypt(&key, &plaintext)?;
        let blob_url = blossom::upload(&self.blob_servers, signer, &ciphertext).await?;

        let mut builder = EventBuilder::new(Kind::Custom(15), blob_url);
        if let Some(recipient) = recipient {
            builder = builder.tag(Tag::public_key(recipient));
        }
        builder = builder
            .tag(custom_tag("file-type", mime_type))
            .tag(custom_tag("size", ciphertext.len().to_string()))
            .tag(custom_tag("encryption-algorithm", ENCRYPTION_ALGORITHM))
            .tag(custom_tag("decryption-key", key.key_hex()))
            .tag(custom_tag("decryption-nonce", key.nonce_hex()))
            .tag(custom_tag("ox", ox));
        if let Some(filename) = metadata.filename.filter(|value| !value.trim().is_empty()) {
            builder = builder.tag(custom_tag("filename", filename));
        }
        if let Some(blurhash) = metadata.blurhash.filter(|value| !value.trim().is_empty()) {
            builder = builder.tag(custom_tag("blurhash", blurhash));
        }
        if let Some(dim) = metadata.dim {
            builder = builder.tag(custom_tag("dim", dim));
        }
        if let Some(reply_to) = metadata.reply_to {
            let event_id = EventId::parse(&reply_to)
                .map_err(|_| invalid_params("reply_to must be a hex event id"))?;
            builder = builder.tag(Tag::custom(
                TagKind::e(),
                [event_id.to_hex(), String::new(), "reply".to_string()],
            ));
        }

        Ok(PreparedAttachment {
            rumor: builder.build(signer.public_key()),
            cleanup_path,
        })
    }

    /// Remove a path-sourced entry after publication succeeds. A cleanup
    /// failure is logged but does not turn an already-published send into an
    /// error that a handler might retry and duplicate.
    pub fn complete(&self, prepared: &PreparedAttachment) {
        let Some(path) = &prepared.cleanup_path else {
            return;
        };
        if let Err(error) = std::fs::remove_file(path) {
            warn!(error = %error, "failed to remove successfully sent outbound spool entry");
        }
    }

    async fn load_source(
        &self,
        source: AttachmentSource,
    ) -> Result<(Vec<u8>, Option<PathBuf>), DaemonError> {
        match source {
            AttachmentSource::InlineBase64(encoded) => {
                let decoded = STANDARD
                    .decode(encoded)
                    .map_err(|_| invalid_params("inline_base64 is not valid standard base64"))?;
                if decoded.len() > MAX_INLINE_ATTACHMENT_BYTES {
                    return Err(DaemonError::AttachmentTooLarge {
                        limit: MAX_INLINE_ATTACHMENT_BYTES as u64,
                    });
                }
                Ok((decoded, None))
            }
            AttachmentSource::SpoolPath(path) => {
                let spool = Arc::clone(&self.spool);
                let max = self.max_plaintext_bytes;
                tokio::task::spawn_blocking(move || {
                    let (canonical, mut file) = spool.resolve_outbound(&path)?;
                    let mut limited = (&mut file).take(max.saturating_add(1));
                    let capacity = usize::try_from(max.min(1024 * 1024)).unwrap_or(0);
                    let mut bytes = Vec::with_capacity(capacity);
                    limited.read_to_end(&mut bytes)?;
                    if bytes.len() as u64 > max {
                        return Err(DaemonError::AttachmentTooLarge { limit: max });
                    }
                    Ok((bytes, Some(canonical)))
                })
                .await
                .map_err(|_| invalid_params("outbound spool read task failed"))?
            }
        }
    }
}

fn custom_tag(name: &'static str, value: impl Into<String>) -> Tag {
    Tag::custom(TagKind::custom(name), [value.into()])
}

fn invalid_params(message: &'static str) -> DaemonError {
    DaemonError::JsonRpc(JsonRpcError::new(-32602, message))
}
