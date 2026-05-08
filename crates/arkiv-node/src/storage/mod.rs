pub mod jsonrpc;
pub mod logging;

pub use arkiv_bindings::wire::{ArkivBlock, ArkivBlockHeader, ArkivBlockRef, ArkivTransaction};
pub use jsonrpc::{EntityDbClient, JsonRpcStore};

use alloy_primitives::B256;
use eyre::Result;

// ---------------------------------------------------------------------------
// Storage trait
// ---------------------------------------------------------------------------

/// Storage backend for the Arkiv ExEx.
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
