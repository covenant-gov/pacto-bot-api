mod common;
use nostr::ToBech32;
/// req(R31, R35, R37)
use pacto_bot_api::bot_state::BotState;
use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
use pacto_bot_api::diagnostics::{
    BotHealth, DaemonStatus, Diagnostics, ErrorRecord, HealthSnapshot,
};
use pacto_bot_api::nostr::NostrClient;
use pacto_bot_api::transport::protocol::MetricsResponse;
use secrecy::SecretString;
use serde_json::json;
use std::path::Path;

fn read_latest_report(data_dir: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let path = data_dir.join("reports").join("latest.json");
    Ok(std::fs::read_to_string(&path)?)
}

#[tokio::test]
async fn counter_increments() {
    let diag = Diagnostics::new();
    diag.record_event_received().await;
    diag.record_event_received().await;
    diag.record_event_dispatched().await;
    diag.record_rate_limited().await;
    diag.record_relay_reconnect().await;
    diag.record_bunker_sign_failure().await;
    diag.record_bunker_sign_failure().await;
    diag.record_invalid_event().await;
    diag.set_handlers_registered(3).await;

    let snap = diag.snapshot().await;
    assert_eq!(snap.events_received_total, 2);
    assert_eq!(snap.events_dispatched_total, 1);
    assert_eq!(snap.rate_limited_total, 1);
    assert_eq!(snap.relay_reconnects_total, 1);
    assert_eq!(snap.bunker_sign_failures_total, 2);
    assert_eq!(snap.invalid_events_total, 1);
    assert_eq!(snap.handlers_registered, 3);
}

#[tokio::test]
async fn status_transitions() {
    let diag = Diagnostics::new();
    assert_eq!(diag.snapshot().await.status, DaemonStatus::Initializing);

    diag.set_status(DaemonStatus::Ready).await;
    assert_eq!(diag.snapshot().await.status, DaemonStatus::Ready);

    diag.set_status(DaemonStatus::ShuttingDown).await;
    assert_eq!(diag.snapshot().await.status, DaemonStatus::ShuttingDown);

    diag.set_status(DaemonStatus::Stopped).await;
    assert_eq!(diag.snapshot().await.status, DaemonStatus::Stopped);
}

#[tokio::test]
async fn report_flushes_and_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = common::tempdir()?;
    let diag = Diagnostics::new();

    diag.set_status(DaemonStatus::Ready).await;
    diag.record_event_received().await;
    diag.record_event_dispatched().await;
    diag.set_bots(vec![
        BotHealth {
            bot_id: "bot-one".into(),
            npub: "npub1one".into(),
            relay_count: 2,
            relays: vec!["wss://r1.example".into(), "wss://r2.example".into()],
            bunker_connected: true,
            signer_backend: "bunker_local".into(),
            error: None,
        },
        BotHealth {
            bot_id: "bot-two".into(),
            npub: "npub1two".into(),
            relay_count: 0,
            relays: vec![],
            bunker_connected: false,
            signer_backend: "nsec".into(),
            error: None,
        },
    ])
    .await;

    diag.flush_report(tmp.path()).await?;

    let contents = read_latest_report(tmp.path())?;
    let parsed: HealthSnapshot = serde_json::from_str(&contents)?;

    assert_eq!(parsed.status, DaemonStatus::Ready);
    assert_eq!(parsed.events_received_total, 1);
    assert_eq!(parsed.events_dispatched_total, 1);
    assert_eq!(parsed.bots.len(), 2);
    assert_eq!(parsed.bots[0].bot_id, "bot-one");
    assert!(!parsed.bots[0].npub.is_empty());

    Ok(())
}

#[tokio::test]
async fn flushed_report_contains_no_secrets() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = common::tempdir()?;
    let diag = Diagnostics::new();

    diag.record_error(
        None,
        "bunker signer rejected nsec1deadbeefcafebabe01020304050607",
        None,
    )
    .await;
    diag.record_error(
        None,
        "token=super-secret-token and secret=do-not-share",
        None,
    )
    .await;
    diag.flush_report(tmp.path()).await?;

    let contents = read_latest_report(tmp.path())?;

    assert!(!contents.contains("nsec1deadbeefcafebabe01020304050607"));
    assert!(!contents.contains("super-secret-token"));
    assert!(!contents.contains("do-not-share"));
    assert!(contents.contains("[REDACTED]"));

    Ok(())
}

#[tokio::test]
async fn report_directory_is_created_lazily() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = common::tempdir()?;
    let nested = tmp.path().join("a").join("b");
    let diag = Diagnostics::new();

    diag.flush_report(&nested).await?;

    let contents = read_latest_report(&nested)?;
    assert!(contents.contains("\"status\""));
    Ok(())
}

