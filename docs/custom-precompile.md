# Custom precompiles in op-reth

How custom precompiles are wired into the Arkiv node, and the constraints
that come with them. Reflects the working POC in
`crates/arkiv-node/src/precompile.rs`, which installs an EntityDB-write
precompile at `0x0000000000000000000000000000000000aa01` when both
`--arkiv.db-url` and `--arkiv.precompile` are passed.

## Workspace pins

The wiring below references concrete types. Versions used:

| Crate | Version | Source |
|---|---|---|
| `paradigmxyz/reth` | rev `27bfddeada3953edc22759080a3659ccea62ca1f` | git |
| `ethereum-optimism/optimism` | tag `op-reth/v2.2.0` (rev `484be19`) | git |
| `revm` | 38.0.0 | crates.io |
| `revm-precompile` | 34.0.0 | crates.io (transitive) |
| `alloy-evm` | 0.33.x | crates.io |
| `alloy-op-evm` | 0.31.0 | git, op-reth tag |
| `op-revm` | 19.0.0 | git, op-reth tag |

Local cache paths (substitute these in the file references below):

```
~/.cargo/git/checkouts/reth-e231042ee7db3fb7/27bfdde/
~/.cargo/git/checkouts/optimism-852bcbde357560e3/484be19/rust/
~/.cargo/registry/src/index.crates.io-*/{alloy-evm,revm-precompile,…}/
```

Several APIs below have churned over recent versions (especially
`revm-precompile` 32 → 34, which reworked the error/halt model). On
every reth/op-reth bump, re-check **§3** (precompile return shape) and
the line numbers cited in **§5** before assuming the doc still applies.

---

## 1. Mental model

A precompile is a fixed-address handler the EVM consults instead of
running EVM bytecode. It receives the call's calldata, gas, caller, and
value, and returns either output bytes (with gas accounting) or a halt
status. It is part of the state-transition function — every node must
agree on its presence and behaviour or the chain forks.

Adding one to op-reth means inserting an entry into the
`PrecompilesMap` of every fresh `OpEvm` instance the node creates.
That requires reaching three layers up the stack:

1. **Write the precompile** — a closure (or `Precompile` impl) producing
   `PrecompileResult` from `PrecompileInput`.
2. **Install it in every EVM** — wrap `OpEvmFactory` so that
   `create_evm` and `create_evm_with_inspector` both call
   `evm.precompiles_mut().apply_precompile(...)` after the default
   set has been loaded.
3. **Plumb the wrapped factory through the node builder** — replace
   `OpExecutorBuilder` (the component slot that constructs `OpEvmConfig`)
   with a custom one that uses our factory, then put the result behind
   a wrapper `Node` impl so the AddOns are typed against the customised
   components.

The third step is where most of the friction lives; **§5** documents the
specific patterns that make it compile.

## 2. Layering

Four crates, low to high:

1. **`revm-precompile`** — wire types: `PrecompileResult`,
   `PrecompileOutput`, `PrecompileError`, `PrecompileHalt`,
   `PrecompileId`, `PrecompileFn`.
2. **`alloy-evm`** — higher-level `Precompile` trait,
   `PrecompileInput`, `PrecompilesMap`, `DynPrecompile`, and the
   `EvmFactory` trait. This is the registration layer.
3. **`alloy-op-evm` + `reth-optimism-evm`** — OP-specific glue:
   `OpEvm`, `OpEvmFactory`, `OpEvmConfig`, `OpBlockExecutorFactory`,
   `OpBlockAssembler`, `OpRethReceiptBuilder`, `OpTx`. The node passes
   `OpEvmConfig` around as `ConfigureEvm`.
4. **`reth-node-builder` + `reth-optimism-node`** — `Node` /
   `ExecutorBuilder` traits and `OpNode` itself. This is where the
   custom executor builder gets installed.

The reference for the entire pattern is upstream
`reth/examples/precompile-cache/src/main.rs` (a wrap-every-precompile
cache rather than a new precompile, but it touches every relevant API).
For the specific OP-stack interactions covered in §5,
`op-reth/examples/custom-node/src/evm/` is the closest analogue.

---

## 3. The precompile API (`alloy-evm 0.33` / `revm-precompile 34.0.0`)

### 3.1 Trait

```rust
pub trait Precompile {
    fn precompile_id(&self) -> &PrecompileId;
    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult;
    fn supports_caching(&self) -> bool { true }
}
```

`supports_caching = false` (`DynPrecompile::new_stateful`) tells the
engine that results are not a pure function of `(data, gas)` and must
not be memoised. Use this for any precompile whose return depends on
state outside `PrecompileInput`.

### 3.2 Input

```rust
pub struct PrecompileInput<'a> {
    pub data: &'a [u8],          // calldata
    pub gas: u64,                // gas limit available
    pub reservoir: u64,          // EIP-8037 state-gas reservoir; 0 on mainnet today
    pub caller: Address,         // msg.sender
    pub value: U256,             // call value
    pub target_address: Address, // address being called
    pub is_static: bool,         // STATICCALL?
    pub bytecode_address: Address,
    pub internals: EvmInternals<'a>, // hooks back into journaled EVM state
}
```

`PrecompileInput::is_direct_call()` returns `true` when not invoked via
`DELEGATECALL` / `CALLCODE`; useful for refusing indirect calls.

### 3.3 Return

