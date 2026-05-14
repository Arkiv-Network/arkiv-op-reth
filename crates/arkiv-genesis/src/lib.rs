//! Genesis primitives for the Arkiv chain.
//!
//! Provides:
//! - the three canonical predeploy addresses (registry / precompile /
//!   system account, at `0x44…0044 / 0045 / 0046`),
//! - the `EntityRegistry` runtime bytecode (read at build time from
//!   `contracts/artifacts/EntityRegistry.runtime.hex` — kept in sync
//!   with the Solidity source in `contracts/src/` by `just contracts-build`),
//! - a convenience helper that builds the genesis `alloc` for a
//!   self-contained Arkiv dev chain.
//!
//! Used by:
//!   - `arkiv-node`: chainspec assembly at startup or build time,
//!     predeploy detection.
//!   - `arkiv-cli inject-predeploy`: post-processing op-deployer output
//!     to splice the predeploys into a standard OP genesis JSON.

// Re-export so consumers (e.g. `arkiv-cli inject-predeploy`) don't need to
// take a direct dep on alloy-genesis.
pub use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, Bytes, U256};
use alloy_signer_local::{MnemonicBuilder, PrivateKeySigner, coins_bip39::English};
use eyre::{Result, bail};
use std::collections::BTreeMap;

/// Canonical predeploy address for `EntityRegistry`.
pub const ENTITY_REGISTRY_ADDRESS: Address = Address::new([
    0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x44,
]);

/// Address at which `arkiv-op-reth`'s custom `EvmFactory` registers
/// the Arkiv precompile. Not a Solidity contract — a native Rust
/// precompile invoked by `EntityRegistry` via a `CALL`.
pub const ARKIV_PRECOMPILE_ADDRESS: Address = Address::new([
    0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x45,
]);

/// Singleton account that holds the global entity counter and the
/// trie-committed ID ↔ address maps. Re-exported from
/// [`arkiv_entitydb`] — the state-model crate is the canonical home
/// of the system-account address. Re-export here so consumers that
/// only care about genesis allocs (`arkiv-cli inject-predeploy`,
/// `arkiv-node`'s predeploy detection) don't need to depend on
/// `arkiv-entitydb`.
pub use arkiv_entitydb::SYSTEM_ACCOUNT_ADDRESS;

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

/// Runtime bytecode for `EntityRegistry`, baked in at build time from
/// `contracts/artifacts/EntityRegistry.runtime.hex`. Refresh via
/// `just contracts-build` after editing `contracts/src/EntityRegistry.sol`.
const ENTITY_REGISTRY_RUNTIME_HEX: &str =
    include_str!("../../../contracts/artifacts/EntityRegistry.runtime.hex");

/// Runtime bytecode for the `EntityRegistry` predeploy.
///
/// Chain-id-independent — the contract reads `block.chainid` at runtime
/// rather than baking it into a constructor immutable, so the same
/// bytes work on every chain.
pub fn entity_registry_runtime_code() -> Result<Bytes> {
    let trimmed = ENTITY_REGISTRY_RUNTIME_HEX.trim();
    let stripped = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    let bytes = hex::decode(stripped)
        .map_err(|e| eyre::eyre!("invalid EntityRegistry runtime hex: {}", e))?;
    if bytes.is_empty() {
        bail!("EntityRegistry runtime bytecode is empty; run `just contracts-build`");
    }
    Ok(Bytes::from(bytes))
}

/// Build the complete Arkiv dev alloc: the EntityRegistry predeploy +
/// the system account + [`ARKIV_DEV_ACCOUNT_COUNT`] mnemonic-derived
/// accounts each prefunded with [`arkiv_dev_balance_wei`].
pub fn genesis_alloc() -> Result<BTreeMap<Address, GenesisAccount>> {
    let mut alloc = BTreeMap::new();
    for (addr, acc) in dev_funding_alloc(ARKIV_DEV_ACCOUNT_COUNT, arkiv_dev_balance_wei())? {
        alloc.insert(addr, acc);
    }
    alloc.insert(ENTITY_REGISTRY_ADDRESS, entity_registry_account()?);
    alloc.insert(SYSTEM_ACCOUNT_ADDRESS, system_account());
    Ok(alloc)
}

/// `GenesisAccount` for the `EntityRegistry` predeploy. Suitable for
/// splicing into any external genesis JSON.
pub fn entity_registry_account() -> Result<GenesisAccount> {
    Ok(GenesisAccount {
        code: Some(entity_registry_runtime_code()?),
        ..Default::default()
    })
}

/// `GenesisAccount` for the system account. Empty code, empty storage,
/// `nonce = 1` so EIP-161 doesn't prune it before the precompile gets
/// a chance to write into it.
pub fn system_account() -> GenesisAccount {
    GenesisAccount {
        nonce: Some(1),
        ..Default::default()
    }
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

    #[test]
    fn entity_registry_runtime_code_decodes() {
        let code = entity_registry_runtime_code().expect("decode runtime hex");
        assert!(!code.is_empty());
        // Sanity: starts with the standard solc dispatcher prologue.
        assert_eq!(&code[..2], &[0x60, 0x80]);
    }

    #[test]
    fn genesis_alloc_includes_three_predeploys_and_funding() {
        let alloc = genesis_alloc().expect("alloc");
        assert!(alloc.contains_key(&ENTITY_REGISTRY_ADDRESS));
        assert!(alloc.contains_key(&SYSTEM_ACCOUNT_ADDRESS));
        assert_eq!(
            alloc.get(&SYSTEM_ACCOUNT_ADDRESS).unwrap().nonce,
            Some(1),
            "system account must have nonce=1 to survive EIP-161",
        );
        assert_eq!(alloc.len(), ARKIV_DEV_ACCOUNT_COUNT + 2);
    }
}
