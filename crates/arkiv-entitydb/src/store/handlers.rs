//! Op handlers and read-side helpers that run against a [`Store`].
//!
//! Each handler assumes the contract has already validated ownership /
//! liveness. It performs all the state mutations: system-account
//! counter, ID maps, bitmap deltas (built-in + user annotations), and
//! the entity-account RLP write.

use alloy_primitives::{Address, B256};
use eyre::Result;

use super::addresses::{encode_address, encode_b256, encode_u64_be};
use super::{
    ANNOT_ALL, ANNOT_CONTENT_TYPE, ANNOT_CREATED_AT_BLOCK, ANNOT_CREATOR, ANNOT_EXPIRATION,
    ANNOT_KEY, ANNOT_OWNER, Attribute, Bitmap, Entity, IndexTree, Store, entity_address,
};

// ─── Op handlers ──────────────────────────────────────────────────────

/// Create a new entity. Allocates a fresh `entity_id`, writes both ID
/// maps on the Arkiv account, populates all built-in + user bitmaps,
/// and writes the entity RLP.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "entitydb_create",
    level = "debug",
    skip_all,
    fields(
        payload_bytes = payload.len(),
        n_attrs = attributes.len(),
    ),
)]
pub fn create<S: Store>(
    state: &mut S,
    sender: Address,
    entity_key: B256,
    expires_at: u64,
    current_block: u64,
    payload: Vec<u8>,
    content_type: Vec<u8>,
    attributes: Vec<Attribute>,
) -> Result<()> {
    // 1) Allocate entity_id.
    let entity_id = state.get_entity_count()?;
    state.set_entity_count(entity_id + 1)?;

    // 2) Write ID maps.
    let entity_addr = entity_address(entity_key);
    state.set_id_to_addr(entity_id, entity_addr)?;
    state.set_addr_to_id(&entity_addr, entity_id)?;

    // 3) Insert into every bitmap (built-in + user).
    for (k, v) in built_in_pairs(
        sender,
        sender,
        entity_key,
        current_block,
        expires_at,
        &content_type,
    )
    .into_iter()
    .chain(user_pairs(&attributes))
    {
        insert_into_pair_bitmap(state, &k, &v, entity_id)?;
    }

    // 4) Write the entity RLP.
    let entity = Entity {
        payload,
        creator: sender,
        created_at_block: current_block,
        owner: sender,
        expires_at,
        content_type,
        key: entity_key,
        attributes,
        last_modified_at_block: current_block,
    };
    state.set_entity(&entity_addr, entity)?;

    Ok(())
}

/// Replace an entity's payload / content type / annotations.
///
/// Preserves `creator`, `created_at_block`, `key`, `owner`,
/// `expires_at`. Bitmap diff: only annotations that changed get
/// touched (incl. `$contentType` if the content type changed).
#[tracing::instrument(
    name = "entitydb_update",
    level = "debug",
    skip_all,
    fields(
        payload_bytes = payload.len(),
        n_attrs = attributes.len(),
    ),
)]
pub fn update<S: Store>(
    state: &mut S,
    entity_key: B256,
    current_block: u64,
    payload: Vec<u8>,
    content_type: Vec<u8>,
    attributes: Vec<Attribute>,
) -> Result<()> {
    let entity_addr = entity_address(entity_key);
    let entity_id = state.get_addr_to_id(&entity_addr)?;
    let mut entity = read_entity(state, entity_addr)?;

    // Annotation diff. Built-ins that don't change on UPDATE
    // (`$creator`, `$createdAtBlock`, `$key`, `$owner`, `$expiration`,
    // `$all`) aren't included on either side, so the diff doesn't
    // touch them. `$contentType` IS in the diff so it moves if the
    // content type changed.
    let old_pairs = updatable_pairs(&entity.content_type, &entity.attributes);
    let new_pairs = updatable_pairs(&content_type, &attributes);
    apply_pair_diff(state, &old_pairs, &new_pairs, entity_id)?;

    entity.payload = payload;
    entity.content_type = content_type;
    entity.attributes = attributes;
    entity.last_modified_at_block = current_block;
    state.set_entity(&entity_addr, entity)?;

    Ok(())
}

