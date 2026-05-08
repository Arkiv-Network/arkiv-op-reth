//! Arkiv ExEx — filters blocks for EntityRegistry transactions,
//! decodes operations, and forwards them to the configured Storage backend.

use crate::storage::{ArkivBlock, ArkivBlockHeader, ArkivBlockRef, Storage};
use alloy_consensus::{BlockHeader, Transaction, TxReceipt};
use alloy_primitives::{Address, B256, U256};
use arkiv_bindings::storage_layout::{
    block_node_slot, decode_block_node, decode_hash_at, decode_head_block, decode_tx_op_count,
    hash_at_slot, head_block_slot, tx_op_count_slot,
};
use arkiv_bindings::wire;
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
/// blocks inherit this value; non-empty blocks update the carried value
/// for subsequent empty blocks.
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

            let registry_logs: Vec<_> = receipt
                .logs()
                .iter()
                .filter(|l| l.address == ENTITY_REGISTRY_ADDRESS)
                .cloned()
                .collect();

            let parsed = match wire::ParsedRegistryTx::parse(tx.input(), &registry_logs) {
                Ok(p) => p,
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

            if parsed.is_empty() {
                continue;
            }

            match parsed.decode(tx_hash, tx_index as u32, *sender) {
                Ok((arkiv_tx, last_hash)) => {
                    if let Some(h) = last_hash {
                        last_in_block = Some(h);
                    }
                    transactions.push(arkiv_tx);
                }
                Err(e) => {
                    tracing::error!(
                        tx = %tx_hash,
                        block = block.header().number(),
                        error = %e,
                        "failed to decode registry transaction operations"
                    );
                }
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
