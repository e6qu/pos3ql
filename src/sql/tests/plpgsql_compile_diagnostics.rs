//! PL/pgSQL compile errors (their cursor positions and PostgreSQL's
//! `compilation of PL/pgSQL function` context), analysis positions in
//! definitions and SQL/JSON result conversion, compared with PostgreSQL 18.6.

use super::compatibility_diagnostics::with_engine;
use super::*;

/// Each ErrorResponse as `SQLSTATE message`, with ` @position` and
/// ` context: ...` when it carries them, and each command tag.
fn reports(engine: &mut Engine, budget: &mut Budget, sql: &str) -> Vec<String> {
    let bytes = run_with(engine, budget, sql);
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let tag = bytes[offset];
        let length = u32::from_be_bytes(bytes[offset + 1..offset + 5].try_into().unwrap()) as usize;
        let payload = &bytes[offset + 5..offset + 1 + length];
        match tag {
            b'E' => {
                let mut fields = std::collections::BTreeMap::new();
                let mut rest = payload;
                while let Some((&field, remaining)) = rest.split_first() {
                    if field == 0 {
                        break;
                    }
                    let end = remaining.iter().position(|&byte| byte == 0).unwrap();
                    fields.insert(
                        field,
                        String::from_utf8_lossy(&remaining[..end]).into_owned(),
                    );
                    rest = &remaining[end + 1..];
                }
                let mut line = format!("{} {}", fields[&b'C'], fields[&b'M']);
                if let Some(position) = fields.get(&b'P') {
                    line.push_str(&format!(" @{position}"));
                }
                if let Some(context) = fields.get(&b'W') {
                    line.push_str(&format!(" context: {context}"));
                }
                out.push(line);
            }
            b'C' => out.push(String::from_utf8_lossy(&payload[..payload.len() - 1]).into_owned()),
            b'D' => {
                let columns = u16::from_be_bytes([payload[0], payload[1]]);
                let mut at = 2;
                let mut row = Vec::new();
                for _ in 0..columns {
                    let length = i32::from_be_bytes(payload[at..at + 4].try_into().unwrap());
                    at += 4;
                    if length < 0 {
                        row.push("NULL".to_owned());
                    } else {
                        let length = length as usize;
                        row.push(String::from_utf8_lossy(&payload[at..at + length]).into_owned());
                        at += length;
                    }
                }
                out.push(format!("row {}", row.join("|")));
            }
            _ => {}
        }
        offset += 1 + length;
    }
    out
}

#[test]
fn plpgsql_compile_errors_carry_positions_or_the_compilation_line() {
    with_engine("plpgsql-compile-diagnostics", |engine, budget| {
        run_with(engine, budget, "CREATE TABLE t (a int, b text)");
        for (sql, expected) in [
            (
                "CREATE FUNCTION f1() RETURNS int LANGUAGE plpgsql AS $$ DECLARE v t.nope%TYPE; BEGIN RETURN 1; END $$",
                "42703 column \"nope\" of relation \"t\" does not exist context: compilation of PL/pgSQL function \"f1\" near line 1",
            ),
            (
                "CREATE FUNCTION f2() RETURNS int LANGUAGE plpgsql AS $$\nDECLARE\n  w int;\n  v nope%ROWTYPE;\nBEGIN RETURN 1; END $$",
                "42P01 relation \"nope\" does not exist context: compilation of PL/pgSQL function \"f2\" near line 4",
            ),
            (
                "CREATE FUNCTION f3() RETURNS int LANGUAGE plpgsql AS $$ DECLARE v anyelement; BEGIN RETURN 1; END $$",
                "0A000 variable \"v\" has pseudo-type anyelement context: compilation of PL/pgSQL function \"f3\" near line 1",
            ),
            (
                "DO $$ BEGIN IF 1 + THEN NULL; END IF; END $$",
                "42601 syntax error at end of input @19",
            ),
            (
                "DO $$ BEGIN IF THEN NULL; END IF; END $$",
                "42601 missing expression at or near \"THEN\" @16",
            ),
            (
                "DO $$ BEGIN PERFORM 1 +; END $$",
                "42601 syntax error at end of input @24",
            ),
            (
                "DO $$ BEGIN SELEC 1; END $$",
                "42601 syntax error at or near \"SELEC\" @13",
            ),
            (
                "DO $$ BEGIN x := 1; END $$",
                "42601 \"x\" is not a known variable @13",
            ),
            (
                "DO $$ DECLARE w int; BEGIN w.x := 1; END $$",
                "42601 \"w.x\" is not a known variable @28",
            ),
            (
                "DO $$ BEGIN SELECT 1 INTO y; END $$",
                "42601 \"y\" is not a known variable @27",
            ),
            (
                "DO $$ DECLARE x int; BEGIN x.y.z := 1; END $$",
                "42601 \"x.y.z\" is not a known variable @28",
            ),
            (
                "DO $$ <<b>> DECLARE x int; BEGIN b.x.z := 1; END $$",
                "42601 \"b.x.z\" is not a known variable @34",
            ),
            (
                "DO $$ BEGIN FOR r IN SELECT 1 LOOP END LOOP; END $$",
                "42601 loop variable of loop over rows must be a record variable or list of scalar variables @17",
            ),
            (
                "DO $$ DECLARE w int NOT NULL; BEGIN END $$",
                "22004 variable \"w\" must have a default value, since it's declared NOT NULL @21",
            ),
            (
                "DO $$ BEGIN RAISE EXCEPTION SQLSTATE 'zzzzz'; END $$",
                "42601 invalid SQLSTATE code at or near \"'zzzzz'\" @38",
            ),
            (
                "DO $$ BEGIN RAISE EXCEPTION USING ERRCODE = 'division_by_zero'; END $$",
                "22012 division_by_zero context: PL/pgSQL function inline_code_block line 1 at RAISE",
            ),
            (
                "DO $$ BEGIN RAISE EXCEPTION USING ERRCODE = 'zzz'; END $$",
                "42704 unrecognized exception condition \"zzz\" context: PL/pgSQL function inline_code_block line 1 at RAISE",
            ),
        ] {
            assert_eq!(reports(engine, budget, sql), [expected], "{sql}");
        }
        // A function that compiles is created; its assignments name known
        // variables only.
        assert_eq!(
            reports(
                engine,
                budget,
                "CREATE FUNCTION ok(a int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN ok.a := 2; $1 := 3; found := true; RETURN a; END $$",
            ),
            ["CREATE FUNCTION"]
        );
    });
}

