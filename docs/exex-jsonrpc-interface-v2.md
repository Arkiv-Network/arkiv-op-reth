# ExEx -> EntityDB JSON-RPC Interface (v2)

This document supersedes the original `exex-jsonrpc-interface.md`. It addresses three structural problems in the v1 wire format:

1. **Lost transaction granularity** -- v1 flattened all operations into a single per-block array, losing which operations came from which transaction.
2. **Missing changeset hash** -- the contract's rolling `changeSetHash` was not included in the wire format, so the EntityDB had no way to verify consistency with on-chain state.
3. **Empty block semantics** -- clarified and formalised.

---

## Contract Event Surface

The ExEx consumes two events per executed operation, both emitted by
`EntityRegistry`:

```solidity
event EntityOperation(
    bytes32 indexed entityKey,
    uint8   indexed operationType,
    address indexed owner,
    uint32  expiresAt,   // absolute block number: currentBlock + op.btl
    bytes32 entityHash
);

event ChangeSetHashUpdate(
    bytes32 indexed entityKey,
    uint256 indexed operationKey,
    bytes32 changeSetHash
);
```

`expiresAt` in the event is the **absolute** expiry block the contract
computed as `currentBlock + op.btl`. The raw `btl` (blocks-to-live)
field from calldata is not emitted — the event carries the resolved
value. This is what the ExEx uses for `CreateOp.expires_at` and
`ExtendOp.expires_at` in the wire format.

