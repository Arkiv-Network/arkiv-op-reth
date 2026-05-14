//! Installation of Arkiv extensions onto an op-stack node builder.
//!
//! Registers the `arkiv_*` JSON-RPC namespace on the node's HTTP RPC
//! server. The custom `EvmFactory` (precompile, predeploys) is wired
//! separately via [`crate::ArkivOpNode`] passed to `builder.node(...)`.

use reth_node_builder::{
    FullNodeTypes, NodeAdapter, NodeBuilderWithComponents, NodeComponentsBuilder, NodeTypes,
    WithLaunchContext, rpc::RethRpcAddOns,
};
use reth_optimism_primitives::OpPrimitives;

use crate::rpc::{ArkivApiServer, ArkivRpc};

/// Installer that registers the `arkiv_*` RPC namespace.
pub fn install<T, CB, AO>(
    node: WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>,
) -> WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>
where
    T: FullNodeTypes,
    T::Types: NodeTypes<Primitives = OpPrimitives>,
    CB: NodeComponentsBuilder<T>,
    AO: RethRpcAddOns<NodeAdapter<T, CB::Components>>,
{
    node.extend_rpc_modules(|ctx| {
        let provider = ctx.provider().clone();
        let api = ArkivRpc::new(provider);
        ctx.modules.merge_configured(api.into_rpc())?;
        tracing::info!(target: "arkiv::rpc", "registered arkiv_* RPC namespace");
        Ok(())
    })
}
