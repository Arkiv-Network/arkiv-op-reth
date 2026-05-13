//! Installation of Arkiv extensions onto an op-stack node builder.
//!
//! Currently a no-op pass-through. The v1 ExEx + EntityDB JSON-RPC bridge
//! has been demolished; the v2 precompile + custom `EvmFactory` +
//! `arkiv_*` RPC namespace + `ArkivPairs` MDBX table are not yet wired in.
//!
//! Phase 2+ will fill this function with the v2 wiring (`EvmFactory`
//! override on `OpNode`, RPC module registration, `ArkivPairs` handle
//! plumbing). See `docs/v2-migration-plan.md` in the workspace root.

use reth_node_builder::{
    FullNodeTypes, NodeAdapter, NodeBuilderWithComponents, NodeComponentsBuilder, NodeTypes,
    WithLaunchContext, rpc::RethRpcAddOns,
};
use reth_optimism_primitives::OpPrimitives;

/// No-op installer. Returns the builder unchanged.
///
/// The bounds match what Phase 2+ will need once the `EvmFactory` override
/// and RPC extension calls go back in.
pub fn install<T, CB, AO>(
    node: WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>,
) -> WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>
where
    T: FullNodeTypes,
    T::Types: NodeTypes<Primitives = OpPrimitives>,
    CB: NodeComponentsBuilder<T>,
    AO: RethRpcAddOns<NodeAdapter<T, CB::Components>>,
{
    node
}
