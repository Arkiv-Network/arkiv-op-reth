mod exex;
mod genesis;
mod storage;

use reth_optimism_cli::Cli;
use reth_optimism_node::OpNode;
use std::sync::Arc;

fn main() -> eyre::Result<()> {
    Cli::parse_args().run(|mut builder, rollup_args| async move {
        // Merge the EntityRegistry predeploy + dev account into the chain spec genesis,
        // *without* overriding anything that the user already provided in their own
        // `--chain <genesis.json>`. This keeps the genesis hash stable for users who
        // bring a pre-generated genesis (e.g. from op-deployer) that already contains
        // the EntityRegistry contract.
        {
            let config = builder.config_mut();

            let mut chain_genesis = config.chain.genesis.clone();

            // Respect a user-provided chain_id; only fall back to the dev default
            // (1337) when the chain spec does not specify one.
            let chain_id = if chain_genesis.config.chain_id == 0 {
                chain_genesis.config.chain_id = 1337;
                1337u64
            } else {
                chain_genesis.config.chain_id
            };

            // Only insert predeploy/dev entries that the user has *not* already
            // provided. This preserves the genesis hash for custom genesis files
            // produced by op-deployer (which typically already include the
            // EntityRegistry contract at its predeploy address).
            let mut injected_any = false;
            for (addr, account) in genesis::genesis_alloc(chain_id)? {
                if let std::collections::btree_map::Entry::Vacant(slot) =
                    chain_genesis.alloc.entry(addr)
                {
                    slot.insert(account);
                    injected_any = true;
                    tracing::info!(address = %addr, "injected default genesis account");
                } else {
                    tracing::info!(
                        address = %addr,
                        "genesis already contains account; preserving user-provided value"
                    );
                }
            }
            if !injected_any {
                tracing::info!(
                    "user-provided genesis already contains all default Arkiv accounts; \
                     no injection needed"
                );
            }

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

            // Optionally dump the fully-resolved genesis (with our injections applied)
            // to a JSON file and exit. This lets operators generate a canonical
            // genesis.json that matches what the node will actually load, which can
            // then be reused as the `--chain` argument so the database hash stays
            // stable across restarts.
            if let Ok(path) = std::env::var("ARKIV_DUMP_GENESIS") {
                let json = serde_json::to_string_pretty(&chain_genesis)
                    .map_err(|e| eyre::eyre!("failed to serialize genesis: {e}"))?;
                std::fs::write(&path, json)
                    .map_err(|e| eyre::eyre!("failed to write genesis to {path}: {e}"))?;
                tracing::info!(path = %path, "wrote resolved genesis.json; exiting");
                return Ok(());
            }

            config.chain =
                std::sync::Arc::new(reth_optimism_chainspec::OpChainSpec::from(chain_genesis));
        }

        let store: Arc<dyn storage::Storage> = if let Ok(url) = std::env::var("ARKIV_ENTITYDB_URL")
        {
            tracing::info!(url = %url, "using JsonRpcStore backend");
            let store = storage::jsonrpc::JsonRpcStore::new(url.clone());
            if let Err(e) = store.health_check().await {
                tracing::error!(url = %url, error = %e, "EntityDB health check failed; exiting");
                return Err(e);
            }
            Arc::new(store)
        } else {
            tracing::info!("using LoggingStore backend");
            Arc::new(storage::logging::LoggingStore::new())
        };

        let handle = builder
            .node(OpNode::new(rollup_args))
            .install_exex("arkiv", {
                let store = Arc::clone(&store);
                move |ctx| async move { Ok(exex::arkiv_exex(ctx, store)) }
            })
            .launch_with_debug_capabilities()
            .await?;

        handle.wait_for_node_exit().await
    })
}
