//! Mock MLS group peer for tests.
//!
//! Uses real `mdk-core` with ephemeral in-memory `MdkSqliteStorage` so the
//! daemon's MLS send path can be exercised against a real OpenMLS group.

#![allow(dead_code)]
#![allow(clippy::expect_used)]

use base64::Engine;
use mdk_core::prelude::*;
use mdk_sqlite_storage::MdkSqliteStorage;
use nostr::{Event, EventBuilder, Keys, Kind, RelayUrl, Tag, Timestamp, UnsignedEvent};
use pacto_bot_api::signer::LocalKeyCrypto;

/// A peer that can create an MLS group and invite a daemon bot.
pub struct MockMlsPeer {
    pub keys: Keys,
    engine: MDK<MdkSqliteStorage>,
}

impl MockMlsPeer {
    /// Create a new peer with an ephemeral in-memory MLS engine.
    pub fn new() -> Self {
        let keys = Keys::generate();
        let engine = MDK::new(MdkSqliteStorage::new_in_memory().expect("memory storage"));
        Self { keys, engine }
    }

    /// The peer's Nostr public key.
    pub fn public_key(&self) -> nostr::PublicKey {
        self.keys.public_key()
    }

    /// Create a key package for this peer.
    ///
    /// Returns the kind:443 event ready to publish.
    pub async fn create_key_package_event(&self, relays: Vec<String>) -> Event {
        self.create_key_package_event_at(relays, Timestamp::now())
            .await
    }

    /// Create a key package with a specific `created_at` timestamp.
    pub async fn create_key_package_event_at(
        &self,
        relays: Vec<String>,
        created_at: Timestamp,
    ) -> Event {
        let relay_urls: Vec<RelayUrl> = relays
            .into_iter()
            .filter_map(|r| RelayUrl::parse(&r).ok())
            .collect();
        let key_package_data = self
            .engine
            .create_key_package_for_event(&self.keys.public_key(), relay_urls)
            .expect("create key package");
        let (encoded, tags) = (key_package_data.content, key_package_data.tags_443);
        let unsigned = UnsignedEvent::new(
            self.keys.public_key(),
            created_at,
            Kind::MlsKeyPackage,
            tags,
            encoded,
        );
        unsigned.sign(&self.keys).await.expect("sign key package")
    }

    /// Create a key package event using the addressable kind:30443 format
    /// (`tags_30443`, which includes the mandatory `d` tag). Used to test
    /// that the daemon accepts the current MIP-00 addressable KeyPackage
    /// kind, not just the legacy kind:443.
    pub async fn create_key_package_event_kind_30443(&self, relays: Vec<String>) -> Event {
        let relay_urls: Vec<RelayUrl> = relays
            .into_iter()
            .filter_map(|r| RelayUrl::parse(&r).ok())
            .collect();
        let key_package_data = self
            .engine
            .create_key_package_for_event(&self.keys.public_key(), relay_urls)
            .expect("create key package");
        let unsigned = UnsignedEvent::new(
            self.keys.public_key(),
            Timestamp::now(),
            Kind::Custom(30443),
            key_package_data.tags_30443,
            key_package_data.content,
        );
        unsigned.sign(&self.keys).await.expect("sign key package")
    }

