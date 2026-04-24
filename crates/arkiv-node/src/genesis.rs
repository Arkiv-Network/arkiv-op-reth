//! Genesis allocations for the Arkiv chain.
//!
//! Consolidates the logic from the standalone arkiv-genesis crate.
//! Uses creation bytecode from arkiv-bindings and deploys via revm
//! to produce runtime bytecode with populated immutables.

use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, Bytes, U256};
use arkiv_bindings::ENTITY_REGISTRY_CREATION_CODE;
use eyre::{Result, bail, ensure};
use revm::{
    MainContext,
    context::{Context, TxEnv},
    database::{CacheDB, EmptyDB},
    handler::{ExecuteEvm, MainBuilder},
    primitives::TxKind,
    state::AccountInfo,
};
use std::collections::BTreeMap;

/// Predeploy address for EntityRegistry.
pub const ENTITY_REGISTRY_ADDRESS: Address = Address::new([
    0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x42,
]);

/// Hardhat test mnemonic first account.
pub const DEV_ADDRESS: Address = Address::new([
    0xf3, 0x9F, 0xd6, 0xe5, 0x1a, 0xad, 0x88, 0xF6, 0xF4, 0xce, 0x6a, 0xB8, 0x82, 0x72, 0x79, 0xcf,
    0xff, 0xb9, 0x22, 0x66,
]);

const GAS_LIMIT: u64 = 30_000_000;

const DEPLOYER: Address = Address::new([
    0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
]);

/// Execute creation bytecode in revm and return the runtime bytecode.
fn deploy_creation_code(creation_code: &[u8], chain_id: u64) -> Result<Bytes> {
    let mut db = CacheDB::<EmptyDB>::default();
    db.insert_account_info(
        DEPLOYER,
        AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000u128),
            nonce: 0,
            ..Default::default()
        },
    );

    let ctx = Context::mainnet()
        .with_db(db)
        .modify_cfg_chained(|cfg| {
            cfg.chain_id = chain_id;
        })
        .modify_block_chained(|block| {
            block.number = U256::ZERO;
            block.gas_limit = GAS_LIMIT;
        });

    let mut evm = ctx.build_mainnet();

    let tx = TxEnv::builder()
        .caller(DEPLOYER)
        .kind(TxKind::Create)
        .data(Bytes::copy_from_slice(creation_code))
        .gas_limit(15_000_000)
        .gas_price(0)
        .nonce(0)
        .chain_id(Some(chain_id))
        .build()
        .map_err(|e| eyre::eyre!("failed to build tx: {:?}", e))?;

    let result = evm
        .transact(tx)
        .map_err(|e| eyre::eyre!("revm execution failed: {:?}", e))?;

    ensure!(
        result.result.is_success(),
        "contract deployment failed: {:?}",
        result.result
    );

    let deployed_addr = result
        .result
        .created_address()
        .ok_or_else(|| eyre::eyre!("contract creation did not return an address"))?;

    let account = result
        .state
        .get(&deployed_addr)
        .ok_or_else(|| eyre::eyre!("deployed account not found in state"))?;

    let bytecode = account
        .info
        .code
        .as_ref()
        .ok_or_else(|| eyre::eyre!("no code on deployed account"))?;

    let runtime_bytes = bytecode.bytes();
    if runtime_bytes.is_empty() {
        bail!("deployed contract has empty bytecode");
    }

    Ok(runtime_bytes.clone())
}

/// Return genesis alloc entries for the EntityRegistry predeploy and dev account.
///
/// `chain_id` is forwarded to revm so the constructor's `block.chainid` read
/// (used for the EIP-712 domain separator) matches the target chain.
pub fn genesis_alloc(chain_id: u64) -> Result<BTreeMap<Address, GenesisAccount>> {
    let creation_code = hex::decode(ENTITY_REGISTRY_CREATION_CODE)
        .map_err(|e| eyre::eyre!("invalid creation bytecode hex: {}", e))?;

    let runtime_bytecode = deploy_creation_code(&creation_code, chain_id)?;

    let dev_balance = U256::from(10_000u64) * U256::from(1_000_000_000_000_000_000u128);

    let mut alloc = BTreeMap::new();

    alloc.insert(
        DEV_ADDRESS,
        GenesisAccount {
            balance: dev_balance,
            ..Default::default()
        },
    );

    alloc.insert(
        ENTITY_REGISTRY_ADDRESS,
        GenesisAccount {
            code: Some(runtime_bytecode),
            ..Default::default()
        },
    );

    Ok(alloc)
}