```rust
pub type PrecompileResult = Result<PrecompileOutput, PrecompileError>;

pub struct PrecompileOutput {
    pub status: PrecompileStatus,        // Success | Revert | Halt(PrecompileHalt)
    pub gas_used: u64,
    pub gas_refunded: i64,
    pub state_gas_used: u64,             // EIP-8037
    pub reservoir: u64,                  // EIP-8037; passthrough of input.reservoir
    pub bytes: Bytes,
}

pub enum PrecompileError { Fatal(String), FatalAny(AnyError) }

pub enum PrecompileHalt {
    OutOfGas,
    // … many crypto-precompile-specific halts …
    Other(Cow<'static, str>),
}
```

Three concrete return paths:

- **Success.** `Ok(PrecompileOutput::new(gas_used, bytes, input.reservoir))`.
- **Out-of-gas / soft halt.**
  `Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, input.reservoir))`
  — the EVM consumes all available gas for the call frame and reverts
  the call's state changes, but the surrounding transaction continues.
  This is the pattern for "couldn't afford the work" — `PrecompileError`
  has no `OutOfGas` variant any more.
- **Revert with return data.** `Ok(PrecompileOutput::revert(gas_used, bytes, input.reservoir))`.
- **Fatal.** `Err(PrecompileError::Fatal(msg))` aborts the entire
  transaction. Reserved for things the EVM cannot recover from
  (precompile panicked; unrecoverable I/O on a path that is ostensibly
  consensus-deterministic; etc.).

**Gas accounting is the precompile's responsibility.** Nothing checks
that `gas_used ≤ input.gas` for you in the trait contract; check it
explicitly and return an OOG halt if you can't afford the work.

### 3.4 `PrecompileId`

Enum, one variant per stdlib precompile plus
`Custom(Cow<'static, str>)`. For new precompiles use
`PrecompileId::custom("ARKIV_<NAME>")`. The string is informational
(tracing, EIP-7910 introspection); it is not the precompile's address.
Pick stable, namespaced strings — they end up in client-facing tooling.

### 3.5 `DynPrecompile`

`PrecompilesMap` stores `DynPrecompile`, which is
`Arc<dyn Precompile + Send + Sync>`. Three convenient constructors:

```rust
// 1. From a closure.
let p: DynPrecompile = (
    PrecompileId::custom("ARKIV_X"),
    |input: PrecompileInput<'_>| -> PrecompileResult { /* ... */ },
).into();

// 2. From a struct that implements Precompile.
let p = DynPrecompile::new(id, my_struct);
let p = DynPrecompile::new_stateful(id, my_struct); // disables caching
```

The closure form is what the POC uses — it captures the
`Arc<EntityDbClient>` and is otherwise stateless from the trait's
perspective.

---

## 4. `PrecompilesMap` — registration mechanics

`PrecompilesMap` is the mutable container of precompiles attached to a
specific EVM instance. Source:
`alloy-evm-0.33.x/src/precompiles.rs`. Methods that matter:

| Method | Purpose |
|---|---|
| `from_static(&'static Precompiles)` | Initial population from a hardfork's static set. |
| `apply_precompile(&Address, FnOnce(Option<DynPrecompile>) -> Option<DynPrecompile>)` | Insert / replace / remove at one address. Returning `None` removes; `Some(p)` installs. |
| `with_applied_precompile(...)` | Builder-style version (consuming). |
| `map_precompile(&Address, F)` / `map_precompiles(F)` | Transform existing precompile(s) in place; this is what the cache example uses. |
| `extend_precompiles(I)` | Bulk insert; replaces on collision. |
| `move_precompiles(I)` | Relocate by `(src, dst)` pairs; errors if `src` isn't a precompile. |
| `set_precompile_lookup(L)` | Install a fallback resolver. Cold-access penalty applies; static entries take priority. |

Behavioural notes:

- `set_precompile_lookup` is invoked on **every** precompile check for
  unregistered addresses. It must be cheap, and addresses it returns
  are always treated as cold. For a fixed-address precompile, prefer
  `apply_precompile` — the address gets warmed by the standard rules.
- `extend_precompiles` *replaces* on collision; to wrap or extend an
  existing precompile use `map_precompile` / `apply_precompile`.

The POC's call site is a one-liner inside the wrapped EvmFactory:

```rust
evm.precompiles_mut().apply_precompile(&ARKIV_PRECOMPILE_ADDRESS, |_existing| {
    Some(precompile)
});
```

Choice of address:

- Outside the reserved precompile range (`0x01..=0x11`).
- Distinct from any predeploy in `chain.inner.genesis.alloc`. Arkiv
  reserves `0x4400000000000000000000000000000000000044` for
  `EntityRegistry`; the POC uses `0x00…00aa01` per high-address
  convention.

---

## 5. op-reth wiring

The OP stack mirrors the Ethereum reference example with its own
types:

| Concept | OP type |
|---|---|
| Spec id | `OpSpecId` |
| Default precompiles | `OpPrecompiles` |
| EVM | `OpEvm` |
| EVM factory | `OpEvmFactory<Tx = OpTx>` |
| EVM config | `OpEvmConfig<ChainSpec, N, R, EvmFactory>` |
| Default executor builder | `OpExecutorBuilder` |
| Tx env | `OpTransaction<TxEnv>` (wrapped as `OpTx`) |
| Halt reason | `OpHaltReason` |
| Tx error | `EVMError<DBErr, OpTxError>` |

Wiring breaks into four pieces, each with a non-obvious wrinkle:

1. The custom `EvmFactory` (§5.1).
2. A local newtype around `OpEvmConfig` to dodge the orphan rule on
   `ConfigureEngineEvm` (§5.2).
3. A local `Node` impl so the AddOns are typed against the customised
   components (§5.3).
4. A `DebugNode` impl so `--dev` mining still works (§5.4).

