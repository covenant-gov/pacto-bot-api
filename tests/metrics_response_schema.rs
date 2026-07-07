//! Live schema validation for the `agent.metrics` response payload.
//!
//! req(R32)

#![allow(clippy::unwrap_used)]

use pacto_bot_api::diagnostics::Diagnostics;
use pacto_bot_api::transport::protocol::MetricsResponse;

#[tokio::test]
async fn metrics_response_exposes_all_counters() {
    let diag = Diagnostics::new();
    diag.record_event_received().await;
    diag.record_event_dispatched().await;
    diag.record_rate_limited().await;
    diag.record_relay_reconnect().await;
    diag.record_bunker_sign_failure().await;
    diag.record_invalid_event().await;
    diag.record_send_dm().await;
    diag.record_send_dm_failed().await;
    diag.record_reply_send_failed().await;
    diag.set_handlers_registered(3).await;

    let snap = diag.snapshot().await;
    let metrics = MetricsResponse::from(snap.clone());

    assert_eq!(metrics.uptime_seconds, Some(snap.uptime_seconds));
    assert_eq!(metrics.handlers_registered, Some(3));
    assert_eq!(metrics.events_received_total, Some(1));
    assert_eq!(metrics.events_dispatched_total, Some(1));
    assert_eq!(metrics.rate_limited_total, Some(1));
    assert_eq!(metrics.relay_reconnects_total, Some(1));
    assert_eq!(metrics.bunker_sign_failures_total, Some(1));
    assert_eq!(metrics.invalid_events_total, Some(1));
    assert_eq!(metrics.send_dm_total, Some(1));
    assert_eq!(metrics.send_dm_failed_total, Some(1));
    assert_eq!(metrics.reply_send_failed_total, Some(1));

    // Recent-counts fields are populated from the rolling window.
    assert!(metrics.events_received_last_10_min.unwrap() >= 1);
    assert!(metrics.events_dispatched_last_10_min.unwrap() >= 1);
    assert!(metrics.replies_last_10_min.is_some());
    assert!(metrics.reply_send_failed_last_10_min.unwrap() >= 1);
    assert!(metrics.send_dm_last_10_min.unwrap() >= 1);
    assert!(metrics.send_dm_failed_last_10_min.unwrap() >= 1);

    let value = serde_json::to_value(&metrics).unwrap();
    let object = value.as_object().unwrap();
    assert!(object.contains_key("send_dm_total"));
    assert!(object.contains_key("send_dm_failed_total"));
    assert!(object.contains_key("send_dm_last_10_min"));
    assert!(object.contains_key("send_dm_failed_last_10_min"));
    assert!(!object.contains_key("status"));
    assert!(!object.contains_key("errors"));
    assert!(!object.contains_key("startup_time"));
    assert!(!object.contains_key("reported_at"));
}

#[tokio::test]
async fn metrics_response_validates_against_schema() -> Result<(), Box<dyn std::error::Error>> {
    let diag = Diagnostics::new();
    diag.record_event_received().await;
    diag.record_send_dm().await;
    diag.record_send_dm_failed().await;
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
