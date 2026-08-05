//! Hardened inbound kind:15 attachment processing.
//!
//! The rumor URL and ciphertext are controlled by a counterparty. Fetching is
//! therefore isolated behind [`BlobFetcher`], with the production fetcher
//! enforcing HTTPS, DNS/address restrictions, no redirects, a total timeout,
//! and a streamed byte budget before any plaintext reaches the spool.

use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nostr::{TagKind, Tags, UnsignedEvent};
use reqwest::redirect::Policy;
use tracing::warn;

use crate::attachment::crypto::{AttachmentKey, decrypt, sha256_hex};
use crate::attachment::mime::extension_for_mime;
use crate::errors::DaemonError;
use crate::events::AttachmentPayload;
use crate::spool::{INBOUND_RETENTION, Spool};

/// AES-GCM appends a 16-byte authentication tag to the plaintext.
const GCM_TAG_BYTES: u64 = 16;
/// Bound the complete connect/request/body operation for a counterparty host.
const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Fetch ciphertext under an explicit byte budget.
///
/// This seam keeps attachment parsing/decryption independently testable while
/// production always uses [`HardenedBlobFetcher`]. Implementations must return
/// an error without embedding the URL or response body in it.
#[async_trait]
pub trait BlobFetcher: Send + Sync {
    async fn fetch(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, DaemonError>;
}

/// Production HTTPS fetcher with SSRF and resource-exhaustion defenses.
#[derive(Debug, Clone)]
pub struct HardenedBlobFetcher {
    timeout: Duration,
}

impl Default for HardenedBlobFetcher {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_FETCH_TIMEOUT,
        }
    }
}

impl HardenedBlobFetcher {
    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

#[async_trait]
impl BlobFetcher for HardenedBlobFetcher {
    async fn fetch(&self, raw_url: &str, max_bytes: u64) -> Result<Vec<u8>, DaemonError> {
        let url = reqwest::Url::parse(raw_url).map_err(|_| invalid("invalid blob URL"))?;
        if url.scheme() != "https" {
            return Err(invalid("blob URL must use https"));
        }

        let host = url
            .host_str()
            .ok_or_else(|| invalid("blob URL missing host"))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| invalid("blob URL missing port"))?;
        // `lookup_host` runs the OS resolver on the shared blocking pool with
        // no deadline of its own. A counterparty naming a host whose
        // authoritative server never answers would otherwise pin one of those
        // threads for the resolver's full retry budget, and that pool also
        // backs MLS SQLite work and outbound spool reads.
        let addrs = tokio::time::timeout(self.timeout, resolve_safe_addrs(host, port))
            .await
            .map_err(|_| invalid("blob host resolution timed out"))??;

        // Pin this request to the addresses we checked. This both preserves TLS
        // hostname verification and closes the DNS-rebinding gap between a
        // separate lookup and reqwest's connection attempt.
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(self.timeout)
            .resolve_to_addrs(host, &addrs)
            .build()
            .map_err(|_| invalid("blob fetch client setup failed"))?;

        let mut response = client
            .get(url)
            .send()
            .await
            .map_err(|_| invalid("blob fetch failed"))?;
        if !response.status().is_success() {
            return Err(invalid("blob fetch returned unsuccessful status"));
        }
        if response
            .content_length()
            .is_some_and(|length| length > max_bytes)
        {
            return Err(DaemonError::AttachmentTooLarge { limit: max_bytes });
        }

