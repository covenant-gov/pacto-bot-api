//! Service-version compatibility probing for the pacto-bot-api dev environment.
//!
//! Compares versions reported by local backing services against the windows
//! declared in `schemas/service-compatibility-data.json`.

use crate::errors::DaemonError;
use crate::service_compatibility_generated::{
    ServiceCompatibilityGenerated as ServiceCompatibility, VersionWindowGenerated as ServiceWindow,
};
use std::cmp::Ordering;

/// Outcome of probing one service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeStatus {
    /// Detected version is inside the configured window.
    Ok,
    /// Detected version is outside the configured window.
    OutOfWindow {
        detected: String,
        min: String,
        max: String,
    },
    /// The service could not be reached.
    Unreachable(String),
    /// The response was received but could not be parsed for a version.
    ParseError(String),
}

/// Result of probing a single service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub service: &'static str,
    pub endpoint: String,
    pub status: ProbeStatus,
}

impl ServiceCompatibility {
    /// Load the canonical compatibility windows embedded at build time.
    pub fn load() -> Result<Self, DaemonError> {
        const DATA: &str = include_str!("../schemas/service-compatibility-data.json");
        serde_json::from_str(DATA).map_err(DaemonError::Json)
    }
}

/// Default endpoint URLs, overridable through environment variables.
pub fn relay_endpoint() -> String {
    std::env::var("PACTO_PROBE_RELAY_URL").unwrap_or_else(|_| "http://localhost:7000".into())
}

pub fn evm_endpoint() -> String {
    std::env::var("PACTO_PROBE_EVM_URL").unwrap_or_else(|_| "http://localhost:8545".into())
}

pub fn nostra_endpoint() -> String {
    std::env::var("PACTO_PROBE_NOSTRA_URL").unwrap_or_else(|_| "http://localhost:3002".into())
}

pub fn aztec_endpoint() -> String {
    std::env::var("PACTO_PROBE_AZTEC_URL").unwrap_or_else(|_| "http://localhost:8080".into())
}

/// Explicit endpoints used by `run_probe_with_endpoints`.
#[derive(Debug, Clone)]
pub struct ProbeEndpoints {
    pub relay: String,
    pub evm: String,
    pub nostra: String,
    pub aztec: String,
}

impl Default for ProbeEndpoints {
    fn default() -> Self {
        Self {
            relay: relay_endpoint(),
            evm: evm_endpoint(),
            nostra: nostra_endpoint(),
            aztec: aztec_endpoint(),
        }
    }
}

/// Probe every configured service using the default endpoints and return
/// per-service results.
///
/// Services are queried concurrently. The function never panics; unreachable
/// or misbehaving services are reported as `ProbeStatus::Unreachable` or
/// `ProbeStatus::ParseError`.
pub async fn run_probe() -> Vec<ProbeResult> {
    run_probe_with_endpoints(ProbeEndpoints::default()).await
}

/// Probe every configured service using the supplied endpoints.
pub async fn run_probe_with_endpoints(endpoints: ProbeEndpoints) -> Vec<ProbeResult> {
    let schema = match ServiceCompatibility::load() {
        Ok(s) => s,
        Err(e) => {
            return vec![ProbeResult {
                service: "schema",
                endpoint: "schemas/service-compatibility-data.json".into(),
                status: ProbeStatus::ParseError(e.to_string()),
            }];
        }
    };

    let client = reqwest::Client::new();

    let relay = probe_relay(&client, &endpoints.relay, &schema.relay);
    let evm = probe_evm(&client, &endpoints.evm, &schema.evm);
    let nostra = probe_nostra(&client, &endpoints.nostra, &schema.nostra);
    let aztec = probe_aztec(&client, &endpoints.aztec, &schema.aztec);

    let (relay, evm, nostra, aztec) = tokio::join!(relay, evm, nostra, aztec);
    vec![relay, evm, nostra, aztec]
}

/// Probe the Nostr relay NIP-11 metadata endpoint for a `version` field.
async fn probe_relay(client: &reqwest::Client, url: &str, window: &ServiceWindow) -> ProbeResult {
    let response = match client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/nostr+json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return ProbeResult {
                service: "relay",
                endpoint: url.into(),
                status: ProbeStatus::Unreachable(format!("request failed: {e}")),
            };
        }
    };

    let body = match response.json::<serde_json::Value>().await {
        Ok(v) => v,
        Err(e) => {
            return ProbeResult {
                service: "relay",
                endpoint: url.into(),
                status: ProbeStatus::ParseError(format!("invalid json: {e}")),
            };
        }
    };

    let version = match body.get("version").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => {
            return ProbeResult {
                service: "relay",
                endpoint: url.into(),
                status: ProbeStatus::ParseError("missing version field".into()),
            };
        }
    };

    ProbeResult {
        service: "relay",
        endpoint: url.into(),
        status: check_version(version, &window.min_version, &window.max_version),
    }
}

