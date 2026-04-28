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
use crate::storage::{EntityDbClient, JsonRpcStore, Storage, logging::LoggingStore};

/// CLI extension over [`RollupArgs`]. Adds Arkiv-specific flags.
#[derive(Debug, clap::Args)]
struct ArkivExt {
    /// EntityDB JSON-RPC URL. On an Arkiv chainspec, enables the ExEx
    /// (forwarding to EntityDB) and the `arkiv_query` JSON-RPC method.
    #[arg(long = "arkiv.db-url", env = "ARKIV_ENTITYDB_URL")]
    arkiv_db_url: Option<String>,

    /// Debug mode: run the ExEx with the in-process `LoggingStore` backend
    /// (decoded ops are emitted as tracing events). Useful for local dev
    /// without a running EntityDB. The `arkiv_*` RPC namespace is not
    /// installed in this mode.
    #[arg(long = "arkiv.debug", conflicts_with = "arkiv_db_url")]
    arkiv_debug: bool,

    #[command(flatten)]
    rollup: RollupArgs,
}

fn main() -> Result<()> {
    Cli::<OpChainSpecParser, ArkivExt>::parse().run(|builder, ext| async move {
        let predeploy = has_arkiv_predeploy(&builder.config().chain);
        let mut node = builder.node(OpNode::new(ext.rollup));

        // clap's `conflicts_with` rejects --arkiv.db-url + --arkiv.debug at parse time.
        match (predeploy, ext.arkiv_db_url, ext.arkiv_debug) {
            (false, None, false) => {
                tracing::info!("EntityRegistry predeploy not detected; running as plain op-reth");
            }
            (false, _, _) => {
                bail!(
                    "Arkiv flags set but the loaded chainspec does not contain the \
                     EntityRegistry predeploy at {}",
                    arkiv_genesis::ENTITY_REGISTRY_ADDRESS,
                );
            }
            (true, None, false) => {
                bail!(
                    "EntityRegistry predeploy detected; either --arkiv.db-url (or \
                     ARKIV_ENTITYDB_URL) or --arkiv.debug is required",
                );
            }
            (true, None, true) => {
                tracing::info!("Arkiv: predeploy detected; installing ExEx with LoggingStore (debug)");
                let store: Arc<dyn Storage> = Arc::new(LoggingStore::new());
                node = node.install_exex("arkiv", move |ctx| async move {
                    Ok(exex::arkiv_exex(ctx, store))
                });
            }
            (true, Some(url), false) => {
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
            (true, Some(_), true) => unreachable!("clap conflicts_with rejects this combination"),
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
