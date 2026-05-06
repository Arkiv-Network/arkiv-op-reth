#!/usr/bin/env bash
#
# End-to-end exercise of the Arkiv EntityDB-write precompile.
#
# Prereqs (each in its own terminal, in the workspace root):
#   1. just mock-entitydb
#   2. just node-dev-jsonrpc       # has --arkiv.precompile baked in
#
# Then run this script. It will:
#   - forge build the harness contract
#   - deploy it from the standard dev account
#   - invoke callPrecompile() with some calldata
#   - print the transaction receipt's logs (the PrecompileResult event)
#   - read back lastStateRoot via `cast call` for sanity
#
# Watch the mock-entitydb terminal: every successful invocation should log
# an `arkiv_precompileWrite` request with the calldata you passed in.

set -euo pipefail

cd "$(dirname "$0")"

# Defaults; override via env if your setup differs.
RPC_URL="${RPC_URL:-http://localhost:8545}"
PRIVATE_KEY="${PRIVATE_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
CALLDATA="${CALLDATA:-0xdeadbeef}"

# 1. Build.
echo "[1/4] forge build"
forge build --root . --silent

BYTECODE=$(jq -r '.bytecode.object' out/PrecompileCaller.sol/PrecompileCaller.json)

# 2. Deploy.
echo "[2/4] deploy PrecompileCaller"
DEPLOY_TX=$(cast send \
    --rpc-url "$RPC_URL" \
    --private-key "$PRIVATE_KEY" \
    --create "$BYTECODE" \
    --json)
ADDRESS=$(echo "$DEPLOY_TX" | jq -r '.contractAddress')
echo "    deployed at: $ADDRESS"

# 3. Invoke.
echo "[3/4] callPrecompile($CALLDATA)"
CALL_TX=$(cast send \
    --rpc-url "$RPC_URL" \
    --private-key "$PRIVATE_KEY" \
    "$ADDRESS" \
    'callPrecompile(bytes)' "$CALLDATA" \
    --json)
TX_HASH=$(echo "$CALL_TX" | jq -r '.transactionHash')
echo "    tx: $TX_HASH"
echo "    logs:"
echo "$CALL_TX" | jq -r '.logs[] | "      address=\(.address) topic0=\(.topics[0]) data=\(.data)"'

# 4. Read back.
echo "[4/4] lastStateRoot()"
ROOT=$(cast call --rpc-url "$RPC_URL" "$ADDRESS" 'lastStateRoot()(bytes32)')
echo "    lastStateRoot = $ROOT"

echo
echo "Done. Cross-check the mock-entitydb terminal: expect a request like"
echo "  { method: 'arkiv_precompileWrite', params: [{ data: '$CALLDATA', caller: '...', value: '0x0' }] }"
