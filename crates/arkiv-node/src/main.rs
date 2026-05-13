use arkiv_node::{ArkivExt, has_arkiv_predeploy, install};
use clap::Parser;
use eyre::{Result, bail};
use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};
use reth_optimism_node::OpNode;

fn main() -> Result<()> {
    Cli::<OpChainSpecParser, ArkivExt>::parse().run(|builder, ext| async move {
        let ArkivExt { rollup } = ext;

        // Hard fail if the predeploy is missing — v2 has no fallback mode.
        // (Phase 2+ will install the precompile / RPC / table iff this is true.)
        if !has_arkiv_predeploy(&builder.config().chain) {
            bail!(
                "EntityRegistry predeploy not detected at {} in the loaded chainspec; \
                 arkiv-node currently requires a chainspec with the Arkiv predeploy",
                arkiv_genesis::ENTITY_REGISTRY_ADDRESS,
            );
        }

        let node = install(builder.node(OpNode::new(rollup)));
        let handle = node.launch_with_debug_capabilities().await?;
        handle.wait_for_node_exit().await
    })
}