All concrete code below is in `crates/arkiv-node/src/precompile.rs`.

### 5.1 Custom `EvmFactory`

Wrap `OpEvmFactory<OpTx>` and install the precompile after the default
set is loaded. Both code paths must apply the same mutation — forgetting
`create_evm_with_inspector` silently breaks `debug_traceTransaction` and
similar simulation/tracing endpoints.

```rust
pub struct ArkivOpEvmFactory {
    inner: OpEvmFactory<OpTx>,
    client: Option<Arc<EntityDbClient>>,
}

impl EvmFactory for ArkivOpEvmFactory {
    // Spell associated types concretely; do NOT forward through
    // `<OpEvmFactory<OpTx> as EvmFactory>::X` projections — see §6.4.
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> =
        OpEvm<DB, I, PrecompilesMap, OpTx>;
    type Context<DB: Database> = OpEvmContext<DB>;
    type Tx = OpTx;
    type Error<DBError: ...> = EVMError<DBError, OpTxError>;
    type HaltReason = OpHaltReason;
    type Spec = OpSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(&self, db: DB, input: EvmEnv<...>)
        -> Self::Evm<DB, NoOpInspector>
    {
        let mut evm = self.inner.create_evm(db, input);
        self.install(&mut evm);
        evm
    }

    fn create_evm_with_inspector<DB, I>(&self, db: DB, input: EvmEnv<...>, inspector: I)
        -> Self::Evm<DB, I>
    {
        let mut evm = self.inner.create_evm_with_inspector(db, input, inspector);
        self.install(&mut evm);
        evm
    }
}
```

`install` calls `apply_precompile` only when a client is configured;
when `client = None` the wrapper is a transparent passthrough over the
default OP factory.

### 5.2 Why we need a local newtype for `OpEvmConfig`

`OpEvmConfig` is generic over the factory:

```rust
pub struct OpEvmConfig<
    ChainSpec = OpChainSpec,
    N = OpPrimitives,
    R = OpRethReceiptBuilder,
    EvmFactory = OpEvmFactory<OpTx>,
> { … }
```

The blanket `ConfigureEvm` impl
(`op-reth/crates/evm/src/lib.rs:129`) is fully generic over all four,
so a custom-factory `OpEvmConfig` is a valid `ConfigureEvm`. **But two
other impls op-reth needs in the launcher path are gated to the
3-generic form** (i.e. defaulted factory):

```rust
// op-reth/crates/evm/src/lib.rs:215
impl<ChainSpec, N, R> ConfigureEngineEvm<OpExecutionData>
    for OpEvmConfig<ChainSpec, N, R> { … }

// op-reth/crates/payload/src/lib.rs:35
impl<ChainSpec, N, R> ConfigureEngineEvm<OpExecData>
    for OpEvmConfig<ChainSpec, N, R>
where Self: ConfigureEngineEvm<OpExecutionData>, … { … }
```

`ConfigureEngineEvm<OpExecData>` is required transitively by
`BasicEngineValidatorBuilder: EngineValidatorBuilder<N>` (the default
`EVB` slot in `OpAddOns`), which is itself required by
`OpAddOns: NodeAddOns<N>` and `RethRpcAddOns<N>`. Without it,
`with_add_ons(…)` and `launch*()` reject the builder. The diagnostic is
a deeply nested trait-bound error whose only useful hint is
`expected `OpEvmConfig<_>`, found `…`` — misleading; the *real* missing
piece is `ConfigureEngineEvm`, not `OpEvmConfig` itself.

Adding `impl ConfigureEngineEvm<OpExecData> for OpEvmConfig<…, ArkivOpEvmFactory>`
directly hits the orphan rule (E0117): both the trait and the type are
foreign; the only local type (`ArkivOpEvmFactory`) is buried as a
generic argument inside `OpEvmConfig`, which Rust does not count as
"local at the head".

The fix is a local newtype that wraps `OpEvmConfig` and also stores a
default-factory `OpEvmConfig` to delegate the engine-API methods to:

```rust
pub struct ArkivOpEvmConfig {
    inner: OpEvmConfig<OpChainSpec, OpPrimitives, OpRethReceiptBuilder, ArkivOpEvmFactory>,
    /// Default-factory `OpEvmConfig` sharing the same chain spec.
    /// Used by the `ConfigureEngineEvm` shim, whose upstream impl body
    /// does not actually depend on the EVM factory.
    inner_default: OpEvmConfig<OpChainSpec, OpPrimitives, OpRethReceiptBuilder>,
}
```

- `ConfigureEvm` is a passthrough to `inner` (concrete associated types
  — see §6.4 — so downstream bound checks normalise).
- `ConfigureEngineEvm<OpExecData>` delegates to `inner_default`.
- `inner_default` is **stored as a field**, not constructed on the fly,
  because `tx_iterator_for_payload` returns
  `impl ExecutableTxIterator<Self>` and the compiler treats the iterator
  as `'static`-bounded; a temporary `OpEvmConfig` would be dropped at
  end-of-statement.

### 5.3 Why we need a local `Node` impl

`OpNode::add_ons() -> Self::AddOns` returns
`OpAddOns<NodeAdapter<N, <OpNode::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>, …>`.
The `Components` here are derived from `OpNode::ComponentsBuilder`,
which hardcodes `OpExecutorBuilder` and therefore the default
`OpEvmConfig`. If you go

```rust
let nb = builder
    .with_types::<OpNode>()
    .with_components(op_node.components().executor(ArkivOpExecutorBuilder::new(client)))
    .with_add_ons(op_node.add_ons());
```

