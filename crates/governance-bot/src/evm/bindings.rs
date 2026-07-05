//! Inline `alloy::sol!` bindings for on-chain Pacto governance reads.
//!
//! Mirrors the style of `pacto-app/src-tauri/src/evm/contracts/pacto_gov/read_bindings.rs`.
//! Bindings are generated directly from the Solidity interfaces in `pacto-gov` so that
//! no JSON ABI files need to be vendored.

use alloy::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface INavePirataRegistry {
        struct Deployment {
            address safe;
            address quartermaster;
            address mutinyModule;
            address treasuryAuthority;
            address squadAdminProxy;
            uint256 topHatId;
            uint256 captainHatId;
            uint256 crewHatId;
            uint256 squadAdminHatId;
            uint256 mutinyRoleHatId;
            uint256 quartermasterRoleHatId;
            uint256 treasuryAuthorityRoleHatId;
            uint64 deployedAt;
            address deployer;
        }

        function deploymentCount() external view returns (uint256 _count);
        function deploymentAt(uint256 _i) external view returns (uint256 _topHatId);
        function deployment(uint256 _topHatId) external view returns (Deployment memory _deployment);
    }

    interface ITreasuryAuthority {
        enum Operation {
            CALL,
            DELEGATECALL
        }

        function openProposalOf(address _proposer) external view returns (uint256 _openProposalId);

        function proposal(uint256 _id)
            external
            view
            returns (
                address _proposer,
                address _to,
                uint256 _value,
                Operation _op,
                bytes memory _data,
                uint64 _deadline,
                uint64 _snapshot,
                uint64 _yeas,
                uint64 _nays,
                bool _captainApproved,
                bool _captainDefeated,
                bool _executed
            );
    }

    interface IMutinyModule {
        function activeMutinyId() external view returns (uint256 _id);

        function mutiny(uint256 _id)
            external
            view
            returns (address _proposedNewCaptain, uint64 _startedAt, uint64 _snapshot, uint64 _yeas, bool _executed);
    }

    interface IQuartermaster {
        function pendingCrewAddAt(address _candidate) external view returns (uint256 _executableAt);
        function pendingCrewRemoveAt(address _crew) external view returns (uint256 _executableAt);
        function crewChangeDelay() external view returns (uint256 _delay);
        function mutinyActive() external view returns (bool _active);
        function captainHatId() external view returns (uint256 _captainHatId);
        function crewHatId() external view returns (uint256 _crewHatId);
    }

    interface IERC20 {
        function balanceOf(address _account) external view returns (uint256 _balance);
    }

    interface IHats {
        function isWearerOfHat(address _user, uint256 _hatId) external view returns (bool _isWearer);
    }
}
