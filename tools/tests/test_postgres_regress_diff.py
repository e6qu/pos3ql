import importlib.util
import pathlib
import unittest


PATH = pathlib.Path(__file__).parents[2] / "tests/external/postgres_regress_diff.py"
SPEC = importlib.util.spec_from_file_location("postgres_regress_diff", PATH)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class SplitSqlTests(unittest.TestCase):
    def test_quotes_comments_and_function_bodies(self):
        source = """-- prefix ;\nSELECT ';'; /* ; /* nested */ */ SELECT 2;
CREATE FUNCTION f() RETURNS text AS $$ BEGIN RETURN ';'; END $$ LANGUAGE plpgsql;
"""
        statements = MODULE.split_sql(source).statements
        self.assertEqual(len(statements), 3)
        self.assertIn("SELECT ';'", statements[0][1])
        self.assertEqual(statements[1][1], "/* ; /* nested */ */ SELECT 2;")
        self.assertIn("RETURN ';'", statements[2][1])
        self.assertTrue(all(statement[2] is None for statement in statements))

    def test_inline_copy_data_is_attached_to_copy_statement(self):
        source = "COPY t FROM stdin;\n1\tone\n2\ttwo\n\\.\nSELECT count(*) FROM t;\n"
        statements = MODULE.split_sql(source).statements
        self.assertEqual(len(statements), 2)
        self.assertEqual(statements[0][1], "COPY t FROM stdin;")
        self.assertEqual(statements[0][2], "1\tone\n2\ttwo\n")
        self.assertEqual(statements[1][1], "SELECT count(*) FROM t;")
        self.assertIsNone(statements[1][2])

    def test_unterminated_inline_copy_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "unterminated COPY data"):
            MODULE.split_sql("COPY t FROM stdin;\n1\n")

    def test_copy_options_comments_and_marker_preserve_following_line(self):
        source = "/* COPY fake FROM stdin; */\nCOPY \"t\" (id)\nFROM /* input */ STDIN WITH (FORMAT csv); -- rows\n1\r\n\\.\r\n\nSELECT 2;\n"
        parsed = MODULE.split_sql(source)
        self.assertEqual(parsed.copy_inputs, ((3, 5),))
        self.assertEqual(parsed.statements[0][2], "1\r\n")
        self.assertEqual(parsed.statements[1], (7, "SELECT 2;", None))

    def test_copy_lookalikes_inside_queries_and_quoted_bodies_are_not_input(self):
        source = "COPY (SELECT id FROM t) FROM STDIN; COPY (SELECT 'FROM stdin') TO STDOUT; SELECT E'can\\\'t; split'; SELECT $$ COPY t FROM STDIN; $$; SELECT name$part;"
        parsed = MODULE.split_sql(source)
        self.assertEqual(len(parsed.statements), 5)
        self.assertEqual(parsed.copy_inputs, ())
        self.assertTrue(all(statement[2] is None for statement in parsed.statements))

    def test_copy_header_cannot_swallow_a_second_command(self):
        with self.assertRaisesRegex(ValueError, "COPY input must start on a new line"):
            MODULE.split_sql("COPY t FROM STDIN; SELECT 1;\n\\.\n")

    def test_display_only_psql_commands_are_removed(self):
        path = pathlib.Path(self.id().replace(".", "_") + ".sql")
        try:
            path.write_text("\\x\nSELECT 1;\n\\d t\n", encoding="utf-8")
            self.assertEqual(MODULE.read_upstream(path, 1, 0), "\nSELECT 1;\n\n")
        finally:
            path.unlink(missing_ok=True)

    def test_source_ranges_preserve_upstream_line_numbers(self):
        path = pathlib.Path(self.id().replace(".", "_") + ".sql")
        try:
            path.write_text("SELECT 1;\nSELECT 2;\nSELECT 3;\n", encoding="utf-8")
            statements = list(MODULE.split_sql(MODULE.read_upstream(path, 2, 2)).statements)
            self.assertEqual(statements, [(2, "SELECT 2;", None)])
        finally:
            path.unlink(missing_ok=True)

    def test_manifest_rejects_overlapping_ranges(self):
        path = pathlib.Path(self.id().replace(".", "_") + ".tsv")
        try:
            path.write_text("input.sql\t1\t10\ninput.sql\t10\t20\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "overlapping range"):
                MODULE.manifest_entries(path)
        finally:
            path.unlink(missing_ok=True)

    def test_unterminated_quote_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "unterminated single"):
            MODULE.split_sql("SELECT 'broken")


if __name__ == "__main__":
    unittest.main()
