pub mod jsonrpc;
pub mod logging;

use alloy_primitives::{Address, B256, Bytes, U256};
use eyre::Result;
use serde::Serialize;

// ---------------------------------------------------------------------------
// Wire types (v2) — block -> transaction -> operation hierarchy
// ---------------------------------------------------------------------------

/// Block header subset forwarded to the EntityDB.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArkivBlockHeader {
    #[serde(with = "hex_u64")]
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
    /// Rolling changeset hash *as of the end of this block*. If the block
    /// contains operations, this is the hash after the last op. If the block
    /// is empty, this is the rolling hash carried forward from the most
    /// recent prior block that had operations. `B256::ZERO` only when no
    /// operation has ever been recorded as of this block.
    ///
    /// Note: this differs from the contract's `changeSetHashAtBlock(N)`,
    /// which returns `bytes32(0)` for any empty block. The ExEx computes
    /// the rolling form by reading the contract's storage at the parent
    /// of each chain notification.
    pub changeset_hash: B256,
}

/// A block with its decoded Arkiv transactions (may be empty).
#[derive(Serialize)]
pub struct ArkivBlock {
    pub header: ArkivBlockHeader,
    pub transactions: Vec<ArkivTransaction>,
}

/// A transaction targeting the EntityRegistry with decoded operations.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArkivTransaction {
    pub hash: B256,
    pub index: u32,
    pub sender: Address,
    pub operations: Vec<ArkivOperation>,
}

/// Minimal block identifier for revert payloads.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArkivBlockRef {
    #[serde(with = "hex_u64")]
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
    #[serde(rename = "transfer")]
    Transfer(TransferOp),
    Delete(DeleteOp),
    Expire(ExpireOp),
}

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

/// Decoded attribute value, mirroring the contract's three value types.
///
/// `#[serde(untagged)]` discriminates by which value field is present:
///   - `stringValue`: opaque UTF-8 (≤128 bytes per `value128-encoding.md`)
///   - `numericValue`: hex-encoded `U256` (right-aligned in `data[0]`)
///   - `entityKey`: hex-encoded `B256` (cross-reference to another entity)
#[derive(Serialize)]
#[serde(untagged)]
pub enum Attribute {
    String {
        name: String,
        #[serde(rename = "stringValue")]
        string_value: String,
    },
    Numeric {
        name: String,
        #[serde(rename = "numericValue")]
        numeric_value: U256,
    },
    EntityKey {
        name: String,
        #[serde(rename = "entityKey")]
        entity_key: B256,
    },
}

// ---------------------------------------------------------------------------
// Storage trait
// ---------------------------------------------------------------------------

/// Storage backend for the Arkiv ExEx.
///
/// Returns `Option<B256>` state root: `None` for backends that don't
/// maintain a trie (e.g. LoggingStore), `Some(root)` for JsonRpcStore.
pub trait Storage: Send + Sync + 'static {
    /// Process a chain of committed blocks (oldest-first).
    fn handle_commit(&self, blocks: &[ArkivBlock]) -> Result<Option<B256>>;

    /// Revert blocks (newest-first).
    fn handle_revert(&self, blocks: &[ArkivBlockRef]) -> Result<Option<B256>>;

    /// Atomically revert old blocks and commit new blocks (reorg).
    fn handle_reorg(
        &self,
        reverted: &[ArkivBlockRef],
        new_blocks: &[ArkivBlock],
    ) -> Result<Option<B256>>;
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

#[cfg(test)]
mod tests {
    use super::Attribute;
    use alloy_primitives::{B256, U256};
    use serde_json::json;

    #[test]
    fn attribute_serializes_with_name_field() {
        let string = serde_json::to_value(Attribute::String {
            name: "title".into(),
            string_value: "the answer".into(),
        })
        .expect("string attribute serializes");
        assert_eq!(
            string,
            json!({
                "name": "title",
                "stringValue": "the answer"
            })
        );

        let numeric = serde_json::to_value(Attribute::Numeric {
            name: "priority".into(),
            numeric_value: U256::from(42),
        })
        .expect("numeric attribute serializes");
        assert_eq!(
            numeric,
            json!({
                "name": "priority",
                "numericValue": "0x2a"
            })
        );

        let entity_key = serde_json::to_value(Attribute::EntityKey {
            name: "linked.to".into(),
            entity_key: B256::repeat_byte(0xab),
        })
        .expect("entity-key attribute serializes");
        assert_eq!(
            entity_key,
            json!({
                "name": "linked.to",
                "entityKey": format!("0x{}", "ab".repeat(32))
            })
        );
    }
}
