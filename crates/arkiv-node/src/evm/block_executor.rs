//! BlockExecutor wrapper that owns the per-pass cache lifecycle.
//!
//! Sits between `op-reth` and the underlying `OpBlockExecutor`. On
//! `apply_pre_execution_changes` it mints a [`SessionId`], inserts an
//! empty [`CacheStore`] into the shared [`SessionCacheMap`], and asks
//! the Arkiv precompile (via [`transact_system_call`](alloy_evm::Evm::transact_system_call))
//! to journal the session id into slot 0 of
//! [`ARKIV_ADDRESS`]. On `finish` it asks the precompile to clear the
//! slot, takes the cache back from the session map, and drops it.
//!
//! All other `BlockExecutor` methods delegate to the inner executor.
//!
//! See `account-cache-design.md` §4.

use alloy_evm::{
    Evm,
    block::{BlockExecutionError, BlockExecutionResult, BlockExecutor, BlockExecutorFactory},
    revm::{DatabaseCommit, Inspector},
};
use alloy_primitives::B256;
use arkiv_genesis::ARKIV_ADDRESS;

use super::session::{
    ARKIV_SESSION_CALLER, SessionCacheMap, SessionId, encode_clear_session, encode_flush_session,
    encode_set_session,
};
use crate::state_adapter::CacheStore;

/// Wraps any [`BlockExecutorFactory`] to layer Arkiv's per-pass cache
/// lifecycle on top. Construct via [`Self::new`] from the inner
/// factory and the shared [`SessionCacheMap`].
#[derive(Debug, Clone)]
pub struct ArkivOpBlockExecutorFactory<Inner> {
    inner: Inner,
    sessions: SessionCacheMap,
}

impl<Inner> ArkivOpBlockExecutorFactory<Inner> {
    pub fn new(inner: Inner, sessions: SessionCacheMap) -> Self {
        Self { inner, sessions }
    }

    pub fn inner(&self) -> &Inner {
        &self.inner
    }
}

