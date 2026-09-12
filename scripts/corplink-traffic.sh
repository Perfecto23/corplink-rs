#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG="${CORPLINK_CONFIG:-"$ROOT/config.local.json"}"
if [[ -n "${CORPLINK_BIN:-}" ]]; then
  BIN="$CORPLINK_BIN"
elif [[ -x "$ROOT/target/release/corplink-rs" ]]; then
  BIN="$ROOT/target/release/corplink-rs"
elif [[ -x "$ROOT/corplink-rs" ]]; then
  BIN="$ROOT/corplink-rs"
else
  BIN="$ROOT/target/release/corplink-rs"
fi
RUN_DIR="${CORPLINK_RUN_DIR:-"$ROOT/.run"}"
PID_FILE="$RUN_DIR/corplink-traffic.pid"
CHILD_PID_FILE="$RUN_DIR/corplink-traffic.child.pid"
STATE_FILE="${CORPLINK_STATE_FILE:-"$RUN_DIR/corplink-runtime.json"}"
STATE_LOCK_DIR="$STATE_FILE.lock"
EVENT_FILE="$RUN_DIR/corplink-runtime-events.log"
LOG_FILE="$RUN_DIR/corplink-traffic.log"
STOP_FILE="$RUN_DIR/corplink-traffic.stop"
LOCK_FILE="$RUN_DIR/corplink-traffic.lockfile"
# Versions before the file-descriptor lock used this directory as the
# operation lock. Keep it as a legacy marker and retire it only while the
# kernel lock is held, so two new contenders cannot race its cleanup.
LEGACY_LOCK_DIR="$RUN_DIR/corplink-traffic.lock"
LOCK_FD=9
LOCK_FD_OPEN=0
LOCK_HELD=0
LOG_LEVEL="${RUST_LOG:-info}"
TEST_REPO="${TEST_REPO:-}"
TEST_HOST="${TEST_HOST:-}"
TEST_PORT="${TEST_PORT:-}"
PLATFORM="${CORPLINK_PLATFORM:-$(uname -s)}"
MAX_RESTARTS="${CORPLINK_MAX_RESTARTS:-3}"
START_TIMEOUT_SECS="${CORPLINK_START_TIMEOUT_SECS:-45}"
STOP_TIMEOUT_SECS="${CORPLINK_STOP_TIMEOUT_SECS:-20}"

