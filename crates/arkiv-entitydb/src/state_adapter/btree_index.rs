//! In-storage B+ tree for the Tier-2 index (Int mode, values ≤ 32 bytes).
//!
//! One tree per annotation key, stored as a set of EVM accounts whose
//! storage slots hold the node data. Traversal and mutation use only
//! `StateAdapter::raw_storage` / `set_raw_storage` / `ensure_raw_account`
//! point reads and writes — no MDBX cursor, no ordered storage scan.
//! This is compatible with reth v2.0's `HashedStorages`.
//!
//! ## Node layout
//!
//! ### Header account (`btree_header_address(attr_key)`)
//! Slot 0: `[0..7]` = root_node_id (u64 BE), `[8..15]` = next_node_id
//! (u64 BE), `[16]` = `BTREE_MAGIC` (0x42).
//!
//! ### Node account (`btree_node_address(header_addr, node_id)`)
//! Slot 0 (metadata): `[0..7]` = right_sibling (u64 BE), `[8..9]` = key_count
//! (u16 BE), `[10]` = flags (bit0: 1=leaf, 0=internal).
//! Key slots: `u64_slot(1+i)` for i in `0..key_count`.
//! Value/child slots: `u64_slot(1+ORDER+i)` for i in `0..key_count` (leaf)
//! or `0..key_count+1` (internal).
//!
//! Leaf values: `slot_presence(len)` — non-zero means present.
//! Internal values: `u64_slot(child_node_id)` — child address pointer.
//!
//! ## Key encoding
//!
//! Annotation values (≤ 32 bytes) are stored as B256 keys via
//! `annot_val_to_slot` (right-padded with zeros). Lex order on B256
//! matches numeric big-endian order, so `iter_gte(bound)` correctly
//! implements numeric range queries.
//!
//! ## Lazy deletion
//!
//! `btree_lazy_delete` zeroes out the leaf value slot without restructuring
//! the tree. `btree_iter_from` skips entries where `value == B256::ZERO`.
//! Re-inserts of the same key update the value in place (duplicate-free).

use alloy_primitives::{Address, B256, keccak256};
use eyre::Result;

use super::StateAdapter;

// ─── Constants ───────────────────────────────────────────────────────────────

/// Magic byte at offset 16 of the B+ tree header slot. Distinguishes the header
/// from a list account whose slot 0 is a u64 count (byte 16 always zero).
pub const BTREE_MAGIC: u8 = 0x42;

/// Maximum number of keys per B+ tree node (leaf or internal).
pub const BTREE_ORDER: usize = 32;

// ─── Address derivations ─────────────────────────────────────────────────────

/// Header address for the B+ tree that indexes annotation key `attr_key`.
pub fn btree_header_address(attr_key: &[u8]) -> Address {
    let mut buf = Vec::with_capacity(b"arkiv.ibth".len() + attr_key.len());
    buf.extend_from_slice(b"arkiv.ibth");
    buf.extend_from_slice(attr_key);
    Address::from_slice(&keccak256(buf).0[..20])
}

fn btree_node_address(header_addr: &Address, node_id: u64) -> Address {
    let mut buf = [0u8; 38];
    buf[..10].copy_from_slice(b"arkiv.ibtn");
    buf[10..30].copy_from_slice(header_addr.as_slice());
    buf[30..].copy_from_slice(&node_id.to_be_bytes());
    Address::from_slice(&keccak256(buf).0[..20])
}

// ─── Slot encodings ──────────────────────────────────────────────────────────

/// Right-pad an annotation value (≤ 32 bytes) to a B256 slot key.
#[inline]
pub fn annot_val_to_slot(val: &[u8]) -> B256 {
    debug_assert!(val.len() <= 32, "annot_val_to_slot: value too long ({} bytes)", val.len());
    let mut buf = [0u8; 32];
    buf[..val.len()].copy_from_slice(val);
    B256::from(buf)
}

