#![allow(clippy::unwrap_used)]

//! Integration tests for the on-chain governance reader.
//!
//! These tests run against a tiny in-process JSON-RPC server that returns
//! zero-valued responses, so no anvil node or real network is required.

use alloy::primitives::{Address, U256};
use alloy::providers::ProviderBuilder;
use governance_bot::evm::addresses::{hats_address, registry_address};
use governance_bot::evm::reader::{GovernanceError, GovernanceReader, TokenInfo};
use governance_bot::evm::snapshot::SnapshotData;
use serde_json::json;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawn a TCP server that echoes every JSON-RPC `id` and always returns a
/// zero-valued `eth_call` / `eth_getBalance` result.
async fn start_zero_rpc_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let body = String::from_utf8_lossy(&buf[..n]);
                let id = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.get("id").cloned())
                    .unwrap_or(json!(null));
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": "0x0000000000000000000000000000000000000000000000000000000000000000"
                });
                let response_body = response.to_string();
                let http = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                let _ = stream.write_all(http.as_bytes()).await;
            });
        }
    });
    addr
}

fn test_reader(addr: SocketAddr) -> GovernanceReader<impl alloy::providers::Provider> {
    let url = format!("http://{}", addr).parse().unwrap();
    let provider = ProviderBuilder::new().connect_http(url);
    GovernanceReader::new(provider, registry_address(), hats_address())
}

fn address(n: u8) -> Address {
    let mut bytes = [0u8; 20];
    bytes[19] = n;
    Address::from(bytes)
}

#[tokio::test]
async fn discover_squads_returns_empty_when_count_zero() {
    let addr = start_zero_rpc_server().await;
    let reader = test_reader(addr);
    let squads = reader.discover_squads().await.unwrap();
    assert!(
        squads.is_empty(),
        "deploymentCount=0 should yield empty squads"
    );
}

#[tokio::test]
async fn read_proposals_returns_empty_when_no_open_proposal() {
    let addr = start_zero_rpc_server().await;
    let reader = test_reader(addr);
    let proposer = address(1);
    let proposals = reader
        .read_proposals(address(2), &[proposer])
        .await
        .unwrap();
    assert!(
        proposals.is_empty(),
        "openProposalOf=0 should yield no proposals"
    );
}

#[tokio::test]
async fn read_mutiny_returns_empty_when_no_active_mutiny() {
    let addr = start_zero_rpc_server().await;
    let reader = test_reader(addr);
    let mutinies = reader.read_mutiny(address(3)).await.unwrap();
    assert!(
        mutinies.is_empty(),
        "activeMutinyId=0 should yield no mutiny"
    );
}

#[tokio::test]
async fn read_crew_deadlines_returns_empty_when_no_pending() {
    let addr = start_zero_rpc_server().await;
    let reader = test_reader(addr);
    let candidate = address(4);
    let deadlines = reader
        .read_crew_deadlines(address(5), &[candidate])
        .await
        .unwrap();
    assert!(
        deadlines.is_empty(),
        "pending timestamps=0 should yield no deadlines"
    );
}

#[tokio::test]
async fn read_treasury_balance_returns_zero() {
    let addr = start_zero_rpc_server().await;
    let reader = test_reader(addr);
    let balance = reader.read_treasury_balance(address(6)).await.unwrap();
    assert!(balance.eth_balance.is_zero());
    assert!(balance.tokens.is_empty());
}

#[tokio::test]
async fn read_crew_state_returns_inactive_for_zero_responses() {
    let addr = start_zero_rpc_server().await;
    let reader = test_reader(addr);
    let captain = address(7);
    let crew = address(8);
    let state = reader
        .read_crew_state(U256::from(1), U256::from(2), captain, &[crew])
        .await
        .unwrap();
    assert!(!state.captain.active);
    assert_eq!(state.crew.len(), 1);
    assert!(!state.crew[0].active);
}

#[tokio::test]
async fn rpc_failure_returns_provider_error_without_panic() {
    // Nothing is listening on 127.0.0.1:1, so the provider call should fail cleanly.
    let url = "http://127.0.0.1:1".parse().unwrap();
    let provider = ProviderBuilder::new().connect_http(url);
    let reader = GovernanceReader::new(provider, registry_address(), hats_address());
    let result = reader.discover_squads().await;
    assert!(
        result.is_err(),
        "unreachable RPC should surface a provider error"
    );
    let err = result.unwrap_err();
    assert!(matches!(err, GovernanceError::Provider(_)));
}

#[test]
fn snapshot_data_serde_roundtrip() {
    let original = SnapshotData::default();
    let json = serde_json::to_string(&original).unwrap();
    let decoded: SnapshotData = serde_json::from_str(&json).unwrap();
    assert_eq!(original, decoded);
}

#[tokio::test]
async fn read_treasury_balance_includes_configured_tokens() {
    let addr = start_zero_rpc_server().await;
    let reader = test_reader(addr).with_known_tokens(vec![TokenInfo {
        address: address(9),
        symbol: "TEST".to_string(),
        decimals: 18,
    }]);
    let balance = reader.read_treasury_balance(address(10)).await.unwrap();
    assert_eq!(balance.tokens.len(), 1);
    assert_eq!(balance.tokens[0].symbol, "TEST");
    assert!(balance.tokens[0].balance.is_zero());
}

#[tokio::test]
#[ignore = "requires anvil + pacto-dev-env (set PACTO_DEV_ENV=1)"]
async fn anvil_integration_reads_registry_without_panic() {
    if std::env::var("PACTO_DEV_ENV").unwrap_or_default() != "1" {
        return;
    }
    let url =
        std::env::var("PACTO_ANVIL_RPC").unwrap_or_else(|_| "http://localhost:8545".to_string());
    let provider = ProviderBuilder::new().connect_http(url.parse().unwrap());
    let reader = GovernanceReader::new(provider, registry_address(), hats_address());
    let squads = reader.discover_squads().await;
    assert!(
        squads.is_ok(),
        "anvil registry read should succeed or fail cleanly, not panic"
    );
}
