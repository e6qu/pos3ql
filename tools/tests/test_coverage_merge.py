import pathlib
import subprocess
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
MERGE = ROOT / "tools" / "coverage-merge.py"
WORKFLOW = ROOT / ".github" / "workflows" / "coverage.yml"


class CoverageMergeTests(unittest.TestCase):
    def run_merge(self, trace):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "trace.lcov"
            path.write_text(trace)
            return subprocess.run(
                [sys.executable, str(MERGE), "70", str(path)],
                capture_output=True,
                text=True,
                check=False,
            )

    def test_accepts_lcov_data_records_with_optional_checksum(self):
        result = self.run_merge("SF:src/lib.rs\nDA:1,1,checksum\nend_of_record\n")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_rejects_truncated_lcov_data_record(self):
        result = self.run_merge("SF:src/lib.rs\nDA:1\nend_of_record\n")
        self.assertEqual(result.returncode, 1)
        self.assertIn("malformed LCOV data record", result.stdout)

    def test_differential_artifacts_have_distinct_trace_names(self):
        workflow = WORKFLOW.read_text()
        self.assertIn("coverage-run-diff-${{ matrix.name }}.lcov", workflow)
        self.assertNotIn('COVERAGE_LCOV="${{ github.workspace }}/coverage-run-diff.lcov"', workflow)


if __name__ == "__main__":
    unittest.main()
