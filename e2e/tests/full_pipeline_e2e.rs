//! End-to-end narrative test exercising the whole Arkiv stack.
//!
//! Boots an `ArkivOpNode` once, then walks through a story:
//!
//! 1. CREATE — three signers, varied payloads, content types, and
//!    attribute mixes (strings, numerics, entity-key refs).
//! 2. Query built-ins — `$owner`, `$contentType`, `$creator`.
//! 3. Query user annotations — string equality + numeric equality.
//! 4. Query boolean combinators — `AND`, `OR`, nested parens.
//! 5. Inclusion — `IN`, `NOT IN`.
//! 6. UPDATE — payload + annotation change; verify the old-value
//!    bitmap empties out and the new-value bitmap fills.
//! 7. EXTEND — `$expiration` moves cleanly.
//! 8. TRANSFER — `$owner` moves; non-owner UPDATE reverts.
//! 9. DELETE — entity disappears from every bitmap.
//! 10. atBlock — historical query observes pre-transfer state.
//! 11. Pagination — 30 entities, page_size=10, follow cursor across
//!     three pages with no overlap.
//!
//! All op submission, ABI encoding, signing, and query plumbing
//! lives in [`arkiv_e2e`] (the crate's `src/lib.rs`). This file is
//! pure narrative + assertions.

use alloy_primitives::{Address, B256};
use arkiv_e2e::{CreateOp, UpdateOp, WorldOps, boot, OP_UPDATE, Operation};
use arkiv_node::rpc::EntityData;

