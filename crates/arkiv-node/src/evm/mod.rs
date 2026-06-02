//! Custom EVM stack that installs the Arkiv precompile on every fresh
//! revm instance, plus the per-block cache lifecycle.
//!
//! Submodules:
//!
//! - [`factory`] — [`ArkivOpEvmFactory`] (installs the precompile) and
//!   the [`ArkivOpEvm`] tracing newtype.
//! - [`config`] — [`ArkivOpEvmConfig`] (the local `ConfigureEvm` /
//!   `ConfigureEngineEvm` / `ConfigurePostExecEvm` newtype around
//!   `OpEvmConfig`).
//! - [`node`] — [`ArkivOpExecutorBuilder`] + [`ArkivOpNode`] + the
//!   verbatim payload-attributes builder used by `DebugNode`.
//! - [`block_executor`] — [`ArkivOpBlockExecutor`] +
//!   [`ArkivOpBlockExecutorFactory`], which mint the per-pass
//!   [`SessionId`] and orchestrate cache flush at `finish`.
//! - [`session`] — the [`SessionCacheMap`], slot helpers, and reserved
//!   selectors that bridge the wrapper and the precompile.

pub mod block_executor;
pub mod config;
pub mod factory;
pub mod node;
pub mod session;

pub use block_executor::{ArkivOpBlockExecutor, ArkivOpBlockExecutorFactory};
pub use config::ArkivOpEvmConfig;
pub use factory::{ArkivOpEvm, ArkivOpEvmFactory};
pub use node::{ArkivOpExecutorBuilder, ArkivOpNode};
pub use session::{
    ARKIV_SESSION_CALLER, SESSION_CLEAR_SELECTOR, SESSION_FLUSH_SELECTOR, SESSION_SET_SELECTOR,
    SessionCacheMap, SessionId, SessionKind, clear_session_slot, derive_session,
    encode_clear_session, encode_flush_session, encode_set_session, new_session_cache_map,
    write_session_slot,
};
