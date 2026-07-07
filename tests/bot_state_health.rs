use nostr::ToBech32;
use pacto_bot_api::bot_state::BotState;
use pacto_bot_api::config::{BotConfig, SigningConfig};
use secrecy::SecretString;

fn generate_nsec_config(id: &str) -> BotConfig {
    let keys = nostr::Keys::generate();
    let nsec = keys.secret_key().to_bech32().unwrap();
    let npub = keys.public_key().to_bech32().unwrap();
    BotConfig {
        id: id.into(),
        npub,
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(nsec.into()),
        },
        relays: vec![],
        capabilities: vec![],
        ..Default::default()
    }
}

#[test]
fn to_bot_health_reports_local_key_fields() {
    let mut config = generate_nsec_config("local-bot");
    config.relays = vec!["wss://relay.example.com".to_string()];

    let state = BotState::new(config).expect("valid local config");
    let health = state.to_bot_health();

    assert_eq!(health.bot_id, "local-bot");
    assert!(!health.npub.is_empty());
    assert_eq!(health.relay_count, 1);
    assert_eq!(health.relays, vec!["wss://relay.example.com"]);
    assert!(!health.bunker_connected);
    assert_eq!(health.signer_backend, "nsec");
    assert!(health.error.is_none());
}

#[test]
fn to_bot_health_reports_multiple_relays() {
    let mut config = generate_nsec_config("relay-bot");
    config.relays = vec![
        "wss://relay1.example.com".to_string(),
        "wss://relay2.example.com".to_string(),
    ];

    let state = BotState::new(config).expect("valid local config");
    let health = state.to_bot_health();

    assert_eq!(health.relay_count, 2);
    assert_eq!(health.relays.len(), 2);
    assert_eq!(health.relays[0], "wss://relay1.example.com");
    assert_eq!(health.relays[1], "wss://relay2.example.com");
}

#[test]
fn to_bot_health_reports_empty_relays() {
    let config = generate_nsec_config("no-relay-bot");
    let state = BotState::new(config).expect("valid local config");
    let health = state.to_bot_health();

    assert_eq!(health.relay_count, 0);
    assert!(health.relays.is_empty());
}

#[test]
fn to_bot_health_reports_bunker_connected() {
    let keys = nostr::Keys::generate();
    let npub = keys.public_key().to_bech32().unwrap();
    let uri = format!(
        "bunker://{}?relay=ws://127.0.0.1:4848",
        keys.public_key().to_hex()
    );
    let config = BotConfig {
        id: "bunker-bot".into(),
        npub,
        signing: SigningConfig::BunkerLocal {
            uri: SecretString::new(uri.into()),
        },
        relays: vec![],
        capabilities: vec![],
        ..Default::default()
    };

    let state = BotState::new(config).expect("valid bunker config");
    let health = state.to_bot_health();

    assert_eq!(health.bot_id, "bunker-bot");
    assert!(health.bunker_connected);
    assert_eq!(health.signer_backend, "bunker_local");
    assert!(health.error.is_none());
}

#[test]
fn new_rejects_empty_nsec() {
    let keys = nostr::Keys::generate();
    let npub = keys.public_key().to_bech32().unwrap();
    let config = BotConfig {
        id: "empty-nsec-bot".into(),
        npub,
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(String::new().into()),
        },
        relays: vec![],
        capabilities: vec![],
        ..Default::default()
    };

    let err = BotState::new(config).expect_err("empty nsec should fail");
    assert!(err.to_string().contains("non-empty"));
}

#[test]
fn new_rejects_nsec_public_key_mismatch() {
    let keys = nostr::Keys::generate();
    let other_keys = nostr::Keys::generate();
    let nsec = keys.secret_key().to_bech32().unwrap();
    let other_npub = other_keys.public_key().to_bech32().unwrap();

    let config = BotConfig {
        id: "mismatch-bot".into(),
        npub: other_npub,
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(nsec.into()),
        },
        relays: vec![],
        capabilities: vec![],
        ..Default::default()
    };

    let err = BotState::new(config).expect_err("mismatched nsec should fail");
    assert!(err.to_string().contains("does not match"));
}

#[test]
fn new_rejects_invalid_npub() {
    let config = BotConfig {
        id: "bad-npub-bot".into(),
        npub: "not-a-valid-npub".into(),
        signing: SigningConfig::Nsec {
            nsec: SecretString::new("nsec1dummy".into()),
        },
        relays: vec![],
        capabilities: vec![],
        ..Default::default()
    };

    let err = BotState::new(config).expect_err("invalid npub should fail");
    assert!(err.to_string().contains("invalid npub"));
}

#[test]
fn new_rejects_bunker_uri_missing_scheme() {
    let keys = nostr::Keys::generate();
    let npub = keys.public_key().to_bech32().unwrap();
    let config = BotConfig {
        id: "bad-bunker-bot".into(),
        npub,
        signing: SigningConfig::BunkerLocal {
            uri: SecretString::new("not-a-bunker-uri".into()),
        },
        relays: vec![],
        capabilities: vec![],
        ..Default::default()
    };

    let err = BotState::new(config).expect_err("missing scheme should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("invalid bunker URI") || msg.contains("not a bunker URI"),
        "unexpected error: {msg}"
    );
}

#[test]
fn new_rejects_bunker_uri_missing_remote_signer_pubkey() {
    let keys = nostr::Keys::generate();
    let npub = keys.public_key().to_bech32().unwrap();
    let config = BotConfig {
        id: "missing-pubkey-bot".into(),
        npub,
        signing: SigningConfig::BunkerLocal {
            uri: SecretString::new("bunker://?relay=ws://127.0.0.1:4848".into()),
        },
        relays: vec![],
        capabilities: vec![],
        ..Default::default()
    };

    let err = BotState::new(config).expect_err("missing pubkey should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("invalid bunker URI") || msg.contains("missing remote signer pubkey"),
        "unexpected error: {msg}"
    );
}

#[test]
fn new_rejects_bunker_remote_with_ws_relay() {
    let keys = nostr::Keys::generate();
    let npub = keys.public_key().to_bech32().unwrap();
    let uri = format!(
        "bunker://{}?relay=ws://relay.example.com",
        keys.public_key().to_hex()
    );
    let config = BotConfig {
        id: "remote-ws-bot".into(),
        npub,
        signing: SigningConfig::BunkerRemote {
            uri: SecretString::new(uri.into()),
        },
        relays: vec![],
        capabilities: vec![],
        ..Default::default()
    };

    let err = BotState::new(config).expect_err("ws remote should fail");
    assert!(err.to_string().contains("wss://"));
}
