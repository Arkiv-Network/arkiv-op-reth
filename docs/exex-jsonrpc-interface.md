# ExEx → EntityDB JSON-RPC Interface

This document specifies the JSON-RPC interface between the Arkiv ExEx (Rust, in arkiv-op-reth) and the Go EntityDB service. It resolves the wire format, Rust type mapping, ExEx decoding responsibilities, error handling, and the response contract.

---

## Design Decisions

### ExEx Must Decode

The architecture doc specifies typed operations (`CreateOp`, `UpdateOp`, etc.) with decoded fields — not raw calldata. This means the ExEx is **not** a raw filter; it must:

1. Decode `execute()` calldata via ABI to get `Operation[]`
2. Parse `EntityOperation` event logs to extract `entityKey`, `entityAddress`, `txSeq`, `opSeq`
3. Correlate calldata operations with event logs (positional 1:1 match)
4. Build typed `ArkivOperation` variants with decoded fields
5. Serialize to JSON-RPC and forward

This departs from the current "thin filter" implementation in `arkiv-store` which passes raw `calldata + logs`. The rationale: the Go EntityDB should not need to re-implement Solidity ABI decoding. The ExEx already has `arkiv-bindings` with the exact type definitions. Decode once, in the language that has the bindings.

The existing `decode_registry_transaction()` function in `arkiv-store/src/decode.rs` does most of this work already. The ExEx calls it and maps the result into the JSON-RPC types.

### Block Headers Are Required

The architecture doc requires `number`, `hash`, and `parent_hash` per block. The current ExEx only extracts `block_number`. This must be extended — the EntityDB uses:

- `number` — block key in journal, expiry comparison, state root mapping
- `hash` — journal key, `arkiv_root` mapping, block identity for revert
- `parent_hash` — continuity check (guards against gaps or out-of-order delivery)

### Empty Blocks Are Forwarded

The architecture doc requires that blocks with no Arkiv transactions are still forwarded (with an empty `operations` list) so the EntityDB can advance its state root for every canonical block. The current ExEx skips empty blocks (`extract_registry_block` returns `None`). This must change.

### Response Returns State Root

`arkiv_commitChain` returns `arkiv_stateRoot` — the root of the EntityDB's trie after applying the last block in the batch. The ExEx needs this to submit the on-chain commitment. The response is synchronous: the ExEx blocks until the EntityDB returns the root.

---

## Rust Types

These types live in `arkiv-store` and are what the ExEx constructs and the `JsonRpcStore` serializes. They replace the current `RegistryBlock`/`RegistryTransaction`.

```rust
use alloy_primitives::{Address, Bytes, B256};
use serde::Serialize;

/// Block header subset forwarded to the EntityDB.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArkivBlockHeader {
    #[serde(with = "u64_hex")]
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
}

/// A block with its decoded Arkiv operations (may be empty).
#[derive(Serialize)]
pub struct ArkivBlock {
    pub header: ArkivBlockHeader,
    pub operations: Vec<ArkivOperation>,
}

/// Minimal block identifier for revert payloads.
#[derive(Serialize)]
pub struct ArkivBlockRef {
    #[serde(with = "u64_hex")]
    pub number: u64,
    pub hash: B256,
}

/// A decoded Arkiv operation, tagged by type.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ArkivOperation {
    Create(CreateOp),
    Update(UpdateOp),
    Extend(ExtendOp),
    #[serde(rename = "changeOwner")]
    ChangeOwner(ChangeOwnerOp),
    Delete(DeleteOp),
    Expire(ExpireOp),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateOp {
    pub tx_seq: u32,
    pub op_seq: u32,
    pub sender: Address,
    pub entity_address: Address,
    pub payload: Bytes,
    pub content_type: String,
    pub expires_at: u64,
    pub owner: Address,
    pub annotations: Vec<Annotation>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateOp {
    pub entity_address: Address,
    pub payload: Bytes,
    pub content_type: String,
    pub annotations: Vec<Annotation>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtendOp {
    pub entity_address: Address,
    pub new_expires_at: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeOwnerOp {
    pub entity_address: Address,
    pub new_owner: Address,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteOp {
    pub entity_address: Address,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpireOp {
    pub entity_address: Address,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum Annotation {
    String { key: String, string_value: String },
    Numeric { key: String, numeric_value: u64 },
}
```

---

## Methods

### arkiv_commitChain