absolutize_path() {
  case "$1" in
    /*) printf '%s\n' "$1" ;;
    *) printf '%s/%s\n' "$PWD" "$1" ;;
  esac
}

CONFIG="$(absolutize_path "$CONFIG")"
BIN="$(absolutize_path "$BIN")"
RUN_DIR="$(absolutize_path "$RUN_DIR")"
PID_FILE="$RUN_DIR/corplink-traffic.pid"
CHILD_PID_FILE="$RUN_DIR/corplink-traffic.child.pid"
STATE_FILE="$(absolutize_path "$STATE_FILE")"
STATE_LOCK_DIR="$STATE_FILE.lock"
EVENT_FILE="$RUN_DIR/corplink-runtime-events.log"
LOG_FILE="$(absolutize_path "$LOG_FILE")"
STOP_FILE="$RUN_DIR/corplink-traffic.stop"
LOCK_FILE="$RUN_DIR/corplink-traffic.lockfile"
LEGACY_LOCK_DIR="$RUN_DIR/corplink-traffic.lock"

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

run_privileged() {
  local sudo_cmd="${CORPLINK_SUDO:-sudo}"
  "$sudo_cmd" "$@"
}

state_write_initial() {
  local generation="$1" backend="$2" tmp
  mkdir -p "$RUN_DIR"
  acquire_state_lock || return 1
  tmp="$STATE_FILE.$$.$RANDOM.tmp"
  python3 - "$tmp" "$generation" "$backend" "$BIN" "$CONFIG" "$LOG_FILE" <<'PY'
import json
import pathlib
import sys
import time

path, generation, backend, binary, config, log_file = sys.argv[1:]
data = {
    "schema_version": 1,
    "generation": generation,
    "intent": "running",
    "phase": "starting",
    "ready": False,
    "pid": None,
    "supervisor_pid": None,
    "backend": backend,
    "binary": binary,
    "config": config,
    "log_file": log_file,
    "restart_count": 0,
    "supervisor_crash_count": 0,
    "last_exit": None,
    "reason": "starting",
    "updated_at": str(time.time()),
}
pathlib.Path(path).write_text(json.dumps(data, sort_keys=True, indent=2) + "\n", encoding="utf-8")
PY
  chmod 0644 "$tmp"
  mv -f "$tmp" "$STATE_FILE"
  release_state_lock
}

state_update() {
  local tmp="$STATE_FILE.$$.$RANDOM.tmp"
  acquire_state_lock || return 1
  if ! python3 - "$STATE_FILE" "$tmp" "$@" <<'PY'
import json
import pathlib
import sys
import time

source, target, *pairs = sys.argv[1:]
try:
    data = json.loads(pathlib.Path(source).read_text(encoding="utf-8"))
except FileNotFoundError:
    data = {}
except json.JSONDecodeError as error:
    print(f"could not parse runtime state {source}: {error}", file=sys.stderr)
    raise SystemExit(75)
if len(pairs) % 2:
    raise SystemExit("state_update requires key/value pairs")
for index in range(0, len(pairs), 2):
    key, value = pairs[index:index + 2]
    if value == "<null>":
        data[key] = None
    elif value == "<true>":
        data[key] = True
    elif value == "<false>":
        data[key] = False
    elif key in {"pid", "supervisor_pid", "restart_count", "supervisor_crash_count", "last_exit"} and value.isdigit():
        data[key] = int(value)
    else:
        data[key] = value
data["updated_at"] = str(time.time())
pathlib.Path(target).write_text(json.dumps(data, sort_keys=True, indent=2) + "\n", encoding="utf-8")
PY
  then
    rm -f "$tmp"
    release_state_lock
    return 1
  fi
  chmod 0644 "$tmp"
  mv -f "$tmp" "$STATE_FILE"
  release_state_lock
  return 0
}

state_get() {
  local key="$1"
  python3 - "$STATE_FILE" "$key" <<'PY'
import json
import pathlib
import sys

try:
    value = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")).get(sys.argv[2])
except (FileNotFoundError, json.JSONDecodeError):
    value = None
if value is not None:
    print(value)
PY
}

append_log_marker() {
  mkdir -p "$RUN_DIR"
  python3 - "$LOG_FILE" "$1" <<'PY'
from datetime import datetime, timezone
import pathlib
import sys

path, marker = sys.argv[1:3]
pathlib.Path(path).parent.mkdir(parents=True, exist_ok=True)
with open(path, "a", encoding="utf-8") as file:
    file.write(f"\n[{datetime.now(timezone.utc).isoformat(timespec='seconds')}] corplink runtime event: {marker}\n")
PY
}

record_event() {
  local event="$1" detail="${2:-}"
  mkdir -p "$RUN_DIR"
  python3 - "$EVENT_FILE" "$event" "$detail" <<'PY'
from datetime import datetime, timezone
import pathlib
import re
import sys

path, event, detail = sys.argv[1:4]
detail = re.sub(r"(?i)(password|token|secret|cookie|private[_ -]?key)\s*[:=]\s*[^\s,;]+", r"\1=<redacted>", detail)
detail = re.sub(r"\b[A-Fa-f0-9]{32,}\b", "<redacted>", detail)
detail = re.sub(r"\b[A-Z2-7]{16,}\b", "<redacted>", detail)
with open(path, "a", encoding="utf-8") as file:
    file.write(f"{datetime.now(timezone.utc).isoformat(timespec='seconds')} {event}: {detail}\n")
PY
}

notification_uid() {
  local candidate="${CORPLINK_NOTIFY_UID:-${SUDO_UID:-}}"
  if [[ "$candidate" =~ ^[1-9][0-9]*$ ]]; then
    printf '%s\n' "$candidate"
  else
    candidate="$(id -u)"
    if [[ "$candidate" =~ ^[1-9][0-9]*$ ]]; then
      printf '%s\n' "$candidate"
    else
      printf '\n'
    fi
  fi
}

notify_terminal_once() {
  local generation="$1" reason="${2:-}"
  [[ "$PLATFORM" == "Darwin" ]] || return 0
  [[ "${CORPLINK_NOTIFY:-1}" != "0" ]] || return 0
  local uid
  uid="$(notification_uid)"
  [[ "$uid" =~ ^[0-9]+$ ]] || {
    record_event notification-failed "invalid notification uid"
    return 0
  }
  local marker="$RUN_DIR/corplink-notified-$generation"
  mkdir "$marker" 2>/dev/null || return 0
  local message='VPN runtime failed; run corplink status and logs'
  if [[ "$reason" == *authentication* || "$reason" == *interaction* ]]; then
    message='VPN runtime needs authentication; run corplink foreground'
  fi
  local launchctl_cmd="${CORPLINK_LAUNCHCTL:-launchctl}"
  local sudo_cmd="${CORPLINK_SUDO:-sudo}"
  local osascript_cmd="${CORPLINK_OSASCRIPT:-osascript}"
  if "$launchctl_cmd" asuser "$uid" "$sudo_cmd" -u "#$uid" "$osascript_cmd" -e "display notification \"$message\" with title \"Corplink\""; then
    record_event notification-sent "generation=$generation"
  else
    record_event notification-failed "generation=$generation"
  fi
}

acquire_lock() {
  mkdir -p "$RUN_DIR"
  local deadline=$((SECONDS + ${CORPLINK_LOCK_TIMEOUT_SECS:-10}))
  if [[ ! -e "$LOCK_FILE" ]]; then
    if ! (umask 022; : >"$LOCK_FILE"); then
      echo "could not create corplink runtime operation lock: $LOCK_FILE" >&2
      return 1
    fi
    chmod 0644 "$LOCK_FILE" 2>/dev/null || true
  fi
  if ! exec 9<"$LOCK_FILE"; then
    echo "could not open corplink runtime operation lock: $LOCK_FILE" >&2
    return 1
  fi
  LOCK_FD_OPEN=1
  while true; do
    if python3 - "$LOCK_FD" <<'PY'
import fcntl
import sys

try:
    fcntl.flock(int(sys.argv[1]), fcntl.LOCK_EX | fcntl.LOCK_NB)
except BlockingIOError:
    raise SystemExit(1)
PY
    then
      LOCK_HELD=1
      # A legacy directory lock has no kernel lock to coordinate with us.
      # Its owner PID is authoritative only while this new lock is held.
      if [[ -d "$LEGACY_LOCK_DIR" ]]; then
        local owner
        owner=""
        if [[ -f "$LEGACY_LOCK_DIR/pid" ]]; then
          owner="$(sed -n '1p' "$LEGACY_LOCK_DIR/pid" || true)"
        fi
        if [[ "$owner" =~ ^[0-9]+$ ]] && process_alive "$owner"; then
          python3 - "$LOCK_FD" <<'PY'
import fcntl
import sys

fcntl.flock(int(sys.argv[1]), fcntl.LOCK_UN)
PY
          LOCK_HELD=0
          if (( SECONDS >= deadline )); then
            exec 9>&-
            LOCK_FD_OPEN=0
            echo "another corplink runtime operation is in progress: $LEGACY_LOCK_DIR" >&2
            return 1
          fi
          sleep 0.05
          continue
        fi
        rm -f "$LEGACY_LOCK_DIR/pid"
        if ! rmdir "$LEGACY_LOCK_DIR" 2>/dev/null; then
          python3 - "$LOCK_FD" <<'PY'
import fcntl
import sys

fcntl.flock(int(sys.argv[1]), fcntl.LOCK_UN)
PY
          LOCK_HELD=0
          exec 9>&-
          LOCK_FD_OPEN=0
          echo "could not retire legacy corplink runtime operation lock: $LEGACY_LOCK_DIR" >&2
          return 1
        fi
      fi
      trap 'release_lock' EXIT
      return 0
    fi
    if (( SECONDS >= deadline )); then
      exec 9>&-
      LOCK_FD_OPEN=0
      echo "another corplink runtime operation is in progress: $LOCK_FILE" >&2
      return 1
    fi
    sleep 0.05
  done
}

release_lock() {
  trap - EXIT
  if (( LOCK_HELD == 1 )); then
    python3 - "$LOCK_FD" <<'PY'
import fcntl
import sys

fcntl.flock(int(sys.argv[1]), fcntl.LOCK_UN)
PY
    LOCK_HELD=0
  fi
  if (( LOCK_FD_OPEN == 1 )); then
    exec 9>&-
    LOCK_FD_OPEN=0
  fi
}

acquire_state_lock() {
  local deadline=$((SECONDS + 3))
  while ! mkdir "$STATE_LOCK_DIR" 2>/dev/null; do
    if [[ -f "$STATE_LOCK_DIR/pid" ]]; then
      local owner
      owner="$(sed -n '1p' "$STATE_LOCK_DIR/pid" || true)"
      if [[ "$owner" =~ ^[0-9]+$ ]] && ! process_alive "$owner"; then
        rm -f "$STATE_LOCK_DIR/pid"
        rmdir "$STATE_LOCK_DIR" 2>/dev/null || true
        continue
      fi
    fi
    if (( SECONDS >= deadline )); then
      return 1
    fi
    sleep 0.01
  done
  printf '%s\n' "$$" > "$STATE_LOCK_DIR/pid"
}

release_state_lock() {
  rm -f "$STATE_LOCK_DIR/pid"
  rmdir "$STATE_LOCK_DIR" 2>/dev/null || true
}

read_pid() {
  if [[ -f "$STATE_FILE" ]]; then
    local pid
    pid="$(state_get pid || true)"
    if [[ -n "$pid" ]]; then
      printf '%s\n' "$pid"
      return
    fi
  fi
  if [[ -f "$PID_FILE" ]]; then
    if [[ -f "$CHILD_PID_FILE" ]]; then
      sed -n '1p' "$CHILD_PID_FILE"
    else
      sed -n '1p' "$PID_FILE"
    fi
  fi
}

read_supervisor_pid() {
  if [[ -f "$STATE_FILE" ]]; then
    state_get supervisor_pid
  fi
}

read_child_pid() {
  if [[ -f "$STATE_FILE" ]]; then
    local pid
    pid="$(state_get pid || true)"
    [[ "$pid" =~ ^[0-9]+$ ]] && printf '%s\n' "$pid"
  fi
}

process_alive() {
  local pid="$1"
  [[ "$pid" =~ ^[0-9]+$ ]] || return 1
  ps -p "$pid" -o stat= >/dev/null 2>&1 || return 1
  local stat
  stat="$(ps -p "$pid" -o stat= 2>/dev/null | tr -d ' ' || true)"
  [[ "$stat" != Z* ]]
}

process_matches() {
  local pid="$1" binary="${2:-$BIN}" config="${3:-$CONFIG}" expected_start="${4-}"
  process_alive "$pid" || return 1
  local command
  command="$(ps -p "$pid" -o command= 2>/dev/null || true)"
  [[ "$command" == *"$binary"* && "$command" == *"$config"* ]] || return 1
  local expected_start current_start
  if [[ -z "$expected_start" ]]; then
    expected_start="$(state_get process_start || true)"
  fi
  [[ "$expected_start" == "-" ]] && return 0
  if [[ -n "$expected_start" ]]; then
    current_start="$(ps -p "$pid" -o lstart= 2>/dev/null | sed 's/^[[:space:]]*//; s/[[:space:]]*$//' || true)"
    [[ "$current_start" == "$expected_start" ]] || return 1
  fi
}