/// Encode "value of length `len` is present" as a slot value.
/// `slot_presence(0)` = `[0,…,0,0,0,0,1]` (last byte = 1).
#[inline]
pub fn slot_presence(len: usize) -> B256 {
    let mut buf = [0u8; 32];
    buf[28..].copy_from_slice(&(len as u32 + 1).to_be_bytes());
    B256::from(buf)
}

/// Decode the annotation value length from a slot presence value.
/// Returns `None` if zero (absent or lazy-deleted).
#[inline]
pub fn slot_to_val_len(b: B256) -> Option<usize> {
    let n = u32::from_be_bytes(b.0[28..].try_into().unwrap());
    if n == 0 { None } else { Some((n - 1) as usize) }
}

/// Slot index for node metadata / system counters stored as u64.
#[inline]
fn u64_slot(n: u64) -> B256 {
    let mut buf = [0u8; 32];
    buf[24..].copy_from_slice(&n.to_be_bytes());
    B256::from(buf)
}

#[inline]
fn btree_key_slot(i: usize) -> B256 {
    u64_slot(1 + i as u64)
}

#[inline]
fn btree_value_slot(i: usize) -> B256 {
    u64_slot(1 + BTREE_ORDER as u64 + i as u64)
}

// ─── Header and node I/O ─────────────────────────────────────────────────────

struct BTreeNode {
    is_leaf: bool,
    right_sibling: u64,
    keys: Vec<B256>,
    values: Vec<B256>,
}

fn btree_read_header<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
) -> Result<(u64, u64)> {
    let raw = state.raw_storage(header_addr, B256::ZERO)?;
    let root_id = u64::from_be_bytes(raw.0[0..8].try_into().unwrap());
    let next_id = u64::from_be_bytes(raw.0[8..16].try_into().unwrap());
    Ok((root_id, next_id))
}

fn btree_write_header<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
    root_id: u64,
    next_id: u64,
) -> Result<()> {
    let mut buf = [0u8; 32];
    buf[0..8].copy_from_slice(&root_id.to_be_bytes());
    buf[8..16].copy_from_slice(&next_id.to_be_bytes());
    buf[16] = BTREE_MAGIC;
    state.ensure_raw_account(header_addr)?;
    state.set_raw_storage(header_addr, B256::ZERO, B256::from(buf))
}

fn btree_alloc_node<S: StateAdapter>(state: &mut S, header_addr: &Address) -> Result<u64> {
    let (root_id, next_id) = btree_read_header(state, header_addr)?;
    let node_id = if next_id == 0 { 1 } else { next_id };
    btree_write_header(state, header_addr, root_id, node_id + 1)?;
    Ok(node_id)
}

fn btree_read_node<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
    node_id: u64,
) -> Result<BTreeNode> {
    let node_addr = btree_node_address(header_addr, node_id);
    let meta = state.raw_storage(&node_addr, B256::ZERO)?;
    let right_sibling = u64::from_be_bytes(meta.0[0..8].try_into().unwrap());
    let key_count = u16::from_be_bytes(meta.0[8..10].try_into().unwrap()) as usize;
    let is_leaf = meta.0[10] & 1 != 0;

    let mut keys = Vec::with_capacity(key_count);
    for i in 0..key_count {
        keys.push(state.raw_storage(&node_addr, btree_key_slot(i))?);
    }
    let val_count = if is_leaf { key_count } else { key_count + 1 };
    let mut values = Vec::with_capacity(val_count);
    for i in 0..val_count {
        values.push(state.raw_storage(&node_addr, btree_value_slot(i))?);
    }
    Ok(BTreeNode { is_leaf, right_sibling, keys, values })
}

