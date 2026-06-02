//! Session-id plumbing for the per-block account cache.
//!
//! Each in-flight block-execution pass on a node carries its own
//! `State<DB>` working copy. The cache must be per-pass — two passes
//! for the same `block_number` must not see each other's in-flight
//! typed values.
//!
//! The session-id beacon is stored in slot 0 of [`ARKIV_ADDRESS`] —
//! the same precompile registration target. Storing it on the
//! precompile's own account keeps everything Arkiv-related in one
//! well-known address: the
//! [`BlockExecutor`](crate::store::ArkivOpBlockExecutor) wrapper
//! invokes the precompile with [`ARKIV_SESSION_CALLER`] as caller
//! and a `SESSION_SET` / `SESSION_CLEAR` selector to journal the
//! slot transition; the precompile reads the slot on every
//! user-issued call (via [`derive_session`]) to find its
//! [`CacheStore`] in the shared [`SessionCacheMap`].
//!
//! Net effect on canonical state: the slot round-trips through
//! `0 → sid → 0` within a single block, so the merged bundle records
//! no transition on [`ARKIV_ADDRESS`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use alloy_evm::EvmInternals;
use alloy_primitives::{B256, Bytes, U256};
use arkiv_genesis::ARKIV_ADDRESS;
use eyre::Result;

use super::cache_store::CacheStore;

/// Standard EIP-4788 system caller used for journaled block-level
/// state mutations. The
/// [`BlockExecutor`](crate::store::ArkivOpBlockExecutor) wrapper
/// passes this as the caller to `transact_system_call`; the
/// precompile gates its session-write branch on this address.
pub use alloy_eips::eip4788::SYSTEM_ADDRESS as ARKIV_SESSION_CALLER;

/// Per-pass identity for a `State<DB>` instance. Minted by the
/// [`BlockExecutor`](crate::store::ArkivOpBlockExecutor) wrapper at
/// `apply_pre_execution_changes`; used by the precompile (via
/// [`derive_session`]) to look up its [`CacheStore`] in the shared
/// map.
pub type SessionId = B256;

/// Slot 0 of [`ARKIV_ADDRESS`] carries the session id.
pub(crate) const SESSION_SLOT: U256 = U256::ZERO;

/// Magic selector the
/// [`BlockExecutor`](crate::store::ArkivOpBlockExecutor) wrapper uses
/// on its `apply_pre` system call to ask the precompile to SSTORE
/// the session id into slot 0 of [`ARKIV_ADDRESS`]. Picked to be
/// unambiguously non-ABI: real Solidity selectors are
/// `keccak256(signature)[:4]`, so a fixed sentinel is statistically
/// safe and structurally distinct.
pub const SESSION_SET_SELECTOR: [u8; 4] = [0xFF, 0xFE, 0x00, 0x01];

/// Counterpart to [`SESSION_SET_SELECTOR`] for the `finish`-time
/// system call that clears the slot.
pub const SESSION_CLEAR_SELECTOR: [u8; 4] = [0xFF, 0xFE, 0x00, 0x02];

/// What [`derive_session`] returned. Speculative lanes (gas
/// estimation, pending-state `eth_call`) read a zero slot because no
/// BlockExecutor wrapper ran `apply_pre`; they get
/// [`Self::Speculative`] and fall through to a non-caching code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Canonical(SessionId),
    Speculative,
}

/// Shared registry of live [`CacheStore`]s keyed by [`SessionId`].
/// Cloned into every BlockExecutor wrapper and into the precompile
/// closure. The outer `Mutex` serialises only `HashMap` insert /
/// remove; per-call work runs on a locally-owned `CacheStore` checked
/// out under a brief lock.
pub type SessionCacheMap = Arc<Mutex<HashMap<SessionId, CacheStore>>>;

/// Construct an empty session-cache map.
pub fn new_session_cache_map() -> SessionCacheMap {
    Arc::new(Mutex::new(HashMap::new()))
}

/// SLOAD slot 0 of [`ARKIV_ADDRESS`]. Non-zero → canonical pass;
/// zero → speculative lane that bypassed the BlockExecutor wrapper
/// (gas estimation, pending-state `eth_call`).
pub fn derive_session(internals: &mut EvmInternals<'_>) -> Result<SessionKind> {
    let raw = internals
        .sload(ARKIV_ADDRESS, SESSION_SLOT)
        .map_err(|e| eyre::eyre!("session sload: {e:?}"))?
        .data;
    if raw == U256::ZERO {
        Ok(SessionKind::Speculative)
    } else {
        Ok(SessionKind::Canonical(B256::from(raw.to_be_bytes())))
    }
}

/// SSTORE the session id into slot 0 of [`ARKIV_ADDRESS`]. Called
/// from inside the precompile when the wrapper's `apply_pre` system
/// call carries [`SESSION_SET_SELECTOR`].
pub fn write_session_slot(internals: &mut EvmInternals<'_>, sid: SessionId) -> Result<()> {
    let val = U256::from_be_bytes(sid.0);
    internals
        .sstore(ARKIV_ADDRESS, SESSION_SLOT, val)
        .map_err(|e| eyre::eyre!("session sstore (set): {e:?}"))?;
    Ok(())
}

/// SSTORE slot 0 back to zero. Pairs with [`write_session_slot`]: the
/// merged bundle records no net transition for slot 0 across the
/// block.
pub fn clear_session_slot(internals: &mut EvmInternals<'_>) -> Result<()> {
    internals
        .sstore(ARKIV_ADDRESS, SESSION_SLOT, U256::ZERO)
        .map_err(|e| eyre::eyre!("session sstore (clear): {e:?}"))?;
    Ok(())
}

/// Calldata the BlockExecutor wrapper passes to
/// [`transact_system_call`](alloy_evm::Evm::transact_system_call) at
/// `apply_pre` to set the session id.
pub fn encode_set_session(sid: SessionId) -> Bytes {
    let mut buf = Vec::with_capacity(4 + 32);
    buf.extend_from_slice(&SESSION_SET_SELECTOR);
    buf.extend_from_slice(sid.as_slice());
    Bytes::from(buf)
}

/// Calldata the BlockExecutor wrapper passes at `finish` to clear
/// the session id.
pub fn encode_clear_session() -> Bytes {
    Bytes::from(SESSION_CLEAR_SELECTOR.to_vec())
}
