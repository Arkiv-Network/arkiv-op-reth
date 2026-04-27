# ExEx -> EntityDB JSON-RPC Interface (v2)

This document supersedes the original `exex-jsonrpc-interface.md`. It addresses three structural problems in the v1 wire format:

1. **Lost transaction granularity** -- v1 flattened all operations into a single per-block array, losing which operations came from which transaction.
2. **Missing changeset hash** -- the contract's rolling `changeSetHash` was not included in the wire format, so the EntityDB had no way to verify consistency with on-chain state.
3. **Empty block semantics** -- clarified and formalised.

---

## Contract Prerequisite

The `EntityOperation` event must be extended with a `changesetHash` field:

```solidity
event EntityOperation(
    bytes32 indexed entityKey,
    uint8   indexed operationType,
    address indexed owner,
    uint32  expiresAt,
    bytes32 entityHash,
    bytes32 changesetHash   // NEW: rolling hash after this operation
);
```

This is the only contract change required. The hash is already computed in the contract -- it just needs to be emitted. After this change, `arkiv-bindings` regenerates automatically via `build.rs`.

---

## Data Model

### Hierarchy

```
ArkivBlock
  +-- header: ArkivBlockHeader
  +-- transactions: ArkivTransaction[]    // may be empty
        +-- hash: B256
        +-- index: u32
        +-- sender: Address
        +-- operations: ArkivOperation[]  // 1:1 with EntityOperation events
              +-- changesetHash: B256     // rolling hash after this op
```

This preserves the full chain of provenance: block -> transaction -> operation. The EntityDB can reconstruct exactly which account submitted which operations in which transaction.

### Block Header

```rust
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArkivBlockHeader {
    /// Block number (hex-encoded in JSON).
    #[serde(with = "u64_hex")]
    pub number: u64,
    /// Block hash.
    pub hash: B256,
    /// Parent block hash (for continuity verification).
    pub parent_hash: B256,
    /// Rolling changeset hash *as of the end of this block*. Equals the
    /// last operation's `changeset_hash` if this block has operations,
    /// otherwise the rolling hash carried forward from the most recent
    /// prior block that had operations. `0x000...000` only when no
    /// operation has ever been recorded as of this block.
    pub changeset_hash: B256,
}
```

The block-level `changeset_hash` is a convenience -- it's derivable from the last operation's hash but saves the EntityDB from scanning. For empty blocks it carries forward the previous block's value, confirming no state change.

**Sourcing.** Within a single chain notification the ExEx carries the
rolling value forward across blocks. The starting value (rolling hash at
the parent of the first block in the notification) is read directly from
`EntityRegistry`'s storage at the parent block's state, using the slot
layout exposed by `arkiv_bindings::storage_layout`. This intentionally
differs from the contract's `changeSetHashAtBlock(N)` view, which returns
`bytes32(0)` for any empty block.

### Block

```rust
#[derive(Serialize)]
pub struct ArkivBlock {
    pub header: ArkivBlockHeader,
    /// All transactions targeting the EntityRegistry in this block.
    /// Empty array for blocks with no Arkiv activity.
    pub transactions: Vec<ArkivTransaction>,
}
```

### Transaction

```rust
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArkivTransaction {
    /// Transaction hash.
    pub hash: B256,
    /// Transaction index within the block.
    pub index: u32,
    /// Transaction sender (recovered from signature).
    pub sender: Address,
    /// Decoded operations from this transaction's execute() call.
    /// Ordered by position in the Operation[] calldata array.
    pub operations: Vec<ArkivOperation>,
}
```

### Operation

```rust
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ArkivOperation {
    Create(CreateOp),
    Update(UpdateOp),
    Extend(ExtendOp),
    #[serde(rename = "transfer")]
    Transfer(TransferOp),
    Delete(DeleteOp),
    Expire(ExpireOp),
}
```

Each variant carries a `changeset_hash` field -- the contract's rolling hash immediately after this operation was applied.

