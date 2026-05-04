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

exec arkiv-node "$@"
