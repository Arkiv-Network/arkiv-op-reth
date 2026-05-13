//! Predeploy detection for Arkiv chainspecs.

use alloy_primitives::keccak256;
use reth_optimism_chainspec::OpChainSpec;

/// Returns `true` iff the chainspec's genesis alloc contains the Arkiv
/// `EntityRegistry` predeploy at the canonical address with bytecode
/// equal to the in-tree runtime form
/// (`contracts/artifacts/EntityRegistry.runtime.hex`).
///
/// The bytecode hash check (rather than mere address presence) guards
/// against squatting at `0x44…0044` with unrelated code.
pub fn has_arkiv_predeploy(chain: &OpChainSpec) -> bool {
    let Some(account) = chain
        .inner
        .genesis
        .alloc
        .get(&arkiv_genesis::ENTITY_REGISTRY_ADDRESS)
    else {
        return false;
    };
    let Some(code) = &account.code else {
        return false;
    };
    let Ok(expected) = arkiv_genesis::entity_registry_runtime_code() else {
        return false;
    };
    keccak256(code) == keccak256(&expected)
}