/// Extend an entity's `expires_at`. Updates the `$expiration` bitmap
/// and re-encodes the RLP with the new value.
#[tracing::instrument(name = "entitydb_extend", level = "debug", skip_all)]
pub fn extend<S: Store>(
    state: &mut S,
    entity_key: B256,
    current_block: u64,
    new_expires_at: u64,
) -> Result<()> {
    let entity_addr = entity_address(entity_key);
    let entity_id = state.get_addr_to_id(&entity_addr)?;
    let mut entity = read_entity(state, entity_addr)?;

    remove_from_pair_bitmap(
        state,
        ANNOT_EXPIRATION,
        &encode_u64_be(entity.expires_at),
        entity_id,
    )?;
    insert_into_pair_bitmap(
        state,
        ANNOT_EXPIRATION,
        &encode_u64_be(new_expires_at),
        entity_id,
    )?;

    entity.expires_at = new_expires_at;
    entity.last_modified_at_block = current_block;
    state.set_entity(&entity_addr, entity)?;

    Ok(())
}

/// Hand an entity's ownership to `new_owner`. Updates the `$owner`
/// bitmap and re-encodes the RLP.
#[tracing::instrument(name = "entitydb_transfer", level = "debug", skip_all)]
pub fn transfer<S: Store>(
    state: &mut S,
    entity_key: B256,
    current_block: u64,
    new_owner: Address,
) -> Result<()> {
    let entity_addr = entity_address(entity_key);
    let entity_id = state.get_addr_to_id(&entity_addr)?;
    let mut entity = read_entity(state, entity_addr)?;

    remove_from_pair_bitmap(state, ANNOT_OWNER, &encode_address(entity.owner), entity_id)?;
    insert_into_pair_bitmap(state, ANNOT_OWNER, &encode_address(new_owner), entity_id)?;

    entity.owner = new_owner;
    entity.last_modified_at_block = current_block;
    state.set_entity(&entity_addr, entity)?;

    Ok(())
}

/// Remove an entity. Clears every bitmap entry (built-in + user),
/// clears both ID-map slots on the Arkiv account, and tombstones the
/// entity account (`code = nil`, `nonce = 1`).
#[tracing::instrument(name = "entitydb_delete", level = "debug", skip_all)]
pub fn delete<S: Store>(state: &mut S, entity_key: B256) -> Result<()> {
    let entity_addr = entity_address(entity_key);
    let entity_id = state.get_addr_to_id(&entity_addr)?;
    let entity = read_entity(state, entity_addr)?;

    for (k, v) in built_in_pairs(
        entity.creator,
        entity.owner,
        entity_key,
        entity.created_at_block,
        entity.expires_at,
        &entity.content_type,
    )
    .into_iter()
    .chain(user_pairs(&entity.attributes))
    {
        remove_from_pair_bitmap(state, &k, &v, entity_id)?;
    }

    // Clear ID-map slots.
    state.set_id_to_addr(entity_id, Address::ZERO)?;
    state.set_addr_to_id(&entity_addr, 0)?;

    // Tombstone — keeps nonce=1 to defeat EIP-161.
    state.tombstone_entity(&entity_addr)?;

    Ok(())
}

/// Identical state path to [`delete`]. The contract has already
/// validated `block.number > expiresAt`.
#[tracing::instrument(name = "entitydb_expire", level = "debug", skip_all)]
pub fn expire<S: Store>(state: &mut S, entity_key: B256) -> Result<()> {
    delete(state, entity_key)
}

// ─── Public read-side helpers (used by the query interpreter) ─────────

