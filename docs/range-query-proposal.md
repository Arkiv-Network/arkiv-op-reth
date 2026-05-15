# Proposal: Two-Tier Trie Index for Range and Glob Queries

## Problem

The existing Tier-1 pair accounts (see §2 and §4.3 of [statedb-design.md](statedb-design.md))
give O(1) equality lookups: for a query `k = v`, derive
`pair_addr = keccak256("arkiv.pair" || k || 0x00 || v)[:20]`, read the bitmap,
done.

Range queries (`price > 100 AND price < 500`) and glob queries
(`tag ~ "image/*"`) require an ordered enumeration over the set of values that
exist for a given key. The pair-account address scheme hashes `(k, v)` together,
so the trie's ordering over addresses has no relation to value ordering. There is
no way to range-scan Tier-1 accounts without reading every pair account ever
written — which is unbounded.

---

## Proposal: Tier-2 ART Index Accounts

Introduce a second kind of trie account — the **index account** — one per
attribute key. The account's `code` holds a serialised **Adaptive Radix Tree
(ART)** over the set of values that currently exist (i.e., are referenced by at
least one live entity) for that key. Because the account sits in the standard
trie, historical versions are retained for free (same mechanism as Tier-1
bitmaps), and no custom MDBX tables are needed.

### Address Derivation

```
index_address(k) = keccak256("arkiv.index" || attribute_key)[:20]
```

The prefix `"arkiv.index"` is disjoint from `"arkiv.pair"`, so addresses from
the two namespaces cannot collide.

### Account Structure

```
Index Account  (address = keccak256("arkiv.index" || attribute_key)[:20])
  nonce    = 1
  codeHash = keccak256(art_bytes)          // content-addressed, same as pair accounts
  code     = serialised_ART_bytes

  storage slots: none
```

The ART is content-addressed in the trie for the same reason pair bitmaps are:
`codeHash = keccak256(art_bytes)` by construction. Historical ART versions are
retained in op-reth's `Bytecodes` table keyed by their `codeHash`, so range
queries against a past block (`atBlock = N`) deserialise the ART as it stood at
that block.

### Value Encoding in the ART

