//! Address derivations, built-in annotation keys, and the byte
//! encodings used to construct pair/index account addresses.
//!
//! These three concerns are grouped because they all describe *how
//! annotations turn into bytes*: the addresses hash those bytes, the
//! `ANNOT_*` constants name them, and the encoders produce the
//! canonical byte form for the values.

use alloy_primitives::{Address, B256, keccak256};

// ─── Address derivations ──────────────────────────────────────────────

/// Entity-account address. Spec: `entity_address = entityKey[:20]`
/// (statedb-design §2.1). The address is a pure identity anchor;
/// content commitment is via `codeHash`.
#[inline]
pub fn entity_address(entity_key: B256) -> Address {
    Address::from_slice(&entity_key.0[..20])
}

/// Pair-account address. Spec: `pair_addr = keccak256("arkiv.pair" || k
/// || 0x00 || v)[:20]` (statedb-design §2.3). The `0x00` separator
/// prevents prefix collisions; annot keys and values must not contain
/// `0x00` (precompile enforces).
pub fn pair_address(annot_key: &[u8], annot_val: &[u8]) -> Address {
    let mut buf = Vec::with_capacity(b"arkiv.pair".len() + annot_key.len() + 1 + annot_val.len());
    buf.extend_from_slice(b"arkiv.pair");
    buf.extend_from_slice(annot_key);
    buf.push(0x00);
    buf.extend_from_slice(annot_val);
    Address::from_slice(&keccak256(buf).0[..20])
}

/// Index-account address. Spec: `index_address(k) =
/// keccak256("arkiv.index" || k)[:20]` (2_state-model §2, Index
/// Accounts). Namespace is disjoint from `"arkiv.pair"`.
pub fn index_address(attr_key: &[u8]) -> Address {
    let mut buf = Vec::with_capacity(b"arkiv.index".len() + attr_key.len());
    buf.extend_from_slice(b"arkiv.index");
    buf.extend_from_slice(attr_key);
    Address::from_slice(&keccak256(buf).0[..20])
}

// ─── Built-in annotation keys ─────────────────────────────────────────
//
// Every entity carries these implicit pairs in addition to its
// user-supplied annotations. The op handlers derive them from the op
// inputs (no caller input needed).

/// Universal "every entity" annotation — every entity is in
/// `("$all", "")`'s bitmap. Lets clients enumerate all entities via a
/// single bitmap read.
pub const ANNOT_ALL: &[u8] = b"$all";

/// `("$creator", creator_address)` — set on `Create`, immutable.
pub const ANNOT_CREATOR: &[u8] = b"$creator";

/// `("$createdAtBlock", be_block_number)` — set on `Create`, immutable.
pub const ANNOT_CREATED_AT_BLOCK: &[u8] = b"$createdAtBlock";

/// `("$owner", owner_address)` — set on `Create`, mutated on
/// `Transfer`.
pub const ANNOT_OWNER: &[u8] = b"$owner";

/// `("$key", entityKey)` — set on `Create`, immutable.
pub const ANNOT_KEY: &[u8] = b"$key";

/// `("$expiration", be_block_number)` — set on `Create`, mutated on
/// `Extend`. Encoded as fixed-width big-endian uint64 so lex order
/// matches numeric order (needed for range scans).
pub const ANNOT_EXPIRATION: &[u8] = b"$expiration";

/// `("$contentType", content_type_bytes)` — set on `Create`, mutated on
/// `Update`.
pub const ANNOT_CONTENT_TYPE: &[u8] = b"$contentType";

// ─── Annotation value encodings (for pair-account addresses) ─────────
//
// Encoding choices are critical: lex order of these byte sequences
// must match the intended ordering for range queries. For numeric
// values (block numbers, uint annotations) that means fixed-width
// big-endian. For addresses it doesn't matter (range queries on
// addresses don't make sense); we use the natural 20-byte form.

#[inline]
pub(super) fn encode_u64_be(n: u64) -> Vec<u8> {
    n.to_be_bytes().to_vec()
}

#[inline]
pub(super) fn encode_address(addr: Address) -> Vec<u8> {
    addr.as_slice().to_vec()
}

#[inline]
pub(super) fn encode_b256(b: B256) -> Vec<u8> {
    b.0.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;

    #[test]
    fn entity_address_truncates_to_first_20_bytes() {
        let key = b256!("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff");
        assert_eq!(entity_address(key).as_slice(), &key.0[..20]);
    }

    #[test]
    fn pair_address_separator_prevents_prefix_collision() {
        assert_ne!(pair_address(b"ab", b"c"), pair_address(b"a", b"bc"));
    }
}
