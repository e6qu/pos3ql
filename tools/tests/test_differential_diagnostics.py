"""Check that SQLSTATE comparison preserves rows that resemble diagnostics."""

import os
import pathlib
import subprocess
import tempfile
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


    def test_every_diagnostic_mode_uses_the_reference_fixture_settings(self):
        for harness in ("ci_diff.sh", "differential.sh"):
            source = (ROOT / "tests/external" / harness).read_text()
            start = source.index("run_corpus() {")
            end = source.index("\n}", start) + 2
            command = source[start:end]
            for diagnostics in ("sqlstate", "verbose"):
                for reference in (False, True):
                    with self.subTest(
                        harness=harness, diagnostics=diagnostics, reference=reference
                    ):
                        with tempfile.TemporaryDirectory(prefix="corpus settings ") as work:
                            port = "55432" if reference else "65432"
                            if harness == "ci_diff.sh":
                                arguments = [
                                    "127.0.0.1", port, work + "/output",
                                    work + "/input.sql", diagnostics,
                                ]
                            else:
                                arguments = [
                                    port, "output", work + "/input.sql", diagnostics,
                                ]
                            setup = r"""
set -eu
WORK=$1
shift
PG_PORT=55432
PGPORT=55432
P3_PORT=65432
PGHOST=127.0.0.1
PGUSER=postgres
PSQL=psql
P3_PID=1
REFERENCE_DATABASE=postgres
REFERENCE_EXTENSION_CONTROL_ROOT=/oracle/extensions
PGOPTIONS=
server_alive() { return 0; }
stdbuf() { shift; shift; "$@"; }
psql() { printf '%s\n' "options=$PGOPTIONS" "$@"; }
normalize() { printf '%s\n' 'normalizer=sqlstate'; cat; }
normalize_exact() { printf '%s\n' 'normalizer=verbose'; cat; }
"""
                            subprocess.run(
                                [
                                    "bash", "-c", setup + command + '\nrun_corpus "$@"',
                                    "test", work, *arguments,
                                ],
                                text=True,
                                capture_output=True,
                                check=True,
                            )
                            rows = pathlib.Path(work, "output").read_text().splitlines()
                            self.assertEqual(rows[0], "normalizer=" + diagnostics)
                            self.assertEqual(
                                rows[1],
                                "options=-c timezone=UTC -c extension_control_path=/oracle/extensions"
                                if reference else "options=",
                            )
                            self.assertIn("VERBOSITY=" + diagnostics, rows)
                            self.assertEqual(rows[-2:], ["-f", work + "/input.sql"])


    def test_port_only_oracles_reject_an_ignored_remote_host(self):
        completed = subprocess.run(
            ["bash", "tests/external/differential.sh"],
            cwd=ROOT,
            env=dict(
                os.environ,
                POS3QL_REFERENCE_PG_HOST="198.51.100.1",
                POS3QL_REFERENCE_PG_PORT="5432",
                POS3QL_REFERENCE_PSQL="must-not-be-invoked",
            ),
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("requires POS3QL_REFERENCE_PG_HOST=127.0.0.1", completed.stderr)
        self.assertNotIn("must-not-be-invoked", completed.stderr)


if __name__ == "__main__":
    unittest.main()
