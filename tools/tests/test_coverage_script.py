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
    def test_external_coverage_builds_runs_and_reports_the_same_profile(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for directory in ["tools", "src", "commands", "tests/external", "target/release"]:
                (root / directory).mkdir(parents=True, exist_ok=True)
            (root / "src/lib.rs").write_text("// fixture\n")
            shutil.copyfile(ROOT / "tools/coverage.sh", root / "tools/coverage.sh")
            stale = root / "target/release/pos3ql"
            stale.write_text("#!/bin/sh\nexit 99\n")
            stale.chmod(0o755)
            (root / "tests/external/run.sh").write_text('exec "$POS3QL_BIN"\n')
            cargo = root / "commands/cargo"
            cargo.write_text("""#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
with Path(os.environ['CARGO_CALLS']).open('a') as calls:
    calls.write(json.dumps(args) + '\\n')
if args == ['llvm-cov', 'show-env', '--sh']:
    print('export LLVM_PROFILE_FILE=target/fixture.profraw')
elif args == ['build', '--profile', 'coverage', '--locked']:
    binary = Path('target/coverage/pos3ql')
    binary.parent.mkdir(parents=True)
    binary.write_text('#!/bin/sh\\nprintf profile > "$LLVM_PROFILE_FILE"\\nprintf "PASS: instrumented coverage server\\\\n"\\n')
    binary.chmod(0o755)
elif args[:4] == ['llvm-cov', 'report', '--profile', 'coverage']:
    assert Path('target/fixture.profraw').read_text() == 'profile'
    Path(args[args.index('--output-path') + 1]).write_text('TN:fixture')
else:
    raise SystemExit(58)
""")
            cargo.chmod(0o755)
            llvm_cov = root / "commands/cargo-llvm-cov"
            llvm_cov.write_text("#!/bin/sh\nexit 59\n")
            llvm_cov.chmod(0o755)
            calls = root / "cargo-calls.jsonl"
            output = root / "coverage.lcov"
            environment = dict(os.environ,
                PATH=str(root / "commands") + os.pathsep + os.environ["PATH"],
                CARGO_CALLS=str(calls), COVERAGE_SHARD="run:probe",
                COVERAGE_LCOV=str(output))
            result = subprocess.run(["sh", str(root / "tools/coverage.sh")],
                cwd=root, env=environment, capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(output.read_text(), "TN:fixture")
            self.assertIn("PASS: instrumented coverage server", result.stdout)
            self.assertEqual([json.loads(line) for line in calls.read_text().splitlines()], [
                ["llvm-cov", "show-env", "--sh"],
                ["build", "--profile", "coverage", "--locked"],
                ["llvm-cov", "report", "--profile", "coverage", "--lcov", "--output-path", str(output)],
            ])

    def test_library_debugger_keeps_selection_stack_parallelism_and_failed_status(self):
        for test_status in [0, 139]:
            with self.subTest(test_status=test_status), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / "tools").mkdir()
                (root / ".cargo").mkdir()
                (root / "target").mkdir()
                for name in ["coverage.sh", "test_command.py", "native-test-artifact.py"]:
                    shutil.copyfile(ROOT / "tools" / name, root / "tools" / name)
                shutil.copyfile(ROOT / ".cargo/config.toml", root / ".cargo/config.toml")
                commands = root / "commands"
                commands.mkdir()
                native = commands / "native-test"
                native.write_text("""#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
if sys.argv[1:] == ['--list']:
    for name in ['one', 'two', 'three', 'four']:
        print('sample::' + name + ': test')
    raise SystemExit(0)
Path(os.environ['NATIVE_CALLS']).write_text(json.dumps(sys.argv[1:]))
print('deliberate native result', flush=True)
raise SystemExit(int(os.environ['PROBE_TEST_EXIT']))
""")
                cargo = commands / "cargo"
                cargo.write_text("""#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
if args == ['llvm-cov', 'show-env', '--sh']:
    print('export LLVM_PROFILE_FILE=fixture.profraw')
elif args == ['test', '--locked', '--lib', '--profile', 'coverage', '--no-run', '--message-format=json-render-diagnostics']:
    print(json.dumps({'reason': 'compiler-artifact', 'target': {'name': 'pos3ql', 'kind': ['lib']},
        'profile': {'test': True}, 'executable': os.environ['NATIVE_TEST']}))
    print(json.dumps({'reason': 'build-finished', 'success': True}))
elif args[:2] == ['llvm-cov', 'report']:
    assert args[2:4] == ['--profile', 'coverage']
    Path(os.environ['COVERAGE_LCOV']).write_text('TN:fixture')
else:
    raise SystemExit(58)
""")
                debugger = commands / "gdb"
                debugger.write_text("""#!/usr/bin/env python3
import json, os, subprocess, sys
from pathlib import Path
Path(os.environ['DEBUGGER_CALLS']).write_text(json.dumps({'args': sys.argv[1:],
    'stack': os.environ['RUST_MIN_STACK'], 'threads': os.environ['RUST_TEST_THREADS']}))
command = sys.argv[sys.argv.index('--args') + 1:]
raise SystemExit(subprocess.run(command, check=False).returncode)
""")
                llvm_cov = commands / "cargo-llvm-cov"
                llvm_cov.write_text("#!/bin/sh\nexit 59\n")
                for command in [native, cargo, debugger, llvm_cov]:
                    command.chmod(0o755)
                native_calls = root / "native-calls.json"
                debugger_calls = root / "debugger-calls.json"
                output = root / "coverage.lcov"
                environment = dict(os.environ,
                    PATH=str(commands) + os.pathsep + os.environ["PATH"],
                    COVERAGE_SHARD="lib:0-of-2", COVERAGE_LCOV=str(output),
                    COVERAGE_NATIVE_DEBUGGER="gdb", NATIVE_TEST=str(native),
                    NATIVE_CALLS=str(native_calls), DEBUGGER_CALLS=str(debugger_calls),
                    PROBE_TEST_EXIT=str(test_status), RUST_MIN_STACK="17", RUST_TEST_THREADS="3")
                result = subprocess.run(["sh", str(root / "tools/coverage.sh")],
                    cwd=root, env=environment, capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 0 if test_status == 0 else 1,
                    result.stdout + result.stderr)
                self.assertEqual(output.exists(), test_status == 0)
                self.assertIn("deliberate native result", result.stdout)
                self.assertEqual(json.loads(native_calls.read_text()),
                    ["--exact", "--skip", "sample::two", "--skip", "sample::four"])
                captured = json.loads(debugger_calls.read_text())
                self.assertEqual(captured["stack"], "134217728")
                self.assertEqual(captured["threads"], "3")
                self.assertIn("--return-child-result", captured["args"])
                self.assertIn("thread apply all bt 40", captured["args"])
                self.assertIn("set startup-with-shell off", captured["args"])
                self.assertNotIn("--test-threads=1", captured["args"])

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
                    "if sys.argv[1:] in (['build', '--release', '--locked'], ['build', '--profile', 'coverage', '--locked']):\n"
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
                    expected = [["llvm-cov", "show-env", "--sh"],
                        ["build", "--profile", "coverage", "--locked"]]
                if failure == "environment":
                    expected.pop()
                self.assertEqual(actual, expected)


if __name__ == "__main__":
    unittest.main()