the `N` in the AddOns no longer matches the `N` the customised
components produce, and `with_add_ons` rejects the builder. This is the
second cause of the same `expected OpEvmConfig<_>, found …` error.

The fix mirrors `op-reth/examples/custom-node`: define a local `Node`
impl that wraps `OpNode` and overrides `components_builder()` to swap
the executor and `add_ons()` to call `inner.add_ons_builder().build()`
(the latter is generic over `N`, so it'll be parameterised over the
actual customised components).

```rust
pub struct ArkivOpNode {
    inner: OpNode,
    precompile_client: Option<Arc<EntityDbClient>>,
}

impl NodeTypes for ArkivOpNode {
    type Primitives = OpPrimitives;
    type ChainSpec = OpChainSpec;
    type Storage = OpStorage;
    type Payload = OpEngineTypes;
}

impl<N: FullNodeTypes<Types = Self>> Node<N> for ArkivOpNode {
    type ComponentsBuilder = ComponentsBuilder<
        N, OpPoolBuilder, BasicPayloadServiceBuilder<OpPayloadBuilder>,
        OpNetworkBuilder, ArkivOpExecutorBuilder, OpConsensusBuilder,
    >;
    type AddOns = OpAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
        OpEthApiBuilder, OpEngineValidatorBuilder,
        OpEngineApiBuilder<OpEngineValidatorBuilder>,
        BasicEngineValidatorBuilder<OpEngineValidatorBuilder>,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        self.inner
            .components()
            .executor(ArkivOpExecutorBuilder::new(self.precompile_client.clone()))
    }

    fn add_ons(&self) -> Self::AddOns { self.inner.add_ons_builder().build() }
}
```

`OpFullNodeTypes`, `OpNodeTypes`, and `NodeTypesForProvider` are all
blanket-impl'd, so as long as our `NodeTypes` associated types match
`OpNode`'s (they do — same `Primitives`, `ChainSpec`, `Storage`,
`Payload`) we get them for free.

The entrypoint in `main.rs` becomes:

```rust
let arkiv_node = ArkivOpNode::new(OpNode::new(ext.rollup), pc_client);
let node = install(builder.node(arkiv_node), mode);
let handle = node.launch_with_debug_capabilities().await?;
```

i.e. the original shorthand survives, just with `ArkivOpNode` in place
of `OpNode`.

### 5.4 `DebugNode` for `--dev` mining

`launch_with_debug_capabilities()` requires
`<T::Types>: DebugNode<NodeAdapter<T, CB::Components>>`, which provides
`local_payload_attributes_builder()` for the `--dev` LocalMiner. For
`ArkivOpNode` this is two methods:

- `rpc_to_primitive_block` — one-liner (`rpc_block.into_consensus()`).
- `local_payload_attributes_builder` — must return
  `impl PayloadAttributesBuilder<OpPayloadAttrs>`.

`OpNode`'s impl returns `OpLocalPayloadAttributesBuilder`, which is
**private to `op-reth/crates/node/src/node.rs`** and we cannot reuse.
The POC copies the body verbatim into a local
`ArkivLocalPayloadAttributesBuilder`. It pulls in deps for the types
it constructs: `alloy-eips` (for `BaseFeeParams::optimism()`),
`alloy-hardforks` (for `EthereumHardforks` — see §6.5),
`alloy-rpc-types-engine`, `op-alloy-consensus`.

When bumping `op-reth/v2.2.0`, diff the upstream
`OpLocalPayloadAttributesBuilder` against ours. Things to look for:

- The hard-coded `TX_SET_L1_BLOCK` deposit transaction (synthetic
  `setL1BlockValuesEcotone` call, required so dev blocks pass OP's
  "first tx must be a deposit" rule). If the deposit ABI ever
  changes, this constant must change with it.
- The `OP_DEV_EIP1559_DENOMINATOR` / `OP_DEV_EIP1559_ELASTICITY` /
  `OP_DEV_GAS_LIMIT` env-var hooks.
- Hardfork checks.

If you don't need `--dev` mining, `launch()` (using `EngineNodeLauncher`
directly) drops the `DebugNode` requirement entirely — but it silently
swallows the `--dev` flag, which is worse than the copy-paste cost.

### 5.5 The custom `ExecutorBuilder`

The piece that actually constructs `ArkivOpEvmConfig` from a
`BuilderContext`:

```rust
pub struct ArkivOpExecutorBuilder { client: Option<Arc<EntityDbClient>> }

impl<N> ExecutorBuilder<N> for ArkivOpExecutorBuilder
where
    N: FullNodeTypes<Types: NodeTypes<ChainSpec = OpChainSpec, Primitives = OpPrimitives>>,
{
    type EVM = ArkivOpEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<N>) -> eyre::Result<Self::EVM> {
        Ok(ArkivOpEvmConfig::new(ctx.chain_spec(), self.client))
    }
}
```

Bounded on a concrete `OpChainSpec` rather than `OpHardforks` because
`ArkivOpEvmConfig` itself is concretely typed against `OpChainSpec`;
generality here would only complicate the wrapper without buying
anything.

---

## 6. Constraints and gotchas

### 6.1 Consensus and determinism

A precompile is part of the state-transition function. Every node must
agree on its presence and behaviour, or the chain forks. There is no
"opt-in per node" mode — if it's compiled into the canonical binary, it
must be on for everyone, or activation must be gated by chain id /
timestamp inside `create_evm`.

