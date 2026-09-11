#!/usr/bin/env python3
"""Isolated C ABI probe for the patched userspace WireGuard bridge.

This is intentionally named outside ``test_*.py`` so normal unittest
discovery does not start a userspace listener. Each case runs in a child
process because an unpatched Go panic must not terminate the whole probe run.
"""

from __future__ import annotations

import argparse
import ctypes
import os
import re
import socket
import subprocess
import sys
from pathlib import Path


LOG_LEVEL_ERROR = 1
PROTOCOL_UDP = 0
ADDRESS = b"10.0.0.2/24"
DNS = b"10.0.0.53"
USER = b""
PASSWORD = b""
MTU = 1420
ERRNO_RE = re.compile(r"(?:^|\n)errno=(-?\d+)(?:\n|$)")


def load_bridge(path: Path):
    bridge = ctypes.CDLL(str(path))
    bridge.startWgNetstack.argtypes = [
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_int,
    ]
    bridge.startWgNetstack.restype = ctypes.c_int
    bridge.stopWg.argtypes = []
    bridge.stopWg.restype = None
    bridge.uapi.argtypes = [ctypes.c_char_p]
    bridge.uapi.restype = ctypes.c_void_p
    libc = ctypes.CDLL(None)
    libc.free.argtypes = [ctypes.c_void_p]
    libc.free.restype = None
    return bridge, libc


def uapi_text(bridge, libc) -> str:
    pointer = bridge.uapi(b"get=1\n\n")
    if not pointer:
        raise AssertionError("uapi returned a null pointer")
    try:
        return ctypes.string_at(pointer).decode("utf-8", errors="replace")
    finally:
        libc.free(pointer)


def errno_value(response: str) -> int:
    match = ERRNO_RE.search(response)
    if not match:
        raise AssertionError("uapi response did not contain errno")
    return int(match.group(1))


def start(bridge, port: int) -> int:
    return bridge.startWgNetstack(
        LOG_LEVEL_ERROR,
        PROTOCOL_UDP,
        ADDRESS,
        DNS,
        f"127.0.0.1:{port}".encode(),
        USER,
        PASSWORD,
        MTU,
    )


def reserve_port() -> tuple[socket.socket, int]:
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    return listener, listener.getsockname()[1]


def greeting(port: int) -> None:
    with socket.create_connection(("127.0.0.1", port), timeout=3) as client:
        client.sendall(b"\x05\x01\x00")
        response = client.recv(2)
    if response != b"\x05\x00":
        raise AssertionError(f"unexpected SOCKS greeting response: {response!r}")


def case_unstarted(bridge, libc) -> None:
    bridge.stopWg()
    bridge.stopWg()
    if errno_value(uapi_text(bridge, libc)) == 0:
        raise AssertionError("unstarted uapi reported errno=0")


def case_occupied(bridge, libc) -> None:
    occupied, port = reserve_port()
    try:
        result = start(bridge, port)
        if result == 0:
            raise AssertionError("start unexpectedly succeeded on an occupied port")
        if errno_value(uapi_text(bridge, libc)) == 0:
            raise AssertionError("failed start left uapi available")
    finally:
        bridge.stopWg()
        occupied.close()


def case_stop_releases_port(bridge, libc) -> None:
    del libc
    occupied, port = reserve_port()
    occupied.close()
    if start(bridge, port) != 0:
        raise AssertionError("userspace netstack failed to start")
    greeting(port)
    bridge.stopWg()
    bridge.stopWg()
    rebound = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        rebound.bind(("127.0.0.1", port))
    finally:
        rebound.close()


def case_restart(bridge, libc) -> None:
    del libc
    occupied, port = reserve_port()
    occupied.close()
    for _ in range(2):
        if start(bridge, port) != 0:
            raise AssertionError("userspace netstack failed to restart")
        greeting(port)
        bridge.stopWg()
        bridge.stopWg()


CASES = {
    "unstarted": case_unstarted,
    "occupied": case_occupied,
    "stop-releases-port": case_stop_releases_port,
    "restart": case_restart,
}


def child(library: Path, case_name: str) -> int:
    bridge, libc = load_bridge(library)
    CASES[case_name](bridge, libc)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("library", type=Path)
    parser.add_argument("--child", choices=sorted(CASES))
    args = parser.parse_args()
    if sys.platform not in {"darwin", "linux"}:
        print(f"SKIP: unsupported host for FFI probe: {sys.platform}", file=sys.stderr)
        return 77
    if not args.library.is_file():
        print(f"library does not exist: {args.library}", file=sys.stderr)
        return 2
    if args.child:
        child(args.library, args.child)
        return 0

    failures: list[str] = []
    for case_name in CASES:
        result = subprocess.run(
            [sys.executable, str(Path(__file__).resolve()), str(args.library), "--child", case_name],
            capture_output=True,
            text=True,
            timeout=20,
        )
        if result.returncode != 0:
            failures.append(
                f"{case_name}: exit={result.returncode}; "
                f"stdout={result.stdout[-400:]!r}; stderr={result.stderr[-400:]!r}"
            )
        else:
            print(f"ffi probe {case_name}: ok")
    if failures:
        for failure in failures:
            print(failure, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
