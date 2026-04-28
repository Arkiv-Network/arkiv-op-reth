# AGENTS.md

Orientation for coding agents working in this repo. For human-facing docs
read `README.md` and `docs/architecture.md` first.

## What this repo is

A drop-in [op-reth](https://github.com/ethereum-optimism/optimism) build
for the **Arkiv** chain plus operator tooling. Arkiv = OP-stack L2 with
one extra predeploy (`EntityRegistry` at
`0x4400000000000000000000000000000000000044`) and an Execution Extension
(ExEx) that streams decoded entity ops to a downstream Go indexer
(EntityDB) over JSON-RPC.

The binary auto-detects the predeploy in the loaded chainspec; against a
plain OP chainspec the ExEx stays inactive and it behaves as vanilla
op-reth.

## Workspace layout

```
crates/
  arkiv-node/       # op-reth binary + ExEx + storage backends (logging, jsonrpc)
  arkiv-cli/        # operator CLI: entity ops, batch, simulate, inject-predeploy
  arkiv-genesis/    # shared lib: predeploy address, runtime bytecode, alloc helpers
chainspec/dev.base.json       # geth-format dev chainspec (no predeploy)
docs/architecture.md          # primary design doc — read this
docs/exex-jsonrpc-interface-v2.md   # ExEx → EntityDB wire format
scripts/fixtures/             # batch JSON fixtures
scripts/mock-entitydb.js      # JSON-RPC mock for local dev
justfile                      # all dev recipes
```

## Where things live (quick map)

| Concern                       | File                                       |
| ----------------------------- | ------------------------------------------ |
| ExEx loop / decode / rolling hash | `crates/arkiv-node/src/exex.rs`        |
| Wire types + storage trait    | `crates/arkiv-node/src/storage/mod.rs`     |
| JSON-RPC backend              | `crates/arkiv-node/src/storage/jsonrpc.rs` |
| Logging backend               | `crates/arkiv-node/src/storage/logging.rs` |
| ExEx activation check         | `crates/arkiv-node/src/main.rs` (`has_arkiv_predeploy`) |
| CLI commands + batch format   | `crates/arkiv-cli/src/main.rs`             |
| Traffic simulator             | `crates/arkiv-cli/src/simulate.rs`         |
| Predeploy address + bytecode  | `crates/arkiv-genesis/src/lib.rs`          |

External: ABI types, decoders, storage-layout helpers come from
`arkiv-bindings` (pinned by rev in the root `Cargo.toml`, sourced from
[`arkiv-contracts`](https://github.com/Arkiv-Network/arkiv-contracts)).

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
just node-dev           # full dev node (auto-mining, logging ExEx)
just node-dev-jsonrpc   # dev node + ExEx forwarding to mock-entitydb
just mock-entitydb      # JSON-RPC mock on :9545
just simulate ...       # continuous traffic generator
just batch <fixture>    # submit a batch JSON
```

Standard verification loop after touching ExEx / wire-format code:
`just check` → `just lint` → in two terminals `just mock-entitydb` and
`just node-dev-jsonrpc` → `just batch scripts/fixtures/...` and observe
the mock's logged payloads.

## Conventions and gotchas

- **Edition 2024**, MSRV `1.94` (CI uses `1.95.0`). Keep that in mind
  before reaching for nightly-only features.
- **`reth-*` and `reth-optimism-*` are pinned to specific git revs** in
  the root `Cargo.toml`. Bumping them is a coordinated change that
  routinely requires re-resolving API drift inside `exex.rs`. Do not bump
  unless asked.
- **No runtime mutation of state to install the predeploy.** It must be
  in `alloc` from block 0. `arkiv-cli inject-predeploy` is the supported
  path; the same chainspec file must drive both `init` and `node` so
  genesis hashes match. See `docs/architecture.md` §4.
- **The runtime bytecode is chain-id-bound** (constructor immutables).
  `arkiv-genesis` re-derives it per chain id; never hardcode bytecode.
- **ExEx activation is bytecode-equality-gated.** If you change the
  contract source, both the bindings rev *and* the embedded reference
  bytecode must move together, otherwise the activation guard
  silently leaves the ExEx off on real chains.
- **Rolling changeset hash differs from the contract's
  `changeSetHashAtBlock`.** The ExEx reads parent storage and chains
  per-op; do not "simplify" it to read the contract view directly.
  Background in `docs/architecture.md` §5.2.
- **There is no test suite in-tree.** Verification today is the manual
  fixture-driven loop in `docs/architecture.md` §6. Don't claim
  `cargo test` passes — there's nothing to run. If you add tests, add
  them surgically and wire them into a `just test` recipe.

## Working style for this repo

- Follow the global behavior guidelines in `~/.pi/agent/AGENTS.md`
  (Think Before Coding · Simplicity First · Surgical Changes ·
  Goal-Driven Execution).
- Prefer matching the existing terse, comment-light Rust style. Module
  docstrings on `exex.rs` / `storage/mod.rs` are the model.
- When changing the ExEx wire format, update
  `docs/exex-jsonrpc-interface-v2.md` in the same change. The Go
  EntityDB consumes this format and is out-of-tree.
- When changing genesis / predeploy logic, update
  `docs/architecture.md` §4 if the operator-facing flow changes.
- The repo is tracked with both `git` and `jj` (`.jj/` present); make
  changes via the working tree as normal — no special VCS handling
  required.
