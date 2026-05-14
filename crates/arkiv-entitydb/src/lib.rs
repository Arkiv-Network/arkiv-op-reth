//! Core encode / decode + address-derivation primitives for the v2
//! Arkiv state model.
//!
//! Every entity and every annotation bitmap lives in op-reth's standard
//! world-state trie as an Ethereum account:
//!
//! - **Entity account** at `entity_address(entityKey)` carries the
//!   RLP-encoded entity (payload + content type + annotations +
//!   owner/expires_at) in `code`, prefixed with `0xFE` so a stray
//!   `CALL` reverts immediately.
//! - **Pair account** at `pair_address(annot_key, annot_val)` carries a
//!   roaring64 bitmap of entity IDs as `code`. `codeHash` is
//!   `keccak256(bitmap_bytes)` by construction — every bitmap is
//!   content-addressed in the trie.
//!
//! This crate is pure data: it holds the struct definitions, the codec
//! (RLP for entities, RoaringFormatSpec for bitmaps), and the address
//! derivations. It does **not** touch revm, MDBX, or any reth APIs.
//! The precompile (in `arkiv-node`) and any reader (RPC, future SDK)
//! all consume these primitives.

use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use eyre::{Result, ensure};
use roaring::RoaringTreemap;

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

// ─── Built-in annotations ─────────────────────────────────────────────
//
// Every entity carries these implicit pairs in addition to its
// user-supplied annotations. The precompile derives them from the op
// itself (no caller input needed).

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

// ─── Bitmap (roaring64) ───────────────────────────────────────────────
//
// Thin wrapper around `roaring::RoaringTreemap`. The on-disk format is
// the portable RoaringFormatSpec layout — byte-deterministic for a
// given set, which is required so `codeHash = keccak256(bitmap_bytes)`
// agrees across all nodes executing the same op batch.

/// Roaring64 bitmap of entity IDs.
///
/// Determinism guarantee: `Bitmap::to_bytes` produces the same bytes
/// for any two instances that contain the same set of IDs. The
/// underlying `RoaringTreemap` is a `BTreeMap<u32, RoaringBitmap>`
/// (high-half keyed, sorted iteration) and the inner `RoaringBitmap`'s
/// container vector is also sorted. Combined with `LittleEndian`
/// big-uint encoding (per RoaringFormatSpec), the output is canonical.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bitmap(RoaringTreemap);

impl Bitmap {
    pub fn new() -> Self {
        Self(RoaringTreemap::new())
    }

    /// Deserialize from the portable RoaringFormatSpec layout (i.e. the
    /// output of [`Bitmap::to_bytes`]).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        RoaringTreemap::deserialize_from(bytes)
            .map(Self)
            .map_err(|e| eyre::eyre!("invalid roaring bitmap bytes: {e}"))
    }

    /// Serialize to the portable RoaringFormatSpec layout. Same set →
    /// same bytes, on every node.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.0.serialized_size());
        self.0
            .serialize_into(&mut buf)
            .expect("writing to Vec is infallible");
        buf
    }

    pub fn insert(&mut self, id: u64) -> bool {
        self.0.insert(id)
    }

    pub fn remove(&mut self, id: u64) -> bool {
        self.0.remove(id)
    }

    pub fn contains(&self, id: u64) -> bool {
        self.0.contains(id)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> u64 {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.iter()
    }
}

impl FromIterator<u64> for Bitmap {
    fn from_iter<I: IntoIterator<Item = u64>>(iter: I) -> Self {
        Self(RoaringTreemap::from_iter(iter))
    }
}

// ─── Entity RLP ───────────────────────────────────────────────────────
//
// The on-trie representation of an entity: everything a reader needs
// in a single `eth_getCode(entity_address)` (statedb-design §2.2).
//
// The contract maintains a parallel `(owner, expiresAt)` mapping for
// fast validation — those fields are duplicated here so the RLP is
// self-sufficient for query reads.

/// Prefix prepended to the RLP bytes before storing as account `code`.
/// `0xFE` is the EVM `INVALID` opcode — any `CALL` to an entity
/// address halts immediately. The prefix is part of the on-trie code
/// (and therefore `codeHash`), not part of the RLP.
pub const ENTITY_CODE_PREFIX: u8 = 0xFE;

/// On-trie representation of an entity. Encoded as
/// `0xFE || RLP(EntityRlp)` and stored as the entity-account `code`.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct EntityRlp {
    /// Opaque entity payload. Bounded by the precompile's content cap.
    pub payload: Vec<u8>,
    /// Address that submitted the original `Create`. Immutable.
    pub creator: Address,
    /// Block at which `Create` landed. Immutable.
    pub created_at_block: u64,
    /// Current owner. Rewritten on `Transfer`.
    pub owner: Address,
    /// Block past which the entity expires. Rewritten on `Extend`.
    pub expires_at: u64,
    /// MIME-shaped content type, byte-validated by the precompile.
    pub content_type: Vec<u8>,
    /// Full 32-byte entity key — the last 12 bytes aren't recoverable
    /// from the 20-byte entity address alone.
    pub key: B256,
    pub string_annotations: Vec<StringAnnotation>,
    pub numeric_annotations: Vec<NumericAnnotation>,
}

