//! Continuous load generator for the EntityRegistry contract.
//!
//! Maintains an in-memory pool of "alive" entities and submits a weighted
//! random mix of CREATE/UPDATE/EXTEND/TRANSFER/DELETE operations against
//! a running node. Past-expiry entities are picked up by EXPIRE
//! preferentially, so the alive pool stays bounded and EXPIRE coverage
//! happens naturally.
//!
//! Sequential execution: one transaction in flight at a time, globally.
//! Multi-signer: rotates through up to [`arkiv_genesis::ARKIV_DEV_ACCOUNT_COUNT`]
//! mnemonic-derived keys, all of which are pre-funded by the dev chainspec.

use alloy_network::{EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, FixedBytes};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolEvent};
use arkiv_bindings::{
    Attribute, IEntityRegistry::EntityOperation, Mime128, OP_CREATE, OP_DELETE, OP_EXPIRE,
    OP_EXTEND, OP_TRANSFER, OP_UPDATE, Operation,
    IEntityRegistry::{self},
};
use arkiv_genesis::dev_signers;
use clap::Args;
use eyre::{Result, bail};
use rand::{Rng, SeedableRng, seq::IndexedRandom};
use rand_chacha::ChaCha8Rng;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::time::sleep;

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct SimulateArgs {
    /// Operations per second.
    #[arg(long, default_value_t = 0.5)]
    pub rate: f64,

    /// Total runtime; "0" or "infinity" means run until Ctrl-C.
    #[arg(long, default_value = "0", value_parser = parse_duration_or_zero)]
    pub duration: Duration,

    /// Number of mnemonic-derived signers to rotate through. Capped at
    /// `arkiv_genesis::ARKIV_DEV_ACCOUNT_COUNT`.
    #[arg(long, default_value_t = 10)]
    pub signer_count: usize,

    /// Op weights, e.g. "create=4,update=3,extend=2,transfer=1,delete=1".
    /// EXPIRE is event-driven (fires when an entity passes expiry) and
    /// has no weight.
    #[arg(long, default_value = "create=4,update=3,extend=2,transfer=1,delete=1")]
    pub weights: String,

    /// Cap on simultaneously-tracked alive entities. CREATE is throttled
    /// when this is reached.
    #[arg(long, default_value_t = 1000)]
    pub max_alive: usize,

    /// Status report interval.
    #[arg(long, default_value = "10s", value_parser = humantime::parse_duration)]
    pub status_interval: Duration,

    /// Deterministic RNG seed. Omit for non-reproducible runs.
    #[arg(long)]
    pub seed: Option<u64>,
}

