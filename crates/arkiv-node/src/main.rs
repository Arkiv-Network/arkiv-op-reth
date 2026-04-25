mod exex;
mod genesis;
mod rpc;
mod storage;

use reth_optimism_cli::Cli;
use reth_optimism_node::OpNode;
use rpc::{ArkivApiServer, ArkivRpc};
use std::sync::Arc;

fn main() -> eyre::Result<()> {
    Cli::parse_args().run(|mut builder, rollup_args| async move {
        // Inject EntityRegistry predeploy + dev account into the chain spec genesis.
        {
            let config = builder.config_mut();
            let chain_id = config.chain.genesis.config.chain_id;

            let mut chain_genesis = config.chain.genesis.clone();
            chain_genesis
                .alloc
                .extend(genesis::genesis_alloc(chain_id)?);

            let extra = &mut chain_genesis.config.extra_fields;
            let zero = serde_json::Value::Number(0.into());
            for key in [
                "bedrockBlock",
                "regolithTime",
                "canyonTime",
                "deltaTime",
                "ecotoneTime",
                "fjordTime",
                "graniteTime",
                "holoceneTime",
                "isthmusTime",
            ] {
                extra.entry(key.to_string()).or_insert(zero.clone());
            }
            if !extra.contains_key("optimism") {
                extra.insert(
                    "optimism".to_string(),
                    serde_json::json!({
                        "eip1559Elasticity": 6,
                        "eip1559Denominator": 50,
                        "eip1559DenominatorCanyon": 250
                    }),
                );
            }

            config.chain =
                std::sync::Arc::new(reth_optimism_chainspec::OpChainSpec::from(chain_genesis));
        }

        let (store, query_store): (
            Arc<dyn storage::Storage>,
            Option<Arc<storage::rocks::RocksDbStore>>,
        ) = if let Ok(url) = std::env::var("ARKIV_ENTITYDB_URL") {
            tracing::info!(url = %url, "using JsonRpcStore backend");
            (Arc::new(storage::jsonrpc::JsonRpcStore::new(url)), None)
        } else if std::env::var("ARKIV_LOGGING_STORE").is_ok() {
            tracing::info!("using LoggingStore backend");
            (
                Arc::new(storage::logging::LoggingStore::new(
                    genesis::ENTITY_REGISTRY_ADDRESS,
                )),
                None,
            )
        } else {
            let path =
                std::env::var("ARKIV_ROCKSDB_PATH").unwrap_or_else(|_| "arkiv-rocksdb".to_string());
            tracing::info!(path = %path, "using RocksDbStore backend");
            let rocks = Arc::new(storage::rocks::RocksDbStore::open(path)?);
            (rocks.clone(), Some(rocks))
        };

        let handle = builder
            .node(OpNode::new(rollup_args))
            .extend_rpc_modules(move |ctx| {
                if let Some(query_store) = query_store.clone() {
                    ctx.modules
                        .merge_configured(ArkivApiServer::into_rpc(ArkivRpc::new(query_store)))?;
                }
                Ok(())
            })
            .install_exex("arkiv", {
                let store = Arc::clone(&store);
                move |ctx| async move { Ok(exex::arkiv_exex(ctx, store)) }
            })
            .launch_with_debug_capabilities()
            .await?;

        handle.wait_for_node_exit().await
    })
}
