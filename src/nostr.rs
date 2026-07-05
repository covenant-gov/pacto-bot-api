//! Nostr relay client wrapper.
//!
//! Provides a thin, bot-aware layer over [`nostr_sdk::Client`] for sending and
//! receiving NIP-17 / NIP-59 direct messages (gift wraps, kind 1059).

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

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

use tracing::{error, info};

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

    /// Register a signer for a bot so that incoming gift wraps addressed to
    /// `pubkey` can be decrypted.
    pub async fn add_signer(&self, pubkey: PublicKey, bot_id: String, signer: Arc<dyn Signer>) {
        self.signers.write().await.insert(pubkey, (bot_id, signer));
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

        let seal_content = signer
            .nip44_encrypt(&recipient, &rumor_event.as_json())
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
            &recipient,
            seal_event.as_json(),
            Version::default(),
        )
        .map_err(|e| DaemonError::Nostr(format!("failed to encrypt gift wrap: {e}")))?;
        let gift = UnsignedEvent::new(
            ephemeral.public_key(),
            nostr::Timestamp::tweaked(nip59::RANGE_RANDOM_TIMESTAMP_TWEAK),
            Kind::GiftWrap,
            [Tag::public_key(recipient)],
            gift_content,
        );
        let gift_event = gift
            .sign_with_keys(&ephemeral)
            .map_err(|e| DaemonError::Nostr(format!("failed to sign gift wrap: {e}")))?;

        let output = self.client.send_event(&gift_event).await.map_err(|e| {
            error!(
                bot_npub = %bot_npub,
                recipient = %recipient_npub,
                error = %e,
                "failed to publish DM"
            );
            DaemonError::Nostr(format!("failed to publish event: {e}"))
        })?;

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

        // Build the rumor event for the group message
        let rumor = UnsignedEvent::new(
            bot_pubkey,
            Timestamp::now(),
            Kind::MlsGroupMessage,
            Vec::new(),
            content,
        );

        let wrapper = mls_engine
            .create_group_message(group_id, rumor)
            .await
            .map_err(|e| DaemonError::Nostr(format!("MLS group message creation failed: {e}")))?;

        let output = self.client.send_event(&wrapper).await.map_err(|e| {
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

    /// Decrypt a single incoming gift-wrap event using the registered bot signer.
    pub async fn decrypt_event(&self, event: &Event) -> Result<AgentEvent, DaemonError> {
        let snapshot = self.signers.read().await.clone();
        Self::process_gift_wrap(&snapshot, event, self.diagnostics.as_ref()).await
    }

    /// Return an async stream of incoming DMs converted to [`AgentEvent`].
    pub fn receive_events(&self) -> impl Stream<Item = Result<AgentEvent, DaemonError>> {
        let (tx, rx) = unbounded_channel();
        let client = self.client.clone();
        let signers = Arc::clone(&self.signers);
        let diagnostics = self.diagnostics.clone();

        tokio::spawn(async move {
            let _ = client
                .handle_notifications(|notification| {
                    let tx: UnboundedSender<Result<AgentEvent, DaemonError>> = tx.clone();
                    let signers = Arc::clone(&signers);
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
        if let Err(e) = event.verify() {
            let message = format!("gift wrap signature verification failed: {e}");
            if let Some(d) = diagnostics {
                d.record_invalid_event();
                d.record_error(Some("gift_wrap_verify_failed"), &message, None);
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
                d.record_invalid_event();
                d.record_error(Some("seal_verify_failed"), &message, None);
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

        // Detect MLS Welcome messages (kind:444) and route them separately
        let event_type = if rumor.kind == Kind::MlsWelcome {
            EventType::MlsWelcomeReceived
        } else {
            EventType::DmReceived
        };

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

    #[tokio::test]
    async fn new_with_empty_relays_works() {
        let client = NostrClient::new(vec![]).await.unwrap();
        assert!(client.signers.read().await.is_empty());
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
        assert!(!event_id.to_hex().is_empty());
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
        assert!(!event_id.to_hex().is_empty());
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
        assert!(!event_id.to_hex().is_empty());
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
}
