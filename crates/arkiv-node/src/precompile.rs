//! Arkiv precompile — **stub**.
//!
//! Registered by [`crate::evm::ArkivOpEvmFactory`] at
//! [`ARKIV_PRECOMPILE_ADDRESS`](arkiv_genesis::ARKIV_PRECOMPILE_ADDRESS).
//! Called by `EntityRegistry.execute()` with `abi.encode(OpRecord[])`
//! as its calldata; today every per-op handler is a no-op that logs
//! the op and returns success. A later phase fills in:
//!
//! - content validation (payload caps, attribute formats, `0x00` ban)
//! - entity / pair / system-account state writes via `EvmInternals`
//! - `ArkivPairs` first-sight MDBX put
//! - roaring64 bitmap deser/ser
//! - `EntityRlp` encode/decode (including the v2 `owner` / `expires_at`
//!   fields that make the entity account self-sufficient for queries)
//! - real gas accounting (pure function of op shape, per design doc §5)
//!
//! The contract↔precompile ABI is mirrored from
//! `contracts/src/EntityRegistry.sol`'s `OpRecord` struct via `sol!`.
//! Field layouts must stay in lockstep across both sides.

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolValue, sol};
use arkiv_genesis::ENTITY_REGISTRY_ADDRESS;
use revm::precompile::{PrecompileError, PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult};

pub use arkiv_genesis::ARKIV_PRECOMPILE_ADDRESS;

// Mirror of `EntityRegistry.OpRecord` in contracts/src/EntityRegistry.sol.
// Field order, types, and names must match exactly — the contract calls
// `ARKIV_PRECOMPILE.call(abi.encode(records))` with the same schema.
sol! {
    #[derive(Debug)]
    struct OpRecord {
        uint8 operationType;
        address sender;
        bytes32 entityKey;
        address newOwner;
        uint32 newExpiresAt;     // BlockNumber32 UDVT in Solidity; uint32 on the wire
        bytes payload;
        Mime128 contentType;
        Attribute[] attributes;
    }

    #[derive(Debug)]
    struct Mime128 {
        bytes32[4] data;
    }

    #[derive(Debug)]
    struct Attribute {
        bytes32 name;            // Ident32 UDVT in Solidity; bytes32 on the wire
        uint8 valueType;
        bytes32[4] value;
    }
}

// Op-type tags — must match the constants in contracts/src/EntityRegistry.sol
// (`Entity.CREATE` .. `Entity.EXPIRE`, 1-indexed).
const OP_CREATE: u8 = 1;
const OP_UPDATE: u8 = 2;
const OP_EXTEND: u8 = 3;
const OP_TRANSFER: u8 = 4;
const OP_DELETE: u8 = 5;
const OP_EXPIRE: u8 = 6;

// Stub gas model — placeholder while the precompile is wired in. A later
// phase replaces this with the per-op formulas from design doc §5.
const STUB_BASE_GAS: u64 = 5_000;
const STUB_GAS_PER_OP: u64 = 1_000;

const PRECOMPILE_NAME: &str = "ARKIV";

/// Build the stub Arkiv precompile.
///
/// Returns a [`DynPrecompile`] suitable for insertion into a
/// `PrecompilesMap`. Every state-mutation path is a no-op log; the
/// intent is to land the precompile↔contract ABI plumbing (calldata
/// decode, caller restriction, dispatch shape) so the real handlers
/// drop in later without churning the call site.
pub fn arkiv_precompile() -> DynPrecompile {
    let id = PrecompileId::custom(PRECOMPILE_NAME);
    let call = move |input: PrecompileInput<'_>| -> PrecompileResult {
        // ── Caller restriction ──────────────────────────────────────
        //
        // Direct call only, from the `EntityRegistry` predeploy.

        if input.target_address != input.bytecode_address {
            return Err(PrecompileError::Fatal(
                "arkiv precompile: DELEGATECALL/CALLCODE not allowed".into(),
            ));
        }
        if input.is_static {
            return Err(PrecompileError::Fatal(
                "arkiv precompile: STATICCALL not allowed".into(),
            ));
        }
        if input.value != U256::ZERO {
            return Err(PrecompileError::Fatal(
                "arkiv precompile: value-bearing call not allowed".into(),
            ));
        }
        if input.caller != ENTITY_REGISTRY_ADDRESS {
            return Err(PrecompileError::Fatal(format!(
                "arkiv precompile: only EntityRegistry ({}) may call; got {}",
                ENTITY_REGISTRY_ADDRESS, input.caller,
            )));
        }

        // ── Decode the batched OpRecord[] ───────────────────────────

        let records = match <Vec<OpRecord> as SolValue>::abi_decode(input.data) {
            Ok(r) => r,
            Err(e) => {
                return Err(PrecompileError::Fatal(format!(
                    "arkiv precompile: failed to decode OpRecord[]: {e}"
                )));
            }
        };

        let gas_used = STUB_BASE_GAS + STUB_GAS_PER_OP * records.len() as u64;
        if gas_used > input.gas {
            return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, input.reservoir));
        }

        // ── Dispatch ────────────────────────────────────────────────

        for (i, rec) in records.iter().enumerate() {
            match rec.operationType {
                OP_CREATE => handle_create(i, rec),
                OP_UPDATE => handle_update(i, rec),
                OP_EXTEND => handle_extend(i, rec),
                OP_TRANSFER => handle_transfer(i, rec),
                OP_DELETE => handle_delete(i, rec),
                OP_EXPIRE => handle_expire(i, rec),
                t => {
                    return Err(PrecompileError::Fatal(format!(
                        "arkiv precompile: unknown operationType {t} in record #{i}"
                    )));
                }
            }
        }

        Ok(PrecompileOutput::new(gas_used, Bytes::new(), input.reservoir))
    };
    DynPrecompile::new_stateful(id, call)
}

