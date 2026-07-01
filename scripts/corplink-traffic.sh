#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG="${CORPLINK_CONFIG:-"$ROOT/config.local.json"}"
BIN="${CORPLINK_BIN:-"$ROOT/target/release/corplink-rs"}"
RUN_DIR="$ROOT/.run"
PID_FILE="$RUN_DIR/corplink-traffic.pid"
LOG_FILE="$RUN_DIR/corplink-traffic.log"
LOG_LEVEL="${RUST_LOG:-info}"
TEST_REPO="${TEST_REPO:-}"
TEST_HOST="${TEST_HOST:-}"
TEST_PORT="${TEST_PORT:-}"

usage() {
  cat <<'EOF'
Usage: scripts/corplink-traffic.sh <command>

Commands:
  start       Start corplink-rs in the background
  foreground  Run corplink-rs in the foreground
  stop        Stop the background corplink-rs process
  restart     Stop, then start
  status      Show process, interface, route, and managed source status
  preflight   Resolve managed_routes without printing config secrets
  test        Test GitHub repo access; requires TEST_REPO
  test-host   Test the inferred route target and optional TEST_PORT TCP connectivity
  logs        Print recent logs
  logs -f     Follow logs
EOF
}

config_value() {
  python3 - "$CONFIG" "$1" "$2" <<'PY'
import json
import sys

path, key, default = sys.argv[1:4]
try:
    with open(path, encoding="utf-8") as file:
        data = json.load(file)
except FileNotFoundError:
    print(default)
    raise SystemExit

value = data.get(key, default)
print(value if value is not None else default)
PY
}

interface_name() {
  config_value interface_name utun12345
}

route_check_host() {
  python3 - "$CONFIG" "${TEST_HOST:-}" "${TEST_PORT:-}" <<'PY'
import json
import sys

config_path, explicit_host, explicit_port = sys.argv[1:4]
if explicit_host:
    print(explicit_host)
    raise SystemExit

try:
    with open(config_path, encoding="utf-8") as file:
        data = json.load(file)
except Exception:
    print("github.com")
    raise SystemExit

managed = data.get("managed_routes")
sources = []
if isinstance(managed, dict) and managed.get("enabled", True) is not False:
    configured_sources = managed.get("sources")
    if isinstance(configured_sources, list):
        sources = configured_sources

if explicit_port:
    for source in sources:
        if not isinstance(source, dict) or source.get("type") != "dns_hosts":
            continue
        if str(source.get("port", "")) != explicit_port:
            continue
        hosts = source.get("hosts")
        if not isinstance(hosts, list):
            continue
        for host in hosts:
            if isinstance(host, str) and host.strip():
                print(host.strip())
                raise SystemExit

for source in sources:
    if isinstance(source, dict) and source.get("type") == "github_meta":
        print("github.com")
        raise SystemExit

for source in sources:
    if not isinstance(source, dict) or source.get("type") != "dns_hosts":
        continue
    hosts = source.get("hosts")
    if not isinstance(hosts, list):
        continue
    for host in hosts:
        if isinstance(host, str) and host.strip():
            print(host.strip())
            raise SystemExit

print("github.com")
PY
}

ensure_config() {
  if [[ ! -f "$CONFIG" ]]; then
    echo "missing config: $CONFIG" >&2
    exit 1
  fi
}

ensure_bin() {
  if [[ ! -x "$BIN" ]]; then
    echo "missing binary, building release target..."
    (cd "$ROOT" && cargo build --release)
  fi
}

read_pid() {
  if [[ -f "$PID_FILE" ]]; then
    cat "$PID_FILE"
  fi
}

is_running() {
  local pid
  pid="$(read_pid || true)"
  [[ -n "${pid:-}" ]] && ps -p "$pid" >/dev/null 2>&1
}

