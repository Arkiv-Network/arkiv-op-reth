#!/bin/bash

set -euo pipefail

print_command() {
    printf '[dev-entrypoint] exec:'
    printf ' %q' "$@"
    printf '\n'
}

if [ "${1:-}" = "fund-account" ]; then
    print_command /usr/local/bin/fund-account.sh "$@"
    exec /usr/local/bin/fund-account.sh "$@"
fi

GENESIS="${ARKIV_DEV_GENESIS:-/home/docker/genesis.json}"
CHAINSPEC_TEMPLATE="${ARKIV_DEV_CHAINSPEC:-/opt/arkiv/dev.base.json}"
DATADIR="${ARKIV_DEV_DATADIR:-/home/docker/.local/share/arkiv-node}"

if [ "${ARKIV_DEV_USE_EXISTING_GENESIS:-false}" = "true" ] && [ -f "$GENESIS" ]; then
    echo "[dev-entrypoint] existing genesis at ${GENESIS} - skipping bootstrap"
else
    if [ -z "$DATADIR" ] || [ "$DATADIR" = "/" ]; then
        echo "[dev-entrypoint] refusing to delete unsafe data dir: ${DATADIR}" >&2
        exit 1
    fi

    echo "[dev-entrypoint] bootstrapping fresh dev chain at ${GENESIS}"
    mkdir -p "$DATADIR"
    echo "[dev-entrypoint] removing previous data dir contents at ${DATADIR}"
    find "$DATADIR" -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +

    cp "$CHAINSPEC_TEMPLATE" "$GENESIS"
    print_command arkiv-cli inject-predeploy "$GENESIS"
    arkiv-cli inject-predeploy "$GENESIS"
    print_command arkiv-node init --chain "$GENESIS" --datadir "$DATADIR"
    arkiv-node init --chain "$GENESIS" --datadir "$DATADIR"
fi

if [ -n "${ARKIV_NODE_CLI:-}" ]; then
    echo "[dev-entrypoint] using ARKIV_NODE_CLI override: ${ARKIV_NODE_CLI}"
    echo "[dev-entrypoint] exec: arkiv-node ${ARKIV_NODE_CLI}"
    exec sh -c "exec arkiv-node ${ARKIV_NODE_CLI}"
else
    cmd=(
        arkiv-node
        node
        --datadir "$DATADIR"
        --chain "$GENESIS"
        --http
        --http.addr 0.0.0.0
        --http.port 8545
        --http.api eth,net,web3,debug
        --http.corsdomain '*'
        --ws
        --ws.addr 0.0.0.0
        --ws.port 8546
        --ws.api eth,net,web3,debug
        --ws.origins '*'
        --dev.block-time 2s
        --dev
        --arkiv-storaged-path=/usr/local/bin/arkiv-storaged
        --arkiv-storaged-args="--chain-addr=0.0.0.0:2704 --query-addr=0.0.0.0:2705"
        --arkiv.db-url=http://127.0.0.1:2704
        --arkiv.query-url=http://127.0.0.1:2705
        --metrics 0.0.0.0:5678
    )
    print_command "${cmd[@]}"
    exec "${cmd[@]}"
fi
