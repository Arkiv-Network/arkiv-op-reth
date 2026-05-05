#!/usr/bin/env python3
"""List and prune GHCR container packages for the Arkiv org.

Reads `GH_TOKEN` from the environment (used both for `gh api` calls and to
mint a GHCR registry pull token for manifest size lookups). The org defaults
to `arkiv-network` and can be overridden via `--org` or the `ORG` env var.
"""

from __future__ import annotations

import argparse
import base64
import datetime as dt
import json
import os
import subprocess
import sys
import urllib.parse
import urllib.request


DEFAULT_ORG = "arkiv-network"
COMMIT_TAG_MIN_LEN = 7


def gh_api(path: str, method: str = "GET", paginate: bool = False) -> object:
    cmd = ["gh", "api"]
    if paginate:
        cmd.append("--paginate")
    if method != "GET":
        cmd.extend(["-X", method])
    cmd.append(path)
    try:
        result = subprocess.run(cmd, capture_output=True, text=True)
    except FileNotFoundError:
        sys.stderr.write("error: `gh` CLI is required but not found on PATH\n")
        sys.exit(2)
    if result.returncode != 0:
        sys.stderr.write(f"gh api {method} {path} failed: {result.stderr.strip()}\n")
        return None
    body = result.stdout.strip()
    if not body:
        return None
    try:
        return json.loads(body)
    except json.JSONDecodeError:
        return None


def get_ghcr_token(org: str, package: str, gh_token: str) -> str:
    url = (
        f"https://ghcr.io/token?service=ghcr.io"
        f"&scope=repository:{org}/{package}:pull"
    )
    creds = base64.b64encode(f"token:{gh_token}".encode()).decode()
    req = urllib.request.Request(url, headers={"Authorization": f"Basic {creds}"})
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            data = json.load(r)
        return data.get("token", data.get("access_token", ""))
    except Exception as exc:
        sys.stderr.write(f"warn: failed to get ghcr token for {package}: {exc}\n")
        return ""


def get_manifest_size(org: str, package: str, tag: str, token: str) -> int | None:
    url = f"https://ghcr.io/v2/{org}/{package}/manifests/{tag}"
    req = urllib.request.Request(
        url,
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.docker.distribution.manifest.v2+json",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            manifest = json.load(r)
    except Exception:
        return None
    layers = manifest.get("layers", [])
    if not layers:
        return None
    return sum(l.get("size", 0) for l in layers)


def fmt_size(n: int | None) -> str:
    if n is None or n == 0:
        return "unknown"
    for unit, div in [("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10)]:
        if n >= div:
            return f"{n / div:.1f} {unit}"
    return f"{n} B"


def is_commit_tag(tag: str) -> bool:
    """Return True if `tag` looks like a git commit SHA (hex, len >= 7)."""
    if len(tag) < COMMIT_TAG_MIN_LEN:
        return False
    try:
        int(tag, 16)
    except ValueError:
        return False
    return True


def parse_created_at(value: str) -> dt.datetime | None:
    if not value:
        return None
    try:
        return dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=dt.timezone.utc
        )
    except ValueError:
        return None


def list_versions(org: str, package: str) -> list[dict]:
    encoded = urllib.parse.quote(package, safe="")
    versions = gh_api(
        f"/orgs/{org}/packages/container/{encoded}/versions", paginate=True
    )
    if not isinstance(versions, list):
        return []
    return versions


def delete_version(org: str, package: str, version_id: int) -> bool:
    encoded = urllib.parse.quote(package, safe="")
    try:
        result = subprocess.run(
            ["gh", "api", "-X", "DELETE",
             f"/orgs/{org}/packages/container/{encoded}/versions/{version_id}"],
            capture_output=True, text=True,
        )
    except FileNotFoundError:
        sys.stderr.write("error: `gh` CLI is required but not found on PATH\n")
        return False
    if result.returncode != 0:
        sys.stderr.write(
            f"  ! delete failed for version {version_id}: {result.stderr.strip()}\n"
        )
        return False
    return True


def cmd_list(args: argparse.Namespace, gh_token: str) -> int:
    org = args.org
    package = args.package

    print(f"Package: ghcr.io/{org}/{package}")
    versions = list_versions(org, package)
    if not versions:
        print("  (no versions found)")
        return 0

    tagged = [
        v for v in versions
        if v.get("metadata", {}).get("container", {}).get("tags")
    ]
    tagged.sort(key=lambda v: v.get("created_at", ""), reverse=True)

    if not tagged:
        print("  (no tagged versions)")
    else:
        token = get_ghcr_token(org, package, gh_token)
        for version in tagged:
            tags = sorted(version["metadata"]["container"]["tags"])
            date = version.get("created_at", "")[:10]
            size = (
                get_manifest_size(org, package, tags[0], token) if token else None
            )
            print(
                f"  - {', '.join(tags)}  size: {fmt_size(size)}  uploaded: {date}"
            )

    if args.remove_untagged_older_than is not None:
        prune(org, package, versions, args.remove_untagged_older_than)

    return 0


def prune(org: str, package: str, versions: list[dict], hours: int) -> None:
    cutoff = dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=hours)
    print(
        f"\nPruning versions of {package} with no human tags older than "
        f"{hours}h (before {cutoff.isoformat()}):"
    )

    candidates = []
    for v in versions:
        tags = v.get("metadata", {}).get("container", {}).get("tags") or []
        if tags and not all(is_commit_tag(t) for t in tags):
            continue
        created = parse_created_at(v.get("created_at", ""))
        if created is None or created >= cutoff:
            continue
        candidates.append((v, tags, created))

    if not candidates:
        print("  (nothing to prune)")
        return

    deleted = 0
    for v, tags, created in candidates:
        vid = v.get("id")
        label = ", ".join(tags) if tags else "<untagged>"
        date_str = created.strftime("%Y-%m-%d %H:%M:%SZ")
        if delete_version(org, package, vid):
            deleted += 1
            print(f"  - deleted version {vid} ({label}) created {date_str}")
    print(f"  pruned {deleted}/{len(candidates)} version(s)")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="packages.py",
        description="List GHCR container packages and optionally prune commit-only tags.",
    )
    parser.add_argument(
        "--org",
        default=os.environ.get("ORG", DEFAULT_ORG),
        help=f"GitHub org (default: env ORG or {DEFAULT_ORG})",
    )
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_list = sub.add_parser("list", help="List versions of a package")
    p_list.add_argument(
        "--package", required=True, help="Container package name (without org prefix)"
    )
    p_list.add_argument(
        "--remove-untagged-older-than",
        type=int,
        metavar="HOURS",
        default=None,
        help=(
            "If set, delete versions older than HOURS hours that are either "
            "untagged or tagged only by commit hash (hex)."
        ),
    )
    p_list.set_defaults(func=cmd_list)

    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    gh_token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN") or ""
    if not gh_token:
        sys.stderr.write("error: GH_TOKEN (or GITHUB_TOKEN) must be set\n")
        return 2

    return args.func(args, gh_token)


if __name__ == "__main__":
    raise SystemExit(main())