resolve_host_ips() {
  python3 - "$CONFIG" "$1" <<'PY'
import ipaddress
import json
import socket
import sys
import urllib.parse
import urllib.request

config_path, host = sys.argv[1:3]
include_ipv6 = False
try:
    with open(config_path, encoding="utf-8") as file:
        data = json.load(file)
    managed = data.get("managed_routes")
    include_ipv6 = bool(isinstance(managed, dict) and managed.get("include_ipv6", False))
except Exception:
    pass

def is_fake_ip(ip):
    parsed = ipaddress.ip_address(ip)
    if parsed.version != 4:
        return False
    return ipaddress.ip_address("198.18.0.0") <= parsed <= ipaddress.ip_address("198.19.255.255")

def doh(record_type):
    url = "https://cloudflare-dns.com/dns-query?name={}&type={}".format(
        urllib.parse.quote(host),
        record_type,
    )
    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/dns-json",
            "User-Agent": "corplink-rs-managed-routes",
        },
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        payload = json.load(response)
    expected = {"A": 1, "AAAA": 28}[record_type]
    for answer in payload.get("Answer") or []:
        if isinstance(answer, dict) and answer.get("type") == expected:
            value = answer.get("data")
            if isinstance(value, str):
                yield str(ipaddress.ip_address(value))

seen = set()
try:
    record_types = ["A", "AAAA"] if include_ipv6 else ["A"]
    for record_type in record_types:
        for ip in doh(record_type):
            if not is_fake_ip(ip) and ip not in seen:
                print(ip)
                seen.add(ip)
except Exception:
    for item in socket.getaddrinfo(host, None, proto=socket.IPPROTO_TCP):
        ip = item[4][0]
        if not is_fake_ip(ip) and ip not in seen:
            print(ip)
            seen.add(ip)
PY
}

route_interface_for() {
  local ip="$1"
  case "$(uname -s)" in
    Darwin)
      route -n get "$ip" 2>/dev/null | awk '/interface:/{print $2; exit}'
      ;;
    Linux)
      ip route get "$ip" 2>/dev/null | awk '{for (i=1; i<=NF; i++) if ($i=="dev") {print $(i+1); exit}}'
      ;;
    *)
      return 1
      ;;
  esac
}

managed_preflight() {
  ensure_config
  python3 "$ROOT/scripts/update-managed-routes.py" "$CONFIG" --dry-run
}

managed_summary() {
  ensure_config
  python3 - "$ROOT" "$CONFIG" <<'PY'
import json
import subprocess
import sys

root, config = sys.argv[1:3]
cmd = [sys.executable, f"{root}/scripts/update-managed-routes.py", config, "--dry-run"]
result = subprocess.run(cmd, text=True, capture_output=True)
if result.returncode != 0:
    print(f"managed_routes: failed: {result.stderr.strip() or result.stdout.strip()}")
    raise SystemExit

cache = json.loads(result.stdout)
sources = cache.get("sources") or []
if not sources:
    print("managed_routes: disabled or empty")
    raise SystemExit
for source in sources:
    print(
        f"managed_routes: {source['name']} "
        f"({source['source_type']}): {len(source.get('routes') or [])} routes"
    )
PY
}

wait_ready() {
  local host iface ip route_iface
  host="$(route_check_host)"
  iface="$(interface_name)"
  for _ in {1..25}; do
    ip="$(resolve_host_ips "$host" | head -1 || true)"
    route_iface=""
    if [[ -n "${ip:-}" ]]; then
      route_iface="$(route_interface_for "$ip" || true)"
    fi

    if ifconfig "$iface" >/dev/null 2>&1 && [[ "$route_iface" == "$iface" ]]; then
      echo "ready: ${host} ${ip} via ${iface}"
      return 0
    fi

    if ! is_running; then
      echo "corplink-rs exited before becoming ready" >&2
      show_logs >&2 || true
      return 1
    fi
    sleep 1
  done

  echo "started, but ${host} route is not ready yet" >&2
  status >&2
  show_logs >&2 || true
  return 1
}

