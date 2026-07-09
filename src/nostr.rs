//! Nostr relay client wrapper.
//!
//! Provides a thin, bot-aware layer over [`nostr_sdk::Client`] for sending and
//! receiving NIP-17 / NIP-59 direct messages (gift wraps, kind 1059).

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use nostr::event::tag::{Tag, TagKind};
use nostr::nips::nip44::Version;
use nostr::nips::{nip44, nip59};
use nostr::secp256k1::schnorr::Signature;
use nostr::{
    Event, EventBuilder, EventId, Filter, JsonUtil, Keys, Kind, PublicKey, SubscriptionId,
    Timestamp, ToBech32, UnsignedEvent,
};
use nostr_sdk::{Client, RelayPoolNotification};
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_stream::Stream;
use tokio_stream::wrappers::UnboundedReceiverStream;

use tracing::{debug, error, info};

use crate::config::BotConfig;
use crate::diagnostics::Diagnostics;
use crate::errors::DaemonError;
use crate::events::{AgentEvent, EventType};
use crate::mls::MlsEngineHandle;
use crate::signer::Signer;

/// Bot signer storage: maps recipient public key to bot id and signer.
type BotSigners = HashMap<PublicKey, (String, Arc<dyn Signer>)>;

/// Wrapper around [`nostr_sdk::Client`] providing Pacto-specific relay operations.
#[derive(Clone)]
pub struct NostrClient {
    client: Client,
    signers: Arc<RwLock<BotSigners>>,
    mls_engines: Arc<RwLock<HashMap<PublicKey, (String, MlsEngineHandle)>>>,
    diagnostics: Option<Diagnostics>,
}