process_identity_summary() {
  local pid="${1:-}" binary="${2:-$BIN}" config="${3:-$CONFIG}" start_token="${4-}"
  if [[ -n "$pid" ]] && process_matches "$pid" "$binary" "$config" "$start_token"; then
    printf 'valid\n'
  elif [[ -n "$pid" ]] && process_alive "$pid"; then
    printf 'mismatch\n'
  else
    printf 'gone\n'
  fi
}

state_health_fresh() {
  python3 - "$STATE_FILE" "${CORPLINK_HEALTH_STALE_SECS:-300}" <<'PY'
from datetime import datetime, timezone
import json
import math
import pathlib
import sys
import time

try:
    data = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
    updated = data.get("updated_at")
    age = data.get("handshake_age_secs")
    if not isinstance(updated, str) or not isinstance(age, (int, float)):
        raise SystemExit(1)
    try:
        updated_at = float(updated)
    except ValueError:
        updated_at = datetime.fromisoformat(updated.replace("Z", "+00:00")).timestamp()
    now = time.time()
    elapsed = now - updated_at
    age_value = float(age)
    if not math.isfinite(age_value) or age_value < 0:
        raise SystemExit(1)
    if elapsed < 0 or age_value + elapsed > float(sys.argv[2]):
        raise SystemExit(1)
except (FileNotFoundError, json.JSONDecodeError, ValueError, TypeError):
    raise SystemExit(1)
PY
}

state_process_ready() {
  local pid
  pid="$(read_child_pid || true)"
  [[ -n "$pid" ]] || return 1
  process_matches "$pid" "$(state_get binary || printf '%s' "$BIN")" "$(state_get config || printf '%s' "$CONFIG")" || return 1
  [[ "$(state_get intent || true)" == "running" ]] || return 1
  [[ "$(state_get phase || true)" == "ready" ]] || return 1
  [[ "$(state_get ready || true)" == "True" || "$(state_get ready || true)" == "true" ]] || return 1
  state_health_fresh
}

