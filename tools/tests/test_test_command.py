import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


RUNNER = Path(__file__).resolve().parents[1] / "test_command.py"


class TestCommandTests(unittest.TestCase):
    def test_preserves_arguments_output_and_failed_status(self):
        argument = "literal argument with spaces and $(no shell)"
        result = subprocess.run([sys.executable, str(RUNNER), sys.executable,
            "-c", "import sys; print(sys.argv[1]); print('stderr', file=sys.stderr); sys.exit(7)",
            argument], capture_output=True, text=True)
        self.assertEqual(result.returncode, 7)
        self.assertIn(argument, result.stdout)
        self.assertIn("stderr", result.stdout)

    def test_success_does_not_run_diagnostics(self):
        result = subprocess.run([sys.executable, str(RUNNER), sys.executable,
            "-c", "print('test result: ok. 1 passed; 0 failed;')"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0)
        self.assertNotIn("Allocation backtrace", result.stdout)

    def test_symbolizes_guard_frames_and_keeps_the_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "deps" / "pos3ql-abcd"
            binary.parent.mkdir()
            binary.touch()
            tool = root / "addr2line"
            tool.write_text(f"#!{sys.executable}\nimport sys\n"
                "assert sys.argv[1:5] == ['-C', '-f', '-i', '-e']\n"
                f"assert sys.argv[5:] == [{str(binary)!r}, '0x123']\n"
                "print('allocating_function at source.rs:42')\n")
            tool.chmod(0o700)
            test_env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ.get("PATH", ""))
            source = ("print('pos3ql: heap allocation after freeze or inside forbid_alloc scope; aborting');"
                f"print({str(binary)!r} + '(+0x123)[0x456]'); raise SystemExit(101)")
            result = subprocess.run([sys.executable, str(RUNNER), sys.executable, "-c", source],
                capture_output=True, text=True, env=test_env)
            self.assertEqual(result.returncode, 101)
            self.assertIn("allocating_function at source.rs:42", result.stdout)


if __name__ == "__main__":
    unittest.main()
