//! Arkiv-specific clap args. Designed to be `#[command(flatten)]`-ed into
//! a host CLI so downstream binaries can compose Arkiv onto their own
//! argument surface.

use reth_optimism_node::args::RollupArgs;

/// CLI extension over [`RollupArgs`]. Adds Arkiv-specific flags.
#[derive(Debug, clap::Args)]
pub struct ArkivExt {
    /// EntityDB JSON-RPC URL. On an Arkiv chainspec, enables the ExEx
    /// (forwarding to EntityDB) and the `arkiv_query` JSON-RPC method.
    #[arg(long = "arkiv.db-url", env = "ARKIV_ENTITYDB_URL")]
    pub arkiv_db_url: Option<String>,

    /// Debug mode: run the ExEx with the in-process `LoggingStore` backend
    /// (decoded ops are emitted as tracing events). Useful for local dev
    /// without a running EntityDB. The `arkiv_*` RPC namespace is not
    /// installed in this mode.
    #[arg(long = "arkiv.debug", conflicts_with = "arkiv_db_url")]
    pub arkiv_debug: bool,

    #[command(flatten)]
    pub rollup: RollupArgs,
}
