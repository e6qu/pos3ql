"""Keep psql rows separate from complete, source-positioned diagnostics."""

import importlib.util
import io
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "corpus_diagnostics", ROOT / "tests/external/differential_diagnostics.py"
)
DIAGNOSTICS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DIAGNOSTICS)


class DifferentialDiagnosticsTests(unittest.TestCase):
    def render_transcript(self, stdout, stderr, diagnostics):
        output = io.BytesIO()
        DIAGNOSTICS.write_transcript(io.BytesIO(stdout), len(stdout), stderr, diagnostics, {}, output,
                                     input_diagnostics=diagnostics)
        return output.getvalue()

    def test_stdout_streams_with_bounded_reads_and_rejects_a_changed_length(self):
        class BoundedReader(io.BytesIO):
            def read(self, size):
                if not 0 < size <= 64 * 1024:
                    raise AssertionError("stdout capture requires bounded reads")
                return super().read(size)

        payload = b"x" * (128 * 1024 + 1)
        output = io.BytesIO()
        DIAGNOSTICS.write_transcript(
            BoundedReader(payload), len(payload), "", "verbose", {}, output,
            input_diagnostics="verbose",
        )
        self.assertEqual(
            output.getvalue(), f"STDOUT {len(payload)} bytes\n".encode() + payload
            + b"\nDIAGNOSTICS\n",
        )
        with self.assertRaises(ValueError):
            DIAGNOSTICS.write_transcript(io.BytesIO(b"x"), 2, "", "verbose", {}, io.BytesIO(),
                                         input_diagnostics="verbose")

    def test_diagnostic_lookalikes_in_rows_are_preserved_verbatim(self):
        stdout = (
            "DETAIL: this is a returned value\n"
            "drop cascades to user mapping for public on server lifetime_server\n"
            "ERROR: 23514\nDIAGNOSTICS\n7\né\n"
        ).encode()
        stderr = (
            "psql:corpus.sql:54: ERROR:  2BP01\n"
            "psql:corpus.sql:56: NOTICE:  00000\n"
        )
        result = self.render_transcript(stdout, stderr, "sqlstate")
        self.assertTrue(result.startswith(
            f"STDOUT {len(stdout)} bytes\n".encode() + stdout + b"\nDIAGNOSTICS\n"
        ))
        self.assertEqual(DIAGNOSTICS.normalize_diagnostics(stderr, "sqlstate", {},
                         input_diagnostics="sqlstate"), [{
            "source": "corpus.sql", "line": 54, "severity": "ERROR", "sqlstate": "2BP01",
        }])

    def test_non_utf8_bytes_and_unicode_line_characters_are_not_replaced(self):
        primary = b"value \xff with " + "\u0085".encode()
        stderr = b"psql:corpus.sql:8: ERROR:  23505: " + primary + b"\n"
        text = stderr.decode("utf-8", errors="surrogateescape")
        transcript = self.render_transcript(b"\x00\xff", text, "verbose")
        encoded_record = transcript.split(b"\nDIAGNOSTICS\n", 1)[1]
        record = json.loads(encoded_record)
        self.assertEqual(record["message"].encode("utf-8", errors="surrogateescape"), primary)
        self.assertNotEqual(
            transcript,
            self.render_transcript(b"\x00\xff", text.replace("\udcff", "\udcfe"), "verbose"),
        )

    def test_cli_keeps_raw_bytes_and_escapes_diagnostics_losslessly(self):
        with tempfile.TemporaryDirectory() as work:
            stdout = pathlib.Path(work, "stdout")
            stderr = pathlib.Path(work, "stderr")
            source = pathlib.Path(work, "corpus.sql")
            source.write_text("SELECT 1;\n")
            stdout.write_bytes(b"\xff\x00\r\n")
            stderr.write_bytes(b"psql:corpus.sql:8: ERROR:  23505: value \xff\rretained\n")
            completed = subprocess.run(
                [
                    sys.executable, str(ROOT / "tests/external/differential_diagnostics.py"),
                    "--source", str(source), "--stdout", str(stdout), "--stderr", str(stderr),
                    "--diagnostics", "verbose",
                    "--input-diagnostics", "verbose",
                ],
                capture_output=True, check=True,
            )
        self.assertTrue(completed.stdout.startswith(b"STDOUT 4 bytes\n\xff\x00\r\n"))
        record = json.loads(completed.stdout.split(b"\nDIAGNOSTICS\n", 1)[1])
        self.assertEqual(
            record["message"].encode("utf-8", errors="surrogateescape"),
            b"value \xff\rretained",
        )

    def test_verbose_notice_and_detail_continuations_are_whole_records(self):
        stderr = (
            "WARNING: startup warning\nHINT: warning detail\n"
            "another warning detail\n"
            "psql:corpus.sql:10: NOTICE:  00000: drop cascades to 2 other objects\n"
            "DETAIL:  drop cascades to server lifetime_server\n"
            "drop cascades to user mapping for public on server lifetime_server\n"
            "LOCATION:  reportDependentObjects, dependency.c:1200\n"
            "psql:corpus.sql:20: ERROR:  2BP01: cannot drop type child\n"
            "DETAIL:  column value of table example depends on type child\n"
            "another dependent object\nHINT:  Use DROP CASCADE.\n"
            "LOCATION:  reportDependentObjects, dependency.c:1200\n"
            "psql:corpus.sql:30: ERROR:  P0001:   first primary line\n"
            "second primary line\nCONTEXT:  function example()\n"
        )
        records = DIAGNOSTICS.normalize_diagnostics(stderr, "verbose", {}, input_diagnostics="verbose")
        self.assertEqual([record["line"] for record in records], [20, 30])
        self.assertEqual([record["message"] for record in records], [
            "cannot drop type child", "  first primary line\nsecond primary line",
        ])
        self.assertEqual([record["sqlstate"] for record in records], ["2BP01", "P0001"])

    def test_error_statement_and_primary_message_remain_comparison_inputs(self):
        first = "psql:corpus.sql:8: ERROR:  23505: duplicate value\n"
        for changed in (first.replace(":8:", ":9:"), first.replace("duplicate", "different")):
            self.assertNotEqual(
                self.render_transcript(b"", first, "verbose"),
                self.render_transcript(b"", changed, "verbose"),
            )
        self.assertNotEqual(
            self.render_transcript(b"", "psql:corpus.sql:8: ERROR:  23505\n", "sqlstate"),
            self.render_transcript(b"", "psql:corpus.sql:9: ERROR:  23505\n", "sqlstate"),
        )

    def test_same_line_errors_cannot_move_between_repeated_sql_commands(self):
        statement = b"INSERT INTO example VALUES (1);\n"
        first_succeeds = statement + b"INSERT 0 1\n" + statement
        second_succeeds = statement + statement + b"INSERT 0 1\n"
        stderr = "psql:corpus.sql:1: ERROR:  23505: duplicate value\n"
        self.assertNotEqual(
            self.render_transcript(first_succeeds, stderr, "verbose"),
            self.render_transcript(second_succeeds, stderr, "verbose"),
        )

    def test_client_failures_and_malformed_diagnostics_cannot_disappear(self):
        stderr = "psql: error: connection failed\nserver closed the connection\n"
        self.assertEqual(DIAGNOSTICS.normalize_diagnostics(stderr, "verbose", {},
                         input_diagnostics="verbose"), [
            {"client": "psql: error: connection failed"},
            {"client": "server closed the connection"},
        ])
        for style, stderr in (
            ("verbose", "psql:corpus.sql:8: ERROR: missing SQLSTATE\n"),
            ("sqlstate", "psql:corpus.sql:8: ERROR: 23505: unexpected wording\n"),
            ("unknown", ""),
        ):
            with self.subTest(style=style), self.assertRaises(ValueError):
                DIAGNOSTICS.normalize_diagnostics(stderr, style, {}, input_diagnostics=style)

    def test_copy_errors_keep_the_command_position_while_input_is_drained(self):
        source = "SELECT 1;\nCOPY t\nFROM STDIN WITH (FORMAT csv); -- payload follows\n'not SQL\n\\.\n\nSELECT 2;\n"
        positions = DIAGNOSTICS.copy_line_positions(source, "corpus.sql")
        self.assertEqual(positions, {("corpus.sql", line): 3 for line in (3, 4, 5)})
        early = "psql:corpus.sql:3: ERROR:  42809\n"
        late = "psql:corpus.sql:5: ERROR:  42809\n"
        self.assertEqual(
            DIAGNOSTICS.normalize_diagnostics(early, "sqlstate", positions, input_diagnostics="sqlstate"),
            DIAGNOSTICS.normalize_diagnostics(late, "sqlstate", positions, input_diagnostics="sqlstate"),
        )
        for unrelated in (
            "psql:corpus.sql:7: ERROR:  42809\n",
            "psql:another.sql:5: ERROR:  42809\n",
        ):
            self.assertNotEqual(
                DIAGNOSTICS.normalize_diagnostics(early, "sqlstate", positions, input_diagnostics="sqlstate"),
                DIAGNOSTICS.normalize_diagnostics(unrelated, "sqlstate", positions, input_diagnostics="sqlstate"),
            )
        with self.assertRaisesRegex(ValueError, "unterminated COPY data"):
            DIAGNOSTICS.copy_line_positions("COPY t FROM STDIN;\n", "corpus.sql")

    def test_verbose_capture_keeps_state_comparison_separate_from_primary_text(self):
        first = "psql:corpus.sql:8: ERROR:  54000: memtable exhausted\n"
        changed = first.replace("memtable exhausted", "another primary message")
        for mode, equal in (("sqlstate", True), ("verbose", False)):
            rendered = []
            for stderr in (first, changed):
                output = io.BytesIO()
                DIAGNOSTICS.write_transcript(io.BytesIO(b""), 0, stderr, mode, {}, output,
                                             input_diagnostics="verbose")
                rendered.append(output.getvalue())
            self.assertEqual(rendered[0] == rendered[1], equal)
        for input_mode in ("sqlstate", "unknown"):
            with self.subTest(input_mode=input_mode), self.assertRaises(ValueError):
                DIAGNOSTICS.normalize_diagnostics(first, "verbose", {}, input_diagnostics=input_mode)
        with self.assertRaises(ValueError):
            DIAGNOSTICS.normalize_diagnostics("psql:corpus.sql:8: ERROR:  54000\n",
                "sqlstate", {}, input_diagnostics="verbose")

    def run_harness(self, harness, diagnostics, reference, client_status):
        source = (ROOT / "tests/external" / harness).read_text()
        start = source.index("run_corpus() {")
        end = source.index("\n}", start) + 2
        command = source[start:end]
        with tempfile.TemporaryDirectory(prefix="corpus settings ") as work:
            port = "55432" if reference else "65432"
            input_path = work + "/input.sql"
            pathlib.Path(input_path).write_text("SELECT 1;\n")
            if harness == "ci_diff.sh":
                arguments = ["127.0.0.1", port, work + "/output", input_path, diagnostics]
            else:
                arguments = [port, "output", input_path, diagnostics]
            setup = r'''
set -eu
WORK=$1
EXT=$2
MOCK_CLIENT_STATUS=$3
shift 3
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
psql() {
  printf '%s\n' "options=$PGOPTIONS" "$@"
  case "$*" in
    *VERBOSITY=verbose*)
      printf '%s\n' 'psql:fixture.sql:8: ERROR: 23505: duplicate marker' >&2 ;;
    *VERBOSITY=sqlstate*)
      printf '%s\n' 'psql:fixture.sql:8: ERROR: 23505' >&2 ;;
  esac
  return "$MOCK_CLIENT_STATUS"
}
'''
            completed = subprocess.run(
                [
                    "bash", "-c", setup + command + '\nrun_corpus "$@"', "test",
                    work, str(ROOT / "tests/external"), str(client_status), *arguments,
                ],
                text=True, capture_output=True, check=False,
            )
            output = pathlib.Path(work, "output")
            return completed, output.read_bytes() if output.exists() else None, input_path

    def test_every_diagnostic_mode_uses_the_reference_fixture_settings(self):
        for harness in ("ci_diff.sh", "differential.sh"):
            for diagnostics in ("sqlstate", "verbose"):
                for reference in (False, True):
                    with self.subTest(
                        harness=harness, diagnostics=diagnostics, reference=reference
                    ):
                        completed, output, input_path = self.run_harness(
                            harness, diagnostics, reference, 0
                        )
                        self.assertEqual(completed.returncode, 0, completed.stderr)
                        stdout, records = output.decode().split("\nDIAGNOSTICS\n", 1)
                        rows = stdout.splitlines()[1:]
                        self.assertEqual(
                            rows[0],
                            "options=-c timezone=UTC -c extension_control_path=/oracle/extensions"
                            if reference else "options=",
                        )
                        self.assertIn("-e", rows)
                        self.assertNotIn("-a", rows)
                        self.assertNotIn("-q", rows)
                        self.assertIn("VERBOSITY=verbose", rows)
                        self.assertEqual(rows[-2:], ["-f", input_path])
                        record = json.loads(records)
                        self.assertEqual(record["line"], 8)
                        self.assertEqual(record["sqlstate"], "23505")
                        self.assertEqual("message" in record, diagnostics == "verbose")

    def test_both_harnesses_reject_a_failed_client_before_comparing_results(self):
        for harness in ("ci_diff.sh", "differential.sh"):
            with self.subTest(harness=harness):
                completed, output, _ = self.run_harness(harness, "verbose", True, 2)
                self.assertNotEqual(completed.returncode, 0)
                self.assertIn("corpus psql client exited with status 2", completed.stderr)
                self.assertIsNone(output)

    def test_port_only_oracles_reject_an_ignored_remote_host(self):
        completed = subprocess.run(
            ["bash", "tests/external/differential.sh"], cwd=ROOT,
            env=dict(
                os.environ, POS3QL_REFERENCE_PG_HOST="198.51.100.1",
                POS3QL_REFERENCE_PG_PORT="5432", POS3QL_REFERENCE_PSQL="must-not-be-invoked",
            ),
            text=True, capture_output=True, check=False,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("requires POS3QL_REFERENCE_PG_HOST=127.0.0.1", completed.stderr)
        self.assertNotIn("must-not-be-invoked", completed.stderr)


if __name__ == "__main__":
    unittest.main()