#[test]
fn definitions_are_analysed_where_they_are_written() {
    with_engine("definition-analysis-positions", |engine, budget| {
        for (sql, expected) in [
            (
                "CREATE TABLE d1 (a int DEFAULT 1::nope)",
                "42704 type \"nope\" does not exist @35",
            ),
            (
                "CREATE TABLE d2 (a int CHECK (a + 1))",
                "42804 argument of CHECK must be type boolean, not type integer @31",
            ),
            (
                "CREATE TABLE d3 (a int CHECK (a::nope IS NULL))",
                "42704 type \"nope\" does not exist @34",
            ),
            (
                "SELECT * FROM json_to_record('{\"a\":1}') AS x(a nope)",
                "42704 type \"nope\" does not exist @48",
            ),
            (
                "SELECT * FROM generate_series(1::nope, 2)",
                "42704 type \"nope\" does not exist @34",
            ),
            (
                "SELECT pg_typeof(1::nope.t)",
                "3F000 schema \"nope\" does not exist @21",
            ),
            (
                "SELECT * FROM unnest(ARRAY[1,2]) AS u(x int)",
                "42601 a column definition list is only allowed for functions returning \"record\" @39",
            ),
        ] {
            assert_eq!(reports(engine, budget, sql), [expected], "{sql}");
        }
        // ALTER TABLE analyses an added CHECK without the statement text.
        run_with(engine, budget, "CREATE TABLE d4 (a int)");
        assert_eq!(
            reports(engine, budget, "ALTER TABLE d4 ADD CHECK (a + 1)"),
            ["42804 argument of CHECK must be type boolean, not type integer"]
        );
    });
}

#[test]
fn sql_json_results_convert_through_their_returning_type() {
    with_engine("sql-json-returning-conversion", |engine, budget| {
        for sql in [
            "CREATE DOMAIN dt AS text CHECK (length(VALUE) < 5)",
            "CREATE DOMAIN di AS int",
            "CREATE TYPE en AS ENUM ('a', 'b')",
        ] {
            run_with(engine, budget, sql);
        }
        for (sql, expected) in [
            (
                "SELECT JSON_QUERY('\"abcdef\"', '$' RETURNING varchar(3))",
                vec!["row NULL", "SELECT 1"],
            ),
            (
                "SELECT JSON_QUERY('\"abcdef\"', '$' RETURNING varchar(3) ERROR ON ERROR)",
                vec!["22001 value too long for type character varying(3)"],
            ),
            (
                "SELECT JSON_VALUE('\"abcdef\"', '$' RETURNING varchar(3))",
                vec!["row NULL", "SELECT 1"],
            ),
            (
                "SELECT JSON_QUERY('\"a\"', '$' RETURNING en OMIT QUOTES), JSON_QUERY('7', '$' RETURNING di), JSON_QUERY('[1,2]', '$' RETURNING int[])",
                vec!["row a|7|{1,2}", "SELECT 1"],
            ),
            (
                "SELECT JSON_OBJECT('a': 1 RETURNING di)",
                vec!["42846 cannot cast type json to di"],
            ),
            (
                "SELECT 1::anyelement, '1'::anycompatible",
                vec!["row 1|1", "SELECT 1"],
            ),
            (
                "SELECT ARRAY[1]::anynonarray",
                vec!["42846 cannot cast type integer[] to anynonarray @16"],
            ),
        ] {
            assert_eq!(reports(engine, budget, sql), expected, "{sql}");
        }
    });
}
