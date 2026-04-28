pub mod jsonrpc;
pub mod logging;

pub use jsonrpc::{EntityDbClient, JsonRpcStore};

use alloy_primitives::{Address, B256};
use arkiv_bindings::wire;
use eyre::Result;
use serde::Serialize;

// ---------------------------------------------------------------------------
// Wire envelopes (v2) — block -> transaction hierarchy.
//
// Operation + Attribute types live upstream in `arkiv_bindings::wire`; this
// module owns only the reth-shaped envelopes around them, per the upstream
// `wire.rs` doc: block / transaction / block-ref envelopes live in the
// consumer because they're built from reth-specific inputs (`RecoveredBlock`,
// signature recovery, etc.).
// ---------------------------------------------------------------------------

/// Block header subset forwarded to the EntityDB.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArkivBlockHeader {
    #[serde(with = "arkiv_bindings::wire::hex_u64")]
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
    pub operations: Vec<wire::Operation>,
}

/// Minimal block identifier for revert payloads.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArkivBlockRef {
    #[serde(with = "arkiv_bindings::wire::hex_u64")]
    pub number: u64,
    pub hash: B256,
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
