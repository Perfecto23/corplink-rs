#!/usr/bin/env python3
import pathlib
import subprocess
import sys


def main() -> int:
    print(
        "deprecated: forwarding to scripts/update-managed-routes.py; "
        "managed_routes.sources is the single route source",
        file=sys.stderr,
    )
    script = pathlib.Path(__file__).with_name("update-managed-routes.py")
    return subprocess.run([sys.executable, str(script), *sys.argv[1:]], check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())
