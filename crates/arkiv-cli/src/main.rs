mod simulate;

use alloy_network::EthereumWallet;
use alloy_primitives::{Address, B256, Bytes, FixedBytes, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types::eth::Log as RpcLog;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolEvent;
use arkiv_bindings::types::{Ident32, Mime128Str, op_type_name};
use arkiv_bindings::{IEntityRegistry::EntityOperation, *};
use clap::{Parser, Subcommand};
use eyre::{Result, bail};
use rand::Rng;
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// CLI for submitting EntityRegistry operations.
#[derive(Parser)]
#[command(name = "arkiv-cli")]
struct Cli {
    /// RPC endpoint URL.
    #[arg(long, default_value = "http://localhost:8545")]
    rpc_url: String,

    /// Private key for signing transactions (hex, with or without 0x prefix).
    /// Defaults to the first test mnemonic account.
    #[arg(
        long,
        default_value = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
    )]
    private_key: String,

    /// EntityRegistry contract address.
    #[arg(long, default_value = "0x4400000000000000000000000000000000000044")]
    registry: Address,

    /// Assumed block time for duration-to-block conversion (e.g. "2s").
    #[arg(long, default_value = "2s", value_parser = humantime::parse_duration)]
    block_time: Duration,

    /// Gas price in wei. OP dev nodes require an explicit gas price.
    #[arg(long, default_value = "1000000000")]
    gas_price: u128,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create an entity with a random payload.
    Create {
        /// Content type MIME string.
        #[arg(long, default_value = "application/octet-stream")]
        content_type: String,

        /// Payload size in bytes (random data).
        #[arg(long, default_value = "256")]
        size: usize,

        /// How long until the entity expires (e.g. "1h", "30m", "7d").
        #[arg(long, default_value = "1h", value_parser = humantime::parse_duration)]
        expires_in: Duration,
    },

    /// Update an existing entity with a new random payload.
    Update {
        /// Entity key to update.
        #[arg(long)]
        key: B256,

        /// Content type MIME string.
        #[arg(long, default_value = "application/octet-stream")]
        content_type: String,

        /// Payload size in bytes (random data).
        #[arg(long, default_value = "256")]
        size: usize,
    },

    /// Extend an entity's expiration.
    Extend {
        /// Entity key to extend.
        #[arg(long)]
        key: B256,

        /// How long from now until expiry (e.g. "2h", "1d").
        #[arg(long, value_parser = humantime::parse_duration)]
        expires_in: Duration,
    },

    /// Transfer entity ownership.
    Transfer {
        /// Entity key to transfer.
        #[arg(long)]
        key: B256,

        /// New owner address.
        #[arg(long)]
        new_owner: Address,
    },

    /// Delete an entity.
    Delete {
        /// Entity key to delete.
        #[arg(long)]
        key: B256,
    },

    /// Expire an entity (must be past its expiration block).
    Expire {
        /// Entity key to expire.
        #[arg(long)]
        key: B256,
    },

    /// Query an entity's on-chain commitment.
    Query {
        /// Entity key to query.
        #[arg(long)]
        key: B256,
    },

    /// Read the current changeset hash.
    Hash,

    /// Walk the changeset hash chain from head to genesis.
    History {
        /// Maximum number of operations to display (default: all).
        #[arg(long)]
        depth: Option<u32>,
    },

    /// Check an account's ETH balance.
    Balance {
        /// Address to check. Defaults to the signer's address.
        #[arg(long)]
        address: Option<Address>,
    },

    /// Submit a batch of operations from a JSON file in a single tx.
    /// See `scripts/fixtures/` for examples.
    Batch {
        /// Path to a JSON file containing an array of operations.
        file: PathBuf,
    },

    /// Splice the EntityRegistry predeploy into a geth-format genesis JSON.
    ///
    /// Reads `chainId` from the input, runs the contract creation bytecode
    /// against that chain ID (so the EIP-712 cached domain separator
    /// matches), and inserts the resulting runtime bytecode at the canonical
    /// predeploy address. Designed for post-processing op-deployer output.
    InjectPredeploy {
        /// Input genesis JSON (geth format).
        file: PathBuf,

        /// Output path. Defaults to overwriting the input.
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Fire off multiple entity creates.
    Spam {
        /// Number of entities to create.
        #[arg(long, default_value = "10")]
        count: u32,

        /// Payload size in bytes per entity.
        #[arg(long, default_value = "256")]
        size: usize,

        /// How long until entities expire (e.g. "1h", "7d").
        #[arg(long, default_value = "1h", value_parser = humantime::parse_duration)]
        expires_in: Duration,
    },

    /// Continuously generate a weighted mix of entity operations against
    /// a running node, simulating live system traffic.
    Simulate(simulate::SimulateArgs),
}

