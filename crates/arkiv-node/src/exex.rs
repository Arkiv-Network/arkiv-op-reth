//! Arkiv ExEx — filters blocks for EntityRegistry transactions,
//! decodes operations, and forwards them to the configured Storage backend.

use crate::storage::{
    ArkivBlock, ArkivBlockHeader, ArkivBlockRef, ArkivOperation, ArkivTransaction, Attribute,
    CreateOp, DeleteOp, ExpireOp, ExtendOp, Storage, TransferOp, UpdateOp,
};
use alloy_consensus::{BlockHeader, Transaction, TxReceipt};
use alloy_primitives::{Address, B256, U256};
use arkiv_bindings::decode::decode_registry_transaction;
use arkiv_bindings::storage_layout::{
    block_node_slot, decode_block_node, decode_hash_at, decode_head_block, decode_tx_op_count,
    hash_at_slot, head_block_slot, tx_op_count_slot,
};
use arkiv_bindings::types::DecodedOperation;
use arkiv_bindings::{
    ATTR_ENTITY_KEY, ATTR_STRING, ATTR_UINT, OP_CREATE, OP_DELETE, OP_EXPIRE, OP_EXTEND,
    OP_TRANSFER, OP_UPDATE,
};
use arkiv_genesis::ENTITY_REGISTRY_ADDRESS;
use eyre::Result;
use futures_util::TryStreamExt;
use reth_execution_types::Chain;
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_optimism_primitives::OpPrimitives;
use reth_storage_api::{StateProvider, StateProviderFactory};
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
                let prior_rolling = prior_rolling_hash(&ctx, new)?;
                let blocks = extract_blocks(new, prior_rolling);
                store.handle_commit(&blocks)?;
                ctx.events
                    .send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
            }
            ExExNotification::ChainReorged { old, new } => {
                let rev_refs = extract_block_refs(old);
                let prior_rolling = prior_rolling_hash(&ctx, new)?;
                let new_blocks = extract_blocks(new, prior_rolling);
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
///
/// `prior_rolling` is the rolling changeset hash *before* the first block
/// in this chain (read from contract storage at the parent block). Empty
/// blocks inherit this value; non-empty blocks emit their last op's hash
/// and update the carried value for subsequent empty blocks.
fn extract_blocks(chain: &Arc<OpChain>, mut prior_rolling: B256) -> Vec<ArkivBlock> {
    let mut blocks = Vec::new();
    for (block, receipts) in chain.blocks_and_receipts() {
        let mut transactions = Vec::new();
        let mut last_in_block: Option<B256> = None;
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

            let operations: Vec<ArkivOperation> = ops
                .iter()
                .enumerate()
                .filter_map(|(op_index, op)| {
                    last_in_block = Some(op.changeset_hash);
                    to_arkiv_operation(op, op_index as u32, op.changeset_hash)
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

        let block_rolling = last_in_block.unwrap_or(prior_rolling);
        if let Some(h) = last_in_block {
            prior_rolling = h;
        }

        blocks.push(ArkivBlock {
            header: ArkivBlockHeader {
                number: block.header().number(),
                hash: block.header().hash_slow(),
                parent_hash: block.header().parent_hash(),
                changeset_hash: block_rolling,
            },
            transactions,
        });
    }

    blocks
}

/// Compute the rolling changeset hash *before* the first block in `chain`
/// by reading EntityRegistry storage at the parent block's state.
///
/// Returns `B256::ZERO` if the chain is empty or the registry has never
/// recorded any operations as of the parent block.
fn prior_rolling_hash<Node>(ctx: &ExExContext<Node>, chain: &Arc<OpChain>) -> Result<B256>
where
    Node: FullNodeComponents<Types: NodeTypes<Primitives = OpPrimitives>>,
{
    let Some(first) = chain.blocks_iter().next() else {
        return Ok(B256::ZERO);
    };
    let parent_hash = first.header().parent_hash();
    let state = ctx.provider().history_by_block_hash(parent_hash)?;
    rolling_hash_at(&*state, ENTITY_REGISTRY_ADDRESS)
}

/// Walk the EntityRegistry's storage slots to recover the rolling changeset
/// hash at whatever historical state `state` represents. Mirrors the
/// derivation in `EntityRegistry.changeSetHash()`:
///
///   _hashAt[operationKey(_headBlock, lastTx, lastOp)]
///
/// where `lastTx = _blocks[_headBlock].txCount - 1` and
/// `lastOp   = _txOpCount[transactionKey(_headBlock, lastTx)] - 1`.
///
/// Returns `B256::ZERO` if no mutation has ever been recorded.
fn rolling_hash_at(state: &dyn StateProvider, registry: Address) -> Result<B256> {
    let head = decode_head_block(read_slot(state, registry, head_block_slot())?);
    if head == 0 {
        return Ok(B256::ZERO);
    }

    let node = decode_block_node(read_slot(state, registry, block_node_slot(head))?);
    if node.txCount == 0 {
        return Ok(B256::ZERO);
    }
    let last_tx = node.txCount - 1;

    let op_count = decode_tx_op_count(read_slot(state, registry, tx_op_count_slot(head, last_tx))?);
    if op_count == 0 {
        return Ok(B256::ZERO);
    }
    let last_op = op_count - 1;

    Ok(decode_hash_at(read_slot(
        state,
        registry,
        hash_at_slot(head, last_tx, last_op),
    )?))
}

/// Read a single 32-byte storage slot, treating an absent slot as ZERO.
fn read_slot(state: &dyn StateProvider, addr: Address, slot: B256) -> Result<B256> {
    let v: U256 = state.storage(addr, slot)?.unwrap_or(U256::ZERO);
    Ok(B256::from(v.to_be_bytes::<32>()))
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
                attributes: to_attributes(entity),
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
                attributes: to_attributes(entity),
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
        OP_TRANSFER => Some(ArkivOperation::Transfer(TransferOp {
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

/// Decode an entity's attributes into wire-format `Attribute` variants.
///
/// Encoding rules (see `arkiv-contracts/docs/value128-encoding.md`):
///   - `ATTR_UINT`: `raw_value[0]` is right-aligned big-endian uint256;
///     other words are zero. Decoded losslessly to `U256`.
///   - `ATTR_STRING`: left-aligned UTF-8 across all four words, zero-padded.
///     Truncated at the first NUL byte.
///   - `ATTR_ENTITY_KEY`: `raw_value[0]` is the entity key; other words zero.
///
/// Unknown `value_type` values are skipped with a warning rather than
/// guessing — keeps wire output honest if/when the contract adds new types.
fn to_attributes(entity: &arkiv_bindings::types::EntityRecord) -> Vec<Attribute> {
    entity
        .attributes
        .iter()
        .filter_map(|attr| match attr.value_type {
            ATTR_UINT => Some(Attribute::Numeric {
                key: attr.name.clone(),
                numeric_value: U256::from_be_bytes(attr.raw_value[0].0),
            }),
            ATTR_STRING => {
                let mut buf = Vec::with_capacity(128);
                for b32 in &attr.raw_value {
                    buf.extend_from_slice(b32.as_ref());
                }
                if let Some(end) = buf.iter().position(|b| *b == 0) {
                    buf.truncate(end);
                }
                Some(Attribute::String {
                    key: attr.name.clone(),
                    string_value: String::from_utf8_lossy(&buf).to_string(),
                })
            }
            ATTR_ENTITY_KEY => Some(Attribute::EntityKey {
                key: attr.name.clone(),
                entity_key: attr.raw_value[0],
            }),
            other => {
                tracing::warn!(
                    name = %attr.name,
                    value_type = other,
                    "unknown attribute value_type — skipping"
                );
                None
            }
        })
        .collect()
}
