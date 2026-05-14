//! Arkiv node library.
//!
//! Post-v1-demolition scaffold. The v1 ExEx + EntityDB JSON-RPC bridge has
//! been removed; the v2 precompile + custom `EvmFactory` + `ArkivPairs`
//! MDBX table are not yet wired in. See `docs/v2-migration-plan.md` (in
//! workspace root) for the phased plan.
//!
//! Currently exposes:
//! - [`ArkivExt`] — clap args (empty wrapper over `RollupArgs`; will grow
//!   v2 flags as needed).
//! - [`has_arkiv_predeploy`] — bytecode-equality check for the
//!   EntityRegistry predeploy in a chainspec's genesis alloc.
//! - [`install`] — currently a no-op pass-through; Phase 2+ will fill it
//!   in with the `EvmFactory` + RPC wiring.

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
