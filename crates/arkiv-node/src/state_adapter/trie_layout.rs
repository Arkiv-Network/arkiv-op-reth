//! On-trie encoding for Arkiv's system-account slots.
//!
//! Centralises the constants and value encodings every trie-backed
//! [`StateAdapter`](arkiv_entitydb::StateAdapter) impl in this crate (read-write,
//! read-only, in-memory) needs to agree on so that all impls produce
//! the same canonical bytes for the same logical state. Drift here
//! would mean two honest nodes computing different state roots for
//! the same block — a consensus bug.
//!
//! Layered above this module are the three [`StateAdapter`] impls. Layered
//! below is the raw account / storage I/O each impl gets from its
//! backing store (revm `EvmInternals`, reth `StateProvider`, or a
//! plain `HashMap`).
//!
//! `arkiv-entitydb` deliberately knows nothing about this module —
//! its [`StateAdapter`] trait is typed end-to-end, and op handlers never see
//! a slot key or a packed encoding.

use alloy_primitives::{Address, B256, keccak256};
use alloy_rlp::{Decodable, Encodable};
use arkiv_entitydb::Entity;
use eyre::{Result, ensure};

// ─── System-account address ────────────────────────────────────────

/// Singleton account that hosts the global entity counter, ID ↔
/// address maps, and per-EOA minting nonces as storage slots.
/// Materialised lazily on the first write by each [`StateAdapter`] impl
/// (raising the nonce to ≥ 1 so EIP-161 doesn't prune it). No genesis
/// allocation required.
pub const SYSTEM_ACCOUNT_ADDRESS: Address = Address::new([
    0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x46,
]);

// ─── System-account slot derivations ───────────────────────────────
//
// Slot keys are scoped by a short tag so the four keyspaces below
// cannot collide.

/// `slot[keccak256("entity_count")]` → next `entity_id` (uint64).
pub fn slot_entity_count() -> B256 {
    keccak256(b"entity_count")
}

/// `slot[keccak256("id_to_addr" || id_be_bytes)]` → entity_address.
pub fn slot_id_to_addr(entity_id: u64) -> B256 {
    let mut buf = [0u8; 10 + 8];
    buf[..10].copy_from_slice(b"id_to_addr");
    buf[10..].copy_from_slice(&entity_id.to_be_bytes());
    keccak256(buf)
}

/// `slot[keccak256("addr_to_id" || entity_address_bytes)]` → uint64
/// ID.
pub fn slot_addr_to_id(entity_addr: Address) -> B256 {
    let mut buf = [0u8; 10 + 20];
    buf[..10].copy_from_slice(b"addr_to_id");
    buf[10..].copy_from_slice(entity_addr.as_slice());
    keccak256(buf)
}

/// `slot[keccak256("nonces" || caller_address)]` → uint32 entity-key
/// minting nonce, returned by the SDK-visible `nonces(address)` view.
pub fn slot_nonces(caller: Address) -> B256 {
    let mut buf = [0u8; 6 + 20];
    buf[..6].copy_from_slice(b"nonces");
    buf[6..].copy_from_slice(caller.as_slice());
    keccak256(buf)
}

// ─── Storage value encodings ───────────────────────────────────────
//
// All slot values are 32 bytes; we pack u32/u64/Address into the
// rightmost bytes (zero-padded on the left) so the on-trie
// representation matches how Solidity packs the same types in
// `bytes32` storage.

#[inline]
pub fn u64_to_storage(n: u64) -> B256 {
    let mut buf = [0u8; 32];
    buf[24..].copy_from_slice(&n.to_be_bytes());
    B256::from(buf)
}

#[inline]
pub fn storage_to_u64(b: B256) -> u64 {
    u64::from_be_bytes(b.0[24..].try_into().unwrap())
}

#[inline]
pub fn u32_to_storage(n: u32) -> B256 {
    let mut buf = [0u8; 32];
    buf[28..].copy_from_slice(&n.to_be_bytes());
    B256::from(buf)
}

#[inline]
pub fn storage_to_u32(b: B256) -> u32 {
    u32::from_be_bytes(b.0[28..].try_into().unwrap())
}

#[inline]
pub fn address_to_storage(addr: Address) -> B256 {
    let mut buf = [0u8; 32];
    buf[12..].copy_from_slice(addr.as_slice());
    B256::from(buf)
}

#[inline]
pub fn storage_to_address(b: B256) -> Address {
    Address::from_slice(&b.0[12..])
}

// ─── Entity account code encoding ──────────────────────────────────
//
// Entity accounts carry `0xFE || RLP(entity)` as their `code`. The
// `0xFE` is the EVM `INVALID` opcode — any stray `CALL` to an entity
// address halts immediately. Trie-backed [`StateAdapter`](arkiv_entitydb::StateAdapter)
// impls wrap the RLP form here on write and strip it on read so that
// `arkiv-entitydb` never has to know.

/// Prefix prepended to the RLP bytes when storing an entity as
/// account `code`. `0xFE` is the EVM `INVALID` opcode.
pub const ENTITY_CODE_PREFIX: u8 = 0xFE;

/// Encode an [`Entity`] for storage as account code: `0xFE || RLP(entity)`.
pub fn entity_to_code(entity: &Entity) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + entity.length());
    buf.push(ENTITY_CODE_PREFIX);
    entity.encode(&mut buf);
    buf
}

/// Decode an [`Entity`] from account code. Verifies the `0xFE`
/// prefix and then RLP-decodes the rest.
pub fn entity_from_code(code: &[u8]) -> Result<Entity> {
    ensure!(
        code.first() == Some(&ENTITY_CODE_PREFIX),
        "entity code is missing the {:#x} prefix",
        ENTITY_CODE_PREFIX,
    );
    let mut rest = &code[1..];
    Entity::decode(&mut rest).map_err(|e| eyre::eyre!("RLP decode of Entity failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{U256, b256};
    use arkiv_entitydb::{ATTR_ENTITY_KEY, ATTR_STRING, ATTR_UINT, Attribute};

    #[test]
    fn entity_roundtrip_via_code() {
        let original = Entity {
            payload: b"hello".to_vec(),
            creator: Address::repeat_byte(0xaa),
            created_at_block: 1234,
            owner: Address::repeat_byte(0xbb),
            expires_at: 99_999,
            content_type: b"application/json".to_vec(),
            key: b256!("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"),
            attributes: vec![
                Attribute {
                    key: b"title".to_vec(),
                    value_type: ATTR_STRING,
                    value: b"the answer".to_vec(),
                },
                Attribute {
                    key: b"priority".to_vec(),
                    value_type: ATTR_UINT,
                    value: U256::from(42).to_be_bytes::<32>().to_vec(),
                },
                Attribute {
                    key: b"replyTo".to_vec(),
                    value_type: ATTR_ENTITY_KEY,
                    value: vec![0xab; 32],
                },
            ],
            last_modified_at_block: 1234,
        };
        let code = entity_to_code(&original);
        assert_eq!(code[0], ENTITY_CODE_PREFIX);
        assert_eq!(entity_from_code(&code).expect("decode"), original);
    }

    #[test]
    fn entity_decode_requires_fe_prefix() {
        let entity = Entity {
            payload: vec![],
            creator: Address::ZERO,
            created_at_block: 0,
            owner: Address::ZERO,
            expires_at: 0,
            content_type: vec![],
            key: B256::ZERO,
            attributes: vec![],
            last_modified_at_block: 0,
        };
        let mut bad = entity_to_code(&entity);
        bad[0] = 0x00;
        assert!(entity_from_code(&bad).is_err());
    }
}
