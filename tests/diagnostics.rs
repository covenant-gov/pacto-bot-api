use pacto_bot_api::diagnostics::{BotHealth, DaemonStatus, Diagnostics, HealthSnapshot};
use std::path::Path;

fn read_latest_report(data_dir: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let path = data_dir.join("reports").join("latest.json");
    Ok(std::fs::read_to_string(&path)?)
}

#[test]
fn counter_increments() {
    let diag = Diagnostics::new();
    diag.record_event_received();
    diag.record_event_received();
    diag.record_event_dispatched();
    diag.record_rate_limited();
    diag.record_relay_reconnect();
    diag.record_bunker_sign_failure();
    diag.record_bunker_sign_failure();
    diag.set_handlers_registered(3);

    let snap = diag.snapshot();
    assert_eq!(snap.events_received_total, 2);
    assert_eq!(snap.events_dispatched_total, 1);
    assert_eq!(snap.rate_limited_total, 1);
    assert_eq!(snap.relay_reconnects_total, 1);
    assert_eq!(snap.bunker_sign_failures_total, 2);
    assert_eq!(snap.handlers_registered, 3);
}

#[test]
fn status_transitions() {
    let diag = Diagnostics::new();
    assert_eq!(diag.snapshot().status, DaemonStatus::Initializing);

    diag.set_status(DaemonStatus::Ready);
    assert_eq!(diag.snapshot().status, DaemonStatus::Ready);

    diag.set_status(DaemonStatus::ShuttingDown);
    assert_eq!(diag.snapshot().status, DaemonStatus::ShuttingDown);

    diag.set_status(DaemonStatus::Stopped);
    assert_eq!(diag.snapshot().status, DaemonStatus::Stopped);
}

#[test]
fn report_flushes_and_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let diag = Diagnostics::new();

    diag.set_status(DaemonStatus::Ready);
    diag.record_event_received();
    diag.record_event_dispatched();
    diag.set_bots(vec![
        BotHealth {
            id: "bot-one".into(),
            npub: "npub1one".into(),
            relay_count: 2,
            bunker_connected: true,
        },
        BotHealth {
            id: "bot-two".into(),
            npub: "npub1two".into(),
            relay_count: 0,
            bunker_connected: false,
        },
    ]);

    diag.flush_report(tmp.path())?;

    let contents = read_latest_report(tmp.path())?;
    let parsed: HealthSnapshot = serde_json::from_str(&contents)?;

    assert_eq!(parsed.status, DaemonStatus::Ready);
    assert_eq!(parsed.events_received_total, 1);
    assert_eq!(parsed.events_dispatched_total, 1);
    assert_eq!(parsed.bots.len(), 2);
    assert_eq!(parsed.bots[0].id, "bot-one");
    assert!(!parsed.bots[0].npub.is_empty());

    Ok(())
}

#[test]
fn flushed_report_contains_no_secrets() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let diag = Diagnostics::new();

    diag.record_error("bunker signer rejected nsec1deadbeefcafebabe01020304050607");
    diag.record_error("token=super-secret-token and secret=do-not-share");
    diag.flush_report(tmp.path())?;

    let contents = read_latest_report(tmp.path())?;

    assert!(!contents.contains("nsec1deadbeefcafebabe01020304050607"));
    assert!(!contents.contains("super-secret-token"));
    assert!(!contents.contains("do-not-share"));
    assert!(contents.contains("[REDACTED]"));

    Ok(())
}

#[test]
fn report_directory_is_created_lazily() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let nested = tmp.path().join("a").join("b");
    let diag = Diagnostics::new();

    diag.flush_report(&nested)?;

    let contents = read_latest_report(&nested)?;
    assert!(contents.contains("\"status\""));
    Ok(())
}