The stricter requirement is on the call itself: `Precompile::call` must
be a **pure function of `PrecompileInput` and any journaled EVM state**
visible via `EvmInternals`. The EVM relies on this to compute a single
deterministic state root after each block. If two honest nodes given
the same input compute different precompile outputs, they produce
different receipts, different state roots, and the chain forks at that
block. The usual violators — wall-clock time, RNG, non-deterministic
iteration over hash maps, **and any I/O to anything outside the EVM** —
are unsafe for exactly this reason.

#### What the POC does, and why it is not a normal precompile

The POC's precompile makes a synchronous HTTP/JSON-RPC call to an
EntityDB instance via the `EntityDbClient` already used by the ExEx
(§6.6 has the runtime mechanics). That call leaves the EVM sandbox.
Mechanically, this is the canonical example of "do not do this in a
precompile" — there is nothing in the precompile contract that says the
remote endpoint will exist, respond on time, return the same bytes on
every node, or even be the same physical server twice in a row.

The POC ships anyway because the project's working assumption is that
**EntityDB is treated as part of the protocol, not an external service
the node happens to talk to**. The mental model is: every Arkiv node
runs its own EntityDB co-located with the execution client; EntityDB
itself is a deterministic state machine whose state is a pure function
of the consensus-determined sequence of writes (precompile calls today,
plus the existing ExEx-driven event stream); and the call from the
precompile is an in-process commit, not a network query against
arbitrary infrastructure. Under that model the precompile output is
still a deterministic function of consensus inputs — it just happens to
be computed by a sibling component instead of inline EVM code.

That assumption is the load-bearing one. If it breaks, the chain forks.

#### Empirical: a single tx fires the precompile multiple times

Running `precompiles/run.sh` against the local mock surfaces this
immediately. One `cast send` invocation of `callPrecompile(0xdeadbeef)`
produces a sequence of byte-identical `arkiv_precompileWrite` requests
on the mock — same caller, same calldata, same value, typically six
in a row, with only the JSON-RPC envelope `id` advancing:

```json
{ "id": 77, "jsonrpc": "2.0", "method": "arkiv_precompileWrite",
  "params": [{ "caller": "0x9fe4…6e0", "data": "0xdeadbeef", "value": "0x0" }] }
{ "id": 78, …same params… }
{ "id": 79, …same params… }
…
{ "id": 82, …same params… }
```

The transaction is one user-level send, but the EVM executes it
repeatedly across the node's lifecycle. The paths, in roughly the
order reth invokes them:

| # | Path | Where |
|---|---|---|
| 1 | `eth_estimateGas` | Cast probes for a gas limit before signing. |
| 2 | Mempool admission | `OpTransactionValidator` partial-executes the tx to validate. |
| 3 | Pending-block / state computation | `OpEvmConfig` builds pending state for queries that follow. |
| 4 | Block payload building | `OpPayloadBuilder` runs the tx while assembling the next block. |
| 5 | Canonical block execution | The block executor produces the canonical state diff. |
| 6 | Engine API validation | `BasicEngineValidatorBuilder`'s validator re-executes during the forkchoice update. |

All six paths go through `EvmFactory::create_evm` (or
`create_evm_with_inspector`); `ArkivOpEvmFactory` installs the
precompile in every `OpEvm` it produces, so each path independently
fires the JSON-RPC call.

This is the textbook "side-effecting precompile" hazard, made
concrete: **external side effects multiply by execution count, not
by transaction count.** With the current mock-entitydb response
(constant `0x000…0`) the duplication is harmless. The moment
EntityDB's response or stored state depends on prior writes the
duplication becomes consensus-affecting on a single node — before
two nodes even have a chance to disagree — and the chain forks.

##### This is non-negotiable from the precompile side

The instinct is to read this as a bug to fix in the precompile
layer — track "have we executed this tx already?", short-circuit on
re-entry, etc. **It is not.** Repeated execution of the same
transaction (and the same block) is a load-bearing property of how
reth — and every EVM client — operates:

- gas estimation is by definition speculative execution against
  pending state;
- mempool admission cannot trust user-supplied gas limits without
  partially executing the tx;
- payload building has to run the tx to learn its receipt and the
  resulting state root;
- the engine API re-executes the canonical block to validate the
  payload it was just asked to import;
- historical replay (`debug_traceBlock`, snapshot regeneration,
  full resync from genesis) re-runs every block, often many times
  in a node's lifetime.

A precompile sits inside `EvmFactory::create_evm` and cannot tell
these callers apart — and even if it could, suppressing the side
effect on any of them would break the path that needs it. Gas
estimation that skips the precompile reports the wrong gas;
validation that skips the precompile accepts blocks that canonical
execution rejects. The EVM has to be free to invoke the precompile
as many times as it wants, with no awareness on the precompile's
part of which invocation this one is.

The corollary is that **a side-effecting precompile cannot be made
once-only from inside the precompile**. Whatever mitigation exists
has to live downstream of the precompile, in the component that
actually accepts the side effect — i.e. EntityDB.

##### Whether the DB can absorb this is an open question

In principle EntityDB could be made idempotent under repeated
identical writes: compute a deterministic key from the request and
treat duplicate keys as a no-op rather than as an append. In
practice this hits two structural problems with no obviously clean
answer:

1. **Distinguishing replays from legitimate identical calls.** Two
   transactions, in two different blocks, that both call
   `precompileWrite(0xdeadbeef)` with the same caller and value
   are logically distinct user events. A payload-hash key collapses
   them into one. To dedupe re-executions while preserving distinct
   user calls the key has to include something the EVM ties to a
   specific call-site within a specific transaction within a
   specific block — tx hash + call-frame index + intra-call
   counter, say. None of those are exposed on `PrecompileInput`,
   and the engine API's pre-canonicalisation re-execution operates
   on a tx hash that is not yet final.

