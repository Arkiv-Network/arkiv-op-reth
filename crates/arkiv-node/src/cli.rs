//! Arkiv-specific clap args. Designed to be `#[command(flatten)]`-ed into
//! a host CLI so downstream binaries can compose Arkiv onto their own
//! argument surface.

use reth_optimism_node::args::RollupArgs;

#[derive(Debug, clap::Args)]
pub struct ArkivExt {
    #[command(flatten)]
    pub rollup: RollupArgs,
}
