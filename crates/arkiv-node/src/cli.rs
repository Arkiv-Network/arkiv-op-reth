//! Arkiv-specific clap args. Designed to be `#[command(flatten)]`-ed into
//! a host CLI so downstream binaries can compose Arkiv onto their own
//! argument surface.

use reth_optimism_node::args::RollupArgs;
use std::path::PathBuf;

/// CLI extension over [`RollupArgs`]. Adds Arkiv-specific flags.
#[derive(Debug, clap::Args)]
pub struct ArkivExt {
    /// EntityDB JSON-RPC URL (ExEx write API).
    #[arg(long = "arkiv.db-url", env = "ARKIV_ENTITYDB_URL")]
    pub arkiv_db_url: Option<String>,

    /// EntityDB query API URL. Defaults to `--arkiv.db-url` if omitted.
    #[arg(long = "arkiv.query-url", env = "ARKIV_QUERY_URL")]
    pub arkiv_query_url: Option<String>,

    /// Debug mode: run the ExEx with the in-process `LoggingStore` backend
    /// (decoded ops are emitted as tracing events). Useful for local dev
    /// without a running EntityDB. The `arkiv_*` RPC namespace is not
    /// installed in this mode.
    #[arg(long = "arkiv.debug", conflicts_with = "arkiv_db_url")]
    pub arkiv_debug: bool,

    /// Optional arkiv-storaged executable to run as a child process for the
    /// lifetime of arkiv-node.
    #[arg(long = "arkiv-storaged-path", env = "ARKIV_STORAGED_PATH")]
    pub arkiv_storaged_path: Option<PathBuf>,

    /// Space-separated arguments passed to `--arkiv-storaged-path`.
    #[arg(
        long = "arkiv-storaged-args",
        env = "ARKIV_STORAGED_ARGS",
        requires = "arkiv_storaged_path"
    )]
    pub arkiv_storaged_args: Option<String>,

    #[command(flatten)]
    pub rollup: RollupArgs,
}
