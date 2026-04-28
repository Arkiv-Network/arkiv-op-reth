//! Genesis primitives for the Arkiv chain.
//!
//! Provides the canonical EntityRegistry predeploy address, the runtime
//! bytecode generator (which executes the bindings' creation code in revm
//! to populate constructor immutables for a given chain ID), and a
//! convenience helper that builds the genesis `alloc` for a self-contained
//! Arkiv dev chain.
//!
//! Used by:
//!   - `arkiv-node`: chainspec assembly at startup or build time.
//!   - `arkiv-cli inject-predeploy`: post-processing op-deployer output to
//!     splice the predeploy into a standard OP genesis JSON.

// Re-export so consumers (e.g. `arkiv-cli inject-predeploy`) don't need to
// take a direct dep on alloy-genesis.
pub use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, Bytes, U256};
use alloy_signer_local::{MnemonicBuilder, PrivateKeySigner, coins_bip39::English};
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

/// Canonical predeploy address for `EntityRegistry`.
pub const ENTITY_REGISTRY_ADDRESS: Address = Address::new([
    0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x44,
]);

/// First account derived from [`ARKIV_DEV_MNEMONIC`] at standard BIP-44
/// path `m/44'/60'/0'/0/0`. Kept as a `const` so callers that only need
/// the well-known dev address don't have to derive at runtime.
///
/// Verified by [`tests::dev_address_matches_first_signer`].
pub const DEV_ADDRESS: Address = Address::new([
    0xf3, 0x9F, 0xd6, 0xe5, 0x1a, 0xad, 0x88, 0xF6, 0xF4, 0xce, 0x6a, 0xB8, 0x82, 0x72, 0x79, 0xcf,
    0xff, 0xb9, 0x22, 0x66,
]);

/// Hardhat-compatible test mnemonic. The first 20 derived addresses match
/// the standard hardhat / foundry / anvil defaults; subsequent indices
/// (20..[`ARKIV_DEV_ACCOUNT_COUNT`]) are deterministic but novel.
///
/// **Do not use in production.** This phrase is published in every
/// JavaScript and Rust EVM testing toolkit.
pub const ARKIV_DEV_MNEMONIC: &str = "test test test test test test test test test test test junk";

/// Number of accounts derived from [`ARKIV_DEV_MNEMONIC`] and pre-funded
/// in the dev chainspec. Caps the simulator's signer pool size.
pub const ARKIV_DEV_ACCOUNT_COUNT: usize = 100;

/// Default per-account balance for [`dev_funding_alloc`]: 10,000 ETH.
pub fn arkiv_dev_balance_wei() -> U256 {
    U256::from(10_000u64) * U256::from(1_000_000_000_000_000_000u128)
}

const GAS_LIMIT: u64 = 30_000_000;

/// Synthetic deployer for the genesis-time CREATE call. Doesn't matter — we
/// don't keep the deployment, just the resulting runtime bytecode.
const DEPLOYER: Address = Address::new([
    0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
]);

/// Run the EntityRegistry creation bytecode in revm at the given chain ID
/// and return the resulting runtime bytecode (with constructor immutables
/// populated, e.g. the EIP-712 cached domain separator).
///
/// Deterministic: same `chain_id` + same bindings rev always yields the
/// same bytes.
pub fn deploy_creation_code(chain_id: u64) -> Result<Bytes> {
    let creation_code = hex::decode(ENTITY_REGISTRY_CREATION_CODE)
        .map_err(|e| eyre::eyre!("invalid creation bytecode hex: {}", e))?;
    deploy_inner(&creation_code, chain_id)
}

fn deploy_inner(creation_code: &[u8], chain_id: u64) -> Result<Bytes> {
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

/// Build the complete Arkiv dev alloc: the EntityRegistry predeploy plus
/// [`ARKIV_DEV_ACCOUNT_COUNT`] mnemonic-derived accounts each prefunded
/// with [`arkiv_dev_balance_wei`].
///
/// `chain_id` is forwarded to revm so the EntityRegistry constructor's
/// `block.chainid` (used for the EIP-712 domain separator) matches the
/// target chain.
pub fn genesis_alloc(chain_id: u64) -> Result<BTreeMap<Address, GenesisAccount>> {
    let mut alloc = BTreeMap::new();
    for (addr, acc) in dev_funding_alloc(ARKIV_DEV_ACCOUNT_COUNT, arkiv_dev_balance_wei())? {
        alloc.insert(addr, acc);
    }
    alloc.insert(ENTITY_REGISTRY_ADDRESS, predeploy_account(chain_id)?);
    Ok(alloc)
}

/// Build a `GenesisAccount` for the EntityRegistry predeploy at the given
/// chain ID. Suitable for splicing into any external genesis JSON.
pub fn predeploy_account(chain_id: u64) -> Result<GenesisAccount> {
    Ok(GenesisAccount {
        code: Some(deploy_creation_code(chain_id)?),
        ..Default::default()
    })
}

/// Derive `count` `PrivateKeySigner`s from [`ARKIV_DEV_MNEMONIC`] at
/// standard BIP-44 paths `m/44'/60'/0'/0/{0..count}`.
///
/// The first 20 addresses match the well-known hardhat/foundry/anvil
/// defaults. Indices 20..100 are the same on every machine but aren't
/// part of any other tool's defaults.
pub fn dev_signers(count: usize) -> Result<Vec<PrivateKeySigner>> {
    if count > ARKIV_DEV_ACCOUNT_COUNT {
        bail!(
            "requested {} signers but only {} are funded in the dev chainspec",
            count,
            ARKIV_DEV_ACCOUNT_COUNT
        );
    }
    (0..count as u32)
        .map(|i| {
            MnemonicBuilder::<English>::default()
                .phrase(ARKIV_DEV_MNEMONIC)
                .index(i)
                .map_err(|e| eyre::eyre!("index {} invalid: {}", i, e))?
                .build()
                .map_err(|e| eyre::eyre!("signer {} build failed: {}", i, e))
        })
        .collect()
}

/// Derive `count` mnemonic addresses and pair each with a `GenesisAccount`
/// of `balance_wei` and no code/storage. Used by `arkiv-cli inject-funding`
/// and by [`genesis_alloc`].
pub fn dev_funding_alloc(
    count: usize,
    balance_wei: U256,
) -> Result<Vec<(Address, GenesisAccount)>> {
    Ok(dev_signers(count)?
        .into_iter()
        .map(|s| {
            (
                s.address(),
                GenesisAccount {
                    balance: balance_wei,
                    ..Default::default()
                },
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_address_matches_first_signer() {
        let signers = dev_signers(1).expect("derive");
        assert_eq!(signers[0].address(), DEV_ADDRESS);
    }

    #[test]
    fn dev_signers_count_capped() {
        assert!(dev_signers(ARKIV_DEV_ACCOUNT_COUNT + 1).is_err());
        assert!(dev_signers(ARKIV_DEV_ACCOUNT_COUNT).is_ok());
    }

    #[test]
    fn dev_funding_alloc_produces_count() {
        let alloc = dev_funding_alloc(5, arkiv_dev_balance_wei()).expect("alloc");
        assert_eq!(alloc.len(), 5);
        for (_, acc) in &alloc {
            assert_eq!(acc.balance, arkiv_dev_balance_wei());
        }
    }
}
