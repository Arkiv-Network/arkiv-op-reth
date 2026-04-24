mod genesis;

use reth_optimism_cli::Cli;
use reth_optimism_node::OpNode;

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

            // Ensure OP hardfork activation timestamps exist in the genesis
            // config extra fields. OpChainSpec::from(Genesis) reads these to
            // build the hardfork schedule. The dev chain spec sets hardforks
            // programmatically but may not populate extra_fields, so we
            // default them to 0 (active at genesis) when absent.
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

        let handle = builder
            .node(OpNode::new(rollup_args))
            .launch_with_debug_capabilities()
            .await?;
        handle.wait_for_node_exit().await
    })
}
