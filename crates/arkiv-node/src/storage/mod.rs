pub mod logging;

use alloy_primitives::{Address, Bytes, B256};
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
    /// Rolling changeset hash after the last operation in this block.
    /// `None` for blocks before any operations have occurred.
    pub changeset_hash: Option<B256>,
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
    #[serde(rename = "changeOwner")]
    ChangeOwner(ChangeOwnerOp),
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
    pub annotations: Vec<Annotation>,
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
    pub annotations: Vec<Annotation>,
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
pub struct ChangeOwnerOp {
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

#[derive(Serialize)]
#[serde(untagged)]
pub enum Annotation {
    String {
        key: String,
        string_value: String,
    },
    Numeric {
        key: String,
        numeric_value: u64,
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