/// Read the pair-account bitmap for `(annot_key, annot_val)`. An
/// account with empty code (never written, or tombstoned) decodes to
/// an empty [`Bitmap`] — not an error. Thin wrapper around
/// [`Store::get_pair_bitmap`] for ergonomic call sites.
pub fn read_pair_bitmap<S: Store>(
    state: &mut S,
    annot_key: &[u8],
    annot_val: &[u8],
) -> Result<Bitmap> {
    state.get_pair_bitmap(annot_key, annot_val)
}

/// Bitmap of every live entity ID — the `$all` built-in bitmap.
pub fn all_entities<S: Store>(state: &mut S) -> Result<Bitmap> {
    state.get_pair_bitmap(ANNOT_ALL, b"")
}

/// Read the Tier-2 [`IndexTree`] for `attr_key`. An absent or
/// tombstoned index account decodes to an empty tree — not an error.
pub fn read_index_tree<S: Store>(state: &mut S, attr_key: &[u8]) -> Result<IndexTree> {
    state.get_index_tree(attr_key)
}

/// Resolve a query-hit entity ID to its on-trie [`Entity`].
///
/// Returns `Ok(None)` if the ID's `id_to_addr` slot is zero (never
/// written, or cleared by `delete` / `expire`) or if the entity
/// account has empty code (tombstoned). Returns `Err` only on
/// underlying state errors or malformed entity bytes.
pub fn resolve_id<S: Store>(state: &mut S, id: u64) -> Result<Option<Entity>> {
    let entity_addr = state.get_id_to_addr(id)?;
    if entity_addr == Address::ZERO {
        return Ok(None);
    }
    state.get_entity(&entity_addr)
}

// ─── Internal helpers ─────────────────────────────────────────────────

fn read_entity<S: Store>(state: &mut S, entity_addr: Address) -> Result<Entity> {
    state
        .get_entity(&entity_addr)?
        .ok_or_else(|| eyre::eyre!("no entity at {entity_addr}"))
}

/// All built-in `(key, value)` pairs for an entity. Used by `create`
/// (to insert) and `delete`/`expire` (to remove).
fn built_in_pairs(
    creator: Address,
    owner: Address,
    entity_key: B256,
    created_at_block: u64,
    expires_at: u64,
    content_type: &[u8],
) -> Vec<(Vec<u8>, Vec<u8>)> {
    vec![
        (ANNOT_ALL.to_vec(), Vec::new()),
        (ANNOT_CREATOR.to_vec(), encode_address(creator)),
        (
            ANNOT_CREATED_AT_BLOCK.to_vec(),
            encode_u64_be(created_at_block),
        ),
        (ANNOT_OWNER.to_vec(), encode_address(owner)),
        (ANNOT_KEY.to_vec(), encode_b256(entity_key)),
        (ANNOT_EXPIRATION.to_vec(), encode_u64_be(expires_at)),
        (ANNOT_CONTENT_TYPE.to_vec(), content_type.to_vec()),
    ]
}

/// User-supplied attributes flattened to `(key, value)` byte pairs.
/// The value bytes are stored verbatim — the precompile is responsible
/// for producing the canonical byte form per `value_type` (32-byte BE
/// for `ATTR_UINT`, packed bytes for `ATTR_STRING`, 32 raw bytes for
/// `ATTR_ENTITY_KEY`).
fn user_pairs<'a>(
    attributes: &'a [Attribute],
) -> impl Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a {
    attributes
        .iter()
        .map(|a| (a.key.clone(), a.value.clone()))
}

/// Pairs that an UPDATE op diffs: the user attributes plus
/// `$contentType`. Other built-ins (`$creator` / `$key` /
/// `$createdAtBlock` / `$owner` / `$expiration` / `$all`) don't change
/// on UPDATE and so aren't in the diff set.
fn updatable_pairs(content_type: &[u8], attributes: &[Attribute]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::with_capacity(1 + attributes.len());
    out.push((ANNOT_CONTENT_TYPE.to_vec(), content_type.to_vec()));
    out.extend(user_pairs(attributes));
    out
}

