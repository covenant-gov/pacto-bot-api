//! Integration tests for the dev-env service-version probe.
//!
//! req(R36)

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::{Router, response::Json, routing};
use pacto_bot_api::dev_env_probe::{
    ProbeEndpoints, ProbeStatus, is_failure, run_probe_with_endpoints,
};
use serde_json::json;

fn in_window_app() -> Router {
    Router::new()
        .route(
            "/",
            routing::get(|| async { Json(json!({"version": "1.2.3"})) }),
        )
        .route(
            "/evm",
            routing::post(|| async {
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": "anvil/v1.2.3"
                }))
            }),
        )
        .route(
            "/version",
            routing::get(|| async { Json(json!({"version": "1.2.3"})) }),
        )
        .route(
            "/aztec",
            routing::post(|| async {
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": "aztec/v1.2.3"
                }))
            }),
        )
}

async fn spawn_mock_server(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}", addr);
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, handle)
}

#[tokio::test]
async fn all_services_in_window_return_ok() {
    let (url, _handle) = spawn_mock_server(in_window_app()).await;
    let endpoints = ProbeEndpoints {
        relay: url.clone(),
        evm: format!("{url}/evm"),
        nostra: url.clone(),
        aztec: format!("{url}/aztec"),
    };
    let results = run_probe_with_endpoints(endpoints).await;
    for result in &results {
        assert_eq!(
            result.status,
            ProbeStatus::Ok,
            "{} probe failed: {:?}",
            result.service,
            result.status
        );
    }
}

#[tokio::test]
async fn unreachable_required_service_is_a_failure() {
    let endpoints = ProbeEndpoints {
        relay: "http://127.0.0.1:1".into(),
        evm: "http://127.0.0.1:1".into(),
        nostra: "http://127.0.0.1:1".into(),
        aztec: "http://127.0.0.1:1".into(),
    };
    let results = run_probe_with_endpoints(endpoints).await;
    let relay = results.iter().find(|r| r.service == "relay").unwrap();
    assert!(is_failure(relay), "unreachable relay should be a failure");
    let evm = results.iter().find(|r| r.service == "evm").unwrap();
    assert!(is_failure(evm), "unreachable evm should be a failure");
}

#[tokio::test]
async fn unreachable_optional_service_is_not_a_failure() {
    let endpoints = ProbeEndpoints {
        relay: "http://127.0.0.1:1".into(),
        evm: "http://127.0.0.1:1".into(),
        nostra: "http://127.0.0.1:1".into(),
        aztec: "http://127.0.0.1:1".into(),
    };
    let results = run_probe_with_endpoints(endpoints).await;
    let nostra = results.iter().find(|r| r.service == "nostra").unwrap();
    assert!(
        !is_failure(nostra),
        "unreachable nostra should not be a failure"
    );
    let aztec = results.iter().find(|r| r.service == "aztec").unwrap();
    assert!(
        !is_failure(aztec),
        "unreachable aztec should not be a failure"
    );
}
