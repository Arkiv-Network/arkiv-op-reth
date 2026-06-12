//! Arkiv node library — op-reth wrapper with the custom `EvmFactory`
//! (Arkiv precompile) and the `arkiv_*` JSON-RPC namespace.

pub mod bitmap_cache;
mod cli;
pub mod evm;
mod install;
pub mod precompile;
pub mod rpc;
pub mod state_adapter;

pub use cli::ArkivExt;
pub use evm::ArkivOpNode;
pub use install::install;
