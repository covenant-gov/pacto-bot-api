//! On-chain governance reader for a single NavePirata squad.
//!
//! Uses inline `alloy::sol!` bindings and the `Provider::call`/`Provider::get_balance`
//! surface so that all reads can run against any JSON-RPC endpoint (Sepolia, anvil,
//! or a mocked transport).

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;

use crate::evm::bindings::{
    IERC20, IHats, IMutinyModule, INavePirataRegistry, IQuartermaster, ITreasuryAuthority,
};
use crate::evm::snapshot::{
    CrewDeadline, CrewState, DeadlineKind, HatState, Mutiny, Proposal, SnapshotData, SquadInfo,
    TokenBalance, TreasuryBalance,
};

pub use crate::evm::bindings::INavePirataRegistry::Deployment;

/// Errors that can occur while reading governance state.
#[derive(Debug, thiserror::Error)]
pub enum GovernanceError {
    /// Underlying RPC transport failure.
    #[error("provider error: {0}")]
    Provider(String),
    /// Return data could not be decoded.
    #[error("decode error: {0}")]
    Decode(String),
    /// The requested squad index is out of bounds.
    #[error("invalid squad index {index} (count: {count})")]
    InvalidSquadIndex { index: usize, count: usize },
}

/// A token the reader should track in treasury balances.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenInfo {
    pub address: Address,
    pub symbol: String,
    pub decimals: u8,
}

/// Reads public governance state for a Pacto squad.
#[derive(Debug, Clone)]
pub struct GovernanceReader<P> {
    provider: P,
    registry: Address,
    hats: Address,
    known_tokens: Vec<TokenInfo>,
}

impl<P: Provider> GovernanceReader<P> {
    /// Create a new reader bound to the given registry and Hats contracts.
    pub fn new(provider: P, registry: Address, hats: Address) -> Self {
        Self {
            provider,
            registry,
            hats,
            known_tokens: Vec::new(),
        }
    }

    /// Add ERC-20 tokens whose balances should be included in treasury reads.
    pub fn with_known_tokens(mut self, tokens: Vec<TokenInfo>) -> Self {
        self.known_tokens = tokens;
        self
    }

    /// Discover every squad registered in `NavePirataRegistry`.
    ///
    /// Returns an empty list when `deploymentCount() == 0`.
    pub async fn discover_squads(&self) -> Result<Vec<SquadInfo>, GovernanceError> {
        let count = self
            .eth_call_decode(self.registry, &INavePirataRegistry::deploymentCountCall {})
            .await?;
        let count: usize = count.try_into().unwrap_or(usize::MAX);

        let mut squads = Vec::with_capacity(count);
        for i in 0..count {
            let top_hat = self
                .eth_call_decode(
                    self.registry,
                    &INavePirataRegistry::deploymentAtCall { _i: U256::from(i) },
                )
                .await?;
            let deployment = self
                .eth_call_decode(
                    self.registry,
                    &INavePirataRegistry::deploymentCall { _topHatId: top_hat },
                )
                .await?;
            squads.push(deployment_to_squad_info(deployment));
        }
        Ok(squads)
    }

    /// Read all open proposals for a list of candidate proposers.
    pub async fn read_proposals(
        &self,
        treasury_authority: Address,
        candidate_proposers: &[Address],
    ) -> Result<Vec<Proposal>, GovernanceError> {
        let mut proposals = Vec::new();
        for proposer in candidate_proposers {
            let id = self
                .eth_call_decode(
                    treasury_authority,
                    &ITreasuryAuthority::openProposalOfCall {
                        _proposer: *proposer,
                    },
                )
                .await?;
            if id.is_zero() {
                continue;
            }
            let p = self
                .eth_call_decode(
                    treasury_authority,
                    &ITreasuryAuthority::proposalCall { _id: id },
                )
                .await?;
            proposals.push(Proposal {
                id,
                proposer: p._proposer,
                to: p._to,
                value: p._value,
                op: p._op as u8,
                data: p._data.to_vec(),
                deadline: p._deadline,
                snapshot: U256::from(p._snapshot),
                yeas: U256::from(p._yeas),
                nays: U256::from(p._nays),
                captain_approved: p._captainApproved,
                captain_defeated: p._captainDefeated,
                executed: p._executed,
            });
        }
        Ok(proposals)
    }

    /// Read the active mutiny, if any, from the MutinyModule.
    pub async fn read_mutiny(
        &self,
        mutiny_module: Address,
    ) -> Result<Vec<Mutiny>, GovernanceError> {
        let active_id = self
            .eth_call_decode(mutiny_module, &IMutinyModule::activeMutinyIdCall {})
            .await?;
        if active_id.is_zero() {
            return Ok(Vec::new());
        }
        let m = self
            .eth_call_decode(mutiny_module, &IMutinyModule::mutinyCall { _id: active_id })
            .await?;
        Ok(vec![Mutiny {
            id: active_id,
            proposed_new_captain: m._proposedNewCaptain,
            started_at: m._startedAt,
            snapshot: U256::from(m._snapshot),
            yeas: U256::from(m._yeas),
            executed: m._executed,
        }])
    }

