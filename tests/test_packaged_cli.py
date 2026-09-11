"""Exercise the same preflight entry points from a release-style directory."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]


class PackagedCliTests(unittest.TestCase):
    def test_preflight_uses_the_binary_bundled_next_to_scripts(self):
        with tempfile.TemporaryDirectory(prefix="corplink bundle ") as directory:
            bundle = Path(directory)
            scripts = bundle / "scripts"
            scripts.mkdir()
            for name in ("corplink-traffic.sh", "update-managed-routes.py"):
                shutil.copy2(ROOT / "scripts" / name, scripts / name)
            binary = bundle / "corplink-rs"
            binary.write_text(
                "#!/usr/bin/env python3\n"
                "import json, sys\n"
                "assert sys.argv[1] == 'routes'\n"
                "print(json.dumps({'routes': ['192.0.2.0/24'], 'sources': []}))\n"
            )
            binary.chmod(0o755)
            config = bundle / "config.local.json"
            config.write_text('{"company_name":"fixture","username":"fixture"}')
            before = config.read_bytes()
            env = {k: v for k, v in os.environ.items() if not k.startswith("CORPLINK_")}
            for command in (
                [str(scripts / "corplink-traffic.sh"), "preflight"],
                ["python3", str(scripts / "update-managed-routes.py"), str(config), "--dry-run"],
            ):
                with self.subTest(command=Path(command[0]).name):
                    result = subprocess.run(
                        command, cwd=bundle.parent, env=env, capture_output=True,
                        text=True, timeout=10,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(json.loads(result.stdout)["routes"], ["192.0.2.0/24"])
            self.assertEqual(config.read_bytes(), before)
            self.assertFalse((bundle / ".run").exists())


if __name__ == "__main__":
    unittest.main()