start() {
  ensure_config
  ensure_bin
  mkdir -p "$RUN_DIR"

  if is_running; then
    echo "already running: pid $(read_pid)"
    status
    return
  fi

  if ! managed_preflight >/dev/null; then
    echo "managed_routes preflight failed; corplink-rs may still use a fresh cache" >&2
  fi
  : > "$LOG_FILE"

  echo "starting corplink-rs in background; sudo may ask for your macOS password"
  sudo sh -c 'cd "$1" || exit 1; RUST_LOG="$2" nohup "$3" "$4" >> "$5" 2>&1 & echo $! > "$6"' \
    sh "$ROOT" "$LOG_LEVEL" "$BIN" "$CONFIG" "$LOG_FILE" "$PID_FILE"

  sleep 1
  wait_ready
  echo "logs: $LOG_FILE"
}

foreground() {
  ensure_config
  ensure_bin
  echo "running in foreground; press Ctrl-C to stop"
  sudo -E RUST_LOG="$LOG_LEVEL" "$BIN" "$CONFIG"
}

stop() {
  local pid
  pid="$(read_pid || true)"
  if [[ -z "${pid:-}" ]]; then
    echo "not running: no pid file"
    return
  fi
  if ps -p "$pid" >/dev/null 2>&1; then
    echo "stopping pid $pid"
    sudo kill "$pid" || true
    sleep 1
  fi
  rm -f "$PID_FILE"
}

status() {
  local host iface ip route_iface pid
  host="${1:-$(route_check_host)}"
  iface="$(interface_name)"
  pid="$(read_pid || true)"
  if [[ -n "${pid:-}" ]] && ps -p "$pid" >/dev/null 2>&1; then
    echo "process: running pid $pid"
  else
    echo "process: stopped"
  fi

  if ifconfig "$iface" >/dev/null 2>&1; then
    echo "interface: $iface up"
  else
    echo "interface: $iface not found"
  fi

  while read -r ip; do
    [[ -z "$ip" ]] && continue
    route_iface="$(route_interface_for "$ip" || true)"
    echo "${host}: $ip via ${route_iface:-unknown}"
  done < <(resolve_host_ips "$host" || true)

  managed_summary || true
}

test_repo() {
  if [[ -z "$TEST_REPO" ]]; then
    echo "missing TEST_REPO; example: TEST_REPO=git@github.com:owner/repo.git scripts/corplink-traffic.sh test" >&2
    return 1
  fi
  status "${TEST_HOST:-github.com}"
  GIT_SSH_COMMAND='ssh -o BatchMode=yes -o ConnectTimeout=10' \
    git ls-remote "$TEST_REPO" HEAD
}

test_host() {
  local host iface failed ip route_iface seen_ip
  host="$(route_check_host)"
  iface="$(interface_name)"
  failed=0
  seen_ip=0
  while read -r ip; do
    [[ -z "$ip" ]] && continue
    seen_ip=1
    route_iface="$(route_interface_for "$ip" || true)"
    echo "${host}: $ip via ${route_iface:-unknown}"
    if [[ "$route_iface" != "$iface" ]]; then
      failed=1
    fi
  done < <(resolve_host_ips "$host")

  if [[ "$seen_ip" -eq 0 ]]; then
    echo "route check failed: ${host} resolved no usable addresses" >&2
    return 1
  fi

  if [[ "$failed" -ne 0 ]]; then
    echo "route check failed: expected interface $iface" >&2
    return 1
  fi

  if [[ -n "$TEST_PORT" ]]; then
    nc -vz -G 10 "$host" "$TEST_PORT" 2>/dev/null || nc -vz -w 10 "$host" "$TEST_PORT"
  fi
}

show_logs() {
  mkdir -p "$RUN_DIR"
  if [[ "${1:-}" == "-f" ]]; then
    tail -f "$LOG_FILE"
  else
    tail -100 "$LOG_FILE"
  fi
}

case "${1:-}" in
  start)
    start
    ;;
  foreground)
    foreground
    ;;
  stop)
    stop
    ;;
  restart)
    stop
    start
    ;;
  status)
    status
    ;;
  preflight)
    managed_preflight
    ;;
  test)
    test_repo
    ;;
  test-host)
    test_host
    ;;
  logs)
    shift
    show_logs "$@"
    ;;
  -h|--help|help|"")
    usage
    ;;
  *)
    echo "unknown command: $1" >&2
    usage >&2
    exit 1
    ;;
esac