/// Parse a duration string, treating "0" / "infinity" / "inf" as a sentinel
/// for "unbounded" — represented by [`Duration::ZERO`] in the parsed value.
fn parse_duration_or_zero(s: &str) -> std::result::Result<Duration, String> {
    if s == "0" || s.eq_ignore_ascii_case("infinity") || s.eq_ignore_ascii_case("inf") {
        Ok(Duration::ZERO)
    } else {
        humantime::parse_duration(s).map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Op kinds + weight parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
enum OpKind {
    Create,
    Update,
    Extend,
    Transfer,
    Delete,
    Expire,
}

impl OpKind {
    fn name(self) -> &'static str {
        match self {
            OpKind::Create => "create",
            OpKind::Update => "update",
            OpKind::Extend => "extend",
            OpKind::Transfer => "transfer",
            OpKind::Delete => "delete",
            OpKind::Expire => "expire",
        }
    }

    fn from_name(s: &str) -> Result<Self> {
        Ok(match s {
            "create" => OpKind::Create,
            "update" => OpKind::Update,
            "extend" => OpKind::Extend,
            "transfer" => OpKind::Transfer,
            "delete" => OpKind::Delete,
            other => bail!("unknown op kind: {}", other),
        })
    }
}

fn parse_weights(s: &str) -> Result<Vec<(OpKind, u32)>> {
    let mut out = Vec::new();
    for entry in s.split(',') {
        let (k, v) = entry
            .split_once('=')
            .ok_or_else(|| eyre::eyre!("expected key=value, got '{}'", entry))?;
        let kind = OpKind::from_name(k.trim())?;
        let weight: u32 = v
            .trim()
            .parse()
            .map_err(|e| eyre::eyre!("invalid weight '{}': {}", v, e))?;
        out.push((kind, weight));
    }
    if out.iter().all(|(_, w)| *w == 0) {
        bail!("all op weights are zero");
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct AliveEntity {
    owner_idx: usize,
    expires_at: u32,
}

#[derive(Default)]
struct State {
    alive: HashMap<B256, AliveEntity>,
    expired: Vec<B256>,
    counts: HashMap<OpKind, Counters>,
}

#[derive(Default, Debug, Clone, Copy)]
struct Counters {
    sent: u64,
    confirmed: u64,
    failed: u64,
}

impl State {
    fn record_sent(&mut self, op: OpKind) {
        self.counts.entry(op).or_default().sent += 1;
    }
    fn record_confirmed(&mut self, op: OpKind) {
        self.counts.entry(op).or_default().confirmed += 1;
    }
    fn record_failed(&mut self, op: OpKind) {
        self.counts.entry(op).or_default().failed += 1;
    }

    /// Move alive entries past `current_block` into the expired queue.
    fn promote_expired(&mut self, current_block: u32) {
        let mut to_expire = Vec::new();
        for (k, e) in &self.alive {
            if e.expires_at <= current_block {
                to_expire.push(*k);
            }
        }
        for k in to_expire {
            self.alive.remove(&k);
            self.expired.push(k);
        }
    }
}

// ---------------------------------------------------------------------------
// Op selection
// ---------------------------------------------------------------------------

/// Decide which op to perform next.
///
/// Priority: if there's anything in `expired`, fire EXPIRE on it (no
/// weight roll). Otherwise weighted random among feasible ops.
fn pick_op(
    state: &State,
    weights: &[(OpKind, u32)],
    rng: &mut ChaCha8Rng,
    max_alive: usize,
    signer_count: usize,
) -> Option<OpKind> {
    if !state.expired.is_empty() {
        return Some(OpKind::Expire);
    }

    // Filter weights to feasible ops.
    let feasible: Vec<(OpKind, u32)> = weights
        .iter()
        .copied()
        .filter(|(k, w)| *w > 0 && is_feasible(*k, state, max_alive, signer_count))
        .collect();
    if feasible.is_empty() {
        return None;
    }

    let total: u32 = feasible.iter().map(|(_, w)| *w).sum();
    let mut roll = rng.random_range(0..total);
    for (kind, weight) in feasible {
        if roll < weight {
            return Some(kind);
        }
        roll -= weight;
    }
    None
}

fn is_feasible(kind: OpKind, state: &State, max_alive: usize, signer_count: usize) -> bool {
    match kind {
        OpKind::Create => state.alive.len() < max_alive,
        OpKind::Update | OpKind::Extend | OpKind::Delete => !state.alive.is_empty(),
        OpKind::Transfer => !state.alive.is_empty() && signer_count >= 2,
        OpKind::Expire => !state.expired.is_empty(),
    }
}

// ---------------------------------------------------------------------------
// Op construction
// ---------------------------------------------------------------------------

fn random_payload(rng: &mut ChaCha8Rng, size: usize) -> Bytes {
    let mut buf = vec![0u8; size];
    rng.fill(&mut buf[..]);
    Bytes::from(buf)
}

fn random_content_type(rng: &mut ChaCha8Rng) -> Mime128 {
    const TYPES: &[&str] = &[
        "application/json",
        "application/octet-stream",
        "text/plain",
        "image/png",
        "image/jpeg",
    ];
    let s = TYPES.choose(rng).copied().unwrap_or(TYPES[0]);
    let bytes = s.as_bytes();
    let mut data = [FixedBytes::ZERO; 4];
    for (i, chunk) in bytes.chunks(32).enumerate().take(4) {
        let mut buf = [0u8; 32];
        buf[..chunk.len()].copy_from_slice(chunk);
        data[i] = FixedBytes::from(buf);
    }
    Mime128 { data }
}

/// Pick an owner-controlled alive entity at random.
fn pick_alive<'a>(state: &'a State, rng: &mut ChaCha8Rng) -> Option<(&'a B256, &'a AliveEntity)> {
    let keys: Vec<&B256> = state.alive.keys().collect();
    let key = keys.choose(rng)?;
    state.alive.get_key_value(*key)
}

// ---------------------------------------------------------------------------
// Submission
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct SubmitOutcome {
    success: bool,
}

#[allow(clippy::too_many_arguments)]
async fn submit_op<P: Provider>(
    kind: OpKind,
    state: &mut State,
    rng: &mut ChaCha8Rng,
    provider: &P,
    registry: Address,
    signers: &[PrivateKeySigner],
    gas_price: u128,
    current_block: u64,
    block_time: Duration,
) -> Result<SubmitOutcome> {
    // Build the operation + select the signer that should send it.
    let (op, sender_idx, post_op_state): (Operation, usize, PostOpState) = match kind {
        OpKind::Create => {
            let sender_idx = rng.random_range(0..signers.len());
            // Random expiry: 30–300 blocks ahead.
            let lifespan = rng.random_range(30u64..300);
            let expires_at = (current_block + lifespan) as u32;
            let size = rng.random_range(64..512);
            let payload = random_payload(rng, size);
            (
                Operation {
                    operationType: OP_CREATE,
                    entityKey: B256::ZERO,
                    payload,
                    contentType: random_content_type(rng),
                    attributes: Vec::<Attribute>::new(),
                    expiresAt: expires_at,
                    newOwner: Address::ZERO,
                },
                sender_idx,
                PostOpState::ExpectCreate { expires_at },
            )
        }
        OpKind::Update => {
            let (key, entity) = pick_alive(state, rng).map(|(k, e)| (*k, e.clone())).unwrap();
            (
                Operation {
                    operationType: OP_UPDATE,
                    entityKey: key,
                    payload: {
                        let size = rng.random_range(64..512);
                        random_payload(rng, size)
                    },
                    contentType: random_content_type(rng),
                    ..Default::default()
                },
                entity.owner_idx,
                PostOpState::Noop,
            )
        }
        OpKind::Extend => {
            let (key, entity) = pick_alive(state, rng).map(|(k, e)| (*k, e.clone())).unwrap();
            // New expiry strictly later than current.
            let bump = rng.random_range(50u64..400);
            let new_expires_at = (current_block + bump).max(entity.expires_at as u64 + 1) as u32;
            (
                Operation {
                    operationType: OP_EXTEND,
                    entityKey: key,
                    expiresAt: new_expires_at,
                    ..Default::default()
                },
                entity.owner_idx,
                PostOpState::Extend {
                    key,
                    expires_at: new_expires_at,
                },
            )
        }
        OpKind::Transfer => {
            let (key, entity) = pick_alive(state, rng).map(|(k, e)| (*k, e.clone())).unwrap();
            // Pick a different signer.
            let mut new_idx = rng.random_range(0..signers.len());
            if new_idx == entity.owner_idx {
                new_idx = (new_idx + 1) % signers.len();
            }
            (
                Operation {
                    operationType: OP_TRANSFER,
                    entityKey: key,
                    newOwner: signers[new_idx].address(),
                    ..Default::default()
                },
                entity.owner_idx,
                PostOpState::Transfer {
                    key,
                    new_owner_idx: new_idx,
                },
            )
        }
        OpKind::Delete => {
            let (key, entity) = pick_alive(state, rng).map(|(k, e)| (*k, e.clone())).unwrap();
            (
                Operation {
                    operationType: OP_DELETE,
                    entityKey: key,
                    ..Default::default()
                },
                entity.owner_idx,
                PostOpState::Remove { key },
            )
        }
        OpKind::Expire => {
            let key = state.expired.pop().unwrap();
            // Anyone can call expire — pick any signer.
            let sender_idx = rng.random_range(0..signers.len());
            (
                Operation {
                    operationType: OP_EXPIRE,
                    entityKey: key,
                    ..Default::default()
                },
                sender_idx,
                PostOpState::Remove { key },
            )
        }
    };

    // Encode + send. We build the transaction request manually so we can
    // use the multi-signer wallet's `from` selection.
    let calldata = IEntityRegistry::executeCall { ops: vec![op] }.abi_encode();
    let tx = TransactionRequest::default()
        .with_from(signers[sender_idx].address())
        .with_to(registry)
        .with_input(calldata)
        .with_gas_price(gas_price);

    state.record_sent(kind);

    let pending = match provider.send_transaction(tx).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(op = kind.name(), error = %e, "send failed");
            return Ok(SubmitOutcome {
                success: false,
            });
        }
    };
    let receipt = match pending.get_receipt().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(op = kind.name(), error = %e, "receipt wait failed");
            return Ok(SubmitOutcome {
                success: false,
            });
        }
    };
    if !receipt.status() {
        tracing::warn!(op = kind.name(), tx = %receipt.transaction_hash, "tx reverted");
        return Ok(SubmitOutcome {
            success: false,
        });
    }

    // Apply the post-op state transition on success.
    match post_op_state {
        PostOpState::Noop => {}
        PostOpState::ExpectCreate { expires_at } => {
            // Find the EntityOperation event to recover the new key.
            let mut found = None;
            for log in receipt.inner.logs() {
                if let Ok(ev) = EntityOperation::decode_log(&log.inner)
                    && ev.data.operationType == OP_CREATE
                {
                    found = Some(ev.data.entityKey);
                    break;
                }
            }
            if let Some(key) = found {
                state.alive.insert(
                    key,
                    AliveEntity {
                        owner_idx: sender_idx,
                        expires_at,
                    },
                );
            } else {
                tracing::warn!("CREATE confirmed but no EntityOperation log found");
            }
        }
        PostOpState::Extend { key, expires_at } => {
            if let Some(e) = state.alive.get_mut(&key) {
                e.expires_at = expires_at;
            }
        }
        PostOpState::Transfer { key, new_owner_idx } => {
            if let Some(e) = state.alive.get_mut(&key) {
                e.owner_idx = new_owner_idx;
            }
        }
        PostOpState::Remove { key } => {
            state.alive.remove(&key);
        }
    }

    state.record_confirmed(kind);
    let _ = block_time; // currently unused but reserved for future pacing
    Ok(SubmitOutcome { success: true })
}

