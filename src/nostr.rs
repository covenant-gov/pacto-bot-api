//! Nostr relay client wrapper.
//!
//! Provides a thin, bot-aware layer over [`nostr_sdk::Client`] for sending and
//! receiving NIP-17 / NIP-59 direct messages (gift wraps, kind 1059).

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use nostr::event::tag::Tag;
use nostr::nips::nip44::Version;
use nostr::nips::{nip44, nip59};
use nostr::secp256k1::schnorr::Signature;
use nostr::{
    Event, EventBuilder, EventId, Filter, JsonUtil, Keys, Kind, PublicKey, SubscriptionId,
    UnsignedEvent,
};
use nostr_sdk::{Client, RelayPoolNotification};
use serde_json::json;
use tokio::sync::RwLock;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_stream::Stream;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::errors::DaemonError;
use crate::events::{AgentEvent, EventType};
use crate::signer::Signer;

/// Bot signer storage: maps recipient public key to bot id and signer.
type BotSigners = HashMap<PublicKey, (String, Arc<dyn Signer>)>;

/// Wrapper around [`nostr_sdk::Client`] providing Pacto-specific relay operations.
#[derive(Clone)]
pub struct NostrClient {
    client: Client,
    signers: Arc<RwLock<BotSigners>>,
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
        };
        this.add_relays(&relays).await?;
        this.client.connect().await;
        Ok(this)
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

    /// Subscribe to kind 1059 gift wraps addressed to `npub`.
    pub async fn subscribe_bot(&self, npub: &PublicKey) -> Result<SubscriptionId, DaemonError> {
        let filter = Filter::new().kind(Kind::GiftWrap).pubkey(*npub);
        let output = self
            .client
            .subscribe(filter, None)
            .await
            .map_err(|e| DaemonError::Nostr(format!("subscribe failed: {e}")))?;
        Ok(output.val)
    }

    /// Unsubscribe a previously created bot subscription.
    pub async fn unsubscribe_bot(&self, sub_id: &SubscriptionId) -> Result<(), DaemonError> {
        self.client.unsubscribe(sub_id).await;
        Ok(())
    }

    /// Disconnect from all relays.
    pub async fn disconnect(&self) {
        self.client.disconnect().await;
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
        let recipient = PublicKey::parse(recipient_npub)
            .map_err(|e| DaemonError::Nostr(format!("invalid recipient npub: {e}")))?;

        let mut rumor_builder = EventBuilder::private_msg_rumor(recipient, content);
        if let Some(reply_id) = reply_to {
            let event_id = EventId::parse(reply_id)
                .map_err(|e| DaemonError::Nostr(format!("invalid reply_to event id: {e}")))?;
            rumor_builder = rumor_builder.tags([Tag::event(event_id)]);
        }
        let rumor = rumor_builder.build(signer.public_key());
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

        let output = self
            .client
            .send_event(&gift_event)
            .await
            .map_err(|e| DaemonError::Nostr(format!("failed to publish event: {e}")))?;

        Ok(*output.id())
    }

    /// Decrypt a single incoming gift-wrap event using the registered bot signer.
    pub async fn decrypt_event(&self, event: &Event) -> Result<AgentEvent, DaemonError> {
        let snapshot = self.signers.read().await.clone();
        Self::process_gift_wrap(&snapshot, event).await
    }

    /// Return an async stream of incoming DMs converted to [`AgentEvent`].
    pub fn receive_events(&self) -> impl Stream<Item = Result<AgentEvent, DaemonError>> {
        let (tx, rx) = unbounded_channel();
        let client = self.client.clone();
        let signers = Arc::clone(&self.signers);

        tokio::spawn(async move {
            let _ = client
                .handle_notifications(|notification| {
                    let tx: UnboundedSender<Result<AgentEvent, DaemonError>> = tx.clone();
                    let signers = Arc::clone(&signers);
                    async move {
                        match notification {
                            RelayPoolNotification::Event { event, .. } => {
                                if event.kind == Kind::GiftWrap {
                                    let snapshot = signers.read().await.clone();
                                    let result = Self::process_gift_wrap(&snapshot, &event).await;
                                    let _ = tx.send(result);
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
    ) -> Result<AgentEvent, DaemonError> {
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

        Ok(AgentEvent {
            bot_id: bot_id.clone(),
            event_id: event.id.to_hex(),
            event_type: EventType::DmReceived,
            chat_id: None,
            content: rumor.content,
            rumor_id,
            author: seal_event.pubkey.to_hex(),
            timestamp: rumor.created_at.as_u64(),
        })
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
        let agent_event = NostrClient::process_gift_wrap(&signers, &event)
            .await
            .unwrap();
        assert_eq!(agent_event.bot_id, "bot-1");
        assert_eq!(agent_event.event_type, EventType::DmReceived);
        assert_eq!(agent_event.content, "secret message");
        assert_eq!(agent_event.author, sender_keys.public_key().to_hex());
    }

    #[tokio::test]
    async fn missing_signer_returns_error() {
        let client = NostrClient::new(vec![]).await.unwrap();
        let bot_keys = nostr::Keys::generate();
        let sender_keys = nostr::Keys::generate();
        let event = EventBuilder::private_msg(
            &sender_keys,
            bot_keys.public_key(),
            "secret message",
            Vec::<Tag>::new(),
        )
        .await
        .unwrap();

        let signers = client.signers.read().await.clone();
        let err = NostrClient::process_gift_wrap(&signers, &event)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no signer registered"));
    }
}
