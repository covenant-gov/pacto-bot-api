//! Generated from schemas/service-compatibility.json — do not edit manually.
//! Run `cargo xtask codegen` to regenerate.

use serde::{Deserialize, Serialize};

/// Generated from `#/definitions/versionWindow`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionWindowGenerated {
    /// max_version
    pub max_version: String,
    /// min_version
    pub min_version: String,
}

/// Service compatibility windows generated from schemas/service-compatibility.json.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceCompatibilityGenerated {
    /// aztec
    pub aztec: VersionWindowGenerated,
    /// bunker
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bunker: Option<VersionWindowGenerated>,
    /// evm
    pub evm: VersionWindowGenerated,
    /// nostra
    pub nostra: VersionWindowGenerated,
    /// relay
    pub relay: VersionWindowGenerated,
}
