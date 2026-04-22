# Default profile (fast local dev)
profile := env("FOUNDRY_PROFILE", "default")

# ── Build ────────────────────────────────────────────────────

# Build contracts
build:
    forge build

# Build with contract sizes
build-sizes:
    forge build --sizes

# Build with production profile
build-prod:
    FOUNDRY_PROFILE=prod forge build --sizes

# ── Test ─────────────────────────────────────────────────────

# Run tests (default profile)
test *args='':
    forge test {{ args }}

# Run tests verbose
test-v *args='':
    forge test -vvv {{ args }}

# Run tests with CI profile (via_ir, 5k fuzz runs)
test-ci *args='':
    FOUNDRY_PROFILE=ci forge test {{ args }}

# Run a single test by name
test-match name:
    forge test --match-test {{ name }} -vvv

# Run tests in a single file
test-file path:
    forge test --match-path {{ path }} -vvv

# ── Coverage ─────────────────────────────────────────────────

# Coverage summary
coverage:
    forge coverage --exclude-tests

# Coverage with lcov output
coverage-lcov:
    forge coverage --exclude-tests --report lcov

# ── Quality ──────────────────────────────────────────────────

# Format check
fmt-check:
    forge fmt --check

# Format fix
fmt:
    forge fmt

# Lint
lint:
    forge lint

# Gas report
gas:
    forge test --gas-report

# Gas report with CI profile
gas-ci:
    FOUNDRY_PROFILE=ci forge test --gas-report

# ── CI (mirrors GitHub Actions) ──────────────────────────────

# Run the full CI pipeline locally
ci: fmt-check lint build-sizes test-ci coverage

# ── Rust / Node ──────────────────────────────────────────────

# Print the genesis configuration to stdout
print-genesis:
    cargo run -p arkiv-genesis --bin print-genesis

# Run arkiv-node in dev mode with datadir in a temporary directory
node-dev:
    #!/usr/bin/env bash
    set -e
    TMPDIR=$(mktemp -d)
    echo "Starting arkiv-node in dev mode with datadir: $TMPDIR"
    cargo run -p arkiv-node -- node --dev --datadir "$TMPDIR" --dev.block-time 2s --http -vvv --log.file.directory "$TMPDIR/logs"
    echo "Cleaning up $TMPDIR"
    rm -rf "$TMPDIR"

# Run arkiv-cli commands against the local node
cli *args:
    cargo run -p arkiv-cli -- {{ args }}

# Fire off multiple entity creates
spam count="10":
    cargo run -p arkiv-cli -- spam --count {{ count }}