/// Validate and pack a MIME type string into the contract's `Mime128`
/// (`bytes32[4]`) representation. Validation per RFC 2045 (lowercase only,
/// `type/subtype[; param=value]*`) — see `arkiv_bindings::types::Mime128Str`.
fn encode_mime128(mime: &str) -> Result<Mime128> {
    let data = Mime128Str::encode(mime)
        .map_err(|e| eyre::eyre!("invalid content-type '{}': {}", mime, e))?
        .to_bytes32x4();
    Ok(Mime128 { data })
}

fn random_payload(size: usize) -> Bytes {
    let mut rng = rand::rng();
    let mut buf = vec![0u8; size];
    rng.fill(&mut buf[..]);
    Bytes::from(buf)
}

/// Convert a duration from now into an absolute block number.
async fn expiry_block(
    provider: &impl Provider,
    duration: Duration,
    block_time: Duration,
) -> Result<u32> {
    let current = provider.get_block_number().await?;
    let blocks = duration.as_secs() / block_time.as_secs().max(1);
    Ok((current + blocks) as u32)
}

fn print_events(logs: &[RpcLog]) {
    for log in logs {
        if let Ok(event) = EntityOperation::decode_log(&log.inner) {
            let e = event.data;
            println!("---");
            println!("  op:          {}", op_type_name(e.operationType));
            println!("  entity_key:  {}", e.entityKey);
            println!("  owner:       {}", e.owner);
            println!("  expires_at:  {}", e.expiresAt);
            println!("  entity_hash: {}", e.entityHash);
        }
    }
}

// ---------------------------------------------------------------------------
// Batch JSON schema
// ---------------------------------------------------------------------------

/// An entity-key field in a batch op. Either a hex literal (`"0x..."`) or a
/// reference (`"$N"`) to the Nth op in the batch (which must be a CREATE).
#[derive(Debug, Clone)]
enum EntityKeyRef {
    Literal(B256),
    Ref(usize),
}

impl<'de> Deserialize<'de> for EntityKeyRef {
    fn deserialize<D: Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        if let Some(rest) = s.strip_prefix('$') {
            let idx: usize = rest.parse().map_err(serde::de::Error::custom)?;
            Ok(EntityKeyRef::Ref(idx))
        } else {
            let key = s.parse::<B256>().map_err(serde::de::Error::custom)?;
            Ok(EntityKeyRef::Literal(key))
        }
    }
}

fn de_humantime<'de, D: Deserializer<'de>>(de: D) -> std::result::Result<Duration, D::Error> {
    let s = String::deserialize(de)?;
    humantime::parse_duration(&s).map_err(serde::de::Error::custom)
}

fn default_content_type() -> String {
    "application/octet-stream".to_string()
}

