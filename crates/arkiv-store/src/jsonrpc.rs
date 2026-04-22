//! JSON-RPC storage backend for the Go EntityDB service.
//!
//! Implements `arkiv_commitChain`, `arkiv_revert`, and `arkiv_reorg`.
//! Internally decodes raw ExEx data into the typed wire format before sending.

use alloy_consensus::Transaction;
use arkiv_bindings::decode::decode_registry_transaction;
use arkiv_bindings::types::DecodedOperation;
use crate::{RegistryBlock, RegistryBlockRef, Storage};
use alloy_primitives::{Address, Bytes, B256};
use arkiv_bindings::{
    OP_CREATE, OP_DELETE, OP_EXPIRE, OP_EXTEND, OP_TRANSFER, OP_UPDATE,
};
use eyre::{bail, Result};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Wire types — arkiv_commitChain / arkiv_revert / arkiv_reorg format
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireBlockHeader {
    #[serde(serialize_with = "hex_u64::serialize")]
    number: u64,
    hash: B256,
    parent_hash: B256,
}

#[derive(Serialize)]
struct WireBlock {
    header: WireBlockHeader,
    operations: Vec<WireOperation>,
}

#[derive(Serialize)]
struct WireBlockRef {
    #[serde(serialize_with = "hex_u64::serialize")]
    number: u64,
    hash: B256,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum WireOperation {
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
struct CreateOp {
    entity_key: B256,
    owner: Address,
    expires_at: u32,
    entity_hash: B256,
    payload: Bytes,
    content_type: String,
    annotations: Vec<WireAnnotation>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateOp {
    entity_key: B256,
    owner: Address,
    entity_hash: B256,
    payload: Bytes,
    content_type: String,
    annotations: Vec<WireAnnotation>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExtendOp {
    entity_key: B256,
    owner: Address,
    expires_at: u32,
    entity_hash: B256,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChangeOwnerOp {
    entity_key: B256,
    owner: Address,
    entity_hash: B256,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeleteOp {
    entity_key: B256,
    owner: Address,
    entity_hash: B256,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExpireOp {
    entity_key: B256,
    owner: Address,
    entity_hash: B256,
}

#[derive(Serialize)]
#[serde(untagged)]
enum WireAnnotation {
    String { key: String, string_value: String },
    Numeric { key: String, numeric_value: u64 },
}

// ---------------------------------------------------------------------------
// Decode mapping: DecodedOperation → WireOperation
// ---------------------------------------------------------------------------

fn to_wire_operation(op: &DecodedOperation) -> Option<WireOperation> {
    match op.op_type {
        OP_CREATE => {
            let entity = op.entity.as_ref()?;
            Some(WireOperation::Create(CreateOp {
                entity_key: op.entity_key,
                owner: op.owner,
                expires_at: op.expires_at,
                entity_hash: op.entity_hash,
                payload: entity.payload.clone().unwrap_or_default(),
                content_type: entity.content_type.clone().unwrap_or_default(),
                annotations: to_wire_annotations(entity),
            }))
        }
        OP_UPDATE => {
            let entity = op.entity.as_ref()?;
            Some(WireOperation::Update(UpdateOp {
                entity_key: op.entity_key,
                owner: op.owner,
                entity_hash: op.entity_hash,
                payload: entity.payload.clone().unwrap_or_default(),
                content_type: entity.content_type.clone().unwrap_or_default(),
                annotations: to_wire_annotations(entity),
            }))
        }
        OP_EXTEND => Some(WireOperation::Extend(ExtendOp {
            entity_key: op.entity_key,
            owner: op.owner,
            expires_at: op.expires_at,
            entity_hash: op.entity_hash,
        })),
        OP_TRANSFER => Some(WireOperation::ChangeOwner(ChangeOwnerOp {
            entity_key: op.entity_key,
            owner: op.owner,
            entity_hash: op.entity_hash,
        })),
        OP_DELETE => Some(WireOperation::Delete(DeleteOp {
            entity_key: op.entity_key,
            owner: op.owner,
            entity_hash: op.entity_hash,
        })),
        OP_EXPIRE => Some(WireOperation::Expire(ExpireOp {
            entity_key: op.entity_key,
            owner: op.owner,
            entity_hash: op.entity_hash,
        })),
        _ => None,
    }
}

fn to_wire_annotations(entity: &arkiv_bindings::types::EntityRecord) -> Vec<WireAnnotation> {
    entity
        .attributes
        .iter()
        .map(|attr| match attr.value_type {
            1 => {
                let bytes: &[u8] = attr.raw_value[0].as_ref();
                let val = u64::from_be_bytes(bytes[24..32].try_into().unwrap_or_default());
                WireAnnotation::Numeric {
                    key: attr.name.clone(),
                    numeric_value: val,
                }
            }
            _ => {
                let mut buf = Vec::with_capacity(128);
                for b32 in &attr.raw_value {
                    buf.extend_from_slice(b32.as_ref());
                }
                if let Some(end) = buf.iter().position(|b| *b == 0) {
                    buf.truncate(end);
                }
                WireAnnotation::String {
                    key: attr.name.clone(),
                    string_value: String::from_utf8_lossy(&buf).to_string(),
                }
            }
        })
        .collect()
}

fn to_wire_block(block: &RegistryBlock) -> WireBlock {
    let mut operations = Vec::new();

    for tx in &block.transactions {
        if let Ok(decoded_ops) = decode_registry_transaction(
            tx.transaction.to(),
            tx.transaction.input(),
            *tx.transaction.tx_hash(),
            tx.receipt.success,
            &tx.receipt.logs,
            block.number,
        ) {
            for op in &decoded_ops {
                if let Some(wire_op) = to_wire_operation(op) {
                    operations.push(wire_op);
                }
            }
        }
    }

    WireBlock {
        header: WireBlockHeader {
            number: block.number,
            hash: block.hash,
            parent_hash: block.parent_hash,
        },
        operations,
    }
}

fn to_wire_block_ref(block: &RegistryBlockRef) -> WireBlockRef {
    WireBlockRef {
        number: block.number,
        hash: block.hash,
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC client
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct JsonRpcResponse<T> {
    result: Option<T>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Deserialize)]
struct AckResponse {}

/// A [`Storage`] implementation that forwards blocks to a Go EntityDB via JSON-RPC.
pub struct JsonRpcStore {
    client: reqwest::blocking::Client,
    url: String,
    next_id: AtomicU64,
}

impl JsonRpcStore {
    pub fn new(url: String) -> Self {
        Self {
            client: reqwest::blocking::Client::builder()
                .pool_max_idle_per_host(1)
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
            url,
            next_id: AtomicU64::new(1),
        }
    }

    fn rpc_call<R: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<R> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": [params]
        });

        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .map_err(|e| eyre::eyre!("EntityDB request failed: {}", e))?;

        let rpc_resp: JsonRpcResponse<R> = resp
            .json()
            .map_err(|e| eyre::eyre!("EntityDB response parse failed: {}", e))?;

        match rpc_resp.error {
            Some(e) => bail!("EntityDB error {}: {}", e.code, e.message),
            None => rpc_resp
                .result
                .ok_or_else(|| eyre::eyre!("EntityDB returned empty result")),
        }
    }
}

impl Storage for JsonRpcStore {
    fn handle_commit(&self, blocks: &[RegistryBlock]) -> Result<()> {
        let wire_blocks: Vec<WireBlock> = blocks.iter().map(to_wire_block).collect();
        let _: AckResponse = self.rpc_call(
            "arkiv_commitChain",
            serde_json::json!({ "blocks": wire_blocks }),
        )?;
        Ok(())
    }

    fn handle_revert(&self, blocks: &[RegistryBlockRef]) -> Result<()> {
        let wire_refs: Vec<WireBlockRef> = blocks.iter().map(to_wire_block_ref).collect();
        let _: AckResponse = self.rpc_call(
            "arkiv_revert",
            serde_json::json!({ "blocks": wire_refs }),
        )?;
        Ok(())
    }

    fn handle_reorg(
        &self,
        reverted: &[RegistryBlockRef],
        new_blocks: &[RegistryBlock],
    ) -> Result<()> {
        let wire_reverted: Vec<WireBlockRef> = reverted.iter().map(to_wire_block_ref).collect();
        let wire_new: Vec<WireBlock> = new_blocks.iter().map(to_wire_block).collect();
        let _: AckResponse = self.rpc_call(
            "arkiv_reorg",
            serde_json::json!({
                "revertedBlocks": wire_reverted,
                "newBlocks": wire_new
            }),
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Hex u64 serialization
// ---------------------------------------------------------------------------

mod hex_u64 {
    use serde::Serializer;

    pub fn serialize<S: Serializer>(val: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("0x{:x}", val))
    }
}