#[tokio::test]
async fn metrics_response_matches_schema_fields() {
    let diag = Diagnostics::new();
    diag.record_event_received().await;
    diag.record_event_dispatched().await;
    diag.record_rate_limited().await;
    diag.record_relay_reconnect().await;
    diag.record_bunker_sign_failure().await;
    diag.record_bunker_sign_failure().await;
    diag.record_invalid_event().await;
    diag.set_handlers_registered(5).await;
    diag.set_bots(vec![BotHealth {
        bot_id: "bot-one".into(),
        npub: "npub1one".into(),
        relay_count: 2,
        relays: vec!["wss://r1.example".into(), "wss://r2.example".into()],
        bunker_connected: true,
        signer_backend: "bunker_local".into(),
        error: None,
    }])
    .await;

    let snap = diag.snapshot().await;
    let metrics = MetricsResponse::from(snap.clone());

    assert_eq!(metrics.uptime_seconds, Some(snap.uptime_seconds));
    assert_eq!(metrics.handlers_registered, Some(5));
    assert_eq!(metrics.events_received_total, Some(1));
    assert_eq!(metrics.events_dispatched_total, Some(1));
    assert_eq!(metrics.rate_limited_total, Some(1));
    assert_eq!(metrics.relay_reconnects_total, Some(1));
    assert_eq!(metrics.bunker_sign_failures_total, Some(2));
    assert_eq!(metrics.invalid_events_total, Some(1));
    assert_eq!(metrics.bots.as_ref().map(|v| v.len()), Some(1));

    let value = serde_json::to_value(&metrics).unwrap();
    let object = value.as_object().unwrap();
    assert!(object.contains_key("uptime_seconds"));
    assert!(object.contains_key("handlers_registered"));
    assert!(object.contains_key("events_received_total"));
    assert!(object.contains_key("events_dispatched_total"));
    assert!(object.contains_key("rate_limited_total"));
    assert!(object.contains_key("relay_reconnects_total"));
    assert!(object.contains_key("bunker_sign_failures_total"));
    assert!(object.contains_key("invalid_events_total"));
    assert!(object.contains_key("bots"));
    assert!(!object.contains_key("status"));
    assert!(!object.contains_key("errors"));
    assert!(!object.contains_key("startup_time"));
    assert!(!object.contains_key("reported_at"));
}

#[tokio::test]
async fn metrics_response_validates_against_schema() -> Result<(), Box<dyn std::error::Error>> {
    let diag = Diagnostics::new();
    diag.record_event_received().await;
    diag.set_handlers_registered(2).await;

    let metrics = MetricsResponse::from(diag.snapshot().await);
    let value = serde_json::to_value(&metrics)?;

    let schema: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string("schemas/metrics.json")?)?;
    let validator = jsonschema::validator_for(&schema)?;
    assert!(
        validator.validate(&value).is_ok(),
        "agent.metrics response must validate against schemas/metrics.json"
    );

    Ok(())
}

#[tokio::test]
async fn error_record_preserves_code_and_redacts_data() {
    let diag = Diagnostics::new();
    let data = json!({
        "context": "dm parsing",
        "secret": "nsec1do-not-leak-this",
    });

    diag.record_error(Some("E_DM_PARSE"), "handler error", Some(&data))
        .await;

    let snap = diag.snapshot().await;
    let record: &ErrorRecord = snap
        .errors
        .iter()
        .find(|e| e.code == "E_DM_PARSE")
        .expect("error record missing");

    assert_eq!(record.message, "handler error");
    let data_str = record.data.as_ref().expect("data preserved");
    assert!(data_str.contains("dm parsing"));
    assert!(!data_str.contains("nsec1do-not-leak-this"));
    assert!(data_str.contains("[REDACTED]"));
}

#[test]
fn bot_health_reflects_bot_state() {
    let keys = nostr::Keys::generate();
    let npub = keys.public_key().to_bech32().unwrap();
    let nsec = keys.secret_key().to_bech32().unwrap();
    let config = BotConfig {
        id: "snapshot-bot".into(),
        npub,
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(nsec.into()),
        },
        relays: vec!["wss://relay.example".into()],
        capabilities: vec![],
        ..Default::default()
    };

    let bot = BotState::new(config).unwrap();
    let health = bot.to_bot_health();

    assert_eq!(health.bot_id, "snapshot-bot");
    assert_eq!(health.relay_count, 1);
    assert_eq!(health.relays, vec!["wss://relay.example"]);
    assert_eq!(health.signer_backend, "nsec");
    assert!(!health.bunker_connected);
    assert!(health.error.is_none());
}

#[tokio::test]
async fn client_manager_populates_diagnostics_bots() {
    let keys = nostr::Keys::generate();
    let npub = keys.public_key().to_bech32().unwrap();
    let nsec = keys.secret_key().to_bech32().unwrap();
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![BotConfig {
            id: "diag-bot".into(),
            npub,
            signing: SigningConfig::Nsec {
                nsec: SecretString::new(nsec.into()),
            },
            relays: vec!["wss://relay.example".into()],
            capabilities: vec![],
            ..Default::default()
        }],
    };

    let data_dir = tempfile::tempdir().unwrap();
    let manager = ClientManager::new(
        data_dir.path(),
        config,
        NostrClient::new(vec![]).await.unwrap(),
    )
    .await
    .unwrap();

    let diag = Diagnostics::new();
    manager.update_diagnostics(&diag).await;

    let snap = diag.snapshot().await;
    assert_eq!(snap.bots.len(), 1);
    assert_eq!(snap.bots[0].bot_id, "diag-bot");
    assert_eq!(snap.bots[0].signer_backend, "nsec");
    assert_eq!(snap.bots[0].relay_count, 1);
}
