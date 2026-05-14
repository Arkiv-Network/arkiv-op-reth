//! End-to-end test for the `arkiv_query` JSON-RPC method.
//!
//! Boots an `ArkivOpNode`, submits an `EntityRegistry.execute([CREATE
//! op])` tx that the precompile applies to entity / pair / system
//! state, mines the block, then calls `arkiv_query("*")` over JSON-RPC
//! and asserts that the response contains the entity we just created.
//!
//! Exercises the full read path: parse → evaluate (`RethStateAdapter`
//! over the latest `StateProvider`) → resolve IDs → render `EntityData`.

use alloy_eips::eip2718::Encodable2718;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B64, B256, Bytes, FixedBytes, TxKind, U256, hex};
use alloy_rpc_types_engine::PayloadAttributes;
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use alloy_sol_types::{SolCall, sol};
use arkiv_genesis::ENTITY_REGISTRY_ADDRESS;
use arkiv_node::{ArkivOpNode, install};
use jsonrpsee::core::client::ClientT;
use reth_e2e_test_utils::{node::NodeTestContext, transaction::TransactionTestContext};
use reth_node_builder::{NodeBuilder, NodeHandle};
use reth_node_core::{args::RpcServerArgs, node_config::NodeConfig};
use reth_optimism_chainspec::OpChainSpecBuilder;
use reth_optimism_node::payload::{OpPayloadAttributes, OpPayloadAttrs};
use reth_tasks::Runtime;
use serde::Deserialize;

// Mirror of the v1-compatible `EntityRegistry.execute(Operation[])` ABI.
sol! {
    #[derive(Debug)]
    struct Mime128 { bytes32[4] data; }

    #[derive(Debug)]
    struct Attribute { bytes32 name; uint8 valueType; bytes32[4] value; }

    #[derive(Debug)]
    struct Operation {
        uint8 operationType;
        bytes32 entityKey;
        bytes payload;
        Mime128 contentType;
        Attribute[] attributes;
        uint32 btl;
        address newOwner;
    }

    function execute(Operation[] ops) external;
}

const OP_CREATE: u8 = 1;

/// Shape of the `arkiv_query` response. Field selection here is what the
/// SDK will deserialize against — failing to deserialize means the wire
/// shape drifted.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryResponse {
    data: Vec<EntityData>,
    #[allow(dead_code)]
    block_number: u64,
    #[allow(dead_code)]
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EntityData {
    #[allow(dead_code)]
    key: B256,
    value: Bytes,
    content_type: String,
    expires_at: u64,
    owner: Address,
    creator: Address,
    created_at_block: u64,
}

#[tokio::test(flavor = "multi_thread")]
async fn arkiv_query_returns_created_entity() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut genesis: Genesis =
        serde_json::from_str(include_str!("../../chainspec/dev.base.json"))?;
    for (addr, account) in arkiv_genesis::genesis_alloc()? {
        genesis.alloc.insert(addr, account);
    }
    let chain_spec = OpChainSpecBuilder::base_mainnet()
        .genesis(genesis)
        .isthmus_activated()
        .build();
    let chain_id = chain_spec.chain.id();

    let runtime = Runtime::test();
    let node_config = NodeConfig::test()
        .map_chain(chain_spec)
        .with_rpc(RpcServerArgs::default().with_http().with_unused_ports());
    // `install(...)` adds the `arkiv_*` RPC namespace on top of the
    // EvmFactory/precompile wiring that `ArkivOpNode` brings in.
    let builder = NodeBuilder::new(node_config)
        .testing_node(runtime)
        .node(ArkivOpNode::default());
    let NodeHandle { node, node_exit_future: _ } = install(builder).launch().await?;
    let mut node = NodeTestContext::new(node, payload_attrs_with_l1_info_deposit).await?;
    let rpc = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("rpc client unavailable"))?;

    // ── Submit a CREATE op ──────────────────────────────────────────

    let create_op = Operation {
        operationType: OP_CREATE,
        entityKey: B256::ZERO,
        payload: Bytes::from(vec![0x11, 0x22, 0x33, 0x44]),
        contentType: mime("text/plain"),
        attributes: vec![],
        btl: 1_800,
        newOwner: Address::ZERO,
    };
    let calldata = executeCall { ops: vec![create_op] }.abi_encode();

    let entity_signer = arkiv_genesis::dev_signers(1)?
        .into_iter()
        .next()
        .expect("dev signer 0");
    let sender = entity_signer.address();

    let tx_req = TransactionRequest {
        from: Some(sender),
        to: Some(TxKind::Call(ENTITY_REGISTRY_ADDRESS)),
        input: TransactionInput::new(calldata.into()),
        nonce: Some(0),
        gas: Some(500_000),
        max_fee_per_gas: Some(20_000_000_000),
        max_priority_fee_per_gas: Some(20_000_000_000),
        chain_id: Some(chain_id),
        value: Some(U256::ZERO),
        ..Default::default()
    };
    let signed = TransactionTestContext::sign_tx(entity_signer, tx_req).await;
    let raw_tx: Bytes = signed.encoded_2718().into();
    let _tx_hash = node.rpc.inject_tx(raw_tx).await?;
    let _payload = node.advance_block().await?;

    // ── arkiv_query("*") ────────────────────────────────────────────

    let response: QueryResponse = rpc
        .request(
            "arkiv_query",
            ("*", serde_json::Value::Null),
        )
        .await?;

    assert_eq!(
        response.data.len(),
        1,
        "expected exactly one entity; got: {:#?}",
        response.data,
    );
    let entity = &response.data[0];
    assert_eq!(entity.owner, sender, "owner should be the tx sender");
    assert_eq!(entity.creator, sender, "creator should be the tx sender");
    assert_eq!(entity.value.as_ref(), &[0x11, 0x22, 0x33, 0x44]);
    assert_eq!(entity.content_type, "text/plain");
    assert!(entity.expires_at > 0, "expires_at should be > 0");
    assert!(entity.created_at_block > 0, "created_at_block should be > 0");

    Ok(())
}

/// Build a Mime128 from a UTF-8 string, padded with trailing zeros.
fn mime(s: &str) -> Mime128 {
    let mut buf = [0u8; 128];
    buf[..s.len()].copy_from_slice(s.as_bytes());
    let words: [FixedBytes<32>; 4] = [
        FixedBytes::from_slice(&buf[..32]),
        FixedBytes::from_slice(&buf[32..64]),
        FixedBytes::from_slice(&buf[64..96]),
        FixedBytes::from_slice(&buf[96..128]),
    ];
    Mime128 { data: words }
}

/// Canonical L1-info deposit tx, forced as the first tx of every block.
/// See precompile_e2e.rs for the full explanation.
const L1_INFO_DEPOSIT_TX: [u8; 251] = hex!(
    "7ef8f8a0683079df94aa5b9cf86687d739a60a9b4f0835e520ec4d664e2e415dca17a6df94deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e200000146b000f79c500000000000000040000000066d052e700000000013ad8a3000000000000000000000000000000000000000000000000000000003ef1278700000000000000000000000000000000000000000000000000000000000000012fdf87b89884a61e74b322bbcf60386f543bfae7827725efaaf0ab1de2294a590000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985"
);

fn payload_attrs_with_l1_info_deposit(timestamp: u64) -> OpPayloadAttrs {
    OpPayloadAttrs(OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
        },
        transactions: Some(vec![L1_INFO_DEPOSIT_TX.into()]),
        no_tx_pool: None,
        gas_limit: Some(30_000_000),
        eip_1559_params: Some(B64::ZERO),
        min_base_fee: Some(0),
    })
}