runtime_process_active() {
  local supervisor child binary config
  binary="$(state_get binary || printf '%s' "$BIN")"
  config="$(state_get config || printf '%s' "$CONFIG")"
  supervisor="$(read_supervisor_pid || true)"
  if [[ -n "$supervisor" ]] && process_matches "$supervisor" "$ROOT/scripts/corplink-traffic.sh" "$config" "-"; then
    return 0
  fi
  child="$(read_child_pid || true)"
  if [[ -z "$child" && ! -f "$STATE_FILE" ]]; then
    child="$(read_pid || true)"
  fi
  [[ -n "$child" ]] && process_matches "$child" "$binary" "$config"
}

backend_for() {
  if [[ -n "${CORPLINK_MONITOR_BACKEND:-}" && "$CORPLINK_MONITOR_BACKEND" != "auto" ]]; then
    printf '%s\n' "$CORPLINK_MONITOR_BACKEND"
  elif [[ "$PLATFORM" == "Darwin" ]]; then
    printf 'launchd\n'
  else
    printf 'process\n'
  fi
}

write_pid_file() {
  local pid="$1" temporary
  mkdir -p "$RUN_DIR"
  temporary="$(mktemp "$RUN_DIR/.corplink-pid.XXXXXX")"
  if ! printf '%s\n' "$pid" > "$temporary" || ! chmod 0644 "$temporary"; then
    rm -f "$temporary"
    return 1
  fi
  mv -f "$temporary" "$PID_FILE"
}

stop_requested() {
  [[ -f "$STOP_FILE" ]] || [[ "$(state_get intent || true)" == "stopping" ]] || [[ "$(state_get intent || true)" == "stopped" ]]
}

send_term() {
  local pid="$1"
  if [[ -n "${CORPLINK_KILL:-}" ]]; then
    "$CORPLINK_KILL" -TERM "$pid"
  elif [[ "$(id -u)" == "0" ]]; then
    kill -TERM "$pid"
  else
    run_privileged kill -TERM "$pid"
  fi
}

run_supervisor() {
  local config="$1"
  local generation="$2"
  if [[ "${CORPLINK_FOREGROUND:-0}" == "1" ]]; then
    # sudo preserves standard input but closes inherited extra descriptors.
    exec 8<&0
  fi
  local supervisor_pid="$$"
  local backend="${CORPLINK_RUNTIME_BACKEND:-process}"
  local child_pid=""
  local restart_count=0
  local backoff=1

  local previous_phase previous_intent supervisor_crashes
  previous_phase="$(state_get phase || true)"
  previous_intent="$(state_get intent || true)"
  supervisor_crashes="$(state_get supervisor_crash_count || true)"
  supervisor_crashes="${supervisor_crashes:-0}"
  if [[ "$previous_intent" == "stopping" || "$previous_intent" == "stopped" || "$previous_intent" == "failed" ]]; then
    exit 0
  fi
  if [[ "$previous_intent" == "running" ]] && (( supervisor_crashes >= MAX_RESTARTS )); then
    state_update intent failed phase failed ready "<false>" reason "supervisor crash limit reached" supervisor_crash_count "$supervisor_crashes" || true
    notify_terminal_once "$generation" "supervisor crash limit reached"
    record_event supervisor-limit "generation=$generation attempts=$supervisor_crashes"
    exit 0
  fi
  supervisor_crashes=$((supervisor_crashes + 1))

  trap '
    stop_ok=1
    if [[ -n "${child_pid:-}" ]] && process_alive "$child_pid"; then
      if ! send_term "$child_pid" >/dev/null 2>&1; then
        stop_ok=0
      else
        for _ in {1..40}; do
          process_alive "$child_pid" || break
          sleep 0.5
        done
        process_alive "$child_pid" && stop_ok=0
      fi
    fi
    if (( stop_ok == 0 )); then
      state_update intent stopping phase stopping ready "<false>" reason "child stop failed or timed out; process retained" || true
      exit 1
    fi
    if [[ "$(state_get phase || true)" == "failed" && "$(state_get intent || true)" == "failed" ]]; then
      state_update pid "<null>" supervisor_pid "<null>" || true
      rm -f "$CHILD_PID_FILE" "$STOP_FILE" "$PID_FILE"
      exit 0
    fi
    state_update intent stopped phase stopped ready "<false>" pid "<null>" supervisor_pid "<null>" reason "stopped by user" last_exit "<null>" || true
    rm -f "$CHILD_PID_FILE" "$STOP_FILE" "$PID_FILE"
    exit 0
  ' TERM

  state_update intent running phase starting ready "<false>" pid "<null>" supervisor_pid "$supervisor_pid" generation "$generation" backend "$backend" supervisor_crash_count "$supervisor_crashes" reason "supervisor started" restart_count 0 last_exit "<null>"
  write_pid_file "$supervisor_pid"
  while true; do
    if stop_requested; then
      state_update intent stopped phase stopped ready "<false>" pid "<null>" supervisor_pid "<null>" reason "stop requested" || true
      rm -f "$CHILD_PID_FILE" "$STOP_FILE" "$PID_FILE"
      exit 0
    fi

    state_update intent running phase connecting ready "<false>" pid "<null>" supervisor_pid "$supervisor_pid" generation "$generation" backend "$backend" reason "starting child" restart_count "$restart_count" last_exit "<null>"
    if [[ "${CORPLINK_FOREGROUND:-0}" == "1" ]]; then
      (
        export CORPLINK_RUNTIME_STATE="$STATE_FILE"
        export CORPLINK_RUNTIME_GENERATION="$generation"
        export CORPLINK_RUNTIME_LOG="$LOG_FILE"
        export RUST_LOG="$LOG_LEVEL"
        exec "$BIN" "$config" <&8
      ) &
    else
      (
        export CORPLINK_RUNTIME_STATE="$STATE_FILE"
        export CORPLINK_RUNTIME_GENERATION="$generation"
        export CORPLINK_RUNTIME_LOG="$LOG_FILE"
        export RUST_LOG="$LOG_LEVEL"
        exec "$BIN" "$config"
      ) >> "$LOG_FILE" 2>&1 &
    fi
    child_pid=$!
    printf '%s\n' "$child_pid" > "$CHILD_PID_FILE"

    set +e
    wait "$child_pid"
    local exit_code=$?
    set -e
    rm -f "$CHILD_PID_FILE"
    child_pid=""

    if stop_requested; then
      state_update intent stopped phase stopped ready "<false>" pid "<null>" supervisor_pid "<null>" reason "stopped by user" last_exit "$exit_code" || true
      rm -f "$STOP_FILE" "$PID_FILE"
      exit 0
    fi

    # Rust owns terminal authentication/configuration failures. Preserve that
    # diagnostic and return success so launchd's SuccessfulExit=false policy
    # does not create an outer infinite retry loop.
    if [[ "$(state_get phase || true)" == "failed" && "$(state_get intent || true)" == "failed" ]]; then
      notify_terminal_once "$generation" "$(state_get reason || true)"
      record_event terminal-child "generation=$generation reason=$(state_get reason || true)"
      exit 0
    fi

    if (( exit_code == 0 )); then
      state_update intent failed phase failed ready "<false>" pid "<null>" supervisor_pid "<null>" backend "$backend" reason "child exited unexpectedly" last_exit 0 || true
      notify_terminal_once "$generation" "child exited unexpectedly"
      record_event child-exit "generation=$generation exit=0"
      exit 0
    fi

    restart_count=$((restart_count + 1))
    if (( restart_count >= MAX_RESTARTS )); then
      state_update intent failed phase failed ready "<false>" pid "<null>" supervisor_pid "<null>" backend "$backend" reason "restart limit reached" last_exit "$exit_code" restart_count "$restart_count" || true
      notify_terminal_once "$generation" "restart limit reached"
      record_event restart-limit "generation=$generation attempts=$restart_count exit=$exit_code"
      exit 0
    fi

    state_update intent running phase degraded ready "<false>" pid "<null>" supervisor_pid "$supervisor_pid" backend "$backend" reason "child exited; bounded recovery pending" last_exit "$exit_code" restart_count "$restart_count" || true
    record_event child-restart "generation=$generation attempt=$restart_count exit=$exit_code"
    sleep "$backoff"
    backoff=$((backoff * 2))
  done
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
    literal = ipaddress.ip_address(host)
except ValueError:
    literal = None
if literal is not None:
    print(literal)
    raise SystemExit
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
  case "$PLATFORM" in
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
  CORPLINK_BIN="$BIN" python3 "$ROOT/scripts/update-managed-routes.py" "$CONFIG" --dry-run
}

