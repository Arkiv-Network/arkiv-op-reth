# contracts

Solidity sources for Arkiv's on-chain components.

| Contract | Address | Notes |
|---|---|---|
| [`EntityRegistry`](src/EntityRegistry.sol) | `0x4400000000000000000000000000000000000044` | User-facing entry point. Validates owner/expiry, mints entity keys, dispatches to the precompile. |
| Arkiv precompile | `0x4400000000000000000000000000000000000045` | Native Rust precompile registered by `arkiv-op-reth`'s custom `EvmFactory`. Not a Solidity contract. |
| System account | `0x4400000000000000000000000000000000000046` | Pre-allocated empty account; the precompile writes the entity counter and ID maps to its storage slots. No code. |

## Build

```
forge build
```

Produces `out/EntityRegistry.sol/EntityRegistry.json`. The runtime
bytecode is committed at [`artifacts/EntityRegistry.runtime.hex`](artifacts/EntityRegistry.runtime.hex);
`arkiv-genesis` reads it via `include_str!`.

To refresh the committed artifact after editing the source:

```
just contracts-build
```

(from the repo root). CI checks that the committed artifact matches a
fresh `forge build`.

## Layout

Standard Foundry. Sources in `src/`, build output in `out/`
(gitignored). No external deps (`lib/` empty); add via `forge install`
if needed.