```rust
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateOp {
    pub op_index: u32,
    pub entity_key: B256,
    pub owner: Address,
    #[serde(with = "u64_hex")]
    pub expires_at: u64,
    pub entity_hash: B256,
    pub changeset_hash: B256,
    pub payload: Bytes,
    pub content_type: String,
    pub attributes: Vec<Attribute>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateOp {
    pub op_index: u32,
    pub entity_key: B256,
    pub owner: Address,
    pub entity_hash: B256,
    pub changeset_hash: B256,
    pub payload: Bytes,
    pub content_type: String,
    pub attributes: Vec<Attribute>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtendOp {
    pub op_index: u32,
    pub entity_key: B256,
    pub owner: Address,
    #[serde(with = "u64_hex")]
    pub expires_at: u64,
    pub entity_hash: B256,
    pub changeset_hash: B256,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferOp {
    pub op_index: u32,
    pub entity_key: B256,
    pub owner: Address,
    pub entity_hash: B256,
    pub changeset_hash: B256,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteOp {
    pub op_index: u32,
    pub entity_key: B256,
    pub owner: Address,
    pub entity_hash: B256,
    pub changeset_hash: B256,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpireOp {
    pub op_index: u32,
    pub entity_key: B256,
    pub owner: Address,
    pub entity_hash: B256,
    pub changeset_hash: B256,
}
```

### Attribute

Mirrors the contract's `Attribute` type. Three variants, discriminated by
which value field is present (`#[serde(untagged)]`):

```rust
#[derive(Serialize)]
#[serde(untagged, rename_all = "camelCase")]
pub enum Attribute {
    /// `ATTR_STRING` — opaque UTF-8 (≤128 bytes per `value128-encoding.md`).
    String    { key: String, string_value: String },
    /// `ATTR_UINT` — right-aligned big-endian uint256 from the contract's
    /// `bytes32[4]` value. Serialized as a `0x`-prefixed lowercase hex string.
    Numeric   { key: String, numeric_value: U256 },
    /// `ATTR_ENTITY_KEY` — cross-reference to another entity. Serialized
    /// as a 32-byte hex string.
    EntityKey { key: String, entity_key: B256 },
}
```

Unknown `valueType` values from the contract are skipped (with a warning
logged); they do not appear in the wire payload.

### Block Reference (for reverts)

```rust
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArkivBlockRef {
    #[serde(with = "u64_hex")]
    pub number: u64,
    pub hash: B256,
}
```

---

## Methods

### arkiv_commitChain

Apply a contiguous sequence of blocks to the EntityDB's canonical head. Blocks must be oldest-first and include every canonical block (even those with no Arkiv transactions). The EntityDB applies them in order; if any block fails, the call returns an error and no state from that call is committed.

**Request:**

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "arkiv_commitChain",
  "params": [{
    "blocks": [
      {
        "header": {
          "number": "0x3039",
          "hash": "0xabc...",
          "parentHash": "0xdef...",
          "changesetHash": "0xfed..."
        },
        "transactions": [
          {
            "hash": "0x111...",
            "index": 0,
            "sender": "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
            "operations": [
              {
                "type": "create",
                "opIndex": 0,
                "entityKey": "0x...",
                "owner": "0xf39F...",
                "expiresAt": "0x34bc",
                "entityHash": "0x...",
                "changesetHash": "0xaaa...",
                "payload": "0xdeadbeef...",
                "contentType": "application/octet-stream",
                "attributes": [
                  { "key": "linked.to", "entityKey": "0xabc..." },
                  { "key": "priority", "numericValue": "0x2a" },
                  { "key": "type", "stringValue": "note" }
                ]
              },
              {
                "type": "create",
                "opIndex": 1,
                "entityKey": "0x...",
                "owner": "0xf39F...",
                "expiresAt": "0x34bc",
                "entityHash": "0x...",
                "changesetHash": "0xbbb...",
                "payload": "0xcafe...",
                "contentType": "text/plain",
                "attributes": []
              }
            ]
          },
          {
            "hash": "0x222...",
            "index": 1,
            "sender": "0xaaaa...",
            "operations": [
              {
                "type": "delete",
                "opIndex": 0,
                "entityKey": "0x...",
                "owner": "0xaaaa...",
                "entityHash": "0x...",
                "changesetHash": "0xfed..."
              }
            ]
          }
        ]
      },
      {
        "header": {
          "number": "0x303a",
          "hash": "0x123...",
          "parentHash": "0xabc...",
          "changesetHash": "0xfed..."
        },
        "transactions": []
      }
    ]
  }]
}
```

Note in the example above:
- Block `0x3039` has two transactions with a total of three operations. The block's `changesetHash` (`0xfed...`) matches the last operation's `changesetHash`.
- Block `0x303a` is empty -- its `changesetHash` carries forward from the previous block.

**Response (success):**

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "stateRoot": "0x7f8e..."
  }
}
```

