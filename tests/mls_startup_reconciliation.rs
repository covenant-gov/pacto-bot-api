//! Regression test for the daemon startup MLS engine reconciliation pass.
//!
//! This covers the crash window where the MLS engine mutation and welcome
//! publish succeeded, but the `agent.db` insert did not commit. On restart,
//! the daemon scans the engine storage and restores the missing `mls_groups`
//! and `mls_group_members` rows using the Squad wire id as the stable key.
//! req(pacto-bot-api-p0pd)
mod common;
mod support;

use std::path::PathBuf;
use std::sync::Arc;

use nostr::ToBech32;
use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
use pacto_bot_api::db::{Db, MlsGroupRow};
use pacto_bot_api::nostr::NostrClient;
use rusqlite::Connection;
use secrecy::SecretString;
use support::mock_mls_peer::MockMlsPeer;
use tokio::sync::RwLock;

fn bot_config(id: &str, keys: &nostr::Keys) -> BotConfig {
    BotConfig {
        id: id.to_string(),
        npub: keys.public_key().to_bech32().unwrap(),
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(keys.secret_key().to_bech32().unwrap().into()),
        },
        relays: vec!["wss://relay.example".into()],
        capabilities: vec!["CreateMlsGroup".into()],
        mls_dedup_window_secs: None,
        mls_db_path: Some(PathBuf::from("vector-mls.db")),
        mls_key_package_freshness_secs: None,
        ..Default::default()
    }
}

#[tokio::test]
async fn startup_reconciliation_restores_missing_mls_group_rows() {
    let dir = common::tempdir().expect("tempdir");
    let bot_keys = nostr::Keys::generate();
    let creator_npub = bot_keys.public_key().to_bech32().unwrap();
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot_config("mls-bot", &bot_keys)],
    };

    // First daemon boot: open agent.db and create the ClientManager.
    let db = Db::open(dir.path().join("agent.db").as_path())
        .await
        .expect("open agent.db");
    let nostr_client = NostrClient::new(vec![]).await.expect("nostr client");
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config.clone(), nostr_client, &db)
            .await
            .expect("client manager initializes"),
    ));

    // Create a peer and invite it into a new group via the daemon's MLS engine.
    let peer = MockMlsPeer::new();
    let peer_pubkey = peer.public_key();
    let peer_npub = peer_pubkey.to_bech32().unwrap();
    let key_package = peer.create_key_package_event(vec![]).await;

    let (wire_id, _welcome) = {
        let cm_guard = cm.read().await;
        let bot = cm_guard
            .get_bot_by_id("mls-bot")
            .expect("bot exists in first manager");
        let mls = bot.mls.as_ref().expect("mls enabled");
        mls.create_group(
            bot_keys.public_key(),
            peer_pubkey,
            key_package,
            "reconciled-group".into(),
            vec![nostr::RelayUrl::parse("wss://relay.example").unwrap()],
        )
        .await
        .expect("create group via engine")
    };

    // Simulate the normal path: the row would have been persisted in agent.db.
    let row = MlsGroupRow {
        bot_id: "mls-bot".into(),
        group_name: "reconciled-group".into(),
        wire_id: wire_id.clone(),
        creator_npub: creator_npub.clone(),
        relay: "wss://relay.example".into(),
        invited_bots: vec![peer_npub.clone()],
    };
    db.insert_mls_group(row).await.expect("insert group row");

    // Verify the row is present before simulating the crash.
    let before = db
        .load_mls_group("mls-bot", "reconciled-group")
        .await
        .expect("load group before crash")
        .expect("group exists before crash");
    assert_eq!(before.wire_id, wire_id);
    assert!(before.invited_bots.contains(&peer_npub));

    // Simulate the crash window: delete the group rows from agent.db while the
    // MLS engine storage retains the group. Use a separate raw connection so
    // we do not rely on internal Db helpers.
    let db_path = dir.path().join("agent.db");
    tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&db_path).expect("open raw connection");
        conn.execute("DELETE FROM mls_groups WHERE bot_id = ?1", ["mls-bot"])
            .expect("delete mls_groups");
        conn.execute(
            "DELETE FROM mls_group_members WHERE bot_id = ?1",
            ["mls-bot"],
        )
        .expect("delete mls_group_members");
    })
    .await
    .expect("delete rows");

    assert!(
        db.load_mls_group("mls-bot", "reconciled-group")
            .await
            .expect("load group after deletion")
            .is_none(),
        "group row should be gone after simulated crash"
    );

    // Reconstruct the ClientManager as the daemon would on restart. Drop the
    // first manager so the MLS engine handle is released before reopening it.
    drop(cm);
    let nostr_client = NostrClient::new(vec![])
        .await
        .expect("nostr client on restart");
    let _cm = ClientManager::new(dir.path(), config, nostr_client, &db)
        .await
        .expect("client manager restarts");

    let restored = db
        .load_mls_group("mls-bot", "reconciled-group")
        .await
        .expect("load group after reconciliation")
        .expect("group row restored by reconciliation");
    assert_eq!(restored.wire_id, wire_id);
    assert_eq!(restored.group_name, "reconciled-group");
    // The engine reports all members, so the restored member set contains the
    // invited peer. The daemon's own bot is also a member of the group from
    // the engine's perspective and may be present in the restored members.
    assert!(
        restored.invited_bots.contains(&peer_npub),
        "restored members should include the invited peer"
    );
}