managed_summary() {
  ensure_config
  if ! "$BIN" routes-status "$CONFIG"; then
    echo "managed_routes: status unavailable" >&2
    return 1
  fi
}

wait_ready() {
  local host iface ip route_iface phase pid netstack_listen
  local deadline=$((SECONDS + START_TIMEOUT_SECS))
  host="$(route_check_host)"
  iface="$(interface_name)"
  netstack_listen="$(config_value socks5_listen "")"
  while (( SECONDS < deadline )); do
    phase="$(state_get phase || true)"
    pid="$(read_child_pid || true)"

    # Check process identity before interface or route observations. A stale
    # route must never make a dead or reused PID appear ready.
    if [[ "$phase" == "failed" || "$phase" == "stopped" || "$phase" == "" ]]; then
      echo "corplink-rs failed before becoming ready: ${phase:-unknown}" >&2
      status >&2 || true
      return 1
    fi
    if [[ -n "$pid" ]]; then
      if ! process_matches "$pid" "$(state_get binary || printf '%s' "$BIN")" "$(state_get config || printf '%s' "$CONFIG")"; then
        echo "corplink-rs process identity is gone or mismatched before readiness" >&2
        state_update phase failed intent failed ready "<false>" reason "process identity mismatch before readiness" || true
        record_event identity-mismatch "pid=$pid"
        return 1
      fi
    else
      local supervisor_pid
      supervisor_pid="$(read_supervisor_pid || true)"
      if [[ -z "$supervisor_pid" ]]; then
        sleep 0.1
        continue
      fi
      if ! process_matches "$supervisor_pid" "$ROOT/scripts/corplink-traffic.sh" "$CONFIG" "-"; then
        echo "corplink-rs supervisor identity is gone before readiness" >&2
        state_update phase failed intent failed ready "<false>" reason "supervisor identity missing before readiness" || true
        record_event identity-mismatch "supervisor=$supervisor_pid command=$(ps -p "$supervisor_pid" -o command= 2>/dev/null || true)"
        return 1
      fi
    fi

    # The Rust child publishes ready only after a current WireGuard handshake.
    if [[ "$phase" == "ready" ]] && state_process_ready; then
      if [[ -n "$netstack_listen" ]]; then
        echo "ready: ${netstack_listen} (current handshake)"
        return 0
      fi
      ip="$(resolve_host_ips "$host" | head -1 || true)"
      route_iface=""
      if [[ -n "${ip:-}" ]]; then
        route_iface="$(route_interface_for "$ip" || true)"
      fi
      if ifconfig "$iface" >/dev/null 2>&1 && [[ "$route_iface" == "$iface" ]]; then
        echo "ready: ${host} ${ip} via ${iface}"
        return 0
      fi
    fi
    sleep 1
  done

  echo "started, but current handshake/readiness for ${host} did not complete before deadline; supervised process remains running" >&2
  state_update phase degraded intent running ready "<false>" reason "CLI readiness wait deadline exceeded; supervisor continues" || true
  record_event readiness-timeout "host=$host interface=$iface"
  status >&2
  show_logs >&2 || true
  return 1
}

