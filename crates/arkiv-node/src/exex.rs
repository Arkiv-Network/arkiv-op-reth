//! Arkiv ExEx — filters blocks for EntityRegistry transactions,
//! decodes operations, and forwards them to the configured Storage backend.

use crate::storage::{ArkivBlock, ArkivBlockHeader, ArkivBlockRef, ArkivTransaction, Storage};
use alloy_consensus::{BlockHeader, Transaction, TxReceipt};
use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::{SolCall, SolEvent};
use arkiv_bindings::IEntityRegistry::{ChangeSetHashUpdate, EntityOperation, executeCall};
use arkiv_bindings::storage_layout::{
    block_node_slot, decode_block_node, decode_hash_at, decode_head_block, decode_tx_op_count,
    hash_at_slot, head_block_slot, tx_op_count_slot,
};
use arkiv_bindings::wire;
use arkiv_genesis::ENTITY_REGISTRY_ADDRESS;
use eyre::{Result, bail};
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

            let operations =
                match decode_arkiv_tx(tx.input(), receipt.logs(), &mut last_in_block) {
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
// Per-tx decode: calldata + paired event logs -> typed wire operations.
// ---------------------------------------------------------------------------

/// Decode one EntityRegistry transaction into a vector of typed
/// `wire::Operation`s, updating `last_in_block` with the rolling-hash value
/// of each successfully-decoded op.
///
/// Pairing contract (mirrors `EntityRegistry.sol::execute`, lines 113–122):
/// each calldata op causes `_dispatch` to emit one `EntityOperation`,
/// followed by one `ChangeSetHashUpdate` from the loop body. So the
/// receipt's registry-address logs interleave per op and the three
/// sequences (calldata `ops`, `EntityOperation` events, `ChangeSetHashUpdate`
/// events) are 1:1 in emission order. Anything else is contract drift —
/// bail and let the caller skip the tx (same loud-failure semantics as the
/// previous `decode_registry_transaction` Err path).
///
/// `last_in_block` is updated *after* each successful per-op decode using
/// `cshu.changeSetHash`, so a per-op decode failure breaks the rolling-hash
/// chain at that tx — preserves today's behaviour and avoids silently
/// masking decode failures.
fn decode_arkiv_tx(
    input: &[u8],
    logs: &[alloy_primitives::Log],
    last_in_block: &mut Option<B256>,
) -> Result<Vec<wire::Operation>> {
    let call = executeCall::abi_decode(input)?;

    let mut entity_events: Vec<EntityOperation> = Vec::new();
    let mut hash_events: Vec<ChangeSetHashUpdate> = Vec::new();
    for log in logs.iter().filter(|l| l.address == ENTITY_REGISTRY_ADDRESS) {
        match log.topics().first() {
            Some(t) if *t == EntityOperation::SIGNATURE_HASH => {
                entity_events.push(EntityOperation::decode_log_data(&log.data)?);
            }
            Some(t) if *t == ChangeSetHashUpdate::SIGNATURE_HASH => {
                hash_events.push(ChangeSetHashUpdate::decode_log_data(&log.data)?);
            }
            other => {
                bail!(
                    "unexpected log from EntityRegistry: topic0={:?}",
                    other
                );
            }
        }
    }

    if call.ops.len() != entity_events.len() || call.ops.len() != hash_events.len() {
        bail!(
            "event/calldata length mismatch: ops={}, entity_events={}, hash_events={}",
            call.ops.len(),
            entity_events.len(),
            hash_events.len(),
        );
    }

    let mut out = Vec::with_capacity(call.ops.len());
    for (op_index, ((cd, eo), cshu)) in call
        .ops
        .iter()
        .zip(&entity_events)
        .zip(&hash_events)
        .enumerate()
    {
        // Cheap sanity check: both events index on entityKey for the same op.
        if eo.entityKey != cshu.entityKey {
            bail!(
                "event entityKey mismatch at op_index={}: EntityOperation={}, ChangeSetHashUpdate={}",
                op_index, eo.entityKey, cshu.entityKey,
            );
        }
        let wire_op = wire::decode_operation(op_index as u32, cd, eo, cshu)?;
        *last_in_block = Some(cshu.changeSetHash);
        out.push(wire_op);
    }

    Ok(out)
}
