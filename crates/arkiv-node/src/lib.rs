//! Arkiv node library.
//!
//! This crate exposes the building blocks the `arkiv-node` binary uses to
//! turn an op-stack node builder into an Arkiv node:
//!
//! - [`ArkivExt`] — clap args (`--arkiv.db-url`, `--arkiv.debug`).
//! - [`ArkivMode`] — resolved configuration (off / debug / EntityDB).
//! - [`resolve_mode`] — validates flags against the loaded chainspec and
//!   performs the EntityDB health check. Returns an [`ArkivMode`].
//! - [`install`] — wires the ExEx (and the `arkiv_*` RPC namespace, when
//!   applicable) onto an op-stack [`NodeBuilderWithComponents`].
//! - [`has_arkiv_predeploy`] — bytecode-equality check for the
//!   EntityRegistry predeploy in a chainspec's genesis alloc.
//!
//! Consumers compose these the same way op-reth's
//! `launch_node_with_proof_history` composes its ExEx onto `OpNode`.

pub mod exex;
pub mod rpc;
pub mod storage;

mod cli;
mod genesis;
mod install;

pub use cli::ArkivExt;
pub use genesis::has_arkiv_predeploy;
pub use install::{ArkivMode, install, resolve_mode};
