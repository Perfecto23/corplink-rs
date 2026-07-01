#!/usr/bin/env python3
import sys


def main() -> int:
    print(
        "deprecated: use scripts/update-managed-routes.py with managed_routes.github_meta; "
        "this tool no longer rewrites config extra_allowed_ips",
        file=sys.stderr,
    )
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
