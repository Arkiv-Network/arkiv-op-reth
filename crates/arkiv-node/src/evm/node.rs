//! Node + executor builder + payload-attributes builder.
//!
//! - [`ArkivOpExecutorBuilder`] replaces `OpExecutorBuilder` in the
//!   node's component bundle so [`super::factory::ArkivOpEvmFactory`]
//!   is the one used at runtime.
//! - [`ArkivOpNode`] replaces `OpNode` so `add_ons()` matches the
//!   swapped Components type. Forwards everything else to the inner
//!   `OpNode`.
//! - [`ArkivLocalPayloadAttributesBuilder`] is a verbatim copy of
//!   op-reth's private `OpLocalPayloadAttributesBuilder`, required by
//!   `DebugNode::local_payload_attributes_builder`.

use std::sync::Arc;

use alloy_eips::eip1559::BaseFeeParams;
use alloy_hardforks::EthereumHardforks;
use reth_node_api::PayloadAttributesBuilder;
use reth_node_builder::{
    BuilderContext, DebugNode, FullNodeComponents, FullNodeTypes, Node, NodeAdapter,
    NodeComponentsBuilder, NodeTypes,
    components::{BasicPayloadServiceBuilder, ComponentsBuilder, ExecutorBuilder},
    rpc::BasicEngineValidatorBuilder,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::{
    OpAddOns, OpEngineApiBuilder, OpEngineTypes, OpNode, OpStorage,
    node::{
        OpConsensusBuilder, OpEngineValidatorBuilder, OpNetworkBuilder, OpPayloadBuilder,
        OpPoolBuilder,
    },
    payload::{OpPayloadAttributes, OpPayloadAttrs},
    rpc::OpEthApiBuilder,
};
use reth_optimism_primitives::OpPrimitives;

use super::config::ArkivOpEvmConfig;

/// Drop-in replacement for `OpExecutorBuilder` that produces an
/// [`ArkivOpEvmConfig`] (which uses
/// [`super::factory::ArkivOpEvmFactory`] internally).
#[derive(Debug, Clone, Default)]
pub struct ArkivOpExecutorBuilder;

impl<N> ExecutorBuilder<N> for ArkivOpExecutorBuilder
where
    N: FullNodeTypes<Types: NodeTypes<ChainSpec = OpChainSpec, Primitives = OpPrimitives>>,
{
    type EVM = ArkivOpEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<N>) -> eyre::Result<Self::EVM> {
        Ok(ArkivOpEvmConfig::new(ctx.chain_spec()))
    }
}

/// Thin wrapper around `OpNode` that swaps the executor.
///
/// Required because `OpNode::add_ons()` returns
/// `OpAddOns<NodeAdapter<N, OpDefaultComponents>, ...>` — the AddOns
/// is hardcoded to the default component bundle (with
/// `OpEvmConfig<.., OpEvmFactory<OpTx>>`). When we swap in
/// [`ArkivOpExecutorBuilder`] the `Components` type changes, so
/// `op_node.add_ons()` no longer matches the components we built and
/// `with_add_ons` rejects it.
///
/// The fix mirrors the upstream `examples/custom-node` pattern:
/// define a local `Node` impl whose `ComponentsBuilder` and `AddOns`
/// are typed consistently against [`ArkivOpExecutorBuilder`], and
/// forward to the inner `OpNode` for everything else.
#[derive(Debug, Clone, Default)]
pub struct ArkivOpNode {
    inner: OpNode,
}

impl ArkivOpNode {
    pub fn new(inner: OpNode) -> Self {
        Self { inner }
    }
}

impl NodeTypes for ArkivOpNode {
    type Primitives = OpPrimitives;
    type ChainSpec = OpChainSpec;
    type Storage = OpStorage;
    type Payload = OpEngineTypes;
}

impl<N> Node<N> for ArkivOpNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        OpPoolBuilder,
        BasicPayloadServiceBuilder<OpPayloadBuilder>,
        OpNetworkBuilder,
        ArkivOpExecutorBuilder,
        OpConsensusBuilder,
    >;

    type AddOns = OpAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
        OpEthApiBuilder,
        OpEngineValidatorBuilder,
        OpEngineApiBuilder<OpEngineValidatorBuilder>,
        BasicEngineValidatorBuilder<OpEngineValidatorBuilder>,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        self.inner.components().executor(ArkivOpExecutorBuilder)
    }

    fn add_ons(&self) -> Self::AddOns {
        self.inner.add_ons_builder().build()
    }
}