impl std::fmt::Debug for NostrClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NostrClient")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
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
            rumor_builder = rumor_builder.tags([Tag::custom(
                TagKind::e(),
                [event_id.to_hex(), String::new(), String::from("reply")],
            )]);
        }
        rumor_builder = rumor_builder.tag(Tag::custom(TagKind::custom("ms"), [ms]));
        Ok(rumor_builder.build(*sender))
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
            .nip44_encrypt(recipient, &rumor.as_json())
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
            seal_event.as_json(),
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
        let gift_event = gift
            .sign_with_keys(&ephemeral)
            .map_err(|e| DaemonError::Nostr(format!("failed to sign gift wrap: {e}")))?;

        let output = self
            .client
            .send_event(&gift_event)
            .await
            .map_err(|e| DaemonError::Nostr(format!("failed to publish event: {e}")))?;

        Ok(*output.id())
    }

    /// Fetch a fresh kind:443 KeyPackage authored by `recipient` from the relay
    /// pool.
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
        let filter = Filter::new()
            .kind(Kind::MlsKeyPackage)
            .author(*recipient)
            .limit(1);

        let events = self
            .client
            .fetch_events(filter, timeout)
            .await
            .map_err(|e| DaemonError::Nostr(format!("key package fetch failed: {e}")))?;

        if events.is_empty() {
            return Err(DaemonError::Nostr("key package fetch timed out".into()));
        }

        let now = Timestamp::now().as_u64();
        let freshness_secs = freshness.as_secs();
        let mut selected: Option<Event> = None;
        let mut selected_ts: u64 = 0;

        for event in events.iter() {
            if event.verify().is_err() {
                continue;
            }
            if event.kind != Kind::MlsKeyPackage || event.pubkey != *recipient {
                continue;
            }
            let event_ts = event.created_at.as_u64();
            if event_ts > now + 60 {
                continue;
            }
            if event_ts + freshness_secs < now {
                continue;
            }
            if event_ts > selected_ts {
                selected_ts = event_ts;
                selected = Some(event.clone());
            }
        }

        let event = selected.ok_or(DaemonError::StaleKeyPackage)?;
        let age = Duration::from_secs(now - selected_ts);
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

        info!(
            event_id = %event.id.to_hex(),
            author = %event.pubkey.to_hex(),
            "published evolution event"
        );

        Ok(*output.id())
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
        content: String,
    ) -> Result<EventId, DaemonError> {
        let bot_pubkey = signer.public_key();

        // Build the plaintext inner rumor event. The inner kind must differ from
        // the kind:445 MLS wrapper so that decrypted group content is not mistaken
        // for the wire-format wrapper itself.
        let rumor = UnsignedEvent::new(
            bot_pubkey,
            Timestamp::now(),
            Kind::TextNote,
            Vec::new(),
            content,
        );

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

        Ok(*output.id())
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
    pub async fn decrypt_event(&self, event: &Event) -> Result<AgentEvent, DaemonError> {
        let snapshot = self.signers.read().await.clone();
        Self::process_gift_wrap(&snapshot, event, self.diagnostics.as_ref()).await
    }

    /// Return an async stream of incoming DMs converted to [`AgentEvent`].
    pub fn receive_events(&self) -> impl Stream<Item = Result<AgentEvent, DaemonError>> + use<> {
        let (tx, rx) = unbounded_channel();
        let client = self.client.clone();
        let signers = Arc::clone(&self.signers);
        let mls_engines = Arc::clone(&self.mls_engines);
        let diagnostics = self.diagnostics.clone();

        tokio::spawn(async move {
            let _ = client
                .handle_notifications(|notification| {
                    let tx: UnboundedSender<Result<AgentEvent, DaemonError>> = tx.clone();
                    let signers = Arc::clone(&signers);
                    let mls_engines = Arc::clone(&mls_engines);
                    let diagnostics = diagnostics.clone();
                    async move {
                        match notification {
                            RelayPoolNotification::Event { event, .. } => {
                                if event.kind == Kind::GiftWrap {
                                    // Spawn each gift-wrap decryption in its own task so that
                                    // one bot's slow signer (e.g. a NIP-46 bunker) does not block
                                    // other bots from receiving DMs.
                                    let tx = tx.clone();
                                    let signers = Arc::clone(&signers);
                                    let diagnostics = diagnostics.clone();
                                    tokio::spawn(async move {
                                        let snapshot = signers.read().await.clone();
                                        let result = Self::process_gift_wrap(
                                            &snapshot,
                                            &event,
                                            diagnostics.as_ref(),
                                        )
                                        .await;
                                        let _ = tx.send(result);
                                    });
                                } else if event.kind == Kind::MlsGroupMessage {
                                    // Spawn each group message decryption in its own task so that
                                    // one bot's slow MLS engine does not block other bots.
                                    let tx = tx.clone();
                                    let signers = Arc::clone(&signers);
                                    let mls_engines = Arc::clone(&mls_engines);
                                    tokio::spawn(async move {
                                        let signers = signers.read().await.clone();
                                        let mls_engines = mls_engines.read().await.clone();
                                        match Self::process_group_message(
                                            &signers,
                                            &mls_engines,
                                            &event,
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
                                }
                                Ok(false)
                            }
                            RelayPoolNotification::Shutdown => Ok(true),
                            _ => Ok(false),
                        }
                    }
                })
                .await;
        });

        UnboundedReceiverStream::new(rx)
    }

    async fn process_gift_wrap(
        signers: &HashMap<PublicKey, (String, Arc<dyn Signer>)>,
        event: &Event,
        diagnostics: Option<&Diagnostics>,
    ) -> Result<AgentEvent, DaemonError> {
        info!(
            event_id = %event.id.to_hex(),
            kind = %event.kind.as_u16(),
            "received gift wrap from relay"
        );

        if let Err(e) = event.verify() {
            let message = format!("gift wrap signature verification failed: {e}");
            if let Some(d) = diagnostics {
                d.record_invalid_event().await;
                d.record_error(Some("gift_wrap_verify_failed"), &message, None)
                    .await;
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
        let seal_event = Event::from_json(&seal_json)
            .map_err(|e| DaemonError::Nostr(format!("invalid seal event: {e}")))?;

        if let Err(e) = seal_event.verify() {
            let message = format!("seal signature verification failed: {e}");
            if let Some(d) = diagnostics {
                d.record_invalid_event().await;
                d.record_error(Some("seal_verify_failed"), &message, None)
                    .await;
            }
            return Err(DaemonError::Nostr(message));
        }

        // Seal is encrypted by the sender to the recipient.
        let rumor_json = signer
            .nip44_decrypt(&seal_event.pubkey, &seal_event.content)
            .await
            .map_err(|e| DaemonError::Nostr(format!("failed to decrypt seal: {e}")))?;
        let rumor = UnsignedEvent::from_json(&rumor_json)
            .map_err(|e| DaemonError::Nostr(format!("invalid rumor event: {e}")))?;

        let rumor_id = rumor
            .id
            .ok_or_else(|| DaemonError::Nostr("rumor missing id".into()))?
            .to_hex();

        let event_type = if rumor.kind == Kind::MlsWelcome {
            EventType::MlsWelcomeReceived
        } else {
            EventType::DmReceived
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

        Ok(AgentEvent {
            bot_id: bot_id.clone(),
            event_id: event.id.to_hex(),
            event_type,
            chat_id: None,
            content: rumor.content,
            rumor_id,
            author: seal_event.pubkey.to_hex(),
            timestamp: rumor.created_at.as_u64(),
        })
    }

    /// Decrypt a kind:445 MLS group message wrapper and produce an
    /// [`AgentEvent`] for application messages.
    ///
    /// Protocol-only messages (proposals, commits, etc.) return `Ok(None)` so
    /// they do not fan out to handlers. Skip-own and membership checks are
    /// intentionally stubbed here and will be implemented in U3.
    async fn process_group_message(
        signers: &HashMap<PublicKey, (String, Arc<dyn Signer>)>,
        mls_engines: &HashMap<PublicKey, (String, MlsEngineHandle)>,
        event: &Event,
    ) -> Result<Option<AgentEvent>, DaemonError> {
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
        // we identify the recipient by finding a bot that is a member of the
        // group identified by the h tag.
        let group_id = event
            .tags
            .iter()
            .find(|t| t.kind() == nostr::TagKind::h())
            .and_then(|t| t.content())
            .map(|s| s.to_string())
            .ok_or_else(|| DaemonError::Nostr("group message missing h tag".into()))?;

        let mut recipient: Option<PublicKey> = None;
        debug!(
            "looking for group_id={} among {} engines",
            group_id,
            mls_engines.len()
        );
        for (pubkey, (_, mls)) in mls_engines {
            match mls.has_group_with_wire_id(&group_id).await {
                Ok(true) => {
                    debug!("found engine for pubkey={} with group {}", pubkey, group_id);
                    recipient = Some(*pubkey);
                    break;
                }
                Ok(false) => {
                    debug!(
                        "engine for pubkey={} does NOT have group {}",
                        pubkey, group_id
                    );
                    continue;
                }
                Err(e) => {
                    return Err(DaemonError::Nostr(format!(
                        "failed to check squad membership: {e}"
                    )));
                }
            }
        }

        let recipient = recipient.ok_or_else(|| {
            DaemonError::Nostr("group message not addressed to a bot with an MLS engine".into())
        })?;

        let (bot_id, signer) = signers
            .get(&recipient)
            .ok_or_else(|| DaemonError::Nostr(format!("no signer registered for {recipient}")))?;
        let (_mls_bot_id, mls) = mls_engines.get(&recipient).ok_or_else(|| {
            DaemonError::Nostr(format!("no MLS engine registered for {recipient}"))
        })?;

        // U3: skip own events.
        if event.pubkey == signer.public_key() {
            debug!(
                event_id = %event.id.to_hex(),
                bot_id = %bot_id,
                "skipping own group message"
            );
            return Ok(None);
        }

        let decrypted = mls
            .decrypt_group_message(event)
            .await
            .map_err(|e| DaemonError::Nostr(format!("failed to decrypt group message: {e}")))?;

        if let Some(decrypted) = decrypted {
            Ok(Some(AgentEvent {
                bot_id: bot_id.clone(),
                event_id: decrypted.event_id,
                event_type: EventType::MlsGroupMessageReceived,
                chat_id: Some(decrypted.group_id),
                content: decrypted.content,
                rumor_id: event.id.to_hex(),
                author: decrypted.author,
                timestamp: decrypted.timestamp,
            }))
        } else {
            Ok(None)
        }
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
async fn sign_unsigned_event(
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
        unsigned.sign_with_keys(keys).unwrap()
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
        let valid_event = unsigned.sign_with_keys(keys).unwrap();
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
        unsigned.sign_with_keys(&other_keys).unwrap()
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
        let client = NostrClient::new(vec![dummy_relay()]).await.unwrap();
        let (sender, _) = test_signer();
        let recipient_keys = nostr::Keys::generate();
        let recipient_npub = recipient_keys.public_key().to_bech32().unwrap();

        let event_id = client
            .send_dm(&sender, &recipient_npub, "hello", None)
            .await
            .unwrap();
        assert_valid_event_id(&event_id);
    }

    #[tokio::test]
    async fn send_dm_with_reply_to_adds_e_tag() {
        let client = NostrClient::new(vec![dummy_relay()]).await.unwrap();
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

        let e_tag = rumor
            .tags
            .find(TagKind::e())
            .expect("rumor should have an e tag");
        assert!(e_tag.is_reply(), "e tag should be marked as reply");
        assert_eq!(e_tag.content().unwrap(), reply_id.to_hex());

        let ms_tag = rumor
            .tags
            .find(TagKind::custom("ms"))
            .expect("rumor should have an ms tag");
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
            rumor.tags.find(TagKind::e()).is_none(),
            "rumor should not have an e tag"
        );
        let ms_tag = rumor
            .tags
            .find(TagKind::custom("ms"))
            .expect("rumor should have an ms tag");
        let ms_value: u64 = ms_tag.content().unwrap().parse().unwrap();
        assert!(ms_value < 1000, "ms tag must be a millisecond offset 0-999");
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
        let agent_event = NostrClient::process_gift_wrap(&signers, &event, None)
            .await
            .unwrap();
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
    async fn fetch_key_package_rejects_stale_package() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();

        let recipient_keys = nostr::Keys::generate();
        let recipient = recipient_keys.public_key();
        let secret_marker = "STALE_KP_CIPHERTEXT_abc123";

        let stale_ts = Timestamp::from_secs(Timestamp::now().as_u64() - 301);
        let event = build_key_package(&recipient_keys, secret_marker, stale_ts);
        relay.inject_event(event).await;

        let err = client
            .fetch_key_package(&recipient, Duration::from_secs(5), Duration::from_secs(300))
            .await
            .unwrap_err();

        assert!(
            matches!(err, DaemonError::StaleKeyPackage),
            "expected StaleKeyPackage, got {err:?}"
        );
        assert!(!err.to_string().contains(secret_marker));
    }

    #[tokio::test]
    async fn fetch_key_package_rejects_future_package() {
        let relay = MockRelay::start().await.expect("mock relay should start");
        let client = NostrClient::new(vec![relay.url()]).await.unwrap();

        let recipient_keys = nostr::Keys::generate();
        let recipient = recipient_keys.public_key();

        let future_ts = Timestamp::from_secs(Timestamp::now().as_u64() + 61);
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

        let err = client
            .fetch_key_package(&recipient, Duration::from_secs(5), Duration::from_secs(300))
            .await
            .unwrap_err();

        assert!(
            matches!(err, DaemonError::Nostr(_) | DaemonError::StaleKeyPackage),
            "expected timeout or StaleKeyPackage when only forged packages are present, got {err:?}"
        );
        assert!(!err.to_string().contains(secret_marker));
    }

    #[tokio::test]
    async fn fetch_key_package_returns_timeout_when_none_arrives() {
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
            matches!(err, DaemonError::Nostr(_)),
            "expected Nostr error, got {err:?}"
        );
        assert!(err.to_string().contains("timed out"));
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
        let stale_ts = Timestamp::from_secs(Timestamp::now().as_u64() - 301);
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
}
