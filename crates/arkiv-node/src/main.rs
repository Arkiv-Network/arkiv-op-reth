mod exex;

use alloy_primitives::{Address, U256};
use arkiv_genesis::{generate_genesis, GenesisConfig};
use arkiv_store::logging::LoggingStore;
use futures::future;
use reth::chainspec::ChainSpec;
use reth::cli::Cli;
use reth_node_ethereum::EthereumNode;
use std::sync::Arc;

/// Default dev account (first account from "test test test test test test test test test test test junk").
/// Private key: 0xac0974bec39a17e36ba4a6b4d238ff944bacb478c6b8d6c1f02960247590a993
const DEV_ADDRESS: Address = Address::new([
    0xf3, 0x9F, 0xd6, 0xe5, 0x1a, 0xad, 0x88, 0xF6, 0xF4, 0xce,
    0x6a, 0xB8, 0x82, 0x72, 0x79, 0xcf, 0xfF, 0xb9, 0x22, 0x66,
]);

/// 1,000,000 ETH
const DEV_BALANCE: U256 = U256::from_limbs([0xD3C21BCECCEDA1000000_u128 as u64, (0xD3C21BCECCEDA1000000_u128 >> 64) as u64, 0, 0]);

fn main() -> eyre::Result<()> {
    Cli::parse_args().run(|mut builder, _| async move {
        // Generate chain spec with EntityRegistry predeployed and dev account funded.
        let config = GenesisConfig {
            prefunded_accounts: vec![(DEV_ADDRESS, DEV_BALANCE)],
            ..Default::default()
        };
        let genesis = generate_genesis(&config)?;
        let chain_spec = Arc::new(ChainSpec::from(genesis));
        builder.config_mut().chain = chain_spec;

        let store = Arc::new(LoggingStore::new(arkiv_genesis::ENTITY_REGISTRY_ADDRESS));

        let handle = builder
            .node(EthereumNode::default())
            .install_exex("arkiv-exex", move |ctx| {
                let store = store.clone();
                future::ok(exex::arkiv_exex(ctx, store))
            })
            .launch_with_debug_capabilities()
            .await?;

        handle.wait_for_node_exit().await
    })
}
