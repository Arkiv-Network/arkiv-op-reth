mod exex;
mod rpc;
mod storage;

use alloy_primitives::keccak256;
use clap::Parser;
use eyre::{Result, WrapErr, bail};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};
use reth_optimism_node::{OpNode, args::RollupArgs};
use std::sync::Arc;

use crate::rpc::{ArkivApiServer, ArkivRpc};
use crate::storage::{EntityDbClient, JsonRpcStore, Storage};

/// CLI extension over [`RollupArgs`]. Adds Arkiv-specific flags.
#[derive(Debug, clap::Args)]
struct ArkivExt {
    /// EntityDB JSON-RPC URL. Required when running on a chainspec containing
    /// the EntityRegistry predeploy: enables the Arkiv ExEx and the
    /// `arkiv_query` JSON-RPC method.
    #[arg(long = "arkiv.db-url", env = "ARKIV_ENTITYDB_URL")]
    arkiv_db_url: Option<String>,

    #[command(flatten)]
    rollup: RollupArgs,
}

fn main() -> Result<()> {
    Cli::<OpChainSpecParser, ArkivExt>::parse().run(|builder, ext| async move {
        let predeploy = has_arkiv_predeploy(&builder.config().chain);
        let mut node = builder.node(OpNode::new(ext.rollup));

        match (predeploy, ext.arkiv_db_url) {
            (false, None) => {
                tracing::info!("EntityRegistry predeploy not detected; running as plain op-reth");
            }
            (false, Some(_)) => {
                bail!(
                    "--arkiv.db-url is set but the loaded chainspec does not contain the \
                     EntityRegistry predeploy at {}",
                    arkiv_genesis::ENTITY_REGISTRY_ADDRESS,
                );
            }
            (true, None) => {
                bail!(
                    "EntityRegistry predeploy detected; --arkiv.db-url (or ARKIV_ENTITYDB_URL) \
                     is required to run the Arkiv ExEx and arkiv_* RPC",
                );
            }
            (true, Some(url)) => {
                let client = Arc::new(EntityDbClient::new(url.clone()));
                client
                    .health_check()
                    .await
                    .wrap_err_with(|| format!("EntityDB unreachable at {url}"))?;
                tracing::info!(%url, "Arkiv: predeploy + EntityDB OK; installing ExEx + arkiv_* RPC");

                let store: Arc<dyn Storage> = Arc::new(JsonRpcStore::from_client(client.clone()));
                let rpc_client = client.clone();

                node = node
                    .install_exex("arkiv", move |ctx| async move {
                        Ok(exex::arkiv_exex(ctx, store))
                    })
                    .extend_rpc_modules(move |ctx| {
                        ctx.modules
                            .merge_configured(ArkivRpc::new(rpc_client).into_rpc())?;
                        Ok(())
                    });
            }
        }

        let handle = node.launch_with_debug_capabilities().await?;
        handle.wait_for_node_exit().await
    })
}

/// Returns `true` iff the chainspec's genesis alloc contains the Arkiv
/// EntityRegistry predeploy at the canonical address with bytecode that
/// matches the runtime form for this chain's chain_id.
///
/// The bytecode hash check (rather than mere address presence) guards
/// against squatting at `0x44…0044` with unrelated code.
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
