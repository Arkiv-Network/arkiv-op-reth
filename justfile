registry := "0x4400000000000000000000000000000000000044"
dev_key  := "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
dev_addr := "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
rpc      := "http://localhost:8545"
arkiv_node := env_var_or_default("ARKIV_NODE", "cargo run -p arkiv-node --")
arkiv_cli  := env_var_or_default("ARKIV_CLI", "cargo run -p arkiv-cli --")

# ── Build ────────────────────────────────────────────────────

# Check workspace compiles
check:
    cargo check --workspace

# Build workspace
build:
    cargo build --workspace

# Build workspace (release)
build-release:
    cargo build --workspace --release

# Run clippy across the workspace
lint:
    cargo clippy --workspace -- -D warnings

# Format the workspace
fmt:
    cargo fmt --all

# ── Node ─────────────────────────────────────────────────────

# Print an Arkiv dev genesis JSON to stdout (dev.base.json + injected predeploy)
genesis:
    #!/usr/bin/env bash
    set -e
    TMP=$(mktemp)
    cp chainspec/dev.base.json "$TMP"
    {{ arkiv_cli }} inject-predeploy "$TMP" 2>/dev/null
    cat "$TMP"
    rm -f "$TMP"

# Run arkiv-node in dev mode against a freshly assembled Arkiv genesis.
# Generates genesis -> init datadir -> launch node, all against the same
# chainspec file so init/node agree on the genesis hash.
node-dev *args='':
    #!/usr/bin/env bash
    set -e
    TMPDIR=$(mktemp -d)
    GENESIS="$TMPDIR/genesis.json"
    cp chainspec/dev.base.json "$GENESIS"
    {{ arkiv_cli }} inject-predeploy "$GENESIS"
    {{ arkiv_node }} init --chain "$GENESIS" --datadir "$TMPDIR"
    echo "datadir: $TMPDIR"
    echo "genesis: $GENESIS"
    echo "registry: {{ registry }}"
    echo "dev account: {{ dev_addr }}"
    {{ arkiv_node }} node \
        --chain "$GENESIS" \
        --dev \
        --dev.block-time 2s \
        --datadir "$TMPDIR" \
        --http \
        --log.file.directory "$TMPDIR/logs" \
        {{ args }}
    rm -rf "$TMPDIR"

# Run arkiv-node with custom args
node *args='':
    {{ arkiv_node }} {{ args }}

# ── CLI ──────────────────────────────────────────────────────

# Run arkiv-cli with arbitrary args
cli *args='':
    {{ arkiv_cli }} {{ args }}

# Create an entity (random payload)
create *args='':
    {{ arkiv_cli }} create {{ args }}

# Update an entity
update key *args='':
    {{ arkiv_cli }} update --key {{ key }} {{ args }}

# Extend an entity's expiration
extend key expires_in='1h':
    {{ arkiv_cli }} extend --key {{ key }} --expires-in {{ expires_in }}

# Transfer entity ownership
transfer key new_owner:
    {{ arkiv_cli }} transfer --key {{ key }} --new-owner {{ new_owner }}

# Delete an entity
delete key:
    {{ arkiv_cli }} delete --key {{ key }}

# Expire an entity (must be past expiration)
expire key:
    {{ arkiv_cli }} expire --key {{ key }}

# Query an entity commitment
query key:
    {{ arkiv_cli }} query --key {{ key }}

# Read the current changeset hash
hash:
    {{ arkiv_cli }} hash

# Walk the changeset history
history *args='':
    {{ arkiv_cli }} history {{ args }}

# Check dev account balance
balance *args='':
    {{ arkiv_cli }} balance {{ args }}

# Submit a batch of operations from a JSON file in a single tx
batch file:
    {{ arkiv_cli }} batch {{ file }}

# Fire off multiple entity creates
spam *args='':
    {{ arkiv_cli }} spam {{ args }}

# ── EntityDB Mock ────────────────────────────────────────────

# Run mock EntityDB that logs incoming JSON-RPC requests
mock-entitydb port='9545':
    node scripts/mock-entitydb.js {{ port }}

