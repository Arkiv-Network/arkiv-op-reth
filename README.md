# arkiv-op-reth

An [op-reth](https://github.com/ethereum-optimism/optimism)-derived execution
node for the **Arkiv** chain, plus operator tooling. Arkiv adds a single
predeploy — `EntityRegistry` — to an OP-stack chain, and an Execution
Extension (ExEx) that streams decoded entity operations to a downstream
indexer (the Go EntityDB).

The binary is a **drop-in op-reth**: against a vanilla OP chainspec it runs
unchanged. On a chainspec containing the EntityRegistry predeploy, the ExEx
is enabled by passing one of:

- `--arkiv.db-url <URL>` — forward decoded ops to EntityDB and expose the
  `arkiv_query` JSON-RPC proxy method.
- `--arkiv.debug` — emit decoded ops to tracing logs (no EntityDB,
  no RPC). For local dev / smoke tests.

When `--arkiv-storaged-path <PATH>` is set, `arkiv-node` also supervises
that `arkiv-storaged` subprocess for the node lifetime. Extra arguments
can be supplied with `--arkiv-storaged-args "<space separated args>"`.

```
                ┌─────────────────────────────────────────────┐
                │  arkiv-node (op-reth + Arkiv ExEx)          │
                │                                             │
  L1 / op-node  │   ┌─────────┐                ┌─────────┐    │   ┌────────────┐
  ───────────►  │   │ Reth    │  ChainCommit   │ Arkiv   │────┼──►│ EntityDB   │
                │   │ engine  │ ─────────────► │ ExEx    │ JSON   │ (Go)       │
                │   └─────────┘                └─────────┘ -RPC   └────────────┘
                │        ▲                                    │
                │        │ EntityRegistry calls               │
                │        │                                    │
                └────────┼────────────────────────────────────┘
                         │
                    arkiv-cli (operator CLI)
```

---

## What this repository contains

| Crate | Role |
|---|---|
| `crates/arkiv-node` | The execution-client binary. Wraps op-reth's `Cli`, conditionally installs the Arkiv ExEx. |
| `crates/arkiv-cli` | Operator CLI: submit entity ops, batch ops from JSON, post-process genesis files for deployment. |
| `crates/arkiv-genesis` | Shared library: predeploy address constant, runtime-bytecode generator, genesis-alloc helpers. |

External dependencies of note:

| Crate | Repo | Role |
|---|---|---|
| `arkiv-bindings` | [`arkiv-contracts`](https://github.com/Arkiv-Network/arkiv-contracts) | Solidity ABI types, decoders, storage-layout helpers. |
| `reth-optimism-*` | [`ethereum-optimism/optimism`](https://github.com/ethereum-optimism/optimism) | OP-reth runtime, chainspec, primitives. |
| `reth-*` | [`paradigmxyz/reth`](https://github.com/paradigmxyz/reth) | ExEx framework, state-provider API. |

---

## Quick start

### Local dev node (with logging storage)

```bash
just node-dev
```

Generates an Arkiv dev genesis (chain ID `1337`, dev account funded,
EntityRegistry at `0x4400000000000000000000000000000000000044`),
initialises the datadir against it, and launches the node with auto-mining
at 2 s blocks. The recipe passes `--arkiv.debug`, so the ExEx emits decoded
ops as tracing events.

### Local dev node forwarding to a mock EntityDB

```bash
just mock-entitydb            # terminal 1: starts a JSON-RPC mock on :9545
just node-dev-jsonrpc         # terminal 2: node + --arkiv.db-url=http://localhost:9545
```

The ExEx invokes `arkiv_commitChain` / `arkiv_revert` / `arkiv_reorg`
against the mock; the same URL also backs the read-side `arkiv_query`
RPC proxy. The mock script logs every inbound JSON-RPC payload — useful
for inspecting the wire format end-to-end:

```bash
just query                                  # default null payload
just query '{"key":"0x..."}'                # arbitrary JSON payload
```

### Submit operations

```bash
just balance                                 # 10,000 ETH
just create --content-type application/json  # mint an entity
just update --key 0x... --content-type ...   # update its content
just history                                 # walk the changeset chain
```

Or batch a sequence in one transaction (with cross-references):

```bash
just batch scripts/fixtures/double-op-same-entity.json
just batch scripts/fixtures/attributes-all-types.json
```

See [`docs/architecture.md`](docs/architecture.md#cli-the-batch-format)
for the batch JSON schema.

### Continuous simulation

For a steady stream of mixed traffic against a running node — useful for
exercising the ExEx, EntityDB, or downstream observers under realistic
load:

```bash
just simulate                                          # 0.5 batches/s, 10 signers, until Ctrl-C
just simulate --rate 2 --duration 5m                   # 2 batches/s for 5 min
just simulate --max-ops-per-tx 8 --signer-count 25     # bigger batches, more parallelism
just simulate --seed 42                                # deterministic run
```

The simulator runs **per-signer in parallel** (each signer can hold one
in-flight tx; up to `--signer-count` concurrent batches) and bundles
**multiple ops per transaction** (each batch carries `1..=max-ops-per-tx`
ops in a single `execute()` call). It rotates through the first N
mnemonic-derived signers (default 10, capped at
`ARKIV_DEV_ACCOUNT_COUNT = 100`), tracks alive entities in memory, and
submits a weighted random mix of CREATE/UPDATE/EXTEND/TRANSFER/DELETE.
EXPIRE fires event-driven on past-expiry entities.

### Inspect the embedded dev chainspec

```bash
just genesis            # prints assembled JSON to stdout
just genesis | jq .alloc
```

---

## Project layout

```
.
├── crates/
│   ├── arkiv-node/           # op-reth binary + ExEx
│   ├── arkiv-cli/            # operator CLI
│   └── arkiv-genesis/        # shared genesis primitives (lib)
├── chainspec/
│   └── dev.base.json         # geth-format dev chainspec sans predeploy
├── docs/
│   ├── architecture.md       # system design (start here)
│   └── exex-jsonrpc-interface-v2.md   # wire format spec
├── scripts/
│   ├── mock-entitydb.js      # logs incoming JSON-RPC for local testing
│   └── fixtures/             # example batch files
└── justfile                  # all dev/test recipes
```

---

## Running against a real OP chain

For production / testnet deployment, the EntityRegistry predeploy must be
in the genesis allocs from block 0. Two integration paths exist
(see [`docs/architecture.md`](docs/architecture.md#genesis-construction)):

### Option A — Post-process op-deployer output (current)

```bash
op-deployer apply --intent intent.toml --workdir ./ops    # standard OP genesis
arkiv-cli inject-predeploy ops/genesis.json               # add EntityRegistry
op-reth init --chain ops/genesis.json --datadir ./data
op-reth node --chain ops/genesis.json --datadir ./data
```

`inject-predeploy` reads `chainId` from the input, computes the matching
runtime bytecode (constructor immutables bound to that chain ID), and
splices it into `alloc` at the canonical predeploy address. The same
chainspec drives both `init` and `node`, so genesis hashes match.

### Option B — Upstream contribution to `L2Genesis.s.sol`

Contribute (or fork) op-deployer's L2Genesis script to include
EntityRegistry as a standard predeploy. Then op-deployer output already
contains it, no post-processing needed. Not yet pursued — see the
architecture doc for trade-offs.

---

## Status

Working today:

- `--chain arkiv` … is **not** registered as a built-in. The current
  approach is a JSON-file chainspec (`chainspec/dev.base.json` +
  `inject-predeploy`); the binary takes the file via `--chain <path>`.
- ExEx detects the predeploy by chainspec content and activates on
  explicit operator opt-in (`--arkiv.db-url` or `--arkiv.debug`).
- ExEx → EntityDB JSON-RPC v2 wire format is complete and documented.
- `arkiv_*` JSON-RPC namespace (registered when `--arkiv.db-url` is set;
  transparent passthrough to EntityDB). Currently: `arkiv_query`,
  `arkiv_getEntityCount`, `arkiv_getBlockTiming`.
- Operator CLI covers all six entity-operation types plus batched submission
  with cross-references between ops.
- Storage backends: `LoggingStore` (tracing) and `JsonRpcStore`
  (forwarding to Go EntityDB).

Open / future:

- A registered `--chain arkiv` shortcut (custom `ChainSpecParser`) — not
  blocked, just hasn't been needed yet.
- Upstream contribution to `L2Genesis.s.sol` — out of scope for this repo.
- Mainnet deployment.

---

## Documentation

| Doc | What's in it |
|---|---|
| [`docs/architecture.md`](docs/architecture.md) | System overview, component breakdown, data flow, design decisions |
| [`docs/exex-jsonrpc-interface-v2.md`](docs/exex-jsonrpc-interface-v2.md) | Exact wire format the ExEx posts to EntityDB |

External references:

- EntityRegistry contract: <https://github.com/Arkiv-Network/arkiv-contracts>
  - `contracts/EntityRegistry.sol` — the contract itself
  - `contracts/Entity.sol` — encoding / hashing library
  - `docs/value128-encoding.md` — how attribute values are packed
- op-reth: <https://github.com/ethereum-optimism/optimism/tree/develop/rust/op-reth>
- reth ExEx framework: <https://reth.rs/exex.html>

---

## Build & lint

```bash
just check          # cargo check --workspace
just build          # cargo build --workspace
just lint           # cargo clippy -- -D warnings
just fmt            # cargo fmt --all
```

The workspace pins `reth-*` and `reth-optimism-*` to specific git revs in
the root `Cargo.toml`. Bumping them is a coordinated change; expect to
re-resolve API drift across the ExEx module.

---

## License

GPL-3.0-or-later. See `LICENSE`.
