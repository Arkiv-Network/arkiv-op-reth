//! Arkiv ExEx — filters blocks for EntityRegistry transactions and
//! forwards them to the configured Storage backend.

use alloy_consensus::{BlockHeader, Transaction};
use crate::genesis::ENTITY_REGISTRY_ADDRESS;
use crate::storage::{RegistryBlock, RegistryBlockRef, RegistryTransaction, Storage};
use eyre::Result;
use futures_util::TryStreamExt;
use reth_execution_types::Chain;
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_optimism_primitives::OpPrimitives;
use std::sync::Arc;

type OpChain = Chain<OpPrimitives>;

/// Run the Arkiv ExEx.
///
/// Filters each block for transactions targeting the EntityRegistry,
/// extracts the full transaction + receipt, and forwards to the Storage backend.
pub async fn arkiv_exex<Node>(
    mut ctx: ExExContext<Node>,
    store: Arc<dyn Storage>,
) -> Result<()>
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

/// Extract RegistryBlocks from a committed chain.
fn extract_blocks(chain: &Arc<OpChain>) -> Vec<RegistryBlock> {
    let mut blocks = Vec::new();

    for (block, receipts) in chain.blocks_and_receipts() {
        let mut transactions = Vec::new();

        for (tx, receipt) in block.body().transactions().zip(receipts.iter()) {
            if tx.to() != Some(ENTITY_REGISTRY_ADDRESS) {
                continue;
            }

            transactions.push(RegistryTransaction {
                transaction: tx.clone(),
                receipt: receipt.clone(),
            });
        }

        blocks.push(RegistryBlock {
            number: block.header().number(),
            hash: block.header().hash_slow(),
            parent_hash: block.header().parent_hash(),
            transactions,
        });
    }

    blocks
}

/// Extract block refs from a reverted chain (newest-first for revert ordering).
fn extract_block_refs(chain: &Arc<OpChain>) -> Vec<RegistryBlockRef> {
    let mut refs: Vec<RegistryBlockRef> = chain
        .blocks_iter()
        .map(|b| RegistryBlockRef {
            number: b.header().number(),
            hash: b.header().hash_slow(),
        })
        .collect();

    refs.reverse();
    refs
}