        // Preallocate from the advertised length only up to a bounded amount.
        // `content_length` is counterparty-controlled, so trusting it up to
        // `max_bytes` lets one unsent request reserve the whole cap before any
        // body byte arrives; the streaming loop below still enforces the real
        // ceiling as bytes land.
        const MAX_PREALLOC: u64 = 64 * 1024;
        let capacity = usize::try_from(
            response
                .content_length()
                .unwrap_or(0)
                .min(max_bytes)
                .min(MAX_PREALLOC),
        )
        .unwrap_or(0);
        let mut body = Vec::with_capacity(capacity);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| invalid("blob body read failed"))?
        {
            let next_len = (body.len() as u64).saturating_add(chunk.len() as u64);
            if next_len > max_bytes {
                return Err(DaemonError::AttachmentTooLarge { limit: max_bytes });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

/// Parse, fetch, decrypt, verify, and spool inbound kind:15 rumors.
#[derive(Clone)]
pub struct InboundAttachmentProcessor {
    spool: Arc<Spool>,
    fetcher: Arc<dyn BlobFetcher>,
    max_plaintext_bytes: u64,
    outbound_retention: Duration,
}

impl std::fmt::Debug for InboundAttachmentProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboundAttachmentProcessor")
            .field("spool", &self.spool)
            .field("max_plaintext_bytes", &self.max_plaintext_bytes)
            .field("outbound_retention", &self.outbound_retention)
            .finish_non_exhaustive()
    }
}

impl InboundAttachmentProcessor {
    pub fn new(spool: Arc<Spool>, max_plaintext_bytes: u64, outbound_retention: Duration) -> Self {
        Self::with_fetcher(
            spool,
            max_plaintext_bytes,
            outbound_retention,
            Arc::new(HardenedBlobFetcher::default()),
        )
    }

    pub fn with_fetcher(
        spool: Arc<Spool>,
        max_plaintext_bytes: u64,
        outbound_retention: Duration,
        fetcher: Arc<dyn BlobFetcher>,
    ) -> Self {
        Self {
            spool,
            fetcher,
            max_plaintext_bytes,
            outbound_retention,
        }
    }

    /// Process one decrypted kind:15 rumor into handler-facing attachment data.
    pub async fn process_rumor(
        &self,
        rumor: &UnsignedEvent,
    ) -> Result<AttachmentPayload, DaemonError> {
        let tags = AttachmentTags::parse(&rumor.tags)?;
        let max_ciphertext_bytes = self.max_plaintext_bytes.saturating_add(GCM_TAG_BYTES);
        if tags
            .reported_ciphertext_size
            .is_some_and(|size| size > max_ciphertext_bytes)
        {
            return Err(DaemonError::AttachmentTooLarge {
                limit: self.max_plaintext_bytes,
            });
        }

        let key = AttachmentKey::from_hex(&tags.key_hex, &tags.nonce_hex)?;
        let ciphertext = self
            .fetcher
            .fetch(&rumor.content, max_ciphertext_bytes)
            .await?;

        let spool = Arc::clone(&self.spool);
        let max_plaintext_bytes = self.max_plaintext_bytes;
        let outbound_retention = self.outbound_retention;
        tokio::task::spawn_blocking(move || {
            let plaintext = decrypt(&key, &ciphertext)?;
            if plaintext.len() as u64 > max_plaintext_bytes {
                return Err(DaemonError::AttachmentTooLarge {
                    limit: max_plaintext_bytes,
                });
            }
            if let Some(expected) = &tags.ox
                && !sha256_hex(&plaintext).eq_ignore_ascii_case(expected)
            {
                return Err(invalid("plaintext hash mismatch"));
            }

            let expires_at = SystemTime::now()
                .checked_add(INBOUND_RETENTION)
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
                .ok_or_else(|| invalid("system clock cannot represent attachment expiry"))?;

            let extension = extension_for_mime(&tags.mime_type);
            let (path, mut file) = spool.create_inbound(extension)?;
            if let Err(error) = file.write_all(&plaintext).and_then(|()| file.flush()) {
                spool.discard_inbound(&path);
                return Err(DaemonError::Io(error));
            }

            // Sweep only after the new file is complete. The amortized gate
            // makes this cheap on the common path.
            //
            // A sweep failure is unrelated housekeeping -- it means a
            // directory listing failed, not that this payload is bad. Log it
            // and still deliver; discarding the file we just wrote and
            // verified would fail the delivery as though the attachment
            // itself were invalid.
            if let Err(error) = spool.sweep(outbound_retention) {
                warn!(error = %error, "attachment spool retention sweep failed");
            }

            Ok(AttachmentPayload {
                mime_type: tags.mime_type,
                size: plaintext.len() as u64,
                ox: tags.ox,
                filename: tags.filename,
                blurhash: tags.blurhash,
                dim: tags.dim,
                path: path.to_string_lossy().into_owned(),
                expires_at,
            })
        })
        .await
        .map_err(|_| invalid("attachment processing task failed"))?
    }
}

#[derive(Debug)]
struct AttachmentTags {
    mime_type: String,
    key_hex: String,
    nonce_hex: String,
    reported_ciphertext_size: Option<u64>,
    ox: Option<String>,
    filename: Option<String>,
    blurhash: Option<String>,
    dim: Option<String>,
}

impl AttachmentTags {
    fn parse(tags: &Tags) -> Result<Self, DaemonError> {
        let required = |name: &'static str| {
            custom_tag(tags, name)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    invalid(match name {
                        "file-type" => "missing file-type tag",
                        "decryption-key" => "missing decryption-key tag",
                        "decryption-nonce" => "missing decryption-nonce tag",
                        _ => "missing required attachment tag",
                    })
                })
        };
        let reported_ciphertext_size = custom_tag(tags, "size")
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| invalid("malformed size tag"))
            })
            .transpose()?;

        Ok(Self {
            mime_type: required("file-type")?,
            key_hex: required("decryption-key")?,
            nonce_hex: required("decryption-nonce")?,
            reported_ciphertext_size,
            ox: custom_tag(tags, "ox").map(str::to_owned),
            filename: custom_tag(tags, "filename").map(str::to_owned),
            blurhash: custom_tag(tags, "blurhash").map(str::to_owned),
            dim: custom_tag(tags, "dim").map(str::to_owned),
        })
    }
}

