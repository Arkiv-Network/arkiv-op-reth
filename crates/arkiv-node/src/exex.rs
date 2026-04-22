use alloy_consensus::{BlockHeader, EthereumReceipt, Transaction};
use arkiv_genesis::ENTITY_REGISTRY_ADDRESS;
use arkiv_store::{RegistryBlock, RegistryBlockRef, RegistryTransaction, Storage};
use eyre::Result;
use futures_util::TryStreamExt;
use reth::builder::NodeTypes;
use reth::primitives::EthPrimitives;
use reth_ethereum_primitives::{Block, Receipt};
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_execution_types::Chain;
use reth_node_api::FullNodeComponents;
use reth::primitives::RecoveredBlock;
use std::sync::Arc;
use tracing::info;

type EthChain = Chain<EthPrimitives>;

/// Run the Arkiv ExEx.
///
/// Filters each block for transactions targeting the EntityRegistry,
/// extracts the full transaction + receipt, and forwards to the Storage backend.
pub async fn arkiv_exex<
    Node: FullNodeComponents<Types: NodeTypes<Primitives = EthPrimitives>>,
>(
    mut ctx: ExExContext<Node>,
    store: Arc<dyn Storage>,
) -> Result<()> {
    info!("arkiv-exex starting");

    while let Some(notification) = ctx.notifications.try_next().await? {
        match &notification {
            ExExNotification::ChainCommitted { new } => {
                let blocks = extract_blocks(new);
                store.handle_commit(&blocks)?;
                ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
            }
            ExExNotification::ChainReorged { old, new } => {
                let rev_refs = extract_block_refs(old);
                let new_blocks = extract_blocks(new);
                store.handle_reorg(&rev_refs, &new_blocks)?;
                ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
            }
            ExExNotification::ChainReverted { old } => {
                let rev_refs = extract_block_refs(old);
                store.handle_revert(&rev_refs)?;
            }
        }
    }

    info!("arkiv-exex exiting");
    Ok(())
}

/// Extract RegistryBlocks from a committed chain.
/// Includes all blocks — even those with no registry transactions.
fn extract_blocks(chain: &EthChain) -> Vec<RegistryBlock> {
    let mut blocks = Vec::new();

    for (block, receipts) in chain.blocks_and_receipts() {
        let transactions = extract_registry_txs(block, receipts);

        blocks.push(RegistryBlock {
            number: block.header().number(),
            hash: block.header().hash_slow(),
            parent_hash: block.header().parent_hash(),
            transactions,
        });
    }

    blocks
}

/// Filter and extract registry transactions from a single block.
fn extract_registry_txs(
    block: &RecoveredBlock<Block>,
    receipts: &[Receipt],
) -> Vec<RegistryTransaction> {
    let mut transactions = Vec::new();

    for (tx, receipt) in block.body().transactions.iter().zip(receipts.iter()) {
        if tx.to() != Some(ENTITY_REGISTRY_ADDRESS) {
            continue;
        }

        transactions.push(RegistryTransaction {
            transaction: tx.clone(),
            receipt: EthereumReceipt {
                tx_type: receipt.tx_type as u8,
                success: receipt.success,
                cumulative_gas_used: receipt.cumulative_gas_used,
                logs: receipt.logs.clone(),
            },
        });
    }

    transactions
}

/// Extract block refs from a reverted chain (newest-first for revert ordering).
fn extract_block_refs(chain: &EthChain) -> Vec<RegistryBlockRef> {
    let mut refs: Vec<RegistryBlockRef> = chain
        .blocks_iter()
        .map(|b: &RecoveredBlock<Block>| RegistryBlockRef {
            number: b.header().number(),
            hash: b.header().hash_slow(),
        })
        .collect();

    refs.reverse();
    refs
}
