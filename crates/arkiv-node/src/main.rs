use arkiv_node::{ArkivExt, ArkivStoragedProcess, install, resolve_mode};
use clap::Parser;
use eyre::Result;
use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};
use reth_optimism_node::OpNode;
use std::time::Duration;
use tokio::time::sleep;

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
        let wait_for_storaged = arkiv_storaged_path.is_some();

        let mut storaged = arkiv_storaged_path
            .map(|path| ArkivStoragedProcess::start(path, arkiv_storaged_args))
            .transpose()?;

        let mode = if wait_for_storaged {
            tracing::info!("waiting for arkiv-storaged / EntityDB to become ready");

            let mut last_err = None;
            let mut mode = None;
            for attempt in 1..=5 {
                match resolve_mode(
                    arkiv_db_url.clone(),
                    arkiv_query_url.clone(),
                    arkiv_debug,
                    &builder.config().chain,
                )
                .await
                {
                    Ok(resolved) => {
                        mode = Some(resolved);
                        break;
                    }
                    Err(err) => {
                        last_err = Some(err);
                        if attempt == 5 {
                            break;
                        }

                        tracing::info!(
                            attempt,
                            remaining = 5 - attempt,
                            "waiting for arkiv-storaged / EntityDB to become ready"
                        );
                        sleep(Duration::from_secs(1)).await;
                    }
                }
            }

            match mode {
                Some(mode) => mode,
                None => {
                    if let Some(storaged) = storaged {
                        storaged.shutdown().await?;
                    }
                    return Err(last_err.expect("retry loop must set last error"));
                }
            }
        } else {
            match resolve_mode(
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