/// Probe the EVM node using `web3_clientVersion`.
async fn probe_evm(client: &reqwest::Client, url: &str, window: &ServiceWindow) -> ProbeResult {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "web3_clientVersion",
        "params": []
    });

    let response = match client.post(url).json(&payload).send().await {
        Ok(r) => r,
        Err(e) => {
            return ProbeResult {
                service: "evm",
                endpoint: url.into(),
                status: ProbeStatus::Unreachable(format!("request failed: {e}")),
            };
        }
    };

    let body = match response.json::<serde_json::Value>().await {
        Ok(v) => v,
        Err(e) => {
            return ProbeResult {
                service: "evm",
                endpoint: url.into(),
                status: ProbeStatus::ParseError(format!("invalid json: {e}")),
            };
        }
    };

    let version = match body.get("result").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => {
            return ProbeResult {
                service: "evm",
                endpoint: url.into(),
                status: ProbeStatus::ParseError("missing result field".into()),
            };
        }
    };

    ProbeResult {
        service: "evm",
        endpoint: url.into(),
        status: check_version(version, &window.min_version, &window.max_version),
    }
}

/// Probe the Nostra service via a simple HTTP `GET /version` endpoint.
async fn probe_nostra(client: &reqwest::Client, url: &str, window: &ServiceWindow) -> ProbeResult {
    let endpoint = format!("{url}/version");
    let response = match client.get(&endpoint).send().await {
        Ok(r) => r,
        Err(e) => {
            return ProbeResult {
                service: "nostra",
                endpoint,
                status: ProbeStatus::Unreachable(format!("request failed: {e}")),
            };
        }
    };

    let body = match response.json::<serde_json::Value>().await {
        Ok(v) => v,
        Err(e) => {
            return ProbeResult {
                service: "nostra",
                endpoint,
                status: ProbeStatus::ParseError(format!("invalid json: {e}")),
            };
        }
    };

    let version = match body.get("version").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => {
            return ProbeResult {
                service: "nostra",
                endpoint,
                status: ProbeStatus::ParseError("missing version field".into()),
            };
        }
    };

    ProbeResult {
        service: "nostra",
        endpoint,
        status: check_version(version, &window.min_version, &window.max_version),
    }
}

/// Probe the Aztec sandbox using an Ethereum-compatible `web3_clientVersion`
/// JSON-RPC call.
async fn probe_aztec(client: &reqwest::Client, url: &str, window: &ServiceWindow) -> ProbeResult {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "web3_clientVersion",
        "params": []
    });

    let response = match client.post(url).json(&payload).send().await {
        Ok(r) => r,
        Err(e) => {
            return ProbeResult {
                service: "aztec",
                endpoint: url.into(),
                status: ProbeStatus::Unreachable(format!("request failed: {e}")),
            };
        }
    };

    let body = match response.json::<serde_json::Value>().await {
        Ok(v) => v,
        Err(e) => {
            return ProbeResult {
                service: "aztec",
                endpoint: url.into(),
                status: ProbeStatus::ParseError(format!("invalid json: {e}")),
            };
        }
    };

    let version = match body.get("result").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => {
            return ProbeResult {
                service: "aztec",
                endpoint: url.into(),
                status: ProbeStatus::ParseError("missing result field".into()),
            };
        }
    };

    ProbeResult {
        service: "aztec",
        endpoint: url.into(),
        status: check_version(version, &window.min_version, &window.max_version),
    }
}

/// Compare `version` against `[min, max]` and return a `ProbeStatus`.
fn check_version(version: &str, min: &str, max: &str) -> ProbeStatus {
    let detected = match extract_version(version) {
        Some(v) => v,
        None => {
            return ProbeStatus::ParseError(format!("could not parse version from '{version}'"));
        }
    };
    let min_parts = match extract_version(min) {
        Some(v) => v,
        None => {
            return ProbeStatus::ParseError(format!("could not parse min_version '{min}'"));
        }
    };
    let max_parts = match extract_version(max) {
        Some(v) => v,
        None => {
            return ProbeStatus::ParseError(format!("could not parse max_version '{max}'"));
        }
    };

    if cmp_versions(&detected, &min_parts) == Ordering::Less
        || cmp_versions(&detected, &max_parts) == Ordering::Greater
    {
        ProbeStatus::OutOfWindow {
            detected: version.into(),
            min: min.into(),
            max: max.into(),
        }
    } else {
        ProbeStatus::Ok
    }
}

