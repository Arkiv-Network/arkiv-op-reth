//! Mode resolution + installation of Arkiv onto an op-stack node builder.
//!
//! The split is deliberate:
//!
//! - [`resolve_mode`] is pure validation + a network health check. No
//!   reth/builder generics. Embedders can call it directly, or skip it
//!   entirely and construct an [`ArkivMode`] themselves.
//! - [`install`] is the only function that touches `reth-node-builder`
//!   generics. It mirrors op-reth's `launch_node_with_proof_history`
//!   pattern: take the post-`.node()` builder, call `install_exex` and
//!   (conditionally) `extend_rpc_modules`, return the builder.

use std::sync::Arc;

use eyre::{Result, WrapErr, bail};
use reth_node_builder::{
    FullNodeTypes, NodeAdapter, NodeBuilderWithComponents, NodeComponentsBuilder, NodeTypes,
    WithLaunchContext, rpc::RethRpcAddOns,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_primitives::OpPrimitives;

use crate::exex;
use crate::genesis::has_arkiv_predeploy;
use crate::rpc::{ArkivApiServer, ArkivRpc};
use crate::storage::{EntityDbClient, JsonRpcStore, Storage, logging::LoggingStore};

/// Resolved Arkiv configuration. Decouples "what was decided" from how it
/// was decided (CLI flags, programmatic config, …).
#[derive(Clone)]
pub enum ArkivMode {
    /// No Arkiv extensions; behave as plain op-reth.
    Disabled,
    /// In-process [`LoggingStore`] backend; no RPC namespace.
    Debug,
    /// Forward to EntityDB; install `arkiv_*` RPC.
    EntityDb { client: Arc<EntityDbClient> },
}

/// Validate the given Arkiv flags against the loaded chainspec and, in
/// the EntityDB case, run a health check. Mirrors the original `match` in
/// `main` 1:1; only the shape is different.
pub async fn resolve_mode(
    arkiv_db_url: Option<String>,
    arkiv_query_url: Option<String>,
    arkiv_debug: bool,
    chain: &OpChainSpec,
) -> Result<ArkivMode> {
    let predeploy = has_arkiv_predeploy(chain);

    match (predeploy, arkiv_db_url, arkiv_debug) {
        (false, None, false) => {
            tracing::info!("EntityRegistry predeploy not detected; running as plain op-reth");
            Ok(ArkivMode::Disabled)
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
            Ok(ArkivMode::Debug)
        }
        (true, Some(url), false) => {
            let client = Arc::new(EntityDbClient::new(url.clone(), arkiv_query_url));
            client
                .health_check()
                .await
                .wrap_err_with(|| format!("EntityDB unreachable at {url}"))?;
            tracing::info!(%url, "Arkiv: predeploy + EntityDB OK; installing ExEx + arkiv_* RPC");
            Ok(ArkivMode::EntityDb { client })
        }
        (true, Some(_), true) => {
            // Mirrors `clap::conflicts_with` for callers that bypass clap.
            bail!("--arkiv.db-url and --arkiv.debug are mutually exclusive");
        }
    }
}

/// Install the Arkiv ExEx (and, in [`ArkivMode::EntityDb`], the `arkiv_*`
/// RPC namespace) on an op-stack node builder. No-op for
/// [`ArkivMode::Disabled`].
///
/// The bounds match what the underlying `install_exex` /
/// `extend_rpc_modules` calls require, plus `Primitives = OpPrimitives`
/// (the ExEx assumes op-stack primitives).
pub fn install<T, CB, AO>(
    node: WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>,
    mode: ArkivMode,
) -> WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>
where
    T: FullNodeTypes,
    T::Types: NodeTypes<Primitives = OpPrimitives>,
    CB: NodeComponentsBuilder<T>,
    AO: RethRpcAddOns<NodeAdapter<T, CB::Components>>,
{
    match mode {
        ArkivMode::Disabled => node,
        ArkivMode::Debug => {
            let store: Arc<dyn Storage> = Arc::new(LoggingStore::new());
            node.install_exex("arkiv", move |ctx| async move {
                Ok(exex::arkiv_exex(ctx, store))
            })
        }
        ArkivMode::EntityDb { client } => {
            let store: Arc<dyn Storage> = Arc::new(JsonRpcStore::from_client(client.clone()));
            let rpc_client = client;
            node.install_exex("arkiv", move |ctx| async move {
                Ok(exex::arkiv_exex(ctx, store))
            })
            .extend_rpc_modules(move |ctx| {
                ctx.modules
                    .merge_configured(ArkivRpc::new(rpc_client).into_rpc())?;
                Ok(())
            })
        }
    }
}
