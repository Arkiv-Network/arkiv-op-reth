//! On-trie representation of an entity, plus the `(key, value_type,
//! value)` attribute type it embeds.
//!
//! The struct derives [`alloy_rlp::RlpEncodable`] /
//! [`alloy_rlp::RlpDecodable`], so the canonical wire format is plain
//! RLP. The trie-specific framing (`0xFE` prefix to defend against
//! accidental `CALL`, storage as account `code`) lives in
//! `arkiv-node`'s trie-layout module — not here.

use alloy_primitives::{Address, B256};
use alloy_rlp::{RlpDecodable, RlpEncodable};

/// Attribute `value_type` tags. Must match
/// `Entity.ATTR_{UINT,STRING,ENTITY_KEY}` in EntityRegistry.sol and
/// the ABI shape decoded by the precompile.
pub const ATTR_UINT: u8 = 1;
pub const ATTR_STRING: u8 = 2;
pub const ATTR_ENTITY_KEY: u8 = 3;

/// Typed representation of an entity. The RLP form is what gets
/// committed to chain state by trie-backed [`Store`](crate::Store)
/// impls; the typed form is what op handlers and queries work with.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct Entity {
    pub payload: Vec<u8>,
    pub creator: Address,
    pub created_at_block: u64,
    pub owner: Address,
    pub expires_at: u64,
    pub content_type: Vec<u8>,
    pub key: B256,
    pub attributes: Vec<Attribute>,
    /// Block number of the most recent mutation (CREATE / UPDATE /
    /// EXTEND / TRANSFER) — equals `created_at_block` until the
    /// entity is first modified.
    pub last_modified_at_block: u64,
}

/// Discriminated `(key, value)` attribute mirroring the precompile
/// ABI. `value_type` selects how `value` should be interpreted:
/// `ATTR_UINT` → 32-byte big-endian uint256; `ATTR_STRING` → opaque
/// bytes (UTF-8 by SDK convention); `ATTR_ENTITY_KEY` → 32 raw
/// bytes of an entity key.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct Attribute {
    pub key: Vec<u8>,
    pub value_type: u8,
    pub value: Vec<u8>,
}
