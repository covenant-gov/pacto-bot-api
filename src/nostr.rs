//! Nostr relay client wrapper.
//!
//! Provides a thin, bot-aware layer over [`nostr_sdk::Client`] for sending and
//! receiving NIP-17 / NIP-59 direct messages (gift wraps, kind 1059).

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use nostr::event::tag::Tag;
use nostr::nips::nip44::Version;
use nostr::nips::{nip44, nip59};
use nostr::secp256k1::schnorr::Signature;
use nostr::{
    Event, EventBuilder, EventId, Filter, Keys, Kind, PublicKey, SubscriptionId, Timestamp,
    ToBech32, UnsignedEvent,
};
use nostr_sdk::{Client, RelayPoolNotification};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::{RwLock, Semaphore};
use tokio_stream::Stream;
use tokio_stream::wrappers::UnboundedReceiverStream;

use tracing::{debug, error, info, warn};

use crate::attachment::inbound::InboundAttachmentProcessor;
use crate::config::BotConfig;
use crate::diagnostics::Diagnostics;
use crate::errors::DaemonError;
use crate::events::{AgentEvent, EventType, ReactionPayload};
use crate::mls;
use crate::mls::MlsEngineHandle;
use crate::nostr_tags;
use crate::signer::Signer;

/// Bot signer storage: maps recipient public key to bot id and signer.
type BotSigners = HashMap<PublicKey, (String, Arc<dyn Signer>)>;

/// Shared slot holding the sender half of the stream `receive_events()`
/// handed out, so `deliver_locally` can push into that same stream.
type LocalDeliveryTx =
    Arc<std::sync::Mutex<Option<UnboundedSender<Result<AgentEvent, DaemonError>>>>>;

/// Wrapper around [`nostr_sdk::Client`] providing Pacto-specific relay operations.
#[derive(Clone)]
pub struct NostrClient {
    client: Client,
    signers: Arc<RwLock<BotSigners>>,
    mls_engines: Arc<RwLock<HashMap<PublicKey, (String, MlsEngineHandle)>>>,
    diagnostics: Option<Diagnostics>,
    attachment_processor: Option<Arc<InboundAttachmentProcessor>>,
    /// Same-daemon multi-bot delivery correction (see `deliver_locally`):
    /// set once `receive_events()` is called, so a later self-published
    /// event addressed to another locally managed bot can still reach the
    /// same stream `receive_events()` returned.
    local_delivery_tx: LocalDeliveryTx,
    /// Each configured bot's own relay URLs, used only to scope
    /// `deliver_locally`'s same-daemon delivery correction to a recipient
    /// that is genuinely reachable through at least one connected relay
    /// (see `deliver_locally`) -- it must not synthesize delivery to a bot
    /// whose entire configured relay set is unreachable.
    bot_relays: Arc<RwLock<HashMap<PublicKey, Vec<String>>>>,
}

impl std::fmt::Debug for NostrClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NostrClient")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

/// Parsed mention envelope shape produced by the Pacto app for squad messages.
#[derive(Debug, Deserialize)]
struct MentionEnvelope {
    kind: String,
    body: String,
    mentions: Vec<MentionEntry>,
    #[serde(rename = "pacto_virtual_bucket")]
    pacto_virtual_bucket: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MentionEntry {
    npub: String,
}

const MENTION_ENVELOPE_KIND: &str = "pacto.mentions.envelope.v1";

/// Maximum number of gift-wrap decrypt/parse tasks allowed to run
/// concurrently. An inbound `kind:1059` event spawns a task that acquires a
/// permit before running; once the constant's worth of tasks are in
/// flight, further intake notifications back-pressure on the semaphore
/// rather than spawning unboundedly, bounding memory and file-descriptor
/// growth from a hostile burst (R33).
pub const MAX_CONCURRENT_GIFT_WRAP_TASKS: usize = 32;

/// Per-event ceiling on the gift-wrap decrypt-and-parse critical path (seal
/// decrypt, rumor decode, and author-match check). A stuck or hostile
/// payload past this deadline is dropped — never retried inline — so one
/// bad event cannot stall the intake loop (R33, R48).
pub const GIFT_WRAP_PROCESS_TIMEOUT: Duration = Duration::from_secs(5);

/// Attempt to parse the decrypted MLS group message content as a structured
/// `{ kind, body, mentions, pacto_virtual_bucket }` envelope. Only succeeds
/// when `kind == "pacto.mentions.envelope.v1"`. On success, return the body,
/// the list of target npubs, and the optional virtual bucket. On any parse
/// failure, shape mismatch, or wrong kind, return the raw plaintext as the
/// body, an empty mention list, and no bucket.
fn parse_mention_envelope(plaintext: &str) -> (String, Vec<String>, Option<String>) {
    match serde_json::from_str::<MentionEnvelope>(plaintext) {
        Ok(envelope) if envelope.kind == MENTION_ENVELOPE_KIND => (
            envelope.body,
            envelope.mentions.into_iter().map(|m| m.npub).collect(),
            envelope.pacto_virtual_bucket,
        ),
        Ok(envelope) => {
            debug!(
                kind = %envelope.kind,
                plaintext_len = plaintext.len(),
                "mention envelope kind mismatch; treating as legacy content"
            );
            (plaintext.to_string(), Vec::new(), None)
        }
        Err(e) => {
            debug!(
                error = %e,
                plaintext_len = plaintext.len(),
                "failed to parse mention envelope; treating as legacy content"
            );
            (plaintext.to_string(), Vec::new(), None)
        }
    }
}

/// Owned output of the decrypt+parse phase of gift-wrap processing
/// ([`NostrClient::decrypt_gift_wrap_rumor`]), consumed by the untimed
/// continuation ([`NostrClient::finish_gift_wrap`]).
struct DecryptedGiftWrap {
    recipient: PublicKey,
    bot_id: String,
    seal_event: Event,
    rumor: UnsignedEvent,
    rumor_id: String,
}

impl NostrClient {
    /// Create a new client, add the given relays, and begin connecting.
    pub async fn new(relays: Vec<String>) -> Result<Self, DaemonError> {
        let client = Client::default();
        let this = Self {
            client,
            signers: Arc::new(RwLock::new(HashMap::new())),
            mls_engines: Arc::new(RwLock::new(HashMap::new())),
            diagnostics: None,
            attachment_processor: None,
            local_delivery_tx: Arc::new(std::sync::Mutex::new(None)),
            bot_relays: Arc::new(RwLock::new(HashMap::new())),
        };
        this.add_relays(&relays).await?;
        this.client.connect().await;
        Ok(this)
    }

    /// Attach a diagnostics aggregator to the client.
    ///
    /// Signature verification failures during gift-wrap processing are recorded
    /// here. [`Diagnostics`] is internally reference counted, so the same
    /// instance can be shared with the dispatch layer.
    pub fn with_diagnostics(mut self, diagnostics: Diagnostics) -> Self {
        self.diagnostics = Some(diagnostics);
        self
    }

    /// Attach the processor used for eager inbound kind:15 handling.
    pub fn with_attachment_processor(mut self, processor: Arc<InboundAttachmentProcessor>) -> Self {
        self.attachment_processor = Some(processor);
        self
    }

    /// Add relays to the underlying pool. Empty strings are skipped.
    pub async fn add_relays(&self, relays: &[String]) -> Result<(), DaemonError> {
        for url in relays {
            if url.trim().is_empty() {
                continue;
            }
            self.client
                .add_relay(url)
                .await
                .map_err(|e| DaemonError::Nostr(format!("failed to add relay {url}: {e}")))?;
        }
        Ok(())
    }

    /// Return the URLs of all relays currently configured in the pool.
    pub async fn relays(&self) -> Vec<String> {
        self.client
            .relays()
            .await
            .into_keys()
            .map(|url| url.to_string())
            .collect()
    }

    /// Return the connection status of every configured relay as a map of URL to status name.
    pub async fn relay_statuses(&self) -> HashMap<String, String> {
        self.client
            .relays()
            .await
            .into_iter()
            .map(|(url, relay)| (url.to_string(), relay.status().to_string()))
            .collect()
    }

    /// Register a signer for a bot so that incoming gift wraps addressed to
    /// `pubkey` can be decrypted.
    pub async fn add_signer(&self, pubkey: PublicKey, bot_id: String, signer: Arc<dyn Signer>) {
        self.signers.write().await.insert(pubkey, (bot_id, signer));
    }

    /// Register an MLS engine for a bot so that inbound kind:445 group
    /// messages addressed to `pubkey` can be decrypted.
    pub async fn add_mls_engine(&self, pubkey: PublicKey, bot_id: String, mls: MlsEngineHandle) {
        self.mls_engines.write().await.insert(pubkey, (bot_id, mls));
    }

    /// Record a bot's own configured relay URLs, used by
    /// `deliver_locally`'s same-daemon delivery correction to scope
    /// itself to a recipient genuinely reachable through at least one
    /// connected relay.
    pub async fn add_bot_relays(&self, pubkey: PublicKey, relays: Vec<String>) {
        self.bot_relays.write().await.insert(pubkey, relays);
    }

    /// Subscribe to kind 1059 gift wraps addressed to `npub`, optionally
    /// restricted to events with `created_at` >= `since`.
    pub async fn subscribe_bot_with_since(
        &self,
        npub: &PublicKey,
        since: Option<Timestamp>,
    ) -> Result<SubscriptionId, DaemonError> {
        let mut filter = Filter::new().kind(Kind::GiftWrap).pubkey(*npub);
        if let Some(since) = since {
            filter = filter.since(since);
        }
        let output = self
            .client
            .subscribe(filter, None)
            .await
            .map_err(|e| DaemonError::Nostr(format!("subscribe failed: {e}")))?;
        Ok(output.val)
    }

    /// Subscribe to kind 1059 gift wraps addressed to `npub`.
    pub async fn subscribe_bot(&self, npub: &PublicKey) -> Result<SubscriptionId, DaemonError> {
        self.subscribe_bot_with_since(npub, None).await
    }

    /// Subscribe to kind:445 MLS group messages addressed to `npub`, optionally
    /// restricted to events with `created_at` >= `since`.
    pub async fn subscribe_group_messages_with_since(
        &self,
        _npub: &PublicKey,
        since: Option<Timestamp>,
    ) -> Result<SubscriptionId, DaemonError> {
        let mut filter = Filter::new().kind(Kind::MlsGroupMessage);
        if let Some(since) = since {
            filter = filter.since(since);
        }
        let output = self
            .client
            .subscribe(filter, None)
            .await
            .map_err(|e| DaemonError::Nostr(format!("subscribe failed: {e}")))?;
        Ok(output.val)
    }

    /// Subscribe to kind:445 MLS group messages addressed to `npub`.
    pub async fn subscribe_group_messages(
        &self,
        npub: &PublicKey,
    ) -> Result<SubscriptionId, DaemonError> {
        self.subscribe_group_messages_with_since(npub, None).await
    }

    /// Unsubscribe a previously created bot subscription.
    pub async fn unsubscribe_bot(&self, sub_id: &SubscriptionId) -> Result<(), DaemonError> {
        self.client.unsubscribe(sub_id).await;
        Ok(())
    }

    /// Disconnect from all relays and stop the notification loop.
    pub async fn shutdown(&self) {
        self.client.shutdown().await;
    }

    /// Disconnect from all relays.
    pub async fn disconnect(&self) {
        self.client.disconnect().await;
    }

    /// Build a NIP-17 private message rumor with millisecond ordering and
    /// an optional reply marker.
    fn build_dm_rumor(
        sender: &PublicKey,
        recipient: &PublicKey,
        content: &str,
        reply_to: Option<&str>,
    ) -> Result<UnsignedEvent, DaemonError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| DaemonError::Nostr(format!("system clock before Unix epoch: {e}")))?;
        let ms = (now.as_millis() % 1000).to_string();

