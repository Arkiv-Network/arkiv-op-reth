// TODO: re-enable once arkiv-exex crate is extracted
// mod exex;

use reth_optimism_cli::Cli;
use reth_optimism_node::OpNode;

fn main() -> eyre::Result<()> {
    Cli::parse_args().run(|builder, rollup_args| async move {
        let handle = builder
            .node(OpNode::new(rollup_args))
            .launch_with_debug_capabilities()
            .await?;
        handle.wait_for_node_exit().await
    })
}
