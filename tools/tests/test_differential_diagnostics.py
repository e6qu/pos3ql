"""Check that SQLSTATE comparison preserves rows that resemble diagnostics."""

import pathlib
import subprocess
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]


class DifferentialDiagnosticsTests(unittest.TestCase):
    def test_sqlstate_and_rows(self):
        transcript = (
            "SELECT value FROM example;\n"
            "DETAIL: this is a returned value\n"
            "column value of table example depends on type child\n"
            "psql:/tmp/corpus.sql:54: ERROR:  2BP01\n"
            "psql:/tmp/corpus.sql:56: NOTICE:  00000\n"
            "ERROR:  23514\n"
            "WARNING:  01000\n"
            "7\n"
        )
        expected = (
            "SELECT value FROM example;\n"
            "DETAIL: this is a returned value\n"
            "column value of table example depends on type child\n"
            "ERROR 2BP01\nERROR 23514\n7\n"
        )
        for name in ("ci_diff.sh", "differential.sh"):
            with self.subTest(harness=name):
                source = (ROOT / "tests/external" / name).read_text()
                start = source.index("normalize() {")
                end = source.index("\n}", start) + 2
                completed = subprocess.run(
                    ["bash", "-c", source[start:end] + "\nnormalize"],
                    input=transcript,
                    text=True,
                    capture_output=True,
                    check=True,
                )
                self.assertEqual(completed.stdout, expected)


if __name__ == "__main__":
    unittest.main()
