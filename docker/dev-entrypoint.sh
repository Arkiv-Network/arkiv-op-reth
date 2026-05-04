#!/bin/bash

set -euo pipefail

GENESIS="${ARKIV_DEV_GENESIS:-/home/docker/genesis.json}"
CHAINSPEC_TEMPLATE="${ARKIV_DEV_CHAINSPEC:-/opt/arkiv/dev.base.json}"

if [ ! -f "$GENESIS" ] || [ "${ARKIV_DEV_FRESH:-false}" = "true" ]; then
    echo "[dev-entrypoint] bootstrapping fresh dev chain at ${GENESIS}"
    cp "$CHAINSPEC_TEMPLATE" "$GENESIS"
    arkiv-cli inject-predeploy "$GENESIS"
    arkiv-node init --chain "$GENESIS"
else
    echo "[dev-entrypoint] existing genesis at ${GENESIS} — skipping bootstrap (set ARKIV_DEV_FRESH=true to recreate)"
fi

if [ -n "${ARKIV_NODE_CLI:-}" ]; then
    echo "[dev-entrypoint] using ARKIV_NODE_CLI override: ${ARKIV_NODE_CLI}"
    exec sh -c "exec arkiv-node ${ARKIV_NODE_CLI}"
fi

exec arkiv-node \
    node \
    --chain "$GENESIS" \
    --http \
    --http.addr 0.0.0.0 \
    --http.port 8545 \
    --http.api eth,net,web3,debug,arkiv \
    --http.corsdomain '*' \
    --ws \
    --ws.addr 0.0.0.0 \
    --ws.port 8546 \
    --ws.api eth,net,web3,debug,golembase,arkiv \
    --ws.origins '*' \
    --dev
