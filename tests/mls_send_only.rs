//! req(R19, R20)
mod common;
mod support;

use support::mock_mls_peer::{MockMlsPeer, group_wire_id};

use std::path::PathBuf;
use std::sync::Arc;

use nostr::ToBech32;
use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
use pacto_bot_api::db::Db;
use pacto_bot_api::diagnostics::Diagnostics;
use pacto_bot_api::dispatch::Dispatch;
use pacto_bot_api::events::EventType;
use pacto_bot_api::nostr::NostrClient;
use secrecy::SecretString;
use tokio::sync::RwLock;

fn bot_config(id: &str, keys: &nostr::Keys) -> BotConfig {
    BotConfig {
        id: id.to_string(),
        npub: keys.public_key().to_bech32().unwrap(),
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(keys.secret_key().to_bech32().unwrap().into()),
        },
        relays: vec![],
        capabilities: vec!["SendGroupMessages".into()],
        mls_dedup_window_secs: None,
        mls_db_path: Some(PathBuf::from("vector-mls.db")),
        mls_key_package_freshness_secs: None,
        ..Default::default()
    }
}

#[tokio::test]
async fn daemon_bot_joins_mls_group_and_sends_message() {
    let dir = common::tempdir().expect("tempdir");
    let bot_keys = nostr::Keys::generate();
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot_config("mls-bot", &bot_keys)],
    };
    let nostr_client = NostrClient::new(vec![]).await.expect("nostr client");
    let db = Db::open(dir.path().join("test.db").as_path())
        .await
        .expect("db");
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config, nostr_client, &db)
            .await
            .expect("client manager"),
    ));
    let dispatch = Arc::new(Dispatch::new(cm.clone(), db, Diagnostics::new()));

    // Peer: create key package and group with daemon bot as member.
    let peer = MockMlsPeer::new();
    let daemon_pubkey = bot_keys.public_key();
    let bot_key_package = {
        let cm = cm.read().await;
        let bot = cm.get_bot_by_id("mls-bot").expect("bot exists");
        let mls = bot.mls.as_ref().expect("mls enabled");
        mls.publish_key_package(&daemon_pubkey, vec![])
            .await
            .expect("publish key package")
    };
    let bot_key_package_event = {
        let unsigned = nostr::UnsignedEvent::new(
            daemon_pubkey,
            nostr::Timestamp::now(),
            nostr::Kind::MlsKeyPackage,
            bot_key_package.1,
            bot_key_package.0,
        );
        unsigned
            .sign(&bot_keys)
            .await
            .expect("sign bot key package")
    };

    let (_group_result, welcome_rumor) = peer.create_group_with(&bot_key_package_event);
    let signed_welcome = peer.sign(welcome_rumor).await;
    let wrapper_event_id = nostr::EventId::all_zeros();

    // Daemon: process the welcome and accept it.
    let group_info = {
        let cm = cm.read().await;
        let bot = cm.get_bot_by_id("mls-bot").expect("bot exists");
        let mls = bot.mls.as_ref().expect("mls enabled");
        mls.process_welcome(wrapper_event_id, signed_welcome)
            .await
            .expect("process welcome");
        mls.accept_pending_welcome()
            .await
            .expect("accept pending welcome")
    };

    // Daemon bot creates and sends an encrypted group message.
    let message_event = {
        let cm = cm.read().await;
        let bot = cm.get_bot_by_id("mls-bot").expect("bot exists");
        let mls = bot.mls.as_ref().expect("mls enabled");
        mls.create_group_message(
            group_info.mls_group_id,
            nostr::UnsignedEvent::new(
                daemon_pubkey,
                nostr::Timestamp::now(),
                nostr::Kind::Custom(9),
                Vec::new(),
                "hello squad",
            ),
        )
        .await
        .expect("create group message")
    };

    // Peer processes the group message and verifies the decrypted content.
    let decrypted = peer.process_group_message(&message_event);
    assert_eq!(decrypted.kind, nostr::Kind::Custom(9));
    assert_eq!(decrypted.content, "hello squad");

    // The wrapper event is a kind:445 MLS group message with an h tag.
    assert_eq!(message_event.kind, nostr::Kind::MlsGroupMessage);
    let wire_id = group_wire_id(&message_event).expect("h tag");
    assert_eq!(wire_id, hex::encode(&group_info.nostr_group_id));

    // No handler was registered, so no dm_received event should be dispatched.
    let event = pacto_bot_api::events::AgentEvent {
        bot_id: "mls-bot".into(),
        event_id: wrapper_event_id.to_hex(),
        event_type: EventType::MlsWelcomeReceived,
        chat_id: None,
        content: "welcome".into(),
        rumor_id: wrapper_event_id.to_hex(),
        author: peer.public_key().to_hex(),
        timestamp: nostr::Timestamp::now().as_u64(),
    };
    dispatch
        .dispatch_event(event)
        .await
        .expect("dispatch welcome event");
}