/// One attribute in a batch JSON op. The value type is discriminated by
/// which of `string` / `uint` / `entityKey` is present (untagged enum).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BatchAttribute {
    /// `Ident32` name (lowercase ASCII, validated client-side).
    name: String,
    #[serde(flatten)]
    value: BatchAttributeValue,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BatchAttributeValue {
    String {
        string: String,
    },
    Uint {
        uint: U256,
    },
    EntityKey {
        #[serde(rename = "entityKey")]
        entity_key: EntityKeyRef,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum BatchOp {
    Create {
        #[serde(default = "default_content_type", rename = "contentType")]
        content_type: String,
        /// Optional payload string. If prefixed with `0x` decoded as hex,
        /// otherwise treated as raw UTF-8 bytes. Mutually exclusive with `size`.
        payload: Option<String>,
        /// Random payload size in bytes. Mutually exclusive with `payload`.
        size: Option<usize>,
        #[serde(deserialize_with = "de_humantime", rename = "expiresIn")]
        expires_in: Duration,
        #[serde(default)]
        attributes: Vec<BatchAttribute>,
    },
    Update {
        #[serde(rename = "entityKey")]
        entity_key: EntityKeyRef,
        #[serde(default = "default_content_type", rename = "contentType")]
        content_type: String,
        payload: Option<String>,
        size: Option<usize>,
        #[serde(default)]
        attributes: Vec<BatchAttribute>,
    },
    Extend {
        #[serde(rename = "entityKey")]
        entity_key: EntityKeyRef,
        #[serde(deserialize_with = "de_humantime", rename = "expiresIn")]
        expires_in: Duration,
    },
    Transfer {
        #[serde(rename = "entityKey")]
        entity_key: EntityKeyRef,
        #[serde(rename = "newOwner")]
        new_owner: Address,
    },
    Delete {
        #[serde(rename = "entityKey")]
        entity_key: EntityKeyRef,
    },
    Expire {
        #[serde(rename = "entityKey")]
        entity_key: EntityKeyRef,
    },
}

/// Build a sol `Attribute` from a batch entry, validating the Ident32 name
/// and packing the value per the contract's `bytes32[4]` encoding rules
/// (see `arkiv-contracts/docs/value128-encoding.md`).
fn build_attribute(
    attr: &BatchAttribute,
    resolve: &impl Fn(&EntityKeyRef) -> Result<B256>,
) -> Result<Attribute> {
    let name = Ident32::encode(&attr.name)
        .map_err(|e| eyre::eyre!("invalid attribute name '{}': {}", attr.name, e))?
        .as_b256();

    let (value_type, value) = match &attr.value {
        BatchAttributeValue::Uint { uint } => {
            let mut v = [FixedBytes::ZERO; 4];
            v[0] = FixedBytes::from(uint.to_be_bytes::<32>());
            (ATTR_UINT, v)
        }
        BatchAttributeValue::String { string } => {
            let bytes = string.as_bytes();
            if bytes.len() > 128 {
                bail!(
                    "attribute '{}' string value exceeds 128 bytes ({})",
                    attr.name,
                    bytes.len()
                );
            }
            let mut v = [FixedBytes::ZERO; 4];
            for (i, chunk) in bytes.chunks(32).enumerate() {
                let mut buf = [0u8; 32];
                buf[..chunk.len()].copy_from_slice(chunk);
                v[i] = FixedBytes::from(buf);
            }
            (ATTR_STRING, v)
        }
        BatchAttributeValue::EntityKey { entity_key } => {
            let key = resolve(entity_key)?;
            let mut v = [FixedBytes::ZERO; 4];
            v[0] = FixedBytes::from(key.0);
            (ATTR_ENTITY_KEY, v)
        }
    };

    Ok(Attribute {
        name,
        valueType: value_type,
        value,
    })
}

/// Build the contract's `Attribute[]` from batch entries, sorted by name
/// ascending as the contract requires for deterministic hashing.
fn build_attributes(
    attrs: &[BatchAttribute],
    resolve: &impl Fn(&EntityKeyRef) -> Result<B256>,
) -> Result<Vec<Attribute>> {
    let mut out: Vec<Attribute> = attrs
        .iter()
        .map(|a| build_attribute(a, resolve))
        .collect::<Result<_>>()?;
    out.sort_by_key(|a| a.name);
    Ok(out)
}

/// Resolve `payload`/`size` fields into raw bytes.
fn resolve_payload(payload: Option<&str>, size: Option<usize>) -> Result<Bytes> {
    match (payload, size) {
        (Some(_), Some(_)) => bail!("payload and size are mutually exclusive"),
        (Some(s), None) => {
            if let Some(hex) = s.strip_prefix("0x") {
                Ok(Bytes::from(hex::decode(hex)?))
            } else {
                Ok(Bytes::from(s.as_bytes().to_vec()))
            }
        }
        (None, Some(n)) => Ok(random_payload(n)),
        (None, None) => Ok(Bytes::new()),
    }
}

/// Splice the Arkiv predeploy and prefunded dev accounts into a geth-format
/// genesis JSON.
///
/// Reads `chainId` from the input's `config`, then merges
/// [`arkiv_genesis::genesis_alloc`] into the alloc:
///   - the EntityRegistry predeploy at the canonical address (runtime
///     bytecode generated for this chain ID so the EIP-712 cached domain
///     separator matches),
///   - the [`arkiv_genesis::ARKIV_DEV_ACCOUNT_COUNT`] mnemonic-derived
///     dev accounts, each prefunded with [`arkiv_genesis::arkiv_dev_balance_wei`].
///
/// Output is pretty-printed back to disk (overwriting the input by
/// default, or to `out` if specified).
fn inject_predeploy(input: &std::path::Path, out: Option<&std::path::Path>) -> Result<()> {
    use arkiv_genesis::{ENTITY_REGISTRY_ADDRESS, genesis_alloc};

    let raw = std::fs::read_to_string(input)
        .map_err(|e| eyre::eyre!("failed to read {}: {}", input.display(), e))?;
    let mut genesis: arkiv_genesis::Genesis = serde_json::from_str(&raw)
        .map_err(|e| eyre::eyre!("failed to parse {} as genesis JSON: {}", input.display(), e))?;

    let chain_id = genesis.config.chain_id;
    if chain_id == 0 {
        bail!("genesis config.chainId is zero — refusing to inject");
    }

    if genesis.alloc.contains_key(&ENTITY_REGISTRY_ADDRESS) {
        eprintln!(
            "warning: alloc already contains an entry at {ENTITY_REGISTRY_ADDRESS}; overwriting",
        );
    }

    let arkiv_alloc = genesis_alloc(chain_id)?;
    let account_count = arkiv_alloc.len();
    for (addr, account) in arkiv_alloc {
        genesis.alloc.insert(addr, account);
    }

    let dest = out.unwrap_or(input);
    let serialized = serde_json::to_string_pretty(&genesis)?;
    std::fs::write(dest, serialized)
        .map_err(|e| eyre::eyre!("failed to write {}: {}", dest.display(), e))?;

    eprintln!(
        "injected EntityRegistry predeploy at {} + {} dev accounts (chainId={}) into {}",
        ENTITY_REGISTRY_ADDRESS,
        account_count - 1,
        chain_id,
        dest.display(),
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // `inject-predeploy` is a pure JSON munger — no network, no signer.
    // Handle it before any of the provider setup below.
    if let Command::InjectPredeploy { file, out } = &cli.command {
        return inject_predeploy(file, out.as_deref());
    }

    // `simulate` builds its own multi-signer provider; bypass the
    // single-signer setup below.
    if let Command::Simulate(args) = cli.command {
        return simulate::run(args, &cli.rpc_url, cli.registry, cli.gas_price, cli.block_time)
            .await;
    }

    let signer: PrivateKeySigner = cli.private_key.parse()?;
    let signer_address = signer.address();
    let wallet = EthereumWallet::from(signer);

    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(cli.rpc_url.parse()?);

    let registry = IEntityRegistry::new(cli.registry, &provider);

    match cli.command {
        Command::Create {
            content_type,
            size,
            expires_in,
        } => {
            let expires_at = expiry_block(&provider, expires_in, cli.block_time).await?;
            let op = Operation {
                operationType: OP_CREATE,
                entityKey: B256::ZERO,
                payload: random_payload(size),
                contentType: encode_mime128(&content_type)?,
                attributes: vec![],
                expiresAt: expires_at,
                newOwner: Address::ZERO,
            };

            let receipt = registry
                .execute(vec![op])
                .gas_price(cli.gas_price)
                .send()
                .await?
                .get_receipt()
                .await?;
            println!("tx: {}", receipt.transaction_hash);
            print_events(receipt.inner.logs());
        }

        Command::Update {
            key,
            content_type,
            size,
        } => {
            let op = Operation {
                operationType: OP_UPDATE,
                entityKey: key,
                payload: random_payload(size),
                contentType: encode_mime128(&content_type)?,
                attributes: vec![],
                ..Default::default()
            };

            let receipt = registry
                .execute(vec![op])
                .gas_price(cli.gas_price)
                .send()
                .await?
                .get_receipt()
                .await?;
            println!("tx: {}", receipt.transaction_hash);
            print_events(receipt.inner.logs());
        }

        Command::Extend { key, expires_in } => {
            let expires_at = expiry_block(&provider, expires_in, cli.block_time).await?;
            let op = Operation {
                operationType: OP_EXTEND,
                entityKey: key,
                expiresAt: expires_at,
                ..Default::default()
            };

            let receipt = registry
                .execute(vec![op])
                .gas_price(cli.gas_price)
                .send()
                .await?
                .get_receipt()
                .await?;
            println!("tx: {}", receipt.transaction_hash);
            print_events(receipt.inner.logs());
        }

        Command::Transfer { key, new_owner } => {
            let op = Operation {
                operationType: OP_TRANSFER,
                entityKey: key,
                newOwner: new_owner,
                ..Default::default()
            };

            let receipt = registry
                .execute(vec![op])
                .gas_price(cli.gas_price)
                .send()
                .await?
                .get_receipt()
                .await?;
            println!("tx: {}", receipt.transaction_hash);
            print_events(receipt.inner.logs());
        }

        Command::Delete { key } => {
            let op = Operation {
                operationType: OP_DELETE,
                entityKey: key,
                ..Default::default()
            };

            let receipt = registry
                .execute(vec![op])
                .gas_price(cli.gas_price)
                .send()
                .await?
                .get_receipt()
                .await?;
            println!("tx: {}", receipt.transaction_hash);
            print_events(receipt.inner.logs());
        }

        Command::Expire { key } => {
            let op = Operation {
                operationType: OP_EXPIRE,
                entityKey: key,
                ..Default::default()
            };

            let receipt = registry
                .execute(vec![op])
                .gas_price(cli.gas_price)
                .send()
                .await?
                .get_receipt()
                .await?;
            println!("tx: {}", receipt.transaction_hash);
            print_events(receipt.inner.logs());
        }

        Command::Query { key } => {
            let result = registry.commitment(key).call().await?;
            let c = result;
            println!("creator:    {}", c.creator);
            println!("owner:      {}", c.owner);
            println!("created_at: {}", c.createdAt);
            println!("updated_at: {}", c.updatedAt);
            println!("expires_at: {}", c.expiresAt);
            println!("core_hash:  {}", c.coreHash);
        }

        Command::Hash => {
            let hash = registry.changeSetHash().call().await?;
            println!("{hash}");
        }

        Command::History { depth } => {
            let head = registry.headBlock().call().await?;
            let genesis = registry.genesisBlock().call().await?;

            if head == genesis {
                let node = registry.getBlockNode(head).call().await?;
                if node.txCount == 0 {
                    println!("No operations recorded.");
                    return Ok(());
                }
            }

            // Collect blocks from head back to genesis
            let mut block_num = head;
            let mut blocks = Vec::new();
            loop {
                let node = registry.getBlockNode(block_num).call().await?;
                let prev = node.prevBlock;
                if node.txCount > 0 {
                    blocks.push((block_num, node));
                }
                if block_num == genesis || prev == 0 {
                    break;
                }
                block_num = prev;
            }

            // Print chronologically, respecting depth limit on ops
            blocks.reverse();
            let max_ops = depth.unwrap_or(u32::MAX);
            let mut op_count_total: u32 = 0;

            'outer: for (block_num, node) in &blocks {
                println!("block {}", block_num);
                for tx_seq in 0..node.txCount {
                    let op_count = registry.txOpCount(*block_num, tx_seq).call().await?;
                    println!("  tx {}", tx_seq);
                    for op_seq in 0..op_count {
                        if op_count_total >= max_ops {
                            break 'outer;
                        }
                        let hash = registry
                            .changeSetHashAtOp(*block_num, tx_seq, op_seq)
                            .call()
                            .await?;
                        println!("    op {} -> {}", op_seq, hash);
                        op_count_total += 1;
                    }
                }
            }
        }

        Command::InjectPredeploy { .. } => unreachable!("handled at top of main"),
        Command::Simulate(_) => unreachable!("handled at top of main"),

        Command::Batch { file } => {
            let json = std::fs::read_to_string(&file)?;
            let ops: Vec<BatchOp> = serde_json::from_str(&json)?;
            if ops.is_empty() {
                bail!("batch file contains no operations");
            }

            // Precompute $N -> entityKey for every CREATE in the batch, before
            // we send execute() (which would mutate the sender's nonce).
            let signer_nonce: u32 = registry.nonces(signer_address).call().await?;
            let mut refs: HashMap<usize, B256> = HashMap::new();
            let mut create_count: u32 = 0;
            for (i, op) in ops.iter().enumerate() {
                if matches!(op, BatchOp::Create { .. }) {
                    let k = registry
                        .entityKey(signer_address, signer_nonce + create_count)
                        .call()
                        .await?;
                    refs.insert(i, k);
                    create_count += 1;
                }
            }

            let resolve = |r: &EntityKeyRef| -> Result<B256> {
                match r {
                    EntityKeyRef::Literal(k) => Ok(*k),
                    EntityKeyRef::Ref(i) => refs.get(i).copied().ok_or_else(|| {
                        eyre::eyre!("${} does not refer to a CREATE op in this batch", i)
                    }),
                }
            };

            let mut sol_ops: Vec<Operation> = Vec::with_capacity(ops.len());
            for op in &ops {
                let sol_op = match op {
                    BatchOp::Create {
                        content_type,
                        payload,
                        size,
                        expires_in,
                        attributes,
                    } => {
                        let expires_at =
                            expiry_block(&provider, *expires_in, cli.block_time).await?;
                        Operation {
                            operationType: OP_CREATE,
                            entityKey: B256::ZERO,
                            payload: resolve_payload(payload.as_deref(), *size)?,
                            contentType: encode_mime128(content_type)?,
                            attributes: build_attributes(attributes, &resolve)?,
                            expiresAt: expires_at,
                            newOwner: Address::ZERO,
                        }
                    }
                    BatchOp::Update {
                        entity_key,
                        content_type,
                        payload,
                        size,
                        attributes,
                    } => Operation {
                        operationType: OP_UPDATE,
                        entityKey: resolve(entity_key)?,
                        payload: resolve_payload(payload.as_deref(), *size)?,
                        contentType: encode_mime128(content_type)?,
                        attributes: build_attributes(attributes, &resolve)?,
                        ..Default::default()
                    },
                    BatchOp::Extend {
                        entity_key,
                        expires_in,
                    } => {
                        let expires_at =
                            expiry_block(&provider, *expires_in, cli.block_time).await?;
                        Operation {
                            operationType: OP_EXTEND,
                            entityKey: resolve(entity_key)?,
                            expiresAt: expires_at,
                            ..Default::default()
                        }
                    }
                    BatchOp::Transfer {
                        entity_key,
                        new_owner,
                    } => Operation {
                        operationType: OP_TRANSFER,
                        entityKey: resolve(entity_key)?,
                        newOwner: *new_owner,
                        ..Default::default()
                    },
                    BatchOp::Delete { entity_key } => Operation {
                        operationType: OP_DELETE,
                        entityKey: resolve(entity_key)?,
                        ..Default::default()
                    },
                    BatchOp::Expire { entity_key } => Operation {
                        operationType: OP_EXPIRE,
                        entityKey: resolve(entity_key)?,
                        ..Default::default()
                    },
                };
                sol_ops.push(sol_op);
            }

            let receipt = registry
                .execute(sol_ops)
                .gas_price(cli.gas_price)
                .send()
                .await?
                .get_receipt()
                .await?;
            println!("tx: {}", receipt.transaction_hash);
            print_events(receipt.inner.logs());
        }

        Command::Spam {
            count,
            size,
            expires_in,
        } => {
            let expires_at = expiry_block(&provider, expires_in, cli.block_time).await?;
            let nonce_start = provider.get_transaction_count(signer_address).await?;

            // Fire all transactions, retrying on pool-full errors
            let mut pending = Vec::new();
            for i in 0..count {
                let nonce = nonce_start + i as u64;
                loop {
                    let op = Operation {
                        operationType: OP_CREATE,
                        entityKey: B256::ZERO,
                        payload: random_payload(size),
                        contentType: encode_mime128("application/octet-stream")?,
                        attributes: vec![],
                        expiresAt: expires_at,
                        newOwner: Address::ZERO,
                    };

                    match registry
                        .execute(vec![op])
                        .nonce(nonce)
                        .gas_price(cli.gas_price)
                        .send()
                        .await
                    {
                        Ok(p) => {
                            pending.push(p);
                            eprint!("\rsent {}/{}", i + 1, count);
                            break;
                        }
                        Err(e) if e.to_string().contains("txpool is full") => {
                            // Pool is full — wait for a block to drain it
                            tokio::time::sleep(cli.block_time).await;
                        }
                        Err(e) => {
                            eprintln!("\rsend failed at {}/{}: {}", i + 1, count, e);
                            break;
                        }
                    }
                }
            }
            eprintln!();

            // Wait for all receipts
            let mut success = 0u32;
            let mut failed = 0u32;
            let total = pending.len();
            for (i, p) in pending.into_iter().enumerate() {
                match p.get_receipt().await {
                    Ok(_) => success += 1,
                    Err(_) => failed += 1,
                }
                eprint!("\rconfirmed {}/{}", i + 1, total);
            }
            eprintln!();
            println!("{} ok, {} failed", success, failed);
        }

        Command::Balance { address } => {
            let addr = address.unwrap_or(signer_address);
            let balance = provider.get_balance(addr).await?;
            let eth = balance / U256::from(10u64).pow(U256::from(18));
            let remainder = balance % U256::from(10u64).pow(U256::from(18));
            println!("{addr}");
            println!("{eth}.{remainder:018} ETH");
        }
    }

    Ok(())
}