    /// Create a KeyPackage event whose content is hex-encoded and whose
    /// tags omit the required `encoding` tag -- the pre-MIP-00/MIP-02 wire
    /// format MDK 0.8.0 rejects as a peer-version mismatch rather than a
    /// generic parse failure. All other required tags are left intact.
    pub async fn create_key_package_event_missing_encoding_tag(
        &self,
        relays: Vec<String>,
    ) -> Event {
        let relay_urls: Vec<RelayUrl> = relays
            .into_iter()
            .filter_map(|r| RelayUrl::parse(&r).ok())
            .collect();
        let key_package_data = self
            .engine
            .create_key_package_for_event(&self.keys.public_key(), relay_urls)
            .expect("create key package");
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&key_package_data.content)
            .expect("decode base64 key package content");
        let hex_content = hex::encode(raw);
        let tags: Vec<Tag> = key_package_data
            .tags_443
            .into_iter()
            .filter(|t| t.as_slice().first().map(String::as_str) != Some("encoding"))
            .collect();
        let unsigned = UnsignedEvent::new(
            self.keys.public_key(),
            Timestamp::now(),
            Kind::MlsKeyPackage,
            tags,
            hex_content,
        );
        unsigned.sign(&self.keys).await.expect("sign key package")
    }

    /// Create a key package with arbitrary content and timestamp.
    ///
    /// The signature is valid for this peer's public key, but the content may
    /// be invalid MLS data. This is useful for testing daemon validation paths
    /// that inspect the event before feeding it to the MLS engine.
    pub async fn create_key_package_event_with_content(
        &self,
        relays: Vec<String>,
        content: String,
        created_at: Timestamp,
    ) -> Event {
        let tags = relays_to_tags(relays);
        let unsigned = UnsignedEvent::new(
            self.keys.public_key(),
            created_at,
            Kind::MlsKeyPackage,
            tags,
            content,
        );
        unsigned.sign(&self.keys).await.expect("sign key package")
    }

    /// Create a key package that is older than the configured freshness window.
    pub async fn create_stale_key_package_event(&self, relays: Vec<String>) -> Event {
        let created_at = Timestamp::now() - 3600;
        self.create_key_package_event_at(relays, created_at).await
    }

    /// Create a key package dated far in the future.
    pub async fn create_future_key_package_event(&self, relays: Vec<String>) -> Event {
        let created_at = Timestamp::now() + 86400;
        self.create_key_package_event_at(relays, created_at).await
    }

    /// Create a key package whose signature or author does not match the
    /// recipient. The daemon should treat it as absent.
    pub async fn create_forged_key_package_event(
        recipient: &nostr::PublicKey,
        relays: Vec<String>,
        content: String,
    ) -> Event {
        let forger = Keys::generate();
        let tags = relays_to_tags(relays);
        let unsigned = UnsignedEvent::new(
            *recipient,
            Timestamp::now(),
            Kind::MlsKeyPackage,
            tags,
            content,
        );
        unsigned
            .sign(&forger)
            .await
            .expect("sign forged key package")
    }

    /// Same as [`MockMlsPeer::create_forged_key_package_event`] but kind:30443,
    /// to verify the kind-guard widening didn't weaken the authorship check.
    pub async fn create_forged_key_package_event_kind_30443(
        recipient: &nostr::PublicKey,
        relays: Vec<String>,
        content: String,
    ) -> Event {
        let forger = Keys::generate();
        let tags = relays_to_tags(relays);
        let unsigned = UnsignedEvent::new(
            *recipient,
            Timestamp::now(),
            Kind::Custom(30443),
            tags,
            content,
        );
        unsigned
            .sign(&forger)
            .await
            .expect("sign forged key package")
    }

    /// Create a group containing the daemon bot as a member using its key package.
    ///
    /// Returns the group result and the unsigned welcome rumor for the daemon bot.
    pub fn create_group_with(
        &self,
        bot_key_package_event: &Event,
    ) -> (mdk_core::groups::GroupResult, nostr::UnsignedEvent) {
        let _kp = self
            .engine
            .parse_key_package(bot_key_package_event)
            .expect("parse daemon key package");

        let image_hash = random_bytes::<32>();
        let image_key = random_bytes::<32>();
        let image_nonce = random_bytes::<12>();
        let name = "Pacto Test Squad".to_owned();
        let description = "Test squad for pacto-bot-api".to_owned();
        let config = NostrGroupConfigData::new(
            name,
            description,
            Some(image_hash),
            Some(image_key),
            Some(image_nonce),
            vec![RelayUrl::parse("wss://test.relay").expect("valid relay url")],
            vec![self.keys.public_key()],
        );

        let result = self
            .engine
            .create_group(
                &self.keys.public_key(),
                vec![bot_key_package_event.clone()],
                config,
            )
            .expect("create group");

        let welcome_rumor = result
            .welcome_rumors
            .first()
            .expect("at least one welcome rumor")
            .clone();
        (result, welcome_rumor)
    }

    /// Add a member to an existing group created by this peer.
    ///
    /// Returns the signed group evolution event and the unsigned welcome rumor
    /// for the new member.
    pub fn add_member_to_group(
        &self,
        group_result: &mdk_core::groups::GroupResult,
        new_member_key_package_event: &Event,
    ) -> (nostr::Event, nostr::UnsignedEvent) {
        let _kp = self
            .engine
            .parse_key_package(new_member_key_package_event)
            .expect("parse new member key package");

        let update = self
            .engine
            .add_members(
                &group_result.group.mls_group_id,
                std::slice::from_ref(new_member_key_package_event),
            )
            .expect("add member");
        self.engine
            .merge_pending_commit(&group_result.group.mls_group_id)
            .expect("merge pending commit");

        let welcome_rumor = update
            .welcome_rumors
            .and_then(|v| v.into_iter().next())
            .expect("welcome rumor for new member");
        (update.evolution_event, welcome_rumor)
    }

    /// Create and sign a kind:445 MLS group message in the first known group.
    ///
    /// The returned event is signed by an ephemeral key derived from the group
    /// exporter secret, matching the production NIP-104 format.
    ///
    /// The inner rumor is kind 14 (`PrivateDirectMessage`), matching the
    /// group text rumor kind pacto-app and pacto-bot-api both send.
    pub async fn create_group_message(&self, content: &str) -> Event {
        let rumor = nostr::UnsignedEvent::new(
            self.keys.public_key(),
            nostr::Timestamp::now(),
            nostr::Kind::PrivateDirectMessage,
            Vec::new(),
            content,
        );
        self.create_group_rumor(rumor).await
    }

    /// Create a kind:445 wrapper carrying an arbitrary inner rumor.
    pub async fn create_group_rumor(&self, rumor: nostr::UnsignedEvent) -> Event {
        let groups = self.engine.get_groups().expect("get groups");
        let group = groups.first().expect("group exists");
        self.engine
            .create_message(&group.mls_group_id, rumor, None)
            .expect("create group message")
    }

    /// Process a kind:445 MLS group message from the daemon bot.
    ///
    /// Returns the decrypted inner rumor event.
    pub fn process_group_message(&self, message_event: &Event) -> Event {
        self.engine
            .process_message(message_event)
            .expect("process message");
        let groups = self.engine.get_groups().expect("get groups");
        let group = groups.first().expect("group exists");
        let messages = self
            .engine
            .get_messages(&group.mls_group_id, None)
            .expect("get messages");
        let msg = messages.first().expect("at least one message");
        pacto_bot_api::nostr_json::sign_unsigned(
            EventBuilder::new(msg.kind, msg.content.clone()).build(self.keys.public_key()),
            &self.keys,
        )
        .expect("sign message")
    }

    /// Sign an unsigned event with the peer's keys.
    pub async fn sign(&self, unsigned: nostr::UnsignedEvent) -> Event {
        unsigned.sign(&self.keys).await.expect("sign event")
    }
}