2. **Reorg-aware retraction.** Re-execution along the canonical
   path is one thing; what happens when the chain reorgs and the
   tx gets unwound is another. Anything the DB committed
   speculatively during pending-block / payload-building now
   refers to a transaction that no longer exists, and the DB has
   to roll it back atomically with the EVM state. The ExEx already
   carries `arkiv_revert` / `arkiv_reorg` for exactly this reason,
   and they are non-trivial.

Whether a workable design exists at the intersection of these two
constraints is genuinely open. It may turn out that the DB cannot
plausibly disambiguate a re-execution from a legitimate identical
user call without changes to the EVM-side API surface that are out
of scope for this project — in which case side-effecting precompile
writes are not a viable model and the project has to fall back to
something else (the ExEx-driven event stream, which runs exactly
once per canonical block by construction, is the obvious candidate).

##### Could the precompile flag canonical execution itself?

Natural follow-up: can the precompile distinguish "this is the
canonical block executor" from "this is gas estimation / payload
building / engine-API validation", and only write in the canonical
case? Mechanisms exist; none are clean.

- **Static factory swap.** One `OpEvmConfig` for block execution,
  another for everything else. Reth's node builder exposes a single
  `ConfigureEvm` slot; routing to two requires upstream cooperation
  or a wrapper that internally dispatches on context it doesn't
  have.
- **DB type discrimination.** Sniff the `DB` handed to
  `create_evm` — the canonical executor passes a particular
  `State<…>`, gas estimation a `CacheDB<…>`, etc. Requires `Any`
  downcasts against reth-internal types that change across
  versions, and "canonical execution" isn't a single DB type — it
  covers the executor, the engine-API validator, and reorg replay.
  Pins consensus correctness to reth's internal type churn.
- **`BlockEnv` discrimination.** The factory sees the block
  header. Doesn't help: gas estimation, payload building, and
  canonical execution all target the same block number.
- **Thread-local / atomic flag** set at the executor's entry.
  Wrong on every axis — re-execution paths set and clear it
  differently, concurrent payload-build vs engine-validate breaks
  the invariant, and the precompile becomes a function of
  out-of-band state by construction.

The deeper objection is the one already named: any of these makes
the precompile **context-sensitive** — its behaviour varies with a
hidden input the EVM doesn't model. The EVM's contract with a
precompile is "deterministic function of input"; a precompile that
sometimes writes and sometimes doesn't is in violation of that
contract even when the EVM-visible return value is identical across
paths.

The one escape hatch: if the EVM-visible return is fully determined
by `(data, caller, value, journaled state)` and the DB write is
purely ornamental — nothing the EVM observes ever depends on
whether the write happened — then varying the side effect across
paths is consistent with the contract. But once the write is
logically detached from the return value, the right place to do it
is the existing ExEx, which already fires once per canonical
commit, is reorg-aware, and sits outside the consensus surface.
Reproducing that pattern from inside a precompile via
canonical-execution discrimination buys nothing and adds coupling
to reth-internal execution-path identity, a fresh consensus
surface, and two side-effect channels for one logical event.

The shape that survives this analysis is a precompile that is
**pure** (or at most a read against committed state via
`EvmInternals`), with the write emitted as an event the ExEx
consumes — the model the existing `arkiv` ExEx already implements
for `EntityRegistry`.

The POC ships with the duplicates visible in the mock and makes no
attempt to suppress them; its job is to prove the wiring, not to
commit to a design.

#### Concrete divergence vectors to keep in mind

Things that would silently break the assumption and cause a fork, in
rough order of how easy it is to trip them by accident:

1. **Different nodes pointing at different EntityDBs.** A misconfigured
   `--arkiv.db-url` aimed at a shared service, or a stale snapshot on
   one node, means two nodes compute against different state. The
   precompile returns different `stateRoot` values; receipts diverge
   on the next block.
2. **Asynchrony between the EVM and EntityDB commits.** If the call
   is fire-and-forget on one side and synchronous on another, or if
   EntityDB lags by a block, the same precompile invocation sees
   different state at different sites. Today the call is synchronous
   on the EVM side; the determinism budget assumes EntityDB commits
   the write before responding and that the response is
   byte-deterministic.
3. **Non-determinism inside EntityDB.** Floating-point arithmetic,
   hash-map iteration order, OS-time-based logic, RNG, parallel
   reduce of mutable state, anything that lets two identical inputs
   produce different outputs. Out-of-tree concern, but every change
   inside EntityDB is now a potential consensus change.
4. **Transport availability skew.** Network failure on one node and
   not another — the precompile returns `Err(Fatal)` on the failing
   node and `Ok(…)` elsewhere. This is a fork at the receipt level
   even if EntityDB itself is perfectly deterministic.
5. **Re-execution / replay paths.** Block replay, `debug_traceBlock`,
   trie re-derivation — anything that re-runs a historical block must
   reach the same EntityDB state it had at the original execution.
   If EntityDB's history isn't pinned to the chain history (rolling
   forward only, with no reorg-aware revert), historical replay
   produces different bytes than canonical execution.

#### What the POC does not yet do, but production needs

- **In-process embedding.** Today EntityDB is a separate process
  reached over local HTTP. The transport-skew vector goes away if
  EntityDB is linked into the node binary and committed atomically
  with the EVM state.
- **Anchor commitments in EVM state.** Have the precompile read an
  EVM-side commitment (e.g. an `EntityRegistry` storage slot) and
  refuse calls whose response doesn't match. Divergence becomes a
  deterministic revert rather than a silent fork.
