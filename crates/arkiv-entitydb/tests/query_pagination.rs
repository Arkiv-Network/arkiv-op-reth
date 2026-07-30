//! Pagination bounds for [`arkiv_entitydb::query::execute`].
//!
//! `page_size` bounds a page by entity *count*, which says nothing
//! about its size — payload length is limited only by the gas paid at
//! write time. These tests pin the `max_page_bytes` budget that bounds
//! a page by size as well, and the invariants a paging client depends
//! on: forward progress, no duplicates, no dropped entities.

use alloy_primitives::{Address, B256};
use arkiv_entitydb::query::{PageParams, entity_size, execute};
use arkiv_entitydb::test_utils::{InMemoryStateAdapter, InMemoryStateDb};
use arkiv_entitydb::{ATTR_STRING, Attribute, create};

const KIB: usize = 1024;
const MIB: u64 = 1024 * 1024;

fn owner() -> Address {
    Address::repeat_byte(0xaa)
}

/// `entity_address` truncates the key to its first 20 bytes, so the
/// index must live in the leading bytes or every entity collides on one
/// account. Index 0 is skipped: an all-zero key maps to the zero
/// address, which `resolve_id` treats as absent. Real keys are
/// `keccak256(chain_id, ARKIV_ADDRESS, owner, nonce)`, so that shape is
/// unreachable on chain.
fn key_for(i: u64) -> B256 {
    let mut key = [0u8; 32];
    key[..8].copy_from_slice(&(i + 1).to_be_bytes());
    B256::from(key)
}

fn seed(state: &mut InMemoryStateAdapter, count: u64, payload_bytes: usize) {
    for i in 0..count {
        create(
            state,
            owner(),
            key_for(i),
            u64::MAX,
            1,
            vec![0xab; payload_bytes],
            b"application/octet-stream".to_vec(),
            vec![Attribute {
                key: b"batch".to_vec(),
                value_type: ATTR_STRING,
                value: b"page-test".to_vec(),
            }],
        )
        .expect("create");
    }
}

fn params(page_size: u64, cursor: Option<u64>, max_page_bytes: Option<u64>) -> PageParams {
    PageParams {
        page_size,
        cursor,
        max_page_bytes,
    }
}

#[test]
fn byte_budget_truncates_page_and_reports_a_cursor() {
    let mut db = InMemoryStateDb::default();
    let mut state = InMemoryStateAdapter::new(&mut db);
    seed(&mut state, 200, 64 * KIB);

    let budget = 4 * MIB;
    let page = execute(&mut state, "$all", params(200, None, Some(budget))).expect("execute");

    let total: u64 = page.entries.iter().map(entity_size).sum();
    assert!(
        total <= budget,
        "page of {} entities used {total} bytes, over the {budget} budget",
        page.entries.len()
    );
    assert!(
        page.entries.len() < 200,
        "expected the budget to truncate the page, got all 200 entities"
    );
    assert!(
        page.next_cursor.is_some(),
        "a truncated page must report a cursor or the remainder is unreachable"
    );
}

#[test]
fn cursor_walk_returns_every_entity_exactly_once() {
    const COUNT: u64 = 200;
    let mut db = InMemoryStateDb::default();
    let mut state = InMemoryStateAdapter::new(&mut db);
    seed(&mut state, COUNT, 64 * KIB);

    let mut seen: Vec<B256> = Vec::new();
    let mut cursor = None;
    // Every page must yield >= 1 entity, so COUNT iterations is a
    // generous bound — exceeding it means pagination stalled.
    for _ in 0..COUNT + 1 {
        let page =
            execute(&mut state, "$all", params(200, cursor, Some(4 * MIB))).expect("execute");
        assert!(
            !page.entries.is_empty(),
            "a page with a cursor must make progress"
        );
        seen.extend(page.entries.iter().map(|e| e.key));
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }

    let mut unique = seen.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), seen.len(), "cursor walk returned duplicates");
    assert_eq!(
        seen.len(),
        COUNT as usize,
        "cursor walk dropped entities: got {} of {COUNT}",
        seen.len()
    );

    // Descending by entity ID (newest first) across page boundaries.
    let mut descending = seen.clone();
    descending.sort_unstable();
    descending.reverse();
    assert_eq!(seen, descending, "cursor walk broke newest-first ordering");
}

#[test]
fn entity_larger_than_the_whole_budget_is_still_returned() {
    let mut db = InMemoryStateDb::default();
    let mut state = InMemoryStateAdapter::new(&mut db);
    seed(&mut state, 3, 128 * KIB);

    // Budget smaller than a single entity: the page must still contain
    // one, otherwise a client paging with this budget can never advance.
    let page = execute(&mut state, "$all", params(200, None, Some(1024))).expect("execute");

    assert_eq!(page.entries.len(), 1);
    assert!(page.next_cursor.is_some(), "remaining entities unreachable");
}

#[test]
fn no_budget_leaves_page_size_as_the_only_bound() {
    let mut db = InMemoryStateDb::default();
    let mut state = InMemoryStateAdapter::new(&mut db);
    seed(&mut state, 50, 64 * KIB);

    let page = execute(&mut state, "$all", params(200, None, None)).expect("execute");

    assert_eq!(page.entries.len(), 50);
    assert!(page.next_cursor.is_none());
}

#[test]
fn budget_that_is_never_reached_does_not_add_a_cursor() {
    let mut db = InMemoryStateDb::default();
    let mut state = InMemoryStateAdapter::new(&mut db);
    seed(&mut state, 10, 16);

    let page = execute(&mut state, "$all", params(200, None, Some(4 * MIB))).expect("execute");

    assert_eq!(page.entries.len(), 10);
    assert!(
        page.next_cursor.is_none(),
        "no cursor when the page held every match"
    );
}

#[test]
fn count_cap_still_applies_under_a_generous_budget() {
    let mut db = InMemoryStateDb::default();
    let mut state = InMemoryStateAdapter::new(&mut db);
    seed(&mut state, 30, 16);

    let page = execute(&mut state, "$all", params(10, None, Some(64 * MIB))).expect("execute");

    assert_eq!(page.entries.len(), 10);
    assert!(page.next_cursor.is_some());
}
