//! Arkiv precompile — thin adapter over [`arkiv_entitydb`].
//!
//! Registered by [`crate::evm::ArkivOpEvmFactory`] at
//! [`ARKIV_PRECOMPILE_ADDRESS`](arkiv_genesis::ARKIV_PRECOMPILE_ADDRESS).
//! `EntityRegistry.execute()` calls this with `abi.encode(OpRecord[])`
//! as calldata; we:
//!
//! 1. Enforce caller restrictions (direct CALL from `EntityRegistry`,
//!    non-static, zero value).
//! 2. Decode the batched `OpRecord[]`.
//! 3. Compute gas as a pure function of op shape and charge it up-front.
//! 4. For each record, convert the ABI types into [`arkiv_entitydb`]
//!    types and dispatch to the corresponding op handler via a
//!    [`RevmStateAdapter`] over revm's [`EvmInternals`].
//!
//! All indexing logic (system counter, ID maps, bitmap deltas across
//! built-in and user annotations, RLP encode/decode, tombstoning) lives
//! in [`arkiv_entitydb`] — this file is the revm-side adapter only.

use alloy_evm::{EvmInternals, precompiles::{DynPrecompile, PrecompileInput}};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::{SolValue, sol};
use arkiv_entitydb::{NumericAnnotation, StateAdapter, StringAnnotation};
use arkiv_genesis::ENTITY_REGISTRY_ADDRESS;
use revm::{
    precompile::{
        PrecompileError, PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult,
    },
    state::Bytecode,
};

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

// Op-type tags — must match `Entity.CREATE..Entity.EXPIRE` (1-indexed)
// in contracts/src/EntityRegistry.sol.
const OP_CREATE: u8 = 1;
const OP_UPDATE: u8 = 2;
const OP_EXTEND: u8 = 3;
const OP_TRANSFER: u8 = 4;
const OP_DELETE: u8 = 5;
const OP_EXPIRE: u8 = 6;

// Attribute valueType tags — must match `Entity.ATTR_UINT..ATTR_ENTITY_KEY`.
const ATTR_UINT: u8 = 1;
const ATTR_STRING: u8 = 2;
const ATTR_ENTITY_KEY: u8 = 3;

// ── Gas model ────────────────────────────────────────────────────────
//
// Pure function of op shape — required for cross-node consensus on the
// returned `gas_used`. Anchors:
//   - SSTORE_INIT  ≈ 22,100
//   - SSTORE_RESET ≈  5,000
//   - per code byte ≈ 200
//
// Per-op base costs cover the fixed state touches:
//   - CREATE writes counter + 2 ID-map slots + 7 built-in bitmaps + entity RLP
//   - DELETE/EXPIRE clears 2 ID-map slots + 7 built-in bitmaps + tombstone
//   - UPDATE rewrites the entity RLP + diffs the $contentType bitmap
//   - EXTEND / TRANSFER rewrite the entity RLP + swap a single bitmap
//
// Per-byte costs cover payload + annotation key/value bytes that land in
// account code; per-annotation cost amortises the extra bitmap touch.

const G_CREATE: u64 = 80_000;
const G_UPDATE: u64 = 30_000;
const G_EXTEND: u64 = 25_000;
const G_TRANSFER: u64 = 25_000;
const G_DELETE: u64 = 50_000;
const G_EXPIRE: u64 = 50_000;
const G_BYTE: u64 = 16;
const G_ANNOTATION: u64 = 5_000;

const PRECOMPILE_NAME: &str = "ARKIV";

/// Build the Arkiv precompile.
///
/// Returns a [`DynPrecompile`] suitable for insertion into a
/// `PrecompilesMap` via [`crate::evm::ArkivOpEvmFactory`].
pub fn arkiv_precompile() -> DynPrecompile {
    let id = PrecompileId::custom(PRECOMPILE_NAME);
    let call = move |mut input: PrecompileInput<'_>| -> PrecompileResult {
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

        // ── Gas: pure function of op shape ──────────────────────────

        let gas_used = total_gas(&records);
        if gas_used > input.gas {
            return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, input.reservoir));
        }

        // ── Dispatch into entitydb ──────────────────────────────────

        let current_block: u64 = input.internals.block_number().saturating_to();
        let mut adapter = RevmStateAdapter::new(&mut input.internals);

        for (i, rec) in records.into_iter().enumerate() {
            if let Err(e) = dispatch(&mut adapter, current_block, &rec) {
                return Err(PrecompileError::Fatal(format!(
                    "arkiv precompile: op #{i} ({}) failed: {e}",
                    op_name(rec.operationType),
                )));
            }
        }

        Ok(PrecompileOutput::new(gas_used, Bytes::new(), input.reservoir))
    };
    DynPrecompile::new_stateful(id, call)
}

