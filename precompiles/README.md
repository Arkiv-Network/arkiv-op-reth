# Precompile experiments

End-to-end harness for the EntityDB-write precompile shipped in
`crates/arkiv-node/src/precompile.rs`. See `docs/custom-precompile.md`
for the design background and consensus caveats.

## Layout

```
contracts/PrecompileCaller.sol   # tiny harness: forwards calldata to 0x00…aa01
foundry.toml                     # standalone foundry config (src=contracts/, out=out/)
run.sh                           # build → deploy → call → print round-trip
```

## Running an experiment

Three terminals, in the workspace root:

1. `just mock-entitydb`
2. `just node-dev-jsonrpc`  (the recipe enables `--arkiv.precompile` by default)
3. `precompiles/run.sh`

`run.sh` builds the contract, deploys it from the standard dev account,
calls `callPrecompile(0xdeadbeef)`, prints the resulting log, and reads
`lastStateRoot()` back. Cross-check terminal 1 — every successful
invocation should print an `arkiv_precompileWrite` request whose `data`
field matches what you passed in.

To send different calldata: `CALLDATA=0xcafebabe precompiles/run.sh`.
Other knobs: `RPC_URL` (default `http://localhost:8545`),
`PRIVATE_KEY` (default = the standard hardhat key 0).

## What this exercises

- The wiring in `crates/arkiv-node/src/precompile.rs` (custom EvmFactory →
  `ArkivOpEvmConfig` → `ArkivOpNode` → executor swap).
- The sync HTTP-from-precompile path
  (`tokio::block_in_place` inside `EntityDbClient::rpc_call`).
- That `mock-entitydb.js` returns `{ stateRoot: ZERO_ROOT }` for the
  unrecognised `arkiv_precompileWrite` method, which the precompile
  parses and returns to the EVM as 32 bytes — so the
  `PrecompileResult` event's `stateRoot` should be all zeroes when
  pointed at the mock.
