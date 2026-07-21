#![allow(clippy::unwrap_used)]

use nostr::ToBech32;
use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
use pacto_bot_api::db::Db;
use pacto_bot_api::diagnostics::{DaemonStatus, Diagnostics};
use pacto_bot_api::nostr::NostrClient;
use secrecy::SecretString;

fn bot_config(id: &str, keys: &nostr::Keys, relays: Vec<String>) -> BotConfig {
    BotConfig {
        id: id.into(),
        display_name: Some(format!("{} Display", id)),
        npub: keys.public_key().to_bech32().unwrap(),
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(keys.secret_key().to_bech32().unwrap().into()),
        },
        relays,
        capabilities: vec!["ReadMessages".into()],
        mls_dedup_window_secs: None,
        ..Default::default()
    }
}

async fn manager_with_bots(bots: Vec<BotConfig>) -> ClientManager {
    let data_dir = tempfile::tempdir().unwrap();
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots,
    };
    let db = Db::open(&data_dir.path().join("agent.db")).await.unwrap();
    ClientManager::new(
        data_dir.path(),
        config,
        NostrClient::new(vec![]).await.unwrap(),
        &db,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn empty_manager_yields_empty_health_snapshots() {
    let manager = manager_with_bots(vec![]).await;
    let snapshots = manager.bot_health_snapshots();
    assert!(snapshots.is_empty());
}

#[tokio::test]
async fn bot_health_snapshots_are_sorted_by_bot_id() {
    let keys_a = nostr::Keys::generate();
    let keys_b = nostr::Keys::generate();
    let keys_c = nostr::Keys::generate();
    let manager = manager_with_bots(vec![
        bot_config("zebra", &keys_a, vec![]),
        bot_config("alpha", &keys_b, vec![]),
        bot_config("mike", &keys_c, vec![]),
    ])
    .await;

    let snapshots = manager.bot_health_snapshots();
    let ids: Vec<_> = snapshots.iter().map(|s| s.bot_id.as_str()).collect();
    assert_eq!(ids, vec!["alpha", "mike", "zebra"]);
}

#[tokio::test]
async fn bot_health_snapshot_reflects_bot_config() {
    let keys = nostr::Keys::generate();
    let npub = keys.public_key().to_bech32().unwrap();
    let manager = manager_with_bots(vec![BotConfig {
        id: "health-bot".into(),
        display_name: Some("health-bot Display".to_string()),
        npub: npub.clone(),
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(keys.secret_key().to_bech32().unwrap().into()),
        },
        relays: vec![
            "wss://relay.a.example".into(),
            "wss://relay.b.example".into(),
        ],
        capabilities: vec!["ReadMessages".into()],
        mls_dedup_window_secs: None,
        ..Default::default()
    }])
    .await;

    let snapshots = manager.bot_health_snapshots();
    assert_eq!(snapshots.len(), 1);
    let health = &snapshots[0];
    assert_eq!(health.bot_id, "health-bot");
    assert_eq!(health.npub, npub);
    assert_eq!(health.relay_count, 2);
    assert_eq!(
        health.relays,
        vec!["wss://relay.a.example", "wss://relay.b.example"]
    );
    assert_eq!(health.signer_backend, "nsec");
    assert!(!health.bunker_connected);
    assert!(health.error.is_none());
}

#[tokio::test]
async fn update_diagnostics_propagates_bot_health_to_snapshot() {
    let keys_a = nostr::Keys::generate();
    let keys_b = nostr::Keys::generate();
    let manager = manager_with_bots(vec![
        bot_config("bot-a", &keys_a, vec!["wss://a.example".into()]),
        bot_config("bot-b", &keys_b, vec!["wss://b.example".into()]),
    ])
    .await;

    let diagnostics = Diagnostics::new();
    manager.update_diagnostics(&diagnostics).await;

    let snapshot = diagnostics.snapshot().await;
    assert_eq!(snapshot.bots.len(), 2);
    let ids: Vec<_> = snapshot.bots.iter().map(|b| b.bot_id.as_str()).collect();
    assert_eq!(ids, vec!["bot-a", "bot-b"]);
    assert_eq!(snapshot.bots[0].relay_count, 1);
    assert_eq!(snapshot.bots[1].relay_count, 1);
    assert_eq!(snapshot.bots[0].relays, vec!["wss://a.example"]);
    assert_eq!(snapshot.bots[1].relays, vec!["wss://b.example"]);
}

#[tokio::test]
async fn diagnostics_update_replaces_previous_bots() {
    let keys_a = nostr::Keys::generate();
    let manager_a = manager_with_bots(vec![bot_config("bot-a", &keys_a, vec![])]).await;

    let diagnostics = Diagnostics::new();
    manager_a.update_diagnostics(&diagnostics).await;

    let keys_b = nostr::Keys::generate();
    let manager_b = manager_with_bots(vec![bot_config("bot-b", &keys_b, vec![])]).await;
    manager_b.update_diagnostics(&diagnostics).await;

    let snapshot = diagnostics.snapshot().await;
    assert_eq!(snapshot.bots.len(), 1);
    assert_eq!(snapshot.bots[0].bot_id, "bot-b");
}

#[tokio::test]
async fn diagnostics_update_preserves_other_snapshot_fields() {
    let keys = nostr::Keys::generate();
    let manager = manager_with_bots(vec![bot_config("field-bot", &keys, vec![])]).await;
    let diagnostics = Diagnostics::new();

    diagnostics.set_status(DaemonStatus::Ready).await;
    diagnostics.set_handlers_registered(3).await;
    diagnostics.record_rate_limited().await;

    manager.update_diagnostics(&diagnostics).await;

    let snapshot = diagnostics.snapshot().await;
    assert_eq!(snapshot.status, DaemonStatus::Ready);
    assert_eq!(snapshot.handlers_registered, 3);
    assert_eq!(snapshot.rate_limited_total, 1);
    assert_eq!(snapshot.bots.len(), 1);
    assert_eq!(snapshot.bots[0].bot_id, "field-bot");
}