#[derive(Debug)]
enum PostOpState {
    Noop,
    ExpectCreate { expires_at: u32 },
    Extend { key: B256, expires_at: u32 },
    Transfer { key: B256, new_owner_idx: usize },
    Remove { key: B256 },
}

// ---------------------------------------------------------------------------
// Status reporting
// ---------------------------------------------------------------------------

fn print_status(state: &State, started: Instant, current_block: u64, header: &str) {
    let elapsed = started.elapsed();
    println!("{header}  elapsed={elapsed:?}  block={current_block}  alive={}  expired_queue={}",
        state.alive.len(),
        state.expired.len(),
    );
    let order = [
        OpKind::Create,
        OpKind::Update,
        OpKind::Extend,
        OpKind::Transfer,
        OpKind::Delete,
        OpKind::Expire,
    ];
    for kind in order {
        let c = state.counts.get(&kind).copied().unwrap_or_default();
        if c.sent == 0 {
            continue;
        }
        println!(
            "  {:<8} sent={:<5} confirmed={:<5} failed={}",
            kind.name(),
            c.sent,
            c.confirmed,
            c.failed
        );
    }
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn run(
    args: SimulateArgs,
    rpc_url: &str,
    registry: Address,
    gas_price: u128,
    block_time: Duration,
) -> Result<()> {
    if args.rate <= 0.0 {
        bail!("--rate must be positive");
    }

    let signers = dev_signers(args.signer_count)?;
    let weights = parse_weights(&args.weights)?;
    let mut rng = match args.seed {
        Some(s) => ChaCha8Rng::seed_from_u64(s),
        None => ChaCha8Rng::from_seed(rand::random()),
    };

    // Multi-signer wallet: register every signer; per-tx selection via `.from(addr)`.
    let mut wallet = EthereumWallet::from(signers[0].clone());
    for s in &signers[1..] {
        wallet.register_signer(s.clone());
    }
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(rpc_url.parse()?);

    let mut state = State::default();
    let started = Instant::now();
    let mut last_status = started;
    let tick = Duration::from_secs_f64(1.0 / args.rate);

    println!(
        "simulate: rate={}/s signers={} weights=[{}] max_alive={} duration={} seed={}",
        args.rate,
        signers.len(),
        args.weights,
        args.max_alive,
        if args.duration.is_zero() {
            "unbounded".to_string()
        } else {
            format!("{:?}", args.duration)
        },
        args.seed
            .map(|s| s.to_string())
            .unwrap_or_else(|| "random".into()),
    );

    let mut interrupted = false;

    loop {
        if !args.duration.is_zero() && started.elapsed() >= args.duration {
            break;
        }

        let current_block = provider.get_block_number().await.unwrap_or(0);
        state.promote_expired(current_block as u32);

        let Some(kind) = pick_op(&state, &weights, &mut rng, args.max_alive, signers.len()) else {
            // No feasible op (e.g. nothing alive yet, weights all skipped).
            // Wait one tick and retry — a CREATE will become feasible
            // again on the next pass.
            sleep(tick).await;
            continue;
        };

        let result = submit_op(
            kind,
            &mut state,
            &mut rng,
            &provider,
            registry,
            &signers,
            gas_price,
            current_block,
            block_time,
        )
        .await;

        match result {
            Ok(outcome) => {
                if !outcome.success {
                    state.record_failed(kind);
                }
            }
            Err(e) => {
                state.record_failed(kind);
                tracing::error!(op = kind.name(), error = %e, "op failed unexpectedly");
            }
        }

        if last_status.elapsed() >= args.status_interval {
            print_status(&state, started, current_block, "[status]");
            last_status = Instant::now();
        }

        // Cooperative cancellation point + pacing.
        tokio::select! {
            _ = sleep(tick) => {},
            _ = tokio::signal::ctrl_c() => {
                interrupted = true;
                break;
            }
        }
    }

    let final_block = provider.get_block_number().await.unwrap_or(0);
    let header = if interrupted { "[final, interrupted]" } else { "[final]" };
    print_status(&state, started, final_block, header);
    Ok(())
}
