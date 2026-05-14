//! End-to-end test that the Arkiv precompile is correctly wired into
//! op-reth's EVM stack and reachable through `EntityRegistry`.
//!
//! Two assertions, sharing one node boot:
//!
//! 1. **Caller restriction.** `eth_call` from a non-`EntityRegistry` EOA
//!    to `0x…0045` must fail — `arkiv_precompile()`'s caller check fatals
//!    on anything other than the registry. A success (`Ok("0x")`) means
//!    the call hit an empty account, i.e. the precompile was never
//!    registered in `PrecompilesMap`.
//! 2. **Full pipeline.** A signed transaction calling
//!    `EntityRegistry.execute([CREATE op])` must succeed (receipt
//!    status `0x1`). This exercises the whole chain: contract validates
//!    `btl > 0`, mints `entityKey`, updates storage, emits
//!    `EntityOperation`, encodes `OpRecord[]`, and dispatches to the
//!    precompile, which passes the EntityRegistry caller check, decodes
//!    the records, charges stub gas, and returns success.
//!
//! Run with `RUST_LOG=arkiv_node::precompile=debug` to see the stub
//! precompile's per-op log lines fire during assertion #2.

use alloy_eips::eip2718::Encodable2718;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B64, B256, Bytes, FixedBytes, TxKind, U256, hex};
use alloy_rpc_types_engine::PayloadAttributes;
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use alloy_sol_types::{SolCall, sol};
use arkiv_genesis::{ARKIV_PRECOMPILE_ADDRESS, ENTITY_REGISTRY_ADDRESS};
use arkiv_node::ArkivOpNode;
use jsonrpsee::core::client::ClientT;
use reth_e2e_test_utils::{node::NodeTestContext, transaction::TransactionTestContext};
use reth_node_builder::{NodeBuilder, NodeHandle};
use reth_node_core::{args::RpcServerArgs, node_config::NodeConfig};
use reth_optimism_chainspec::OpChainSpecBuilder;
use reth_optimism_node::payload::{OpPayloadAttributes, OpPayloadAttrs};
use reth_tasks::Runtime;

// Mirror of the v1-compatible `EntityRegistry.execute(Operation[])` ABI
// from contracts/src/EntityRegistry.sol — declared here rather than
// pulled in from external bindings so the test stays self-contained.
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

