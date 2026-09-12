#!/usr/bin/env python3
"""Behavior tests for the reversible, local runtime CLI seam."""

import os
import pathlib
import json
import plistlib
import signal
import shutil
import subprocess
import tempfile
import time
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "corplink-traffic.sh"


class RuntimeCliTests(unittest.TestCase):
    def run_cli(self, run_dir: pathlib.Path, *args: str):
        env = os.environ.copy()
        env.update(
            {
                "CORPLINK_RUN_DIR": str(run_dir),
                "CORPLINK_CONFIG": str(run_dir / "config.json"),
                "CORPLINK_BIN": str(run_dir / "fake-corplink"),
                "CORPLINK_PLATFORM": "Linux",
                "CORPLINK_MONITOR_BACKEND": "process",
                "CORPLINK_SUDO": str(run_dir / "fake-sudo"),
                "TEST_HOST": "127.0.0.1",
            }
        )
        return subprocess.run(
            [str(SCRIPT), *args],
            cwd=ROOT,
            env=env,
            text=True,
            capture_output=True,
        )

    def test_status_reports_stopped_as_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = pathlib.Path(directory)
            result = self.run_cli(run_dir, "status")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("process: stopped", result.stdout)

    def test_systemd_restarts_panics_but_not_handled_failures(self):
        for unit_name in ("corplink-rs.service", "corplink-rs@.service"):
            unit = (ROOT / "systemd" / unit_name).read_text(encoding="utf-8")
            self.assertIn("Restart=on-failure", unit)
            self.assertIn("RestartPreventExitStatus=1", unit)
            self.assertIn("RestartSec=5s", unit)
            self.assertIn("StartLimitIntervalSec=300s", unit)
            self.assertIn("StartLimitBurst=3", unit)
            self.assertNotIn("RestartPreventExitStatus=2", unit)
            self.assertNotIn("RestartPreventExitStatus=101", unit)

    def test_corrupt_runtime_state_fails_once_without_rewriting_it(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = pathlib.Path(directory)
            config = run_dir / "config.json"
            binary = run_dir / "fake-corplink"
            state = run_dir / "corplink-runtime.json"
            config.write_text(
                json.dumps({"company_name": "test", "interface_name": "utun-test"}),
                encoding="utf-8",
            )
            binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            binary.chmod(0o755)
            corrupt = b"{ definitely not json\n"
            state.write_bytes(corrupt)
            env = os.environ.copy()
            env.update(
                {
                    "CORPLINK_RUN_DIR": str(run_dir),
                    "CORPLINK_CONFIG": str(config),
                    "CORPLINK_BIN": str(binary),
                    "CORPLINK_STATE_FILE": str(state),
                    "CORPLINK_LOG_FILE": str(run_dir / "runtime.log"),
                    "CORPLINK_RUNTIME_BACKEND": "process",
                    "CORPLINK_PLATFORM": "Linux",
                }
            )
            result = subprocess.run(
                [str(SCRIPT), "_supervise", str(config), "corrupt-generation"],
                cwd=ROOT,
                env=env,
                capture_output=True,
                timeout=5,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("could not parse runtime state", result.stderr.decode())
            self.assertEqual(state.read_bytes(), corrupt)
            self.assertFalse((run_dir / "corplink-runtime.json.lock").exists())
            for _ in range(2):
                released = subprocess.run(
                    [str(SCRIPT), "stop"],
                    cwd=ROOT,
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=5,
                )
                self.assertEqual(released.returncode, 0, released.stderr)
    def test_foreground_registers_lifecycle_and_stop_or_sigint_works(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = pathlib.Path(directory)
            config = run_dir / "config.json"
            config.write_text(
                json.dumps({"company_name": "test", "interface_name": "utun-test"}),
                encoding="utf-8",
            )
            (run_dir / "fake-sudo").write_text(
                "#!/usr/bin/env python3\n"
                "import os, sys\n"
                "os.closerange(3, 256)\n"
                "os.execvp(sys.argv[1], sys.argv[1:])\n",
                encoding="utf-8",
            )
            (run_dir / "ifconfig").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            (run_dir / "ip").write_text(
                "#!/bin/sh\nprintf '%s\\n' '127.0.0.1 dev utun-test src 127.0.0.1'\n",
                encoding="utf-8",
            )
            (run_dir / "fake-corplink").write_text(
                """#!/usr/bin/env bash
set -e
if [ "$1" = "routes" ] || [ "$1" = "routes-status" ]; then exit 0; fi
if [ -n "$CORPLINK_RUNTIME_STATE" ] && [ -f "$CORPLINK_RUNTIME_STATE" ]; then
if IFS= read -r input; then
printf '%s\\n' "$input" > "$CORPLINK_RUN_DIR/foreground-input.txt"
fi
sleep "${FAKE_READY_DELAY:-0}"
python3 - "$CORPLINK_RUNTIME_STATE" "$CORPLINK_RUNTIME_GENERATION" "$$" <<'PY'
import json
import pathlib
import subprocess
import sys
import time
path, generation, pid = sys.argv[1:]
data = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
data.update({"phase": "ready", "ready": True, "pid": int(pid), "process_start": subprocess.check_output(["ps", "-p", pid, "-o", "lstart="], text=True).strip(), "handshake_age_secs": 1, "updated_at": str(time.time()), "generation": generation})
pathlib.Path(path).write_text(json.dumps(data) + "\\n", encoding="utf-8")
PY
fi
trap 'exit 0' TERM INT
while :; do sleep 0.05; done
""",
                encoding="utf-8",
            )
            for path in (run_dir / "fake-sudo", run_dir / "fake-corplink", run_dir / "ifconfig", run_dir / "ip"):
                path.chmod(0o755)
            env = os.environ.copy()
            env.update(
                {
                    "CORPLINK_RUN_DIR": str(run_dir),
                    "CORPLINK_CONFIG": str(config),
                    "CORPLINK_BIN": str(run_dir / "fake-corplink"),
                    "CORPLINK_PLATFORM": "Linux",
                    "CORPLINK_MONITOR_BACKEND": "process",
                    "CORPLINK_SUDO": str(run_dir / "fake-sudo"),
                    "TEST_HOST": "127.0.0.1",
                    "FAKE_READY_DELAY": "2",
                    "PATH": f"{run_dir}{os.pathsep}{os.environ['PATH']}",
                }
            )
            foreground = subprocess.Popen(
                [str(SCRIPT), "foreground"],
                cwd=ROOT,
                env=env,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
                umask=0o077,
            )
            try:
                foreground.stdin.write("otp-from-pipe\n")
                foreground.stdin.flush()
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    try:
                        snapshot = json.loads((run_dir / "corplink-runtime.json").read_text(encoding="utf-8"))
                        if snapshot.get("supervisor_pid") and snapshot.get("phase") in {"connecting", "ready"}:
                            break
                    except (FileNotFoundError, json.JSONDecodeError):
                        pass
                    time.sleep(0.05)
                registered = json.loads((run_dir / "corplink-runtime.json").read_text(encoding="utf-8"))
                self.assertTrue(registered.get("supervisor_pid"))
                self.assertEqual((run_dir / "corplink-runtime.json").stat().st_mode & 0o777, 0o644)
                self.assertIn(registered.get("phase"), {"connecting", "ready"})
                starting_duplicate = subprocess.run([str(SCRIPT), "start"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10)
                self.assertNotEqual(starting_duplicate.returncode, 0)
                self.assertIn("already supervised", starting_duplicate.stderr)
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    try:
                        if json.loads((run_dir / "corplink-runtime.json").read_text(encoding="utf-8")).get("phase") == "ready":
                            break
                    except (FileNotFoundError, json.JSONDecodeError):
                        pass
                    time.sleep(0.05)
                self.assertEqual(json.loads((run_dir / "corplink-runtime.json").read_text(encoding="utf-8")).get("phase"), "ready")
                self.assertEqual((run_dir / "foreground-input.txt").read_text(encoding="utf-8"), "otp-from-pipe\n")
                status = subprocess.run([str(SCRIPT), "status"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10)
                self.assertEqual(status.returncode, 0, status.stdout + status.stderr)
                duplicate = subprocess.run([str(SCRIPT), "start"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10)
                self.assertEqual(duplicate.returncode, 0, duplicate.stderr)
                self.assertIn("already running", duplicate.stdout)
                os.killpg(foreground.pid, signal.SIGINT)
                foreground.wait(timeout=10)
                stdout, stderr = foreground.communicate(timeout=1)
                self.assertEqual(foreground.returncode, 0, stdout + stderr)
                terminal = json.loads((run_dir / "corplink-runtime.json").read_text(encoding="utf-8"))
                self.assertEqual(terminal.get("phase"), "stopped")
                self.assertIsNone(terminal.get("pid"))
                self.assertIsNone(terminal.get("supervisor_pid"))
                stopped = subprocess.run([str(SCRIPT), "status"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10)
                self.assertNotEqual(stopped.returncode, 0)
            finally:
                if foreground.poll() is None:
                    foreground.terminate()
                    foreground.wait(timeout=10)
                foreground.communicate(timeout=1)


    def test_dead_operation_lock_is_reclaimed_but_not_a_live_owner(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = pathlib.Path(directory)
            lock = run_dir / "corplink-traffic.lock"
            lock.mkdir()
            (lock / "pid").write_text("999999999\n", encoding="utf-8")
            env = os.environ.copy()
            env.update(
                {
                    "CORPLINK_RUN_DIR": str(run_dir),
                    "CORPLINK_CONFIG": str(run_dir / "missing-config.json"),
                    "CORPLINK_BIN": str(run_dir / "missing-binary"),
                    "CORPLINK_PLATFORM": "Linux",
                    "CORPLINK_MONITOR_BACKEND": "process",
                }
            )
            result = subprocess.run(
                [str(SCRIPT), "stop"],
                cwd=ROOT,
                env=env,
                capture_output=True,
                text=True,
                timeout=0.5,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(lock.exists())

            lock.mkdir()
            (lock / "pid").write_text("999999999\n", encoding="utf-8")
            contenders = [
                subprocess.Popen(
                    [str(SCRIPT), "stop"],
                    cwd=ROOT,
                    env=env,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                for _ in range(2)
            ]
            results = [process.communicate(timeout=5) for process in contenders]
            self.assertTrue(all(process.returncode == 0 for process in contenders), results)
            self.assertFalse(lock.exists())

            lock.mkdir()
            (lock / "pid").write_text(f"{os.getpid()}\n", encoding="utf-8")
            env["CORPLINK_LOCK_TIMEOUT_SECS"] = "0"
            busy = subprocess.run(
                [str(SCRIPT), "stop"],
                cwd=ROOT,
                env=env,
                capture_output=True,
                text=True,
                timeout=0.5,
            )
            self.assertNotEqual(busy.returncode, 0)
            self.assertTrue(lock.exists())

    def test_start_stop_can_repeat_and_tracks_child_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = pathlib.Path(directory)
            (run_dir / "config.json").write_text(
                json.dumps({"company_name": "test", "interface_name": "utun-test"}),
                encoding="utf-8",
            )
            (run_dir / "fake-sudo").write_text(
                "#!/bin/sh\nexec \"$@\"\n", encoding="utf-8"
            )
            (run_dir / "ifconfig").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            (run_dir / "ip").write_text(
                "#!/bin/sh\nprintf '%s\\n' '127.0.0.1 dev utun-test src 127.0.0.1'\n",
                encoding="utf-8",
            )
            (run_dir / "fake-corplink").write_text(
                """#!/usr/bin/env bash
set -eu
if [ "${1:-}" = "routes" ] || [ "${1:-}" = "routes-status" ]; then exit 0; fi
if [ -z "${CORPLINK_RUNTIME_STATE:-}" ] || [ ! -f "$CORPLINK_RUNTIME_STATE" ]; then exit 0; fi
sleep "${FAKE_READY_DELAY:-0}"
GENERATION="${CORPLINK_RUNTIME_GENERATION:-unknown}"
if [ -n "${CORPLINK_RUN_DIR:-}" ]; then echo "child $$ generation $GENERATION" >> "$CORPLINK_RUN_DIR/fake-child.log"; fi
python3 - "${CORPLINK_RUNTIME_STATE:-/dev/null}" "$GENERATION" "$$" <<'PY'
import json
import pathlib
import subprocess
import sys
import time

path, generation, pid = sys.argv[1:]
data = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
start = subprocess.check_output(["ps", "-p", pid, "-o", "lstart="], text=True).strip()
data.update({"phase": "ready", "ready": True, "pid": int(pid), "process_start": start, "handshake_age_secs": 1, "updated_at": str(time.time()), "generation": generation, "reason": "fake handshake confirmed"})
pathlib.Path(path).write_text(json.dumps(data) + "\\n", encoding="utf-8")
PY
if [ -n "${CORPLINK_RUN_DIR:-}" ]; then echo "ready $$" >> "$CORPLINK_RUN_DIR/fake-child.log"; fi
trap 'sleep 0.5; exit 0' TERM INT
while :; do sleep 0.05; done
""",
                encoding="utf-8",
            )
            for path in (run_dir / "fake-sudo", run_dir / "fake-corplink", run_dir / "ifconfig", run_dir / "ip"):
                path.chmod(0o755)
            path_env = f"{run_dir}{os.pathsep}{os.environ['PATH']}"

            env = os.environ.copy()
            env.update(
                {
                    "CORPLINK_RUN_DIR": str(run_dir),
                    "CORPLINK_CONFIG": str(run_dir / "config.json"),
                    "CORPLINK_BIN": str(run_dir / "fake-corplink"),
                    "CORPLINK_PLATFORM": "Linux",
                    "CORPLINK_MONITOR_BACKEND": "process",
                    "CORPLINK_SUDO": str(run_dir / "fake-sudo"),
                    "TEST_HOST": "127.0.0.1",
                    "CORPLINK_START_TIMEOUT_SECS": "3",
                    "PATH": path_env,
                }
            )

            try:
                first = subprocess.run(
                    [str(SCRIPT), "start"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10
                )
                self.assertEqual(first.returncode, 0, first.stderr)
                state = json.loads((run_dir / "corplink-runtime.json").read_text(encoding="utf-8"))
                self.assertEqual(state["phase"], "ready")
                self.assertNotEqual(state["pid"], state["supervisor_pid"])

                status = subprocess.run(
                    [str(SCRIPT), "status"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10
                )
                self.assertEqual(status.returncode, 0, status.stderr)

                state["handshake_age_secs"] = 250
                state["updated_at"] = str(time.time() - 100)
                (run_dir / "corplink-runtime.json").write_text(json.dumps(state), encoding="utf-8")
                stale_health = subprocess.run(
                    [str(SCRIPT), "status"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10
                )
                self.assertNotEqual(stale_health.returncode, 0)
                state["handshake_age_secs"] = 1
                state["updated_at"] = str(time.time())
                (run_dir / "corplink-runtime.json").write_text(json.dumps(state), encoding="utf-8")

                state["process_start"] = "a different process start token"
                (run_dir / "corplink-runtime.json").write_text(json.dumps(state), encoding="utf-8")
                reused = subprocess.run(
                    [str(SCRIPT), "status"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10
                )
                self.assertNotEqual(reused.returncode, 0)
                state["process_start"] = subprocess.check_output(
                    ["ps", "-p", str(state["pid"]), "-o", "lstart="], text=True
                ).strip()
                (run_dir / "corplink-runtime.json").write_text(json.dumps(state), encoding="utf-8")

                (run_dir / "fail-kill").write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
                (run_dir / "fail-kill").chmod(0o755)
                env["CORPLINK_KILL"] = str(run_dir / "fail-kill")
                failed_stop = subprocess.run(
                    [str(SCRIPT), "stop"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10
                )
                self.assertNotEqual(failed_stop.returncode, 0)
                stopping_state = json.loads((run_dir / "corplink-runtime.json").read_text(encoding="utf-8"))
                self.assertEqual(stopping_state["intent"], "stopping")
                self.assertTrue((run_dir / "corplink-traffic.pid").exists())
                env.pop("CORPLINK_KILL")

                stop = subprocess.run(
                    [str(SCRIPT), "stop"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10
                )
                self.assertEqual(stop.returncode, 0, stop.stderr)

                second = subprocess.run(
                    [str(SCRIPT), "start"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10
                )
                self.assertEqual(second.returncode, 0, second.stderr)
                log_text = (run_dir / "corplink-traffic.log").read_text(encoding="utf-8")
                self.assertGreaterEqual(log_text.count("corplink runtime event: start"), 2)

                env["FAKE_READY_DELAY"] = "5"
                slow = subprocess.run(
                    [str(SCRIPT), "stop"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10
                )
                self.assertEqual(slow.returncode, 0, slow.stderr)
                slow_start = subprocess.run(
                    [str(SCRIPT), "start"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10
                )
                self.assertNotEqual(slow_start.returncode, 0)
                connecting = json.loads((run_dir / "corplink-runtime.json").read_text(encoding="utf-8"))
                self.assertEqual(connecting["intent"], "running")
                self.assertIn(connecting["phase"], {"connecting", "degraded"})
                time.sleep(3.5)
                late_status = subprocess.run(
                    [str(SCRIPT), "status"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10
                )
                self.assertEqual(late_status.returncode, 0, late_status.stderr)
                env.pop("FAKE_READY_DELAY")
            finally:
                subprocess.run([str(SCRIPT), "stop"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10)
                shutil.rmtree(run_dir / "corplink-traffic.lock", ignore_errors=True)
    def test_macos_default_launchd_bootstrap_waits_for_ready_child(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = pathlib.Path(directory)
            (run_dir / "config.json").write_text(
                json.dumps({"company_name": "test", "interface_name": "utun-test"}),
                encoding="utf-8",
            )
            (run_dir / "fake-sudo").write_text(
                """#!/usr/bin/env python3
import argparse
import json
import os
import pathlib
import shutil
import sys

args = sys.argv[1:]
root = pathlib.Path(os.environ["CORPLINK_RUN_DIR"])
if args[0] == "install":
    parser = argparse.ArgumentParser()
    parser.add_argument("-o")
    parser.add_argument("-g")
    parser.add_argument("-m")
    parser.add_argument("source")
    parser.add_argument("target")
    options = parser.parse_args(args[1:])
    shutil.copyfile(options.source, options.target)
    pathlib.Path(options.target).chmod(int(options.m, 8))
    (root / "published-plist.json").write_text(json.dumps({
        "path": options.target, "owner": options.o, "group": options.g,
        "mode": int(options.m, 8),
    }))
    raise SystemExit(0)
if args[0] == "-u":
    with (root / "sudo-switch.log").open("a") as stream:
        stream.write(" ".join(args) + "\\n")
    args = args[2:]
os.execvp(args[0], args)
""",
                encoding="utf-8",
            )
            (run_dir / "ifconfig").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            (run_dir / "route").write_text(
                "#!/bin/sh\nprintf '%s\\n' 'interface: utun-test'\n", encoding="utf-8"
            )
            (run_dir / "fake-corplink").write_text(
                """#!/usr/bin/env bash
set -e
if [ "$1" = "routes" ] || [ "$1" = "routes-status" ]; then exit 0; fi
if [ -z "$CORPLINK_RUNTIME_STATE" ] || [ ! -f "$CORPLINK_RUNTIME_STATE" ]; then exit 0; fi
if [ "$CORPLINK_FAKE_ALWAYS_FAIL" = "1" ]; then
python3 - "$CORPLINK_RUNTIME_STATE" "$CORPLINK_RUNTIME_GENERATION" <<'PY'
import json
import pathlib
import sys

path, generation = sys.argv[1:]
data = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
data.update({"phase": "failed", "intent": "failed", "ready": False, "generation": generation, "reason": "authentication required"})
pathlib.Path(path).write_text(json.dumps(data) + "\\n", encoding="utf-8")
PY
exit 1
fi

python3 - "$CORPLINK_RUNTIME_STATE" "$CORPLINK_RUNTIME_GENERATION" "$$" <<'PY'
import json
import pathlib
import subprocess
import sys
import time

path, generation, pid = sys.argv[1:]
data = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
start = subprocess.check_output(["ps", "-p", pid, "-o", "lstart="], text=True).strip()
data.update({"phase": "ready", "ready": True, "pid": int(pid), "process_start": start, "handshake_age_secs": 1, "updated_at": str(time.time()), "generation": generation})
pathlib.Path(path).write_text(json.dumps(data) + "\\n", encoding="utf-8")
PY
trap 'exit 0' TERM INT
while :; do sleep 0.05; done
""",
                encoding="utf-8",
            )
            (run_dir / "fake-launchctl").write_text(
                """#!/usr/bin/env python3
import json
import os
import pathlib
import plistlib
import signal
import subprocess
import sys
import time

command = sys.argv[1]
registry = pathlib.Path(os.environ["CORPLINK_RUN_DIR"]) / "registered-job.json"
if command == "asuser":
    subprocess.run(sys.argv[3:], check=False)
elif command == "print":
    if not registry.exists():
        raise SystemExit(113)
    job = json.loads(registry.read_text())
    process = subprocess.run(["ps", "-p", str(job["pid"]), "-o", "stat="], capture_output=True, text=True)
    active = process.returncode == 0 and not process.stdout.strip().startswith("Z")
    print("state = " + ("running" if active else "not running"))
    print("program = " + job["arguments"][0])
    print("arguments = {")
    for argument in job["arguments"]:
        print(argument)
    print("}")
    if active:
        print("pid = " + str(job["pid"]))
elif command == "bootstrap":
    if registry.exists():
        raise SystemExit("Bootstrap failed: job is already registered")
    plist = pathlib.Path(sys.argv[3])
    publication = pathlib.Path(os.environ["CORPLINK_RUN_DIR"]) / "published-plist.json"
    if not publication.exists():
        raise SystemExit("system launchd rejected a plist not published with root ownership")
    published = json.loads(publication.read_text())
    if published != {"path": str(plist), "owner": "root", "group": "wheel", "mode": 0o644}:
        raise SystemExit("system launchd rejected plist ownership or permissions")
    data = plistlib.loads(plist.read_bytes())
    env = os.environ.copy()
    env.update(data.get("EnvironmentVariables", {}))
    child = subprocess.Popen(data["ProgramArguments"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    registry.write_text(json.dumps({"pid": child.pid, "arguments": data["ProgramArguments"]}))
    # launchd may return after the child has already published readiness.
    # A root-written PID file is not writable by the foreground user.
    state_path = pathlib.Path(env["CORPLINK_STATE_FILE"])
    deadline = time.monotonic() + 4
    while time.monotonic() < deadline:
        try:
            snapshot = json.loads(state_path.read_text())
        except (FileNotFoundError, json.JSONDecodeError):
            snapshot = {}
        if snapshot.get("phase") in {"ready", "failed"}:
            pid_path = pathlib.Path(env["CORPLINK_RUN_DIR"]) / "corplink-traffic.pid"
            if pid_path.exists():
                pid_path.chmod(0o444)
            break
        time.sleep(0.02)
elif command == "bootout":
    state_path = pathlib.Path(os.environ.get("CORPLINK_STATE_FILE", pathlib.Path(os.environ["CORPLINK_RUN_DIR"]) / "corplink-runtime.json"))
    data = json.loads(state_path.read_text(encoding="utf-8"))
    pid = data.get("supervisor_pid")
    if pid:
        os.kill(pid, signal.SIGTERM)
        # bootout acknowledges removal before every process has exited.
    registry.unlink(missing_ok=True)
else:
    raise SystemExit(2)
""",
                encoding="utf-8",
            )
            (run_dir / "fake-osascript").write_text(
                "#!/usr/bin/env python3\nimport pathlib, sys\npathlib.Path(sys.argv[0]).with_name('notify-calls.log').open('a').write(' '.join(sys.argv[1:]) + '\\n')\n",
                encoding="utf-8",
            )
            for path in run_dir.iterdir():
                if path.name.startswith("fake-") or path.name in {"ifconfig", "route"}:
                    path.chmod(0o755)
            env = os.environ.copy()
            env.update(
                {
                    "CORPLINK_RUN_DIR": str(run_dir),
                    "CORPLINK_CONFIG": str(run_dir / "config.json"),
                    "CORPLINK_BIN": str(run_dir / "fake-corplink"),
                    "CORPLINK_PLATFORM": "Darwin",
                    "CORPLINK_MONITOR_BACKEND": "launchd",
                    "CORPLINK_LAUNCHCTL": str(run_dir / "fake-launchctl"),
                    "CORPLINK_OSASCRIPT": str(run_dir / "fake-osascript"),
                    "CORPLINK_SUDO": str(run_dir / "fake-sudo"),
                    "TEST_HOST": "127.0.0.1",
                    "CORPLINK_START_TIMEOUT_SECS": "5",
                    "PATH": f"{run_dir}{os.pathsep}{os.environ['PATH']}",
                }
            )
            try:
                start = subprocess.run([str(SCRIPT), "start"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=15)
                self.assertEqual(start.returncode, 0, start.stderr)
                plist_files = list(run_dir.glob("com.corplink-rs*.plist"))
                self.assertEqual(len(plist_files), 1)
                plist = plistlib.loads(plist_files[0].read_bytes())
                self.assertEqual(plist["KeepAlive"], {"SuccessfulExit": False})
                self.assertEqual(plist["ProgramArguments"][1], "_supervise")

                status = subprocess.run([str(SCRIPT), "status"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10)
                self.assertEqual(status.returncode, 0, status.stderr)

                stop_before_failure = subprocess.run([str(SCRIPT), "stop"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10)
                self.assertEqual(stop_before_failure.returncode, 0, stop_before_failure.stdout + stop_before_failure.stderr)
                env["CORPLINK_FAKE_ALWAYS_FAIL"] = "1"
                failed = subprocess.run([str(SCRIPT), "start"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=15)
                self.assertNotEqual(failed.returncode, 0)
                notifications = (run_dir / "notify-calls.log").read_text(encoding="utf-8").splitlines()
                self.assertEqual(len(notifications), 1)
                sudo_switch = (run_dir / "sudo-switch.log").read_text(encoding="utf-8")
                self.assertIn(f"-u #{os.getuid()}", sudo_switch)
                self.assertIn(str(run_dir / "fake-osascript"), sudo_switch)
                env["CORPLINK_NOTIFY"] = "0"
                failed_disabled = subprocess.run([str(SCRIPT), "start"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=15)
                self.assertNotEqual(failed_disabled.returncode, 0)
                self.assertNotIn("Bootstrap failed", failed_disabled.stderr)
                self.assertEqual(len((run_dir / "notify-calls.log").read_text(encoding="utf-8").splitlines()), 1)
                env["CORPLINK_NOTIFY"] = "1"
                env.pop("CORPLINK_FAKE_ALWAYS_FAIL")
                crash_state = {
                    "schema_version": 1,
                    "generation": "crash-budget-generation",
                    "intent": "running",
                    "phase": "ready",
                    "ready": True,
                    "pid": None,
                    "supervisor_pid": None,
                    "backend": "launchd",
                    "binary": str(run_dir / "fake-corplink"),
                    "config": str(run_dir / "config.json"),
                    "restart_count": 0,
                    "supervisor_crash_count": 3,
                    "last_exit": None,
                    "reason": "ready",
                    "updated_at": "0",
                }
                (run_dir / "corplink-runtime.json").write_text(json.dumps(crash_state), encoding="utf-8")
                crash_budget = subprocess.run(
                    [str(SCRIPT), "_supervise", str(run_dir / "config.json"), "crash-budget-generation"],
                    cwd=ROOT,
                    env=env,
                    text=True,
                    capture_output=True,
                    timeout=10,
                )
                self.assertEqual(crash_budget.returncode, 0, crash_budget.stderr)
                self.assertEqual(len((run_dir / "notify-calls.log").read_text(encoding="utf-8").splitlines()), 2)
            finally:
                subprocess.run([str(SCRIPT), "stop"], cwd=ROOT, env=env, text=True, capture_output=True, timeout=10)




if __name__ == "__main__":
    unittest.main()
