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
use pacto_bot_api::signer::Signer;
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

#[tokio::test]
async fn startup_reconciliation_across_bots_with_shared_squad() {
    let dir = common::tempdir().expect("tempdir");

    // Two bots that are both members of the same Squad.
    let bot_a_keys = nostr::Keys::generate();
    let bot_b_keys = nostr::Keys::generate();
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![
            bot_config("bot-a", &bot_a_keys),
            bot_config("bot-b", &bot_b_keys),
        ],
    };

    let db = Db::open(dir.path().join("agent.db").as_path())
        .await
        .expect("open agent.db");

    let peer = MockMlsPeer::new();
    let peer_pubkey = peer.public_key();
    let wrapper_event_id = nostr::EventId::all_zeros();
    let bot_a_keys_clone = bot_a_keys.clone();

    // Bot-a is the first member: the peer creates the group with it.
    let (peer_result, wire_id) = {
        let cm = Arc::new(RwLock::new(
            ClientManager::new(
                dir.path(),
                config.clone(),
                NostrClient::new(vec![]).await.unwrap(),
                &db,
            )
            .await
            .expect("client manager initializes"),
        ));
        let cm_guard = cm.read().await;
        let bot_a = cm_guard.get_bot_by_id("bot-a").expect("bot-a exists");
        let mls_a = bot_a.mls.as_ref().expect("mls enabled for bot-a");
        let daemon_pubkey = bot_a.signer.public_key();

        let bot_a_key_package = mls_a
            .publish_key_package(&daemon_pubkey, vec![])
            .await
            .expect("publish key package for bot-a");
        let bot_a_key_package_event = {
            let unsigned = nostr::UnsignedEvent::new(
                daemon_pubkey,
                nostr::Timestamp::now(),
                nostr::Kind::MlsKeyPackage,
                bot_a_key_package.1,
                bot_a_key_package.0,
            );
            unsigned.sign(&bot_a_keys_clone).await.unwrap()
        };

        let (result, welcome_rumor) = peer.create_group_with(&bot_a_key_package_event);
        let signed_welcome = peer.sign(welcome_rumor).await;
        mls_a
            .process_welcome(wrapper_event_id, signed_welcome)
            .await
            .expect("bot-a processes welcome");
        mls_a
            .accept_pending_welcome()
            .await
            .expect("bot-a accepts pending welcome");
        let wire_id = hex::encode(result.group.nostr_group_id.as_slice());
        (result, wire_id)
    };

    // Bot-b joins the same Squad by having the peer add it to bot-a's group.
    // Bot-b joins the same Squad by having the peer add it to bot-a's group.
    {
        let cm = Arc::new(RwLock::new(
            ClientManager::new(
                dir.path(),
                config.clone(),
                NostrClient::new(vec![]).await.unwrap(),
                &db,
            )
            .await
            .expect("client manager initializes for bot-b"),
        ));
        let cm_guard = cm.read().await;
        let bot_b = cm_guard.get_bot_by_id("bot-b").expect("bot-b exists");
        let mls_b = bot_b.mls.as_ref().expect("mls enabled for bot-b");
        let daemon_pubkey = bot_b.signer.public_key();

        let bot_b_key_package = mls_b
            .publish_key_package(&daemon_pubkey, vec![])
            .await
            .expect("publish key package for bot-b");
        let bot_b_key_package_event = {
            let unsigned = nostr::UnsignedEvent::new(
                daemon_pubkey,
                nostr::Timestamp::now(),
                nostr::Kind::MlsKeyPackage,
                bot_b_key_package.1,
                bot_b_key_package.0,
            );
            unsigned.sign(&bot_b_keys).await.unwrap()
        };

        let (_evolution, welcome_rumor) =
            peer.add_member_to_group(&peer_result, &bot_b_key_package_event);
        let signed_welcome = peer.sign(welcome_rumor).await;
        mls_b
            .process_welcome(wrapper_event_id, signed_welcome.clone())
            .await
            .expect("bot-b processes welcome");
        mls_b
            .accept_pending_welcome()
            .await
            .expect("bot-b accepts pending welcome");
        signed_welcome
    };

    // Simulate the crash window: drop all mls_groups rows while keeping the
    // engine state (the vector-mls.db files are already on disk).
    let db_path = dir.path().join("agent.db");
    tokio::task::spawn_blocking(move || {
        let conn = Connection::open(&db_path).expect("open raw connection");
        conn.execute("DELETE FROM mls_groups", [])
            .expect("delete mls_groups");
        conn.execute("DELETE FROM mls_group_members", [])
            .expect("delete mls_group_members");
    })
    .await
    .expect("delete rows");

    assert!(
        db.load_all_mls_groups("bot-a")
            .await
            .expect("load bot-a groups")
            .is_empty(),
        "bot-a group rows should be gone"
    );
    assert!(
        db.load_all_mls_groups("bot-b")
            .await
            .expect("load bot-b groups")
            .is_empty(),
        "bot-b group rows should be gone"
    );

    // Restart the ClientManager. This is where the original global UNIQUE
    // constraint on wire_id would fail when the second bot is reconciled.
    let _cm = ClientManager::new(
        dir.path(),
        config,
        NostrClient::new(vec![]).await.unwrap(),
        &db,
    )
    .await
    .expect("client manager restarts with shared squad across bots");

    let groups_a = db
        .load_all_mls_groups("bot-a")
        .await
        .expect("load bot-a groups after reconciliation");
    let groups_b = db
        .load_all_mls_groups("bot-b")
        .await
        .expect("load bot-b groups after reconciliation");

    assert_eq!(
        groups_a.len(),
        1,
        "bot-a should have its reconciled group row"
    );
    assert_eq!(
        groups_b.len(),
        1,
        "bot-b should have its reconciled group row"
    );
    assert_eq!(groups_a[0].wire_id, wire_id, "bot-a wire id mismatch");
    assert_eq!(groups_b[0].wire_id, wire_id, "bot-b wire id mismatch");
    assert!(
        groups_a[0]
            .invited_bots
            .contains(&peer_pubkey.to_bech32().unwrap()),
        "bot-a restored members should include the peer creator"
    );
    assert!(
        groups_b[0]
            .invited_bots
            .contains(&peer_pubkey.to_bech32().unwrap()),
        "bot-b restored members should include the peer creator"
    );
}
