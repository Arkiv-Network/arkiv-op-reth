//! Logging storage backend for development debugging.

use alloy_consensus::Transaction;
use alloy_primitives::Address;
use arkiv_bindings::decode::decode_registry_transaction;
use arkiv_bindings::types;
use crate::{RegistryBlock, RegistryBlockRef, Storage};
use eyre::Result;

pub struct LoggingStore {
    pub registry_address: Address,
}

impl LoggingStore {
    pub fn new(registry_address: Address) -> Self {
        Self { registry_address }
    }
}

impl Storage for LoggingStore {
    fn handle_commit(&self, blocks: &[RegistryBlock]) -> Result<()> {
        for block in blocks {
            if block.transactions.is_empty() {
                continue;
            }

            tracing::info!(
                block = block.number,
                hash = %block.hash,
                tx_count = block.transactions.len(),
                "processing registry block"
            );

            for tx in &block.transactions {
                match decode_registry_transaction(
                    self.registry_address,
                    tx.transaction.input(),
                    *tx.transaction.tx_hash(),
                    tx.receipt.success,
                    &tx.receipt.logs,
                    block.number,
                ) {
                    Ok(ops) => {
                        for op in &ops {
                            tracing::info!(
                                block = op.block_number,
                                tx = %op.tx_hash,
                                op_type = types::op_type_name(op.op_type),
                                entity_key = %op.entity_key,
                                owner = %op.owner,
                                expires_at = op.expires_at,
                                entity_hash = %op.entity_hash,
                                has_content = op.entity.is_some(),
                                "entity operation"
                            );

                            if let Some(entity) = &op.entity {
                                tracing::debug!(
                                    entity_key = %entity.entity_key,
                                    content_type = entity.content_type.as_deref().unwrap_or(""),
                                    payload_len = entity.payload.as_ref().map_or(0, |p: &alloy_primitives::Bytes| p.len()),
                                    attribute_count = entity.attributes.len(),
                                    "entity content"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            tx = %tx.transaction.tx_hash(),
                            block = block.number,
                            error = %e,
                            "failed to decode registry transaction"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    fn handle_revert(&self, blocks: &[RegistryBlockRef]) -> Result<()> {
        for block in blocks {
            tracing::warn!(block = block.number, hash = %block.hash, "reverting block");
        }
        Ok(())
    }

    fn handle_reorg(
        &self,
        reverted: &[RegistryBlockRef],
        new_blocks: &[RegistryBlock],
    ) -> Result<()> {
        tracing::warn!(
            reverted = reverted.len(),
            new = new_blocks.len(),
            "processing reorg"
        );
        self.handle_revert(reverted)?;
        self.handle_commit(new_blocks)
    }
}
