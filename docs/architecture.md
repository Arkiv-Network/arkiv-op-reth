# Architecture

This document describes the design of the `arkiv-op-reth` workspace: what
each crate does, how they fit together, what flows through the system at
runtime, and why the boundaries are drawn the way they are.

For the JSON-RPC wire format the ExEx posts to EntityDB, see
[`exex-jsonrpc-interface-v2.md`](exex-jsonrpc-interface-v2.md). For the
contract itself see the [`arkiv-contracts`](https://github.com/Arkiv-Network/arkiv-contracts)
repo.

---

## 1. What this software does

### 1.1 The Arkiv chain

Arkiv is an OP-stack L2 with one additional standard predeploy:
`EntityRegistry`. The contract sits at the canonical address
`0x4400000000000000000000000000000000000044` and exposes six operations
on opaque entities — `CREATE`, `UPDATE`, `EXTEND`, `TRANSFER`, `DELETE`,
`EXPIRE` — submitted in batches via `execute(Operation[])`.

The `Operation` struct carries a `btl` (blocks-to-live) field for
`CREATE` and `EXTEND`. The contract computes the absolute expiry as
`currentBlock + btl` — callers supply a relative duration, not an
absolute block number. The stored `Commitment` continues to hold the
resolved absolute `expiresAt` and the `EntityOperation` event emits it,
so downstream consumers always see absolute block numbers.

BlockNumber fields throughout the contract are typed as `BlockNumber32`
(a `uint32` UDVT) to enable tight slot packing: three `BlockNumber32`
fields share a single 32-byte storage slot alongside a 20-byte address.

The contract stores a minimal commitment per entity (creator, owner,
expiry, content hash) and emits per-operation events. Full entity payloads
and attributes live in calldata, not on-chain storage. A rolling
`changeSetHash` — produced by chaining each operation's hash into the
previous head — is exposed via view functions and emitted with every
operation, giving downstream consumers a verifiable single-value witness
of the chain's entity state.

### 1.2 The off-chain DB (`arkiv-storage`)

`arkiv-storage` (run as the `arkiv-storaged` daemon, referred to as
EntityDB in the JSON-RPC wire format) is a Go service that consumes
the calldata-only entity stream and serves queries against it (by
entity key, by owner, by attribute, …). It's the canonical reader;
clients of the Arkiv chain talk to it, not directly to the contract.

### 1.3 The role of this repository

`arkiv-op-reth` is the bridge: an op-reth-derived execution node that
decodes EntityRegistry calldata + events from each canonical block and
forwards a structured representation to EntityDB. The forwarding lives
inside an Execution Extension (ExEx) — reth's mechanism for
post-execution side effects.

---

## 2. System overview

```
   ┌─────────────────────────────────────────────────────────────┐
   │ arkiv-node binary                                           │
   │                                                             │
   │  ┌───────────────────┐                                      │
   │  │   reth engine     │  ChainCommitted / ChainReorged       │
   │  │   (op-reth)       │  / ChainReverted notifications       │
   │  │                   │ ───────────────────────┐             │
   │  └─────────┬─────────┘                        │             │
   │            │ executes blocks                  ▼             │
   │            │                          ┌──────────────┐      │
   │            ▼                          │  Arkiv ExEx  │      │
   │     ┌─────────────┐                   │              │      │
   │     │  state DB   │ ◄── slot reads ── │  decode +    │      │
   │     │  (MDBX)     │   for rolling     │  forward     │      │
   │     └─────────────┘   changeset hash  └──────┬───────┘      │
   │                                              │              │
   └──────────────────────────────────────────────┼──────────────┘
                                                  │ Storage trait
                                  ┌───────────────┴───────────────┐
                                  │                               │
                                  ▼                               ▼
                          ┌──────────────┐               ┌────────────────┐
                          │ LoggingStore │               │ JsonRpcStore   │
                          │ (tracing)    │               │ (HTTP → Go DB) │
                          └──────────────┘               └────────────────┘
```

The binary's job is to turn reth's chain notifications into well-typed,
serialisable Arkiv operations and hand them off to whichever Storage
implementation is configured. Everything else (block production, p2p,
state storage, RPC) is unchanged op-reth.

---

## 3. Workspace crates

### 3.1 `arkiv-genesis`

A small library shared by both binaries. It owns:

- The canonical predeploy address constant (`ENTITY_REGISTRY_ADDRESS =
  0x44…0044`).
- The hardhat-compatible dev mnemonic (`ARKIV_DEV_MNEMONIC`),
  `DEV_ADDRESS` (the first account derived from it), and
  `ARKIV_DEV_ACCOUNT_COUNT` (currently `100`) — the number of accounts
  derived from the mnemonic and pre-funded in the dev alloc. The first
  20 indices match the well-known hardhat / foundry / anvil defaults.
- `arkiv_dev_balance_wei()` — per-account dev balance (10,000 ETH).
- `dev_signers(count)` / `dev_funding_alloc(count, balance_wei)` —
  derive `PrivateKeySigner`s and `(Address, GenesisAccount)` pairs for
  the first `count` mnemonic indices.
- `deploy_creation_code(chain_id) -> Result<Bytes>` — runs
  `arkiv_bindings::ENTITY_REGISTRY_CREATION_CODE` through revm at the
  given chain ID and returns the resulting runtime bytecode. The chain ID
  is forwarded to revm's `block.chainid` so the EIP-712 domain separator
  baked into immutables matches the target chain.
- `predeploy_account(chain_id) -> Result<GenesisAccount>` — convenience
  for splicing into a `Genesis.alloc`.
- `genesis_alloc(chain_id) -> Result<BTreeMap<Address, GenesisAccount>>`
  — predeploy + the `ARKIV_DEV_ACCOUNT_COUNT` dev funding accounts;
  suitable for self-contained dev chains.
- Re-exports `alloy_genesis::{Genesis, GenesisAccount}` so consumers
  don't need a direct alloy-genesis dep.

This crate is the single place that knows how to materialise the
EntityRegistry runtime bytecode for a given chain. Both `arkiv-node`
(currently unused, but available for future built-in chainspec support)
and `arkiv-cli inject-predeploy` consume it. The bytecode generation is
deterministic — same chain ID + same `arkiv-bindings` rev produces the
same bytes — which is what lets `init` and `node` agree on the genesis
hash even when the chainspec was assembled at recipe-time.

### 3.2 `arkiv-node`

The execution-client binary. It's a thin wrapper around
`reth_optimism_cli::Cli`, parameterised over an `ArkivExt` clap struct
that layers Arkiv-specific flags on top of `RollupArgs`:

| Flag | Env | Purpose |
|---|---|---|
| `--arkiv.db-url <URL>` | `ARKIV_ENTITYDB_URL` | Enable ExEx (`JsonRpcStore` backend) + `arkiv_*` RPC proxy. Used as the EntityDB write endpoint. |
| `--arkiv.query-url <URL>` | `ARKIV_QUERY_URL` | EntityDB query-side endpoint for the read-path RPC proxy. Defaults to `--arkiv.db-url` when omitted. |
| `--arkiv.debug` | — | Enable ExEx with `LoggingStore` (no RPC). Mutually exclusive with `--arkiv.db-url`. |
| `--arkiv-storaged-path <PATH>` | `ARKIV_STORAGED_PATH` | Start arkiv-storaged as a supervised child process before EntityDB health checks. |
| `--arkiv-storaged-args <ARGS>` | `ARKIV_STORAGED_ARGS` | Space-separated arguments passed to the supervised arkiv-storaged process. |

Main dispatch — predeploy detection + flag combo selects one of five
branches:

```rust
match (predeploy, ext.arkiv_db_url, ext.arkiv_debug) {
    (false, None, false)    => /* plain op-reth */,
    (false, _, _)           => bail!("Arkiv flags set but predeploy missing"),
    (true,  None, false)    => bail!("--arkiv.db-url or --arkiv.debug required"),
    (true,  None, true)     => /* ExEx + LoggingStore */,
    (true,  Some(url), false) => /* ping; ExEx + JsonRpcStore + arkiv_query RPC */,
    (true,  Some(_), true)  => unreachable!(/* clap rejects */),
}
```

There is **no chainspec mutation**. The chainspec is whatever `--chain`
loaded; we never reach into `config_mut()` to inject anything at startup.
This is what lets the binary serve as a true drop-in op-reth.

#### 3.2.1 ExEx activation

Activation is gated by the chainspec **and** an explicit flag. The
predeploy detector (`has_arkiv_predeploy`) checks the loaded genesis
alloc for our predeploy address, computes the expected runtime bytecode
for *this* chain's chain ID, and compares hashes:

```rust
fn has_arkiv_predeploy(chain: &OpChainSpec) -> bool {
    let chain_id = chain.inner.chain.id();
    let Some(account) = chain.inner.genesis.alloc.get(&ENTITY_REGISTRY_ADDRESS)
    else { return false; };
    let Some(code) = &account.code else { return false; };
    let Ok(expected) = arkiv_genesis::deploy_creation_code(chain_id)
    else { return false; };
    keccak256(code) == keccak256(&expected)
}
```

Address-presence alone isn't sufficient: a hostile chainspec could squat
at `0x44…0044` with unrelated code. The hash equality check makes
predeploy detection a property of the chainspec content, not of either
the chain ID or the binary's identity.

Detection on its own no longer auto-installs the ExEx — operators must
opt in explicitly via one of the flags above. The combined matrix:

| predeploy | `--arkiv.db-url` | `--arkiv.debug` | outcome |
|---|---|---|---|
| no  | unset           | unset | plain op-reth (vanilla) |
| no  | set or true     | any   | hard fail: "Arkiv flags set but predeploy missing" |
| yes | unset           | unset | hard fail: "--arkiv.db-url or --arkiv.debug required" |
| yes | set, ping fails | unset | hard fail: "EntityDB unreachable at <url>" |
| yes | set, ping ok    | unset | ExEx (`JsonRpcStore`) + `arkiv_query` RPC |
| yes | unset           | set   | ExEx (`LoggingStore`); no RPC |
| yes | set             | set   | clap rejects at parse time (`conflicts_with`) |

The explicit-opt-in change closes a footgun the previous auto-activate
behaviour created: a chain operator deploying the predeploy via
`inject-predeploy` and forgetting to wire up EntityDB would silently get
a `LoggingStore`-backed ExEx in production. The flag now forces a
startup decision.

If `--arkiv-storaged-path` is set, `arkiv-node` starts that executable as
a child process before resolving Arkiv mode, so `--arkiv.db-url` can point
at the managed service. Stdout and stderr are captured into tracing with
`ARKIV-STORAGED-STDOUT` and `ARKIV-STORAGED-STDERR` prefixes. Any
unexpected storaged exit is fatal for `arkiv-node`; normal node shutdown
terminates the child process.

#### 3.2.2 Storage backend

The flag set selects one of two implementations of `trait Storage`:

- `--arkiv.db-url <URL>` → `JsonRpcStore` posts to that URL. Performs a
  startup health check; if the URL is unreachable the binary exits
  cleanly rather than crashing the ExEx mid-stream.
- `--arkiv.debug` → `LoggingStore` emits structured tracing events for
  every operation. Used for local dev and integration smoke tests.

Both implement:

```rust
pub trait Storage: Send + Sync + 'static {
    fn handle_commit(&self, blocks: &[ArkivBlock]) -> Result<Option<B256>>;
    fn handle_revert(&self, blocks: &[ArkivBlockRef]) -> Result<Option<B256>>;
    fn handle_reorg(&self, reverted: &[ArkivBlockRef], new_blocks: &[ArkivBlock]) -> Result<Option<B256>>;
}
```

`ArkivBlock`, `ArkivBlockHeader`, `ArkivTransaction`, `ArkivBlockRef`,
`ArkivOperation`, and `ArkivAttribute` are all defined in
`arkiv_bindings::wire`, not in this crate. The ExEx constructs them and
passes them through the `Storage` trait; the storage backends serialise
them directly.

The optional `B256` return is a state-root hash for backends that
maintain a Merkle trie (currently only the JSON-RPC backend forwards what
EntityDB returns). The trait is sync; `JsonRpcStore` bridges to async via
`tokio::task::block_in_place` since the ExEx loop runs on a multi-threaded
runtime.

#### 3.2.3 Read-side RPC proxy

When `--arkiv.db-url` is set, the node also registers an `arkiv` JSON-RPC
namespace via reth's `extend_rpc_modules` hook. Today it exposes:

| Method | Params | Returns |
|---|---|---|
| `arkiv_query` | `expr: string`, `options?: object` | `{ data, blockNumber, cursor? }` (per EntityDB) |
| `arkiv_getEntityCount` | none | total entity count (number) |
| `arkiv_getBlockTiming` | none | `{ current_block, current_block_time, duration }` |

Each handler is a transparent proxy: positional args are forwarded
verbatim to the configured EntityDB and the raw `result` is returned to
the caller. The same `EntityDbClient` (connection pool, timeouts) backs
both the write-side `JsonRpcStore` and these read-side handlers. Errors
from EntityDB are surfaced as JSON-RPC error `-32000` with the
underlying message.

Not implementing wildcard forwarding (e.g. anything matching `arkiv_*`)
is intentional: jsonrpsee dispatches by exact method name, and a
wildcard would either need EntityDB-side method introspection at startup
or a custom middleware layer. The typed-trait approach — one rpc trait
method per supported EntityDB endpoint — keeps the wire surface
explicit at the cost of mirroring new EntityDB methods here as they're
added.

The RPC namespace is registered on every transport that the operator has
enabled (`--http`, `--ws`, `--ipc`); operators who want to keep the
proxy off simply don't pass `--arkiv.db-url`.

### 3.3 `arkiv-cli`

The operator command-line tool. Two distinct surfaces:

#### 3.3.1 Entity operations (require an RPC endpoint + signer)

| Subcommand | What it does |
|---|---|
| `create` | Mint an entity. Either `--payload <bytes>` or `--random-payload` (with optional `--size`) is required. Also supports `--content-type` and `--attributes`. |
| `update` | Replace an existing entity's payload. Either `--payload <bytes>` or `--random-payload` (with optional `--size`) is required. Also supports `--content-type` and `--attributes`. |
| `extend` | Push out an entity's expiry |
| `transfer` | Hand ownership to another address |
| `delete` | Owner-initiated removal |
| `expire` | Anyone-callable removal of a past-expiry entity |
| `query` | Read a single entity's commitment |
| `hash` | Print the contract's current rolling `changeSetHash` |
| `history` | Walk the per-op hash chain from head to genesis |
| `balance` | ETH balance for the signer (or `--address`) |
| `spam` | Fire many CREATE txs with backpressure on the txpool |
| `batch <FILE>` | Submit an arbitrary sequence of ops in one tx (see below) |
| `simulate` | Continuously generate a weighted op mix with multi-signer rotation (see [§3.5](#35-the-simulate-command)) |

Defaults are tuned for local dev (`http://localhost:8545`, the hardhat
mnemonic #0 key, `--chain arkiv` predeploy address). All overridable via
flags.

#### 3.3.2 Genesis post-processing (no network required)

```
arkiv-cli inject-predeploy <input.json> [--out output.json]
```

This subcommand short-circuits before any signer/provider setup. It:

1. Parses the input as a geth-format `Genesis`.
2. Reads `config.chainId`.
3. Calls `arkiv_genesis::genesis_alloc(chain_id)` to mint the matching
   predeploy runtime bytecode plus the `ARKIV_DEV_ACCOUNT_COUNT`
   mnemonic-derived dev accounts (each pre-funded with
   `arkiv_dev_balance_wei`).
4. Splices the resulting `(Address, GenesisAccount)` pairs into the
   input's `alloc`, warning if an entry already exists at the
   predeploy address.
5. Writes the augmented JSON back (overwriting the input by default).

Note: today the funding accounts are injected unconditionally — fine for
dev, but in production this puts 100 well-known hardhat accounts into
the genesis. A `--predeploy-only` (or split-command) follow-up is
tracked separately.

It works against *any* geth-format chainspec: a hand-crafted dev base, an
op-deployer-produced production genesis, a snapshot dump from another
node. The chain ID it reads from the input is what binds the predeploy's
EIP-712 immutables, so the resulting bytecode is correct for that chain.

### 3.4 The batch format

`arkiv-cli batch <FILE>` takes a JSON array of operations and submits them
as a single `execute()` call. The schema is loosely OP-flavored:

```json
[
  {
    "type": "create",
    "contentType": "application/json",
    "payload": "{\"v\":1}",
    "expiresIn": "1h",
    "attributes": [
      { "name": "title", "string": "the answer" },
      { "name": "priority", "uint": 42 },
      { "name": "linked.to", "entityKey": "$0" }
    ]
  },
  {
    "type": "update",
    "entityKey": "$0",
    "payload": "{\"v\":2}"
  }
]
```

Notable mechanics:

- **Cross-references**: `"$N"` in an `entityKey` field refers to the Nth
  op in the batch (which must be a CREATE). The CLI calls
  `registry.entityKey(signer, signer_nonce + create_index)` upfront —
  before submission, so the predicted address is stable — and substitutes.
  Literal `"0x…"` hex is accepted in the same fields for pre-existing
  entities.
- **Payload**: optional UTF-8 string (or `0x`-prefixed hex). Mutually
  exclusive with `size` (random bytes). Both omitted → empty payload.
- **Attributes**: validated client-side. Names go through
  `Ident32::encode` (lowercase ASCII, max 32 bytes); strings are capped
  at 128 bytes; values are packed per the `value128-encoding.md` rules.
  Auto-sorted ascending by name (the contract requires strict order).
- **Durations**: `expiresIn` accepts humantime strings (`"1h"`, `"7d"`).
  Converted to a relative block count (`btl = duration / block_time`)
  and passed as `Operation.btl`. The contract resolves the absolute
  expiry as `currentBlock + btl` at execution time. The CLI does not
  need to know the current block number for this calculation.

See `scripts/fixtures/` for end-to-end examples.

### 3.5 The `simulate` command

`arkiv-cli simulate` is a continuous load generator. It rotates through
mnemonic-derived signers, maintains an in-memory pool of "alive"
entities, and submits a weighted random mix of operations against a
running node — meant to exercise the full ExEx → EntityDB pipeline
under traffic that resembles real usage.

```
arkiv-cli simulate \
    [--rate <batches/s>]        # default: 0.5
    [--duration <humantime>]    # 0 = unbounded (default)
    [--signer-count <N>]        # default: 10, capped at ARKIV_DEV_ACCOUNT_COUNT
    [--max-ops-per-tx <N>]      # default: 5; each batch carries 1..=N ops
    [--weights <op=N,…>]        # default: create=4,update=3,extend=2,transfer=1,delete=1
    [--max-alive <N>]           # default: 1000; CREATE throttles when reached
    [--status-interval <dur>]   # default: 10s
    [--seed <u64>]              # deterministic if given
```

Mechanics:

- **Multi-signer wallet.** All `--signer-count` signers are derived from
  `ARKIV_DEV_MNEMONIC` and registered with one shared `EthereumWallet`;
  per-tx selection is via `.from(addr)`. Each tx is signed by one
  designated signer; ops in a batch are constrained to entities that
  signer owns (plus fresh CREATEs).
- **Per-signer concurrency.** Each signer is a "slot" with an
  `AtomicBool` busy flag. Up to `signer_count` batches can be in flight
  at once — one per signer, so each account's nonce stream stays
  sequential without manual tracking. The driver loop ticks at `1/rate`
  seconds, picks an idle slot, builds a batch, and spawns a submission
  task. Effective throughput is bounded by `min(rate × batch_size,
  signer_count × chain_throughput)`.
- **Multi-op batches.** Each tx contains `1..=max_ops_per_tx` operations
  encoded into one `execute()` call. Op selection within a batch is
  independent — no in-batch cross-references, so we don't predict
  entity keys ahead of time. Pending entities (already targeted by an
  in-flight batch) are excluded from selection in any other batch.
- **Op selection.** Priority queue: any past-expiry entity becomes an
  EXPIRE candidate immediately. Otherwise weighted random among feasible
  CRUD ops — feasibility checks include `alive < max_alive` for CREATE
  (counting in-batch creates too), `signer-owned alive > 0` for
  UPDATE/EXTEND/DELETE, plus `signers >= 2` for TRANSFER.
- **State updates.** All transitions apply under a shared
  `tokio::sync::Mutex<State>` after the receipt resolves. Successful
  CREATEs decode `EntityOperation` logs in order to learn new entity
  keys; UPDATEs clear `pending`; EXTENDs bump `expires_at`; TRANSFERs
  swap `owner_idx`; DELETE/EXPIRE remove from `alive`. On revert or
  network error, all `pending` flags are cleared and counters increment
  the `failed` column without state changes.
- **Cancellation.** `tokio::select!` between the work tick and
  `signal::ctrl_c()`; on shutdown the driver waits up to 15 s for
  in-flight tasks to drain, then prints `[final, interrupted]`.

Status reports show send/confirm/fail counters per op type plus alive
count, expired-queue size, and current in-flight batches, every
`--status-interval` seconds.

Scope notes (intentional):

- No persistence — restarts forget their entity pool. The simulator's
  state is recoverable from chain history if anyone ever needs it.
- One tx in flight per signer (not per-signer pipelined). To push beyond
  `signer_count` concurrency you'd add per-signer nonce management; not
  implemented yet.
- No in-batch cross-references. UPDATE-of-just-CREATEd in the same tx
  is supported by the `batch` command but skipped here for simplicity.
- No retry on revert. Failed ops increment the `failed` counter and the
  simulator moves on.

---

## 4. Genesis construction

Genesis is the thorniest part of integrating with the OP stack, and the
design here has been iterated several times. The current rules:

### 4.1 No runtime mutation

Earlier versions of `arkiv-node` mutated the loaded chainspec in `main.rs`
— forcing chain ID 1337, injecting the predeploy, zeroing OP hardfork
timestamps, etc. This **broke `op-reth init`**: `init` reads the
chainspec untouched and writes its genesis hash to disk, while `node`
applied the mutation and computed a different genesis hash, producing the
classic
`genesis hash in the storage does not match the specified chainspec`
error.

The fix was to remove the mutation entirely. The chainspec is treated as
read-only data flowing in from `--chain`. Whatever needs to be in there
must already be in there before `--chain` is parsed.

### 4.2 Path A vs Path B

OP-reth supports two ways to build an `OpChainSpec`:

| | **Path A** (pure JSON) | **Path B** (programmatic forks) |
|---|---|---|
| Hardfork source | `config.{bedrockBlock, regolithTime, …}` + `config.optimism.{eip1559…}` parsed via `OpChainSpec::from(Genesis)` | JSON has only allocs/chainId; forks attached in code via `LazyLock` |
| Examples | `optimism`, `base`, `op_sepolia`, all superchain registry entries | `dev` (`OP_DEV`), the static `OP_MAINNET`/`BASE_MAINNET` definitions |
| `--chain ./file.json` | **Yes** — works end-to-end | **No** — forks won't be activated |
| `dump-genesis` roundtrips? | **Yes** | **No** — output is missing the hardfork data |

For an `--chain ./our.json` flow to work for both `init` and `node`,
the chainspec **must** be Path A. Otherwise reth loads it with no
hardforks active, the engine produces post-hardfork blocks anyway, and
validation explodes.

`chainspec/dev.base.json` is therefore Path A:

```json
{
  "config": {
    "chainId": 1337,
    "bedrockBlock": 0, "regolithTime": 0, "canyonTime": 0,
    "ecotoneTime": 0, "fjordTime": 0, "graniteTime": 0,
    "holoceneTime": 0, "isthmusTime": 0,
    "shanghaiTime": 0, "cancunTime": 0,
    "optimism": { "eip1559Elasticity": 6, "eip1559Denominator": 50, "eip1559DenominatorCanyon": 250 },
    ...
  },
  "extraData": "0x000000000000000000",
  "alloc": {}
}
```

The `extraData` value is the second subtle point.

### 4.3 The Holocene `extraData` requirement

After Holocene activates, EIP-1559 base-fee parameters are encoded in
the previous block's `extraData` (9 bytes:
`[version=0x00][denominator: u32 BE][elasticity: u32 BE]`). When block 1
is validated against genesis, the consensus path calls
`decode_holocene_extra_data(genesis.extra_data)` and bails if the length
isn't exactly 9 bytes.

Empty `extraData` ⇒ `InvalidExtraDataLength` ⇒ `BaseFeeMissing` warning
on every block. The local miner retries until something sticks, so the
chain progresses, but the log is noise.

The decoder has a documented fallback: if the encoded denominator and
elasticity are both zero, it falls back to the chainspec's
`base_fee_params_at_timestamp`. So `0x000000000000000000` (9 zero bytes)
is a valid value that says "use chainspec params at block 0".

This is how `chainspec/dev.base.json` ships. (`OP_DEV` itself has empty
`extraData` and produces the same warnings — they're recoverable in
practice. We do better than `--dev` here.)

### 4.4 The injection step

`chainspec/dev.base.json` ships with an empty `alloc` — no predeploy,
no funded accounts. Both are added via `arkiv-cli inject-predeploy` at
recipe time (it splices in `arkiv_genesis::genesis_alloc(chain_id)`,
which is the predeploy plus the mnemonic-derived dev accounts):

```bash
cp chainspec/dev.base.json $TMPDIR/genesis.json
arkiv-cli inject-predeploy $TMPDIR/genesis.json
op-reth init --chain $TMPDIR/genesis.json --datadir $TMPDIR
op-reth node --chain $TMPDIR/genesis.json --datadir $TMPDIR …
```

Why not bake the predeploy bytecode directly into `dev.base.json`?

- **Drift.** Bytecode regenerates whenever the bindings rev bumps. A
  build-time injection means every build is consistent with the bindings
  it actually depends on; checking in pre-baked bytecode would require
  a CI drift guard.
- **Reusability.** The same `inject-predeploy` tool composes with
  op-deployer output for production deployments — it's not just a dev
  helper. A pre-baked dev file wouldn't generalise.

### 4.5 The production flow

```
op-deployer apply --intent intent.toml --workdir ./ops
   └─► ops/genesis.json   (standard OP chainspec, all OP system contracts in alloc)
arkiv-cli inject-predeploy ops/genesis.json
   └─► ops/genesis.json   (now also has EntityRegistry at 0x44…0044)
op-reth init --chain ops/genesis.json --datadir ./data
op-reth node --chain ops/genesis.json --datadir ./data
```

`inject-predeploy` works against any geth-format chainspec: a
hand-crafted dev base, an op-deployer-produced production genesis, a
snapshot dump from another node. The chain ID it reads from the input
binds the predeploy's EIP-712 immutables, so the resulting bytecode is
correct for that chain. As of today it also injects the
`ARKIV_DEV_ACCOUNT_COUNT` mnemonic-derived dev accounts via the same
`genesis_alloc` call (see §3.3.2) — convenient for dev, but the prod
path needs a follow-up to suppress them.

The longer-term option — contributing EntityRegistry to op-deployer's
`L2Genesis.s.sol` so the standard tool produces it directly — is sketched
in this repo's previous design notes but isn't currently being pursued.
The post-process approach trades upstream contribution for a tiny
maintenance surface that we own.

---

## 5. ExEx mechanics

The ExEx is implemented in `crates/arkiv-node/src/exex.rs`. Top-level
shape:

```rust
pub async fn arkiv_exex<Node>(mut ctx: ExExContext<Node>, store: Arc<dyn Storage>) -> Result<()> {
    while let Some(notification) = ctx.notifications.try_next().await? {
        match &notification {
            ExExNotification::ChainCommitted { new } => {
                let prior_rolling = prior_rolling_hash(&ctx, new)?;
                let blocks = extract_blocks(new, prior_rolling);
                store.handle_commit(&blocks)?;
                ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
            }
            ExExNotification::ChainReorged { old, new } => { /* revert + commit */ }
            ExExNotification::ChainReverted { old }      => { /* revert only */ }
        }
    }
    Ok(())
}
```

### 5.1 Per-block extraction

For each canonical block we forward — *including blocks with no
EntityRegistry transactions* — `extract_blocks` produces an `ArkivBlock`:

```
ArkivBlock {
    header:   { number, hash, parent_hash, changeset_hash },
    transactions: [ArkivTransaction { hash, index, sender, operations: [...] }, ...]
}
```

All types (`ArkivBlock`, `ArkivTransaction`, `ArkivOperation`,
`ArkivAttribute`) are defined in `arkiv_bindings::wire`.

For each transaction whose `to` matches the EntityRegistry address, the
receipt logs are pre-filtered to that address and then passed to
`arkiv_bindings::wire::ParsedRegistryTx::parse(calldata, &filtered_logs)`.
This step decodes the `execute()` calldata, separates the receipt logs
into their two event types (`EntityOperation` and `ChangeSetHashUpdate`),
validates 1:1 correspondence, and cross-checks that each pair shares the
same `entityKey`. A subsequent `.decode(tx_hash, tx_index, sender)` call
validates `Mime128` content types and `Ident32` attribute names, and
returns a complete `(ArkivTransaction, Option<B256>)` — the transaction
plus the rolling changeset hash after its last operation.

### 5.2 Rolling changeset hash

`ArkivBlockHeader.changeset_hash` is the *rolling* hash as of the end of
the block — i.e., empty blocks inherit the previous mutated block's hash.
This intentionally diverges from the contract's `changeSetHashAtBlock(N)`
view, which returns `bytes32(0)` for empty blocks; the ExEx form is what
downstream consumers actually want.

The starting value for a notification (the rolling hash *before* the
first new block) is read directly from the contract's storage at the
parent block's state, using the slot helpers exposed by
`arkiv_bindings::storage_layout`:

```rust
let head     = decode_head_block(state.storage(reg, head_block_slot())?);
let node     = decode_block_node(state.storage(reg, block_node_slot(head))?);
let last_tx  = node.txCount - 1;
let op_count = decode_tx_op_count(state.storage(reg, tx_op_count_slot(head, last_tx))?);
let last_op  = op_count - 1;
let hash     = decode_hash_at(state.storage(reg, hash_at_slot(head, last_tx, last_op))?);
```

That's four `state.storage()` calls plus one `history_by_block_hash()` to
open the historical state — once per chain notification, regardless of
how many blocks the notification spans. The cost is dwarfed by the
HTTP/serialisation work the JSON-RPC store does anyway. Within the
notification, blocks carry the rolling value forward in memory.

Direct slot reads are used (rather than `eth_call` to
`registry.changeSetHash()`) because we control both the contracts repo
and this one — the storage layout coupling is acceptable, and a Foundry
artifact-based drift guard lives in `arkiv-bindings::storage_layout`'s
test suite.

### 5.3 Reorgs and reverts

`ChainReorged { old, new }` and `ChainReverted { old }` map onto
`Storage::handle_reorg` / `handle_revert`. Reverted blocks are sent as
minimal `ArkivBlockRef { number, hash }` references — the EntityDB
already has the full data; it just needs to know which heights to roll
back. The newest-first ordering is preserved (block N revert before
block N-1).

For reorgs, the new chain's `prior_rolling_hash` is computed at its
parent — which is also the parent of the reverted blocks — so the
EntityDB sees a coherent revert+commit pair.

### 5.4 Wire format

The full JSON-RPC payload shape is documented in
[`exex-jsonrpc-interface-v2.md`](exex-jsonrpc-interface-v2.md). The
short version:

| Method | Params |
|---|---|
| `arkiv_commitChain` | `{ "blocks": [ArkivBlock] }` |
| `arkiv_revert` | `{ "blocks": [ArkivBlockRef] }` |
| `arkiv_reorg` | `{ "revertedBlocks": [ArkivBlockRef], "newBlocks": [ArkivBlock] }` |

The server returns `{ "stateRoot": "0x..." }` on success.

All wire types are defined in `arkiv_bindings::wire`:

- **Envelopes**: `ArkivBlock`, `ArkivBlockHeader`, `ArkivTransaction`,
  `ArkivBlockRef`
- **Operations**: `ArkivOperation` (tagged enum), `CreateOp`, `UpdateOp`,
  `ExtendOp`, `TransferOp`, `DeleteOp`, `ExpireOp`
- **Attributes**: `ArkivAttribute` (tagged enum with `Uint`, `String`,
  `EntityKey` variants)

The `content_type` field on `CreateOp` / `UpdateOp` holds a `Mime128`
value that serialises as a plain string. `ArkivAttribute` names are
`Ident32` values that serialise as ASCII strings. Both types carry
validation — `Mime128::validate()` and `Ident32::validate()` are called
during `ParsedRegistryTx::decode()`; invalid calldata causes the entire
transaction to be skipped with a logged error.

---

## 6. Testing surface

| Layer | How to test |
|---|---|
| `arkiv-genesis` | Pure functions; reuse the bindings' Foundry-backed drift tests |
| `arkiv-cli inject-predeploy` | Smoke: feed a known JSON, diff the alloc |
| ExEx decoding | Run `just node-dev-jsonrpc` + `just batch <fixture>`, observe mock-entitydb output |
| ExEx rolling hash | Submit two ops on the same entity in one tx; observe distinct `changesetHash` per op |
| Activation guard | Start with `--chain optimism` → ExEx should stay inactive |

The `scripts/fixtures/` directory is a growing collection of batch JSON
files that exercise specific cases:

- `double-op-same-entity.json` — CREATE then UPDATE of the same entity
  in one tx; tests per-op changeset hash correctness.
- `attributes-all-types.json` — tests decoding for all three attribute
  value types (UINT/STRING/ENTITY_KEY), including auto-sorting of names
  before submission.

`scripts/mock-entitydb.js` is a tiny Node script that accepts any of the
three RPC methods and pretty-prints the payload. Enough to inspect the
wire format end-to-end without involving the real Go EntityDB.

---

## 7. Key design decisions, recapped

| Decision                                      | Why |
|-----------------------------------------------|---|
| Predeploy at `0x44…0044`                      | Matches OP convention for system contracts; the address is a property of the chain, not the binary |
| ExEx requires explicit flag (`--arkiv.db-url` / `--arkiv.debug`) on top of bytecode hash check | Bytecode-only auto-activation silently fell back to `LoggingStore` in prod if EntityDB wiring was forgotten; the explicit flag forces a startup decision. With no flags the binary is still a true drop-in op-reth. |
| Direct storage-slot reads for rolling hash    | Cheaper than `eth_call` (no EVM spin-up); coupling acceptable since we control both sides |
| Path-A chainspec (full hardfork data in JSON) | Required for `op-reth init` and `op-reth node` to agree on genesis hash when reading the same JSON |
| `inject-predeploy` as a separate post-process | Composes with op-deployer output rather than forking it; same tool serves dev and prod |
| `arkiv-genesis` as its own crate              | Both binaries need the same predeploy-bytecode generator; lifting it out avoids cross-bin deps |
| `Storage` as a trait, not a concrete type     | Local logging vs JSON-RPC forwarding are equally valid; future backends (e.g. a local sqlite indexer) drop in |
| No runtime chainspec mutation                 | Removed the `init`/`node` divergence bug structurally, not just patched the symptom |

---

## 8. Things this design does *not* do

Honest scope notes:

- **Built-in `--chain arkiv` name.** A custom `ChainSpecParser` could
  register `arkiv` as a name resolved to an embedded chainspec, removing
  the `inject-predeploy` step for dev. Not done; the file-based flow
  works, and it composes uniformly with prod.
- **Mainnet predeploy registration.** The cleanest long-term home for
  EntityRegistry is op-deployer's `L2Genesis.s.sol`. Not pursued yet.
- **L1 / op-node / op-batcher / op-proposer.** Out of scope. This repo
  is the L2 execution client only.
- **Pre-Bedrock state import.** Standard op-reth concern; the canonical
  Optimism docs cover it.
- **Full coverage of EntityDB's RPC surface.** The namespace currently
  proxies `arkiv_query`, `arkiv_getEntityCount`, and `arkiv_getBlockTiming`.
  Other EntityDB methods (e.g. `arkiv_getNumberOfUsedSlots`) and any
  on-node-only RPCs (e.g. an `arkiv_changeSetHash` reading directly from
  contract storage) can be added as additional trait methods; not pursued
  yet.

---

## 9. Where to read next

- **Contract internals.** `arkiv-contracts/contracts/EntityRegistry.sol`
  and `Entity.sol`. The hashing scheme (EIP-712 typehashes for `CoreHash`
  and `EntityHash`, the per-op rolling extension) is the single most
  important thing to understand if you're touching the ExEx decoder.
  Key field: `Operation.btl` (blocks-to-live, relative) replaces the
  former absolute `expiresAt`; the contract stores and emits the resolved
  absolute expiry.
- **Wire format.** [`exex-jsonrpc-interface-v2.md`](exex-jsonrpc-interface-v2.md).
- **Bindings: decoding.** `arkiv_bindings::wire` — `ParsedRegistryTx`
  (calldata + logs → `ArkivTransaction`), the `ArkivOperation` /
  `ArkivAttribute` enums, and all envelope types (`ArkivBlock`,
  `ArkivBlockHeader`, `ArkivTransaction`, `ArkivBlockRef`).
- **Bindings: encoding.** `arkiv_bindings::encode` — `impl Operation`
  factory methods (`Operation::create`, `Operation::extend`, …) and
  `impl Attribute` factory methods (`Attribute::uint`, `Attribute::string`,
  `Attribute::entity_key`, `Attribute::sort`). These are the primary
  interface for building `execute()` calldata without touching the raw
  flat struct.
- **Bindings: validated types.** `arkiv_bindings::Ident32` and
  `arkiv_bindings::Mime128` are alloy-generated types with validation
  impl blocks. `Ident32::encode(s)` validates the charset;
  `Mime128::encode(s)` validates the RFC 2045 structure. Both carry
  `Serialize` impls for the wire format.
- **Attribute encoding.**
  `arkiv-contracts/docs/value128-encoding.md` — explains the
  `bytes32[4]` packing for UINT/STRING/ENTITY_KEY values and which
  byte-level orderings are guaranteed.
- **Storage layout.** `arkiv_bindings::storage_layout` — slot
  indices, key-packing functions, decoders. The Foundry-artifact drift
  guard there is the safety net for the ExEx's direct slot reads.
- **reth ExEx framework.** <https://reth.rs/exex.html>.
- **op-reth.** <https://github.com/ethereum-optimism/optimism/tree/develop/rust/op-reth>.