/// Diff two `(key, value)` pair sets and apply removals + insertions
/// to the corresponding pair bitmaps.
fn apply_pair_diff<S: Store>(
    state: &mut S,
    old: &[(Vec<u8>, Vec<u8>)],
    new: &[(Vec<u8>, Vec<u8>)],
    entity_id: u64,
) -> Result<()> {
    use std::collections::BTreeSet;
    let old_set: BTreeSet<&(Vec<u8>, Vec<u8>)> = old.iter().collect();
    let new_set: BTreeSet<&(Vec<u8>, Vec<u8>)> = new.iter().collect();
    for p in old.iter().filter(|p| !new_set.contains(*p)) {
        remove_from_pair_bitmap(state, &p.0, &p.1, entity_id)?;
    }
    for p in new.iter().filter(|p| !old_set.contains(*p)) {
        insert_into_pair_bitmap(state, &p.0, &p.1, entity_id)?;
    }
    Ok(())
}

fn insert_into_pair_bitmap<S: Store>(
    state: &mut S,
    annot_key: &[u8],
    annot_val: &[u8],
    entity_id: u64,
) -> Result<()> {
    let mut bitmap = state.get_pair_bitmap(annot_key, annot_val)?;
    let was_empty = bitmap.is_empty();
    bitmap.insert(entity_id);
    state.set_pair_bitmap(annot_key, annot_val, bitmap)?;
    // Tier-2: insert the value into the index tree on first use of this pair.
    if was_empty {
        let mut tree = state.get_index_tree(annot_key)?;
        tree.insert(annot_val.to_vec());
        state.set_index_tree(annot_key, tree)?;
    }
    Ok(())
}