// ── Op dispatch ──────────────────────────────────────────────────────

fn dispatch<S: StateAdapter>(
    state: &mut S,
    current_block: u64,
    rec: &OpRecord,
) -> eyre::Result<()> {
    match rec.operationType {
        OP_CREATE => {
            let (string_annotations, numeric_annotations) = convert_attributes(&rec.attributes)?;
            arkiv_entitydb::create(
                state,
                rec.sender,
                rec.entityKey,
                rec.newExpiresAt as u64,
                current_block,
                rec.payload.to_vec(),
                mime128_to_bytes(&rec.contentType),
                string_annotations,
                numeric_annotations,
            )
        }
        OP_UPDATE => {
            let (string_annotations, numeric_annotations) = convert_attributes(&rec.attributes)?;
            arkiv_entitydb::update(
                state,
                rec.entityKey,
                rec.payload.to_vec(),
                mime128_to_bytes(&rec.contentType),
                string_annotations,
                numeric_annotations,
            )
        }
        OP_EXTEND => arkiv_entitydb::extend(state, rec.entityKey, rec.newExpiresAt as u64),
        OP_TRANSFER => arkiv_entitydb::transfer(state, rec.entityKey, rec.newOwner),
        OP_DELETE => arkiv_entitydb::delete(state, rec.entityKey),
        OP_EXPIRE => arkiv_entitydb::expire(state, rec.entityKey),
        t => Err(eyre::eyre!("unknown operationType {t}")),
    }
}

fn op_name(t: u8) -> &'static str {
    match t {
        OP_CREATE => "CREATE",
        OP_UPDATE => "UPDATE",
        OP_EXTEND => "EXTEND",
        OP_TRANSFER => "TRANSFER",
        OP_DELETE => "DELETE",
        OP_EXPIRE => "EXPIRE",
        _ => "UNKNOWN",
    }
}

// ── Gas accounting ───────────────────────────────────────────────────

fn total_gas(records: &[OpRecord]) -> u64 {
    records
        .iter()
        .map(record_gas)
        .fold(0u64, u64::saturating_add)
}

fn record_gas(rec: &OpRecord) -> u64 {
    let base = match rec.operationType {
        OP_CREATE => G_CREATE,
        OP_UPDATE => G_UPDATE,
        OP_EXTEND => G_EXTEND,
        OP_TRANSFER => G_TRANSFER,
        OP_DELETE => G_DELETE,
        OP_EXPIRE => G_EXPIRE,
        // Unknown op-types still get charged so a malformed batch can't
        // dodge gas; dispatch will fatal anyway.
        _ => G_CREATE,
    };

    // EXTEND / TRANSFER / DELETE / EXPIRE don't read payload or
    // attributes from the record — gas is only the fixed base.
    if !matches!(rec.operationType, OP_CREATE | OP_UPDATE) {
        return base;
    }

    let payload_bytes = rec.payload.len() as u64;
    let annotation_count = rec.attributes.len() as u64;
    // Each annotation's name (≤32 bytes) + value (≤128 bytes) lands in
    // both the entity RLP and a pair-account bitmap.
    let annotation_bytes = annotation_count.saturating_mul(32 + 128);

    base.saturating_add(payload_bytes.saturating_mul(G_BYTE))
        .saturating_add(annotation_bytes.saturating_mul(G_BYTE))
        .saturating_add(annotation_count.saturating_mul(G_ANNOTATION))
}

// ── ABI → entitydb conversions ───────────────────────────────────────

