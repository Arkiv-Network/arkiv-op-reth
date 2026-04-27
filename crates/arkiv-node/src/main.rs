mod exex;
mod storage;

use alloy_primitives::keccak256;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_cli::Cli;
use reth_optimism_node::OpNode;
use std::sync::Arc;

fn main() -> eyre::Result<()> {
    Cli::parse_args().run(|builder, rollup_args| async move {
        // Decide whether the loaded chainspec is an Arkiv chain by checking
        // for the EntityRegistry predeploy at the canonical address with
        // bytecode matching this chain's chain_id. The chainspec itself is
        // used unmodified — `init` and `node` see identical state.
        let arkiv_active = has_arkiv_predeploy(&builder.config().chain);

        let mut node = builder.node(OpNode::new(rollup_args));
        if arkiv_active {
            tracing::info!("EntityRegistry predeploy detected; installing Arkiv ExEx");
            let store = build_store().await?;
            node = node.install_exex("arkiv", move |ctx| async move {
                Ok(exex::arkiv_exex(ctx, store))
            });
        } else {
            tracing::info!("EntityRegistry predeploy not detected; running as plain op-reth");
        }

        let handle = node.launch_with_debug_capabilities().await?;
        handle.wait_for_node_exit().await
    })
}

/// Construct the storage backend used by the ExEx. Reads `ARKIV_ENTITYDB_URL`
/// to choose between the JSON-RPC backend and the local logging backend.
async fn build_store() -> eyre::Result<Arc<dyn storage::Storage>> {
    if let Ok(url) = std::env::var("ARKIV_ENTITYDB_URL") {
        tracing::info!(url = %url, "using JsonRpcStore backend");
        let store = storage::jsonrpc::JsonRpcStore::new(url.clone());
        if let Err(e) = store.health_check().await {
            tracing::error!(url = %url, error = %e, "EntityDB health check failed; exiting");
            return Err(e);
        }
        Ok(Arc::new(store))
    } else {
        tracing::info!("using LoggingStore backend");
        Ok(Arc::new(storage::logging::LoggingStore::new()))
    }
}

/// Returns `true` iff the chainspec's genesis alloc contains the Arkiv
/// EntityRegistry predeploy at the canonical address with bytecode that
/// matches the runtime form for this chain's chain_id.
///
/// The bytecode hash check (rather than mere address presence) guards
/// against squatting at `0x42…0042` with unrelated code.
fn has_arkiv_predeploy(chain: &OpChainSpec) -> bool {
    let chain_id = chain.inner.chain.id();
    let Some(account) = chain
        .inner
        .genesis
        .alloc
        .get(&arkiv_genesis::ENTITY_REGISTRY_ADDRESS)
    else {
        return false;
    };
    let Some(code) = &account.code else {
        return false;
    };
    let Ok(expected) = arkiv_genesis::deploy_creation_code(chain_id) else {
        return false;
    };
    keccak256(code) == keccak256(&expected)
}