launchd_domain() {
  if [[ -n "${CORPLINK_LAUNCHD_DOMAIN:-}" ]]; then
    printf '%s\n' "$CORPLINK_LAUNCHD_DOMAIN"
  else
    # Kernel-TUN mode needs the privileged system domain. The foreground
    # `start` command performs sudo before bootstrap, so a password prompt is
    # never deferred into an invisible launchd job.
    printf 'system\n'
  fi
}

launchd_label() {
  if [[ -n "${CORPLINK_LAUNCHD_LABEL:-}" ]]; then
    printf '%s\n' "$CORPLINK_LAUNCHD_LABEL"
  else
    printf 'com.corplink-rs.%s\n' "$(interface_name | tr -c 'A-Za-z0-9' '-')"
  fi
}

write_launchd_plist() {
  local generation="$1" label plist staging
  label="$(launchd_label)"
  plist="$RUN_DIR/$label.plist"
  staging="$(mktemp "$RUN_DIR/.launchd-plist.XXXXXX")"
  if ! python3 - "$staging" "$label" "$ROOT/scripts/corplink-traffic.sh" "$CONFIG" "$generation" "$RUN_DIR" "$BIN" "$STATE_FILE" "$LOG_FILE" "$LOG_LEVEL" <<'PY'
import pathlib
import plistlib
import os
import sys

path, label, script, config, generation, run_dir, binary, state, log_file, log_level = sys.argv[1:]
data = {
    "Label": label,
    "ProgramArguments": [script, "_supervise", config, generation],
    "RunAtLoad": True,
    # The supervisor returns zero after user stop or a bounded terminal
    # failure, so launchd's outer policy does not restart forever.
    "KeepAlive": {"SuccessfulExit": False},
    "ThrottleInterval": 5,
    "ExitTimeOut": 20,
    "ProcessType": "Background",
    "StandardOutPath": log_file,
    "StandardErrorPath": log_file,
    "EnvironmentVariables": {
        "CORPLINK_RUN_DIR": run_dir,
        "CORPLINK_CONFIG": config,
        "CORPLINK_BIN": binary,
        "CORPLINK_STATE_FILE": state,
        "CORPLINK_LOG_FILE": log_file,
        "CORPLINK_GENERATION": generation,
        "CORPLINK_RUNTIME_BACKEND": "launchd",
        "CORPLINK_NOTIFY": os.environ.get("CORPLINK_NOTIFY", "1"),
        "CORPLINK_NOTIFY_UID": os.environ.get("CORPLINK_NOTIFY_UID", ""),
        "CORPLINK_LAUNCHCTL": os.environ.get("CORPLINK_LAUNCHCTL", "launchctl"),
        "CORPLINK_OSASCRIPT": os.environ.get("CORPLINK_OSASCRIPT", "osascript"),
        "RUST_LOG": log_level,
    },
}
pathlib.Path(path).write_bytes(plistlib.dumps(data, fmt=plistlib.FMT_XML, sort_keys=False))
PY
  then
    rm -f "$staging"
    return 1
  fi
  # System-domain plists must be root-owned. Keep the user's staging file
  # separate so subsequent starts can replace an already privileged copy.
  if ! run_privileged install -o root -g wheel -m 0644 "$staging" "$plist"; then
    rm -f "$staging"
    return 1
  fi
  rm -f "$staging"
  printf '%s\n' "$plist"
}

launchctl_cmd() {
  printf '%s\n' "${CORPLINK_LAUNCHCTL:-launchctl}"
}

retire_inactive_launchd_job() {
  local ctl target description result
  ctl="$(launchctl_cmd)"
  target="$(launchd_domain)/$(launchd_label)"
  if description="$("$ctl" print "$target" 2>/dev/null)"; then
    # An exited job remains registered. Only retire this checkout/config's
    # inactive definition; never unload an active or unrelated job.
    if ! printf '%s\n' "$description" | python3 -c '
import pathlib, sys
program = state = None
arguments = []
reading_arguments = False
for raw in sys.stdin:
    line = raw.strip()
    if line.startswith("state = ") and state is None:
        state = line.split(" = ", 1)[1]
    if line.startswith("program = ") and program is None:
        program = line.split(" = ", 1)[1].strip("\"")
    if line == "arguments = {":
        reading_arguments = True
    elif reading_arguments and line == "}":
        reading_arguments = False
    elif reading_arguments:
        arguments.append(line.strip("\""))
expected_program, expected_config = sys.argv[1:]
owned = program == expected_program and len(arguments) >= 3
owned = owned and arguments[0] == expected_program and arguments[1] == "_supervise"
owned = owned and pathlib.Path(arguments[2]).resolve() == pathlib.Path(expected_config).resolve()
if not owned or state != "not running":
    raise SystemExit("existing launchd job is active or belongs to another configuration")
' "$ROOT/scripts/corplink-traffic.sh" "$CONFIG"; then
      return 1
    fi
    run_privileged "$ctl" bootout "$target"
  else
    result=$?
    if [[ "$result" != "113" ]]; then
      echo "could not inspect launchd job $target (exit $result)" >&2
      return 1
    fi
  fi
}

start_launchd() {
  local generation="$1" domain plist ctl
  domain="$(launchd_domain)"
  if ! plist="$(write_launchd_plist "$generation")"; then
    state_update backend launchd phase failed intent failed reason "launchd plist publication failed" || true
    record_event launchd-plist "privileged publication failed"
    return 1
  fi
  ctl="$(launchctl_cmd)"
  if ! run_privileged "$ctl" bootstrap "$domain" "$plist"; then
    state_update backend launchd phase failed intent failed reason "launchd bootstrap failed" || true
    record_event launchd-bootstrap "domain=$domain"
    return 1
  fi
  # The supervisor owns PID and readiness publication. It may already be
  # ready when bootstrap returns, so the caller must not reset its state.
  record_event launchd-bootstrap "accepted generation=$generation"
  wait_ready
}

