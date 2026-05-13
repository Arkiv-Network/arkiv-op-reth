# arkiv-op-reth

An [op-reth](https://github.com/ethereum-optimism/optimism)-derived execution
node for the **Arkiv** chain, plus operator tooling. Arkiv is an OP-stack
L2/L3 with one additional predeploy — `EntityRegistry` at
`0x4400000000000000000000000000000000000044` — and one in-process
extension to op-reth: a custom `EvmFactory` that registers the **Arkiv
precompile** into revm's `PrecompilesMap`. Entity payloads and the
annotation index live in the L3 state trie as Ethereum accounts; a single
custom MDBX table (`ArkivPairs`) acts as an append-only prefix index for
range / glob queries.

The binary serves both write and read paths:

- **Writes** go through `EntityRegistry.execute(Operation[])`. The
  contract validates ownership / lifetime, then calls the precompile,
  which performs content validation and writes entity / pair / system
  account state via revm's journaled state.
- **Reads** are served by the `arkiv_*` JSON-RPC namespace registered
  on the node's standard transports — backed entirely by local trie
  state and the `ArkivPairs` table. No external indexer process.

```
                  ┌──────────────────────────────────────────────────┐
                  │ arkiv-node binary                                │
                  │                                                  │
                  │   revm + ArkivEvmFactory                         │
   user tx ──────►│   └─► ArkivPrecompile ──► trie state             │
                  │                           (entity / pair /       │
                  │                            system accounts)       │
                  │                       └─► ArkivPairs MDBX table  │
                  │                                                  │
   user query ───►│   arkiv_* RPC (local reads)                      │
                  └──────────────────────────────────────────────────┘
```

> **Implementation status.** Phase 1 of the v1→v2 migration is complete:
> the off-process EntityDB + ExEx + JSON-RPC bridge has been removed.
> Phases 2–6 (precompile, RPC namespace, integration) are in progress.
> The binary currently compiles as predeploy-aware op-reth with no
> Arkiv extensions installed. See [`docs/architecture.md`](docs/architecture.md)
> §10 for the per-phase status and `arkiv-op-reth-v2-migration-plan.md`
> at the workspace root for the full phased plan.

---

## What this repository contains

| Crate | Role |
|---|---|
| `crates/arkiv-node` | Execution-client binary. Wraps op-reth's `Cli`; will host the custom `EvmFactory`, the Arkiv precompile, and the `arkiv_*` RPC namespace. |
| `crates/arkiv-cli` | Operator CLI: submit entity ops, batch ops from JSON, traffic simulator, genesis post-processing. |
| `crates/arkiv-genesis` | Shared library: predeploy address, runtime-bytecode generator, genesis-alloc helpers. |

External dependencies of note:

| Dep | Repo | Role |
|---|---|---|
| `arkiv-bindings` | [`arkiv-contracts`](https://github.com/Arkiv-Network/arkiv-contracts) | Solidity ABI types, validated types (`Ident32`, `Mime128`), operation encoders. |
| `reth-optimism-*` | [`ethereum-optimism/optimism`](https://github.com/ethereum-optimism/optimism) | OP-reth runtime, chainspec, primitives. |
| `reth-*` | [`paradigmxyz/reth`](https://github.com/paradigmxyz/reth) | Node builder, storage API. |

---

## Quick start

### Local dev node

```bash
just node-dev
```

Assembles an Arkiv dev genesis (chain ID `1337`, 100 dev accounts
funded, `EntityRegistry` predeploy at `0x4400…0044`), initialises the
datadir against it, and launches the node with auto-mining at 2 s
blocks. Listens on `localhost:8545`.

### Submit operations

```bash
just balance                                 # 10,000 ETH on the dev account
just create --content-type application/json  # mint an entity
just update --key 0x... --content-type ...   # update it
```

Or batch a sequence in one transaction (with cross-references between
ops in the same batch):

```bash
just batch scripts/fixtures/attributes-all-types.json
```

See [`docs/architecture.md`](docs/architecture.md) for the batch JSON
schema and the entity op surface.

### Continuous simulation

For a steady stream of mixed traffic against a running node:

```bash
just simulate                                          # 0.5 batches/s, 10 signers, until Ctrl-C
just simulate --rate 2 --duration 5m                   # 2 batches/s for 5 min
just simulate --max-ops-per-tx 8 --signer-count 25     # bigger batches, more parallelism
just simulate --seed 42                                # deterministic run
```

The simulator rotates through the first N mnemonic-derived signers
(default 10, capped at `ARKIV_DEV_ACCOUNT_COUNT = 100`), tracks alive
entities in memory, and submits a weighted random mix of
CREATE/UPDATE/EXTEND/TRANSFER/DELETE. Each signer holds at most one
in-flight tx; up to `--signer-count` concurrent batches. Each batch
carries `1..=--max-ops-per-tx` ops in a single `execute()` call.

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
│   ├── arkiv-node/           # binary; v2 wiring lands in phases 2-5
│   ├── arkiv-cli/            # operator CLI
│   └── arkiv-genesis/        # shared genesis primitives (lib)
├── chainspec/
│   └── dev.base.json         # geth-format dev chainspec sans predeploy
├── docs/
│   ├── architecture.md       # system design (start here)
│   └── statedb-design.md     # canonical state model
├── scripts/
│   └── fixtures/             # example batch JSON files
├── docker/                   # runtime + dev container images
└── justfile                  # all dev/test recipes
```

---

## Running against a real OP chain

For production / testnet deployment the `EntityRegistry` predeploy must
be in the genesis allocs from block 0:

```bash
op-deployer apply --intent intent.toml --workdir ./ops     # standard OP genesis
arkiv-cli inject-predeploy ops/genesis.json                # add EntityRegistry
op-reth init --chain ops/genesis.json --datadir ./data
op-reth node --chain ops/genesis.json --datadir ./data
```

`inject-predeploy` reads `chainId` from the input, computes the
matching runtime bytecode (constructor immutables bound to that chain
ID), and splices it into `alloc` at the canonical predeploy address.
The same chainspec drives both `init` and `node`, so genesis hashes
match.

See [`docs/architecture.md`](docs/architecture.md) §8 for the full
genesis-construction rules (Path-A chainspecs, Holocene `extraData`,
why we don't mutate the chainspec at startup).

---

## Documentation

| Doc | What's in it |
|---|---|
| [`docs/architecture.md`](docs/architecture.md) | System overview, workspace layout, precompile / RPC / genesis design, implementation status |
| [`docs/statedb-design.md`](docs/statedb-design.md) | Canonical state model: entity / pair / system accounts, gas, query verification, reorg posture |

External references:

- EntityRegistry contract: <https://github.com/Arkiv-Network/arkiv-contracts>
  - `contracts/EntityRegistry.sol`, `contracts/Entity.sol`,
    `docs/value128-encoding.md`
- op-reth: <https://github.com/ethereum-optimism/optimism/tree/develop/rust/op-reth>
- reth: <https://github.com/paradigmxyz/reth>

---

## Build & lint

```bash
just check          # cargo check --workspace
just build          # cargo build --workspace
just lint           # cargo clippy -- -D warnings
just fmt            # cargo fmt --all
```

The workspace pins `reth-*` and `reth-optimism-*` to specific git revs
in the root `Cargo.toml`. Bumping them is a coordinated change; expect
to re-resolve API drift across the EvmFactory / precompile integration.

---

## License

GPL-3.0-or-later. See `LICENSE`.