- **Reorg-aware EntityDB writes.** EntityDB needs to revert / replay
  in lockstep with the EVM, not just append; the existing ExEx already
  has `arkiv_revert` / `arkiv_reorg` for this, and the precompile path
  needs to participate in the same model.
- **Hardfork gate plus shadow-mode canary.** Activate the precompile
  behind a timestamp, run nodes with it disabled-but-log-only first,
  and only flip the gate once divergence has been observed to be
  zero across a non-trivial period.

None of these are in scope for the POC. The module docs in
`crates/arkiv-node/src/precompile.rs` carry the short-form caveat in
code.

### 6.2 Gas pricing

Gas cost has to be high enough that an adversary can't DoS the chain
with calls. There is no mempool-side filter for "this transaction calls
precompile X N times"; the only knob is the per-call gas charge.

The POC charges a flat 5,000 gas, which is an order-of-magnitude guess,
not a calibrated number. Anything that lands in production needs a
proper analysis against the work the precompile actually performs
(network round-trip, EntityDB-side state mutation, etc.).

### 6.3 Tracing parity

`create_evm` and `create_evm_with_inspector` must apply the same
precompile mutations. Forgetting one means simulation/tracing endpoints
(`debug_traceTransaction`, `eth_call` under inspectors, etc.) see a
different precompile set than block execution, which silently
desynchronises trace output from canonical state.

### 6.4 Concrete associated types beat projection forwarding

Forwarding associated types through projections compiles but breaks
downstream bound checks:

```rust
// Compiles, but downstream `Self::Tx: FromRecoveredTx<…>` bounds fail
// because Rust does not always normalise nested projections through
// trait bounds.
type Tx = <OpEvmFactory<OpTx> as EvmFactory>::Tx;
```

Always spell concrete types in `EvmFactory` and `ConfigureEvm` impls
(`type Tx = OpTx;`, `type Spec = OpSpecId;`, etc.). The POC does this
in both `ArkivOpEvmFactory` and `ArkivOpEvmConfig`.

### 6.5 `EthereumHardforks` supertrait import

`OpHardforks: EthereumHardforks`, and `OpChainSpec: OpHardforks`. So
`OpChainSpec` does have `is_shanghai_active_at_timestamp` etc. — but
Rust requires the trait that *defines* a method to be in scope to call
it, not just a subtrait. The dev-payload-attributes builder needs:

```rust
use alloy_hardforks::EthereumHardforks; // even though OpHardforks would seem to cover it
```

### 6.6 Sync HTTP from inside a precompile (POC choice)

The POC's precompile reuses `EntityDbClient::rpc_call`, which uses
`tokio::task::block_in_place(|| Handle::current().block_on(…))` to make
a sync call inside an async runtime. This works only when:

- the current task runs on a multi-threaded tokio runtime worker, **and**
- the call is *not* nested inside `spawn_blocking` (which is already
  off-runtime; `block_in_place` panics there).

Reth's block executor satisfies both today; the same trick is used by
the ExEx's `JsonRpcStore::handle_commit`. If a future reth refactor
moves block execution off the multi-threaded runtime (or into
`spawn_blocking`), the precompile will start panicking under load with
little warning. Worth keeping on the radar; an integration test that
actually exercises the precompile would catch a regression early.

### 6.7 `OpPayloadBuilder` name collision

`reth_optimism_node` has two unrelated `OpPayloadBuilder` types:

- `reth_optimism_node::OpPayloadBuilder` — re-exported at the crate
  root from `reth_optimism_payload_builder`; takes 3+ generics
  (`OpPayloadBuilder<Pool, Client, Evm, …>`).
- `reth_optimism_node::node::OpPayloadBuilder` — the 1-generic
  `OpPayloadBuilder<Txs = ()>` defined in `node.rs`, used by
  `OpNode::ComponentsBuilder`.

The crate-root re-export shadows the `pub use node::*`. For
component-builder use, always import via
`reth_optimism_node::node::OpPayloadBuilder`. The error if you get this
wrong is "missing generics".

### 6.8 `EntityDbClient: Debug`

`OpEvmConfig`'s `ConfigureEvm` impl bound includes `EvmF: Debug`; that
propagates to `Option<Arc<EntityDbClient>>: Debug` and hence
`EntityDbClient: Debug`. The POC adds `#[derive(Debug)]` to
`EntityDbClient` for this reason. Worth flagging because the diagnostic
chain is indirect.

### 6.9 Genesis collisions

A precompile address must not appear in `chain.inner.genesis.alloc` for
any Arkiv chainspec. Arkiv reserves `0x44…0044` for `EntityRegistry`;
the POC uses `0x00…00aa01`. If you add a new chainspec, audit its alloc
against any precompile addresses you've registered.

### 6.10 EIP-7910 introspection

If precompiles are ever surfaced via an introspection RPC, the
`PrecompileId::custom("ARKIV_<NAME>")` string is what's exposed. Pick
stable, namespaced strings — they end up in client tooling.

### 6.11 Hardfork gating

The POC is always-on whenever both flags are set. To gate by hardfork
instead, branch on `input.cfg_env.spec` (`OpSpecId`) inside
`create_evm` before the `apply_precompile` call. `OpHardforks` provides
fork activation predicates if the gate needs to be timestamp-based.

### 6.12 Read-only state access via `EvmInternals`

`PrecompileInput::internals` exposes hooks back into the EVM journal:
account balances, code, storage. If a precompile needs to read chain
state, this is how. Writes are technically possible but strongly
discouraged — they make gas accounting and reverts subtle. If the
precompile would write state, consider whether the right answer is
actually a normal contract.

