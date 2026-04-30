use arkiv_node::{ArkivExt, install, resolve_mode};
use clap::Parser;
use eyre::Result;
use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};
use reth_optimism_node::OpNode;

fn main() -> Result<()> {
    Cli::<OpChainSpecParser, ArkivExt>::parse().run(|builder, ext| async move {
        let mode = resolve_mode(ext.arkiv_db_url, ext.arkiv_debug, &builder.config().chain).await?;
        let node = install(builder.node(OpNode::new(ext.rollup)), mode);
        let handle = node.launch_with_debug_capabilities().await?;
        handle.wait_for_node_exit().await
    })
}
