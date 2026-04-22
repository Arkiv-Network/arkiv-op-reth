pub mod jsonrpc;
pub mod logging;

use alloy_consensus::{EthereumReceipt, EthereumTxEnvelope, TxEip4844};
use alloy_primitives::{Log, B256};
use eyre::Result;

/// Reth's concrete signed transaction type.
pub type TransactionSigned = EthereumTxEnvelope<TxEip4844>;

/// A transaction targeting the EntityRegistry with its receipt.
pub struct RegistryTransaction {
    pub transaction: TransactionSigned,
    pub receipt: EthereumReceipt<u8, Log>,
}

/// A block forwarded by the ExEx. Contains only EntityRegistry transactions.
pub struct RegistryBlock {
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
    pub transactions: Vec<RegistryTransaction>,
}

/// Minimal block identifier for revert payloads.
pub struct RegistryBlockRef {
    pub number: u64,
    pub hash: B256,
}

/// Storage backend for the Arkiv ExEx.
///
/// The three methods map to the three ExEx notification variants:
/// - `handle_commit` ← ChainCommitted
/// - `handle_revert` ← ChainReverted
/// - `handle_reorg`  ← ChainReorged
///
/// The ExEx passes raw Ethereum primitives. Each implementation
/// decides how much decoding/processing to do.
pub trait Storage: Send + Sync + 'static {
    /// Process committed blocks (oldest-first).
    fn handle_commit(&self, blocks: &[RegistryBlock]) -> Result<()>;

    /// Revert blocks (newest-first).
    fn handle_revert(&self, blocks: &[RegistryBlockRef]) -> Result<()>;

    /// Atomically revert old blocks and commit new blocks.
    fn handle_reorg(
        &self,
        reverted: &[RegistryBlockRef],
        new_blocks: &[RegistryBlock],
    ) -> Result<()>;
}
