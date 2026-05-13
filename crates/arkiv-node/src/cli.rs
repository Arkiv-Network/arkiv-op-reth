//! Arkiv-specific clap args. Designed to be `#[command(flatten)]`-ed into
//! a host CLI so downstream binaries can compose Arkiv onto their own
//! argument surface.
//!
//! v1 flags (`--arkiv.db-url`, `--arkiv.query-url`, `--arkiv.debug`,
//! `--arkiv-storaged-path`, `--arkiv-storaged-args`) have been removed.
//! v2 has no equivalents — the precompile is on iff the predeploy is in
//! the chainspec. New v2 flags will be added here as they appear.

use reth_optimism_node::args::RollupArgs;

/// CLI extension over [`RollupArgs`]. Empty for now; kept as a wrapper so
/// the `Cli::<_, ArkivExt>` shape in `main.rs` stays stable as v2 flags
/// land.
#[derive(Debug, clap::Args)]
pub struct ArkivExt {
    #[command(flatten)]
    pub rollup: RollupArgs,
}