fn btree_write_node<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
    node_id: u64,
    node: &BTreeNode,
) -> Result<()> {
    let node_addr = btree_node_address(header_addr, node_id);
    state.ensure_raw_account(&node_addr)?;
    let mut meta = [0u8; 32];
    meta[0..8].copy_from_slice(&node.right_sibling.to_be_bytes());
    meta[8..10].copy_from_slice(&(node.keys.len() as u16).to_be_bytes());
    meta[10] = if node.is_leaf { 1 } else { 0 };
    state.set_raw_storage(&node_addr, B256::ZERO, B256::from(meta))?;
    for (i, k) in node.keys.iter().enumerate() {
        state.set_raw_storage(&node_addr, btree_key_slot(i), *k)?;
    }
    for (i, v) in node.values.iter().enumerate() {
        state.set_raw_storage(&node_addr, btree_value_slot(i), *v)?;
    }
    Ok(())
}

fn btree_descend_to_leaf<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
    mut node_id: u64,
    key: B256,
) -> Result<u64> {
    loop {
        let node = btree_read_node(state, header_addr, node_id)?;
        if node.is_leaf {
            return Ok(node_id);
        }
        let pos = node.keys.partition_point(|k| *k <= key);
        node_id = u64::from_be_bytes(node.values[pos].0[24..32].try_into().unwrap());
    }
}

// ─── Insert ──────────────────────────────────────────────────────────────────

enum InsertResult {
    Done,
    Split { push_up: B256, new_sibling_id: u64 },
}

fn insert_recursive<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
    node_id: u64,
    key: B256,
    value: B256,
) -> Result<InsertResult> {
    let mut node = btree_read_node(state, header_addr, node_id)?;

    if node.is_leaf {
        let pos = node.keys.partition_point(|k| *k < key);
        if pos < node.keys.len() && node.keys[pos] == key {
            // Update in place (handles lazy-delete re-insert and B256::ZERO $all key).
            node.values[pos] = value;
            btree_write_node(state, header_addr, node_id, &node)?;
            return Ok(InsertResult::Done);
        }
        node.keys.insert(pos, key);
        node.values.insert(pos, value);
        if node.keys.len() <= BTREE_ORDER {
            btree_write_node(state, header_addr, node_id, &node)?;
            return Ok(InsertResult::Done);
        }
        let mid = node.keys.len() / 2;
        let new_id = btree_alloc_node(state, header_addr)?;
        let new_node = BTreeNode {
            is_leaf: true,
            right_sibling: node.right_sibling,
            keys: node.keys.split_off(mid),
            values: node.values.split_off(mid),
        };
        let push_up = new_node.keys[0];
        node.right_sibling = new_id;
        btree_write_node(state, header_addr, node_id, &node)?;
        btree_write_node(state, header_addr, new_id, &new_node)?;
        return Ok(InsertResult::Split { push_up, new_sibling_id: new_id });
    }

    // Internal node
    let pos = node.keys.partition_point(|k| *k <= key);
    let child_id = u64::from_be_bytes(node.values[pos].0[24..32].try_into().unwrap());
    match insert_recursive(state, header_addr, child_id, key, value)? {
        InsertResult::Done => Ok(InsertResult::Done),
        InsertResult::Split { push_up, new_sibling_id } => {
            node.keys.insert(pos, push_up);
            let mut rhs_buf = [0u8; 32];
            rhs_buf[24..32].copy_from_slice(&new_sibling_id.to_be_bytes());
            node.values.insert(pos + 1, B256::from(rhs_buf));
            if node.keys.len() <= BTREE_ORDER {
                btree_write_node(state, header_addr, node_id, &node)?;
                return Ok(InsertResult::Done);
            }
            let mid = node.keys.len() / 2;
            let push_up_key = node.keys[mid];
            let new_id = btree_alloc_node(state, header_addr)?;
            let new_node = BTreeNode {
                is_leaf: false,
                right_sibling: node.right_sibling,
                keys: node.keys.split_off(mid + 1),
                values: node.values.split_off(mid + 1),
            };
            node.keys.truncate(mid);
            node.right_sibling = new_id;
            btree_write_node(state, header_addr, node_id, &node)?;
            btree_write_node(state, header_addr, new_id, &new_node)?;
            Ok(InsertResult::Split { push_up: push_up_key, new_sibling_id: new_id })
        }
    }
}