### 6.13 Precompile vs ExEx

Use a precompile when EVM bytecode needs to call into native logic
during execution and observe the return value within the same
transaction. For after-the-fact side effects — what the existing
`arkiv` ExEx does for `EntityRegistry` events — an ExEx is the right
tool: it doesn't touch consensus and can't fork the chain.

---

## 7. Diagnostic guide

When a precompile-related change fails to compile, two error patterns
account for most of the noise.

### "expected `OpEvmConfig<_>`, found `…`"

The hint is the *closest existing impl*, not literally what's required.
The actual missing constraint is several layers up the bound chain.
Walk it like this:

1. `OpAddOns: NodeAddOns<N>` — bounds at
   `op-reth/crates/node/src/node.rs:594`. The `EVB` slot's
   `EngineValidatorBuilder<N>` requirement is the most common culprit.
2. `BasicEngineValidatorBuilder<EV>: EngineValidatorBuilder<Node>` —
   bounds at `reth/crates/node/builder/src/rpc.rs`. Requires
   `Node::Evm: ConfigureEngineEvm<<Types::Payload as PayloadTypes>::ExecutionData>`.
3. For `OpEngineTypes`, `ExecutionData = OpExecData`. Verify your EVM
   config impls `ConfigureEngineEvm<OpExecData>` directly.

If your EVM config is `OpEvmConfig<…, CustomFactory>` and you've not
added a wrapper newtype, you're hitting §5.2.

### "trait bound … is not implemented for `OpAddOns<…>`"

…where the components in the type contain a custom EVM type but the
AddOns shows defaults. That's §5.3 — `op_node.add_ons()` is bound to
`OpNode::ComponentsBuilder`'s default components.

### Two `revm` versions in errors

If you ever see `expected revm::context::X, found revm::context::X`
(same path, different versions), Cargo has resolved two `revm` majors
in parallel. The workspace currently pins a single `revm = "38.0.0"`
to avoid this. To localise the damage if it recurs, import revm types
via re-exporting crates in the precompile path:

- `alloy_evm::revm::…` for general revm types,
- `op_revm::…` for OP-specific (`OpSpecId`, `OpHaltReason`),
- `alloy_op_evm::…` for `OpEvm`, `OpEvmContext`, `OpTxError`, `OpTx`.

`arkiv-genesis` continues to use `revm` directly; that's fine because
its API surface is stable across recent revm majors.

---

## 8. Reading order

When picking this up cold, read in this order:

1. **The POC itself.** `crates/arkiv-node/src/precompile.rs` — wires
   together everything described above; all the patterns are visible
   in <500 lines.
2. **Upstream Eth reference.**
   `~/.cargo/git/checkouts/reth-e231042ee7db3fb7/27bfdde/examples/precompile-cache/src/main.rs`
   — smallest end-to-end working example; non-OP, but demonstrates the
   `EvmFactory` + `ExecutorBuilder` swap.
3. **API surface.**
   `~/.cargo/registry/src/index.crates.io-*/alloy-evm-0.33.x/src/precompiles.rs` —
   `Precompile`, `PrecompileInput`, `PrecompilesMap`, `DynPrecompile`.
4. **Wire types.**
   `~/.cargo/registry/src/index.crates.io-*/revm-precompile-34.0.0/src/interface.rs` —
   `PrecompileResult`, `PrecompileOutput`, `PrecompileError`,
   `PrecompileHalt`.
5. **OP `EvmFactory`.**
   `~/.cargo/git/checkouts/optimism-852bcbde357560e3/484be19/rust/alloy-op-evm/src/lib.rs`
   ~lines 200–280 — `OpEvmFactory::create_evm`, the body our wrapper
   delegates to.
6. **OP custom-EVM template.**
   `~/.cargo/git/checkouts/optimism-852bcbde357560e3/484be19/rust/op-reth/examples/custom-node/`
   — wrapper-`Node` and wrapper-`EvmConfig` patterns. Doesn't customise
   precompiles, but is the structural model for §5.2 / §5.3.
7. **OP `ExecutorBuilder` and AddOns.**
   `~/.cargo/git/checkouts/optimism-852bcbde357560e3/484be19/rust/op-reth/crates/node/src/node.rs`
   — `OpExecutorBuilder` (~line 976), `OpAddOns` and its `NodeAddOns`
   impl (~line 594), `OpNode::add_ons()` and the private
   `OpLocalPayloadAttributesBuilder` we copy in §5.4 (~lines 74–137).
8. **OP `OpEvmConfig` and the 3-generic engine impls.**
   `~/.cargo/git/checkouts/optimism-852bcbde357560e3/484be19/rust/op-reth/crates/evm/src/lib.rs`
   lines 60–230, plus
   `op-reth/crates/payload/src/lib.rs:35` — the impls described in
   §5.2.

---

## 9. Out of scope (today)

- **State-mutating precompiles** via `EvmInternals`. The API exists; we
  haven't investigated reverts/gas semantics.
- **Hardfork-gated activation** via `OpHardforks` predicates. POC is
  always-on when its flag is set; gating is straightforward (§6.11) but
  unimplemented.
- **System-call path.**
  `Evm::transact_system_call` interactions if a precompile is ever
  invoked from a system tx.
- **Automated tests.** Verification today is by sending real
  transactions to a `--dev`-mining node and watching `mock-entitydb`'s
  logs. For production, unit tests at the `Precompile::call` level plus
  integration tests through the full executor are needed.
- **Performance.** Caching, warm/cold address lists, and the
  `supports_caching` flag — only briefly noted in §3.1.

These are the obvious next things to investigate.