// Required for `launch_with_debug_capabilities()`. Body is a faithful
// copy of upstream `OpNode`'s impl (op-reth/crates/node/src/node.rs:337
// and the private `OpLocalPayloadAttributesBuilder` it constructs at
// :74-137); the type lives in the op-reth `node.rs` module privately
// so we cannot reuse it directly.
impl<N> DebugNode<N> for ArkivOpNode
where
    N: FullNodeComponents<Types = Self>,
{
    type RpcBlock = alloy_rpc_types_eth::Block<op_alloy_consensus::OpTxEnvelope>;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_node_api::BlockTy<Self> {
        rpc_block.into_consensus()
    }

    fn local_payload_attributes_builder(
        chain_spec: &Self::ChainSpec,
    ) -> impl PayloadAttributesBuilder<<Self::Payload as reth_node_api::PayloadTypes>::PayloadAttributes>
    {
        ArkivLocalPayloadAttributesBuilder {
            chain_spec: Arc::new(chain_spec.clone()),
        }
    }
}

/// Local-mining payload attributes builder. Verbatim copy of
/// `OpLocalPayloadAttributesBuilder` from op-reth's private module;
/// kept in sync with `op-reth/v2.2.5`.
struct ArkivLocalPayloadAttributesBuilder {
    chain_spec: Arc<OpChainSpec>,
}

impl PayloadAttributesBuilder<OpPayloadAttrs> for ArkivLocalPayloadAttributesBuilder {
    fn build(
        &self,
        parent: &reth_primitives_traits::SealedHeader<alloy_consensus::Header>,
    ) -> OpPayloadAttrs {
        use alloy_consensus::BlockHeader;
        use alloy_primitives::{Address, B64};

        let timestamp = std::cmp::max(
            parent.timestamp().saturating_add(1),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        );

        let eth_attrs = alloy_rpc_types_engine::PayloadAttributes {
            timestamp,
            prev_randao: alloy_primitives::B256::random(),
            suggested_fee_recipient: Address::random(),
            withdrawals: self
                .chain_spec
                .is_shanghai_active_at_timestamp(timestamp)
                .then(Default::default),
            parent_beacon_block_root: self
                .chain_spec
                .is_cancun_active_at_timestamp(timestamp)
                .then(alloy_primitives::B256::random),
            slot_number: None,
        };

        // OP Mainnet `setL1BlockValuesEcotone` system tx at index 0 of
        // block 124665056. Hard-coded for dev mode so blocks pass the
        // OP "first tx must be a deposit" rule.
        const TX_SET_L1_BLOCK: [u8; 251] = alloy_primitives::hex!(
            "7ef8f8a0683079df94aa5b9cf86687d739a60a9b4f0835e520ec4d664e2e415dca17a6df94deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e200000146b000f79c500000000000000040000000066d052e700000000013ad8a3000000000000000000000000000000000000000000000000000000003ef1278700000000000000000000000000000000000000000000000000000000000000012fdf87b89884a61e74b322bbcf60386f543bfae7827725efaaf0ab1de2294a590000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985"
        );

        let default_params = BaseFeeParams::optimism();
        let denominator = std::env::var("OP_DEV_EIP1559_DENOMINATOR")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(default_params.max_change_denominator as u32);
        let elasticity = std::env::var("OP_DEV_EIP1559_ELASTICITY")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(default_params.elasticity_multiplier as u32);
        let gas_limit = std::env::var("OP_DEV_GAS_LIMIT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok());

        let mut eip1559_bytes = [0u8; 8];
        eip1559_bytes[0..4].copy_from_slice(&denominator.to_be_bytes());
        eip1559_bytes[4..8].copy_from_slice(&elasticity.to_be_bytes());

        OpPayloadAttrs(OpPayloadAttributes {
            payload_attributes: eth_attrs,
            transactions: Some(vec![TX_SET_L1_BLOCK.into()]),
            no_tx_pool: None,
            gas_limit,
            eip_1559_params: Some(B64::from(eip1559_bytes)),
            min_base_fee: Some(0),
        })
    }
}
