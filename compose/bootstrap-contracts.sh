#!/bin/bash

set -xeuo pipefail

cp ../chainspec/dev.base.json genesis.json && sudo chown 999:999 genesis.json
sudo rm -fr ./reth ./storaged

mkdir ./reth && sudo chown 999:999 ./reth
mkdir ./storaged && sudo chown 999:999 ./storaged

docker run --rm \
  -v ./genesis.json:/home/docker/genesis.json \
  -v ./reth:/home/docker/.local/share/reth \
  -v ./storaged:/home/docker/.arkiv-storaged \
  --entrypoint arkiv-cli \
  ghcr.io/arkiv-network/arkiv-node \
  inject-predeploy /home/docker/genesis.json

docker run --rm \
  -v ./genesis.json:/home/docker/genesis.json \
  -v ./reth:/home/docker/.local/share/reth \
  -v ./storaged:/home/docker/.arkiv-storaged \
  ghcr.io/arkiv-network/arkiv-node \
  init --chain /home/docker/genesis.json