    /// Read pending crew-add / crew-remove deadlines for a list of candidate addresses.
    pub async fn read_crew_deadlines(
        &self,
        quartermaster: Address,
        crew_candidates: &[Address],
    ) -> Result<Vec<CrewDeadline>, GovernanceError> {
        let mut deadlines = Vec::new();
        for candidate in crew_candidates {
            let add_at = self
                .eth_call_decode(
                    quartermaster,
                    &IQuartermaster::pendingCrewAddAtCall {
                        _candidate: *candidate,
                    },
                )
                .await?;
            if !add_at.is_zero() {
                deadlines.push(CrewDeadline {
                    kind: DeadlineKind::Add,
                    target: *candidate,
                    executable_at: add_at.try_into().unwrap_or(u64::MAX),
                });
            }
            let remove_at = self
                .eth_call_decode(
                    quartermaster,
                    &IQuartermaster::pendingCrewRemoveAtCall { _crew: *candidate },
                )
                .await?;
            if !remove_at.is_zero() {
                deadlines.push(CrewDeadline {
                    kind: DeadlineKind::Remove,
                    target: *candidate,
                    executable_at: remove_at.try_into().unwrap_or(u64::MAX),
                });
            }
        }
        Ok(deadlines)
    }

    /// Read ETH and configured ERC-20 balances held by the squad Safe.
    pub async fn read_treasury_balance(
        &self,
        safe: Address,
    ) -> Result<TreasuryBalance, GovernanceError> {
        let eth_balance = self
            .provider
            .get_balance(safe)
            .await
            .map_err(|e| GovernanceError::Provider(e.to_string()))?;
        let mut tokens = Vec::with_capacity(self.known_tokens.len());
        for token in &self.known_tokens {
            let balance = self
                .eth_call_decode(token.address, &IERC20::balanceOfCall { _account: safe })
                .await?;
            tokens.push(TokenBalance {
                token: token.address,
                symbol: token.symbol.clone(),
                decimals: token.decimals,
                balance,
            });
        }
        Ok(TreasuryBalance {
            eth_balance,
            tokens,
        })
    }

    /// Read the active status of the captain and a list of candidate crew members.
    pub async fn read_crew_state(
        &self,
        captain_hat: U256,
        crew_hat: U256,
        captain: Address,
        crew_candidates: &[Address],
    ) -> Result<CrewState, GovernanceError> {
        let captain_active = self
            .eth_call_decode(
                self.hats,
                &IHats::isWearerOfHatCall {
                    _user: captain,
                    _hatId: captain_hat,
                },
            )
            .await?;
        let mut crew = Vec::with_capacity(crew_candidates.len());
        for wearer in crew_candidates {
            let active = self
                .eth_call_decode(
                    self.hats,
                    &IHats::isWearerOfHatCall {
                        _user: *wearer,
                        _hatId: crew_hat,
                    },
                )
                .await?;
            crew.push(HatState {
                wearer: *wearer,
                hat_id: crew_hat,
                active,
            });
        }
        Ok(CrewState {
            captain: HatState {
                wearer: captain,
                hat_id: captain_hat,
                active: captain_active,
            },
            crew,
        })
    }

    /// Build a full snapshot for the squad at the given registry index.
    ///
    /// `crew_candidates` and `proposer_candidates` are the addresses the caller
    /// wants to check for crew deadlines / proposals. Because the contracts do
    /// not expose enumerable lists of wearers or proposers, these candidate sets
    /// must be supplied by the caller (e.g. from an off-chain roster or previous
    /// events).
    pub async fn snapshot(
        &self,
        squad_index: usize,
        captain: Address,
        crew_candidates: &[Address],
        proposer_candidates: &[Address],
    ) -> Result<SnapshotData, GovernanceError> {
        let squads = self.discover_squads().await?;
        let squad =
            squads
                .into_iter()
                .nth(squad_index)
                .ok_or(GovernanceError::InvalidSquadIndex {
                    index: squad_index,
                    count: 0,
                })?;

        let proposals = self
            .read_proposals(squad.treasury_authority, proposer_candidates)
            .await?;
        let mutinies = self.read_mutiny(squad.mutiny_module).await?;
        let crew_deadlines = self
            .read_crew_deadlines(squad.quartermaster, crew_candidates)
            .await?;
        let treasury = self.read_treasury_balance(squad.safe).await?;
        let crew_state = self
            .read_crew_state(
                squad.captain_hat_id,
                squad.crew_hat_id,
                captain,
                crew_candidates,
            )
            .await?;

        Ok(SnapshotData {
            squad,
            proposals,
            mutinies,
            crew_deadlines,
            treasury,
            crew_state,
            generated_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default(),
        })
    }

    async fn eth_call_decode<C>(&self, to: Address, call: &C) -> Result<C::Return, GovernanceError>
    where
        C: SolCall,
    {
        let tx = TransactionRequest::default()
            .with_to(to)
            .with_input(Bytes::from(call.abi_encode()));
        let raw = self
            .provider
            .call(tx)
            .await
            .map_err(|e| GovernanceError::Provider(e.to_string()))?;
        C::abi_decode_returns(raw.as_ref()).map_err(|e| GovernanceError::Decode(e.to_string()))
    }
}

fn deployment_to_squad_info(d: Deployment) -> SquadInfo {
    SquadInfo {
        safe: d.safe,
        quartermaster: d.quartermaster,
        mutiny_module: d.mutinyModule,
        treasury_authority: d.treasuryAuthority,
        squad_admin_proxy: d.squadAdminProxy,
        top_hat_id: d.topHatId,
        captain_hat_id: d.captainHatId,
        crew_hat_id: d.crewHatId,
        squad_admin_hat_id: d.squadAdminHatId,
        mutiny_role_hat_id: d.mutinyRoleHatId,
        quartermaster_role_hat_id: d.quartermasterRoleHatId,
        treasury_authority_role_hat_id: d.treasuryAuthorityRoleHatId,
        deployed_at: d.deployedAt,
        deployer: d.deployer,
    }
}