/// Hex-encode without `0x` prefix — matches the wire format the SDK
/// + storage-service use for query string literals.
fn hex_addr(a: Address) -> String {
    let mut s = String::with_capacity(40);
    for b in a.as_slice() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn hex_key(k: B256) -> String {
    let mut s = String::with_capacity(64);
    for b in k.as_slice() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn ids_owned_by(results: &[EntityData], owner: Address) -> Vec<B256> {
    results
        .iter()
        .filter(|e| e.owner == Some(owner))
        .map(|e| e.key)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn full_pipeline() -> eyre::Result<()> {
    let mut world = boot().await?;
    let alice = world.address(0);
    let bob = world.address(1);
    let carol = world.address(2);

    // ── 1. CREATE: three entities, varied annotation mix ────────────
    //
    // alice: text/plain + tag="music" + score=42
    // bob:   text/html  + tag="news"  + score=7
    // carol: image/png  + tag="music" (only) + ref → alice's entity key

    let alice_key = world
        .create(
            0,
            CreateOp::new()
                .payload(b"alice payload".to_vec())
                .content_type("text/plain")
                .btl(1_000)
                .string_attr("tag", "music")
                .numeric_attr("score", 42),
        )
        .await?;

    let bob_key = world
        .create(
            1,
            CreateOp::new()
                .payload(b"bob payload".to_vec())
                .content_type("text/html")
                .btl(1_000)
                .string_attr("tag", "news")
                .numeric_attr("score", 7),
        )
        .await?;

    let carol_key = world
        .create(
            2,
            CreateOp::new()
                .payload(b"carol payload".to_vec())
                .content_type("image/png")
                .btl(1_000)
                .string_attr("tag", "music")
                .entity_key_attr("ref", alice_key),
        )
        .await?;

    let all = world.query("*").await?;
    assert_eq!(all.len(), 3, "all entities count");

    // Sanity: every entity's `key` round-trips through the wire format.
    let returned_keys: Vec<B256> = all.iter().map(|e| e.key).collect();
    assert!(returned_keys.contains(&alice_key));
    assert!(returned_keys.contains(&bob_key));
    assert!(returned_keys.contains(&carol_key));

    // ── 2. Query built-ins ──────────────────────────────────────────

    let alice_owned = world.query(&format!("$owner = 0x{}", hex_addr(alice))).await?;
    assert_eq!(ids_owned_by(&alice_owned, alice), vec![alice_key]);

    let html = world.query(r#"$contentType = "text/html""#).await?;
    assert_eq!(html.len(), 1);
    assert_eq!(html[0].key, bob_key);

    let bob_created = world.query(&format!("$creator = 0x{}", hex_addr(bob))).await?;
    assert_eq!(bob_created.len(), 1);
    assert_eq!(bob_created[0].key, bob_key);

    let by_key = world.query(&format!("$key = 0x{}", hex_key(alice_key))).await?;
    assert_eq!(by_key.len(), 1);
    assert_eq!(by_key[0].key, alice_key);

    // ── 3. Query user annotations ───────────────────────────────────

    let music = world.query(r#"tag = "music""#).await?;
    assert_eq!(music.len(), 2, "alice + carol tagged music");

    let score_42 = world.query("score = 42").await?;
    assert_eq!(score_42.len(), 1);
    assert_eq!(score_42[0].key, alice_key);

    // ── 4. Boolean combinators ──────────────────────────────────────

    let plain_and_music = world.query(r#"$contentType = "text/plain" && tag = "music""#).await?;
    assert_eq!(plain_and_music.len(), 1);
    assert_eq!(plain_and_music[0].key, alice_key);

    let news_or_image = world
        .query(r#"tag = "news" || $contentType = "image/png""#)
        .await?;
    assert_eq!(news_or_image.len(), 2, "bob + carol");

    // Nested parens — `(a || b) && c`
    let q = format!(
        r#"(tag = "music" || tag = "news") && $owner = 0x{}"#,
        hex_addr(alice),
    );
    assert_eq!(world.query(&q).await?.len(), 1, "alice has tag=music");

    // NOT around a paren
    let q = r#"NOT (tag = "music")"#;
    let not_music = world.query(q).await?;
    assert_eq!(not_music.len(), 1);
    assert_eq!(not_music[0].key, bob_key);

    // ── 5. Inclusion ────────────────────────────────────────────────

    let in_tags = world.query(r#"tag IN ("music" "news")"#).await?;
    assert_eq!(in_tags.len(), 3);

    let not_in_tags = world.query(r#"tag NOT IN ("music")"#).await?;
    assert_eq!(not_in_tags.len(), 1);
    assert_eq!(not_in_tags[0].key, bob_key);

    let by_score_set = world.query("score IN (7 42)").await?;
    assert_eq!(by_score_set.len(), 2, "alice + bob have those scores");

    // ── 6. UPDATE: change payload + annotations ─────────────────────

    world
        .update(
            0,
            alice_key,
            UpdateOp::new()
                .payload(b"alice v2".to_vec())
                .content_type("text/plain")
                .string_attr("tag", "podcast")
                .numeric_attr("score", 100),
        )
        .await?;

    let old_tag = world.query(r#"tag = "music""#).await?;
    assert_eq!(old_tag.len(), 1, "only carol still has tag=music");
    assert_eq!(old_tag[0].key, carol_key);

    let new_tag = world.query(r#"tag = "podcast""#).await?;
    assert_eq!(new_tag.len(), 1);
    assert_eq!(new_tag[0].key, alice_key);
    assert_eq!(
        new_tag[0].value.as_ref().map(|b| b.as_ref()),
        Some(b"alice v2".as_slice()),
    );

    // Old score=42 bitmap should be empty for alice.
    assert!(world.query("score = 42").await?.is_empty());
    let score_100 = world.query("score = 100").await?;
    assert_eq!(score_100.len(), 1);
    assert_eq!(score_100[0].key, alice_key);

    // ── 7. EXTEND ───────────────────────────────────────────────────

    let bob_entity = world.query(&format!("$key = 0x{}", hex_key(bob_key))).await?;
    let old_expiration = bob_entity[0].expires_at.expect("expires_at included by default");
    world.extend(1, bob_key, 5_000).await?;

    let bob_entity = world.query(&format!("$key = 0x{}", hex_key(bob_key))).await?;
    let new_expiration = bob_entity[0].expires_at.expect("expires_at included by default");
    assert!(
        new_expiration > old_expiration,
        "extend should raise expiration: {old_expiration} -> {new_expiration}"
    );

    // Old expiration bitmap is empty; new one contains bob.
    assert!(world.query(&format!("$expiration = {old_expiration}")).await?.is_empty());
    let still_bob = world.query(&format!("$expiration = {new_expiration}")).await?;
    assert_eq!(still_bob.len(), 1);
    assert_eq!(still_bob[0].key, bob_key);

    // ── 8. TRANSFER + negative-path UPDATE from old owner ───────────

    // Capture the head BEFORE the transfer for the historical assertion.
    let block_before_transfer = world.head_block().await?;

    world.transfer(0, alice_key, carol).await?;

    let alice_owned = world.query(&format!("$owner = 0x{}", hex_addr(alice))).await?;
    assert!(
        alice_owned.is_empty(),
        "alice no longer owns anything: {:?}",
        alice_owned.iter().map(|e| e.key).collect::<Vec<_>>()
    );
    let carol_owned = world.query(&format!("$owner = 0x{}", hex_addr(carol))).await?;
    let carol_keys: Vec<B256> = carol_owned.iter().map(|e| e.key).collect();
    assert!(carol_keys.contains(&alice_key));
    assert!(carol_keys.contains(&carol_key));

    // Negative: alice (the old owner) tries to UPDATE — must revert
    // at the contract level (status=0x0). The contract enforces
    // owner check before reaching the precompile.
    let bad_update = Operation {
        operationType: OP_UPDATE,
        entityKey: alice_key,
        payload: alloy_primitives::Bytes::from_static(b"sneaky"),
        contentType: arkiv_e2e::Mime128 { data: Default::default() },
        attributes: vec![],
        btl: 0,
        newOwner: Address::ZERO,
    };
    world
        .submit_expecting_revert(0, bad_update, "UPDATE by non-owner")
        .await?;

    // ── 9. DELETE — bob deletes his entity ──────────────────────────

    world.delete(1, bob_key).await?;

    let bob_gone = world.query(&format!("$key = 0x{}", hex_key(bob_key))).await?;
    assert!(bob_gone.is_empty(), "bob's entity should be gone from $key bitmap");

    let by_news = world.query(r#"tag = "news""#).await?;
    assert!(by_news.is_empty(), "bob's tag=news bitmap should be empty");

    let by_owner_bob = world.query(&format!("$owner = 0x{}", hex_addr(bob))).await?;
    assert!(by_owner_bob.is_empty(), "$owner=bob bitmap should be empty");

    // ── 10. Historical query (atBlock) ──────────────────────────────
    //
    // Re-run the alice-owned query at `block_before_transfer` and
    // expect alice's entity back, even though at head it belongs to
    // carol.

    let historical = world
        .query_at(
            &format!("$owner = 0x{}", hex_addr(alice)),
            block_before_transfer,
        )
        .await?;
    assert_eq!(
        historical.len(),
        1,
        "at block {block_before_transfer} alice still owned 1 entity",
    );
    assert_eq!(historical[0].key, alice_key);

    // ── 11. Pagination: 30 entities, page_size=10, follow cursor ───
    //
    // Bulk-create 30 entities owned by alice with a shared
    // `bulk=true` annotation so we can isolate them from the
    // pre-existing ones.

    for i in 0..30 {
        world
            .create(
                0,
                CreateOp::new()
                    .payload(format!("bulk-{i}").into_bytes())
                    .content_type("application/octet-stream")
                    .btl(1_000)
                    .string_attr("bulk", "true"),
            )
            .await?;
    }

    let all_bulk = world.query_paginated(r#"bulk = "true""#, 10).await?;
    assert_eq!(all_bulk.len(), 30, "all 30 bulk entities returned across pages");

    // No duplicates between pages.
    let mut keys: Vec<B256> = all_bulk.iter().map(|e| e.key).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), 30, "all returned keys are unique across pages");

    Ok(())
}