/// Insert `(key, value)` into the B+ tree rooted at `header_addr`.
/// If `key` already exists (lazy-delete re-insert), updates value in place.
pub fn btree_insert<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
    key: B256,
    value: B256,
) -> Result<()> {
    let (root_id, _) = btree_read_header(state, header_addr)?;
    if root_id == 0 {
        let leaf_id = btree_alloc_node(state, header_addr)?;
        let leaf = BTreeNode {
            is_leaf: true,
            right_sibling: 0,
            keys: vec![key],
            values: vec![value],
        };
        btree_write_node(state, header_addr, leaf_id, &leaf)?;
        let (_, next) = btree_read_header(state, header_addr)?;
        return btree_write_header(state, header_addr, leaf_id, next);
    }
    match insert_recursive(state, header_addr, root_id, key, value)? {
        InsertResult::Done => {}
        InsertResult::Split { push_up, new_sibling_id } => {
            let new_root_id = btree_alloc_node(state, header_addr)?;
            let mut lhs_buf = [0u8; 32];
            lhs_buf[24..32].copy_from_slice(&root_id.to_be_bytes());
            let mut rhs_buf = [0u8; 32];
            rhs_buf[24..32].copy_from_slice(&new_sibling_id.to_be_bytes());
            let new_root = BTreeNode {
                is_leaf: false,
                right_sibling: 0,
                keys: vec![push_up],
                values: vec![B256::from(lhs_buf), B256::from(rhs_buf)],
            };
            btree_write_node(state, header_addr, new_root_id, &new_root)?;
            let (_, next) = btree_read_header(state, header_addr)?;
            btree_write_header(state, header_addr, new_root_id, next)?;
        }
    }
    Ok(())
}

// ─── Lazy deletion ────────────────────────────────────────────────────────────

/// Mark `key` as deleted without restructuring the tree. Sets its leaf value
/// slot to B256::ZERO. `btree_iter_from` skips zero-value entries.
pub fn btree_lazy_delete<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
    key: B256,
) -> Result<()> {
    let (root_id, _) = btree_read_header(state, header_addr)?;
    if root_id == 0 {
        return Ok(());
    }
    let leaf_id = btree_descend_to_leaf(state, header_addr, root_id, key)?;
    let node = btree_read_node(state, header_addr, leaf_id)?;
    let pos = node.keys.partition_point(|k| *k < key);
    if pos < node.keys.len() && node.keys[pos] == key {
        let node_addr = btree_node_address(header_addr, leaf_id);
        state.set_raw_storage(&node_addr, btree_value_slot(pos), B256::ZERO)?;
    }
    Ok(())
}

// ─── Range iteration ─────────────────────────────────────────────────────────

/// Return all entries with key ≥ `from` in ascending key order.
/// Lazy-deleted entries (value = B256::ZERO) are skipped.
/// Returns `(slot_key, slot_presence)` pairs where `slot_key =
/// annot_val_to_slot(val)` and `slot_presence = slot_presence(val.len())`.
pub fn btree_iter_from<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
    from: B256,
) -> Result<Vec<(B256, B256)>> {
    let (root_id, _) = btree_read_header(state, header_addr)?;
    if root_id == 0 {
        return Ok(vec![]);
    }
    let mut leaf_id = btree_descend_to_leaf(state, header_addr, root_id, from)?;
    let mut result = Vec::new();
    let mut first = true;
    loop {
        let leaf = btree_read_node(state, header_addr, leaf_id)?;
        let start = if first {
            first = false;
            leaf.keys.partition_point(|k| *k < from)
        } else {
            0
        };
        for i in start..leaf.keys.len() {
            if leaf.values[i] != B256::ZERO {
                result.push((leaf.keys[i], leaf.values[i]));
            }
        }
        if leaf.right_sibling == 0 {
            break;
        }
        leaf_id = leaf.right_sibling;
    }
    Ok(result)
}
