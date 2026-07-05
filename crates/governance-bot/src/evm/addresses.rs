//! Hard-coded Sepolia infrastructure addresses plus anvil override helpers.
//!
//! Per-squad clone addresses are discovered dynamically via
//! [`INavePirataRegistry`](crate::evm::bindings::INavePirataRegistry); only the
//! singleton registry and the canonical Hats contract live here.

use alloy::primitives::Address;

/// Sepolia chain ID.
pub const SEPOLIA_CHAIN_ID: u64 = 11155111;

/// Anvil / local devnet chain ID.
pub const ANVIL_CHAIN_ID: u64 = 31_337;

/// Sepolia `NavePirataRegistry` address from
/// `pacto-gov/deployments/11155111/full-system.json`.
pub const SEPOLIA_REGISTRY: Address =
    Address::new(alloy::hex!("45127C1c92741C0dA38e1A73fbb97a8a2C46770f"));

/// Sepolia Hats Protocol contract address.
pub const SEPOLIA_HATS: Address =
    Address::new(alloy::hex!("3bc1A0Ad72417f2d411118085256fC53CBdDd137"));

/// Environment variable used to override the registry address for local anvil testing.
pub const REGISTRY_OVERRIDE_ENV: &str = "PACTO_GOVERNANCE_REGISTRY";

/// Environment variable used to override the Hats contract address for local anvil testing.
pub const HATS_OVERRIDE_ENV: &str = "PACTO_GOVERNANCE_HATS";

/// Resolve the registry address to use for reads.
///
/// Honors environment overrides first, then falls back to the Sepolia canonical address.
pub fn registry_address() -> Address {
    if let Some(addr) = read_address_from_env(REGISTRY_OVERRIDE_ENV) {
        return addr;
    }
    SEPOLIA_REGISTRY
}

/// Resolve the Hats contract address to use for reads.
///
/// Honors environment overrides first, then falls back to the Sepolia canonical address.
pub fn hats_address() -> Address {
    if let Some(addr) = read_address_from_env(HATS_OVERRIDE_ENV) {
        return addr;
    }
    SEPOLIA_HATS
}

fn read_address_from_env(var: &str) -> Option<Address> {
    std::env::var(var).ok().and_then(|s| parse_address(&s))
}

fn parse_address(s: &str) -> Option<Address> {
    s.parse::<Address>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sepolia_addresses_are_well_formed() {
        assert_eq!(
            SEPOLIA_REGISTRY.to_string(),
            "0x45127C1c92741C0dA38e1A73fbb97a8a2C46770f"
        );
        assert_eq!(
            SEPOLIA_HATS.to_string(),
            "0x3bc1A0Ad72417f2d411118085256fC53CBdDd137"
        );
    }

    #[test]
    fn parse_address_accepts_valid_address_and_rejects_garbage() {
        assert!(parse_address("not-an-address").is_none());
        assert_eq!(
            parse_address("0x0000000000000000000000000000000000000001"),
            Some(Address::new(alloy::hex!(
                "0000000000000000000000000000000000000001"
            )))
        );
    }
}