fn remove_from_pair_bitmap<S: Store>(
    state: &mut S,
    annot_key: &[u8],
    annot_val: &[u8],
    entity_id: u64,
) -> Result<()> {
    let mut bitmap = state.get_pair_bitmap(annot_key, annot_val)?;
    if bitmap.is_empty() {
        // Bitmap doesn't exist yet — nothing to remove. Shouldn't
        // happen under well-formed ops; tolerated.
        return Ok(());
    }
    bitmap.remove(entity_id);
    let now_empty = bitmap.is_empty();
    state.set_pair_bitmap(annot_key, annot_val, bitmap)?;
    // Tier-2: remove the value from the index tree when the last entity
    // using this pair is gone.
    if now_empty {
        let mut tree = state.get_index_tree(annot_key)?;
        tree.remove(annot_val);
        if tree.is_empty() {
            state.tombstone_index_tree(annot_key)?;
        } else {
            state.set_index_tree(annot_key, tree)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_utils::MemStore;
    use super::super::{ATTR_STRING, ATTR_UINT};
    use super::*;
    use alloy_primitives::U256;

    fn alice() -> Address {
        Address::repeat_byte(0xaa)
    }
    fn bob() -> Address {
        Address::repeat_byte(0xbb)
    }
    fn entity_key_n(n: u8) -> B256 {
        B256::from([n; 32])
    }

    #[test]
    fn create_writes_entity_and_all_bitmaps() {
        let mut state = MemStore::new();
        let key = entity_key_n(0x42);
        create(
            &mut state,
            alice(),
            key,
            100,
            10,
            b"hello".to_vec(),
            b"text/plain".to_vec(),
            vec![
                Attribute {
                    key: b"tag".to_vec(),
                    value_type: ATTR_STRING,
                    value: b"music".to_vec(),
                },
                Attribute {
                    key: b"score".to_vec(),
                    value_type: ATTR_UINT,
                    value: U256::from(7).to_be_bytes::<32>().to_vec(),
                },
            ],
        )
        .expect("create");

        // Entity present with the expected fields.
        let entity = state
            .get_entity(&entity_address(key))
            .unwrap()
            .expect("entity present");
        assert_eq!(entity.owner, alice());
        assert_eq!(entity.creator, alice());
        assert_eq!(entity.expires_at, 100);
        assert_eq!(entity.created_at_block, 10);

        // System counter advanced to 1; this entity got id=0.
        assert_eq!(state.entity_count, 1);

        // All built-ins + user annotations contain entity_id=0.
        assert!(state.get_pair_bitmap(ANNOT_ALL, b"").unwrap().contains(0));
        assert!(state
            .get_pair_bitmap(ANNOT_OWNER, alice().as_slice())
            .unwrap()
            .contains(0));
        assert!(state
            .get_pair_bitmap(ANNOT_EXPIRATION, &100u64.to_be_bytes())
            .unwrap()
            .contains(0));
        assert!(state
            .get_pair_bitmap(ANNOT_CONTENT_TYPE, b"text/plain")
            .unwrap()
            .contains(0));
        assert!(state.get_pair_bitmap(b"tag", b"music").unwrap().contains(0));
        assert!(state
            .get_pair_bitmap(b"score", &U256::from(7).to_be_bytes::<32>())
            .unwrap()
            .contains(0));
    }

    #[test]
    fn transfer_moves_owner_bitmap() {
        let mut state = MemStore::new();
        let key = entity_key_n(1);
        create(&mut state, alice(), key, 100, 10, vec![], vec![], vec![]).unwrap();
        transfer(&mut state, key, 20, bob()).unwrap();

        assert!(!state
            .get_pair_bitmap(ANNOT_OWNER, alice().as_slice())
            .unwrap()
            .contains(0));
        assert!(state
            .get_pair_bitmap(ANNOT_OWNER, bob().as_slice())
            .unwrap()
            .contains(0));

        let entity = state.get_entity(&entity_address(key)).unwrap().unwrap();
        assert_eq!(entity.owner, bob());
    }

    #[test]
    fn extend_moves_expiration_bitmap() {
        let mut state = MemStore::new();
        let key = entity_key_n(2);
        create(&mut state, alice(), key, 100, 10, vec![], vec![], vec![]).unwrap();
        extend(&mut state, key, 20, 500).unwrap();

        assert!(!state
            .get_pair_bitmap(ANNOT_EXPIRATION, &100u64.to_be_bytes())
            .unwrap()
            .contains(0));
        assert!(state
            .get_pair_bitmap(ANNOT_EXPIRATION, &500u64.to_be_bytes())
            .unwrap()
            .contains(0));

        let entity = state.get_entity(&entity_address(key)).unwrap().unwrap();
        assert_eq!(entity.expires_at, 500);
    }

    #[test]
    fn update_diffs_only_changed_annotations() {
        let mut state = MemStore::new();
        let key = entity_key_n(3);
        create(
            &mut state,
            alice(),
            key,
            100,
            10,
            vec![],
            b"text/plain".to_vec(),
            vec![Attribute {
                key: b"tag".to_vec(),
                value_type: ATTR_STRING,
                value: b"a".to_vec(),
            }],
        )
        .unwrap();
        // Change the tag value; keep content type the same.
        update(
            &mut state,
            key,
            20,
            vec![0xff],
            b"text/plain".to_vec(),
            vec![Attribute {
                key: b"tag".to_vec(),
                value_type: ATTR_STRING,
                value: b"b".to_vec(),
            }],
        )
        .unwrap();

        // tag=a bitmap loses the entity, tag=b gains it.
        assert!(!state.get_pair_bitmap(b"tag", b"a").unwrap().contains(0));
        assert!(state.get_pair_bitmap(b"tag", b"b").unwrap().contains(0));
        // content type unchanged → bitmap still contains it.
        assert!(state
            .get_pair_bitmap(ANNOT_CONTENT_TYPE, b"text/plain")
            .unwrap()
            .contains(0));
        // Owner/expiration untouched.
        assert!(state
            .get_pair_bitmap(ANNOT_OWNER, alice().as_slice())
            .unwrap()
            .contains(0));
    }

    #[test]
    fn delete_clears_bitmaps_and_tombstones_account() {
        let mut state = MemStore::new();
        let key = entity_key_n(4);
        let entity_addr = entity_address(key);
        create(
            &mut state,
            alice(),
            key,
            100,
            10,
            vec![],
            b"text/plain".to_vec(),
            vec![],
        )
        .unwrap();
        delete(&mut state, key).unwrap();

        // Bitmaps drop the entity.
        assert!(!state.get_pair_bitmap(ANNOT_ALL, b"").unwrap().contains(0));
        assert!(!state
            .get_pair_bitmap(ANNOT_OWNER, alice().as_slice())
            .unwrap()
            .contains(0));

        // ID maps cleared (counter not decremented — only the mapping
        // for the deleted ID).
        assert_eq!(state.get_id_to_addr(0).unwrap(), Address::ZERO);
        assert_eq!(state.get_addr_to_id(&entity_addr).unwrap(), 0);

        // Entity tombstoned.
        assert!(state.get_entity(&entity_addr).unwrap().is_none());
    }

    #[test]
    fn insert_pair_bitmap_writes_index_on_first_entity() {
        let mut state = MemStore::new();
        let val = b"hello".to_vec();
        insert_into_pair_bitmap(&mut state, b"tag", &val, 0).unwrap();

        let tree = state.get_index_tree(b"tag").unwrap();
        let vals: Vec<Vec<u8>> = tree.iter_gte(b"").collect();
        assert_eq!(
            vals,
            [b"hello".to_vec()],
            "index should contain value after first entity"
        );

        // Second insert of same value — bitmap was non-empty, ART unchanged.
        insert_into_pair_bitmap(&mut state, b"tag", &val, 1).unwrap();
        let tree2 = state.get_index_tree(b"tag").unwrap();
        assert_eq!(tree2.iter_gte(b"").count(), 1);
    }

    #[test]
    fn remove_pair_bitmap_removes_index_on_last_entity() {
        let mut state = MemStore::new();
        let val = b"hello".to_vec();
        insert_into_pair_bitmap(&mut state, b"tag", &val, 0).unwrap();
        insert_into_pair_bitmap(&mut state, b"tag", &val, 1).unwrap();

        // Remove first entity — bitmap still has entity 1, ART unchanged.
        remove_from_pair_bitmap(&mut state, b"tag", &val, 0).unwrap();
        assert!(
            !state.get_index_tree(b"tag").unwrap().is_empty(),
            "index should survive while entity 1 remains"
        );

        // Remove last entity — bitmap is now empty, index account tombstoned.
        remove_from_pair_bitmap(&mut state, b"tag", &val, 1).unwrap();
        assert!(
            state.get_index_tree(b"tag").unwrap().is_empty(),
            "index should be empty after last entity removed"
        );
    }

    #[test]
    fn expire_has_same_state_path_as_delete() {
        let mut state_a = MemStore::new();
        let mut state_b = MemStore::new();
        let key = entity_key_n(5);
        for state in [&mut state_a, &mut state_b] {
            create(
                state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![],
            )
            .unwrap();
        }
        delete(&mut state_a, key).unwrap();
        expire(&mut state_b, key).unwrap();

        // Equal: both paths produce the same typed state.
        let entity_addr = entity_address(key);
        assert_eq!(
            state_a.get_entity(&entity_addr).unwrap(),
            state_b.get_entity(&entity_addr).unwrap(),
        );
        assert_eq!(state_a.entity_count, state_b.entity_count);
        assert_eq!(
            state_a.get_addr_to_id(&entity_addr).unwrap(),
            state_b.get_addr_to_id(&entity_addr).unwrap(),
        );
        for (k, v) in [
            (ANNOT_ALL, b"".as_slice()),
            (ANNOT_OWNER, alice().as_slice()),
        ] {
            assert_eq!(
                state_a.get_pair_bitmap(k, v).unwrap().to_bytes(),
                state_b.get_pair_bitmap(k, v).unwrap().to_bytes(),
                "mismatch on pair {:?}",
                std::str::from_utf8(k).unwrap_or("?")
            );
        }
    }
}