# Run arkiv-node in dev mode with JsonRpcStore pointing at mock EntityDB.
# Same setup as `node-dev` plus the ExEx forwarding to a local EntityDB.
node-dev-jsonrpc *args='':
    #!/usr/bin/env bash
    set -e
    TMPDIR=$(mktemp -d)
    GENESIS="$TMPDIR/genesis.json"
    cp chainspec/dev.base.json "$GENESIS"
    {{ arkiv_cli }} inject-predeploy "$GENESIS"
    {{ arkiv_node }} init --chain "$GENESIS" --datadir "$TMPDIR"
    echo "datadir: $TMPDIR"
    echo "genesis: $GENESIS"
    echo "registry: {{ registry }}"
    echo "dev account: {{ dev_addr }}"
    echo "entitydb: http://localhost:9545"
    ARKIV_ENTITYDB_URL=http://localhost:9545 \
    {{ arkiv_node }} node \
        --chain "$GENESIS" \
        --dev \
        --dev.block-time 2s \
        --datadir "$TMPDIR" \
        --http \
        --log.file.directory "$TMPDIR/logs" \
        {{ args }}
    rm -rf "$TMPDIR"

# ── arkiv-storaged ───────────────────────────────────────────

# Run arkiv-node in dev mode with JsonRpcStore pointing at arkiv-storaged
node-dev-storaged *args='':
    #!/usr/bin/env bash
    set -e
    TMPDIR=$(mktemp -d)
    GENESIS="$TMPDIR/genesis.json"
    cp chainspec/dev.base.json "$GENESIS"
    {{ arkiv_cli }} inject-predeploy "$GENESIS"
    {{ arkiv_node }} init --chain "$GENESIS" --datadir "$TMPDIR"
    echo "datadir: $TMPDIR"
    echo "genesis: $GENESIS"
    echo "registry: {{ registry }}"
    echo "dev account: {{ dev_addr }}"
    echo "storaged: http://localhost:2704"
    ARKIV_ENTITYDB_URL=http://localhost:2704 \
    {{ arkiv_node }} node \
        --chain "$GENESIS" \
        --dev \
        --dev.block-time 2s \
        --datadir "$TMPDIR" \
        --http \
        --log.file.directory "$TMPDIR/logs" \
        {{ args }}
    rm -rf "$TMPDIR"

# Run the scripted demo against the local demo EntityDB/query shim.
demo-e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    TMPDIR=$(mktemp -d)
    ENTITYDB_LOG="$TMPDIR/demo-entitydb.log"
    NODE_LOG="$TMPDIR/arkiv-node.log"
    cleanup() {
        for pid in "${NODE_PID:-}" "${ENTITYDB_PID:-}"; do
            if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
                kill "$pid" 2>/dev/null || true
            fi
        done
        sleep 1
        for pid in "${NODE_PID:-}" "${ENTITYDB_PID:-}"; do
            if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
                kill -9 "$pid" 2>/dev/null || true
            fi
        done
        rm -rf "$TMPDIR"
    }
    trap cleanup EXIT

    python3 scripts/demo_entitydb.py >"$ENTITYDB_LOG" 2>&1 &
    ENTITYDB_PID=$!

    for _ in $(seq 1 50); do
        if curl -fsS -X POST http://localhost:2704 \
            -H "Content-Type: application/json" \
            -d '{"jsonrpc":"2.0","id":0,"method":"arkiv_ping","params":[]}' >/dev/null; then
            break
        fi
        sleep 1
    done

    just node-dev-storaged >"$NODE_LOG" 2>&1 &
    NODE_PID=$!

    for _ in $(seq 1 120); do
        if curl -fsS -X POST http://localhost:8545 \
            -H "Content-Type: application/json" \
            -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' >/dev/null; then
            just -f demo/justfile full
            exit 0
        fi
        sleep 1
    done

    echo "arkiv-node did not become ready; see $NODE_LOG" >&2
    exit 1

# ── Dev Helpers ──────────────────────────────────────────────

# Verify EntityRegistry is deployed (requires running node)
verify-registry:
    @cast code {{ registry }} --rpc-url {{ rpc }} | head -c 80
    @echo "..."

# Check dev account balance via cast
verify-balance:
    @cast balance {{ dev_addr }} --rpc-url {{ rpc }} --ether

# Send ETH from the dev account to an address
fund address amount="1ether":
    cast send --private-key {{ dev_key }} --rpc-url {{ rpc }} {{ address }} --value {{ amount }}

# Show current block number
block-number:
    @cast block-number --rpc-url {{ rpc }}
