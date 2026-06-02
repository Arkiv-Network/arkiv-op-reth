//! Local newtype around `OpEvmConfig` so we can satisfy the orphan
//! rule when implementing `ConfigureEngineEvm<OpExecData>` (upstream
//! only impls it for the default `OpEvmFactory<OpTx>` variant of
//! `OpEvmConfig`, so our custom-factory variant gets no impl from the
//! open OP crates).
//!
//! - [`ConfigureEvm`] is a thin passthrough except for
//!   `block_executor_factory()`, which returns our
//!   [`ArkivOpBlockExecutorFactory`] wrapper instead of the inner OP
//!   factory.
//! - [`ConfigureEngineEvm<OpExecData>`] delegates to a stored
//!   default-factory `OpEvmConfig` (whose upstream impl body doesn't
//!   read the factory).
//! - [`ConfigurePostExecEvm`] forwards to `inner` (SDM post-exec).
//!
//! The struct owns two `OpBlockExecutorFactory` instances: one inside
//! `inner` (kept so the upstream `ConfigureEvm` plumbing has the type
//! it expects) and one inside [`ArkivOpBlockExecutorFactory`] (what
//! we hand out via `block_executor_factory()`). They're cheap clones
//! of the same factory; only the wrapper participates in canonical
//! execution.

use std::sync::Arc;

use alloy_consensus::Header;
use alloy_evm::{
    Database,
    block::BlockExecutor,
    revm::database::State,
};
use alloy_op_evm::{
    OpBlockExecutorFactory,
    post_exec::{PostExecEvmFactoryAdapter, PostExecExecutorExt},
};
use op_alloy_consensus::EIP1559ParamError;
use reth_evm::{EvmEnvFor, ExecutionCtxFor, execute::BlockBuilder};
use reth_node_builder::{ConfigureEngineEvm, ConfigureEvm};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::{
    ConfigurePostExecEvm, OpBlockAssembler, OpEvmConfig, OpNextBlockEnvAttributes,
    OpRethReceiptBuilder, PostExecMode, payload::OpExecData,
};
use reth_optimism_primitives::{OpBlock, OpPrimitives};
use reth_primitives_traits::{NodePrimitives, SealedBlock, SealedHeader};

use super::block_executor::ArkivOpBlockExecutorFactory;
use super::factory::ArkivOpEvmFactory;
use super::session::{SessionCacheMap, new_session_cache_map};

type InnerEvmConfig = OpEvmConfig<
    OpChainSpec,
    OpPrimitives,
    OpRethReceiptBuilder,
    PostExecEvmFactoryAdapter<ArkivOpEvmFactory>,
>;
type DefaultEvmConfig = OpEvmConfig<OpChainSpec, OpPrimitives, OpRethReceiptBuilder>;
type InnerExecutorFactory = OpBlockExecutorFactory<
    OpRethReceiptBuilder,
    Arc<OpChainSpec>,
    PostExecEvmFactoryAdapter<ArkivOpEvmFactory>,
>;

#[derive(Debug, Clone)]
pub struct ArkivOpEvmConfig {
    inner: InnerEvmConfig,
    /// Default-factory `OpEvmConfig` sharing the same chain spec. Used
    /// by the `ConfigureEngineEvm<OpExecData>` shim (whose upstream
    /// impl body does not depend on the EVM factory). Stored as a
    /// field rather than constructed on the fly so the iterator
    /// returned by `tx_iterator_for_payload` can outlive the call.
    inner_default: DefaultEvmConfig,
    /// Shared with the precompile closure and the BlockExecutor
    /// wrapper. Cloning the EvmConfig clones the `Arc`, so
    /// payload-build / canonical-exec / validation lanes all share
    /// one registry of live `CacheStore`s per node.
    sessions: SessionCacheMap,
    /// The Arkiv BlockExecutor factory — wraps the inner
    /// `OpBlockExecutorFactory` and layers session-id minting plus
    /// cache flushing. Owned here so
    /// [`ConfigureEvm::block_executor_factory`] can hand out a
    /// reference.
    executor_factory: ArkivOpBlockExecutorFactory<InnerExecutorFactory>,
}