// ── Per-op stub handlers ─────────────────────────────────────────────
//
// Each takes the record's index in the batch (for diagnostic logging)
// and the decoded record. A later phase replaces these bodies with the
// real state mutations.

fn handle_create(i: usize, rec: &OpRecord) {
    tracing::debug!(
        index = i,
        entity_key = %rec.entityKey,
        sender = %rec.sender,
        new_owner = %rec.newOwner,
        expires_at = rec.newExpiresAt,
        payload_len = rec.payload.len(),
        attr_count = rec.attributes.len(),
        "arkiv precompile (stub): CREATE"
    );
    // TODO: bump system.entity_count; write ID maps; create entity
    // account; for each annotation (incl. built-ins $all / $creator /
    // $createdAtBlock / $owner / $key / $expiration / $contentType) do
    // the first-sight ArkivPairs put + bitmap insert; encode the
    // EntityRlp (with owner + expires_at) and SetCode the entity.
    let _ = rec;
}

fn handle_update(i: usize, rec: &OpRecord) {
    tracing::debug!(
        index = i,
        entity_key = %rec.entityKey,
        sender = %rec.sender,
        payload_len = rec.payload.len(),
        attr_count = rec.attributes.len(),
        "arkiv precompile (stub): UPDATE"
    );
    // TODO: read entity_id from system.addr_to_id; decode old
    // EntityRlp; diff annotations; remove ID from bitmaps that dropped,
    // add to bitmaps that appeared; re-encode RLP preserving owner /
    // expires_at / creator / created_at_block / key from the old one.
    let _ = rec;
}

fn handle_extend(i: usize, rec: &OpRecord) {
    tracing::debug!(
        index = i,
        entity_key = %rec.entityKey,
        sender = %rec.sender,
        new_expires_at = rec.newExpiresAt,
        "arkiv precompile (stub): EXTEND"
    );
    // TODO: decode old EntityRlp to recover old expires_at; remove ID
    // from $expiration=old bitmap; add ID to $expiration=newExpiresAt
    // bitmap (first-sight ArkivPairs put if new); re-encode EntityRlp
    // with expires_at=newExpiresAt, everything else preserved.
    let _ = rec;
}

fn handle_transfer(i: usize, rec: &OpRecord) {
    tracing::debug!(
        index = i,
        entity_key = %rec.entityKey,
        sender = %rec.sender,
        new_owner = %rec.newOwner,
        "arkiv precompile (stub): TRANSFER"
    );
    // TODO: decode old EntityRlp to recover old owner; remove ID from
    // $owner=old bitmap; add ID to $owner=newOwner bitmap (first-sight
    // ArkivPairs put if new); re-encode EntityRlp with owner=newOwner,
    // everything else preserved.
    let _ = rec;
}

fn handle_delete(i: usize, rec: &OpRecord) {
    tracing::debug!(
        index = i,
        entity_key = %rec.entityKey,
        sender = %rec.sender,
        "arkiv precompile (stub): DELETE"
    );
    // TODO: read entity_id; decode old EntityRlp; for every annotation
    // (incl. built-ins recovered from RLP — $owner=old_owner,
    // $expiration=old_expires_at) remove ID from the bitmap; clear both
    // system-account ID slots; SetCode(entity, nil) keeping nonce=1.
    let _ = rec;
}

fn handle_expire(i: usize, rec: &OpRecord) {
    tracing::debug!(
        index = i,
        entity_key = %rec.entityKey,
        sender = %rec.sender,
        "arkiv precompile (stub): EXPIRE"
    );
    // TODO: identical state path to handle_delete — the contract has
    // already validated `block.number > expiresAt`.
    let _ = rec;
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address as Addr, B256, FixedBytes};

    // End-to-end dispatch tests (caller restriction in a live revm
    // context) land in a later phase — `PrecompileInput::internals` is
    // an EVM-internal handle that can't be constructed standalone.

    #[test]
    fn op_type_constants_match_contract() {
        assert_eq!(OP_CREATE, 1);
        assert_eq!(OP_UPDATE, 2);
        assert_eq!(OP_EXTEND, 3);
        assert_eq!(OP_TRANSFER, 4);
        assert_eq!(OP_DELETE, 5);
        assert_eq!(OP_EXPIRE, 6);
    }

    #[test]
    fn arkiv_precompile_constructs() {
        // Smoke test — confirms the constructor produces a value
        // without panicking. Behavioural tests are deferred.
        let _ = arkiv_precompile();
    }

    #[test]
    fn record_decodes_minimal_create_batch() {
        // Hand-construct a one-op CREATE record, abi-encode it as a
        // Vec<OpRecord>, then round-trip-decode. Confirms the sol!
        // layout matches what the contract emits.
        let rec = OpRecord {
            operationType: OP_CREATE,
            sender: Addr::repeat_byte(0xaa),
            entityKey: B256::repeat_byte(0xbb),
            newOwner: Addr::repeat_byte(0xaa),
            newExpiresAt: 12345,
            payload: vec![0u8; 8].into(),
            contentType: Mime128 { data: [FixedBytes::ZERO; 4] },
            attributes: vec![],
        };
        let encoded = <Vec<OpRecord> as SolValue>::abi_encode(&vec![rec.clone()]);
        let decoded = <Vec<OpRecord> as SolValue>::abi_decode(&encoded)
            .expect("round-trip decode");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].operationType, OP_CREATE);
        assert_eq!(decoded[0].entityKey, rec.entityKey);
        assert_eq!(decoded[0].newExpiresAt, 12345);
    }
}
