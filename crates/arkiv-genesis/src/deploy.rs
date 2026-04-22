//! Revm-based contract deployment for genesis bytecode extraction.

use alloy_primitives::{Address, Bytes, U256};
use eyre::{bail, ensure, Result};
use revm::{
    context::{Context, TxEnv},
    database::{CacheDB, EmptyDB},
    handler::{ExecuteEvm, MainBuilder},
    primitives::TxKind,
    state::AccountInfo,
    MainContext,
};

const DEPLOYER: Address = Address::new([
    0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
]);

/// Execute creation bytecode in revm and return the runtime bytecode.
///
/// The EVM is configured with `block.number = 0` and the given `chain_id`
/// so that constructor logic reading `block.chainid` or `block.number`
/// produces the correct values for genesis.
pub fn deploy(creation_code: &[u8], chain_id: u64) -> Result<Bytes> {
    let mut db = CacheDB::<EmptyDB>::default();
    db.insert_account_info(
        DEPLOYER,
        AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000u128),
            nonce: 0,
            ..Default::default()
        },
    );

    let ctx = Context::mainnet()
        .with_db(db)
        .modify_cfg_chained(|cfg| {
            cfg.chain_id = chain_id;
        })
        .modify_block_chained(|block| {
            block.number = U256::ZERO;
            block.gas_limit = 30_000_000;
        });

    let mut evm = ctx.build_mainnet();

    let tx = TxEnv::builder()
        .caller(DEPLOYER)
        .kind(TxKind::Create)
        .data(Bytes::copy_from_slice(creation_code))
        .gas_limit(15_000_000)
        .gas_price(0)
        .nonce(0)
        .chain_id(Some(chain_id))
        .build()
        .map_err(|e| eyre::eyre!("failed to build tx: {:?}", e))?;

    let result = evm
        .transact(tx)
        .map_err(|e| eyre::eyre!("revm execution failed: {:?}", e))?;

    ensure!(
        result.result.is_success(),
        "contract deployment failed: {:?}",
        result.result
    );

    let deployed_addr = result
        .result
        .created_address()
        .ok_or_else(|| eyre::eyre!("contract creation did not return an address"))?;

    let account = result
        .state
        .get(&deployed_addr)
        .ok_or_else(|| eyre::eyre!("deployed account not found in state"))?;

    let bytecode = account
        .info
        .code
        .as_ref()
        .ok_or_else(|| eyre::eyre!("no code on deployed account"))?;

    let runtime_bytes = bytecode.bytes();
    if runtime_bytes.is_empty() {
        bail!("deployed contract has empty bytecode");
    }

    Ok(runtime_bytes.clone())
}