fn custom_tag<'a>(tags: &'a Tags, name: &str) -> Option<&'a str> {
    tags.iter()
        .find(|tag| tag.kind() == TagKind::custom(name))
        .and_then(|tag| tag.content())
}

async fn resolve_safe_addrs(host: &str, port: u16) -> Result<Vec<SocketAddr>, DaemonError> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| invalid("blob host resolution failed"))?
        .collect();
    if addrs.is_empty() || addrs.iter().any(|addr| forbidden_ip(addr.ip())) {
        return Err(invalid("blob host resolves to a forbidden address"));
    }
    Ok(addrs)
}

/// Reject an address a counterparty must never be able to make the daemon
/// reach: loopback, link-local (including the cloud metadata endpoint),
/// private, unspecified, multicast, and broadcast.
///
/// The address is canonicalized first. An IPv4-mapped IPv6 literal such as
/// `::ffff:169.254.169.254` answers `false` to every `Ipv6Addr` predicate
/// below while a dual-stack socket still routes it to the embedded IPv4
/// target, so checking the v6 form directly would let a counterparty-supplied
/// AAAA record or bracketed URL host walk straight past this filter.
fn forbidden_ip(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_broadcast()
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unicast_link_local()
                || ip.is_unique_local()
                || ip.is_unspecified()
                || ip.is_multicast()
        }
    }
}

fn invalid(category: &'static str) -> DaemonError {
    DaemonError::AttachmentInvalid {
        category: category.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbids_private_loopback_and_link_local_addresses() {
        for address in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.2",
            "169.254.1.1",
            "::1",
            "fc00::1",
            "fe80::1",
        ] {
            let ip: IpAddr = address.parse().expect("valid test IP");
            assert!(forbidden_ip(ip), "{address} must be forbidden");
        }
        let public: IpAddr = "1.1.1.1".parse().expect("valid test IP");
        assert!(!forbidden_ip(public));
    }

    /// An IPv4-mapped IPv6 address answers `false` to every `Ipv6Addr`
    /// predicate while a dual-stack socket still routes it to the embedded
    /// IPv4 target. Before `forbidden_ip` canonicalized, a counterparty could
    /// publish an AAAA record of `::ffff:169.254.169.254` (or use that as a
    /// bracketed URL host) and drive the daemon straight at the cloud
    /// metadata endpoint, fully bypassing the SSRF filter.
    #[test]
    fn forbids_ipv4_mapped_ipv6_forms_of_private_addresses() {
        for address in [
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:10.0.0.1",
            "::ffff:172.16.0.1",
            "::ffff:192.168.1.1",
            "::ffff:0.0.0.0",
        ] {
            let ip: IpAddr = address.parse().expect("valid test IP");
            assert!(
                forbidden_ip(ip),
                "{address} is an IPv4-mapped private address and must be forbidden"
            );
        }
        let mapped_public: IpAddr = "::ffff:1.1.1.1".parse().expect("valid test IP");
        assert!(
            !forbidden_ip(mapped_public),
            "a mapped public address must still be allowed"
        );
    }
}