        let mut rumor_builder = EventBuilder::private_msg_rumor(*recipient, content);
        if let Some(reply_id) = reply_to {
            let event_id = EventId::parse(reply_id)
                .map_err(|e| DaemonError::Nostr(format!("invalid reply_to event id: {e}")))?;
            rumor_builder = rumor_builder.tags([nostr_tags::reply_e_tag(event_id)]);
        }
        rumor_builder = rumor_builder.tag(nostr_tags::ms_tag(ms));
        Ok(rumor_builder.build(*sender))
    }

    fn build_dm_reaction_rumor(
        sender: &PublicKey,
        recipient: &PublicKey,
        target_rumor_id: &str,
        emoji: &str,
    ) -> Result<UnsignedEvent, DaemonError> {
        let target = EventId::parse(target_rumor_id)
            .map_err(|error| DaemonError::Nostr(format!("invalid target event id: {error}")))?;
        Ok(
            nostr_tags::reaction_event(target, *recipient, Some(Kind::PrivateDirectMessage), emoji)
                .build(*sender),
        )
    }

    fn build_group_reaction_rumor(
        sender: &PublicKey,
        target_rumor_id: &str,
        emoji: &str,
    ) -> Result<UnsignedEvent, DaemonError> {
        let target = EventId::parse(target_rumor_id)
            .map_err(|error| DaemonError::Nostr(format!("invalid target event id: {error}")))?;
        Ok(EventBuilder::new(Kind::Reaction, emoji)
            .tag(Tag::event(target))
            .build(*sender))
    }

    /// Build the plaintext inner rumor for a kind:445 group text message.
    ///
    /// When a virtual bucket is provided, wraps the plaintext in the Pacto
    /// mention envelope so the receiving app can route replies back to the
    /// same virtual channel. Otherwise preserves legacy plain-text behavior.
    /// The inner kind must differ from the kind:445 MLS wrapper so that
    /// decrypted group content is not mistaken for the wire-format wrapper
    /// itself.
    pub fn build_group_text_rumor(
        sender: &PublicKey,
        content: String,
        pacto_virtual_bucket: Option<String>,
    ) -> Result<UnsignedEvent, DaemonError> {
        let payload = match pacto_virtual_bucket {
            Some(bucket) => serde_json::to_string(&json!({
                "kind": MENTION_ENVELOPE_KIND,
                "body": content,
                "mentions": [],
                "pacto_virtual_bucket": bucket,
            }))
            .map_err(DaemonError::Json)?,
            None => content,
        };
        Ok(UnsignedEvent::new(
            *sender,
            Timestamp::now(),
            Kind::PrivateDirectMessage,
            Vec::new(),
            payload,
        ))
    }

    /// Send a NIP-17 private direct message as a NIP-59 gift wrap.
    ///
    /// If `reply_to` is provided, an `e` tag referencing the original rumor or
    /// event id is added to the rumor.
    pub async fn send_dm(
        &self,
        signer: &dyn Signer,
        recipient_npub: &str,
        content: &str,
        reply_to: Option<&str>,
    ) -> Result<EventId, DaemonError> {
        let bot_npub = signer
            .public_key()
            .to_bech32()
            .unwrap_or_else(|_| signer.public_key().to_hex());
        info!(
            bot_npub = %bot_npub,
            recipient = %recipient_npub,
            reply_to = ?reply_to,
            "sending DM"
        );

        let recipient = PublicKey::parse(recipient_npub)
            .map_err(|e| DaemonError::Nostr(format!("invalid recipient npub: {e}")))?;

        let rumor = Self::build_dm_rumor(&signer.public_key(), &recipient, content, reply_to)?;
        let rumor_event = sign_unsigned_event(signer, rumor).await?;

        let event_id = self
            .send_gift_wrap(signer, &recipient, &rumor_event)
            .await
            .map_err(|e| {
                error!(
                    bot_npub = %bot_npub,
                    recipient = %recipient_npub,
                    error = %e,
                    "failed to publish DM"
                );
                e
            })?;

        Ok(event_id)
    }

    /// Send a kind:7 reaction to a private-message rumor.
    pub async fn send_reaction(
        &self,
        signer: &dyn Signer,
        recipient_npub: &str,
        target_rumor_id: &str,
        emoji: &str,
    ) -> Result<EventId, DaemonError> {
        let recipient = PublicKey::parse(recipient_npub)
            .map_err(|error| DaemonError::Nostr(format!("invalid recipient npub: {error}")))?;
        let unsigned = Self::build_dm_reaction_rumor(
            &signer.public_key(),
            &recipient,
            target_rumor_id,
            emoji,
        )?;
        let rumor = sign_unsigned_event(signer, unsigned).await?;
        self.send_gift_wrap(signer, &recipient, &rumor).await
    }

    /// Sign and gift-wrap a prepared kind:15 attachment rumor.
    pub async fn send_attachment(
        &self,
        signer: &dyn Signer,
        recipient: &PublicKey,
        rumor: UnsignedEvent,
    ) -> Result<EventId, DaemonError> {
        if rumor.kind != Kind::Custom(15) {
            return Err(DaemonError::AttachmentInvalid {
                category: "outbound rumor kind is not 15".into(),
            });
        }
        let rumor = sign_unsigned_event(signer, rumor).await?;
        self.send_gift_wrap(signer, recipient, &rumor).await
    }

    /// Build a NIP-59 gift wrap around a signed rumor and publish it to every
    /// configured relay.
    ///
    /// Only the published event id is returned; the rumor, seal ciphertext, and
    /// gift-wrap ciphertext are never logged.
    async fn send_gift_wrap(
        &self,
        signer: &dyn Signer,
        recipient: &PublicKey,
        rumor: &Event,
    ) -> Result<EventId, DaemonError> {
        let seal_content = signer
            .nip44_encrypt(recipient, &crate::nostr_json::event_to_json(rumor))
            .await
            .map_err(|e| DaemonError::Nostr(format!("failed to encrypt seal: {e}")))?;
        let seal = UnsignedEvent::new(
            signer.public_key(),
            nostr::Timestamp::tweaked(nip59::RANGE_RANDOM_TIMESTAMP_TWEAK),
            Kind::Seal,
            Vec::new(),
            seal_content,
        );
        let seal_event = sign_unsigned_event(signer, seal).await?;

        let ephemeral = Keys::generate();
        let gift_content = nip44::encrypt(
            ephemeral.secret_key(),
            recipient,
            crate::nostr_json::event_to_json(&seal_event),
            Version::default(),
        )
        .map_err(|e| DaemonError::Nostr(format!("failed to encrypt gift wrap: {e}")))?;
        let gift = UnsignedEvent::new(
            ephemeral.public_key(),
            nostr::Timestamp::tweaked(nip59::RANGE_RANDOM_TIMESTAMP_TWEAK),
            Kind::GiftWrap,
            [Tag::public_key(*recipient)],
            gift_content,
        );
        let gift_event = crate::nostr_json::sign_unsigned(gift, &ephemeral)
            .map_err(|e| DaemonError::Nostr(format!("failed to sign gift wrap: {e}")))?;

        let output = self
            .client
            .send_event(&gift_event)
            .await
            .map_err(|e| DaemonError::Nostr(format!("failed to publish event: {e}")))?;

        let event_id = *output.id();
        let success_count = output.success.len();
        let failed_count = output.failed.len();

        if success_count == 0 {
            return Err(DaemonError::Nostr(
                "failed to publish gift wrap: no relay accepted the event".into(),
            ));
        }

        if failed_count > 0 {
            for (url, err) in &output.failed {
                warn!(
                    event_id = %event_id.to_hex(),
                    relay_url = %url.to_string(),
                    error = %err,
                    "gift wrap publish failed for relay"
                );
            }
            warn!(
                event_id = %event_id.to_hex(),
                success_relays = success_count,
                failed_relays = failed_count,
                "gift wrap published with partial relay failures"
            );
        }

        self.deliver_locally(&gift_event).await;

        Ok(event_id)
    }

    /// The kind:30443 addressable (NIP-33) KeyPackage kind. MDK 0.8.0
    /// accepts both this and the legacy `Kind::MlsKeyPackage` (443) through
    /// May 31, 2026; the daemon must fetch either kind even though it only
    /// ever *publishes* kind:443 (per R8).
    const MLS_KEY_PACKAGE_KIND_ADDRESSABLE: Kind = Kind::Custom(30443);

    /// Fetch a fresh kind:443 or kind:30443 KeyPackage authored by
    /// `recipient` from the relay pool.
    ///
    /// The returned event is verified, of the correct kind, authored by the
    /// requested pubkey, and within the freshness window. The age of the
    /// selected KeyPackage is returned alongside the event. Only the event id,
    /// author, and age are logged; the KeyPackage ciphertext is never logged.
    pub async fn fetch_key_package(
        &self,
        recipient: &PublicKey,
        timeout: Duration,
        freshness: Duration,
    ) -> Result<(Event, Duration), DaemonError> {
        let now = Timestamp::now();
        let now_secs = now.as_secs();
        let freshness_secs = freshness.as_secs();
        let since = now - freshness_secs;

        let filter = Filter::new()
            .kinds([Kind::MlsKeyPackage, Self::MLS_KEY_PACKAGE_KIND_ADDRESSABLE])
            .author(*recipient)
            .since(since);

        let events = self
            .client
            .fetch_events(filter, timeout)
            .await
            .map_err(|e| DaemonError::Nostr(format!("key package fetch failed: {e}")))?;

        if events.is_empty() {
            let recipient_npub = recipient.to_bech32().unwrap_or_else(|_| recipient.to_hex());
            return Err(DaemonError::KeyPackageNotFound {
                recipient: recipient_npub,
            });
        }

        let mut selected: Option<Event> = None;
        let mut selected_ts: u64 = 0;
        let mut saw_invalid = false;

        for event in events.iter() {
            let event_id = event.id.to_hex();
            let author = event.pubkey.to_hex();

            if event.verify().is_err() {
                warn!(
                    event_id = %event_id,
                    author = %author,
                    "fetched key package failed signature verification; treating as absent"
                );
                saw_invalid = true;
                continue;
            }

            if (event.kind != Kind::MlsKeyPackage
                && event.kind != Self::MLS_KEY_PACKAGE_KIND_ADDRESSABLE)
                || event.pubkey != *recipient
            {
                warn!(
                    event_id = %event_id,
                    author = %author,
                    kind = ?event.kind,
                    expected_author = %recipient.to_hex(),
                    "fetched key package has wrong kind or author; treating as absent"
                );
                saw_invalid = true;
                continue;
            }

            let event_ts = event.created_at.as_secs();
            if event_ts > now_secs + 60 {
                warn!(
                    event_id = %event_id,
                    author = %author,
                    created_at = event_ts,
                    now = now_secs,
                    "fetched key package is dated more than 60 seconds in the future; treating as absent"
                );
                continue;
            }

            if event_ts + freshness_secs < now_secs {
                warn!(
                    event_id = %event_id,
                    author = %author,
                    created_at = event_ts,
                    now = now_secs,
                    freshness_secs = freshness_secs,
                    "fetched key package is older than the freshness window; treating as absent"
                );
                continue;
            }

            if event_ts > selected_ts {
                selected_ts = event_ts;
                selected = Some(event.clone());
            }
        }

        let event = selected.ok_or_else(|| {
            if saw_invalid {
                DaemonError::InvalidKeyPackage
            } else {
                DaemonError::StaleKeyPackage
            }
        })?;
        let age = Duration::from_secs(now_secs - selected_ts);
        info!(
            event_id = %event.id.to_hex(),
            author = %event.pubkey.to_hex(),
            age_secs = age.as_secs(),
            "fetched key package"
        );
        Ok((event, age))
    }

    /// Sign and publish a NIP-59 welcome gift wrap for an MLS welcome rumor.
    ///
    /// The `welcome_rumor` is an unsigned kind:444 event produced by the MLS
    /// engine. It is signed with the bot signer, sealed, gift-wrapped, and
    /// published to every configured relay. Only the recipient, bot, and
    /// published event id are logged at INFO or above; the rumor, seal
    /// ciphertext, and gift-wrap ciphertext are never logged.
    pub async fn send_welcome(
        &self,
        signer: &dyn Signer,
        recipient: &PublicKey,
        welcome_rumor: UnsignedEvent,
    ) -> Result<EventId, DaemonError> {
        let bot_npub = signer
            .public_key()
            .to_bech32()
            .unwrap_or_else(|_| signer.public_key().to_hex());

        let welcome_event = sign_unsigned_event(signer, welcome_rumor).await?;

        let event_id = self
            .send_gift_wrap(signer, recipient, &welcome_event)
            .await?;

        info!(
            bot_npub = %bot_npub,
            recipient = %recipient.to_hex(),
            event_id = %event_id.to_hex(),
            "published welcome gift wrap"
        );

        Ok(event_id)
    }

    /// Publish a pre-signed kind:445 MLS group evolution event to every
    /// configured relay.
    ///
    /// The event is sent as-is without re-signing. Only the event id and author
    /// are logged; the event content is never logged.
    pub async fn send_evolution_event(&self, event: &Event) -> Result<EventId, DaemonError> {
        let output =
            self.client.send_event(event).await.map_err(|e| {
                DaemonError::Nostr(format!("failed to publish evolution event: {e}"))
            })?;

        let event_id = *output.id();
        let success_count = output.success.len();
        let failed_count = output.failed.len();

        if success_count == 0 {
            return Err(DaemonError::Nostr(
                "failed to publish evolution event: no relay accepted the event".into(),
            ));
        }

        if failed_count > 0 {
            for (url, err) in &output.failed {
                warn!(
                    event_id = %event_id.to_hex(),
                    relay_url = %url.to_string(),
                    error = %err,
                    "evolution event publish failed for relay"
                );
            }
            warn!(
                event_id = %event_id.to_hex(),
                success_relays = success_count,
                failed_relays = failed_count,
                "evolution event published with partial relay failures"
            );
        }

        info!(
            event_id = %event.id.to_hex(),
            author = %event.pubkey.to_hex(),
            "published evolution event"
        );

        self.deliver_locally(event).await;

        Ok(event_id)
    }

    /// Publish a KeyPackage event (kind:443) for MLS group participation.
    ///
    /// This method creates a key package using the MLS engine, builds a kind:443
    /// event with the required tags, signs it, and publishes to relays.
    ///
    /// Returns the published event ID on success.
    pub async fn publish_key_package(
        &self,
        mls_engine: &MlsEngineHandle,
        signer: &dyn Signer,
        relays: Vec<String>,
    ) -> Result<EventId, DaemonError> {
        let bot_pubkey = signer.public_key();
        let relay_urls = relays
            .iter()
            .filter_map(|r| nostr::RelayUrl::parse(r).ok())
            .collect::<Vec<_>>();

        let (content, tags) = mls_engine
            .publish_key_package(&bot_pubkey, relay_urls)
            .await
            .map_err(|e| DaemonError::Nostr(format!("MLS key package creation failed: {e}")))?;

        let rumor = UnsignedEvent::new(
            bot_pubkey,
            Timestamp::now(),
            Kind::MlsKeyPackage,
            tags.to_vec(),
            content,
        );

        let event = sign_unsigned_event(signer, rumor).await?;

        let output = self.client.send_event(&event).await.map_err(|e| {
            error!(
                bot_npub = %bot_pubkey.to_bech32().unwrap_or_else(|_| bot_pubkey.to_hex()),
                error = %e,
                "failed to publish KeyPackage"
            );
            DaemonError::Nostr(format!("failed to publish event: {e}"))
        })?;

        Ok(*output.id())
    }

    /// Send an encrypted MLS group message.
    ///
    /// This method creates an MLS group message using the engine, wraps it in
    /// a kind:445 event, and publishes to relays.
    ///
    /// Returns the published event ID on success.
    pub async fn send_group_message(
        &self,
        mls_engine: &MlsEngineHandle,
        signer: &dyn Signer,
        group_id: Vec<u8>,
        rumor: UnsignedEvent,
    ) -> Result<EventId, DaemonError> {
        let bot_pubkey = signer.public_key();
        let wrapper = mls_engine
            .create_group_message(group_id, rumor)
            .await
            .map_err(|e| DaemonError::Nostr(format!("MLS group message creation failed: {e}")))?;

        // The wrapper returned by the MLS engine is signed with an ephemeral group
        // exporter key. Re-sign it with the bot's key so relays attribute the event
        // to the bot and the signature is valid for the bot's public key.
        let unsigned = UnsignedEvent::new(
            bot_pubkey,
            wrapper.created_at,
            wrapper.kind,
            wrapper.tags.to_vec(),
            wrapper.content,
        );
        let signed_wrapper = sign_unsigned_event(signer, unsigned).await?;

        let output = self.client.send_event(&signed_wrapper).await.map_err(|e| {
            error!(
                bot_npub = %bot_pubkey.to_bech32().unwrap_or_else(|_| bot_pubkey.to_hex()),
                error = %e,
                "failed to publish group message"
            );
            DaemonError::Nostr(format!("failed to publish event: {e}"))
        })?;

        self.deliver_locally(&signed_wrapper).await;

        Ok(*output.id())
    }

    /// Send a kind:7 reaction inside an MLS group message.
    pub async fn send_group_reaction(
        &self,
        mls_engine: &MlsEngineHandle,
        signer: &dyn Signer,
        group_id: Vec<u8>,
        target_rumor_id: &str,
        emoji: &str,
    ) -> Result<EventId, DaemonError> {
        let rumor = Self::build_group_reaction_rumor(&signer.public_key(), target_rumor_id, emoji)?;
        self.send_group_message(mls_engine, signer, group_id, rumor)
            .await
    }

    /// Publish a kind:0 metadata event for the bot.
    ///
    /// Only fields that are `Some` are included in the metadata JSON.
    pub async fn set_profile(
        &self,
        signer: &dyn Signer,
        name: Option<&str>,
        about: Option<&str>,
        picture: Option<&str>,
    ) -> Result<EventId, DaemonError> {
        let bot_npub = signer
            .public_key()
            .to_bech32()
            .unwrap_or_else(|_| signer.public_key().to_hex());
        info!(
            bot_npub = %bot_npub,
            name = ?name,
            "setting profile"
        );

        let mut metadata = serde_json::Map::new();
        if let Some(name) = name {
            let _ = metadata.insert("name".to_string(), json!(name));
        }
        if let Some(about) = about {
            let _ = metadata.insert("about".to_string(), json!(about));
        }
        if let Some(picture) = picture {
            let _ = metadata.insert("picture".to_string(), json!(picture));
        }
        let content = serde_json::to_string(&Value::Object(metadata)).map_err(DaemonError::Json)?;

        let unsigned = UnsignedEvent::new(
            signer.public_key(),
            nostr::Timestamp::now(),
            Kind::Metadata,
            Vec::new(),
            content,
        );
        let event = sign_unsigned_event(signer, unsigned).await?;

        let output = self.client.send_event(&event).await.map_err(|e| {
            error!(
                bot_npub = %bot_npub,
                name = ?name,
                error = %e,
                "failed to publish profile event"
            );
            DaemonError::Nostr(format!("failed to publish profile event: {e}"))
        })?;

        Ok(*output.id())
    }

    /// Publish a kind:0 metadata event for the bot using the admin CLI profile
    /// format.
    ///
    /// The metadata JSON includes `bot: true`, the bot's capabilities, and any
    /// configured optional fields (`about`, `picture`). This is the
    /// implementation behind `pacto-bot-admin publish-profile`.
    pub async fn publish_bot_profile(
        &self,
        bot: &BotConfig,
        signer: &dyn Signer,
    ) -> Result<EventId, DaemonError> {
        let event = build_bot_profile_event(bot, signer).await?;

        let output = self.client.send_event(&event).await.map_err(|e| {
            error!(
                bot_id = %bot.id,
                error = %e,
                "failed to publish profile event"
            );
            DaemonError::Nostr(format!("failed to publish event: {e}"))
        })?;

        Ok(*output.id())
    }

    /// Decrypt a single incoming gift-wrap event using the registered bot signer.
    ///
    /// Errors when the decrypted rumor's kind delivers no event (an
    /// unrepresented kind, or an invalid reaction missing its target `e` tag
    /// or content) — this single-shot helper has no cursor to advance past a
    /// skipped rumor, unlike the inbound loop. Callers that need to tell "no
    /// event for this rumor kind" apart from a decrypt failure, or that need
    /// to advance past a skip without erroring (the inbound notification
    /// loop, and any future kind such as attachments or MLS group variants),
    /// should call `process_gift_wrap` directly instead.
    pub async fn decrypt_event(&self, event: &Event) -> Result<AgentEvent, DaemonError> {
        let snapshot = self.signers.read().await.clone();
        let mls_engines = self.mls_engines.read().await.clone();
        Self::process_gift_wrap(
            &snapshot,
            &mls_engines,
            event,
            self.diagnostics.as_ref(),
            self.attachment_processor.as_deref(),
        )
        .await?
        .ok_or_else(|| {
            DaemonError::Nostr(
                "rumor kind delivers no event (unrepresented kind or invalid reaction)".into(),
            )
        })
    }

    /// Return an async stream of incoming DMs converted to [`AgentEvent`].
    pub fn receive_events(&self) -> impl Stream<Item = Result<AgentEvent, DaemonError>> + use<> {
        let (tx, rx) = unbounded_channel();
        if let Ok(mut slot) = self.local_delivery_tx.lock() {
            *slot = Some(tx.clone());
        }
        let local_delivery_tx = Arc::clone(&self.local_delivery_tx);
        let client = self.client.clone();
        let signers = Arc::clone(&self.signers);
        let mls_engines = Arc::clone(&self.mls_engines);
        let diagnostics = self.diagnostics.clone();
        let attachment_processor = self.attachment_processor.clone();
        let gift_wrap_semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_GIFT_WRAP_TASKS));
        tokio::spawn(async move {
            let _ = client
                .handle_notifications(|notification| {
                    let client = client.clone();
                    let tx: UnboundedSender<Result<AgentEvent, DaemonError>> = tx.clone();
                    let signers = Arc::clone(&signers);
                    let mls_engines = Arc::clone(&mls_engines);
                    let diagnostics = diagnostics.clone();
                    let attachment_processor = attachment_processor.clone();
                    let gift_wrap_semaphore = Arc::clone(&gift_wrap_semaphore);
                    async move {
                        match notification {
                            RelayPoolNotification::Event { event, .. } => {
                                if event.kind == Kind::GiftWrap {
                                    // Acquire a permit before spawning so a burst of inbound
                                    // gift wraps backs off on the semaphore instead of spawning
                                    // unboundedly; MAX_CONCURRENT_GIFT_WRAP_TASKS bounds memory
                                    // and file-descriptor growth from a hostile spray (R33).
                                    let permit =
                                        match Arc::clone(&gift_wrap_semaphore).acquire_owned().await
                                        {
                                            Ok(permit) => permit,
                                            Err(_) => {
                                                // The semaphore is never closed, so this branch
                                                // is unreachable in practice; drop the event
                                                // rather than spawn unbounded.
                                                error!(
                                                    "gift wrap semaphore unexpectedly closed; dropping event"
                                                );
                                                return Ok(false);
                                            }
                                        };
                                    // Spawn each gift-wrap decryption in its own task so that
                                    // one bot's slow signer (e.g. a NIP-46 bunker) does not block
                                    // other bots from receiving DMs.
                                    let tx = tx.clone();
                                    let signers = Arc::clone(&signers);
                                    let mls_engines = Arc::clone(&mls_engines);
                                    let diagnostics = diagnostics.clone();
                                    let attachment_processor = attachment_processor.clone();
                                    tokio::spawn(async move {
                                        // Held for the task's lifetime; releases the permit
                                        // back to the semaphore on drop.
                                        let _permit = permit;
                                        let snapshot = signers.read().await.clone();
                                        let mls_engines = mls_engines.read().await.clone();
                                        let decrypted = match tokio::time::timeout(
                                            GIFT_WRAP_PROCESS_TIMEOUT,
                                            Self::decrypt_gift_wrap_rumor(
                                                &snapshot,
                                                &event,
                                                diagnostics.as_ref(),
                                            ),
                                        )
                                        .await
                                        {
                                            Ok(Ok(decrypted)) => decrypted,
                                            Ok(Err(e)) => {
                                                let _ = tx.send(Err(e));
                                                return;
                                            }
                                            Err(_elapsed) => {
                                                // Drop the event rather than stall the intake
                                                // loop; count it in the same aggregated,
                                                // ring-safe category `process_gift_wrap` uses
                                                // for its own rejections (R33, R48). Only the
                                                // decrypt+parse phase is bounded here; the
                                                // untimed continuation below (attachment fetch,
                                                // MLS welcome acceptance) never reaches this
                                                // branch, so it can't be orphaned by a timeout.
                                                if let Some(d) = &diagnostics {
                                                    d.record_gift_wrap_rejected().await;
                                                }
                                                warn!(
                                                    event_id = %event.id.to_hex(),
                                                    "gift wrap decrypt/parse timed out; dropping event"
                                                );
                                                return;
                                            }
                                        };
                                        match Self::finish_gift_wrap(
                                            &mls_engines,
                                            &event,
                                            diagnostics.as_ref(),
                                            attachment_processor.as_deref(),
                                            decrypted,
                                        )
                                        .await
                                        {
                                            Ok(Some(agent_event)) => {
                                                let _ = tx.send(Ok(agent_event));
                                            }
                                            Ok(None) => {}
                                            Err(e) => {
                                                let _ = tx.send(Err(e));
                                            }
                                        }
                                    });
                                } else if event.kind == Kind::MlsGroupMessage {
                                    // Spawn each group message decryption in its own task so that
                                    // one bot's slow MLS engine does not block other bots.
                                    let tx = tx.clone();
                                    let client = client.clone();
                                    let signers = Arc::clone(&signers);
                                    let mls_engines = Arc::clone(&mls_engines);
                                    let diagnostics = diagnostics.clone();
                                    let attachment_processor = attachment_processor.clone();
                                    tokio::spawn(async move {
                                        let signers = signers.read().await.clone();
                                        let mls_engines = mls_engines.read().await.clone();
                                        match Self::process_group_message(
                                            &client,
                                            &signers,
                                            &mls_engines,
                                            &event,
                                            diagnostics.as_ref(),
                                            attachment_processor.as_deref(),
                                        )
                                        .await
                                        {
                                            Ok(agent_events) => {
                                                for agent_event in agent_events {
                                                    let _ = tx.send(Ok(agent_event));
                                                }
                                            }
                                            Err(e) => {
                                                let _ = tx.send(Err(e));
                                            }
                                        }
                                    });
                                }
                                Ok(false)
                            }
                            RelayPoolNotification::Shutdown => Ok(true),
                            _ => Ok(false),
                        }
                    }
                })
                .await;
            // The notification loop has ended (relay pool shutdown); drop
            // this stream's sender from `local_delivery_tx` too, or the
            // client-held clone keeps the channel open forever and
            // `receive_events()`'s stream never yields `None` even after
            // `shutdown()`. Guarded by `same_channel` so a newer
            // `receive_events()` call's sender already in the slot is left
            // alone.
            if let Ok(mut slot) = local_delivery_tx.lock()
                && slot.as_ref().is_some_and(|s| s.same_channel(&tx))
            {
                *slot = None;
            }
        });

        UnboundedReceiverStream::new(rx)
    }

    /// Whether `recipient`'s own configured relay set has at least one
    /// currently connected relay. Used only to scope `deliver_locally`'s
    /// same-daemon delivery correction to a recipient that is genuinely
    /// reachable, not to a bot whose entire relay set is unreachable --
    /// see the doc comment on `deliver_locally`.
    ///
    /// A bot with no recorded relay list (relay tracking never wired up,
    /// e.g. a test double) is treated as reachable so this never blocks
    /// delivery on missing bookkeeping.
    async fn recipient_has_connected_relay(&self, recipient: &PublicKey) -> bool {
        let configured = self.bot_relays.read().await;
        let Some(urls) = configured.get(recipient) else {
            return true;
        };
        if urls.is_empty() {
            return true;
        }
        let urls = urls.clone();
        drop(configured);
        let statuses = self.relay_statuses().await;
        urls.iter()
            .any(|url| statuses.get(url).map(String::as_str) == Some("Connected"))
    }

    /// Same-daemon multi-bot delivery correction.
    ///
    /// `nostr-sdk`'s `Client` keeps its own local database of every event
    /// it has seen, including ones it just published itself (see
    /// `nostr_relay_pool::relay::inner::handle_event_msg`): when the relay
    /// echoes a just-published event back on a live subscription, the
    /// client finds the id already `Saved` and never raises a
    /// `RelayPoolNotification::Event` for it. Since this daemon multiplexes
    /// every configured bot onto the SAME shared `Client`, that silently
    /// swallows an MLS welcome or group message one locally managed bot
    /// sends to another: the recipient's own subscription never observes
    /// it, even though an external subscriber genuinely would have. Run
    /// the normal inbound pipeline directly against the just-published
    /// event -- exactly once, since the pipeline is only ever invoked here
    /// or from a genuine (non-duplicate) relay notification, never both --
    /// so intra-daemon bot-to-bot delivery works the same as delivery from
    /// an external sender.
    async fn deliver_locally(&self, event: &Event) {
        let Some(tx) = self
            .local_delivery_tx
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
        else {
            return;
        };

        match event.kind {
            Kind::GiftWrap => {
                let Some(recipient) = event.tags.public_keys().next().copied() else {
                    return;
                };
                let signers = self.signers.read().await;
                if !signers.contains_key(&recipient) {
                    // Not addressed to a bot this daemon manages; the relay
                    // round trip (if any) is the only path for it.
                    return;
                }
                drop(signers);
                if !self.recipient_has_connected_relay(&recipient).await {
                    // The recipient bot's entire configured relay set is
                    // unreachable, so a real relay round trip could never
                    // have delivered this event either; do not synthesize
                    // delivery to an offline bot.
                    return;
                }
                let signers = self.signers.read().await.clone();
                let mls_engines = self.mls_engines.read().await.clone();
                match Self::process_gift_wrap(
                    &signers,
                    &mls_engines,
                    event,
                    self.diagnostics.as_ref(),
                    self.attachment_processor.as_deref(),
                )
                .await
                {
                    Ok(Some(agent_event)) => {
                        let _ = tx.send(Ok(agent_event));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            }
            Kind::MlsGroupMessage => {
                let signers = self.signers.read().await.clone();
                let mls_engines = self.mls_engines.read().await.clone();
                match Self::process_group_message(
                    &self.client,
                    &signers,
                    &mls_engines,
                    event,
                    self.diagnostics.as_ref(),
                    self.attachment_processor.as_deref(),
                )
                .await
                {
                    Ok(agent_events) => {
                        for agent_event in agent_events {
                            let _ = tx.send(Ok(agent_event));
                        }
                    }
                    // No locally configured bot is a member of this group;
                    // an external member (if any) relies on the relay.
                    Err(DaemonError::Nostr(msg))
                        if msg == "group message not addressed to a bot with an MLS engine" => {}
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            }
            _ => {}
        }
    }

    /// Decrypt+parse phase of gift-wrap processing: gift-wrap signature
    /// verification, recipient `p`-tag lookup, signer lookup, gift-wrap
    /// NIP-44 decrypt, seal event parse+verify, seal NIP-44 decrypt, rumor
    /// parse, and the rumor-author-vs-seal-author consistency check (R30).
    ///
    /// This is the only portion subject to `GIFT_WRAP_PROCESS_TIMEOUT` at
    /// the streaming call site in `receive_events`. The remaining,
    /// side-effecting work (attachment fetch via `spawn_blocking`, MLS
    /// welcome acceptance) lives in [`NostrClient::finish_gift_wrap`] and runs
    /// outside that timeout so a slow decrypt can't orphan in-flight
    /// background work by dropping its future mid-flight.
    async fn decrypt_gift_wrap_rumor(
        signers: &HashMap<PublicKey, (String, Arc<dyn Signer>)>,
        event: &Event,
        diagnostics: Option<&Diagnostics>,
    ) -> Result<DecryptedGiftWrap, DaemonError> {
        info!(
            event_id = %event.id.to_hex(),
            kind = %event.kind.as_u16(),
            "received gift wrap from relay"
        );

        if let Err(e) = event.verify() {
            let message = format!("gift wrap signature verification failed: {e}");
            warn!(event_id = %event.id.to_hex(), reason = %message, "rejecting gift wrap");
            if let Some(d) = diagnostics {
                d.record_invalid_event().await;
                d.record_gift_wrap_rejected().await;
            }
            return Err(DaemonError::Nostr(message));
        }

        let recipient = event
            .tags
            .public_keys()
            .next()
            .copied()
            .ok_or_else(|| DaemonError::Nostr("gift wrap missing recipient p tag".into()))?;

        let (bot_id, signer) = signers
            .get(&recipient)
            .ok_or_else(|| DaemonError::Nostr(format!("no signer registered for {recipient}")))?;

        info!(
            event_id = %event.id.to_hex(),
            bot_id = %bot_id,
            "decrypting gift wrap"
        );

        // Gift-wrap is encrypted by the ephemeral key to the recipient.
        let seal_json = signer
            .nip44_decrypt(&event.pubkey, &event.content)
            .await
            .map_err(|e| DaemonError::Nostr(format!("failed to decrypt gift wrap: {e}")))?;
        let seal_event = crate::nostr_json::event_from_json(&seal_json)
            .map_err(|e| DaemonError::Nostr(format!("invalid seal event: {e}")))?;

        if let Err(e) = seal_event.verify() {
            let message = format!("seal signature verification failed: {e}");
            warn!(event_id = %event.id.to_hex(), reason = %message, "rejecting gift wrap");
            if let Some(d) = diagnostics {
                d.record_invalid_event().await;
                d.record_gift_wrap_rejected().await;
            }
            return Err(DaemonError::Nostr(message));
        }

        // Seal is encrypted by the sender to the recipient.
        let rumor_json = signer
            .nip44_decrypt(&seal_event.pubkey, &seal_event.content)
            .await
            .map_err(|e| DaemonError::Nostr(format!("failed to decrypt seal: {e}")))?;
        let rumor = crate::nostr_json::unsigned_event_from_json(&rumor_json)
            .map_err(|e| DaemonError::Nostr(format!("invalid rumor event: {e}")))?;

        let rumor_id = rumor
            .id
            .ok_or_else(|| DaemonError::Nostr("rumor missing id".into()))?
            .to_hex();

        // The daemon attributes `author` from the seal's pubkey below; reject
        // a rumor that claims a different author than the seal that carried
        // it rather than silently trusting the seal's attribution (R30).
        if rumor.pubkey != seal_event.pubkey {
            let message = "rumor author does not match seal author".to_string();
            warn!(
                event_id = %event.id.to_hex(),
                rumor_id = %rumor_id,
                reason = %message,
                "rejecting gift wrap"
            );
            if let Some(d) = diagnostics {
                d.record_invalid_event().await;
                d.record_gift_wrap_rejected().await;
            }
            return Err(DaemonError::Nostr(message));
        }

        Ok(DecryptedGiftWrap {
            recipient,
            bot_id: bot_id.clone(),
            seal_event,
            rumor,
            rumor_id,
        })
    }

    /// Untimed continuation of gift-wrap processing: kind-specific
    /// dispatch (DM / reaction / inbound attachment / MLS welcome) and
    /// final [`AgentEvent`] construction. Deliberately not wrapped in
    /// `GIFT_WRAP_PROCESS_TIMEOUT` — see [`NostrClient::decrypt_gift_wrap_rumor`].
    async fn finish_gift_wrap(
        mls_engines: &HashMap<PublicKey, (String, MlsEngineHandle)>,
        event: &Event,
        diagnostics: Option<&Diagnostics>,
        attachment_processor: Option<&InboundAttachmentProcessor>,
        decrypted: DecryptedGiftWrap,
    ) -> Result<Option<AgentEvent>, DaemonError> {
        let DecryptedGiftWrap {
            recipient,
            bot_id,
            seal_event,
            rumor,
            rumor_id,
        } = decrypted;

        let (event_type, reaction, attachment) = match rumor.kind {
            Kind::PrivateDirectMessage => (EventType::DmReceived, None, None),
            Kind::MlsWelcome => (EventType::MlsWelcomeReceived, None, None),
            Kind::Reaction => match Self::extract_reaction(&rumor) {
                Some(payload) => (EventType::ReactionReceived, Some(payload), None),
                None => {
                    debug!(
                        event_id = %event.id.to_hex(),
                        rumor_id = %rumor_id,
                        "reaction rumor missing target e tag or content; recording invalid event"
                    );
                    if let Some(d) = diagnostics {
                        d.record_invalid_event().await;
                    }
                    return Ok(None);
                }
            },
            Kind::Custom(15) => {
                let Some(processor) = attachment_processor else {
                    if let Some(diagnostics) = diagnostics {
                        diagnostics.record_invalid_event().await;
                        diagnostics.record_attachment_receive_failed().await;
                    }
                    return Ok(None);
                };
                match processor.process_rumor(&rumor).await {
                    Ok(payload) => {
                        if let Some(diagnostics) = diagnostics {
                            diagnostics.record_attachment_receive().await;
                        }
                        (EventType::AttachmentReceived, None, Some(payload))
                    }
                    Err(error) => {
                        warn!(
                            event_id = %event.id.to_hex(),
                            rumor_id = %rumor_id,
                            error = %error,
                            "rejecting invalid inbound attachment"
                        );
                        if let Some(diagnostics) = diagnostics {
                            diagnostics.record_invalid_event().await;
                            diagnostics.record_attachment_receive_failed().await;
                            diagnostics
                                .record_error(Some("attachment_invalid"), &error.to_string(), None)
                                .await;
                        }
                        return Ok(None);
                    }
                }
            }
            other => {
                debug!(
                    event_id = %event.id.to_hex(),
                    rumor_id = %rumor_id,
                    kind = %other.as_u16(),
                    "skipping unrepresented rumor kind"
                );
                return Ok(None);
            }
        };

        // For MLS Welcome messages, process the welcome using the bot's
        // MLS engine so the daemon can participate in the Squad. The group
        // wire id is surfaced in chat_id for observability, but the event is
        // not fanned out to handlers (the daemon accepts the invite on the
        // bot's behalf).
        let chat_id: Option<String> = if event_type == EventType::MlsWelcomeReceived {
            let (_mls_bot_id, mls) = mls_engines.get(&recipient).ok_or_else(|| {
                DaemonError::Nostr(format!("no MLS engine registered for {recipient}"))
            })?;
            match mls
                .process_welcome_and_return_wire_id(event.id, rumor.clone())
                .await
            {
                Ok(wire_id) => Some(wire_id),
                Err(mls::MlsError::PeerVersionMismatch) => {
                    // Unsolicited inbound Welcome: no caller is waiting on a
                    // JSON-RPC response, and the gift-wrap sender is
                    // attacker-mintable, so this is counted in one
                    // aggregated total rather than a per-event `ErrorRecord`
                    // or any per-peer state (R33, R42) -- see
                    // `Diagnostics::record_welcome_version_mismatch`.
                    warn!(
                        event_id = %event.id.to_hex(),
                        rumor_id = %rumor_id,
                        "inbound MLS welcome missing required encoding tag; peer is on a pre-MIP-00/MIP-02 wire format"
                    );
                    if let Some(d) = diagnostics {
                        d.record_welcome_version_mismatch().await;
                    }
                    return Ok(None);
                }
                Err(e) => {
                    // Any other rejection (correctly tagged but structurally
                    // invalid, decrypt failure, etc.) -- same aggregated,
                    // no-per-peer-state treatment, but a distinct counter
                    // from the version-mismatch case above.
                    warn!(
                        event_id = %event.id.to_hex(),
                        rumor_id = %rumor_id,
                        error = %e,
                        "failed to accept MLS welcome; dropping as structurally invalid"
                    );
                    if let Some(d) = diagnostics {
                        d.record_welcome_rejected().await;
                    }
                    return Ok(None);
                }
            }
        } else {
            None
        };

        info!(
            event_id = %event.id.to_hex(),
            bot_id = %bot_id,
            rumor_id = %rumor_id,
            author = %seal_event.pubkey.to_hex(),
            kind = %rumor.kind.as_u16(),
            event_type = %event_type.as_wire_name(),
            "gift wrap decrypted"
        );

        if let Some(d) = diagnostics {
            d.record_event_decrypted().await;
        }

        Ok(Some(AgentEvent {
            bot_id: bot_id.clone(),
            event_id: event.id.to_hex(),
            event_type,
            chat_id,
            content: rumor.content,
            rumor_id,
            author: seal_event.pubkey.to_hex(),
            timestamp: rumor.created_at.as_secs(),
            reaction,
            attachment,
            ..Default::default()
        }))
    }

    /// Decrypt+parse a gift wrap and dispatch it into an [`AgentEvent`].
    /// Delegates to [`NostrClient::decrypt_gift_wrap_rumor`] (timed at the
    /// streaming call site, untimed here) followed by the untimed
    /// [`NostrClient::finish_gift_wrap`] continuation; external behavior is
    /// unchanged from before the split.
    async fn process_gift_wrap(
        signers: &HashMap<PublicKey, (String, Arc<dyn Signer>)>,
        mls_engines: &HashMap<PublicKey, (String, MlsEngineHandle)>,
        event: &Event,
        diagnostics: Option<&Diagnostics>,
        attachment_processor: Option<&InboundAttachmentProcessor>,
    ) -> Result<Option<AgentEvent>, DaemonError> {
        let decrypted = Self::decrypt_gift_wrap_rumor(signers, event, diagnostics).await?;
        Self::finish_gift_wrap(
            mls_engines,
            event,
            diagnostics,
            attachment_processor,
            decrypted,
        )
        .await
    }

    /// Extract a [`ReactionPayload`] from a decrypted kind:7 rumor by
    /// delegating to [`nostr_tags::decode_reaction`], matching the tag
    /// layout [`nostr_tags::reaction_event`] writes.
    ///
    /// Returns `None` when the rumor has no target `e` tag or empty content,
    /// in which case the caller records an invalid event and delivers
    /// nothing rather than erroring the whole gift-wrap decrypt.
    fn extract_reaction(rumor: &UnsignedEvent) -> Option<ReactionPayload> {
        let (target_rumor_id, emoji) = nostr_tags::decode_reaction(&rumor.tags, &rumor.content)?;
        Some(ReactionPayload {
            target_rumor_id: target_rumor_id.to_string(),
            emoji: emoji.to_string(),
        })
    }

    /// Decrypt a kind:445 MLS group message wrapper and produce one
    /// [`AgentEvent`] per configured bot that is a member of the group and did
    /// not publish the message itself. Application messages are enriched with
    /// the parsed mention envelope; protocol-only messages (proposals, commits,
    /// etc.) return an empty vector so they do not fan out to handlers.
    async fn process_group_message(
        client: &Client,
        signers: &HashMap<PublicKey, (String, Arc<dyn Signer>)>,
        mls_engines: &HashMap<PublicKey, (String, MlsEngineHandle)>,
        event: &Event,
        diagnostics: Option<&Diagnostics>,
        attachment_processor: Option<&InboundAttachmentProcessor>,
    ) -> Result<Vec<AgentEvent>, DaemonError> {
        info!(
            event_id = %event.id.to_hex(),
            kind = %event.kind.as_u16(),
            "received group message from relay"
        );

        if let Err(e) = event.verify() {
            return Err(DaemonError::Nostr(format!(
                "group message signature verification failed: {e}"
            )));
        }

        // Group messages are addressed to a Squad, not to a specific bot, so
        // identify every configured bot that is a member of the group identified
        // by the h tag.
        let group_id = nostr_tags::h_tag_content(&event.tags)
            .ok_or_else(|| DaemonError::Nostr("group message missing h tag".into()))?;

        let mut recipients = Vec::new();
        debug!(
            "looking for group_id={} among {} engines",
            group_id,
            mls_engines.len()
        );
        for (pubkey, (_, mls)) in mls_engines {
            match mls.has_group_with_wire_id(&group_id).await {
                Ok(true) => {
                    debug!("found engine for pubkey={} with group {}", pubkey, group_id);
                    recipients.push(*pubkey);
                }
                Ok(false) => {
                    debug!(
                        "engine for pubkey={} does NOT have group {}",
                        pubkey, group_id
                    );
                    continue;
                }
                Err(e) => {
                    // Match the per-recipient decrypt loop below: one
                    // unhealthy engine must not fail-close discovery for
                    // every co-located member. Empty `recipients` after
                    // this loop still surfaces total failure.
                    warn!(
                        "failed to check squad membership for pubkey={} group={}: {}; skipping unhealthy engine",
                        pubkey, group_id, e
                    );
                    continue;
                }
            }
        }

        if recipients.is_empty() {
            return Err(DaemonError::Nostr(
                "group message not addressed to a bot with an MLS engine".into(),
            ));
        }

        // Every current member's own engine independently needs to process
        // this event: a commit/proposal advances each member's epoch, so
        // applying it to only one recipient candidate (then stopping, as
        // this used to do) leaves every other member stuck at a stale
        // epoch, unable to decrypt later messages -- decoding a *later*
        // application message does not retroactively fix that, since each
        // member's own engine (not whichever one happened to be tried
        // first) is what actually needs the commit applied. Attempt every
        // recipient rather than stopping at the first success. A member
        // freshly added via a Welcome already starts at the post-commit
        // epoch that Welcome encoded, so replaying the same "add" commit
        // through their engine (which never held the pre-commit tree) is a
        // structural mismatch, not a real error, and is expected to fail
        // for exactly that member while others genuinely need it applied.
        //
        // `decrypted` captures the first genuine application-message
        // decode (content is identical for every member that can process
        // it); a commit/proposal event never produces one, so `decrypted`
        // stays `None` and no `AgentEvent`s are built. `delivered` records
        // every recipient whose *own* engine actually decoded this as an
        // application message -- only those get an `AgentEvent`; a
        // recipient whose attempt returned `None`/an error did not
        // genuinely receive this message and must not be reported as
        // having done so.
        let mut decrypted: Option<mls::DecryptedMessage> = None;
        let mut delivered: Vec<PublicKey> = Vec::new();
        let mut any_processed = false;
        for recipient in &recipients {
            let (bot_id, signer) = signers.get(recipient).ok_or_else(|| {
                DaemonError::Nostr(format!("no signer registered for {recipient}"))
            })?;
            let (_mls_bot_id, mls) = mls_engines.get(recipient).ok_or_else(|| {
                DaemonError::Nostr(format!("no MLS engine registered for {recipient}"))
            })?;

            // A bot never needs to (and, per MDK's `CannotDecryptOwnMessage`,
            // cannot) process an application message it published itself.
            // Commits/proposals are not signed under the bot's own npub, so
            // this never filters them out -- every member, including the
            // one who committed, gets a (cheap, idempotent) attempt.
            if event.pubkey == signer.public_key() {
                debug!(
                    event_id = %event.id.to_hex(),
                    bot_id = %bot_id,
                    "skipping own group message"
                );
                continue;
            }

            match mls.decrypt_group_message(event).await {
                Ok(mls::GroupMessageOutcome::Message(d)) => {
                    any_processed = true;
                    delivered.push(*recipient);
                    if decrypted.is_none() {
                        decrypted = Some(d);
                    }
                }
                Ok(mls::GroupMessageOutcome::PublishEvolution(evolution_event)) => {
                    any_processed = true;
                    // MDK auto-committed a self-remove proposal on this
                    // bot's behalf; the commit must reach the group's
                    // relays or every peer's epoch diverges from this
                    // bot's and stops decrypting (R11 makes the bot a
                    // co-admin of every squad it creates, so this path is
                    // common, not exotic). This is a raw `client.send_event`
                    // rather than the `deliver_locally`-wrapped path (this
                    // free fn has no `&NostrClient` to call it on): without
                    // the recursive apply below, no other locally-managed
                    // bot would see it until a genuine relay round trip,
                    // and self-published events on this shared `Client`
                    // never generate one (see `deliver_locally`'s doc
                    // comment) -- so it would otherwise never converge.
                    if let Err(e) = client.send_event(&evolution_event).await {
                        error!(
                            event_id = %event.id.to_hex(),
                            bot_id = %bot_id,
                            error = %e,
                            "failed to publish MLS evolution event from auto-committed proposal"
                        );
                    } else if let Err(e) = Box::pin(Self::process_group_message(
                        client,
                        signers,
                        mls_engines,
                        &evolution_event,
                        diagnostics,
                        attachment_processor,
                    ))
                    .await
                    {
                        error!(
                            event_id = %evolution_event.id.to_hex(),
                            error = %e,
                            "failed to apply auto-committed evolution event to other local bots"
                        );
                    }
                }
                Ok(mls::GroupMessageOutcome::None) => {
                    any_processed = true;
                }
                Err(e) => {
                    debug!(
                        bot_id = %bot_id,
                        error = %e,
                        "recipient could not process group message"
                    );
                }
            }
        }

        let Some(decrypted) = decrypted else {
            if !any_processed {
                warn!(
                    event_id = %event.id.to_hex(),
                    "no recipient could process group message"
                );
            }
            return Ok(vec![]);
        };

        let inner = UnsignedEvent::new(
            PublicKey::parse(&decrypted.author)
                .map_err(|_| DaemonError::Nostr("invalid inner group author".into()))?,
            Timestamp::from(decrypted.timestamp),
            decrypted.kind,
            decrypted.tags.clone(),
            decrypted.content.clone(),
        );

        let (event_type, reaction, attachment, content, mentions, pacto_virtual_bucket) =
            match decrypted.kind {
                Kind::PrivateDirectMessage => {
                    let (content, mentions, pacto_virtual_bucket) =
                        parse_mention_envelope(&decrypted.content);
                    (
                        EventType::MlsGroupMessageReceived,
                        None,
                        None,
                        content,
                        mentions,
                        pacto_virtual_bucket,
                    )
                }
                Kind::Reaction => match Self::extract_reaction(&inner) {
                    Some(payload) => (
                        EventType::MlsGroupReactionReceived,
                        Some(payload),
                        None,
                        decrypted.content.clone(),
                        Vec::new(),
                        None,
                    ),
                    None => {
                        if let Some(diagnostics) = diagnostics {
                            diagnostics.record_invalid_event().await;
                        }
                        return Ok(vec![]);
                    }
                },
                Kind::Custom(15) => {
                    let Some(processor) = attachment_processor else {
                        if let Some(diagnostics) = diagnostics {
                            diagnostics.record_invalid_event().await;
                            diagnostics.record_attachment_receive_failed().await;
                        }
                        return Ok(vec![]);
                    };
                    match processor.process_rumor(&inner).await {
                        Ok(payload) => {
                            if let Some(diagnostics) = diagnostics {
                                diagnostics.record_attachment_receive().await;
                            }
                            (
                                EventType::MlsGroupAttachmentReceived,
                                None,
                                Some(payload),
                                decrypted.content.clone(),
                                Vec::new(),
                                None,
                            )
                        }
                        Err(error) => {
                            warn!(error = %error, "rejecting invalid MLS group attachment");
                            if let Some(diagnostics) = diagnostics {
                                diagnostics.record_invalid_event().await;
                                diagnostics.record_attachment_receive_failed().await;
                                diagnostics
                                    .record_error(
                                        Some("attachment_invalid"),
                                        &error.to_string(),
                                        None,
                                    )
                                    .await;
                            }
                            return Ok(vec![]);
                        }
                    }
                }
                other => {
                    debug!(
                        kind = other.as_u16(),
                        "skipping unrepresented MLS inner rumor kind"
                    );
                    return Ok(vec![]);
                }
            };

        let mut agent_events = Vec::with_capacity(delivered.len());
        for recipient in &delivered {
            let (bot_id, _signer) = signers.get(recipient).ok_or_else(|| {
                DaemonError::Nostr(format!("no signer registered for {recipient}"))
            })?;
            agent_events.push(AgentEvent {
                bot_id: bot_id.clone(),
                event_id: decrypted.event_id.clone(),
                event_type,
                chat_id: Some(decrypted.group_id.clone()),
                content: content.clone(),
                mentions: mentions.clone(),
                pacto_virtual_bucket: pacto_virtual_bucket.clone(),
                rumor_id: decrypted.rumor_id.clone(),
                author: decrypted.author.clone(),
                timestamp: decrypted.timestamp,
                reaction: reaction.clone(),
                attachment: attachment.clone(),
                ..Default::default()
            });
        }

        Ok(agent_events)
    }
}

/// Trait for the subset of Nostr client operations needed by
/// [`ClientManager`](crate::client_manager::ClientManager) to subscribe bots
/// to their gift-wrap filters. It is intentionally narrow so tests can provide
/// a lightweight mock instead of a live relay pool.
#[async_trait::async_trait]
pub trait NostrSubscribe: Send + Sync {
    /// Subscribe to kind 1059 gift wraps addressed to `npub`, optionally
    /// restricted to events with `created_at` >= `since`.
    async fn subscribe_bot_with_since(
        &self,
        npub: &PublicKey,
        since: Option<Timestamp>,
    ) -> Result<SubscriptionId, DaemonError>;

    /// Subscribe to kind:445 MLS group messages addressed to `npub`, optionally
    /// restricted to events with `created_at` >= `since`.
    async fn subscribe_group_messages_with_since(
        &self,
        npub: &PublicKey,
        since: Option<Timestamp>,
    ) -> Result<SubscriptionId, DaemonError>;
}

#[async_trait::async_trait]
impl NostrSubscribe for NostrClient {
    async fn subscribe_bot_with_since(
        &self,
        npub: &PublicKey,
        since: Option<Timestamp>,
    ) -> Result<SubscriptionId, DaemonError> {
        NostrClient::subscribe_bot_with_since(self, npub, since).await
    }

    async fn subscribe_group_messages_with_since(
        &self,
        npub: &PublicKey,
        since: Option<Timestamp>,
    ) -> Result<SubscriptionId, DaemonError> {
        NostrClient::subscribe_group_messages_with_since(self, npub, since).await
    }
}

/// Sign an unsigned event using the daemon [`Signer`] trait.
pub(crate) async fn sign_unsigned_event(
    signer: &dyn Signer,
    unsigned: UnsignedEvent,
) -> Result<Event, DaemonError> {
    let mut unsigned = unsigned;
    unsigned.ensure_id();
    let id = unsigned
        .id
        .ok_or_else(|| DaemonError::Nostr("event id not set".into()))?;
    let payload = event_signing_bytes(&unsigned)?;
    let sig_hex = signer
        .sign_event(&payload)
        .await
        .map_err(|e| DaemonError::Nostr(format!("signing failed: {e}")))?;
    let sig = Signature::from_str(&sig_hex)
        .map_err(|e| DaemonError::Nostr(format!("invalid signature: {e}")))?;
    Ok(Event::new(
        id,
        unsigned.pubkey,
        unsigned.created_at,
        unsigned.kind,
        unsigned.tags.to_vec(),
        unsigned.content,
        sig,
    ))
}

/// Build the kind:0 profile event used by `pacto-bot-admin publish-profile`.
///
/// The metadata JSON includes `bot: true`, the bot's capabilities, and any
/// configured optional fields (`about`, `picture`).
pub async fn build_bot_profile_event(
    bot: &BotConfig,
    signer: &dyn Signer,
) -> Result<Event, DaemonError> {
    let name = bot.display_name.as_deref().unwrap_or(&bot.id);
    let mut profile = json!({
        "name": name,
        "bot": true,
        "capabilities": bot.capabilities,
    });
    if let Some(about) = &bot.about {
        profile["about"] = about.clone().into();
    }
    if let Some(picture) = &bot.picture {
        profile["picture"] = picture.clone().into();
    }
    let content = serde_json::to_string(&profile).map_err(DaemonError::Json)?;

    let unsigned = UnsignedEvent::new(
        signer.public_key(),
        Timestamp::now(),
        Kind::Metadata,
        Vec::new(),
        content,
    );
    sign_unsigned_event(signer, unsigned).await
}

/// Serialize the canonical event-id preimage for signing.
fn event_signing_bytes(unsigned: &UnsignedEvent) -> Result<Vec<u8>, DaemonError> {
    serde_json::to_vec(&json!([
        0,
        unsigned.pubkey,
        unsigned.created_at,
        unsigned.kind,
        unsigned.tags,
        unsigned.content
    ]))
    .map_err(DaemonError::Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::LocalKey;
    use crate::test_support::mock_relay::MockRelay;
    use nostr::ToBech32;
    use std::time::Duration;
    use tokio_stream::StreamExt;

    fn test_signer() -> (LocalKey, String) {
        let keys = nostr::Keys::generate();
        let nsec = keys.secret_key().to_bech32().unwrap();
        let npub = keys.public_key().to_bech32().unwrap();
        (LocalKey::parse(&nsec).unwrap(), npub)
    }

    fn dummy_relay() -> String {
        "wss://localhost:4242".into()
    }

    /// Test temp directory outside `/tmp`/`/dev/shm` so MLS path-hardening
    /// checks do not reject the fixture (mirrors `mls::tests::test_tempdir`,
    /// which is private to that module's own test scope).
    fn test_tempdir() -> tempfile::TempDir {
        let root = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"))
            .join("test-temp")
            .join("nostr-unit");
        std::fs::create_dir_all(&root).expect("create test temp root");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
                .expect("chmod test temp root");
        }
        tempfile::tempdir_in(root).expect("tempdir")
    }

    fn assert_valid_event_id(event_id: &EventId) {
        let hex = event_id.to_hex();
        assert_eq!(hex.len(), 64, "event id should be 64 hex chars");
        assert_ne!(
            hex, "0000000000000000000000000000000000000000000000000000000000000000",
            "event id should not be the zero id"
        );
    }

    fn test_signer_with_nsec() -> (LocalKey, String, String) {
        let keys = nostr::Keys::generate();
        let nsec = keys.secret_key().to_bech32().unwrap();
        let npub = keys.public_key().to_bech32().unwrap();
        (LocalKey::parse(&nsec).unwrap(), npub, nsec)
    }

    fn build_key_package(keys: &nostr::Keys, content: &str, created_at: Timestamp) -> Event {
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            created_at,
            Kind::MlsKeyPackage,
            Vec::new(),
            content.to_string(),
        );
        crate::nostr_json::sign_unsigned(unsigned, keys).unwrap()
    }

    fn build_key_package_bad_sig(
        keys: &nostr::Keys,
        content: &str,
        created_at: Timestamp,
    ) -> Event {
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            created_at,
            Kind::MlsKeyPackage,
            Vec::new(),
            content.to_string(),
        );
        let valid_event = crate::nostr_json::sign_unsigned(unsigned, keys).unwrap();
        let bad_sig = Signature::from_str(
            "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap();
        Event::new(
            valid_event.id,
            valid_event.pubkey,
            valid_event.created_at,
            valid_event.kind,
            valid_event.tags.to_vec(),
            valid_event.content,
            bad_sig,
        )
    }

    fn build_key_package_wrong_author(
        _recipient: &PublicKey,
        content: &str,
        created_at: Timestamp,
    ) -> Event {
        let other_keys = nostr::Keys::generate();
        let unsigned = UnsignedEvent::new(
            other_keys.public_key(),
            created_at,
            Kind::MlsKeyPackage,
            Vec::new(),
            content.to_string(),
        );
        crate::nostr_json::sign_unsigned(unsigned, &other_keys).unwrap()
    }

    #[tokio::test]
    async fn new_with_empty_relays_works() {
        let client = NostrClient::new(vec![]).await.unwrap();
        assert_eq!(client.signers.read().await.len(), 0);
    }

    #[tokio::test]
    async fn subscribe_bot_returns_subscription_id() {
        let client = NostrClient::new(vec![dummy_relay()]).await.unwrap();
        let (signer, _npub) = test_signer();
        let pubkey = signer.public_key();
        client
            .add_signer(pubkey, "bot-1".into(), Arc::new(signer))
            .await;
        let sub_id = client.subscribe_bot(&pubkey).await.unwrap();
        assert!(!sub_id.to_string().is_empty());
    }

    #[tokio::test]
    async fn send_dm_builds_gift_wrap() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();
        let (sender, _) = test_signer();
        let recipient_keys = nostr::Keys::generate();
        let recipient_npub = recipient_keys.public_key().to_bech32().unwrap();

        let event_id = client
            .send_dm(&sender, &recipient_npub, "hello", None)
            .await
            .unwrap();
        assert_valid_event_id(&event_id);

        let events = relay
            .wait_for_event(|e| e.kind == Kind::GiftWrap, Duration::from_secs(5))
            .await
            .expect("gift wrap should be published");
        assert!(events.iter().any(|e| e.kind == Kind::GiftWrap));
        relay.stop().await;
    }

    #[tokio::test]
    async fn send_attachment_publishes_gift_wrap() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();
        let (sender, _) = test_signer();
        let recipient = Keys::generate().public_key();
        let rumor = UnsignedEvent::new(
            sender.public_key(),
            Timestamp::now(),
            Kind::Custom(15),
            [Tag::public_key(recipient)],
            "https://cdn.example/blob",
        );

        let event_id = client
            .send_attachment(&sender, &recipient, rumor)
            .await
            .unwrap();
        assert_valid_event_id(&event_id);
        let events = relay
            .wait_for_event(|event| event.kind == Kind::GiftWrap, Duration::from_secs(5))
            .await
            .expect("gift wrap should be published");
        assert!(events.iter().any(|event| event.kind == Kind::GiftWrap));
        relay.stop().await;
    }

    #[tokio::test]
    async fn send_dm_with_reply_to_adds_e_tag() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();
        let (sender, _) = test_signer();
        let recipient_keys = nostr::Keys::generate();
        let recipient_npub = recipient_keys.public_key().to_bech32().unwrap();
        let reply_id =
            EventId::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();

        let event_id = client
            .send_dm(&sender, &recipient_npub, "reply", Some(&reply_id.to_hex()))
            .await
            .unwrap();
        assert_valid_event_id(&event_id);

        let events = relay
            .wait_for_event(|e| e.kind == Kind::GiftWrap, Duration::from_secs(5))
            .await
            .expect("gift wrap should be published");
        assert!(events.iter().any(|e| e.kind == Kind::GiftWrap));
        relay.stop().await;
    }

    #[test]
    fn build_dm_rumor_adds_ms_tag_and_reply_marker() {
        let (sender, _) = test_signer();
        let recipient_keys = nostr::Keys::generate();
        let reply_id =
            EventId::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();

        let rumor = NostrClient::build_dm_rumor(
            &sender.public_key(),
            &recipient_keys.public_key(),
            "hello",
            Some(&reply_id.to_hex()),
        )
        .unwrap();

        let e_tag = nostr_tags::find_e_tag(&rumor.tags).expect("rumor should have an e tag");
        assert!(e_tag.is_reply(), "e tag should be marked as reply");
        assert_eq!(e_tag.content().unwrap(), reply_id.to_hex());

        let ms_tag =
            nostr_tags::find_custom_tag(&rumor.tags, "ms").expect("rumor should have an ms tag");
        let ms_value: u64 = ms_tag.content().unwrap().parse().unwrap();
        assert!(ms_value < 1000, "ms tag must be a millisecond offset 0-999");
    }

    #[test]
    fn build_dm_rumor_adds_ms_tag_without_reply() {
        let (sender, _) = test_signer();
        let recipient_keys = nostr::Keys::generate();

        let rumor = NostrClient::build_dm_rumor(
            &sender.public_key(),
            &recipient_keys.public_key(),
            "hello",
            None,
        )
        .unwrap();

        assert!(
            nostr_tags::find_e_tag(&rumor.tags).is_none(),
            "rumor should not have an e tag"
        );
        let ms_tag =
            nostr_tags::find_custom_tag(&rumor.tags, "ms").expect("rumor should have an ms tag");
        let ms_value: u64 = ms_tag.content().unwrap().parse().unwrap();
        assert!(ms_value < 1000, "ms tag must be a millisecond offset 0-999");
    }
    #[test]
    fn dm_reaction_rumor_has_app_compatible_tags() {
        let sender = Keys::generate();
        let recipient = Keys::generate();
        let target =
            EventId::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let rumor = NostrClient::build_dm_reaction_rumor(
            &sender.public_key(),
            &recipient.public_key(),
            &target.to_hex(),
            "👍",
        )
        .unwrap();

        assert_eq!(rumor.kind, Kind::Reaction);
        assert_eq!(rumor.content, "👍");
        assert_eq!(
            nostr_tags::find_e_tag(&rumor.tags).and_then(Tag::content),
            Some(target.to_hex().as_str())
        );
        assert_eq!(
            nostr_tags::find_p_tag(&rumor.tags).and_then(Tag::content),
            Some(recipient.public_key().to_hex().as_str())
        );
        assert_eq!(
            nostr_tags::find_k_tag(&rumor.tags).and_then(Tag::content),
            Some(Kind::PrivateDirectMessage.as_u16().to_string().as_str())
        );
    }

    #[test]
    fn group_reaction_rumor_has_only_target_event_tag() {
        let sender = Keys::generate();
        let target =
            EventId::from_hex("0000000000000000000000000000000000000000000000000000000000000002")
                .unwrap();
        let rumor =
            NostrClient::build_group_reaction_rumor(&sender.public_key(), &target.to_hex(), "👩‍💻")
                .unwrap();

        assert_eq!(rumor.kind, Kind::Reaction);
        assert_eq!(rumor.content, "👩‍💻");
        assert_eq!(rumor.tags.len(), 1);
        assert_eq!(
            nostr_tags::find_e_tag(&rumor.tags).and_then(Tag::content),
            Some(target.to_hex().as_str())
        );
    }

    #[test]
    fn build_group_text_rumor_uses_kind_14_private_direct_message() {
        let sender = Keys::generate();
        let rumor =
            NostrClient::build_group_text_rumor(&sender.public_key(), "hello squad".into(), None)
                .unwrap();

        // pacto-app's message views only render kinds 14 / 15 / 30078
        // (PRIVATE_DIRECT_MESSAGE / FILE_ATTACHMENT / APPLICATION_SPECIFIC);
        // a group text rumor must be kind 14 or it is stored but never shown.
        assert_eq!(rumor.kind, Kind::PrivateDirectMessage);
        assert_eq!(rumor.kind.as_u16(), 14);
        assert_eq!(rumor.content, "hello squad");
    }

    #[test]
    fn build_group_text_rumor_mention_envelope_payload_is_unchanged() {
        let sender = Keys::generate();
        let rumor = NostrClient::build_group_text_rumor(
            &sender.public_key(),
            "hey @bot".into(),
            Some("bucket-1".into()),
        )
        .unwrap();

        assert_eq!(rumor.kind, Kind::PrivateDirectMessage);
        let envelope: serde_json::Value = serde_json::from_str(&rumor.content).unwrap();
        assert_eq!(envelope["kind"], MENTION_ENVELOPE_KIND);
        assert_eq!(envelope["body"], "hey @bot");
        assert_eq!(envelope["mentions"], serde_json::json!([]));
        assert_eq!(envelope["pacto_virtual_bucket"], "bucket-1");
    }

    #[tokio::test]
    async fn set_profile_builds_metadata_event() {
        let client = NostrClient::new(vec![dummy_relay()]).await.unwrap();
        let (signer, _npub) = test_signer();

        let event_id = client
            .set_profile(
                &signer,
                Some("Bot Name"),
                Some("About text"),
                Some("https://example.com/pic.png"),
            )
            .await
            .unwrap();
        assert_valid_event_id(&event_id);
    }

    #[tokio::test]
    async fn decrypt_gift_wrap_maps_to_agent_event() {
        let client = NostrClient::new(vec![]).await.unwrap();
        let (bot_signer, _bot_npub) = test_signer();
        let bot_pubkey = bot_signer.public_key();
        let sender_keys = nostr::Keys::generate();

        client
            .add_signer(bot_pubkey, "bot-1".into(), Arc::new(bot_signer))
            .await;

        // Build a gift-wrap addressed to the bot using the sender's keys.
        let event = EventBuilder::private_msg(
            &sender_keys,
            bot_pubkey,
            "secret message",
            Vec::<Tag>::new(),
        )
        .await
        .unwrap();

        let signers = client.signers.read().await.clone();
        let mls_engines = client.mls_engines.read().await.clone();
        let agent_event =
            NostrClient::process_gift_wrap(&signers, &mls_engines, &event, None, None)
                .await
                .unwrap()
                .expect("kind:14 rumor should deliver a dm_received event");
        assert_eq!(agent_event.bot_id, "bot-1");
        assert_eq!(agent_event.event_type, EventType::DmReceived);
        assert_eq!(agent_event.content, "secret message");
        assert_eq!(agent_event.author, sender_keys.public_key().to_hex());
    }

    #[tokio::test]
    async fn receive_events_stream_ends_when_notifications_stop() {
        let client = NostrClient::new(vec![]).await.unwrap();
        let mut stream = client.receive_events();

        // Give the spawned notification handler a chance to subscribe before
        // shutting down the client.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Shutting down the client stops the notification loop. The spawned
        // task should drop the sender and the stream should yield None.
        client.shutdown().await;

        let next = tokio::time::timeout(Duration::from_secs(5), stream.next()).await;
        assert!(
            matches!(next, Ok(None)),
            "stream should terminate after shutdown"
        );
    }

    #[tokio::test]
    async fn receive_events_yields_decrypted_agent_event() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();

        let (signer, _npub) = test_signer();
        let bot_pubkey = signer.public_key();
        client
            .add_signer(bot_pubkey, "bot-1".into(), Arc::new(signer))
            .await;

        let mut stream = client.receive_events();
        client.subscribe_bot(&bot_pubkey).await.unwrap();

        // Allow the client to connect and the relay to record the subscription.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let sender_keys = nostr::Keys::generate();
        let event = EventBuilder::private_msg(
            &sender_keys,
            bot_pubkey,
            "hello from relay",
            Vec::<Tag>::new(),
        )
        .await
        .unwrap();
        relay.inject_event(event.clone()).await;

        let next = tokio::time::timeout(Duration::from_secs(5), stream.next()).await;
        let agent_event = next
            .expect("stream should produce an event before timeout")
            .expect("stream should not end")
            .expect("event should decrypt successfully");

        assert_eq!(agent_event.bot_id, "bot-1");
        assert_eq!(agent_event.event_type, EventType::DmReceived);
        assert_eq!(agent_event.content, "hello from relay");
        assert_eq!(agent_event.author, sender_keys.public_key().to_hex());
    }

    /// Build a real, engine-issued MLS KeyPackage event for `keys`, signed
    /// with `keys` (mirrors `mls::tests::build_key_package`, which is
    /// private to that module's own test scope).
    async fn mls_key_package_for(engine: &MlsEngineHandle, keys: &Keys) -> Event {
        let relays = vec![nostr::RelayUrl::parse("wss://test.relay").unwrap()];
        let (content, tags) = engine
            .publish_key_package(&keys.public_key(), relays)
            .await
            .expect("publish_key_package");
        crate::nostr_json::sign_builder(
            EventBuilder::new(Kind::MlsKeyPackage, content).tags(tags),
            keys,
        )
        .expect("sign key package")
    }

    /// Regression test for the squad-conversation epoch-desync bug:
    /// `process_group_message` used to walk candidate engines until ONE
    /// resolved the incoming event (any outcome, including a commit that
    /// engine just merged), then return immediately -- leaving every OTHER
    /// *current* member's engine un-advanced by that commit. Once a real
    /// application message arrived at the new epoch, whichever member was
    /// never given the commit could no longer decrypt anything, silently
    /// (see `squad_conversation_replays_end_to_end_with_declared_order_in_trace`
    /// in `tests/scenario_replay.rs`, which flaked on exactly this).
    ///
    /// Three same-daemon bots (alice creates+admins, bob is an original
    /// member, carol is added later): after alice's add-carol commit
    /// lands, a *later* application message from alice must independently
    /// decrypt for both bob and carol -- not just whichever engine
    /// happened to process the commit first.
    #[tokio::test]
    async fn every_current_member_advances_past_a_later_add_member_commit() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();
        let temp = test_tempdir();

        let alice_keys = Keys::generate();
        let bob_keys = Keys::generate();
        let carol_keys = Keys::generate();
        let alice_signer: Arc<dyn Signer> =
            Arc::new(LocalKey::parse(&alice_keys.secret_key().to_bech32().unwrap()).unwrap());
        let bob_signer: Arc<dyn Signer> =
            Arc::new(LocalKey::parse(&bob_keys.secret_key().to_bech32().unwrap()).unwrap());
        let carol_signer: Arc<dyn Signer> =
            Arc::new(LocalKey::parse(&carol_keys.secret_key().to_bech32().unwrap()).unwrap());

        let alice_engine = MlsEngineHandle::new_persistent(temp.path().join("alice-mls.db"))
            .expect("new_persistent");
        let bob_engine = MlsEngineHandle::new_persistent(temp.path().join("bob-mls.db"))
            .expect("new_persistent");
        let carol_engine = MlsEngineHandle::new_persistent(temp.path().join("carol-mls.db"))
            .expect("new_persistent");

        client
            .add_signer(
                alice_keys.public_key(),
                "alice-bot".into(),
                alice_signer.clone(),
            )
            .await;
        client
            .add_signer(bob_keys.public_key(), "bob-bot".into(), bob_signer.clone())
            .await;
        client
            .add_signer(carol_keys.public_key(), "carol-bot".into(), carol_signer)
            .await;
        client
            .add_mls_engine(
                alice_keys.public_key(),
                "alice-bot".into(),
                alice_engine.clone(),
            )
            .await;
        client
            .add_mls_engine(bob_keys.public_key(), "bob-bot".into(), bob_engine.clone())
            .await;
        client
            .add_mls_engine(
                carol_keys.public_key(),
                "carol-bot".into(),
                carol_engine.clone(),
            )
            .await;

        let mut stream = client.receive_events();

        // Alice creates the squad with bob.
        let bob_kp = mls_key_package_for(&bob_engine, &bob_keys).await;
        let (wire_id, bob_welcome) = alice_engine
            .create_group(
                alice_keys.public_key(),
                bob_keys.public_key(),
                bob_kp,
                "squad-chat".to_string(),
                vec![nostr::RelayUrl::parse(&relay.url()).unwrap()],
                vec![alice_keys.public_key(), bob_keys.public_key()],
            )
            .await
            .expect("create_group failed");
        client
            .send_welcome(alice_signer.as_ref(), &bob_keys.public_key(), bob_welcome)
            .await
            .expect("send_welcome(bob) failed");

        // Alice invites carol into the existing squad.
        let carol_kp = mls_key_package_for(&carol_engine, &carol_keys).await;
        let outcome = alice_engine
            .add_member(&wire_id, carol_keys.public_key(), carol_kp)
            .await
            .expect("add_member failed");
        client
            .send_welcome(
                alice_signer.as_ref(),
                &carol_keys.public_key(),
                outcome.welcome_rumor,
            )
            .await
            .expect("send_welcome(carol) failed");
        client
            .send_evolution_event(&outcome.evolution_event)
            .await
            .expect("send_evolution_event(add carol) failed");

        // A later application message from alice: with the bug, bob's
        // engine never received the add-carol commit above and could not
        // decrypt this, so only carol (whose Welcome already encoded the
        // post-commit epoch) would receive it.
        let group_id = alice_engine
            .resolve_wire_id(&wire_id)
            .await
            .expect("resolve_wire_id failed");
        let rumor = UnsignedEvent::new(
            alice_keys.public_key(),
            Timestamp::now(),
            Kind::PrivateDirectMessage,
            Vec::new(),
            "welcome to the squad".to_string(),
        );
        client
            .send_group_message(&alice_engine, alice_signer.as_ref(), group_id, rumor)
            .await
            .expect("send_group_message failed");

        // Drain every queued `AgentEvent` (two welcomes plus the message
        // fan-out) and assert both bob and carol -- not just one of them --
        // independently decrypted the application message.
        let mut message_recipients = Vec::new();
        for _ in 0..4 {
            let Ok(Some(Ok(event))) =
                tokio::time::timeout(Duration::from_secs(5), stream.next()).await
            else {
                break;
            };
            if event.event_type == EventType::MlsGroupMessageReceived {
                assert_eq!(event.content, "welcome to the squad");
                message_recipients.push(event.bot_id);
            }
        }
        message_recipients.sort();
        assert_eq!(
            message_recipients,
            vec!["bob-bot".to_string(), "carol-bot".to_string()],
            "both bob and carol must independently decrypt the message after the add-member \
             commit; a missing bot_id means its engine never received that commit"
        );
    }

    /// One unhealthy MLS engine used to fail-close recipient discovery
    /// (`has_group_with_wire_id` returning Err aborted the whole apply),
    /// stranding healthy co-located members at a stale epoch. Soft-skip
    /// that engine so the remaining members still process the message.
    #[tokio::test]
    async fn unhealthy_engine_does_not_block_healthy_members_from_processing() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();
        let temp = test_tempdir();

        let alice_keys = Keys::generate();
        let bob_keys = Keys::generate();
        let carol_keys = Keys::generate();
        let alice_signer: Arc<dyn Signer> =
            Arc::new(LocalKey::parse(&alice_keys.secret_key().to_bech32().unwrap()).unwrap());
        let bob_signer: Arc<dyn Signer> =
            Arc::new(LocalKey::parse(&bob_keys.secret_key().to_bech32().unwrap()).unwrap());

        let alice_engine = MlsEngineHandle::new_persistent(temp.path().join("alice-mls.db"))
            .expect("new_persistent");
        let bob_engine = MlsEngineHandle::new_persistent(temp.path().join("bob-mls.db"))
            .expect("new_persistent");
        let carol_engine = MlsEngineHandle::disconnected_for_test();

        client
            .add_signer(
                alice_keys.public_key(),
                "alice-bot".into(),
                alice_signer.clone(),
            )
            .await;
        client
            .add_signer(bob_keys.public_key(), "bob-bot".into(), bob_signer.clone())
            .await;
        client
            .add_mls_engine(
                alice_keys.public_key(),
                "alice-bot".into(),
                alice_engine.clone(),
            )
            .await;
        client
            .add_mls_engine(bob_keys.public_key(), "bob-bot".into(), bob_engine.clone())
            .await;
        client
            .add_mls_engine(carol_keys.public_key(), "carol-bot".into(), carol_engine)
            .await;

        let mut stream = client.receive_events();

        let bob_kp = mls_key_package_for(&bob_engine, &bob_keys).await;
        let (wire_id, bob_welcome) = alice_engine
            .create_group(
                alice_keys.public_key(),
                bob_keys.public_key(),
                bob_kp,
                "squad-chat".to_string(),
                vec![nostr::RelayUrl::parse(&relay.url()).unwrap()],
                vec![alice_keys.public_key(), bob_keys.public_key()],
            )
            .await
            .expect("create_group failed");
        client
            .send_welcome(alice_signer.as_ref(), &bob_keys.public_key(), bob_welcome)
            .await
            .expect("send_welcome(bob) failed");

        let group_id = alice_engine
            .resolve_wire_id(&wire_id)
            .await
            .expect("resolve_wire_id failed");
        let rumor = UnsignedEvent::new(
            alice_keys.public_key(),
            Timestamp::now(),
            Kind::PrivateDirectMessage,
            Vec::new(),
            "still delivered".to_string(),
        );
        client
            .send_group_message(&alice_engine, alice_signer.as_ref(), group_id, rumor)
            .await
            .expect("send_group_message failed");

        let mut message_recipients = Vec::new();
        for _ in 0..4 {
            match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
                Ok(Some(Ok(event))) => {
                    if event.event_type == EventType::MlsGroupMessageReceived {
                        assert_eq!(event.content, "still delivered");
                        message_recipients.push(event.bot_id);
                    }
                }
                Ok(Some(Err(e))) => {
                    panic!("discovery must not fail-close on an unhealthy engine: {e}");
                }
                _ => break,
            }
        }
        message_recipients.sort();
        assert_eq!(
            message_recipients,
            vec!["bob-bot".to_string()],
            "bob must still decrypt despite carol's unhealthy engine"
        );
    }

    #[tokio::test]
    async fn receive_events_yields_error_for_unregistered_recipient() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();

        let unregistered_keys = nostr::Keys::generate();
        let unregistered_pubkey = unregistered_keys.public_key();

        let mut stream = client.receive_events();
        client.subscribe_bot(&unregistered_pubkey).await.unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;

        let sender_keys = nostr::Keys::generate();
        let event = EventBuilder::private_msg(
            &sender_keys,
            unregistered_pubkey,
            "secret message",
            Vec::<Tag>::new(),
        )
        .await
        .unwrap();
        relay.inject_event(event.clone()).await;

        let next = tokio::time::timeout(Duration::from_secs(5), stream.next()).await;
        let err = next
            .expect("stream should produce an item before timeout")
            .expect("stream should not end")
            .expect_err("decryption should fail for unregistered recipient");

        let expected = format!("no signer registered for {unregistered_pubkey}");
        let msg = match err {
            DaemonError::Nostr(msg) => msg,
            other => panic!("expected Nostr error, got {other:?}"),
        };
        assert_eq!(msg, expected, "error should report missing signer");
    }

    #[tokio::test]
    async fn receive_events_ignores_non_gift_wrap_events() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();

        let (signer, _npub) = test_signer();
        let bot_pubkey = signer.public_key();
        client
            .add_signer(bot_pubkey, "bot-1".into(), Arc::new(signer))
            .await;

        let mut stream = client.receive_events();
        client.subscribe_bot(&bot_pubkey).await.unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;

        let sender_keys = nostr::Keys::generate();
        let text_note = EventBuilder::text_note("not a gift wrap")
            .sign(&sender_keys)
            .await
            .unwrap();
        // The relay filter will match only kind 1059 events, so the text note
        // should never reach the client. We still verify the stream is not
        // polluted by unrelated events.
        relay.inject_event(text_note).await;

        // The stream should remain open and produce nothing for the text note.
        let next = tokio::time::timeout(Duration::from_millis(500), stream.next()).await;
        assert!(
            next.is_err(),
            "non-gift-wrap event should not be emitted on the stream"
        );

        client.shutdown().await;
    }

    #[tokio::test]
    async fn fetch_key_package_selects_fresh_package() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();

        let recipient_keys = nostr::Keys::generate();
        let recipient = recipient_keys.public_key();
        let secret_marker = "SENSITIVE_KP_CIPHERTEXT_abc123";

        let event = build_key_package(&recipient_keys, secret_marker, Timestamp::now());
        relay.inject_event(event).await;

        let (fetched, age) = client
            .fetch_key_package(&recipient, Duration::from_secs(5), Duration::from_secs(60))
            .await
            .expect("fresh key package should be fetched");

        assert_eq!(fetched.kind, Kind::MlsKeyPackage);
        assert_eq!(fetched.pubkey, recipient);
        assert!(fetched.content.contains(secret_marker));
        assert!(age <= Duration::from_secs(5));
    }

    #[tokio::test]
    async fn fetch_key_package_selects_fresh_over_older() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();

        let recipient_keys = nostr::Keys::generate();
        let recipient = recipient_keys.public_key();
        let older_marker = "OLDER_KP_CIPHERTEXT_abc123";
        let fresh_marker = "FRESH_KP_CIPHERTEXT_xyz789";

        // Inject an older package first, then a fresh one. The relay returns
        // events in injection order, so a .limit(1) would return only the older
        // package and the client would select it incorrectly. With a generous
        // limit (or no limit), the client should select the fresh package.
        let older_ts = Timestamp::from_secs(Timestamp::now().as_secs() - 120);
        let older_event = build_key_package(&recipient_keys, older_marker, older_ts);
        relay.inject_event(older_event).await;

        let fresh_event = build_key_package(&recipient_keys, fresh_marker, Timestamp::now());
        relay.inject_event(fresh_event).await;

        let (fetched, age) = client
            .fetch_key_package(&recipient, Duration::from_secs(5), Duration::from_secs(300))
            .await
            .expect("fresh key package should be selected");

        assert_eq!(fetched.kind, Kind::MlsKeyPackage);
        assert_eq!(fetched.pubkey, recipient);
        assert!(
            fetched.content.contains(fresh_marker),
            "fetched package should contain the fresh content"
        );
        assert!(
            !fetched.content.contains(older_marker),
            "fetched package should not contain the older content"
        );
        assert!(age <= Duration::from_secs(5));
    }

    #[tokio::test]
    async fn fetch_key_package_rejects_stale_package() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();

        let recipient_keys = nostr::Keys::generate();
        let recipient = recipient_keys.public_key();
        let secret_marker = "STALE_KP_CIPHERTEXT_abc123";

        let stale_ts = Timestamp::from_secs(Timestamp::now().as_secs() - 301);
        let event = build_key_package(&recipient_keys, secret_marker, stale_ts);
        relay.inject_event(event).await;

        let err = client
            .fetch_key_package(&recipient, Duration::from_secs(5), Duration::from_secs(300))
            .await
            .unwrap_err();

        assert!(
            matches!(err, DaemonError::StaleKeyPackage),
            "expected StaleKeyPackage when stale package is outside the freshness window, got {err:?}"
        );
        assert!(!err.to_string().contains(secret_marker));
    }

    #[tokio::test]
    async fn fetch_key_package_rejects_future_package() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();

        let recipient_keys = nostr::Keys::generate();
        let recipient = recipient_keys.public_key();

        let future_ts = Timestamp::from_secs(Timestamp::now().as_secs() + 61);
        let event = build_key_package(&recipient_keys, "FUTURE_KP_CIPHERTEXT", future_ts);
        relay.inject_event(event).await;

        let err = client
            .fetch_key_package(&recipient, Duration::from_secs(5), Duration::from_secs(300))
            .await
            .unwrap_err();

        assert!(
            matches!(err, DaemonError::StaleKeyPackage),
            "expected StaleKeyPackage for future-dated package, got {err:?}"
        );
    }

    #[tokio::test]
    async fn fetch_key_package_treats_forge_as_absent() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();

        let recipient_keys = nostr::Keys::generate();
        let recipient = recipient_keys.public_key();
        let secret_marker = "FORGED_KP_CIPHERTEXT_abc123";

        let wrong_author =
            build_key_package_wrong_author(&recipient, secret_marker, Timestamp::now());
        relay.inject_event(wrong_author).await;

        let bad_sig = build_key_package_bad_sig(&recipient_keys, secret_marker, Timestamp::now());
        relay.inject_event(bad_sig).await;

        eprintln!(
            "DEBUG: relay.events().len() = {}",
            relay.events().await.len()
        );
        for e in relay.events().await {
            eprintln!(
                "DEBUG: relay event pubkey={} kind={} created_at={}",
                e.pubkey.to_hex(),
                e.kind.as_u16(),
                e.created_at.as_secs()
            );
        }

        let err = client
            .fetch_key_package(&recipient, Duration::from_secs(5), Duration::from_secs(300))
            .await
            .unwrap_err();

        assert!(
            matches!(err, DaemonError::KeyPackageNotFound { .. }),
            "expected KeyPackageNotFound error when client filters out forged packages, got {err:?}"
        );
        assert!(!err.to_string().contains(secret_marker));
    }

    #[tokio::test]
    async fn fetch_key_package_returns_no_valid_package_when_none_arrives() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();

        let recipient = nostr::Keys::generate().public_key();

        let err = client
            .fetch_key_package(
                &recipient,
                Duration::from_millis(200),
                Duration::from_secs(300),
            )
            .await
            .unwrap_err();

        assert!(
            matches!(err, DaemonError::KeyPackageNotFound { .. }),
            "expected KeyPackageNotFound error, got {err:?}"
        );
        assert!(
            err.to_string().contains("no key package found"),
            "error should include operator guidance: {err}"
        );
    }

    #[tokio::test]
    async fn send_welcome_publishes_gift_wrap_to_all_relays() {
        let relay1 = MockRelay::start().await.expect("mock relay should start");
        let relay2 = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay1.url(), relay2.url()])
            .await
            .unwrap();

        let (sender, _) = test_signer();
        let recipient_keys = nostr::Keys::generate();
        let recipient = recipient_keys.public_key();
        let secret_marker = "WELCOME_SECRET_RUMOR_xyz789";

        let welcome_rumor = UnsignedEvent::new(
            sender.public_key(),
            Timestamp::now(),
            Kind::MlsWelcome,
            Vec::new(),
            secret_marker.to_string(),
        );

        let event_id = client
            .send_welcome(&sender, &recipient, welcome_rumor)
            .await
            .expect("welcome should be published");
        assert_valid_event_id(&event_id);

        let events1 = relay1
            .wait_for_event(|e| e.kind == Kind::GiftWrap, Duration::from_secs(5))
            .await
            .expect("relay1 should receive gift wrap");
        let events2 = relay2
            .wait_for_event(|e| e.kind == Kind::GiftWrap, Duration::from_secs(5))
            .await
            .expect("relay2 should receive gift wrap");

        let gw1 = events1
            .iter()
            .find(|e| e.kind == Kind::GiftWrap)
            .expect("relay1 should store the gift wrap");
        let gw2 = events2
            .iter()
            .find(|e| e.kind == Kind::GiftWrap)
            .expect("relay2 should store the gift wrap");

        assert_eq!(
            gw1.id, gw2.id,
            "same gift wrap should be published to both relays"
        );
        assert_eq!(gw1.kind.as_u16(), 1059);
        assert!(gw1.tags.public_keys().any(|p| *p == recipient));
        assert!(!gw1.content.contains(secret_marker));
    }

    #[tokio::test]
    async fn send_evolution_event_publishes_signed_event() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();

        let (signer, _npub) = test_signer();
        let unsigned = UnsignedEvent::new(
            signer.public_key(),
            Timestamp::now(),
            Kind::MlsGroupMessage,
            Vec::new(),
            "evolution content".to_string(),
        );
        let event = sign_unsigned_event(&signer, unsigned)
            .await
            .expect("should sign evolution event");

        let event_id = client
            .send_evolution_event(&event)
            .await
            .expect("evolution event should be published");
        assert_valid_event_id(&event_id);
        assert_eq!(event_id, event.id);

        let events = relay.events().await;
        let published = events
            .iter()
            .find(|e| e.id == event_id)
            .expect("relay should store the evolution event");
        assert_eq!(published.kind, Kind::MlsGroupMessage);
        assert_eq!(published.pubkey, signer.public_key());
    }

    #[tokio::test]
    async fn fetch_and_welcome_errors_do_not_leak_secrets() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();

        let (sender, _, nsec) = test_signer_with_nsec();
        let recipient_keys = nostr::Keys::generate();
        let recipient = recipient_keys.public_key();

        let kp_secret = "KP_SECRET_CIPHERTEXT_abc123";
        let stale_ts = Timestamp::from_secs(Timestamp::now().as_secs() - 301);
        let kp_event = build_key_package(&recipient_keys, kp_secret, stale_ts);
        relay.inject_event(kp_event).await;

        let err = client
            .fetch_key_package(&recipient, Duration::from_secs(2), Duration::from_secs(300))
            .await
            .unwrap_err();

        let err_msg = err.to_string();
        assert!(
            !err_msg.contains(kp_secret),
            "fetch error must not contain key package ciphertext"
        );
        assert!(
            !err_msg.contains(&nsec),
            "fetch error must not contain signer nsec"
        );

        let client_no_relay = NostrClient::new(vec![]).await.unwrap();
        let welcome_secret = "WELCOME_SECRET_RUMOR_xyz789";
        let welcome_rumor = UnsignedEvent::new(
            sender.public_key(),
            Timestamp::now(),
            Kind::MlsWelcome,
            Vec::new(),
            welcome_secret.to_string(),
        );

        let err = client_no_relay
            .send_welcome(&sender, &recipient, welcome_rumor)
            .await
            .unwrap_err();

        let err_msg = err.to_string();
        assert!(
            !err_msg.contains(welcome_secret),
            "welcome error must not contain rumor content"
        );
        assert!(
            !err_msg.contains(&nsec),
            "welcome error must not contain signer nsec"
        );
    }

    #[test]
    fn parse_mention_envelope_valid() {
        let plaintext = r#"{"kind":"pacto.mentions.envelope.v1","body":"@Joke Bot /help","mentions":[{"npub":"npub1joke","alias":"Joke Bot"}]}"#;
        let (content, mentions, bucket) = parse_mention_envelope(plaintext);
        assert_eq!(content, "@Joke Bot /help");
        assert_eq!(mentions, vec!["npub1joke"]);
        assert_eq!(bucket, None);
    }

    #[test]
    fn parse_mention_envelope_with_virtual_bucket() {
        let plaintext = r#"{"kind":"pacto.mentions.envelope.v1","body":"@Joke Bot /help","mentions":[{"npub":"npub1joke"}],"pacto_virtual_bucket":"squad:abc123"}"#;
        let (content, mentions, bucket) = parse_mention_envelope(plaintext);
        assert_eq!(content, "@Joke Bot /help");
        assert_eq!(mentions, vec!["npub1joke"]);
        assert_eq!(bucket.as_deref(), Some("squad:abc123"));
    }

    #[test]
    fn parse_mention_envelope_wrong_kind_falls_back() {
        let plaintext = r#"{"kind":"pacto.mentions.envelope.v0","body":"@Joke Bot /help","mentions":[{"npub":"npub1joke"}]}"#;
        let (content, mentions, bucket) = parse_mention_envelope(plaintext);
        assert_eq!(content, plaintext);
        assert!(mentions.is_empty());
        assert_eq!(bucket, None);
    }

    #[test]
    fn parse_mention_envelope_legacy_plaintext() {
        let plaintext = "!snapshot";
        let (content, mentions, bucket) = parse_mention_envelope(plaintext);
        assert_eq!(content, "!snapshot");
        assert!(mentions.is_empty());
        assert_eq!(bucket, None);
    }

    #[test]
    fn parse_mention_envelope_invalid_json() {
        let plaintext = "{not json";
        let (content, mentions, bucket) = parse_mention_envelope(plaintext);
        assert_eq!(content, "{not json");
        assert!(mentions.is_empty());
        assert_eq!(bucket, None);
    }

    #[test]
    fn parse_mention_envelope_missing_kind_falls_back() {
        let plaintext = r#"{"body":"@Joke Bot /help","mentions":[{"npub":"npub1joke"}]}"#;
        let (content, mentions, bucket) = parse_mention_envelope(plaintext);
        assert_eq!(content, plaintext);
        assert!(mentions.is_empty());
        assert_eq!(bucket, None);
    }

    #[test]
    fn parse_mention_envelope_missing_body_falls_back() {
        let plaintext =
            r#"{"kind":"pacto.mentions.envelope.v1","mentions":[{"npub":"npub1joke"}]}"#;
        let (content, mentions, bucket) = parse_mention_envelope(plaintext);
        assert_eq!(content, plaintext);
        assert!(mentions.is_empty());
        assert_eq!(bucket, None);
    }

    #[test]
    fn parse_mention_envelope_missing_mentions_falls_back() {
        let plaintext = r#"{"kind":"pacto.mentions.envelope.v1","body":"hello"}"#;
        let (content, mentions, bucket) = parse_mention_envelope(plaintext);
        assert_eq!(content, plaintext);
        assert!(mentions.is_empty());
        assert_eq!(bucket, None);
    }

    #[test]
    fn parse_mention_envelope_missing_npub_falls_back() {
        let plaintext = r#"{"kind":"pacto.mentions.envelope.v1","body":"hello","mentions":[{"alias":"Joke Bot"}]}"#;
        let (content, mentions, bucket) = parse_mention_envelope(plaintext);
        assert_eq!(content, plaintext);
        assert!(mentions.is_empty());
        assert_eq!(bucket, None);
    }

    #[test]
    fn parse_mention_envelope_empty_mentions() {
        let plaintext = r#"{"kind":"pacto.mentions.envelope.v1","body":"hello","mentions":[]}"#;
        let (content, mentions, bucket) = parse_mention_envelope(plaintext);
        assert_eq!(content, "hello");
        assert!(mentions.is_empty());
        assert_eq!(bucket, None);
    }

    #[test]
    fn parse_mention_envelope_ignores_alias_and_extra_fields() {
        let plaintext = r#"{"kind":"pacto.mentions.envelope.v1","body":"hi","mentions":[{"npub":"npub1a","alias":"A"},{"npub":"npub1b"}],"extra":true}"#;
        let (content, mentions, bucket) = parse_mention_envelope(plaintext);
        assert_eq!(content, "hi");
        assert_eq!(mentions, vec!["npub1a", "npub1b"]);
        assert_eq!(bucket, None);
    }

    #[test]
    fn parse_mention_envelope_unicode_and_escapes() {
        let plaintext = "{\"kind\":\"pacto.mentions.envelope.v1\",\"body\":\"@Joke Bot \\u263A\",\"mentions\":[{\"npub\":\"npub1joke\"}]}";
        let (content, mentions, bucket) = parse_mention_envelope(plaintext);
        assert_eq!(content, "@Joke Bot ☺");
        assert_eq!(mentions, vec!["npub1joke"]);
        assert_eq!(bucket, None);
    }
}
