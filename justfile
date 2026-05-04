registry := "0x4400000000000000000000000000000000000044"
dev_key  := "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
dev_addr := "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
rpc      := "http://localhost:8545"
arkiv_node := env_var_or_default("ARKIV_NODE", "cargo run -p arkiv-node --")
arkiv_cli  := env_var_or_default("ARKIV_CLI", "cargo run -p arkiv-cli --")

# Single working dir for every recipe that needs scratch space.
# Always REPO_ROOT/tmp so paths are predictable across local / docker / CI.
tmp_dir := justfile_directory() / "tmp"

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
    mkdir -p "{{ tmp_dir }}"
    GENESIS="{{ tmp_dir }}/genesis.json"
    cp chainspec/dev.base.json "$GENESIS"
    {{ arkiv_cli }} inject-predeploy "$GENESIS" 2>/dev/null
    cat "$GENESIS"

# Run arkiv-node in dev mode against a freshly assembled Arkiv genesis.
# Generates genesis -> init datadir -> launch node, all against the same
# chainspec file so init/node agree on the genesis hash.
node-dev *args='':
    #!/usr/bin/env bash
    set -e
    DATADIR="{{ tmp_dir }}/node-dev"
    rm -rf "$DATADIR"
    mkdir -p "$DATADIR"
    GENESIS="$DATADIR/genesis.json"
    cp chainspec/dev.base.json "$GENESIS"
    {{ arkiv_cli }} inject-predeploy "$GENESIS"
    {{ arkiv_node }} init --chain "$GENESIS" --datadir "$DATADIR"
    echo "datadir: $DATADIR"
    echo "genesis: $GENESIS"
    echo "registry: {{ registry }}"
    echo "dev account: {{ dev_addr }}"
    {{ arkiv_node }} node \
        --chain "$GENESIS" \
        --dev \
        --dev.block-time 2s \
        --datadir "$DATADIR" \
        --http \
        --arkiv.debug \
        --log.file.directory "$DATADIR/logs" \
        {{ args }}

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

# Read an entity commitment from the EntityRegistry contract (on-chain)
commitment key:
    {{ arkiv_cli }} query --key {{ key }}

# Send an arkiv_query JSON-RPC request to the running node (proxied to EntityDB).
# `expr` is the query expression (must be a JSON string literal); `opts` is an
# optional options object. Both default to selecting all entities with no opts.
# Examples:
#   just query                                        # all entities, no opts
#   just query '"type = \"nft\""'                      # filter by attribute
#   just query '"$all"' '{"resultsPerPage":"0xa"}'     # with options
query expr='"$all"' opts='null':
    curl -s -X POST {{ rpc }} \
        -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"arkiv_query","params":[{{ expr }}, {{ opts }}]}'

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

# Continuously simulate live system traffic against a running node
simulate *args='':
    cargo run -p arkiv-cli -- simulate {{ args }}

# ── EntityDB Mock ────────────────────────────────────────────

# Run mock EntityDB that logs incoming JSON-RPC requests
mock-entitydb port='9545':
    node scripts/mock-entitydb.js {{ port }}

# Run arkiv-node in dev mode with JsonRpcStore pointing at mock EntityDB.
# Same setup as `node-dev` plus the ExEx forwarding to a local EntityDB.
node-dev-jsonrpc *args='':
    #!/usr/bin/env bash
    set -e
    DATADIR="{{ tmp_dir }}/node-dev-jsonrpc"
    rm -rf "$DATADIR"
    mkdir -p "$DATADIR"
    GENESIS="$DATADIR/genesis.json"
    cp chainspec/dev.base.json "$GENESIS"
    {{ arkiv_cli }} inject-predeploy "$GENESIS"
    {{ arkiv_node }} init --chain "$GENESIS" --datadir "$DATADIR"
    echo "datadir: $DATADIR"
    echo "genesis: $GENESIS"
    echo "registry: {{ registry }}"
    echo "dev account: {{ dev_addr }}"
    echo "entitydb: http://localhost:9545"
    {{ arkiv_node }} node \
        --chain "$GENESIS" \
        --dev \
        --dev.block-time 2s \
        --datadir "$DATADIR" \
        --http \
        --arkiv.db-url http://localhost:9545 \
        --log.file.directory "$DATADIR/logs" \
        {{ args }}

# ── arkiv-storaged ───────────────────────────────────────────

# Run arkiv-node in dev mode with JsonRpcStore pointing at arkiv-storaged
node-dev-storaged *args='':
    #!/usr/bin/env bash
    set -e
    DATADIR="{{ tmp_dir }}/node-dev-storaged"
    rm -rf "$DATADIR"
    mkdir -p "$DATADIR"
    GENESIS="$DATADIR/genesis.json"
    cp chainspec/dev.base.json "$GENESIS"
    {{ arkiv_cli }} inject-predeploy "$GENESIS"
    {{ arkiv_node }} init --chain "$GENESIS" --datadir "$DATADIR"
    echo "datadir: $DATADIR"
    echo "genesis: $GENESIS"
    echo "registry: {{ registry }}"
    echo "dev account: {{ dev_addr }}"
    echo "storaged:  http://localhost:2704 (ExEx)  http://localhost:2705 (query)"
    {{ arkiv_node }} node \
        --chain "$GENESIS" \
        --dev \
        --dev.block-time 2s \
        --datadir "$DATADIR" \
        --http \
        --arkiv.db-url http://localhost:2704 \
        --arkiv.query-url http://localhost:2705 \
        --log.file.directory "$DATADIR/logs" \
        {{ args }} &
    NODE_PID=$!
    if [ -n "${ARKIV_NODE_PID_FILE:-}" ]; then
        printf '%s\n' "$NODE_PID" >"$ARKIV_NODE_PID_FILE"
    fi
    wait "$NODE_PID"