impl ArkivOpEvmConfig {
    pub fn new(chain_spec: Arc<OpChainSpec>) -> Self {
        let sessions = new_session_cache_map();
        let inner_factory = OpBlockExecutorFactory::new(
            OpRethReceiptBuilder::default(),
            chain_spec.clone(),
            PostExecEvmFactoryAdapter::new(ArkivOpEvmFactory::new(sessions.clone())),
        );
        let inner = OpEvmConfig {
            block_assembler: OpBlockAssembler::new(chain_spec.clone()),
            executor_factory: inner_factory.clone(),
            sdm_enabled: false,
            _pd: core::marker::PhantomData,
        };
        let inner_default = OpEvmConfig::new(chain_spec, OpRethReceiptBuilder::default());
        let executor_factory = ArkivOpBlockExecutorFactory::new(inner_factory, sessions.clone());
        Self {
            inner,
            inner_default,
            sessions,
            executor_factory,
        }
    }

    /// Handle to the shared session-cache map. Used by the
    /// BlockExecutor wrapper to mint sessions and flush typed caches
    /// at `finish`.
    pub fn sessions(&self) -> &SessionCacheMap {
        &self.sessions
    }
}

impl ConfigureEvm for ArkivOpEvmConfig {
    // Concrete types (rather than `<InnerEvmConfig as ConfigureEvm>::X`
    // projections) so downstream bounds like
    // `<Self as ConfigureEvm>::NextBlockEnvCtx: BuildNextEnv<...>` in
    // `OpAddOns: NodeAddOns` can be checked without normalising
    // through `OpEvmConfig`'s blanket `ConfigureEvm` impl. Rust's
    // trait solver is sometimes unable to do that normalisation under
    // nested bounds.
    type Primitives = OpPrimitives;
    type Error = EIP1559ParamError;
    type NextBlockEnvCtx = OpNextBlockEnvAttributes;
    type BlockExecutorFactory = ArkivOpBlockExecutorFactory<InnerExecutorFactory>;
    type BlockAssembler = OpBlockAssembler<OpChainSpec>;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        self.inner.block_assembler()
    }

    fn evm_env(&self, header: &Header) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.evm_env(header)
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.next_evm_env(parent, attributes)
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<OpBlock>,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        self.inner.context_for_block(block)
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader<Header>,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<ExecutionCtxFor<'_, Self>, Self::Error> {
        self.inner.context_for_next_block(parent, attributes)
    }
}

impl ConfigureEngineEvm<OpExecData> for ArkivOpEvmConfig {
    fn evm_env_for_payload(
        &self,
        payload: &OpExecData,
    ) -> Result<EvmEnvFor<Self>, <Self as ConfigureEvm>::Error> {
        self.inner_default.evm_env_for_payload(payload)
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a OpExecData,
    ) -> Result<ExecutionCtxFor<'a, Self>, <Self as ConfigureEvm>::Error> {
        self.inner_default.context_for_payload(payload)
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &OpExecData,
    ) -> Result<impl reth_evm::ExecutableTxIterator<Self>, <Self as ConfigureEvm>::Error> {
        self.inner_default.tx_iterator_for_payload(payload)
    }
}

// `OpPayloadBuilder` now requires the EVM config to implement
// `ConfigurePostExecEvm` (SDM post-exec support). Upstream impls it for
// `OpEvmConfig<.., PostExecEvmFactoryAdapter<F>>`, which is exactly our
// `inner` — so forward both methods to it.
impl ConfigurePostExecEvm for ArkivOpEvmConfig {
    fn post_exec_executor_for_block<'a, DB: Database>(
        &'a self,
        db: &'a mut State<DB>,
        block: &'a SealedBlock<<Self::Primitives as NodePrimitives>::Block>,
        post_exec_mode: PostExecMode,
    ) -> Result<
        impl BlockExecutor<
            Transaction = <Self::Primitives as NodePrimitives>::SignedTx,
            Receipt = <Self::Primitives as NodePrimitives>::Receipt,
        > + PostExecExecutorExt
        + 'a,
        Self::Error,
    > {
        self.inner
            .post_exec_executor_for_block(db, block, post_exec_mode)
    }

    fn post_exec_builder_for_next_block<'a, DB: Database + 'a>(
        &'a self,
        db: &'a mut State<DB>,
        parent: &'a SealedHeader<<Self::Primitives as NodePrimitives>::BlockHeader>,
        attributes: Self::NextBlockEnvCtx,
        post_exec_mode: PostExecMode,
    ) -> Result<
        impl BlockBuilder<Primitives = Self::Primitives, Executor: PostExecExecutorExt> + 'a,
        Self::Error,
    > {
        self.inner
            .post_exec_builder_for_next_block(db, parent, attributes, post_exec_mode)
    }
}