/// Concatenate a `bytes32[4]` into 128 bytes, then strip trailing `0x00`.
///
/// Strings written by the SDK are packed left-aligned into the four
/// words and zero-padded on the right; trimming gives the original
/// bytes back. Strings that legitimately end in `0x00` are not
/// supported by this encoding (which is fine — `0x00` is banned in
/// pair keys/values anyway).
fn pack_bytes32_4(words: &[alloy_primitives::FixedBytes<32>; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    for w in words {
        out.extend_from_slice(w.as_slice());
    }
    strip_trailing_zeros(out)
}

fn strip_trailing_zeros(mut v: Vec<u8>) -> Vec<u8> {
    while matches!(v.last(), Some(0)) {
        v.pop();
    }
    v
}

fn mime128_to_bytes(m: &Mime128) -> Vec<u8> {
    pack_bytes32_4(&m.data)
}

/// Strip trailing zeros from an `Ident32` (`bytes32`) annotation key.
fn ident32_to_bytes(name: B256) -> Vec<u8> {
    strip_trailing_zeros(name.0.to_vec())
}

fn convert_attributes(
    attrs: &[Attribute],
) -> eyre::Result<(Vec<StringAnnotation>, Vec<NumericAnnotation>)> {
    let mut strings = Vec::new();
    let mut numerics = Vec::new();
    for a in attrs {
        let key = ident32_to_bytes(a.name);
        match a.valueType {
            ATTR_UINT => {
                // The SDK packs a uint256 into value[0] big-endian; the
                // remaining three words are zero. Treat the leading
                // 32-byte word as the value.
                numerics.push(NumericAnnotation {
                    key,
                    value: U256::from_be_slice(a.value[0].as_slice()),
                });
            }
            ATTR_STRING => {
                strings.push(StringAnnotation {
                    key,
                    value: pack_bytes32_4(&a.value),
                });
            }
            ATTR_ENTITY_KEY => {
                // Entity keys are exactly 32 bytes — store the leading
                // word verbatim (no trailing-zero strip; a key may
                // legitimately end in zeros).
                strings.push(StringAnnotation {
                    key,
                    value: a.value[0].as_slice().to_vec(),
                });
            }
            t => eyre::bail!("unknown attribute valueType {t}"),
        }
    }
    Ok((strings, numerics))
}

// ── revm-side state adapter ──────────────────────────────────────────

/// [`StateAdapter`] over revm's journaled state. Each call goes through
/// the journal so reverts roll back cleanly on dispatch failure.
struct RevmStateAdapter<'a, 'b> {
    internals: &'a mut EvmInternals<'b>,
}

impl<'a, 'b> RevmStateAdapter<'a, 'b> {
    fn new(internals: &'a mut EvmInternals<'b>) -> Self {
        Self { internals }
    }

    /// `set_code(..., Bytecode)` doesn't bump the nonce; new accounts
    /// land with `nonce = 0` which EIP-161 would prune. Force `nonce >=
    /// 1` so the account survives.
    fn ensure_nonce_at_least_one(&mut self, addr: Address) -> eyre::Result<()> {
        let nonce = self
            .internals
            .load_account_code(addr)
            .map_err(|e| eyre::eyre!("load_account_code({addr}): {e:?}"))?
            .data
            .nonce();
        if nonce == 0 {
            self.internals
                .bump_nonce(addr)
                .map_err(|e| eyre::eyre!("bump_nonce({addr}): {e:?}"))?;
        }
        Ok(())
    }
}

impl StateAdapter for RevmStateAdapter<'_, '_> {
    fn code(&mut self, addr: &Address) -> eyre::Result<Vec<u8>> {
        let load = self
            .internals
            .load_account_code(*addr)
            .map_err(|e| eyre::eyre!("load_account_code({addr}): {e:?}"))?;
        Ok(load
            .data
            .code()
            .map(|c| c.original_byte_slice().to_vec())
            .unwrap_or_default())
    }

    fn set_code(&mut self, addr: &Address, code: Vec<u8>) -> eyre::Result<()> {
        let bytecode = Bytecode::new_raw(Bytes::from(code));
        self.internals
            .set_code(*addr, bytecode)
            .map_err(|e| eyre::eyre!("set_code({addr}): {e:?}"))?;
        self.ensure_nonce_at_least_one(*addr)
    }

    fn tombstone_code(&mut self, addr: &Address) -> eyre::Result<()> {
        let bytecode = Bytecode::new_raw(Bytes::new());
        self.internals
            .set_code(*addr, bytecode)
            .map_err(|e| eyre::eyre!("set_code (tombstone, {addr}): {e:?}"))?;
        self.ensure_nonce_at_least_one(*addr)
    }

    fn storage(&mut self, addr: &Address, slot: B256) -> eyre::Result<B256> {
        let key = U256::from_be_bytes(slot.0);
        let load = self
            .internals
            .sload(*addr, key)
            .map_err(|e| eyre::eyre!("sload({addr}, {slot}): {e:?}"))?;
        Ok(B256::from(load.data.to_be_bytes()))
    }

    fn set_storage(&mut self, addr: &Address, slot: B256, value: B256) -> eyre::Result<()> {
        let key = U256::from_be_bytes(slot.0);
        let val = U256::from_be_bytes(value.0);
        self.internals
            .sstore(*addr, key, val)
            .map_err(|e| eyre::eyre!("sstore({addr}, {slot}): {e:?}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address as Addr, B256, FixedBytes};

    // End-to-end dispatch tests (state mutations against a live revm
    // context) live in `e2e/tests/precompile_e2e.rs` — `EvmInternals`
    // can't be constructed standalone.

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
    fn attr_type_constants_match_contract() {
        assert_eq!(ATTR_UINT, 1);
        assert_eq!(ATTR_STRING, 2);
        assert_eq!(ATTR_ENTITY_KEY, 3);
    }

    #[test]
    fn arkiv_precompile_constructs() {
        let _ = arkiv_precompile();
    }

    #[test]
    fn record_decodes_minimal_create_batch() {
        // Hand-construct a one-op CREATE record, abi-encode it as a
        // Vec<OpRecord>, then round-trip-decode.
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

    #[test]
    fn pack_bytes32_4_strips_trailing_zeros() {
        let mut w0 = [0u8; 32];
        w0[..10].copy_from_slice(b"text/plain");
        let m = Mime128 {
            data: [
                FixedBytes::from(w0),
                FixedBytes::ZERO,
                FixedBytes::ZERO,
                FixedBytes::ZERO,
            ],
        };
        assert_eq!(mime128_to_bytes(&m), b"text/plain".to_vec());
    }

    #[test]
    fn ident32_strips_trailing_zeros() {
        let mut buf = [0u8; 32];
        buf[..3].copy_from_slice(b"tag");
        assert_eq!(ident32_to_bytes(B256::from(buf)), b"tag".to_vec());
    }

    #[test]
    fn convert_uint_attribute_packs_be_word() {
        let mut name = [0u8; 32];
        name[..5].copy_from_slice(b"score");
        let mut val = [0u8; 32];
        val[31] = 42;
        let attrs = vec![Attribute {
            name: FixedBytes::from(name),
            valueType: ATTR_UINT,
            value: [
                FixedBytes::from(val),
                FixedBytes::ZERO,
                FixedBytes::ZERO,
                FixedBytes::ZERO,
            ],
        }];
        let (strings, numerics) = convert_attributes(&attrs).expect("convert");
        assert!(strings.is_empty());
        assert_eq!(numerics.len(), 1);
        assert_eq!(numerics[0].key, b"score".to_vec());
        assert_eq!(numerics[0].value, U256::from(42));
    }

    #[test]
    fn convert_string_attribute_strips_zeros() {
        let mut name = [0u8; 32];
        name[..3].copy_from_slice(b"tag");
        let mut val0 = [0u8; 32];
        val0[..5].copy_from_slice(b"music");
        let attrs = vec![Attribute {
            name: FixedBytes::from(name),
            valueType: ATTR_STRING,
            value: [
                FixedBytes::from(val0),
                FixedBytes::ZERO,
                FixedBytes::ZERO,
                FixedBytes::ZERO,
            ],
        }];
        let (strings, numerics) = convert_attributes(&attrs).expect("convert");
        assert!(numerics.is_empty());
        assert_eq!(strings.len(), 1);
        assert_eq!(strings[0].key, b"tag".to_vec());
        assert_eq!(strings[0].value, b"music".to_vec());
    }

    #[test]
    fn convert_entity_key_attribute_keeps_full_32_bytes() {
        let mut name = [0u8; 32];
        name[..3].copy_from_slice(b"ref");
        let val0 = [0x42u8; 32];
        let attrs = vec![Attribute {
            name: FixedBytes::from(name),
            valueType: ATTR_ENTITY_KEY,
            value: [
                FixedBytes::from(val0),
                FixedBytes::ZERO,
                FixedBytes::ZERO,
                FixedBytes::ZERO,
            ],
        }];
        let (strings, _) = convert_attributes(&attrs).expect("convert");
        assert_eq!(strings[0].value, vec![0x42u8; 32]);
    }

    #[test]
    fn record_gas_charges_per_op_correctly() {
        let mk = |op| OpRecord {
            operationType: op,
            sender: Addr::ZERO,
            entityKey: B256::ZERO,
            newOwner: Addr::ZERO,
            newExpiresAt: 0,
            payload: Bytes::new(),
            contentType: Mime128 { data: [FixedBytes::ZERO; 4] },
            attributes: vec![],
        };
        assert_eq!(record_gas(&mk(OP_CREATE)), G_CREATE);
        assert_eq!(record_gas(&mk(OP_UPDATE)), G_UPDATE);
        assert_eq!(record_gas(&mk(OP_EXTEND)), G_EXTEND);
        assert_eq!(record_gas(&mk(OP_TRANSFER)), G_TRANSFER);
        assert_eq!(record_gas(&mk(OP_DELETE)), G_DELETE);
        assert_eq!(record_gas(&mk(OP_EXPIRE)), G_EXPIRE);
    }
}