# Run the scripted demo against the local demo EntityDB/query shim.
demo-e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    TMPDIR="{{ tmp_dir }}/demo-e2e"
    rm -rf "$TMPDIR"
    mkdir -p "$TMPDIR"
    KEEP_LOGS="${E2E_KEEP_LOGS:-false}"
    ENTITYDB_LOG="$TMPDIR/demo-entitydb.log"
    NODE_LOG="$TMPDIR/arkiv-node.log"
    DEMO_LOG="$TMPDIR/demo-script.log"
    HARNESS_LOG="$TMPDIR/demo-e2e.log"
    NODE_PID_FILE="$TMPDIR/arkiv-node.pid"
    # The Python demo backend starts almost immediately.
    ENTITYDB_READY_RETRIES=50
    # `node-dev-storaged` has to assemble genesis, init the datadir, and launch the node.
    NODE_READY_RETRIES=120
    log() {
        local message="$1"
        printf '[demo-e2e] %s %s\n' "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" "$message" | tee -a "$HARNESS_LOG"
    }
    stop_pid() {
        local pid="$1"
        local tries

        if [ -z "$pid" ]; then
            return 0
        fi

        if ! kill -0 "$pid" 2>/dev/null; then
            return 0
        fi

        kill "$pid" 2>/dev/null || true
        for tries in 1 2 3 4 5; do
            if ! kill -0 "$pid" 2>/dev/null; then
                echo "process $pid stopped gently"
                return 0
            fi
            sleep 1
        done

        kill -9 "$pid" 2>/dev/null || true
        if kill -0 "$pid" 2>/dev/null; then
            echo "Failed to stop process" >&2
            return 1
        fi
        return 0
    }
    pid_from_file() {
        local path="$1"
        if [ -f "$path" ]; then
            cat "$path"
        fi
    }
    cleanup() {
        log "cleanup starting"
        stop_pid "$(pid_from_file "$NODE_PID_FILE")"
        if [ -f "$NODE_LOG" ]; then
            log "node log size $(wc -c <"$NODE_LOG") bytes"
        else
            log "node log missing"
        fi
        if [ -f "$DEMO_LOG" ]; then
            log "demo log size $(wc -c <"$DEMO_LOG") bytes"
        else
            log "demo log missing"
        fi
        if [ -f "$ENTITYDB_LOG" ]; then
            log "entitydb log size $(wc -c <"$ENTITYDB_LOG") bytes"
        else
            log "entitydb log missing"
        fi
        find "$TMPDIR" -maxdepth 2 -type f | sort | tee -a "$HARNESS_LOG"
        if [ "$KEEP_LOGS" != "true" ]; then
            rm -rf "$TMPDIR"
        fi
    }
    trap cleanup EXIT

    : >"$HARNESS_LOG"
    log "starting demo-e2e in $TMPDIR"
    log "KEEP_LOGS=$KEEP_LOGS"
    log "ARKIV_NODE=${ARKIV_NODE:-cargo run -p arkiv-node --}"
    log "ARKIV_CLI=${ARKIV_CLI:-cargo run -p arkiv-cli --}"
    log "ARKIV_STORAGED_PATH=${ARKIV_STORAGED_PATH:-<unset>}"
    log "ARKIV_STORAGED_ARGS=${ARKIV_STORAGED_ARGS:-<unset>}"
    log "ENTITYDB_READY_RETRIES=$ENTITYDB_READY_RETRIES"
    log "NODE_READY_RETRIES=$NODE_READY_RETRIES"

    if [ -n "${ARKIV_STORAGED_PATH:-}" ]; then
        log "arkiv-storaged will be supervised by arkiv-node"
    else
        log "arkiv-storaged path is unset; demo will rely on an external backend"
    fi

    : >"$ENTITYDB_LOG"
    ARKIV_NODE_PID_FILE="$NODE_PID_FILE" just node-dev-storaged >"$NODE_LOG" 2>&1 &
    NODE_PID=$!
    log "started just node-dev-storaged with shell pid $NODE_PID"

    for attempt in $(seq 1 "$NODE_READY_RETRIES"); do
        if curl -fsS -X POST http://localhost:8545 \
            -H "Content-Type: application/json" \
            -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' >/dev/null 2>&1; then
            log "node became ready on attempt $attempt"
            log "running demo/justfile full"
            just -f demo/justfile full >"$DEMO_LOG" 2>&1
            log "demo/justfile full completed successfully"
            exit 0
        fi
        if [ "$attempt" = "1" ] || [ $((attempt % 10)) -eq 0 ]; then
            log "node not ready yet on attempt $attempt/$NODE_READY_RETRIES"
        fi
        sleep 1
    done

    log "arkiv-node did not become ready after $NODE_READY_RETRIES attempts"
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