`stateRoot` is the EntityDB's trie root after applying the last block in the batch.

**Response (error):**

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": {
    "code": -32001,
    "message": "parent hash mismatch at block 0x303a",
    "data": { "expected": "0xabc...", "got": "0x999..." }
  }
}
```

### arkiv_revert

Revert a contiguous sequence of blocks from the canonical head back to the common ancestor. Blocks are identified by number and hash only -- the EntityDB uses its journal to undo state changes. Blocks must be newest-first.

**Request:**

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "arkiv_revert",
  "params": [{
    "blocks": [
      { "number": "0x303b", "hash": "0x..." },
      { "number": "0x303a", "hash": "0x..." }
    ]
  }]
}
```

**Response (success):**

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "stateRoot": "0xaaa..."
  }
}
```

### arkiv_reorg

Atomically revert a set of blocks and commit a new set. Semantically equivalent to `arkiv_revert` followed by `arkiv_commitChain`, but issued as a single call so the EntityDB never exposes an intermediate state to concurrent query clients.

**Request:**

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "arkiv_reorg",
  "params": [{
    "revertedBlocks": [
      { "number": "0x303a", "hash": "0x..." },
      { "number": "0x3039", "hash": "0x..." }
    ],
    "newBlocks": [
      {
        "header": { "number": "0x3039", "hash": "0x...", "parentHash": "0x...", "changesetHash": "0x0000000000000000000000000000000000000000000000000000000000000000" },
        "transactions": []
      },
      {
        "header": { "number": "0x303a", "hash": "0x...", "parentHash": "0x...", "changesetHash": "0xccc..." },
        "transactions": [
          {
            "hash": "0x333...",
            "index": 0,
            "sender": "0xbbbb...",
            "operations": [
              {
                "type": "delete",
                "opIndex": 0,
                "entityKey": "0x...",
                "owner": "0xbbbb...",
                "entityHash": "0x...",
                "changesetHash": "0xccc..."
              }
            ]
          }
        ]
      }
    ]
  }]
}
```

