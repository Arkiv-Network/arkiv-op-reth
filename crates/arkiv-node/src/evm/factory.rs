//! `EvmFactory` that installs the Arkiv precompile, plus its `Evm`
//! newtype that adds the `evm_tx` tracing span around `transact_raw`.
//!
//! - [`ArkivOpEvmFactory`] wraps [`OpEvmFactory<OpTx>`] and inserts the
//!   precompile in `create_evm` / `create_evm_with_inspector`. Both
//!   methods must agree, otherwise tracing diverges from execution.
//! - [`ArkivOpEvm`] is a thin newtype over [`OpEvm`] whose only job is
//!   to put a `tracing::debug_span!("evm_tx")` around `transact_raw`.
//!   Every other `Evm` method is a passthrough.

// `revm` types come via `alloy_evm::revm` — that re-exports the matching
// revm 38 line. The workspace also declares `revm 38.0.0` directly (used
// by `precompile.rs`); going through the alloy_evm re-export guarantees
// we pick up the same line as `alloy-op-evm` and avoids silent type
// mismatches across the two `revm` editions if they ever drift.
use alloy_evm::{
    Database, Evm, EvmEnv, EvmFactory,
    precompiles::PrecompilesMap,
    revm::{
        Inspector,
        context::{BlockEnv, CfgEnv, result::ResultAndState},
        context_interface::result::EVMError,
        inspector::NoOpInspector,
    },
};
use alloy_op_evm::{
    OpEvm, OpEvmContext, OpEvmFactory, OpTxError,
    post_exec::{
        PostExecEvmFactoryHooks, PostExecExecutedTx, PostExecTxContext,
    },
};
use alloy_primitives::{Address, Bytes};
use arkiv_genesis::ARKIV_ADDRESS;
use op_revm::{OpHaltReason, OpSpecId};
use reth_optimism_node::OpTx;

use super::session::{SessionCacheMap, new_session_cache_map};
use crate::precompile::arkiv_precompile;

/// EVM factory that defers to the default OP factory and inserts the
/// Arkiv precompile at [`ARKIV_ADDRESS`] on every fresh EVM
/// (both canonical execution and inspector-instrumented contexts).
///
/// Carries a clone of the per-node [`SessionCacheMap`] so each
/// precompile closure can look up its
/// [`crate::state_adapter::CacheStore`] by the session id beaconed at
/// slot 0 of [`ARKIV_ADDRESS`].
#[derive(Debug, Clone)]
pub struct ArkivOpEvmFactory {
    inner: OpEvmFactory<OpTx>,
    sessions: SessionCacheMap,
}

impl Default for ArkivOpEvmFactory {
    fn default() -> Self {
        Self {
            inner: OpEvmFactory::<OpTx>::default(),
            sessions: new_session_cache_map(),
        }
    }
}

impl ArkivOpEvmFactory {
    pub fn new(sessions: SessionCacheMap) -> Self {
        Self {
            inner: OpEvmFactory::<OpTx>::default(),
            sessions,
        }
    }

    /// Handle to the shared session-cache map. Cloned into the
    /// BlockExecutor wrapper so it can insert / remove `CacheStore`s
    /// under the session id minted at `apply_pre_execution_changes`.
    pub fn sessions(&self) -> &SessionCacheMap {
        &self.sessions
    }

    fn install<E>(&self, evm: &mut E)
    where
        E: Evm<Precompiles = PrecompilesMap>,
    {
        let precompile = arkiv_precompile(self.sessions.clone());
        evm.precompiles_mut()
            .apply_precompile(&ARKIV_ADDRESS, |_existing| Some(precompile));
    }
}

impl EvmFactory for ArkivOpEvmFactory {
    // Mirror `OpEvmFactory<OpTx>`'s associated types concretely. Forwarding
    // through `<OpEvmFactory<OpTx> as EvmFactory>::X` projections compiles
    // here but leaves bounds like `Self::Tx: FromRecoveredTx<_>` unresolved
    // in downstream `ConfigureEvm` / `OpAddOns` impls, because the compiler
    // does not always normalise nested projections through trait bounds.
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> = ArkivOpEvm<DB, I>;
    type Context<DB: Database> = OpEvmContext<DB>;
    type Tx = OpTx;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError, OpTxError>;
    type HaltReason = OpHaltReason;
    type Spec = OpSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let mut evm = self.inner.create_evm(db, input);
        self.install(&mut evm);
        ArkivOpEvm { inner: evm }
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let mut evm = self.inner.create_evm_with_inspector(db, input, inspector);
        self.install(&mut evm);
        ArkivOpEvm { inner: evm }
    }
}

// `OpBlockExecutorFactory` only implements `BlockExecutorFactory` for
// the default `OpEvmFactory` or a custom factory wrapped in
// `PostExecEvmFactoryAdapter`, so the adapter requires our factory to
// expose the SDM post-exec hooks. We delegate to the inner `OpEvm`,
// which carries the real implementation.
impl PostExecEvmFactoryHooks for ArkivOpEvmFactory {
    fn begin_post_exec_tx<DB, I>(evm: &mut Self::Evm<DB, I>, ctx: PostExecTxContext)
    where
        DB: Database,
        I: Inspector<Self::Context<DB>>,
    {
        evm.inner.begin_post_exec_tx(ctx);
    }

    fn take_last_post_exec_tx_result<DB, I>(evm: &mut Self::Evm<DB, I>) -> PostExecExecutedTx
    where
        DB: Database,
        I: Inspector<Self::Context<DB>>,
    {
        evm.inner.take_last_post_exec_tx_result()
    }
}

/// Newtype around [`OpEvm`] that puts a `tracing::debug_span!("evm_tx")`
/// around each `transact_raw` call. The span boundary captures *only*
/// the EVM-internal execution of a transaction (Solidity bytecode +
/// nested precompile call). Everything outside — block assembly,
/// payload building, state-root, sealing, RPC, receipt polling — runs
/// outside this span, so subtracting children (e.g. `precompile_call`)
/// from `evm_tx` yields a clean contract-execution slice.
///
/// Every `Evm` trait method that isn't `transact_raw` is a passthrough.
pub struct ArkivOpEvm<DB: Database, I> {
    pub(super) inner: OpEvm<DB, I, PrecompilesMap, OpTx>,
}

impl<DB, I> Evm for ArkivOpEvm<DB, I>
where
    DB: Database,
    I: Inspector<OpEvmContext<DB>>,
{
    type DB = DB;
    type Tx = OpTx;
    type Error = EVMError<DB::Error, OpTxError>;
    type HaltReason = OpHaltReason;
    type Spec = OpSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;
    type Inspector = I;

    fn block(&self) -> &Self::BlockEnv {
        self.inner.block()
    }

    fn cfg_env(&self) -> &CfgEnv<Self::Spec> {
        self.inner.cfg_env()
    }

    fn chain_id(&self) -> u64 {
        self.inner.chain_id()
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        let _span = tracing::debug_span!("evm_tx").entered();
        self.inner.transact_raw(tx)
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.inner.transact_system_call(caller, contract, data)
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec, Self::BlockEnv>) {
        self.inner.finish()
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inner.set_inspector_enabled(enabled);
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        self.inner.components()
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        self.inner.components_mut()
    }
}