Apply a contiguous sequence of blocks to the EntityDB's canonical head. Blocks must be oldest-first. The EntityDB applies them in order; if any block fails, the call returns an error and no state from that call is committed.

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
          "parentHash": "0xdef..."
        },
        "operations": [
          {
            "type": "create",
            "txSeq": 0,
            "opSeq": 0,
            "sender": "0xf39F...",
            "entityAddress": "0x...",
            "payload": "0xdeadbeef...",
            "contentType": "application/octet-stream",
            "expiresAt": 13500,
            "owner": "0xf39F...",
            "annotations": [
              { "key": "type", "stringValue": "note" },
              { "key": "priority", "numericValue": 5 }
            ]
          }
        ]
      },
      {
        "header": {
          "number": "0x303a",
          "hash": "0x123...",
          "parentHash": "0xabc..."
        },
        "operations": []
      }
    ]
  }]
}
```

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

`stateRoot` is the EntityDB's trie root after applying the last block in the batch. The ExEx uses this to submit the on-chain `arkiv_stateRoot` commitment.

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

Revert a contiguous sequence of blocks from the canonical head back to the common ancestor. Blocks are identified by number and hash only — the EntityDB uses its journal to undo state changes. Blocks must be newest-first.

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

`stateRoot` is the EntityDB's trie root after reverting to the common ancestor.

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
        "header": { "number": "0x3039", "hash": "0x...", "parentHash": "0x..." },
        "operations": []
      },
      {
        "header": { "number": "0x303a", "hash": "0x...", "parentHash": "0x..." },
        "operations": [{ "type": "delete", "entityAddress": "0x..." }]
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

## Error Codes

| Code | Meaning |
|------|---------|
| -32001 | Parent hash mismatch — block's `parentHash` doesn't match the EntityDB's current head |
| -32002 | Block not found — revert requested for a block the EntityDB hasn't processed |
| -32003 | Out of order — blocks not in the expected sequence (oldest-first for commit, newest-first for revert) |
| -32004 | Unknown entity — operation references an `entityAddress` that doesn't exist in the EntityDB |
| -32005 | Internal error — EntityDB trie or PebbleDB failure |

Standard JSON-RPC errors (-32600 through -32603) apply for malformed requests, invalid params, etc.

---

## Mapping from Current Rust Code

The current `arkiv-store` types and the ExEx need to evolve:

| Current | Target | Change |
|---------|--------|--------|
| `RegistryBlock { block_number, transactions }` | `ArkivBlock { header, operations }` | Add block hash + parent_hash; operations are decoded, not raw |
| `RegistryTransaction { tx_hash, calldata, logs, success }` | Eliminated | Operations are decoded per-op, not per-tx |
| `Storage::handle_commit(&RegistryBlock)` | `Storage::handle_commit(&[ArkivBlock])` | Receives a chain of blocks (may be >1 for ChainCommitted batches) |
| `Storage::handle_revert(block_number)` | `Storage::handle_revert(&[ArkivBlockRef])` | Receives block refs with hashes, not just numbers |
| ExEx skips empty blocks | ExEx forwards all blocks | EntityDB must advance state root per block |
| `decode_registry_transaction()` returns `DecodedOperation` | Maps to `ArkivOperation` enum | Same decode logic, different output shape |

### Storage Trait (revised)

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

`LoggingStore` returns `None` for state root (it doesn't maintain a trie). `JsonRpcStore` returns `Some(root)` from the EntityDB response.

### ExEx Changes

The ExEx notification loop maps directly to the three JSON-RPC methods:

| ExExNotification | Storage method | JSON-RPC method |
|-----------------|----------------|-----------------|
| `ChainCommitted { new }` | `handle_commit` | `arkiv_commitChain` |
| `ChainReverted { old }` | `handle_revert` | `arkiv_revert` |
| `ChainReorged { old, new }` | `handle_reorg` | `arkiv_reorg` |

The ExEx builds `ArkivBlock`s by:
1. Extracting block header fields from `RecoveredBlock` (number, hash, parent_hash)
2. For each transaction targeting `ENTITY_REGISTRY_ADDRESS` with a successful receipt:
   - Calling `decode_registry_transaction()` to get `Vec<DecodedOperation>`
   - Mapping each `DecodedOperation` to the corresponding `ArkivOperation` variant
3. For blocks with no Arkiv transactions, emitting an `ArkivBlock` with empty `operations`

---

## Annotation Encoding

Annotations in the JSON-RPC payload use the `Annotation` enum. The ExEx maps from the contract's `Attribute` type:

| Contract Attribute | `valueType` | JSON Annotation |
|---|---|---|
| `name="status", valueType=2 (STRING), value=[bytes32[4]]` | STRING | `{ "key": "status", "stringValue": "approved" }` |
| `name="priority", valueType=1 (UINT), value=[bytes32[4]]` | UINT | `{ "key": "priority", "numericValue": 5 }` |
| `name="ref", valueType=3 (ENTITY_KEY), value=[bytes32[4]]` | ENTITY_KEY | `{ "key": "ref", "stringValue": "0x..." }` |

ENTITY_KEY values are serialized as hex strings. The Go EntityDB interprets the value type from context (the `key` field is not typed in JSON — the EntityDB's schema determines how to index it).

---

## Wire Format Notes

- All `u64` block numbers and expiry values are hex-encoded with `0x` prefix in JSON (consistent with Ethereum JSON-RPC conventions)
- Addresses are checksummed hex (`0x` + 40 hex chars)
- Bytes/payload are `0x`-prefixed hex
- B256 hashes are `0x`-prefixed hex (64 hex chars)
- `txSeq` and `opSeq` are JSON integers (they fit in u32)
- Empty `operations` arrays are `[]`, not omitted

---

## Transport

HTTP POST to the EntityDB's ingest endpoint (configurable URL, default `http://localhost:9545`). No authentication for MVP — the ExEx and EntityDB run on the same host. TLS and auth tokens can be added later.

The ExEx should use a persistent HTTP connection (keep-alive) to avoid TCP handshake overhead per block. Timeout should be generous (30s+) since `arkiv_commitChain` blocks until the EntityDB finishes trie computation.