**Response (success):**

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "stateRoot": "0xbbb..."
  }
}
```

---

## Empty Block Semantics

Every canonical block is forwarded, regardless of whether it contains Arkiv transactions.

| Block state | `transactions` | `changesetHash` |
|---|---|---|
| Has Arkiv txs | Populated array | Last operation's changeset hash |
| No Arkiv txs | `[]` | Previous block's changeset hash (carried forward) |
| Genesis (before any ops) | `[]` | `0x000...000` |

The EntityDB uses empty blocks to:
1. Advance its internal block cursor (for continuity checks)
2. Verify that the changeset hash has not diverged
3. Maintain a 1:1 mapping between canonical blocks and EntityDB state snapshots

---

## Changeset Hash Verification

The `changesetHash` enables trustless verification between the ExEx and EntityDB:

1. The contract computes a rolling hash: `keccak256(prevHash, entityKey, entityHash)` per operation
2. The ExEx reads this from the `EntityOperation` event's `changesetHash` field and includes it in the wire format
3. The EntityDB independently computes the same rolling hash as it applies operations
4. After each block, the EntityDB compares its computed hash against the block header's `changesetHash`
5. A mismatch indicates a decode error, missed operation, or state corruption

This is the same verification that can be performed on-chain via `EntityRegistry.changeSetHash()`, giving the EntityDB the ability to detect inconsistencies without an on-chain call.

---

## Error Codes

| Code | Meaning |
|------|---------|
| -32001 | Parent hash mismatch -- block's `parentHash` doesn't match the EntityDB's current head |
| -32002 | Block not found -- revert requested for a block the EntityDB hasn't processed |
| -32003 | Out of order -- blocks not in the expected sequence |
| -32004 | Unknown entity -- operation references an entity that doesn't exist in the EntityDB |
| -32005 | Internal error -- EntityDB trie or storage failure |
| -32006 | Changeset hash mismatch -- EntityDB's computed hash diverges from the block's declared hash |

Standard JSON-RPC errors (-32600 through -32603) apply for malformed requests, invalid params, etc.

---

## Storage Trait (revised)

```rust
pub trait Storage: Send + Sync + 'static {
    /// Process a chain of committed blocks (oldest-first).
    /// Returns the state root after the last block, if the backend produces one.
    fn handle_commit(&self, blocks: &[ArkivBlock]) -> Result<Option<B256>>;

    /// Revert blocks (newest-first).
    /// Returns the state root after reverting to the common ancestor.
    fn handle_revert(&self, blocks: &[ArkivBlockRef]) -> Result<Option<B256>>;

    /// Atomically revert old blocks and commit new blocks (reorg).
    /// Returns the state root after applying the new chain.
    fn handle_reorg(
        &self,
        reverted: &[ArkivBlockRef],
        new_blocks: &[ArkivBlock],
    ) -> Result<Option<B256>>;
}
```

- `LoggingStore` returns `None` for state root (no trie).
- `JsonRpcStore` returns `Some(root)` from the EntityDB response.

---

## ExEx Decoding Pipeline

For each block in a chain notification:

```
block (RecoveredBlock<OpBlock>)
  |
  +-- extract header: number, hash, parent_hash
  |
  +-- for each (tx, receipt) where tx.to == ENTITY_REGISTRY_ADDRESS:
  |     |
  |     +-- recover sender from tx signature
  |     +-- decode execute() calldata -> Operation[]
  |     +-- parse EntityOperation event logs (1:1 with operations)
  |     +-- for each (operation, event_log):
  |           +-- map to ArkivOperation variant
  |           +-- extract changesetHash from event
  |     +-- wrap in ArkivTransaction { hash, index, sender, operations }
  |
  +-- set header.changeset_hash = last operation's hash, or previous block's
  +-- emit ArkivBlock { header, transactions }
```

Blocks with no matching transactions emit `ArkivBlock { header, transactions: [] }`.

---

## Wire Format Notes

- All `u64` values (block numbers, expiry) are hex-encoded with `0x` prefix
- Addresses are checksummed hex (`0x` + 40 hex chars)
- Bytes/payload are `0x`-prefixed hex
- B256 hashes are `0x`-prefixed hex (64 hex chars)
- `index` and `opIndex` are JSON integers (u32)
- Empty arrays are `[]`, not omitted
- `null` is used for `changesetHash` on genesis/pre-operation blocks

---

## Transport

HTTP POST to the EntityDB's ingest endpoint (configurable URL, default `http://localhost:9545`). No authentication for MVP -- the ExEx and EntityDB run on the same host. TLS and auth tokens can be added later.

The ExEx should use a persistent HTTP connection (keep-alive) to avoid TCP handshake overhead per block. Timeout should be generous (30s+) since `arkiv_commitChain` blocks until the EntityDB finishes trie computation.

---

## Migration from v1

| v1 | v2 | Change |
|---|---|---|
| `block.operations[]` flat list | `block.transactions[].operations[]` nested | Restores tx-level grouping |
| No changeset hash | `changesetHash` on every operation + block header | Requires contract event change |
| `txSeq` / `opSeq` on operations | `transaction.index` + `operation.opIndex` | Cleaner separation of concerns |
| `sender` only on CreateOp | `sender` on ArkivTransaction | All ops have sender via their tx |
| `entityAddress` field name | `entityKey` field name | Aligns with contract terminology |