Attribute values follow the three types defined in the
[arkiv-contracts attribute spec](https://github.com/Arkiv-Network/arkiv-contracts/blob/main/docs/architecture.md#attributes).
Each type is encoded into an ART key such that lexicographic byte order matches
the order relevant to range or glob predicates:

| Value type   | Contract encoding                                                  | ART key encoding                                                                                                                 |
| ------------ | ------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------- |
| `UINT`       | 256-bit unsigned integer, first 32 bytes of the 128-byte container | 32-byte big-endian (`[u8; 32]`) — lex order = numeric order                                                                      |
| `STRING`     | UTF-8, left-aligned across 128 bytes, zero-padded                  | Raw UTF-8 bytes with trailing zero bytes stripped — lex order = code-point order                                                 |
| `ENTITY_KEY` | `bytes32` entity key, first 32 bytes of the container              | 32 raw bytes — **not indexed in Tier-2** (range / glob over opaque keys is not meaningful; equality queries use Tier-1 directly) |

The value type is **not** stored inside the ART itself — the query layer knows
the type from the operator being evaluated (`>`, `<=`, `<`, `>=` require `UINT`;
`~` requires `STRING`). Mixing an operator with an incompatible type is a parse
error returned before any trie reads.

The ART does **not** store the pair address — that is re-derivable as
`keccak256("arkiv.pair" || k || 0x00 || v)[:20]` from each value the ART
yields. The ART is an ordered set of `v` bytes.

---

## Range Query Execution

```
Query: price > 100 AND price < 500

1. Encode bounds as 32-byte big-endian UINT:
     lo_key = [0×24 zeros, 0,0,0,0,0,0,0,100]
     hi_key = [0×24 zeros, 0,0,0,0,0,0,1,244]

2. Derive  index_addr = keccak256("arkiv.index" || "price")[:20].
3. Read    index_addr.code  →  deserialise ART.
4. Scan    ART for keys in the open interval (lo_key, hi_key).
           Each key is an encoded value v_i that satisfies the predicate.

5. For each v_i:
     pair_addr_i = keccak256("arkiv.pair" || "price" || 0x00 || v_i)[:20]
     bitmap_i    = deserialise(eth_getCode(pair_addr_i))

6. Union all bitmap_i  →  result_bitmap.

7. Apply cursor / page-size limit; resolve uint64 IDs to entity addresses
   via the system account (same as equality queries).
```

Combined range + equality queries use the standard `&&` / `||` bitmap
combination already implemented for the equality family — range evaluation
produces a bitmap, which is then ANDed or ORed with other sub-expression
bitmaps at the query-tree level.

### Glob / Prefix Query Execution

```
Query: tag ~ "image/*"   (prefix match on "image/")

1. Derive  index_addr = keccak256("arkiv.index" || "tag")[:20].
2. Read and deserialise ART.
3. Scan ART for all keys with prefix bytes "image/" → list of values v_i.
4. Same Tier-1 bitmap lookup + union as for range queries.
```

The ART natively supports prefix iteration, so step 3 is O(matching values),
not O(all values).

---

## Write Path Changes

Every op that touches Tier-1 (add or remove an entity from a pair bitmap) must
also maintain the corresponding Tier-2 ART. The invariant is:

> A value `v` is present in the ART for key `k` iff the Tier-1 pair bitmap
> for `(k, v)` is non-empty after the current op.

### Insert Path (Create, or Update adding a new attribute)

For each `(k, v)` attribute being added where `v` is of type `UINT` or `STRING`
(`ENTITY_KEY` attributes skip Tier-2 maintenance entirely):

1. Update the Tier-1 pair bitmap (existing logic — add `entity_id`).
2. If the bitmap was previously **empty** (this entity is the first to carry
   this `(k, v)` pair), insert `encode(v)` into the ART at `index_addr(k)`.
3. `SetCode(index_addr(k), serialise(updated_ART))`.

Step 2 is a conditional — if the bitmap was already non-empty, the value is
already in the ART; no ART write needed.

### Remove Path (Update dropping an attribute, Delete, Expire)

For each `(k, v)` attribute being removed where `v` is of type `UINT` or `STRING`:

1. Update the Tier-1 pair bitmap (existing logic — remove `entity_id`).
2. If the bitmap is now **empty** (this was the last entity carrying `(k, v)`),
   remove `encode(v)` from the ART at `index_addr(k)`.
3. If the ART was modified, `SetCode(index_addr(k), serialise(updated_ART))`.

### Built-in Attributes

Built-in keys follow the same write path as user-defined attributes, subject to
their value types:

| Built-in key      | Value type                                                     | Tier-2 indexed |
| ----------------- | -------------------------------------------------------------- | -------------- |
| `$expiration`     | `UINT` (block number, fits in lower bytes of 32-byte encoding) | Yes            |
| `$createdAtBlock` | `UINT`                                                         | Yes            |
| `$contentType`    | `STRING`                                                       | Yes            |
| `$owner`          | `ENTITY_KEY` (20-byte address stored as bytes32)               | No             |
| `$creator`        | `ENTITY_KEY`                                                   | No             |
| `$key`            | `ENTITY_KEY`                                                   | No             |
| `$all`            | synthetic (no value)                                           | No             |

`$owner` and `$creator` store Ethereum addresses — these are equality targets,
not range targets, so excluding them from Tier-2 avoids unnecessary ART writes.

---

## Gas Model

Range and glob queries execute on the read path (no gas). On the write path,
ART maintenance adds to the per-op gas charge:

| Surcharge               | When                                                                 |
| ----------------------- | -------------------------------------------------------------------- |
| `G_ART_READ` (≈ 5 000)  | Any attribute write that requires reading an existing ART            |
| `G_ART_WRITE` (≈ 8 000) | Any attribute write that modifies an ART (value inserted or removed) |

These are charged per distinct `UINT`/`STRING` attribute key modified, not per
entity. `ENTITY_KEY` attributes incur no ART charge. Both surcharges are pure
functions of the op calldata (specifically, whether a given `(k, v)` attribute
is being added or removed), satisfying the consensus-determinism requirement.

The exact constants need calibration against the ART serialisation cost for
realistic tree sizes.

---

## Verification

Index accounts are content-addressed in the trie by the same mechanism as pair
accounts. A client can verify any range query result:

1. `eth_getProof(index_addr, [], blockN)` — binds `codeHash` to `stateRoot_N`.
2. `eth_getCode(index_addr, blockN)` — retrieves ART bytes.
3. Verify `keccak256(art_bytes) == codeHash`.
4. Deserialise ART; scan for the range; re-derive pair addresses; fetch and
   verify each bitmap exactly as for equality queries (§7 of statedb-design.md).

---

## ART Serialisation Format

The ART node types (Node4, Node16, Node48, Node256, Leaf) are serialised to a
compact binary format. Key properties:

- Deterministic byte output for identical trees (required for `codeHash` to be
  consensus-stable across nodes).
- Variable-length path compression stored in inner nodes (compressed ART).
- Leaf payload: none — the leaf key itself is the value encoding.

Candidate implementations: the `art` crate or a bespoke encoder tailored to
the fixed-width and variable-length key shapes used here. The format must be
pinned at genesis; any future change to the format is a hard fork.

---

## Properties

| Property                  | Tier-2 ART design                                               |
| ------------------------- | --------------------------------------------------------------- |
| Range queries             | Yes — bounded by matching distinct values, not entity count     |
| Glob / prefix queries     | Yes — ART prefix scan                                           |
| Historical range queries  | Yes — ART version retained per block via `Bytecodes` table      |
| Content-addressed in trie | Yes — `codeHash = keccak256(art_bytes)`                         |
| Verifiable by client      | Yes — same `eth_getProof` + `eth_getCode` pattern               |
| Custom MDBX tables        | None                                                            |
| Reorg safe                | Yes — `SetCode` goes through revm journal                       |
| Gas deterministic         | Yes — pure function of op shape                                 |
| Write overhead per op     | One ART read + conditional write per distinct key added/removed |

---

## Open Questions

1. **ART serialisation format finalisation.** Must be pinned and deterministic
   before mainnet. Which crate / spec to adopt?

2. **ART account tombstoning.** If all values for a given key are removed (the
   ART becomes empty), should the index account be tombstoned (empty code,
   nonce kept at 1) to avoid trie bloat? Same consideration as entity account
   tombstoning on Delete.

3. **Large cardinality keys.** A key used as a timestamp or sequence number
   will accumulate one ART leaf per distinct value ever seen. ART is
   space-efficient for dense integer ranges (Node256 packs 256 children in one
   node), but very high cardinality keys may still produce non-trivial code
   size. Worth measuring with realistic data before setting gas constants.
