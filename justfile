# ── Build ────────────────────────────────────────────────────

# Check arkiv-node compiles
check:
    cargo check

# Build arkiv-node
build:
    cargo build

# Build arkiv-node (release)
build-release:
    cargo build --release

# ── Node ─────────────────────────────────────────────────────

# Run arkiv-node in dev mode (genesis with EntityRegistry is auto-generated)
node-dev *args='':
    #!/usr/bin/env bash
    set -e
    TMPDIR=$(mktemp -d)
    echo "datadir: $TMPDIR"
    echo "registry: 0x4200000000000000000000000000000000000042"
    echo "dev account: 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
    cargo run -p arkiv-node -- node \
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