impl Default for MockMlsPeer {
    fn default() -> Self {
        Self::new()
    }
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf).expect("getrandom");
    buf
}

fn relays_to_tags(relays: Vec<String>) -> Vec<Tag> {
    relays
        .into_iter()
        .filter_map(|r| RelayUrl::parse(&r).ok())
        .map(|url| Tag::reference(url.to_string()))
        .collect()
}

/// Gift-wrap a welcome rumor addressed to a recipient.
///
/// Returns a kind:1059 gift-wrap event containing the welcome rumor.
pub async fn gift_wrap_welcome(
    sender_keys: &Keys,
    recipient: &nostr::PublicKey,
    welcome_rumor: nostr::UnsignedEvent,
) -> Event {
    let rumor_event = welcome_rumor
        .sign(sender_keys)
        .await
        .expect("sign welcome rumor");

    let seal_content = sender_keys
        .nip44_encrypt(
            recipient,
            &pacto_bot_api::nostr_json::event_to_json(&rumor_event),
        )
        .await
        .expect("encrypt seal");
    let seal = nostr::UnsignedEvent::new(
        sender_keys.public_key(),
        nostr::Timestamp::now(),
        Kind::Seal,
        Vec::new(),
        seal_content,
    )
    .sign(sender_keys)
    .await
    .expect("sign seal");

    let ephemeral = Keys::generate();
    let gift_content = nostr::nips::nip44::encrypt(
        ephemeral.secret_key(),
        recipient,
        pacto_bot_api::nostr_json::event_to_json(&seal),
        nostr::nips::nip44::Version::default(),
    )
    .expect("encrypt gift wrap");

    let gift = nostr::UnsignedEvent::new(
        ephemeral.public_key(),
        nostr::Timestamp::now(),
        Kind::GiftWrap,
        [nostr::Tag::public_key(*recipient)],
        gift_content,
    );
    pacto_bot_api::nostr_json::sign_unsigned(gift, &ephemeral).expect("sign gift wrap")
}

/// Extract the group wire id (h tag) from a kind:445 MLS group message wrapper.
pub fn group_wire_id(message_event: &Event) -> Option<String> {
    pacto_bot_api::nostr_tags::h_tag_content(&message_event.tags)
}
