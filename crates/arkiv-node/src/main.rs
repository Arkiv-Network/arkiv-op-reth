use arkiv_node::{ArkivExt, ArkivStoragedProcess, install, resolve_mode};
use clap::Parser;
use eyre::Result;
use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};
use reth_optimism_node::OpNode;

fn main() -> Result<()> {
    Cli::<OpChainSpecParser, ArkivExt>::parse().run(|builder, ext| async move {
        let ArkivExt {
            arkiv_db_url,
            arkiv_query_url,
            arkiv_debug,
            arkiv_storaged_path,
            arkiv_storaged_args,
            rollup,
        } = ext;

        let mut storaged = arkiv_storaged_path
            .map(|path| ArkivStoragedProcess::start(path, arkiv_storaged_args))
            .transpose()?;

        let mode = match resolve_mode(
            arkiv_db_url,
            arkiv_query_url,
            arkiv_debug,
            &builder.config().chain,
        )
        .await
        {
            Ok(mode) => mode,
            Err(err) => {
                if let Some(storaged) = storaged {
                    storaged.shutdown().await?;
                }
                return Err(err);
            }
        };

        if let Some(storaged) = storaged.as_mut() {
            storaged.ensure_running()?;
        }

        let node = install(builder.node(OpNode::new(rollup)), mode);
        let handle = match node.launch_with_debug_capabilities().await {
            Ok(handle) => handle,
            Err(err) => {
                if let Some(storaged) = storaged {
                    storaged.shutdown().await?;
                }
                return Err(err);
            }
        };

        if let Some(storaged) = storaged.as_mut() {
            storaged.ensure_running()?;
        }

        if let Some(storaged) = storaged {
            storaged
                .run_until_node_exit(handle.wait_for_node_exit())
                .await
        } else {
            handle.wait_for_node_exit().await
        }
    })
}
