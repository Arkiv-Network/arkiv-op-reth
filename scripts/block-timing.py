#!/usr/bin/env python3
"""
Pipe arkiv node stdout here to track per-block timing and entity counts.

Usage:
    ARKIV_NODE=./target/release/arkiv-node just node-dev 2>&1 | python3 scripts/block-timing.py
    ARKIV_NODE=./target/release/arkiv-node just node-dev 2>&1 | python3 scripts/block-timing.py --out blocks.json --rpc http://localhost:8545 --db tmp/node-dev/db

The 2>&1 is required — reth writes logs to stderr, not stdout.

Fields in each record:
  block_number          — block height
  block_hash            — block hash
  timestamp             — UTC wall-clock when "Block added" was seen
  block_time_between_s  — wall-clock seconds between consecutive "Block added" events (~2s dev cadence)
  evm_execution_time_ms — elapsed from "Block added" log (EVM + state transition), in ms
  block_flushing_time_ms— wall-clock ms between "Block added" and "Canonical chain committed"
  entity_count          — arkiv_getEntityCount result (from in-memory cache, safe after "Block added")
  db_size               — du -hs of the MDBX directory
"""

import argparse
import json
import re
import subprocess
import sys
import time
from datetime import datetime, timezone

ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;]*[mK]")

BLOCK_ADDED_PATTERN = re.compile(
    r"Block added to canonical chain\s+number=(\d+)\s+hash=(0x[0-9a-fA-F]+).*\belapsed=(\S+)"
)
CHAIN_COMMITTED_PATTERN = re.compile(
    r"Canonical chain committed\s+number=(\d+)"
)

ELAPSED_RE = re.compile(r"^([\d.]+)(µs|us|ms|s)$")

ENTITY_COUNT_PAYLOAD = json.dumps(
    {"jsonrpc": "2.0", "id": 1, "method": "arkiv_getEntityCount", "params": []}
)


def parse_elapsed_ms(s: str) -> float | None:
    m = ELAPSED_RE.match(s)
    if not m:
        return None
    value = float(m.group(1))
    unit = m.group(2)
    if unit in ("µs", "us"):
        return round(value / 1000, 6)
    if unit == "ms":
        return round(value, 6)
    if unit == "s":
        return round(value * 1000, 3)
    return None


def get_db_size(db_path: str) -> str | None:
    try:
        proc = subprocess.run(
            ["du", "-hs", db_path],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if proc.returncode != 0:
            return None
        return proc.stdout.split()[0]
    except Exception:
        return None


def get_entity_count(rpc_url: str) -> int | None:
    try:
        proc = subprocess.run(
            [
                "curl", "-s", "-X", "POST", rpc_url,
                "-H", "Content-Type: application/json",
                "-d", ENTITY_COUNT_PAYLOAD,
            ],
            capture_output=True,
            text=True,
            timeout=5,
        )
        data = json.loads(proc.stdout)
        raw = data.get("result")
        if raw is None:
            return None
        return int(raw, 16) if isinstance(raw, str) and raw.startswith("0x") else int(raw)
    except Exception:
        return None


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Track per-block timing from arkiv node logs"
    )
    parser.add_argument(
        "--out", default="blocks.json", help="Output JSON file (default: blocks.json)"
    )
    parser.add_argument(
        "--rpc", default="http://localhost:8545", help="JSON-RPC endpoint (default: http://localhost:8545)"
    )
    parser.add_argument(
        "--db", default="tmp/node-dev/db", help="DB directory for du size check (default: tmp/node-dev/db)"
    )
    parser.add_argument(
        "--debug", action="store_true",
        help="Print matched log lines with repr() to expose hidden chars"
    )
    args = parser.parse_args()

    records: list[dict] = []
    # block_number → {"wall": monotonic float, "record": dict}
    pending: dict[int, dict] = {}
    prev_block_added_wall: float | None = None
    lines_read = 0

    print(
        f"Listening on stdin for block events → {args.out}  (^C to stop)",
        file=sys.stderr,
    )

    try:
        for raw_line in sys.stdin:
            print(raw_line, end="", file=sys.stderr, flush=True)
            lines_read += 1
            clean_line = ANSI_ESCAPE.sub("", raw_line)

            # --- "Block added to canonical chain" ---
            m = BLOCK_ADDED_PATTERN.search(clean_line)
            if m:
                if args.debug:
                    print(f"DEBUG block_added: {repr(raw_line)}", file=sys.stderr, flush=True)
                now = time.monotonic()
                block_number = int(m.group(1))
                block_hash = m.group(2)
                timestamp = datetime.now(timezone.utc).isoformat()

                block_time_between_s = (
                    round(now - prev_block_added_wall, 3)
                    if prev_block_added_wall is not None else None
                )
                prev_block_added_wall = now

                pending[block_number] = {
                    "wall": now,
                    "record": {
                        "block_number": block_number,
                        "block_hash": block_hash,
                        "timestamp": timestamp,
                        "block_time_between_s": block_time_between_s,
                        "evm_execution_time_ms": parse_elapsed_ms(m.group(3)),
                        "block_flushing_time_ms": None,
                        "entity_count": get_entity_count(args.rpc),
                        "db_size": get_db_size(args.db),
                    },
                }
                continue

            # --- "Canonical chain committed" ---
            m = CHAIN_COMMITTED_PATTERN.search(clean_line)
            if m:
                if args.debug:
                    print(f"DEBUG committed: {repr(raw_line)}", file=sys.stderr, flush=True)
                now = time.monotonic()
                block_number = int(m.group(1))

                if block_number not in pending:
                    continue

                entry = pending.pop(block_number)
                record = entry["record"]
                record["block_flushing_time_ms"] = round((now - entry["wall"]) * 1000, 3)

                records.append(record)
                print(json.dumps(record), flush=True)

                with open(args.out, "w") as f:
                    json.dump(records, f, indent=2)

    except KeyboardInterrupt:
        pass

    # Flush any blocks that never received a commit event
    for entry in sorted(pending.values(), key=lambda e: e["record"]["block_number"]):
        records.append(entry["record"])

    with open(args.out, "w") as f:
        json.dump(records, f, indent=2)

    print(
        f"\nDone. {len(records)} blocks written to {args.out} ({lines_read} lines read)",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
