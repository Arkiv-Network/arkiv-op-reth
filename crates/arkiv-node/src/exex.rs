//! Arkiv ExEx — filters blocks for EntityRegistry transactions,
//! decodes operations, and forwards them to the configured Storage backend.

use alloy_consensus::{BlockHeader, Transaction, TxReceipt};
use alloy_primitives::B256;
use alloy_sol_types::SolEvent;
use arkiv_bindings::decode::decode_registry_transaction;
use arkiv_bindings::types::DecodedOperation;
use arkiv_bindings::IEntityRegistry::ChangeSetHashUpdate;
use arkiv_bindings::{OP_CREATE, OP_DELETE, OP_EXPIRE, OP_EXTEND, OP_TRANSFER, OP_UPDATE};
use crate::genesis::ENTITY_REGISTRY_ADDRESS;
use crate::storage::{
    Annotation, ArkivBlock, ArkivBlockHeader, ArkivBlockRef, ArkivOperation, ArkivTransaction,
    ChangeOwnerOp, CreateOp, DeleteOp, ExpireOp, ExtendOp, Storage, UpdateOp,
};
use eyre::Result;
use futures_util::TryStreamExt;
use reth_execution_types::Chain;
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_optimism_primitives::OpPrimitives;
use std::sync::Arc;

type OpChain = Chain<OpPrimitives>;

/// Run the Arkiv ExEx.
pub async fn arkiv_exex<Node>(mut ctx: ExExContext<Node>, store: Arc<dyn Storage>) -> Result<()>
where
    Node: FullNodeComponents<Types: NodeTypes<Primitives = OpPrimitives>>,
{
    tracing::info!("arkiv-exex starting");

    while let Some(notification) = ctx.notifications.try_next().await? {
        match &notification {
            ExExNotification::ChainCommitted { new } => {
                let blocks = extract_blocks(new);
                store.handle_commit(&blocks)?;
                ctx.events
                    .send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
            }
            ExExNotification::ChainReorged { old, new } => {
                let rev_refs = extract_block_refs(old);
                let new_blocks = extract_blocks(new);
                store.handle_reorg(&rev_refs, &new_blocks)?;
                ctx.events
                    .send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
            }
            ExExNotification::ChainReverted { old } => {
                let rev_refs = extract_block_refs(old);
                store.handle_revert(&rev_refs)?;
            }
        }
    }

    tracing::info!("arkiv-exex exiting");
    Ok(())
}

/// Build v2 ArkivBlocks from a chain notification.
/// Forwards ALL blocks (including those with no Arkiv transactions).
fn extract_blocks(chain: &Arc<OpChain>) -> Vec<ArkivBlock> {
    let mut blocks = Vec::new();
    let mut last_changeset_hash: Option<B256> = None;

    for (block, receipts) in chain.blocks_and_receipts() {
        let mut transactions = Vec::new();
        let senders = block.senders();

        for (tx_index, ((tx, receipt), sender)) in block
            .body()
            .transactions()
            .zip(receipts.iter())
            .zip(senders.iter())
            .enumerate()
        {
            if tx.to() != Some(ENTITY_REGISTRY_ADDRESS) {
                continue;
            }

            let tx_hash = B256::from(tx.tx_hash());
            let logs = receipt.logs();

            // Decode operations from calldata + EntityOperation events.
            let ops = match decode_registry_transaction(
                ENTITY_REGISTRY_ADDRESS,
                tx.input(),
                tx_hash,
                receipt.status(),
                logs,
                block.header().number(),
            ) {
                Ok(ops) => ops,
                Err(e) => {
                    tracing::error!(
                        tx = %tx_hash,
                        block = block.header().number(),
                        error = %e,
                        "failed to decode registry transaction"
                    );
                    continue;
                }
            };

            // Extract ChangeSetHashUpdate events keyed by entityKey.
            let changeset_map = extract_changeset_hashes(logs);

            let operations: Vec<ArkivOperation> = ops
                .iter()
                .enumerate()
                .filter_map(|(op_index, op)| {
                    let hash = changeset_map
                        .get(&op.entity_key)
                        .copied()
                        .unwrap_or(B256::ZERO);
                    last_changeset_hash = Some(hash);
                    to_arkiv_operation(op, op_index as u32, hash)
                })
                .collect();

            if !operations.is_empty() {
                transactions.push(ArkivTransaction {
                    hash: tx_hash,
                    index: tx_index as u32,
                    sender: *sender,
                    operations,
                });
            }
        }

        blocks.push(ArkivBlock {
            header: ArkivBlockHeader {
                number: block.header().number(),
                hash: block.header().hash_slow(),
                parent_hash: block.header().parent_hash(),
                changeset_hash: last_changeset_hash,
            },
            transactions,
        });
    }

    blocks
}