/// Extract the first dotted numeric version sequence from a free-form string.
///
/// Examples:
/// - `"v1.2.3"` -> `[1, 2, 3]`
/// - `"anvil/v0.3.0"` -> `[0, 3, 0]`
/// - `"1.2.3-alpha"` -> `[1, 2, 3]`
fn extract_version(s: &str) -> Option<Vec<u64>> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let start = i;
            let mut dot_count = 0;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                if chars[i] == '.' {
                    dot_count += 1;
                }
                i += 1;
            }
            if dot_count > 0 {
                let parts: Vec<u64> = chars[start..i]
                    .split(|c| *c == '.')
                    .map(|part| part.iter().collect::<String>().parse().unwrap_or(0))
                    .collect();
                if parts.len() >= 2 {
                    return Some(parts);
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Lexicographic comparison of dotted version vectors.
fn cmp_versions(a: &[u64], b: &[u64]) -> Ordering {
    let len = a.len().max(b.len());
    for i in 0..len {
        let av = a.get(i).copied().unwrap_or(0);
        let bv = b.get(i).copied().unwrap_or(0);
        match av.cmp(&bv) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

/// Returns `true` when a probe result should be considered a hard failure.
///
/// Relay and EVM are required for the default dev stack. Nostra and Aztec are
/// optional profiles; they only fail when they are reachable and report an
/// out-of-window version.
pub fn is_failure(result: &ProbeResult) -> bool {
    match &result.status {
        ProbeStatus::Ok => false,
        ProbeStatus::OutOfWindow { .. } => true,
        ProbeStatus::Unreachable(_) | ProbeStatus::ParseError(_) => {
            matches!(result.service, "relay" | "evm")
        }
    }
}

/// Log warnings for every non-Ok probe result.
pub fn log_warnings(results: &[ProbeResult]) {
    for result in results {
        match &result.status {
            ProbeStatus::Ok => {}
            ProbeStatus::OutOfWindow { detected, min, max } => {
                tracing::warn!(
                    service = %result.service,
                    endpoint = %result.endpoint,
                    detected_version = %detected,
                    min_version = %min,
                    max_version = %max,
                    "service version is outside the compatibility window"
                );
            }
            ProbeStatus::Unreachable(e) => {
                tracing::warn!(
                    service = %result.service,
                    endpoint = %result.endpoint,
                    error = %e,
                    "service is unreachable"
                );
            }
            ProbeStatus::ParseError(e) => {
                tracing::warn!(
                    service = %result.service,
                    endpoint = %result.endpoint,
                    error = %e,
                    "service version could not be parsed"
                );
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn schema_loads() {
        let schema = ServiceCompatibility::load().expect("service compatibility data should load");
        assert!(!schema.relay.min_version.is_empty());
        assert!(!schema.relay.max_version.is_empty());
        assert!(!schema.evm.min_version.is_empty());
        assert!(!schema.evm.max_version.is_empty());
        assert!(!schema.nostra.min_version.is_empty());
        assert!(!schema.nostra.max_version.is_empty());
        assert!(!schema.aztec.min_version.is_empty());
        assert!(!schema.aztec.max_version.is_empty());
    }

    #[test]
    fn extract_version_handles_prefixes_and_suffixes() {
        assert_eq!(extract_version("v1.2.3"), Some(vec![1, 2, 3]));
        assert_eq!(extract_version("anvil/v0.3.0"), Some(vec![0, 3, 0]));
        assert_eq!(extract_version("1.2.3-alpha"), Some(vec![1, 2, 3]));
        assert_eq!(extract_version("no version here"), None);
    }

    #[test]
    fn version_comparison_respects_bounds() {
        assert_eq!(check_version("1.2.3", "1.0.0", "2.0.0"), ProbeStatus::Ok);
        assert!(matches!(
            check_version("0.9.0", "1.0.0", "2.0.0"),
            ProbeStatus::OutOfWindow { .. }
        ));
        assert!(matches!(
            check_version("2.1.0", "1.0.0", "2.0.0"),
            ProbeStatus::OutOfWindow { .. }
        ));
    }

    #[test]
    fn failure_detection() {
        let ok = ProbeResult {
            service: "relay",
            endpoint: "http://localhost:7000".into(),
            status: ProbeStatus::Ok,
        };
        assert!(!is_failure(&ok));

        let out = ProbeResult {
            service: "nostra",
            endpoint: "http://localhost:3002".into(),
            status: ProbeStatus::OutOfWindow {
                detected: "9.0.0".into(),
                min: "1.0.0".into(),
                max: "2.0.0".into(),
            },
        };
        assert!(is_failure(&out));

        let unreachable_relay = ProbeResult {
            service: "relay",
            endpoint: "http://localhost:7000".into(),
            status: ProbeStatus::Unreachable("connection refused".into()),
        };
        assert!(is_failure(&unreachable_relay));

        let unreachable_aztec = ProbeResult {
            service: "aztec",
            endpoint: "http://localhost:8080".into(),
            status: ProbeStatus::Unreachable("connection refused".into()),
        };
        assert!(!is_failure(&unreachable_aztec));
    }
}
