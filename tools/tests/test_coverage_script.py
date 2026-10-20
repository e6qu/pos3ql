"""A compiler failure must stop coverage before any server or suite runs."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class CoverageBuildFailureTest(unittest.TestCase):
    def test_build_failure_stops_instrumented_and_correctness_shards(self):
        for shard, failure in [
            ("runtest:probe", "build"),
            ("run:probe", "build"),
            ("run:probe", "environment"),
        ]:
            with self.subTest(shard=shard, failure=failure), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                (root / "tools").mkdir()
                (root / "src").mkdir()
                (root / "src/lib.rs").write_text("// fixture\n")
                shutil.copyfile(ROOT / "tools/coverage.sh", root / "tools/coverage.sh")
                commands = root / "commands"
                commands.mkdir()
                cargo = commands / "cargo"
                cargo.write_text(
                    "#!/usr/bin/env python3\n"
                    "import json, os, sys\n"
                    "with open(os.environ['CARGO_CALLS'], 'a') as calls:\n"
                    "    calls.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                    "if sys.argv[1:] == ['llvm-cov', 'show-env', '--sh']:\n"
                    "    if os.environ['PROBE_FAILURE'] == 'environment':\n"
                    "        print('deliberate environment failure', file=sys.stderr)\n"
                    "        sys.exit(37)\n"
                    "    print('export LLVM_PROFILE_FILE=probe.profraw')\n"
                    "    sys.exit(0)\n"
                    "if sys.argv[1:] == ['build', '--release', '--locked']:\n"
                    "    print('deliberate compiler failure', file=sys.stderr)\n"
                    "    print('final compiler diagnostic', file=sys.stderr)\n"
                    "    sys.exit(37)\n"
                    "sys.exit(58)\n"
                )
                cargo.chmod(0o755)
                llvm_cov = commands / "cargo-llvm-cov"
                llvm_cov.write_text("#!/bin/sh\nexit 59\n")
                llvm_cov.chmod(0o755)
                calls = root / "cargo-calls.jsonl"
                env = os.environ.copy()
                env.update(
                    PATH=str(commands) + os.pathsep + env["PATH"],
                    CARGO_CALLS=str(calls),
                    PROBE_FAILURE=failure,
                    COVERAGE_SHARD=shard,
                    COVERAGE_LCOV=str(root / "probe.lcov"),
                )
                result = subprocess.run(
                    ["sh", str(root / "tools/coverage.sh")],
                    cwd=root,
                    env=env,
                    text=True,
                    capture_output=True,
                    timeout=10,
                )
                self.assertEqual(result.returncode, 37, result.stdout + result.stderr)
                if failure == "build":
                    self.assertIn("deliberate compiler failure", result.stderr)
                    self.assertIn("final compiler diagnostic", result.stderr)
                else:
                    self.assertIn("deliberate environment failure", result.stderr)
                self.assertNotIn("external suite", result.stdout)
                self.assertNotIn("No such file", result.stderr)
                actual = [json.loads(line) for line in calls.read_text().splitlines()]
                expected = [["build", "--release", "--locked"]]
                if shard == "run:probe":
                    expected.insert(0, ["llvm-cov", "show-env", "--sh"])
                if failure == "environment":
                    expected.pop()
                self.assertEqual(actual, expected)


if __name__ == "__main__":
    unittest.main()
