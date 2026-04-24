//! Logging storage backend for development debugging.

use alloy_primitives::{Address, B256};
use crate::storage::{ArkivBlock, ArkivBlockRef, ArkivOperation, Storage};
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
    fn handle_commit(&self, blocks: &[ArkivBlock]) -> Result<Option<B256>> {
        for block in blocks {
            if block.transactions.is_empty() {
                continue;
            }

            tracing::info!(
                block = block.header.number,
                hash = %block.header.hash,
                tx_count = block.transactions.len(),
                changeset_hash = ?block.header.changeset_hash,
                "processing registry block"
            );

            for tx in &block.transactions {
                tracing::info!(
                    tx = %tx.hash,
                    index = tx.index,
                    sender = %tx.sender,
                    op_count = tx.operations.len(),
                    "registry transaction"
                );

                for op in &tx.operations {
                    log_operation(op, block.header.number);
                }
            }
        }

        Ok(None)
    }

    fn handle_revert(&self, blocks: &[ArkivBlockRef]) -> Result<Option<B256>> {
        for block in blocks {
            tracing::warn!(block = block.number, hash = %block.hash, "reverting block");
        }
        Ok(None)
    }

    fn handle_reorg(
        &self,
        reverted: &[ArkivBlockRef],
        new_blocks: &[ArkivBlock],
    ) -> Result<Option<B256>> {
        tracing::warn!(
            reverted = reverted.len(),
            new = new_blocks.len(),
            "processing reorg"
        );
        self.handle_revert(reverted)?;
        self.handle_commit(new_blocks)
    }
}

fn log_operation(op: &ArkivOperation, block_number: u64) {
    match op {
        ArkivOperation::Create(o) => {
            tracing::info!(
                block = block_number,
                op_type = "CREATE",
                op_index = o.op_index,
                entity_key = %o.entity_key,
                owner = %o.owner,
                expires_at = o.expires_at,
                entity_hash = %o.entity_hash,
                changeset_hash = %o.changeset_hash,
                content_type = %o.content_type,
                payload_len = o.payload.len(),
                annotation_count = o.annotations.len(),
                "entity operation"
            );
        }
        ArkivOperation::Update(o) => {
            tracing::info!(
                block = block_number,
                op_type = "UPDATE",
                op_index = o.op_index,
                entity_key = %o.entity_key,
                owner = %o.owner,
                entity_hash = %o.entity_hash,
                changeset_hash = %o.changeset_hash,
                content_type = %o.content_type,
                payload_len = o.payload.len(),
                "entity operation"
            );
        }
        ArkivOperation::Extend(o) => {
            tracing::info!(
                block = block_number,
                op_type = "EXTEND",
                op_index = o.op_index,
                entity_key = %o.entity_key,
                owner = %o.owner,
                expires_at = o.expires_at,
                changeset_hash = %o.changeset_hash,
                "entity operation"
            );
        }
        ArkivOperation::ChangeOwner(o) => {
            tracing::info!(
                block = block_number,
                op_type = "TRANSFER",
                op_index = o.op_index,
                entity_key = %o.entity_key,
                owner = %o.owner,
                changeset_hash = %o.changeset_hash,
                "entity operation"
            );
        }
        ArkivOperation::Delete(o) => {
            tracing::info!(
                block = block_number,
                op_type = "DELETE",
                op_index = o.op_index,
                entity_key = %o.entity_key,
                owner = %o.owner,
                changeset_hash = %o.changeset_hash,
                "entity operation"
            );
        }
        ArkivOperation::Expire(o) => {
            tracing::info!(
                block = block_number,
                op_type = "EXPIRE",
                op_index = o.op_index,
                entity_key = %o.entity_key,
                owner = %o.owner,
                changeset_hash = %o.changeset_hash,
                "entity operation"
            );
        }
    }
}