#[tokio::test(flavor = "multi_thread")]
async fn precompile_wired_through_entity_registry() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // ── Chainspec: dev.base.json + Arkiv predeploys ─────────────────
    //
    // All OP hardforks active (matches the production dev chain).
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

    // ── Launch ArkivOpNode in-process ───────────────────────────────
    let runtime = Runtime::test();
    let node_config = NodeConfig::test()
        .map_chain(chain_spec)
        .with_rpc(RpcServerArgs::default().with_http().with_unused_ports());
    let NodeHandle { node, node_exit_future: _ } = NodeBuilder::new(node_config)
        .testing_node(runtime)
        .node(ArkivOpNode::default())
        .launch()
        .await?;
    let mut node = NodeTestContext::new(node, payload_attrs_with_l1_info_deposit).await?;
    let rpc = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("rpc client unavailable"))?;

    // ── Assertion 1: caller restriction ─────────────────────────────
    //
    // `eth_call` from a random EOA to the precompile address must fail.
    // If the precompile isn't registered the call hits an empty account
    // and succeeds with empty bytes — that's what we're disproving.
    let req = TransactionRequest {
        from: Some(Address::repeat_byte(0xee)),
        to: Some(TxKind::Call(ARKIV_PRECOMPILE_ADDRESS)),
        ..Default::default()
    };
    let result: Result<Bytes, _> = rpc.request("eth_call", (req, "latest")).await;
    assert!(
        result.is_err(),
        "expected eth_call to fail (precompile caller-restriction \
         should fatal on non-EntityRegistry caller); got Ok({:?}). \
         If this is Ok, the precompile is probably not registered in \
         PrecompilesMap — check ArkivOpEvmFactory::install.",
        result,
    );

    // ── Assertion 2: full pipeline through EntityRegistry.execute() ─
    //
    // Build a minimal CREATE op (no annotations, 3-byte payload,
    // btl=1800 blocks), sign + submit via a funded dev account, mine a
    // block, then assert the receipt's status is `0x1`. A failure here
    // could mean:
    //   - contract reverts on validation (btl=0, etc. — shouldn't happen with btl=1800)
    //   - precompile fatals (shouldn't happen for EntityRegistry caller)
    //   - block fails to mine (chainspec / hardfork issue)

    let create_op = Operation {
        operationType: OP_CREATE,
        entityKey: B256::ZERO,
        payload: Bytes::from(vec![0x01, 0x02, 0x03]),
        contentType: Mime128 { data: [FixedBytes::ZERO; 4] },
        attributes: vec![],
        btl: 1_800,
        newOwner: Address::ZERO,
    };
    let calldata = executeCall { ops: vec![create_op] }.abi_encode();

    let entity_signer = arkiv_genesis::dev_signers(1)?
        .into_iter()
        .next()
        .expect("dev signer 0");

    let tx_req = TransactionRequest {
        from: Some(entity_signer.address()),
        to: Some(TxKind::Call(ENTITY_REGISTRY_ADDRESS)),
        input: TransactionInput::new(calldata.into()),
        nonce: Some(0),
        gas: Some(500_000),
        // Match the gas pricing used by op-reth's e2e helpers
        // (20 Gwei across the board).
        max_fee_per_gas: Some(20_000_000_000),
        max_priority_fee_per_gas: Some(20_000_000_000),
        chain_id: Some(chain_id),
        value: Some(U256::ZERO),
        ..Default::default()
    };
    let signed = TransactionTestContext::sign_tx(entity_signer, tx_req).await;
    let raw_tx: Bytes = signed.encoded_2718().into();
    let tx_hash = node.rpc.inject_tx(raw_tx).await?;

    // `payload_attrs_with_l1_info_deposit` forces the canonical
    // L1-info deposit tx as the block's first tx, so the payload
    // builder produces a valid block. Our EntityRegistry tx in the
    // pool is co-mined after it.
    let _payload = node.advance_block().await?;

    let receipt: Option<serde_json::Value> = rpc
        .request("eth_getTransactionReceipt", (tx_hash,))
        .await?;
    let r = receipt.ok_or_else(|| eyre::eyre!("no receipt for tx {tx_hash}"))?;
    let status = r.get("status").and_then(|v| v.as_str());
    assert_eq!(
        status,
        Some("0x1"),
        "expected EntityRegistry.execute([CREATE]) to succeed (status=0x1); \
         got receipt = {}",
        serde_json::to_string_pretty(&r).unwrap_or_else(|_| "<unprintable>".into()),
    );

    Ok(())
}

/// L1-info deposit tx that forms the first tx of every block.
///
/// Op-stack chains require every L2 block to begin with a deposit-type
/// tx whose calldata sets L1 block values on the L1Block predeploy.
/// In production this comes from the rollup node; for tests we force
/// the canonical encoding via `OpPayloadAttributes::transactions`.
///
/// The hex below is an ecotone-format deposit tx (selector `440a5e20`,
/// 160 payload bytes). The L1-info parser dispatches by selector — not
/// by active hardfork — so ecotone-format is accepted even on chains
/// with later forks active. Same bytes as
/// `crates/arkiv-node/src/evm.rs::ArkivLocalPayloadAttributesBuilder`
/// for parity with the production `--dev` mining path.
const L1_INFO_DEPOSIT_TX: [u8; 251] = hex!(
    "7ef8f8a0683079df94aa5b9cf86687d739a60a9b4f0835e520ec4d664e2e415dca17a6df94deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e200000146b000f79c500000000000000040000000066d052e700000000013ad8a3000000000000000000000000000000000000000000000000000000003ef1278700000000000000000000000000000000000000000000000000000000000000012fdf87b89884a61e74b322bbcf60386f543bfae7827725efaaf0ab1de2294a590000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985"
);

/// Payload attrs that pin the L1-info deposit tx as transaction[0] of
/// every payload. Required because op-reth's standard
/// `optimism_payload_attributes` returns `transactions: None`, leaving
/// the payload builder with nothing to satisfy the "first tx must be
/// L1-info deposit" invariant.
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