start_process() {
  local generation="$1" supervisor_pid
  run_privileged env \
    CORPLINK_RUN_DIR="$RUN_DIR" \
    CORPLINK_CONFIG="$CONFIG" \
    CORPLINK_BIN="$BIN" \
    CORPLINK_STATE_FILE="$STATE_FILE" \
    CORPLINK_LOG_FILE="$LOG_FILE" \
    CORPLINK_RUNTIME_BACKEND="process" \
    CORPLINK_NOTIFY="${CORPLINK_NOTIFY:-1}" \
    CORPLINK_NOTIFY_UID="$CORPLINK_NOTIFY_UID" \
    CORPLINK_LAUNCHCTL="${CORPLINK_LAUNCHCTL:-launchctl}" \
    CORPLINK_OSASCRIPT="${CORPLINK_OSASCRIPT:-osascript}" \
    "$ROOT/scripts/corplink-traffic.sh" _supervise "$CONFIG" "$generation" 9>&- >> "$LOG_FILE" 2>&1 &
}

start() {
  ensure_config
  ensure_bin
  acquire_lock
  local backend generation
  backend="$(backend_for)"
  generation="$(date +%s)-$$"

  if state_process_ready; then
    echo "already running: pid $(read_pid)"
    release_lock
    return 0
  fi

  if runtime_process_active; then
    local old_supervisor old_child
    old_supervisor="$(read_supervisor_pid || true)"
    old_child="$(read_child_pid || true)"
    echo "already supervised: supervisor=${old_supervisor:-none} child=${old_child:-none} phase=$(state_get phase || true)" >&2
    release_lock
    return 1
  fi

  if [[ "$backend" == "launchd" ]] && ! retire_inactive_launchd_job; then
    release_lock
    return 1
  fi

  rm -f "$STOP_FILE"
  export CORPLINK_NOTIFY_UID="$(notification_uid)"
  state_write_initial "$generation" "$backend"
  append_log_marker "start generation=$generation backend=$backend"
  record_event start "generation=$generation backend=$backend"

  if ! managed_preflight >/dev/null; then
    echo "managed_routes preflight failed; corplink-rs will report the connection result" >&2
    record_event managed-routes "preflight failed"
  fi

  case "$backend" in
    launchd)
      start_launchd "$generation"
      ;;
    process)
      start_process "$generation"
      wait_ready
      ;;
    *)
      state_update phase failed intent failed reason "unsupported monitor backend: $backend" || true
      release_lock
      return 1
      ;;
  esac
  echo "logs: $LOG_FILE"
  release_lock
}

foreground() {
  ensure_config
  ensure_bin
  acquire_lock
  local generation launcher_pid supervisor_pid child_status phase cleanup_deadline
  if ! run_privileged true; then
    release_lock
    return 1
  fi
  generation="$(date +%s)-$$"
  if state_process_ready || runtime_process_active; then
    echo "already supervised: foreground cannot start another runtime" >&2
    release_lock
    return 1
  fi
  rm -f "$STOP_FILE"
  export CORPLINK_NOTIFY_UID="$(notification_uid)"
  state_write_initial "$generation" "process"
  append_log_marker "foreground generation=$generation"
  echo "running in foreground; press Ctrl-C to stop"
  exec 8<&0
  run_privileged env \
    CORPLINK_RUN_DIR="$RUN_DIR" \
    CORPLINK_CONFIG="$CONFIG" \
    CORPLINK_BIN="$BIN" \
    CORPLINK_STATE_FILE="$STATE_FILE" \
    CORPLINK_LOG_FILE="$LOG_FILE" \
    CORPLINK_RUNTIME_BACKEND="process" \
    CORPLINK_FOREGROUND="1" \
    RUST_LOG="$LOG_LEVEL" \
    "$ROOT/scripts/corplink-traffic.sh" _supervise "$CONFIG" "$generation" <&8 9>&- &
  launcher_pid=$!
  exec 8<&-
  for _ in {1..40}; do
    supervisor_pid="$(read_supervisor_pid || true)"
    [[ -n "$supervisor_pid" ]] && break
    sleep 0.05
  done
  if [[ -z "${supervisor_pid:-}" ]]; then
    phase="$(state_get phase || true)"
    touch "$STOP_FILE"
    if process_alive "$launcher_pid"; then
      (trap - INT TERM; send_term "$launcher_pid" >/dev/null 2>&1 || true) &
    fi
    cleanup_deadline=$((SECONDS + STOP_TIMEOUT_SECS))
    set +e
    while process_alive "$launcher_pid" && (( SECONDS < cleanup_deadline )); do
      sleep 0.1
    done
    if process_alive "$launcher_pid"; then
      state_update intent stopping phase stopping ready "<false>" reason "foreground supervisor startup cleanup timed out" || true
      release_lock
      return 1
    fi
    wait "$launcher_pid" 2>/dev/null || true
    set -e
    state_update intent failed phase failed ready "<false>" reason "foreground supervisor did not publish identity (phase=${phase:-unknown})" || true
    release_lock
    return 1
  fi
  release_lock
  trap 'touch "$STOP_FILE" 2>/dev/null || true; (trap - INT TERM; send_term "$supervisor_pid" >/dev/null 2>&1 || true) &' INT TERM
  set +e
  while process_alive "$launcher_pid"; do
    sleep 0.1
  done
  wait "$launcher_pid" 2>/dev/null
  child_status=$?
  set -e
  trap - INT TERM
  for _ in {1..50}; do
    phase="$(state_get phase || true)"
    [[ "$phase" == "stopped" || "$phase" == "failed" ]] && break
    sleep 0.1
  done
  if [[ "$phase" == "failed" ]]; then
    return 1
  fi
  if [[ "$phase" == "stopped" ]]; then
    return 0
  fi
  return "$child_status"
}

wait_for_exit() {
  local pid="$1"
  local deadline=$((SECONDS + STOP_TIMEOUT_SECS))
  while (( SECONDS < deadline )); do
    if ! process_alive "$pid"; then
      return 0
    fi
    sleep 0.25
  done
  return 1
}

