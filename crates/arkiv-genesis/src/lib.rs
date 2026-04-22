//! Genesis generation for the Arkiv chain.
//!
//! Uses creation bytecode from arkiv-bindings (embedded at build time via forge build)
//! and deploys in revm to produce runtime bytecode with populated immutables.

mod deploy;

use alloy_genesis::{ChainConfig, Genesis, GenesisAccount};
use alloy_primitives::{Address, U256};
use arkiv_bindings::ENTITY_REGISTRY_CREATION_CODE;
use eyre::Result;
use std::collections::BTreeMap;

use deploy::deploy;

/// Default predeploy address for EntityRegistry.
pub const ENTITY_REGISTRY_ADDRESS: Address =
    Address::new([0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x42]);

/// Configuration for genesis generation.
pub struct GenesisConfig {
    /// Chain ID. Flows into revm deployment context and genesis chain config.
    pub chain_id: u64,
    /// Address to place EntityRegistry at in genesis.
    pub predeploy_address: Address,
    /// Prefunded accounts with their balances. Empty by default.
    pub prefunded_accounts: Vec<(Address, U256)>,
    /// Gas limit for the genesis block.
    pub gas_limit: u64,
}

impl Default for GenesisConfig {
    fn default() -> Self {
        Self {
            chain_id: 1337,
            predeploy_address: ENTITY_REGISTRY_ADDRESS,
            prefunded_accounts: Vec::new(),
            gas_limit: 30_000_000,
        }
    }
}

/// Build a chain config with all forks activated at genesis (block 0 / timestamp 0).
pub fn all_forks_active(chain_id: u64) -> ChainConfig {
    ChainConfig {
        chain_id,
        homestead_block: Some(0),
        dao_fork_support: true,
        eip150_block: Some(0),
        eip155_block: Some(0),
        eip158_block: Some(0),
        byzantium_block: Some(0),
        constantinople_block: Some(0),
        petersburg_block: Some(0),
        istanbul_block: Some(0),
        berlin_block: Some(0),
        london_block: Some(0),
        terminal_total_difficulty: Some(U256::ZERO),
        terminal_total_difficulty_passed: true,
        shanghai_time: Some(0),
        cancun_time: Some(0),
        prague_time: Some(0),
        osaka_time: Some(0),
        ..Default::default()
    }
}

/// Generate a complete genesis with EntityRegistry predeployed.
pub fn generate_genesis(config: &GenesisConfig) -> Result<Genesis> {
    let creation_code = hex::decode(ENTITY_REGISTRY_CREATION_CODE)
        .map_err(|e| eyre::eyre!("invalid creation bytecode hex: {}", e))?;

    let runtime_bytecode = deploy(&creation_code, config.chain_id)?;

    let mut alloc = BTreeMap::new();

    for (addr, balance) in &config.prefunded_accounts {
        alloc.insert(
            *addr,
            GenesisAccount {
                balance: *balance,
                ..Default::default()
            },
        );
    }

    alloc.insert(
        config.predeploy_address,
        GenesisAccount {
            code: Some(runtime_bytecode),
            ..Default::default()
        },
    );

    Ok(Genesis {
        config: all_forks_active(config.chain_id),
        gas_limit: config.gas_limit,
        alloc,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_genesis_with_predeploy() {
        let genesis = generate_genesis(&GenesisConfig::default()).unwrap();

        let account = &genesis.alloc[&ENTITY_REGISTRY_ADDRESS];
        assert!(account.code.is_some());
        assert!(!account.code.as_ref().unwrap().is_empty());
        assert_eq!(genesis.alloc.len(), 1);
    }

    #[test]
    fn prefunded_accounts_included() {
        let addr: Address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
            .parse()
            .unwrap();
        let balance = U256::from(1_000_000u64) * U256::from(10u64).pow(U256::from(18));

        let config = GenesisConfig {
            prefunded_accounts: vec![(addr, balance)],
            ..Default::default()
        };
        let genesis = generate_genesis(&config).unwrap();

        assert_eq!(genesis.alloc.len(), 2);
        assert_eq!(genesis.alloc[&addr].balance, balance);
    }

    #[test]
    fn chain_id_flows_through() {
        let config = GenesisConfig {
            chain_id: 42161,
            ..Default::default()
        };
        let genesis = generate_genesis(&config).unwrap();
        assert_eq!(genesis.config.chain_id, 42161);
        assert_eq!(genesis.config.shanghai_time, Some(0));
        assert_eq!(genesis.config.cancun_time, Some(0));
        assert_eq!(genesis.config.prague_time, Some(0));
    }
}