Per `EntityRegistry.execute` (`contracts/EntityRegistry.sol`),
each calldata op causes `_dispatch` to emit one `EntityOperation`,
followed by one `ChangeSetHashUpdate` from the loop body. The two
events are paired 1:1 in op order; see [ExEx Decoding
Pipeline](#exex-decoding-pipeline) for how this pairing is reconstructed
from receipt logs.

The contract's storage layout (head block, block-node linked list,
per-tx op count, per-op rolling hashes) is exposed in Rust via
`arkiv_bindings::storage_layout`. The ExEx uses it to seed the
rolling-hash carry-forward at chain-notification boundaries—see
[Block Header](#block-header) for details.

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
    #[serde(with = "hex_u64")]
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
// arkiv_bindings::wire
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ArkivOperation {
    Create(CreateOp),
    Update(UpdateOp),
    Extend(ExtendOp),
    Transfer(TransferOp),
    Delete(DeleteOp),
    Expire(ExpireOp),
}
```

Each variant carries a `changeset_hash` field -- the contract's rolling hash immediately after this operation was applied.

```rust
// arkiv_bindings::wire
// content_type (Mime128) serialises as a plain string; name (Ident32) as ASCII.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateOp {
    pub op_index: u32,
    pub entity_key: B256,
    pub owner: Address,
    #[serde(with = "hex_u64")]
    pub expires_at: u64,
    pub entity_hash: B256,
    pub changeset_hash: B256,
    pub payload: Bytes,
    pub content_type: Mime128,   // serialises as string
    pub attributes: Vec<ArkivAttribute>,
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
    pub content_type: Mime128,   // serialises as string
    pub attributes: Vec<ArkivAttribute>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtendOp {
    pub op_index: u32,
    pub entity_key: B256,
    pub owner: Address,
    #[serde(with = "hex_u64")]
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

Mirrors the on-chain `Attribute { name, valueType, value }` shape. A
tagged enum, internally discriminated by `valueType`, with `name` and
`value` carrying the same meaning across all variants. Defined in
`arkiv_bindings::wire` as `ArkivAttribute`:

```rust
// arkiv_bindings::wire
#[derive(Serialize)]
#[serde(tag = "valueType", rename_all = "camelCase")]
pub enum ArkivAttribute {
    /// `ATTR_UINT` — right-aligned big-endian uint256 from the contract's
    /// `bytes32[4]` value. Serialized as a `0x`-prefixed lowercase hex string.
    Uint      { name: Ident32, value: U256 },
    /// `ATTR_STRING` — the full 128-byte container, **byte-exact**: no
    /// UTF-8 interpretation, no NUL truncation. Serialized as a single
    /// `0x`-prefixed lowercase hex string of length 258 (`0x` + 256 chars).
    /// UTF-8 / display interpretation is the consumer's choice.
    String    { name: Ident32, value: FixedBytes<128> },
    /// `ATTR_ENTITY_KEY` — cross-reference to another entity. Serialized
    /// as a 32-byte hex string.
    EntityKey { name: Ident32, value: B256 },
}
// Ident32 is arkiv_bindings::Ident32 — an alloy-generated UDVT (type Ident32 is bytes32)
// with validation impl blocks. Serialises as an ASCII string via its Serialize impl.
// Validated by Ident32::validate() during ParsedRegistryTx::decode().
```

JSON shape per attribute (three flat fields, regardless of variant):

```json
{ "valueType": "uint" | "string" | "entityKey", "name": "<ascii>", "value": "0x…" }
```

Unknown `valueType` values cause `wire::decode_operation` to return an
error; the entire transaction is then skipped by the ExEx with a loud
log. They do not appear silently in the wire payload.

**ATTR_STRING is opaque to the protocol.** The 128 bytes are emitted
verbatim. Consumers needing a printable form should apply lossy UTF-8
+ NUL-truncation client-side at display time, never on the stored value.
This is a behaviour change from the initial v2 design, which decoded
ATTR_STRING to a UTF-8 `String` lossily on the ExEx side; that behaviour
has been removed.

### Block Reference (for reverts)

```rust
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArkivBlockRef {
    #[serde(with = "hex_u64")]
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
                  { "valueType": "entityKey", "name": "linked.to", "value": "0xabc..." },
                  { "valueType": "uint",      "name": "priority",  "value": "0x2a" },
                  { "valueType": "string",    "name": "type",      "value": "0x6e6f7465000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000" }
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
2. The ExEx reads this from the `ChangeSetHashUpdate` event's `changeSetHash` field and includes it in the wire format
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

All types in this pipeline (`ArkivBlock`, `ArkivBlockHeader`,
`ArkivTransaction`, `ArkivOperation`, `ArkivAttribute`, `ArkivBlockRef`)
are defined in `arkiv_bindings::wire`.

For each block in a chain notification:

```
block (RecoveredBlock<OpBlock>)
  |
  +-- extract header: number, hash, parent_hash
  |
  +-- for each (tx, receipt) where tx.to == ENTITY_REGISTRY_ADDRESS:
  |     |
  |     +-- recover sender from tx signature
  |     +-- filter receipt logs to EntityRegistry address -> filtered_logs
  |     +-- wire::ParsedRegistryTx::parse(tx.input(), &filtered_logs)
  |           +-- decode executeCall calldata -> Operation[]
  |           +-- partition filtered_logs by topic0 into
  |               EntityOperation[] and ChangeSetHashUpdate[]
  |           +-- assert len(ops) == len(entity_events) == len(hash_events)
  |           +-- for each triple: assert entity_event.entityKey == hash_event.entityKey
  |     +-- .decode(tx_hash, tx_index, sender)
  |           +-- for each op: validate Mime128 content_type, Ident32 attr names
  |           +-- produce ArkivTransaction { hash, index, sender, operations }
  |           +-- last_in_block = last hash_event.changeSetHash
  |
  +-- set header.changeset_hash = last_in_block, falling back to the rolling
  |   hash carried forward across blocks (seeded once per chain notification
  |   from arkiv_bindings::storage_layout at the parent block's state)
  +-- emit ArkivBlock { header, transactions }
```

Blocks with no matching transactions emit `ArkivBlock { header, transactions: [] }`.

The `expires_at` field on `CreateOp` and `ExtendOp` is sourced from
`EntityOperation.expiresAt` (the absolute block number the contract
computed as `currentBlock + op.btl`) — not from the raw calldata `btl`
field, which is not exposed in the wire format.

---

## Wire Format Notes

- All `u64` values (block numbers, expiry) are hex-encoded with `0x` prefix
- Addresses are checksummed hex (`0x` + 40 hex chars)
- Bytes/payload are `0x`-prefixed hex
- B256 hashes are `0x`-prefixed hex (64 hex chars)
- `index` and `opIndex` are JSON integers (u32)
- Empty arrays are `[]`, not omitted
- `changesetHash` is `0x0000…0000` only when no operation has ever been recorded as of that block (genesis / pre-first-mutation); never `null`

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
