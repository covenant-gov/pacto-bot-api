use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

/// Aggregated governance snapshot for a single squad.
///
/// This is the public interface between the on-chain reader (U8) and the
/// Markdown formatter (U9). Field names and types are intentionally stable;
/// do not change them without coordinating with downstream consumers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SnapshotData {
    /// On-chain squad metadata from the registry.
    pub squad: SquadInfo,
    /// Active proposals from the squad's TreasuryAuthority.
    pub proposals: Vec<Proposal>,
    /// Active mutinies from the squad's MutinyModule.
    pub mutinies: Vec<Mutiny>,
    /// Pending crew-add / crew-remove deadlines from the Quartermaster.
    pub crew_deadlines: Vec<CrewDeadline>,
    /// Treasury balances for the squad Safe (ETH + ERC-20s).
    pub treasury: TreasuryBalance,
    /// Captain and crew state derived from Hats.
    pub crew_state: CrewState,
    /// Unix timestamp (seconds) when the snapshot was generated.
    pub generated_at: u64,
}

/// Metadata describing a single NavePirata squad deployment.
///
/// Mirrors the `Deployment` struct returned by `INavePirataRegistry` so that
/// downstream formatters can access every hat id and address without another
/// round-trip.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SquadInfo {
    pub safe: Address,
    pub quartermaster: Address,
    pub mutiny_module: Address,
    pub treasury_authority: Address,
    pub squad_admin_proxy: Address,
    pub top_hat_id: U256,
    pub captain_hat_id: U256,
    pub crew_hat_id: U256,
    pub squad_admin_hat_id: U256,
    pub mutiny_role_hat_id: U256,
    pub quartermaster_role_hat_id: U256,
    pub treasury_authority_role_hat_id: U256,
    pub deployed_at: u64,
    pub deployer: Address,
}

/// A governance proposal tracked by TreasuryAuthority.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Proposal {
    pub id: U256,
    pub proposer: Address,
    pub to: Address,
    pub value: U256,
    pub op: u8,
    pub data: Vec<u8>,
    pub deadline: u64,
    pub snapshot: U256,
    pub yeas: U256,
    pub nays: U256,
    pub captain_approved: bool,
    pub captain_defeated: bool,
    pub executed: bool,
}

/// An active mutiny against the current captain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Mutiny {
    pub id: U256,
    pub proposed_new_captain: Address,
    pub started_at: u64,
    pub snapshot: U256,
    pub yeas: U256,
    pub executed: bool,
}

/// A pending crew roster change with its executable timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CrewDeadline {
    pub kind: DeadlineKind,
    pub target: Address,
    pub executable_at: u64,
}

/// Kind of pending crew roster change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeadlineKind {
    Add,
    Remove,
}

/// Treasury holdings for the squad Safe.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TreasuryBalance {
    pub eth_balance: U256,
    pub tokens: Vec<TokenBalance>,
}

/// Balance of a single ERC-20 token held by the Safe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TokenBalance {
    pub token: Address,
    pub symbol: String,
    pub decimals: u8,
    pub balance: U256,
}

/// Captain and crew state derived from Hats.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CrewState {
    pub captain: HatState,
    pub crew: Vec<HatState>,
}

/// Status of a single hat wearer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HatState {
    pub wearer: Address,
    pub hat_id: U256,
    pub active: bool,
}
