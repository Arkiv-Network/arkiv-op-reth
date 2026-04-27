# arkiv-reth
Arkiv reth execution client

## Genesis

By default, when no `--chain` is provided, the node loads its built-in chain
spec and injects two default accounts into the genesis alloc:

- the **EntityRegistry** predeploy at `0x4200000000000000000000000000000000000042`
- a **dev account** (Hardhat mnemonic, account #0) funded with 10,000 ETH

and forces the chain id to `1337` if the chain spec does not specify one.

### Using a custom `genesis.json` (e.g. from op-deployer)

You can supply your own genesis with `--chain /path/to/genesis.json`. The node
will:

- **Respect** the `chainId` declared in your genesis (no longer forced to `1337`).
- **Only inject** the EntityRegistry predeploy / dev account if the
  corresponding addresses are **not already present** in your `alloc`.
- Leave hardfork timestamps and the `optimism` config block untouched if you
  already set them.

This means a `genesis.json` produced by `op-deployer` that already contains the
`EntityRegistry` contract (and any custom dev funding) will be used verbatim,
and its genesis hash will match the one stored in the database — fixing the

```
genesis hash in the storage does not match the specified chainspec
```

error.

### Generating a canonical `genesis.json`

If you'd like the node to produce a `genesis.json` that already includes the
default `EntityRegistry` predeploy and dev account (so you can hand it to
`op-deployer` or other tools and have a single source of truth), run the node
once with `ARKIV_DUMP_GENESIS` pointing at the desired output path:

```sh
ARKIV_DUMP_GENESIS=./genesis.json arkiv-node node
```

The node will resolve the chain spec, write the merged genesis to the given
file, and exit. You can then start the node normally with
`--chain ./genesis.json` and the genesis hash will be stable.

### Using a fully external `genesis.json`

If you've already prepared a complete `genesis.json` externally (for example by
generating `genesis-deployer.json` and `genesis-op-reth.json` separately and
merging them yourself), you can tell the node to use it as-is, without any
Arkiv-specific injections (matching the original op-reth behavior), by setting
`ARKIV_USE_EXTERNAL_GENESIS`:

```sh
ARKIV_USE_EXTERNAL_GENESIS=1 arkiv-node node --chain ./genesis.json
```

When this variable is set, the node will not inject the default `EntityRegistry`
predeploy, dev account, or Optimism hardfork activation timestamps; the provided
genesis is taken verbatim.