impl<Inner> BlockExecutorFactory for ArkivOpBlockExecutorFactory<Inner>
where
    Inner: BlockExecutorFactory,
{
    type EvmFactory = Inner::EvmFactory;
    type TxExecutionResult = Inner::TxExecutionResult;
    type ExecutionCtx<'a> = Inner::ExecutionCtx<'a>;
    type Transaction = Inner::Transaction;
    type Receipt = Inner::Receipt;
    type Executor<'a, DB, I>
        = ArkivOpBlockExecutor<Inner::Executor<'a, DB, I>>
    where
        DB: alloy_evm::block::StateDB,
        I: Inspector<<Inner::EvmFactory as alloy_evm::EvmFactory>::Context<DB>>;

    fn evm_factory(&self) -> &Self::EvmFactory {
        self.inner.evm_factory()
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: <Self::EvmFactory as alloy_evm::EvmFactory>::Evm<DB, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a, DB, I>
    where
        DB: alloy_evm::block::StateDB,
        I: Inspector<<Self::EvmFactory as alloy_evm::EvmFactory>::Context<DB>>,
    {
        ArkivOpBlockExecutor {
            inner: self.inner.create_executor(evm, ctx),
            sessions: self.sessions.clone(),
            session_id: None,
        }
    }
}

/// Wraps an inner [`BlockExecutor`] to mint a session id at
/// `apply_pre_execution_changes` and tear it down at `finish`.
pub struct ArkivOpBlockExecutor<Inner> {
    inner: Inner,
    sessions: SessionCacheMap,
    session_id: Option<SessionId>,
}

impl<Inner> ArkivOpBlockExecutor<Inner> {
    fn issue_session_set<E>(evm: &mut E, sid: SessionId) -> Result<(), BlockExecutionError>
    where
        E: Evm,
        E::DB: DatabaseCommit,
    {
        let calldata = encode_set_session(sid);
        let result = evm
            .transact_system_call(ARKIV_SESSION_CALLER, ARKIV_ADDRESS, calldata)
            .map_err(|e| {
                BlockExecutionError::msg(format!("arkiv session set system call: {e:?}"))
            })?;
        evm.db_mut().commit(result.state);
        Ok(())
    }

    fn issue_session_clear<E>(evm: &mut E) -> Result<(), BlockExecutionError>
    where
        E: Evm,
        E::DB: DatabaseCommit,
    {
        let calldata = encode_clear_session();
        let result = evm
            .transact_system_call(ARKIV_SESSION_CALLER, ARKIV_ADDRESS, calldata)
            .map_err(|e| {
                BlockExecutionError::msg(format!("arkiv session clear system call: {e:?}"))
            })?;
        evm.db_mut().commit(result.state);
        Ok(())
    }

    fn issue_session_flush<E>(evm: &mut E) -> Result<(), BlockExecutionError>
    where
        E: Evm,
        E::DB: DatabaseCommit,
    {
        let calldata = encode_flush_session();
        let result = evm
            .transact_system_call(ARKIV_SESSION_CALLER, ARKIV_ADDRESS, calldata)
            .map_err(|e| {
                BlockExecutionError::msg(format!("arkiv session flush system call: {e:?}"))
            })?;
        evm.db_mut().commit(result.state);
        Ok(())
    }
}

impl<Inner> BlockExecutor for ArkivOpBlockExecutor<Inner>
where
    Inner: BlockExecutor,
    <Inner::Evm as Evm>::DB: DatabaseCommit,
{
    type Transaction = Inner::Transaction;
    type Receipt = Inner::Receipt;
    type Evm = Inner::Evm;
    type Result = Inner::Result;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        // Inner first: lets system contracts (beacon root, EIP-2935
        // block hashes, etc.) initialise before we add our beacon.
        self.inner.apply_pre_execution_changes()?;

        let sid: SessionId = B256::random();
        self.session_id = Some(sid);

        // Insert an empty CacheStore under this session so the
        // precompile / CachedReadWriteStateAdapter can find it once wired.
        self.sessions
            .lock()
            .expect("session map poisoned")
            .insert(sid, CacheStore::new());

        // Journal the slot write through the precompile.
        Self::issue_session_set(self.inner.evm_mut(), sid)?;
        Ok(())
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl alloy_evm::block::ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        // ExecutableTx<Self> resolves to ExecutableTx<Inner> via the
        // blanket impls: Self::Transaction = Inner::Transaction and
        // Self::Evm = Inner::Evm.
        self.inner.execute_transaction_without_commit(tx)
    }

    fn commit_transaction(&mut self, output: Self::Result) -> alloy_evm::block::GasOutput {
        self.inner.commit_transaction(output)
    }

    fn finish(
        mut self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
        // Tear down BEFORE inner.finish consumes the executor — we
        // need self.inner.evm_mut() to issue the system calls, and
        // finish takes self by value.
        if let Some(sid) = self.session_id.take() {
            // Flush first: the precompile drains the per-block dirty
            // entries out of the CacheStore and writes them to
            // `State<DB>` via byte-level set_code / tombstone_code.
            // The cache is removed from the session map as a side
            // effect, so the subsequent `remove` below is a no-op on
            // the canonical path.
            Self::issue_session_flush(self.inner.evm_mut())?;
            // Then clear the session-id slot so the merged bundle
            // records no net transition on slot 0 of ARKIV_ADDRESS.
            Self::issue_session_clear(self.inner.evm_mut())?;
            // Defensive: if FLUSH didn't reach the precompile (e.g.
            // a future bypass path), reclaim the cache slot here.
            self.sessions
                .lock()
                .expect("session map poisoned")
                .remove(&sid);
        }
        self.inner.finish()
    }

    fn set_state_hook(&mut self, hook: Option<Box<dyn alloy_evm::block::OnStateHook>>) {
        self.inner.set_state_hook(hook)
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        self.inner.evm_mut()
    }

    fn evm(&self) -> &Self::Evm {
        self.inner.evm()
    }

    fn receipts(&self) -> &[Self::Receipt] {
        self.inner.receipts()
    }
}

