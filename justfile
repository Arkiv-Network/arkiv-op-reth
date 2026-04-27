registry := "0x4400000000000000000000000000000000000044"
dev_key  := "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
dev_addr := "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
rpc      := "http://localhost:8545"

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
    cargo run --quiet -p arkiv-cli -- inject-predeploy "$TMP" 2>/dev/null
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
    cargo run --quiet -p arkiv-cli -- inject-predeploy "$GENESIS"
    cargo run --quiet -p arkiv-node -- init --chain "$GENESIS" --datadir "$TMPDIR"
    echo "datadir: $TMPDIR"
    echo "genesis: $GENESIS"
    echo "registry: {{ registry }}"
    echo "dev account: {{ dev_addr }}"
    cargo run -p arkiv-node -- node \
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
    cargo run -p arkiv-node -- {{ args }}

# ── CLI ──────────────────────────────────────────────────────

# Run arkiv-cli with arbitrary args
cli *args='':
    cargo run -p arkiv-cli -- {{ args }}

# Create an entity (random payload)
create *args='':
    cargo run -p arkiv-cli -- create {{ args }}

# Update an entity
update key *args='':
    cargo run -p arkiv-cli -- update --key {{ key }} {{ args }}

# Extend an entity's expiration
extend key expires_in='1h':
    cargo run -p arkiv-cli -- extend --key {{ key }} --expires-in {{ expires_in }}

# Transfer entity ownership
transfer key new_owner:
    cargo run -p arkiv-cli -- transfer --key {{ key }} --new-owner {{ new_owner }}

# Delete an entity
delete key:
    cargo run -p arkiv-cli -- delete --key {{ key }}

# Expire an entity (must be past expiration)
expire key:
    cargo run -p arkiv-cli -- expire --key {{ key }}

# Query an entity commitment
query key:
    cargo run -p arkiv-cli -- query --key {{ key }}

# Read the current changeset hash
hash:
    cargo run -p arkiv-cli -- hash

# Walk the changeset history
history *args='':
    cargo run -p arkiv-cli -- history {{ args }}

# Check dev account balance
balance *args='':
    cargo run -p arkiv-cli -- balance {{ args }}

# Submit a batch of operations from a JSON file in a single tx
batch file:
    cargo run -p arkiv-cli -- batch {{ file }}

# Fire off multiple entity creates
spam *args='':
    cargo run -p arkiv-cli -- spam {{ args }}

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
    cargo run --quiet -p arkiv-cli -- inject-predeploy "$GENESIS"
    cargo run --quiet -p arkiv-node -- init --chain "$GENESIS" --datadir "$TMPDIR"
    echo "datadir: $TMPDIR"
    echo "genesis: $GENESIS"
    echo "registry: {{ registry }}"
    echo "dev account: {{ dev_addr }}"
    echo "entitydb: http://localhost:9545"
    ARKIV_ENTITYDB_URL=http://localhost:9545 \
    cargo run -p arkiv-node -- node \
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
    cargo run --quiet -p arkiv-cli -- inject-predeploy "$GENESIS"
    cargo run --quiet -p arkiv-node -- init --chain "$GENESIS" --datadir "$TMPDIR"
    echo "datadir: $TMPDIR"
    echo "genesis: $GENESIS"
    echo "registry: {{ registry }}"
    echo "dev account: {{ dev_addr }}"
    echo "storaged: http://localhost:2704"
    ARKIV_ENTITYDB_URL=http://localhost:2704 \
    cargo run -p arkiv-node -- node \
        --chain "$GENESIS" \
        --dev \
        --dev.block-time 2s \
        --datadir "$TMPDIR" \
        --http \
        --log.file.directory "$TMPDIR/logs" \
        {{ args }}
    rm -rf "$TMPDIR"

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
