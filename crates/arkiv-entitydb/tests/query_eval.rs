//! Integration tests for the query interpreter.
//!
//! Each test builds state via the public op handlers (`create`,
//! `delete`, `transfer`) against an [`InMemoryAdapter`], then parses
//! and evaluates a query and asserts on the resulting ID set.
//! Mirrors `arkiv-storage-service/query/evaluate_test.go` for the v1
//! grammar subset.

use alloy_primitives::{Address, B256, U256};
use arkiv_entitydb::query::parse;
use arkiv_entitydb::test_utils::{InMemoryAdapter, InMemoryStateDb};
use arkiv_entitydb::{
    NumericAnnotation, StringAnnotation, create, delete, resolve_id, transfer,
};

fn alice() -> Address {
    Address::repeat_byte(0xaa)
}
fn bob() -> Address {
    Address::repeat_byte(0xbb)
}
fn carol() -> Address {
    Address::repeat_byte(0xcc)
}
fn key_n(n: u8) -> B256 {
    B256::from([n; 32])
}

fn fresh() -> InMemoryStateDb {
    InMemoryStateDb::with_system_account_preallocated()
}

#[track_caller]
fn ids(state: &mut InMemoryAdapter, q: &str) -> Vec<u64> {
    let parsed = parse(q).unwrap_or_else(|e| panic!("parse {q:?}: {e}"));
    let bm = parsed
        .evaluate(state)
        .unwrap_or_else(|e| panic!("evaluate {q:?}: {e}"));
    let mut out: Vec<u64> = bm.iter().collect();
    out.sort_unstable();
    out
}

