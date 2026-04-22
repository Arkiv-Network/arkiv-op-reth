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

# Run arkiv-node in dev mode with datadir in a temporary directory
node-dev *args='':
    #!/usr/bin/env bash
    set -e
    TMPDIR=$(mktemp -d)
    echo "Starting arkiv-node in dev mode with datadir: $TMPDIR"
    cargo run -p arkiv-node -- node --dev --datadir "$TMPDIR" --dev.block-time 2s --http -vvv --log.file.directory "$TMPDIR/logs" {{ args }}
    echo "Cleaning up $TMPDIR"
    rm -rf "$TMPDIR"

# Run arkiv-node with custom args
node *args='':
    cargo run -p arkiv-node -- {{ args }}
