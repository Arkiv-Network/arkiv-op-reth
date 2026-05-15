//! Arkiv node library — op-reth wrapper with the Arkiv predeploys,
//! custom `EvmFactory` (precompile + system account), and `arkiv_*`
//! JSON-RPC namespace.

mod cli;
pub mod evm;
mod genesis;
mod install;
pub mod precompile;
pub mod rpc;

pub use cli::ArkivExt;
pub use evm::ArkivOpNode;
pub use genesis::has_arkiv_predeploy;
pub use install::install;