stop() {
  acquire_lock
  local pid supervisor backend phase intent identity target
  pid="$(read_pid || true)"
  supervisor="$(read_supervisor_pid || true)"
  backend="$(state_get backend || backend_for)"
  phase="$(state_get phase || true)"
  intent="$(state_get intent || true)"

  if [[ -z "$pid" && -z "$supervisor" && "$phase" != "starting" && "$phase" != "connecting" && "$phase" != "ready" && "$intent" != "running" ]]; then
    echo "not running: no active runtime"
    release_lock
    return 0
  fi

  touch "$STOP_FILE"
  state_update intent stopping phase stopping ready "<false>" reason "stop requested" || true
  record_event stop "generation=$(state_get generation || true)"

  if [[ "$backend" == "launchd" ]]; then
    local domain label ctl
    domain="$(launchd_domain)"
    label="$(launchd_label)"
    ctl="$(launchctl_cmd)"
    if ! run_privileged "$ctl" bootout "$domain/$label"; then
      state_update intent stopping phase stopping reason "launchd bootout failed" || true
      record_event launchd-stop "domain=$domain label=$label"
      release_lock
      return 1
    fi
  else
    target=""
    if [[ -n "$supervisor" ]]; then
      identity="$(process_identity_summary "$supervisor" "$ROOT/scripts/corplink-traffic.sh" "$CONFIG" "-")"
      if [[ "$identity" == "mismatch" ]]; then
        state_update intent stopping phase stopping reason "refused to signal mismatched supervisor identity" || true
        record_event identity-mismatch "pid=$supervisor"
        release_lock
        return 1
      elif [[ "$identity" == "valid" ]]; then
        target="$supervisor"
      fi
    fi
    [[ -n "$target" ]] || target="$pid"
    if [[ -n "$target" ]]; then
      if [[ "$target" == "$pid" ]]; then
        identity="$(process_identity_summary "$target" "$(state_get binary || printf '%s' "$BIN")" "$(state_get config || printf '%s' "$CONFIG")")"
      elif [[ "$target" != "$supervisor" ]]; then
        identity="$(process_identity_summary "$target" "$(state_get binary || printf '%s' "$BIN")" "$(state_get config || printf '%s' "$CONFIG")")"
      fi
      if [[ "$identity" == "mismatch" ]]; then
        state_update intent stopping phase stopping reason "refused to signal mismatched process identity" || true
        record_event identity-mismatch "pid=$target"
        release_lock
        return 1
      fi
      if [[ "$identity" == "valid" ]]; then
        if ! send_term "$target"; then
          state_update intent stopping phase stopping reason "stop signal failed" || true
          record_event stop-failed "pid=$target"
          release_lock
          return 1
        fi
        if ! wait_for_exit "$target"; then
          state_update intent stopping phase stopping reason "stop timed out; process retained" || true
          record_event stop-timeout "pid=$target"
          release_lock
          return 1
        fi
      fi
    fi
  fi

  if [[ -n "$pid" ]] && ! wait_for_exit "$pid"; then
    state_update intent stopping phase stopping reason "child did not exit before stop deadline" || true
    release_lock
    return 1
  fi
  if [[ -n "$supervisor" ]] && ! wait_for_exit "$supervisor"; then
    state_update intent stopping phase stopping reason "supervisor did not exit before stop deadline" || true
    release_lock
    return 1
  fi
  if [[ "$(state_get phase || true)" == "failed" && "$(state_get intent || true)" == "failed" ]]; then
    rm -f "$PID_FILE" "$CHILD_PID_FILE" "$STOP_FILE"
    append_log_marker "stop generation=$(state_get generation || true) cleanup-failed"
    release_lock
    return 1
  fi
  state_update intent stopped phase stopped ready "<false>" pid "<null>" supervisor_pid "<null>" reason "stopped" last_exit "<null>" || true
  rm -f "$PID_FILE" "$CHILD_PID_FILE" "$STOP_FILE"
  append_log_marker "stop generation=$(state_get generation || true)"
  release_lock
}

status() {
  local host iface ip route_iface pid supervisor phase intent generation backend reason identity rc
  host="${1:-$(route_check_host)}"
  iface="$(interface_name)"
  pid="$(read_pid || true)"
  supervisor="$(read_supervisor_pid || true)"
  phase="$(state_get phase || true)"
  intent="$(state_get intent || true)"
  generation="$(state_get generation || true)"
  backend="$(state_get backend || backend_for)"
  reason="$(state_get reason || true)"
  identity="$(process_identity_summary "$pid" "$(state_get binary || printf '%s' "$BIN")" "$(state_get config || printf '%s' "$CONFIG")")"

  if [[ -n "$pid" ]] && [[ "$identity" == "valid" ]]; then
    echo "process: running pid $pid"
  else
    echo "process: stopped"
  fi
  echo "runtime: phase=${phase:-unknown} intent=${intent:-unknown} backend=$backend generation=${generation:-unknown}"
  local health_age health_state
  health_age="$(state_get handshake_age_secs || true)"
  health_state="stale"
  if state_health_fresh; then
    health_state="current"
  fi
  echo "runtime: child_identity=$identity supervisor_pid=${supervisor:-none} health=$health_state handshake_age_secs=${health_age:-unknown}"
  [[ -z "$reason" ]] || echo "runtime: reason=$reason"

  if [[ -f "$CONFIG" ]]; then
    if ifconfig "$iface" >/dev/null 2>&1; then
      echo "interface: $iface up"
    else
      echo "interface: $iface not found"
    fi
    echo "route: active probe deferred (use test-host for a live route check of ${host})"
    managed_summary || true
  else
    echo "interface: unavailable (missing config)"
  fi

  rc=1
  if state_process_ready; then
    rc=0
  fi
  return "$rc"
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
  _supervise)
    [[ $# -eq 3 ]] || { echo "internal supervise requires config and generation" >&2; exit 2; }
    CONFIG="$2"
    run_supervisor "$2" "$3"
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
