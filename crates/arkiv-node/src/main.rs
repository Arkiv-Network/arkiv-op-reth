mod exex;
mod rpc;
mod storage;
mod storaged;

use alloy_primitives::keccak256;
use clap::Parser;
use eyre::{Result, WrapErr, bail};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};
use reth_optimism_node::{OpNode, args::RollupArgs};
use std::{path::PathBuf, sync::Arc};

use crate::rpc::{ArkivApiServer, ArkivRpc};
use crate::storage::{EntityDbClient, JsonRpcStore, Storage, logging::LoggingStore};
use crate::storaged::StoragedProcess;

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

    /// Path to an arkiv-storaged binary to run as a child process for
    /// the lifetime of the node.
    #[arg(long = "arkiv-storaged-path")]
    arkiv_storaged_path: Option<PathBuf>,

    /// Space-separated arguments passed to the arkiv-storaged child
    /// process. Requires `--arkiv-storaged-path`.
    #[arg(long = "arkiv-storaged-args", requires = "arkiv_storaged_path")]
    arkiv_storaged_args: Option<String>,

    #[command(flatten)]
    rollup: RollupArgs,
}

fn main() -> Result<()> {
    Cli::<OpChainSpecParser, ArkivExt>::parse().run(|builder, ext| async move {
        let ArkivExt {
            arkiv_db_url,
            arkiv_debug,
            arkiv_storaged_path,
            arkiv_storaged_args,
            rollup,
        } = ext;

        let predeploy = has_arkiv_predeploy(&builder.config().chain);
        let storaged_requested = arkiv_storaged_path.is_some();
        let mut node = builder.node(OpNode::new(rollup));

        // clap's `conflicts_with` rejects --arkiv.db-url + --arkiv.debug at parse time.
        match (predeploy, arkiv_db_url.as_ref(), arkiv_debug, storaged_requested) {
            (false, None, false, false) => {
                tracing::info!("EntityRegistry predeploy not detected; running as plain op-reth");
            }
            (false, _, _, _) => {
                bail!(
                    "Arkiv flags set but the loaded chainspec does not contain the \
                     EntityRegistry predeploy at {}",
                    arkiv_genesis::ENTITY_REGISTRY_ADDRESS,
                );
            }
            (true, None, false, _) => {
                bail!(
                    "EntityRegistry predeploy detected; either --arkiv.db-url (or \
                     ARKIV_ENTITYDB_URL) or --arkiv.debug is required",
                );
            }
            (true, None, true, _) => {
                tracing::info!("Arkiv: predeploy detected; installing ExEx with LoggingStore (debug)");
                let store: Arc<dyn Storage> = Arc::new(LoggingStore::new());
                node = node.install_exex("arkiv", move |ctx| async move {
                    Ok(exex::arkiv_exex(ctx, store))
                });
            }
            (true, Some(_), false, _) => {}
            (true, Some(_), true, _) => unreachable!("clap conflicts_with rejects this combination"),
        }

        let mut storaged = if let Some(path) = arkiv_storaged_path {
            Some(StoragedProcess::start(
                path,
                arkiv_storaged_args.unwrap_or_default(),
            )?)
        } else {
            None
        };

        if let (true, Some(url), false) = (predeploy, arkiv_db_url.as_ref(), arkiv_debug) {
            let client = Arc::new(EntityDbClient::new(url.clone()));
            if let Err(err) = client.health_check().await {
                shutdown_storaged(&mut storaged).await?;
                return Err(err).wrap_err_with(|| format!("EntityDB unreachable at {url}"));
            }
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

        let handle = match node.launch_with_debug_capabilities().await {
            Ok(handle) => handle,
            Err(err) => {
                shutdown_storaged(&mut storaged).await?;
                return Err(err);
            }
        };

        if let Some(storaged) = storaged {
            let (shutdown, mut storaged_exit) = storaged.into_parts();
            tokio::select! {
                node_result = handle.wait_for_node_exit() => {
                    shutdown.request();
                    match storaged_exit.await {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => tracing::warn!(%err, "Arkiv: arkiv-storaged shutdown returned an error"),
                        Err(err) => tracing::warn!(%err, "Arkiv: arkiv-storaged supervisor task failed during shutdown"),
                    }
                    node_result
                }
                storaged_result = &mut storaged_exit => {
                    match storaged_result {
                        Ok(result) => result,
                        Err(err) => Err(err.into()),
                    }
                }
            }
        } else {
            handle.wait_for_node_exit().await
        }
    })
}

async fn shutdown_storaged(storaged: &mut Option<StoragedProcess>) -> Result<()> {
    if let Some(storaged) = storaged.take() {
        let (shutdown, join) = storaged.into_parts();
        shutdown.request();
        match join.await {
            Ok(result) => result,
            Err(err) => Err(err.into()),
        }
    } else {
        Ok(())
    }
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
