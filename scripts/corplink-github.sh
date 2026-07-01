#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export TEST_HOST="${TEST_HOST:-github.com}"
exec "$ROOT/scripts/corplink-traffic.sh" "$@"