/// `(key, value)` pair where `value` is opaque bytes — used for the
/// SDK `STRING` and `ENTITY_KEY` annotation types.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct StringAnnotation {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

/// `(key, value)` pair where `value` is a `uint256` — used for the
/// SDK `UINT` annotation type.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct NumericAnnotation {
    pub key: Vec<u8>,
    pub value: U256,
}

impl EntityRlp {
    /// Encode for storage as account code: `0xFE || RLP(self)`.
    pub fn encode_as_code(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(1 + self.length());
        buf.push(ENTITY_CODE_PREFIX);
        self.encode(&mut buf);
        buf
    }

    /// Decode from account code. Verifies the `0xFE` prefix and then
    /// RLP-decodes the rest.
    pub fn decode_from_code(code: &[u8]) -> Result<Self> {
        ensure!(
            code.first() == Some(&ENTITY_CODE_PREFIX),
            "entity code is missing the {:#x} prefix",
            ENTITY_CODE_PREFIX,
        );
        let mut rest = &code[1..];
        Self::decode(&mut rest)
            .map_err(|e| eyre::eyre!("RLP decode of EntityRlp failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;

    // ─── Address derivations ─────────────────────────────────────────

    #[test]
    fn entity_address_truncates_to_first_20_bytes() {
        let key = b256!("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff");
        let addr = entity_address(key);
        assert_eq!(addr.as_slice(), &key.0[..20]);
    }

    #[test]
    fn pair_address_is_deterministic() {
        let a = pair_address(b"contentType", b"image/png");
        let b = pair_address(b"contentType", b"image/png");
        assert_eq!(a, b);
    }

    #[test]
    fn pair_address_separator_prevents_prefix_collision() {
        // ("ab", "c") vs ("a", "bc") would collide if we concatenated
        // without the 0x00 separator.
        let with_separator_a = pair_address(b"ab", b"c");
        let with_separator_b = pair_address(b"a", b"bc");
        assert_ne!(with_separator_a, with_separator_b);
    }

    // ─── Bitmap ───────────────────────────────────────────────────────

    #[test]
    fn bitmap_roundtrip() {
        let mut a = Bitmap::new();
        a.insert(1);
        a.insert(1_000_000);
        a.insert(u64::MAX);
        let bytes = a.to_bytes();
        let b = Bitmap::from_bytes(&bytes).expect("decode");
        assert_eq!(a, b);
    }

    #[test]
    fn bitmap_serialization_is_deterministic() {
        // Same set, two independently-constructed bitmaps: identical
        // bytes. This is the codeHash-equality invariant.
        let ids = [3u64, 1, 2, 42, 1_000_001, 1_000_000];
        let mut a = Bitmap::new();
        let mut b = Bitmap::new();
        for id in ids {
            a.insert(id);
        }
        // Insert in reverse order — must still produce identical
        // bytes (the canonical layout is sorted regardless of insertion
        // order).
        for id in ids.iter().rev() {
            b.insert(*id);
        }
        assert_eq!(a.to_bytes(), b.to_bytes());
    }

    #[test]
    fn bitmap_empty_remove_is_no_op() {
        let mut b = Bitmap::new();
        assert!(!b.remove(42));
        assert!(b.is_empty());
    }

    // ─── EntityRlp ────────────────────────────────────────────────────

    fn sample_entity() -> EntityRlp {
        EntityRlp {
            payload: b"hello".to_vec(),
            creator: Address::repeat_byte(0xaa),
            created_at_block: 1234,
            owner: Address::repeat_byte(0xbb),
            expires_at: 99_999,
            content_type: b"application/json".to_vec(),
            key: b256!("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"),
            string_annotations: vec![StringAnnotation {
                key: b"title".to_vec(),
                value: b"the answer".to_vec(),
            }],
            numeric_annotations: vec![NumericAnnotation {
                key: b"priority".to_vec(),
                value: U256::from(42),
            }],
        }
    }

    #[test]
    fn entity_rlp_roundtrip_via_code() {
        let original = sample_entity();
        let code = original.encode_as_code();
        assert_eq!(code[0], ENTITY_CODE_PREFIX);
        let decoded = EntityRlp::decode_from_code(&code).expect("decode");
        assert_eq!(original, decoded);
    }

    #[test]
    fn entity_rlp_decode_requires_fe_prefix() {
        let original = sample_entity();
        let mut bad = original.encode_as_code();
        bad[0] = 0x00; // strip the 0xFE
        let err = EntityRlp::decode_from_code(&bad).unwrap_err();
        assert!(format!("{err}").contains("0xfe prefix"));
    }
}
