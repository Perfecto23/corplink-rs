#!/usr/bin/env python3
"""Compatibility facade for the Rust managed-routes CLI.

Route parsing, source fingerprints, cache fallback, and atomic writes live in
corplink-rs. This script keeps the historical config/--dry-run/--write-cache
entry point without maintaining a second resolver.
"""

from __future__ import annotations

import argparse
import os
import pathlib
import shutil
import subprocess
import sys


ROOT = pathlib.Path(__file__).resolve().parents[1]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Resolve corplink-rs managed_routes without printing secrets."
    )
    parser.add_argument("config", type=pathlib.Path, help="Path to corplink-rs config.json")
    parser.add_argument(
        "--write-cache",
        action="store_true",
        help="Write the resolved managed routes cache atomically.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Resolve and print the report without writing the cache (default).",
    )
    return parser.parse_args()


def find_binary() -> str:
    configured = os.environ.get("CORPLINK_BIN")
    if configured:
        return configured
    for candidate in (
        ROOT / "target" / "release" / "corplink-rs",
        ROOT / "corplink-rs",
    ):
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
    found = shutil.which("corplink-rs")
    if found:
        return found
    debug = ROOT / "target" / "debug" / "corplink-rs"
    if debug.is_file() and os.access(debug, os.X_OK):
        print(
            "warning: using target/debug/corplink-rs; set CORPLINK_BIN for an exact runtime binary",
            file=sys.stderr,
        )
        return str(debug)
    raise FileNotFoundError(
        "corplink-rs binary not found; build it or set CORPLINK_BIN"
    )


def main() -> int:
    args = parse_args()
    if args.write_cache and args.dry_run:
        print("managed_routes: --dry-run and --write-cache cannot be combined", file=sys.stderr)
        return 2
    try:
        command = [find_binary(), "routes", str(args.config)]
        if args.write_cache:
            command.append("--write-cache")
        return subprocess.run(command, check=False).returncode
    except (OSError, ValueError) as error:
        print(f"managed_routes preflight failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