/// Parse ChangeSetHashUpdate events from receipt logs, keyed by entityKey.
/// If multiple updates exist for the same entityKey (shouldn't happen in
/// a single tx), the last one wins.
fn extract_changeset_hashes(
    logs: &[alloy_primitives::Log],
) -> std::collections::HashMap<B256, B256> {
    let mut map = std::collections::HashMap::new();
    for log in logs {
        if log.address != ENTITY_REGISTRY_ADDRESS {
            continue;
        }
        if let Ok(event) = ChangeSetHashUpdate::decode_log(log) {
            map.insert(event.entityKey, event.data.changeSetHash);
        }
    }
    map
}

/// Extract block refs from a reverted chain (newest-first for revert ordering).
fn extract_block_refs(chain: &Arc<OpChain>) -> Vec<ArkivBlockRef> {
    let mut refs: Vec<ArkivBlockRef> = chain
        .blocks_iter()
        .map(|b| ArkivBlockRef {
            number: b.header().number(),
            hash: b.header().hash_slow(),
        })
        .collect();

    refs.reverse();
    refs
}

// ---------------------------------------------------------------------------
// DecodedOperation -> ArkivOperation mapping
// ---------------------------------------------------------------------------

fn to_arkiv_operation(
    op: &DecodedOperation,
    op_index: u32,
    changeset_hash: B256,
) -> Option<ArkivOperation> {
    match op.op_type {
        OP_CREATE => {
            let entity = op.entity.as_ref()?;
            Some(ArkivOperation::Create(CreateOp {
                op_index,
                entity_key: op.entity_key,
                owner: op.owner,
                expires_at: op.expires_at as u64,
                entity_hash: op.entity_hash,
                changeset_hash,
                payload: entity.payload.clone().unwrap_or_default(),
                content_type: entity.content_type.clone().unwrap_or_default(),
                annotations: to_annotations(entity),
            }))
        }
        OP_UPDATE => {
            let entity = op.entity.as_ref()?;
            Some(ArkivOperation::Update(UpdateOp {
                op_index,
                entity_key: op.entity_key,
                owner: op.owner,
                entity_hash: op.entity_hash,
                changeset_hash,
                payload: entity.payload.clone().unwrap_or_default(),
                content_type: entity.content_type.clone().unwrap_or_default(),
                annotations: to_annotations(entity),
            }))
        }
        OP_EXTEND => Some(ArkivOperation::Extend(ExtendOp {
            op_index,
            entity_key: op.entity_key,
            owner: op.owner,
            expires_at: op.expires_at as u64,
            entity_hash: op.entity_hash,
            changeset_hash,
        })),
        OP_TRANSFER => Some(ArkivOperation::ChangeOwner(ChangeOwnerOp {
            op_index,
            entity_key: op.entity_key,
            owner: op.owner,
            entity_hash: op.entity_hash,
            changeset_hash,
        })),
        OP_DELETE => Some(ArkivOperation::Delete(DeleteOp {
            op_index,
            entity_key: op.entity_key,
            owner: op.owner,
            entity_hash: op.entity_hash,
            changeset_hash,
        })),
        OP_EXPIRE => Some(ArkivOperation::Expire(ExpireOp {
            op_index,
            entity_key: op.entity_key,
            owner: op.owner,
            entity_hash: op.entity_hash,
            changeset_hash,
        })),
        _ => None,
    }
}

fn to_annotations(entity: &arkiv_bindings::types::EntityRecord) -> Vec<Annotation> {
    entity
        .attributes
        .iter()
        .map(|attr| match attr.value_type {
            1 => {
                let bytes: &[u8] = attr.raw_value[0].as_ref();
                let val = u64::from_be_bytes(bytes[24..32].try_into().unwrap_or_default());
                Annotation::Numeric {
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
                Annotation::String {
                    key: attr.name.clone(),
                    string_value: String::from_utf8_lossy(&buf).to_string(),
                }
            }
        })
        .collect()
}
