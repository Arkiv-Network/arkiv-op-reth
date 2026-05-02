
#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Remove complicated instruction to let CODEX discover everything by himself
cp "${SCRIPT_DIR}/fd-agent.md" "${ROOT_DIR}/AGENTS.override.md"

rustup toolchain install 1.95.0
rustup default 1.95.0

rsync -a --delete --mkpath /rust_project_cache/arkiv-op-reth/1.95/target "${ROOT_DIR}/target"


