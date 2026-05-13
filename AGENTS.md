# AGENTS.md

Orientation for coding agents working in this repo. For human-facing
docs read [`README.md`](README.md) and [`docs/architecture.md`](docs/architecture.md)
first. The canonical state model is [`docs/statedb-design.md`](docs/statedb-design.md).

## What this repo is

A drop-in [op-reth](https://github.com/ethereum-optimism/optimism) build
for the **Arkiv** chain plus operator tooling. Arkiv = OP-stack L2/L3
with one extra predeploy (`EntityRegistry` at
`0x4400000000000000000000000000000000000044`) and a custom op-reth
`EvmFactory` that registers the Arkiv precompile into revm's
`PrecompilesMap`. Entity state lives in the L3 trie; one custom MDBX
table (`ArkivPairs`) acts as an append-only prefix index for
range / glob queries. No external indexer process.

> **Migration status.** The repo is mid-migration from the v1
> architecture (ExEx + external `arkiv-storaged` + JSON-RPC bridge) to
> v2 (in-process precompile + custom `EvmFactory` + `ArkivPairs`).
> Phase 1 (demolition) is complete; the binary currently compiles as
> predeploy-aware op-reth with no Arkiv extensions installed. Phases
> 2–6 fill in the precompile, RPC, table, and CLI cleanup. Per-phase
> status: [`docs/architecture.md`](docs/architecture.md) §10. Full
> plan: `arkiv-op-reth-v2-migration-plan.md` (workspace root,
> deliberately not in-tree as it's a working document).

## Workspace layout

```
crates/
  arkiv-node/       # binary; v2 wiring (evm.rs, precompile/, rpc/) lands in phases 2-5
  arkiv-cli/        # operator CLI: entity ops, batches, simulate, inject-predeploy
  arkiv-genesis/    # shared lib: predeploy address, runtime bytecode, alloc helpers
chainspec/dev.base.json   # geth-format dev chainspec (no predeploy)
docs/architecture.md      # design overview — read this
docs/statedb-design.md    # canonical state model — read this if touching precompile/RPC
scripts/fixtures/         # batch JSON fixtures
justfile                  # all dev recipes
```

## Where things live (current state, post-Phase-1)

| Concern | File |
|---|---|
| CLI flags + predeploy gating | `crates/arkiv-node/src/{cli,main}.rs` |
| Predeploy detection (bytecode hash) | `crates/arkiv-node/src/genesis.rs` |
| Installer scaffold (no-op today) | `crates/arkiv-node/src/install.rs` |
| Predeploy address + bytecode generator | `crates/arkiv-genesis/src/lib.rs` |
| CLI commands + batch format | `crates/arkiv-cli/src/main.rs` |
| Traffic simulator | `crates/arkiv-cli/src/simulate.rs` |

Phase 2+ will add:

| Concern | Target file |
|---|---|
| Custom `EvmFactory` wrapping `OpEvmFactory<OpTx>` | `crates/arkiv-node/src/evm.rs` |
| `ArkivPairs` MDBX table definition + handle | new crate `crates/arkiv-db` |
| Arkiv precompile (addr, validate, gas, state, pairs, bitmap, rlp) | `crates/arkiv-node/src/precompile/` |
| `arkiv_*` RPC namespace | `crates/arkiv-node/src/rpc/` |

External: ABI types, decoders, validated types (`Ident32`, `Mime128`),
operation encoders come from `arkiv-bindings` (pinned by rev in the
root `Cargo.toml`, sourced from
[`arkiv-contracts`](https://github.com/Arkiv-Network/arkiv-contracts)).
The v2 contract is a coordinated change; rev-bumping `arkiv-bindings`
picks it up.

## Commands

Use `just` recipes. **Compile/run/network commands are long-running —
defer them to the user per the tool-usage policy and wait for output.**

Read-only / fast (fine to run yourself):

```
just genesis            # print assembled dev genesis JSON
just fmt                # rustfmt
```

Defer to the user:

```
just check              # cargo check --workspace
just build              # cargo build --workspace
just lint               # cargo clippy --workspace -- -D warnings
just node-dev           # full dev node
just simulate ...       # continuous traffic generator
just batch <fixture>    # submit a batch JSON
```

## Conventions and gotchas

- **Edition 2024**, MSRV `1.94`. Keep that in mind before reaching for
  nightly-only features.
- **`reth-*` and `reth-optimism-*` are pinned to specific git revs** in
  the root `Cargo.toml`. Bumping them is a coordinated change; expect
  API drift to surface across the `EvmFactory` / precompile integration
  once that lands.
- **No runtime mutation of state to install the predeploy.** It must be
  in `alloc` from block 0. `arkiv-cli inject-predeploy` is the
  supported path; the same chainspec file must drive both `init` and
  `node` so genesis hashes match. See [`docs/architecture.md`](docs/architecture.md) §8.
- **The runtime bytecode is chain-id-bound** (constructor immutables).
  `arkiv-genesis` re-derives it per chain id; never hardcode bytecode.
- **Predeploy detection is bytecode-equality-gated.** If you change the
  contract source, both the `arkiv-bindings` rev *and* the embedded
  reference bytecode must move together; otherwise the activation
  guard silently fails to detect the predeploy.
- **The `ArkivPairs` MDBX write is the only consensus-non-critical
  precompile output.** Everything else flows through revm's journaled
  state and lands in `stateRoot`. Do not add other direct-MDBX paths
  without revisiting the reorg / verification story.
- **Gas must be a pure function of calldata.** Two nodes executing the
  same op batch from different pre-states must charge identical gas.
  Don't introduce state-dependent gas paths.
- **No test suite in-tree yet** (only `arkiv-genesis` has unit tests).
  Phase 3+ wires up precompile / RPC / e2e tests. Don't claim
  `cargo test` covers behaviour that isn't tested.

## Working style for this repo

- Prefer matching the existing terse, comment-light Rust style.
- When touching the precompile / state model / gas, update
  [`docs/statedb-design.md`](docs/statedb-design.md) in the same
  change. That doc is the canonical spec and other repos
  (`arkiv-contracts`, downstream clients) read from it.
- When touching genesis / predeploy logic, update
  [`docs/architecture.md`](docs/architecture.md) §8 if the
  operator-facing flow changes.
- The repo is tracked with both `git` and `jj` (`.jj/` present); make
  changes via the working tree as normal.
