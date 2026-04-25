mod exex;
mod genesis;
mod query;
mod rpc;
mod storage;

use reth_optimism_cli::Cli;
use reth_optimism_node::OpNode;
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

        // Pick a storage backend:
        //   * `ARKIV_ROCKSDB_PATH` -> embedded RocksDB store (the default for production).
        //     If `ARKIV_RPC_BIND` is also set, also start the JSON-RPC query server.
        //   * `ARKIV_ENTITYDB_URL`  -> external Go EntityDB JSON-RPC backend.
        //   * neither              -> tracing/logging backend (development only).
        let rocksdb_path = std::env::var("ARKIV_ROCKSDB_PATH").ok();
        let entitydb_url = std::env::var("ARKIV_ENTITYDB_URL").ok();
        let rpc_bind = std::env::var("ARKIV_RPC_BIND").ok();

        let mut rpc_handle: Option<jsonrpsee::server::ServerHandle> = None;

        let store: Arc<dyn storage::Storage> = if let Some(path) = rocksdb_path {
            tracing::info!(path = %path, "using RocksDbStore backend");
            let rocks = Arc::new(storage::rocksdb_store::RocksDbStore::open(&path)?);

            if let Some(bind) = rpc_bind {
                let addr: std::net::SocketAddr = bind
                    .parse()
                    .map_err(|e| eyre::eyre!("invalid ARKIV_RPC_BIND '{}': {}", bind, e))?;
                tracing::info!(%addr, "starting Arkiv JSON-RPC server");
                rpc_handle = Some(rpc::spawn(Arc::clone(&rocks), addr).await?);
            }

            rocks as Arc<dyn storage::Storage>
        } else if let Some(url) = entitydb_url {
            tracing::info!(url = %url, "using JsonRpcStore backend");
            Arc::new(storage::jsonrpc::JsonRpcStore::new(url))
        } else {
            tracing::info!("using LoggingStore backend");
            Arc::new(storage::logging::LoggingStore::new(
                genesis::ENTITY_REGISTRY_ADDRESS,
            ))
        };

        let handle = builder
            .node(OpNode::new(rollup_args))
            .install_exex("arkiv", {
                let store = Arc::clone(&store);
                move |ctx| async move { Ok(exex::arkiv_exex(ctx, store)) }
            })
            .launch_with_debug_capabilities()
            .await?;

        let exit_status = handle.wait_for_node_exit().await;

        if let Some(handle) = rpc_handle {
            let _ = handle.stop();
            handle.stopped().await;
        }

        exit_status
    })
}