fn create_simple(
    state: &mut InMemoryAdapter,
    owner: Address,
    key: B256,
    content_type: &[u8],
    expires_at: u64,
) {
    create(
        state,
        owner,
        key,
        expires_at,
        10,
        b"payload".to_vec(),
        content_type.to_vec(),
        vec![],
        vec![],
    )
    .expect("create");
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

// ── Tests ────────────────────────────────────────────────────────────

#[test]
fn star_and_dollar_all_return_every_live_entity() {
    let mut db = fresh();
    let mut s = InMemoryAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/plain", 200);
    assert_eq!(ids(&mut s, "*"), vec![0, 1]);
    assert_eq!(ids(&mut s, "$all"), vec![0, 1]);
}

#[test]
fn equality_owner_address() {
    let mut db = fresh();
    let mut s = InMemoryAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/plain", 200);
    let q = format!("$owner = 0x{}", hex_lower(alice().as_slice()));
    assert_eq!(ids(&mut s, &q), vec![0]);
}

#[test]
fn equality_content_type() {
    let mut db = fresh();
    let mut s = InMemoryAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, alice(), key_n(2), b"text/html", 200);
    assert_eq!(ids(&mut s, r#"$contentType = "text/html""#), vec![1]);
}

#[test]
fn equality_user_string_annotation() {
    let mut db = fresh();
    let mut s = InMemoryAdapter::new(&mut db);
    create(
        &mut s,
        alice(),
        key_n(1),
        100,
        10,
        b"".to_vec(),
        b"text/plain".to_vec(),
        vec![StringAnnotation {
            key: b"tag".to_vec(),
            value: b"music".to_vec(),
        }],
        vec![],
    )
    .expect("create");
    create(
        &mut s,
        alice(),
        key_n(2),
        100,
        10,
        b"".to_vec(),
        b"text/plain".to_vec(),
        vec![StringAnnotation {
            key: b"tag".to_vec(),
            value: b"video".to_vec(),
        }],
        vec![],
    )
    .expect("create");
    assert_eq!(ids(&mut s, r#"tag = "music""#), vec![0]);
}

#[test]
fn equality_user_numeric_annotation() {
    let mut db = fresh();
    let mut s = InMemoryAdapter::new(&mut db);
    create(
        &mut s,
        alice(),
        key_n(1),
        100,
        10,
        b"".to_vec(),
        b"text/plain".to_vec(),
        vec![],
        vec![NumericAnnotation {
            key: b"score".to_vec(),
            value: U256::from(42),
        }],
    )
    .expect("create");
    create(
        &mut s,
        alice(),
        key_n(2),
        100,
        10,
        b"".to_vec(),
        b"text/plain".to_vec(),
        vec![],
        vec![NumericAnnotation {
            key: b"score".to_vec(),
            value: U256::from(7),
        }],
    )
    .expect("create");
    assert_eq!(ids(&mut s, "score = 42"), vec![0]);
}

#[test]
fn inequality_excludes_match() {
    let mut db = fresh();
    let mut s = InMemoryAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/plain", 200);
    create_simple(&mut s, bob(), key_n(3), b"text/plain", 300);
    let q = format!("$owner != 0x{}", hex_lower(bob().as_slice()));
    assert_eq!(ids(&mut s, &q), vec![0]);
}

#[test]
fn and_intersects() {
    let mut db = fresh();
    let mut s = InMemoryAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/plain", 200);
    create_simple(&mut s, alice(), key_n(3), b"text/html", 300);
    let q = format!(
        r#"$owner = 0x{} && $contentType = "text/plain""#,
        hex_lower(alice().as_slice())
    );
    assert_eq!(ids(&mut s, &q), vec![0]);
}

#[test]
fn or_unions() {
    let mut db = fresh();
    let mut s = InMemoryAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/html", 200);
    create_simple(&mut s, carol(), key_n(3), b"text/xml", 300);
    let q = format!(
        r#"$owner = 0x{} || $owner = 0x{}"#,
        hex_lower(alice().as_slice()),
        hex_lower(bob().as_slice()),
    );
    assert_eq!(ids(&mut s, &q), vec![0, 1]);
}

#[test]
fn inclusion_unions() {
    let mut db = fresh();
    let mut s = InMemoryAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/plain", 200);
    create_simple(&mut s, carol(), key_n(3), b"text/plain", 300);
    let q = format!(
        "$owner IN (0x{} 0x{})",
        hex_lower(alice().as_slice()),
        hex_lower(bob().as_slice()),
    );
    assert_eq!(ids(&mut s, &q), vec![0, 1]);
}

#[test]
fn not_inclusion_subtracts() {
    let mut db = fresh();
    let mut s = InMemoryAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/plain", 200);
    create_simple(&mut s, carol(), key_n(3), b"text/plain", 300);
    let q = format!("$owner NOT IN (0x{})", hex_lower(bob().as_slice()));
    assert_eq!(ids(&mut s, &q), vec![0, 2]);
}

#[test]
fn not_around_paren_subtracts() {
    let mut db = fresh();
    let mut s = InMemoryAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/html", 200);
    create_simple(&mut s, carol(), key_n(3), b"text/plain", 300);
    let q = format!(
        r#"NOT ($owner = 0x{} || $contentType = "text/html")"#,
        hex_lower(alice().as_slice()),
    );
    assert_eq!(ids(&mut s, &q), vec![2]);
}

#[test]
fn delete_removes_from_results() {
    let mut db = fresh();
    let mut s = InMemoryAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/plain", 200);
    delete(&mut s, key_n(1)).expect("delete");
    assert_eq!(ids(&mut s, "*"), vec![1]);
}

#[test]
fn transfer_moves_owner_match() {
    let mut db = fresh();
    let mut s = InMemoryAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    transfer(&mut s, key_n(1), 20, bob()).expect("transfer");

    let q_old = format!("$owner = 0x{}", hex_lower(alice().as_slice()));
    let q_new = format!("$owner = 0x{}", hex_lower(bob().as_slice()));
    assert_eq!(ids(&mut s, &q_old), Vec::<u64>::new());
    assert_eq!(ids(&mut s, &q_new), vec![0]);
}

#[test]
fn expiration_numeric_equality() {
    let mut db = fresh();
    let mut s = InMemoryAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, alice(), key_n(2), b"text/plain", 200);
    assert_eq!(ids(&mut s, "$expiration = 100"), vec![0]);
    assert_eq!(ids(&mut s, "$expiration = 200"), vec![1]);
}

#[test]
fn resolve_id_returns_entity_rlp() {
    let mut db = fresh();
    let mut s = InMemoryAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    let entity = resolve_id(&mut s, 0).expect("resolve").expect("some");
    assert_eq!(entity.owner, alice());
    assert_eq!(entity.expires_at, 100);
    assert_eq!(entity.content_type, b"text/plain".to_vec());
}

#[test]
fn resolve_id_returns_none_after_delete() {
    let mut db = fresh();
    let mut s = InMemoryAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    delete(&mut s, key_n(1)).expect("delete");
    assert!(resolve_id(&mut s, 0).expect("resolve").is_none());
}
