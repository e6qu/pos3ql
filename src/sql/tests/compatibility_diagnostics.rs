//! Client-visible scanner, parse-analysis and cast diagnostics compared with
//! PostgreSQL 18.6: SQLSTATE, message, cursor position and notice order.

use super::*;

/// Each ErrorResponse and NoticeResponse as `SEVERITY SQLSTATE message`,
/// followed by ` @position` when it carries a cursor and ` hint: ...` when
/// it carries a hint, interleaved with the command tags of the response.
pub(super) fn diagnostics(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let tag = bytes[offset];
        let length = u32::from_be_bytes(bytes[offset + 1..offset + 5].try_into().unwrap()) as usize;
        let payload = &bytes[offset + 5..offset + 1 + length];
        match tag {
            b'E' | b'N' => {
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
                let mut line = format!("{} {} {}", fields[&b'S'], fields[&b'C'], fields[&b'M']);
                if let Some(position) = fields.get(&b'P') {
                    line.push_str(&format!(" @{position}"));
                }
                if let Some(hint) = fields.get(&b'H') {
                    line.push_str(&format!(" hint: {hint}"));
                }
                if let Some(detail) = fields.get(&b'D') {
                    line.push_str(&format!(" detail: {detail}"));
                }
                out.push(line);
            }
            b'C' => out.push(String::from_utf8_lossy(&payload[..payload.len() - 1]).into_owned()),
            _ => {}
        }
        offset += 1 + length;
    }
    out
}

pub(super) fn with_engine(name: &str, run: impl FnOnce(&mut Engine, &mut Budget)) {
    let config = test_config(name);
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    run(&mut engine, &mut budget);
    drop(engine);
    std::fs::remove_dir_all(config.data_dir).unwrap();
}

#[test]
fn unicode_escape_literals_and_identifiers_follow_postgresql() {
    with_engine("unicode-escape-literals", |engine, budget| {
        let output = run_with(
            engine,
            budget,
            r#"SELECT U&'d!0061t!+000061' UESCAPE '!' AS "U&", U&"\0061bc" FROM (SELECT 1 AS abc) s"#,
        );
        assert_eq!(data_rows(&output), ["data|1"]);
        assert_eq!(row_description_names(&output), ["U&", "abc"]);
        for (sql, expected) in [
            (
                r"SELECT U&'\D83D';",
                "ERROR 42601 invalid Unicode surrogate pair @16",
            ),
            (
                r"SELECT U&'\00';",
                r"ERROR 42601 invalid Unicode escape @11 hint: Unicode escapes must be \XXXX or \+XXXXXX.",
            ),
            (
                "SELECT U&'wrong: !0061' UESCAPE 'a';",
                "ERROR 42601 invalid Unicode escape character at or near \"'a'\" @33",
            ),
            (
                r#"SELECT U&"";"#,
                "ERROR 42601 zero-length delimited identifier at or near \"U&\"\"\" @8",
            ),
        ] {
            assert_eq!(
                diagnostics(&run_with(engine, budget, sql)),
                [expected],
                "{sql}"
            );
        }
    });
}

#[test]
fn long_identifiers_truncate_with_postgresql_notices() {
    with_engine("identifier-truncation", |engine, budget| {
        let long = "a".repeat(70);
        let truncated = "a".repeat(63);
        let wide = "é".repeat(40);
        let output = run_with(
            engine,
            budget,
            &format!(
                "CREATE TABLE {long}(x int); INSERT INTO {}(x) VALUES (1); \
                 SELECT x AS \"{wide}\" FROM {long};",
                long.to_uppercase()
            ),
        );
        let notice = |full: &str, cut: &str| {
            format!("NOTICE 42622 identifier \"{full}\" will be truncated to \"{cut}\"")
        };
        // The scanner reports every truncation before any statement runs.
        assert_eq!(
            diagnostics(&output),
            [
                notice(&long, &truncated),
                notice(&long, &truncated),
                notice(&wide, &"é".repeat(31)),
                notice(&long, &truncated),
                "CREATE TABLE".to_owned(),
                "INSERT 0 1".to_owned(),
                "SELECT 1".to_owned(),
            ]
        );
        assert_eq!(row_description_names(&output), ["é".repeat(31)]);
        // A later syntax error still follows the notices scanned before it.
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                &format!("SELECT 1 AS {long}; SELEC 1")
            )),
            [
                notice(&long, &truncated),
                "ERROR 42601 syntax error at or near \"SELEC\" @{}"
                    .replace("{}", &(long.len() + 15).to_string()),
            ]
        );
    });
}

#[test]
fn type_modifiers_fail_in_parse_analysis_with_postgresql_errors() {
    with_engine("type-modifier-analysis", |engine, budget| {
        for (sql, expected) in [
            (
                "SELECT 1::numeric(2000)",
                "ERROR 22023 NUMERIC precision 2000 must be between 1 and 1000 @11",
            ),
            (
                "SELECT 1::numeric(5, 1001)",
                "ERROR 22023 NUMERIC scale 1001 must be between -1000 and 1000 @11",
            ),
            (
                "SELECT 1::numeric(1,2,3)",
                "ERROR 22023 invalid NUMERIC type modifier @11",
            ),
            (
                "SELECT 1::numeric(1.5)",
                "ERROR 22P02 invalid input syntax for type integer: \"1.5\" @11",
            ),
            (
                "SELECT 1::numeric(1+1)",
                "ERROR 42601 type modifiers must be simple constants or identifiers @11",
            ),
            (
                "SELECT 'a'::varchar(0)",
                "ERROR 22023 length for type varchar must be at least 1 @13",
            ),
            (
                "SELECT 'a'::bpchar(1,2)",
                "ERROR 22023 invalid type modifier @13",
            ),
            (
                "SELECT 'a'::varchar(1,2)",
                "ERROR 42601 syntax error at or near \",\" @22",
            ),
            (
                "SELECT '1'::bit(83886081)",
                "ERROR 22023 length for type bit cannot exceed 83886080 @13",
            ),
            (
                "SELECT now()::timestamptz(-1)",
                "ERROR 22023 TIMESTAMP(-1) WITH TIME ZONE precision must not be negative @15",
            ),
            (
                "SELECT now()::timestamp(-1)",
                "ERROR 42601 syntax error at or near \"-\" @25",
            ),
            (
                "SELECT '1'::\"interval\"(7)",
                "ERROR 22023 invalid INTERVAL type modifier @13",
            ),
            (
                "SELECT 1::int4(5)[]",
                "ERROR 42601 type modifier is not allowed for type \"int4[]\" @11",
            ),
            (
                "SELECT 1::pg_catalog.int4(2)",
                "ERROR 42601 type modifier is not allowed for type \"pg_catalog.int4\" @11",
            ),
            (
                "SELECT 1::integer(5)",
                "ERROR 42601 syntax error at or near \"(\" @18",
            ),
            (
                "SELECT 1::float(0)",
                "ERROR 22023 precision for type float must be at least 1 bit @17",
            ),
            (
                "CREATE TABLE typmod_t(a varchar(0))",
                "ERROR 22023 length for type varchar must be at least 1 @25",
            ),
        ] {
            assert_eq!(
                diagnostics(&run_with(engine, budget, sql)),
                [expected],
                "{sql}"
            );
        }
        // Earlier statements run; a syntax error anywhere wins over analysis.
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "SELECT 1; SELECT 1::numeric(0); SELECT 2"
            )),
            [
                "SELECT 1",
                "ERROR 22023 NUMERIC precision 0 must be between 1 and 1000 @21",
            ]
        );
        assert_eq!(
            diagnostics(&run_with(engine, budget, "SELECT 1::numeric(0); SELEC 2")),
            ["ERROR 42601 syntax error at or near \"SELEC\" @23"]
        );
        // Precision warnings name the resolved SQL type at the type name.
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "SELECT now()::time(7) with time zone, interval '1' second(9), \
                 NULL::\"timestamp\"(8)"
            )),
            [
                "WARNING 22023 TIME(7) WITH TIME ZONE precision reduced to maximum allowed, 6 @15",
                "WARNING 22023 INTERVAL(9) precision reduced to maximum allowed, 6 @39",
                "WARNING 22023 TIMESTAMP(8) precision reduced to maximum allowed, 6 @69",
                "SELECT 1",
            ]
        );
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "CREATE GLOBAL TEMP TABLE gt(a int)"
            )),
            [
                "WARNING 01000 GLOBAL is deprecated in temporary table creation @8",
                "CREATE TABLE",
            ]
        );
    });
}

#[test]
fn numeric_modifiers_accept_postgresql_scales() {
    with_engine("numeric-signed-scale", |engine, budget| {
        let output = run_with(
            engine,
            budget,
            "SELECT 123.456::numeric(5,-2), 0.0049::numeric(3,5), 50::numeric(3,-2), \
             0.0::numeric(3,5), 0.009994::numeric(3,5), -999.994::numeric(5,2), \
             'NaN'::numeric(3,1), ' -INF '::numeric",
        );
        assert_eq!(
            data_rows(&output),
            ["100|0.00490|100|0.00000|0.00999|-999.99|NaN|-Infinity"]
        );
        for (sql, detail) in [
            (
                "SELECT 0.009995::numeric(3,5)",
                "A field with precision 3, scale 5 must round to an absolute value less than 10^-2.",
            ),
            (
                "SELECT 1e5::numeric(3,-2)",
                "A field with precision 3, scale -2 must round to an absolute value less than 10^5.",
            ),
            (
                "SELECT 99999.5::numeric(5,0)",
                "A field with precision 5, scale 0 must round to an absolute value less than 10^5.",
            ),
            (
                "SELECT 'inf'::numeric(5,2)",
                "A field with precision 5, scale 2 cannot hold an infinite value.",
            ),
        ] {
            assert_eq!(
                diagnostics(&run_with(engine, budget, sql)),
                [format!(
                    "ERROR 22003 numeric field overflow detail: {detail}"
                )],
                "{sql}"
            );
        }
        let output = run_with(
            engine,
            budget,
            "CREATE TABLE scaled(a numeric(5,-2), b numeric(3,5)); \
             INSERT INTO scaled VALUES (12345.6, 0.001234); \
             SELECT a, b, format_type(atttypid, atttypmod) FROM scaled, pg_attribute \
             WHERE attrelid = 'scaled'::regclass AND attnum = 1",
        );
        assert_eq!(data_rows(&output), ["12300|0.00123|numeric(5,-2)"]);
    });
}

#[test]
fn unsupported_casts_name_both_types_as_postgresql_does() {
    with_engine("unsupported-cast-names", |engine, budget| {
        for (sql, expected) in [
            (
                "SELECT 1::int::timestamp",
                "ERROR 42846 cannot cast type integer to timestamp without time zone",
            ),
            (
                "SELECT true::date",
                "ERROR 42846 cannot cast type boolean to date",
            ),
        ] {
            assert_eq!(
                diagnostics(&run_with(engine, budget, sql))[0]
                    .split(" @")
                    .next()
                    .unwrap(),
                expected,
                "{sql}"
            );
        }
    });
}

#[test]
fn statement_option_checks_report_postgresql_errors_when_they_run() {
    with_engine("statement-option-checks", |engine, budget| {
        let setup = run_with(engine, budget, "CREATE TABLE ct(a int, b text)");
        assert!(error_sqlstates(&setup).is_empty());
        // Results from PostgreSQL 18.6.
        for (sql, expected) in [
            (
                "COPY (SELECT 1) FROM STDIN",
                "ERROR 42601 syntax error at or near \"FROM\" @17",
            ),
            (
                "COPY ct TO '/tmp/x'",
                "ERROR 0A000 COPY to a server file is not supported hint: Anyone can COPY to stdout or from stdin. psql's \\copy command also works for anyone.",
            ),
            (
                "COPY ct FROM STDIN WITH (FORMAT csv, FORMAT csv)",
                "ERROR 42601 conflicting or redundant options @38",
            ),
            (
                "COPY ct FROM STDIN WITH (FORMAT foo)",
                "ERROR 22023 COPY format \"foo\" not recognized @26",
            ),
            (
                "COPY ct FROM STDIN WITH (foo 1)",
                "ERROR 42601 option \"foo\" not recognized @26",
            ),
            (
                "COPY ct FROM STDIN WITH (ON_ERROR foo)",
                "ERROR 22023 COPY ON_ERROR \"foo\" not recognized @26",
            ),
            (
                "COPY ct TO STDOUT WITH (ON_ERROR ignore)",
                "ERROR 22023 COPY ON_ERROR cannot be used with COPY TO @25",
            ),
            (
                "COPY ct FROM STDIN WITH (REJECT_LIMIT 0)",
                "ERROR 22023 REJECT_LIMIT (0) must be greater than zero",
            ),
            (
                "COPY ct FROM STDIN WITH (REJECT_LIMIT 'x')",
                "ERROR 22P02 invalid input syntax for type bigint: \"x\"",
            ),
            (
                "COPY ct FROM STDIN WITH (REJECT_LIMIT 5)",
                "ERROR 22023 COPY REJECT_LIMIT requires ON_ERROR to be set to IGNORE",
            ),
            (
                "COPY ct FROM STDIN WITH (HEADER 2)",
                "ERROR 42601 header requires a Boolean value or \"match\"",
            ),
            (
                "COPY ct TO STDOUT WITH (HEADER match)",
                "ERROR 0A000 cannot use \"match\" with HEADER in COPY TO",
            ),
            (
                "COPY ct FROM STDIN WITH (FORMAT binary, DELIMITER ',')",
                "ERROR 42601 cannot specify DELIMITER in BINARY mode",
            ),
            (
                "COPY ct FROM STDIN WITH (ON_ERROR ignore, FORMAT binary)",
                "ERROR 42601 only ON_ERROR STOP is allowed in BINARY mode",
            ),
            (
                "COPY ct FROM STDIN WITH (DELIMITER '\\')",
                "ERROR 22023 COPY delimiter cannot be \"\\\"",
            ),
            (
                "COPY ct FROM STDIN WITH (NULL '\\N', DEFAULT '\\N')",
                "ERROR 0A000 NULL specification and DEFAULT specification cannot be the same",
            ),
            (
                "COPY ct TO STDOUT WITH (FORMAT csv, FORCE_NULL (a))",
                "ERROR 22023 COPY FORCE_NULL cannot be used with COPY TO",
            ),
            (
                "COPY ct FROM STDIN WITH (ENCODING 'foo')",
                "ERROR 22023 argument to option \"encoding\" must be a valid encoding name @26",
            ),
            (
                "COPY ct FROM STDIN WITH (ON_ERROR)",
                "ERROR 42601 on_error requires a parameter",
            ),
            (
                "COPY ct (a, a) TO STDOUT",
                "ERROR 42701 column \"a\" specified more than once",
            ),
            (
                "CREATE TABLE ff(a int) WITH (fillfactor = 5)",
                "ERROR 22023 value 5 out of bounds for option \"fillfactor\" detail: Valid values are between \"10\" and \"100\".",
            ),
            (
                "CREATE TABLE ff(a int) WITH (fillfactor = 1.5)",
                "ERROR 22023 value 1.5 out of bounds for option \"fillfactor\" detail: Valid values are between \"10\" and \"100\".",
            ),
            (
                "CREATE TABLE ff(a int) WITH (fillfactor = 'x')",
                "ERROR 22023 invalid value for integer option \"fillfactor\": x",
            ),
            (
                "CREATE INDEX ON ct USING brin (a) WITH (pages_per_range = 0)",
                "ERROR 22023 value 0 out of bounds for option \"pages_per_range\" detail: Valid values are between \"1\" and \"131072\".",
            ),
            (
                "CREATE ROLE r1 CONNECTION LIMIT 99999999999",
                "ERROR 42601 syntax error at or near \"99999999999\" @33",
            ),
            (
                "CREATE SEQUENCE s1 START 99999999999999999999",
                "ERROR 22003 value \"99999999999999999999\" is out of range for type bigint",
            ),
            (
                "CREATE SEQUENCE s1 INCREMENT 1.5",
                "ERROR 22P02 invalid input syntax for type bigint: \"1.5\"",
            ),
            (
                "CREATE DATABASE dd CONNECTION LIMIT 1.5",
                "ERROR 42601 connection_limit requires an integer value",
            ),
            (
                "CREATE DATABASE dd CONNECTION LIMIT -5",
                "ERROR 22023 invalid connection limit: -5",
            ),
            (
                "COMMENT ON LARGE OBJECT -1 IS 'x'",
                "ERROR 42704 large object 4294967295 does not exist",
            ),
            (
                "COMMENT ON LARGE OBJECT 1.5 IS 'x'",
                "ERROR 22P02 invalid input syntax for type oid: \"1.5\"",
            ),
            (
                "SELECT JSON_SERIALIZE('{}' RETURNING int)",
                "ERROR 42804 cannot use type integer in RETURNING clause of JSON_SERIALIZE() hint: Try returning a string type or bytea.",
            ),
            (
                "SELECT JSON_ARRAY(1 RETURNING int[])",
                "ERROR 42846 cannot cast type json to integer[]",
            ),
            (
                "CREATE FUNCTION tf() RETURNS trigger LANGUAGE sql AS 'SELECT 1'",
                "ERROR 42P13 SQL functions cannot return type trigger",
            ),
            (
                "SELECT '1'::interval month to year",
                "ERROR 42601 syntax error at or near \"to\" @28",
            ),
            (
                "SELECT '1'::interval hour(2)",
                "ERROR 42601 syntax error at or near \"(\" @26",
            ),
        ] {
            assert_eq!(
                diagnostics(&run_with(engine, budget, sql)),
                [expected],
                "{sql}"
            );
        }
        assert_eq!(
            data_rows(&run_with(
                engine,
                budget,
                "SELECT '-1'::oid, '-2147483648'::oid"
            )),
            ["4294967295|2147483648"]
        );
    });
}

#[test]
fn analysis_errors_point_at_their_subject() {
    with_engine("analysis-error-positions", |engine, budget| {
        let setup = run_with(engine, budget, "CREATE TABLE pt(a int, b text)");
        assert!(error_sqlstates(&setup).is_empty());
        let function_hint = " hint: No function matches the given name and argument types. You might need to add explicit type casts.";
        let binary_hint = " hint: No operator matches the given name and argument types. You might need to add explicit type casts.";
        let unary_hint = " hint: No operator matches the given name and argument type. You might need to add an explicit type cast.";
        // Positions from PostgreSQL 18.6.
        for (sql, expected) in [
            (
                "SELECT * FROM missing_table",
                "ERROR 42P01 relation \"missing_table\" does not exist @15".to_owned(),
            ),
            (
                "SELECT 1; SELECT * FROM public.Nope",
                "ERROR 42P01 relation \"public.nope\" does not exist @25".to_owned(),
            ),
            (
                "SELECT pt.missing_col FROM pt",
                "ERROR 42703 column pt.missing_col does not exist @8".to_owned(),
            ),
            (
                "SELECT nosuch",
                "ERROR 42703 column \"nosuch\" does not exist @8".to_owned(),
            ),
            (
                "SELECT a FROM pt, pt p2",
                "ERROR 42702 column reference \"a\" is ambiguous @8".to_owned(),
            ),
            (
                "INSERT INTO pt (nosuch) VALUES (1)",
                "ERROR 42703 column \"nosuch\" of relation \"pt\" does not exist @17".to_owned(),
            ),
            (
                "UPDATE pt SET nosuch = 1",
                "ERROR 42703 column \"nosuch\" of relation \"pt\" does not exist @15".to_owned(),
            ),
            (
                "SELECT count(*), a FROM pt",
                "ERROR 42803 column \"pt.a\" must appear in the GROUP BY clause or be used in an aggregate function @18".to_owned(),
            ),
            (
                "SELECT 1::public.missingtype",
                "ERROR 42704 type \"public.missingtype\" does not exist @11".to_owned(),
            ),
            (
                "SELECT 1::missingtype[]",
                "ERROR 42704 type \"missingtype[]\" does not exist @11".to_owned(),
            ),
            (
                "CREATE TABLE pt2 (a missingtype)",
                "ERROR 42704 type \"missingtype\" does not exist @21".to_owned(),
            ),
            (
                "SELECT public.missingfn(1)",
                format!("ERROR 42883 function public.missingfn(integer) does not exist @8{function_hint}"),
            ),
            (
                "SELECT int4 'zap'",
                "ERROR 22P02 invalid input syntax for type integer: \"zap\" @13".to_owned(),
            ),
            (
                "SELECT 1 + 'x'::text",
                format!("ERROR 42883 operator does not exist: integer + text @10{binary_hint}"),
            ),
            (
                "SELECT - 'x'::text",
                format!("ERROR 42883 operator does not exist: - text @8{unary_hint}"),
            ),
            // Utility commands look names up without a position.
            (
                "DROP TABLE missing",
                "ERROR 42P01 table \"missing\" does not exist".to_owned(),
            ),
            (
                "COPY missing TO STDOUT",
                "ERROR 42P01 relation \"missing\" does not exist".to_owned(),
            ),
        ] {
            let output = run_with(engine, budget, sql);
            let reported = diagnostics(&output);
            assert_eq!(reported.last(), Some(&expected), "{sql}");
        }
    });
}

#[test]
fn long_messages_are_reported_whole() {
    with_engine("long-error-messages", |engine, budget| {
        let arguments = vec!["1::numeric"; 40].join(", ");
        let signature = vec!["numeric"; 40].join(", ");
        let output = run_with(engine, budget, &format!("SELECT missingfn({arguments})"));
        assert_eq!(
            diagnostics(&output)[0].split(" @").next().unwrap(),
            format!("ERROR 42883 function missingfn({signature}) does not exist")
        );
        let column = "c".repeat(63);
        let table = "t".repeat(63);
        let output = run_with(
            engine,
            budget,
            &format!("CREATE TABLE {table} ({column} int NULL NOT NULL)"),
        );
        assert!(
            diagnostics(&output)[0].starts_with(&format!(
                "ERROR 42601 conflicting NULL/NOT NULL declarations for column \"{column}\" of table \"{table}\""
            )),
            "{:?}",
            diagnostics(&output)
        );
        let literal = "y".repeat(300);
        let output = run_with(engine, budget, &format!("SELECT 'x' '{literal}'"));
        assert_eq!(
            diagnostics(&output),
            [format!(
                "ERROR 42601 syntax error at or near \"'{literal}'\" @12"
            )]
        );
    });
}

#[test]
fn runtime_type_checks_report_postgresql_errors() {
    with_engine("runtime-type-checks", |engine, budget| {
        let binary_hint = " hint: No operator matches the given name and argument types. You might need to add explicit type casts.";
        let unary_hint = " hint: No operator matches the given name and argument type. You might need to add an explicit type cast.";
        // Messages, SQLSTATEs and hints from PostgreSQL 18.6.
        for (sql, expected) in [
            (
                "SELECT '{}'::jsonb ? 1",
                format!("ERROR 42883 operator does not exist: jsonb ? integer @20{binary_hint}"),
            ),
            (
                "SELECT 1 -> 'a'",
                format!("ERROR 42883 operator does not exist: integer -> unknown @10{binary_hint}"),
            ),
            (
                "SELECT '{}'::jsonb #- 1",
                format!("ERROR 42883 operator does not exist: jsonb #- integer @20{binary_hint}"),
            ),
            (
                "SELECT 'x'::text ! 1",
                "ERROR 42883 operator does not exist: text ! integer".to_owned(),
            ),
            (
                "SELECT int4range(1,2) * 1",
                format!("ERROR 42883 operator does not exist: int4range * integer @23{binary_hint}"),
            ),
            (
                "SELECT ~ 'a'::text",
                format!("ERROR 42883 operator does not exist: ~ text @8{unary_hint}"),
            ),
            (
                "SELECT xmlconcat(1)",
                "ERROR 42804 argument of XMLCONCAT must be type xml, not type integer".to_owned(),
            ),
            (
                "SELECT (1)[1]",
                "ERROR 42804 cannot subscript type integer because it does not support subscripting".to_owned(),
            ),
            (
                "SELECT 1 = ANY(1)",
                "ERROR 42809 op ANY/ALL (array) requires array on right side".to_owned(),
            ),
            (
                "SELECT ('{}'::jsonb)[true]",
                "ERROR 42804 subscript type boolean is not supported hint: jsonb subscript must be coercible to either integer or text.".to_owned(),
            ),
            (
                "SELECT ('{1}'::int[])[1:true]",
                "ERROR 42804 array subscript must have type integer".to_owned(),
            ),
            (
                "SELECT JSON(1)",
                "ERROR 42846 cannot cast type integer to json".to_owned(),
            ),
            (
                "SELECT 1 IS JSON",
                "ERROR 42804 cannot use type integer in IS JSON predicate".to_owned(),
            ),
        ] {
            let reported = diagnostics(&run_with(engine, budget, sql));
            assert_eq!(
                reported[0]
                    .split(" hint:")
                    .next()
                    .map(|line| if expected.contains(" @") || expected.contains(" hint:") {
                        reported[0].clone()
                    } else {
                        line.split(" @").next().unwrap().to_owned()
                    }),
                Some(expected.clone()),
                "{sql}"
            );
        }
    });
}

#[test]
fn array_elements_and_slices_assign_and_read_as_postgresql_does() {
    with_engine("array-subscript-assignment", |engine, budget| {
        let setup = run_with(
            engine,
            budget,
            "CREATE TABLE at (id int, a int[], m int[][], t text[]); \
             INSERT INTO at VALUES (1, '{1,2,3}', '{{1,2},{3,4}}', '{a,b}'), \
             (2, NULL, NULL, NULL), (3, '{}', '{}', '{}')",
        );
        assert!(error_sqlstates(&setup).is_empty());
        let value = |engine: &mut Engine, budget: &mut Budget, column: &str, id: i32| {
            data_rows(&run_with(
                engine,
                budget,
                &format!("SELECT {column} FROM at WHERE id = {id}"),
            ))
        };
        // Results from PostgreSQL 18.6, applied in order.
        for (statement, column, id, expected) in [
            ("UPDATE at SET a[2] = 9 WHERE id = 1", "a", 1, "{1,9,3}"),
            (
                "UPDATE at SET a[5] = 7 WHERE id = 1",
                "a",
                1,
                "{1,9,3,NULL,7}",
            ),
            (
                "UPDATE at SET a[0] = 0 WHERE id = 1",
                "a",
                1,
                "[0:5]={0,1,9,3,NULL,7}",
            ),
            (
                "UPDATE at SET a[-2] = -2 WHERE id = 1",
                "a",
                1,
                "[-2:5]={-2,NULL,0,1,9,3,NULL,7}",
            ),
            ("UPDATE at SET a[3] = 5 WHERE id = 2", "a", 2, "[3:3]={5}"),
            ("UPDATE at SET a[2] = 5 WHERE id = 3", "a", 3, "[2:2]={5}"),
            (
                "UPDATE at SET a[1:2] = '{10,20}' WHERE id = 3",
                "a",
                3,
                "{10,20}",
            ),
            (
                "UPDATE at SET a[2:3] = '{7,8,9}' WHERE id = 3",
                "a",
                3,
                "{10,7,8}",
            ),
            (
                "UPDATE at SET m[1][2] = 20 WHERE id = 1",
                "m",
                1,
                "{{1,20},{3,4}}",
            ),
            (
                "UPDATE at SET m[1:2][1] = '{{7},{8}}' WHERE id = 1",
                "m",
                1,
                "{{7,20},{8,4}}",
            ),
            (
                "UPDATE at SET a[1] = '5', a[2] = 6 WHERE id = 1",
                "a",
                1,
                "[-2:5]={-2,NULL,0,5,6,3,NULL,7}",
            ),
            ("UPDATE at SET t[3] = 'c' WHERE id = 1", "t", 1, "{a,b,c}"),
        ] {
            let output = run_with(engine, budget, statement);
            assert!(
                error_sqlstates(&output).is_empty(),
                "{statement}: {:?}",
                diagnostics(&output)
            );
            assert_eq!(value(engine, budget, column, id), [expected], "{statement}");
        }
        for (statement, expected) in [
            (
                "UPDATE at SET a[5:6] = '{1}' WHERE id = 3",
                "ERROR 2202E source array too small",
            ),
            (
                "UPDATE at SET m[3][1] = 20 WHERE id = 1",
                "ERROR 2202E array subscript out of range",
            ),
            (
                "UPDATE at SET m[1] = 20 WHERE id = 1",
                "ERROR 2202E wrong number of array subscripts",
            ),
            (
                "UPDATE at SET a[NULL] = 6 WHERE id = 1",
                "ERROR 22004 array subscript in assignment must not be null",
            ),
            (
                "UPDATE at SET a[:2] = '{6,6}' WHERE id = 1",
                "ERROR 2202E source array too small",
            ),
            (
                "UPDATE at SET a[1] = 'x' WHERE id = 1",
                "ERROR 22P02 invalid input syntax for type integer: \"x\"",
            ),
            (
                "UPDATE at SET id[1] = 3",
                "ERROR 42804 cannot subscript type integer because it does not support subscripting",
            ),
        ] {
            assert_eq!(
                diagnostics(&run_with(engine, budget, statement))[0]
                    .split(" @")
                    .next()
                    .unwrap(),
                expected,
                "{statement}"
            );
        }
        let output = run_with(
            engine,
            budget,
            "INSERT INTO at (id, a[1], t[2]) VALUES (4, 5, 'z'); \
             INSERT INTO at (id, a[2:3]) VALUES (5, '{1,2}'); \
             SELECT a, t FROM at WHERE id >= 4 ORDER BY id",
        );
        assert_eq!(data_rows(&output), ["{5}|[2:2]={z}", "[2:3]={1,2}|NULL"]);
        let output = run_with(
            engine,
            budget,
            "CREATE DOMAIN posarr AS int[] CHECK (VALUE[1] > 0); \
             CREATE TABLE dt (d posarr); INSERT INTO dt VALUES ('{1,2}')",
        );
        assert!(error_sqlstates(&output).is_empty());
        assert_eq!(
            error_sqlstates(&run_with(engine, budget, "UPDATE dt SET d[1] = -1")),
            ["23514"]
        );
        assert_eq!(
            data_rows(&run_with(
                engine,
                budget,
                "UPDATE dt SET d[2] = -1; SELECT d FROM dt"
            )),
            ["{1,-1}"]
        );
        // A subscript list holding a slice is a slice throughout; otherwise it
        // addresses one element and a partial list yields NULL.
        let output = run_with(
            engine,
            budget,
            "SELECT ('{{1,2},{3,4}}'::int[])[1] AS a, ('{{1,2},{3,4}}'::int[])[1][2] AS b, \
             ('{{1,2},{3,4}}'::int[])[1:1][1] AS c, ('{{1,2},{3,4}}'::int[])[1:1][2] AS d, \
             ('{{1,2},{3,4}}'::int[])[2][1:2] AS e, ('{1,2,3}'::int[])[1][1] AS f, \
             ('[2:4]={1,2,3}'::int[])[2:3] AS g, pg_typeof(('{{1,2},{3,4}}'::int[])[1:1][1])",
        );
        assert_eq!(
            data_rows(&output),
            ["NULL|2|{{1}}|{{1,2}}|{{1,2},{3,4}}|NULL|{1,2}|integer[]"]
        );
        let output = run_with(
            engine,
            budget,
            "SELECT (SELECT ARRAY[1,2] AS x)[1], (SELECT ARRAY[1,2])[1], (SELECT ARRAY[1,2] AS y)[1:1]",
        );
        assert_eq!(row_description_names(&output), ["x", "array", "y"]);
    });
}

#[test]
fn function_body_syntax_errors_point_into_create_function() {
    with_engine("function-body-positions", |engine, budget| {
        // Positions from PostgreSQL 18.6.
        for (sql, expected) in [
            (
                "CREATE FUNCTION sf1() RETURNS int LANGUAGE sql AS 'SELEC 1'",
                "ERROR 42601 syntax error at or near \"SELEC\" @52",
            ),
            (
                "CREATE FUNCTION sf2() RETURNS int LANGUAGE sql AS $$\n  SELECT 1 +;\n$$",
                "ERROR 42601 syntax error at or near \";\" @66",
            ),
            (
                "CREATE FUNCTION sf4() RETURNS text LANGUAGE sql AS 'SELECT ''a'' +'",
                "ERROR 42601 syntax error at end of input @67",
            ),
        ] {
            assert_eq!(
                diagnostics(&run_with(engine, budget, sql)),
                [expected],
                "{sql}"
            );
        }
    });
}

#[test]
fn assignment_targets_require_postgresql_assignment_casts() {
    with_engine("assignment-coercion", |engine, budget| {
        let output = run_with(
            engine,
            budget,
            "CREATE TYPE mood AS ENUM ('a', 'b');
             CREATE TABLE t (a int, m int[], d date, j jsonb, ms mood[]);",
        );
        assert!(!String::from_utf8_lossy(&output).contains("ERROR"));
        const HINT: &str = " hint: You will need to rewrite or cast the expression.";
        // Messages and positions from PostgreSQL 18.6; `^` marks the cursor.
        for (sql, message) in [
            (
                "INSERT INTO t (a) VALUES (^'x'::text)",
                "column \"a\" is of type integer but expression is of type text",
            ),
            (
                "INSERT INTO t (a, d) VALUES (1, now()), (^'y'::text, now())",
                "column \"a\" is of type integer but expression is of type text",
            ),
            (
                "UPDATE t SET a = ^now()",
                "column \"a\" is of type integer but expression is of type timestamp with time zone",
            ),
            (
                "INSERT INTO t (a) VALUES (^$$1$$::varchar)",
                "column \"a\" is of type integer but expression is of type character varying",
            ),
            (
                "UPDATE t SET ^m[1] = 'x'::text",
                "subscripted assignment to \"m\" requires type integer but expression is of type text",
            ),
            (
                "UPDATE t SET ^m[1:2] = '{1}'::text[]",
                "subscripted assignment to \"m\" requires type integer[] but expression is of type text[]",
            ),
            (
                "INSERT INTO t (^m[1:2]) SELECT '{1,2}' UNION ALL SELECT '{3,4}'",
                "subscripted assignment to \"m\" requires type integer[] but expression is of type text",
            ),
            (
                "INSERT INTO t (a, ^m[1]) SELECT 1, 'x'::text",
                "subscripted assignment to \"m\" requires type integer but expression is of type text",
            ),
            (
                "UPDATE t SET ^j['k'] = 1",
                "subscripted assignment to \"j\" requires type jsonb but expression is of type integer",
            ),
            (
                "INSERT INTO t (a) SELECT ^'1' UNION SELECT '2'",
                "column \"a\" is of type integer but expression is of type text",
            ),
            (
                "UPDATE t SET (a, d) = (SELECT ^'x'::text, 1)",
                "column \"a\" is of type integer but expression is of type text",
            ),
            (
                "UPDATE t SET a = ^(SELECT '1')",
                "column \"a\" is of type integer but expression is of type text",
            ),
        ] {
            let at = sql.find('^').unwrap();
            let written = sql.replacen('^', "", 1);
            let output = run_with(engine, budget, &written);
            assert_eq!(
                diagnostics(&output),
                [format!("ERROR 42804 {message} @{}{HINT}", at + 1)],
                "{written}"
            );
        }
        // Untyped literals and assignment casts convert, as in PostgreSQL.
        let output = run_with(
            engine,
            budget,
            "INSERT INTO t (a, m) VALUES ('1', '{2}'), (1.5, NULL), (3::bigint, NULL);
             INSERT INTO t (a, m[1]) SELECT '4', '5';
             INSERT INTO t (ms) VALUES ('{a}');
             UPDATE t SET ms[1] = 'b' WHERE ms IS NOT NULL;
             INSERT INTO t (j) SELECT jsonb_agg(g ORDER BY g) FROM generate_series(1, 2) g;
             SELECT a, m, ms FROM t WHERE j IS NULL ORDER BY a;
             SELECT j FROM t WHERE j IS NOT NULL;",
        );
        assert_eq!(
            data_rows(&output),
            [
                "1|{2}|NULL",
                "2|NULL|NULL",
                "3|NULL|NULL",
                "4|{5}|NULL",
                "NULL|NULL|{b}",
                "[1, 2]"
            ],
            "{}",
            String::from_utf8_lossy(&output)
        );
    });
}

#[test]
fn plpgsql_conversions_read_text_output_and_sql_results_are_checked_at_create() {
    with_engine("plpgsql-conversion", |engine, budget| {
        // PostgreSQL 18.6 converts a value without an assignment cast
        // through its text output, which the target's input then reads.
        for (body, message) in [
            (
                "RETURNS int LANGUAGE plpgsql AS $$ DECLARE x int; BEGIN x := true; RETURN x; END $$",
                "ERROR 22P02 invalid input syntax for type integer: \"t\"",
            ),
            (
                "RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN true; END $$",
                "ERROR 22P02 invalid input syntax for type integer: \"t\"",
            ),
            (
                "RETURNS int LANGUAGE plpgsql AS $$ DECLARE x int[]; BEGIN x[1] := 'abc'::text; RETURN 1; END $$",
                "ERROR 22P02 invalid input syntax for type integer: \"abc\"",
            ),
        ] {
            let output = run_with(
                engine,
                budget,
                &format!("CREATE OR REPLACE FUNCTION conv() {body}; SELECT conv();"),
            );
            assert_eq!(diagnostics(&output), ["CREATE FUNCTION", message], "{body}");
        }
        let output = run_with(
            engine,
            budget,
            "CREATE FUNCTION outconv(OUT a int) LANGUAGE plpgsql AS $$ BEGIN a := 1.6; END $$;
             SELECT outconv();",
        );
        assert_eq!(data_rows(&output), ["2"]);
        // PostgreSQL's grammar reads `RETURNS` after options as `RETURNS NULL
        // ON NULL INPUT`, before it misses the result type.
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "CREATE FUNCTION late() LANGUAGE sql RETURNS int AS 'SELECT 1'",
            )),
            ["ERROR 42601 syntax error at or near \"int\" @45"]
        );
        // A SQL function's final query is checked against its declared
        // result when it is created.
        for (body, detail) in [
            (
                "RETURNS int AS $$SELECT 'abc'$$",
                "Actual return type is text.",
            ),
            (
                "RETURNS int AS $$SELECT 1, 2$$",
                "Final statement must return exactly one column.",
            ),
            (
                "RETURNS SETOF int AS $$SELECT now()$$",
                "Actual return type is timestamp with time zone.",
            ),
            ("RETURNS int AS $$SELECT 1::bigint$$", ""),
            ("RETURNS int AS $$SELECT 1.5 UNION SELECT 2$$", ""),
        ] {
            let output = run_with(
                engine,
                budget,
                &format!("CREATE OR REPLACE FUNCTION sqlres() {body} LANGUAGE sql"),
            );
            let expected = if detail.is_empty() {
                "CREATE FUNCTION".to_owned()
            } else {
                format!(
                    "ERROR 42P13 return type mismatch in function declared to return integer detail: {detail}"
                )
            };
            assert_eq!(diagnostics(&output), [expected], "{body}");
        }
    });
}

#[test]
fn plpgsql_data_changes_return_rows_into_variables() {
    with_engine("plpgsql-returning-into", |engine, budget| {
        let output = run_with(
            engine,
            budget,
            "CREATE TABLE r (id int, v text);
             INSERT INTO r VALUES (1, 'a'), (2, 'b');
             CREATE FUNCTION ins() RETURNS text LANGUAGE plpgsql AS $$
               DECLARE x int; y text;
               BEGIN INSERT INTO r VALUES (3, 'c') RETURNING id, v INTO x, y;
               RETURN x || y || found; END $$;
             CREATE FUNCTION del_none() RETURNS text LANGUAGE plpgsql AS $$
               DECLARE x int; n int;
               BEGIN DELETE FROM r WHERE id > 100 RETURNING id INTO x;
               GET DIAGNOSTICS n = ROW_COUNT;
               RETURN coalesce(x::text, 'null') || found || n; END $$;
             CREATE FUNCTION del_record() RETURNS text LANGUAGE plpgsql AS $$
               DECLARE x record;
               BEGIN DELETE FROM r WHERE id = 3 RETURNING * INTO x;
               RETURN x.id || x.v; END $$;
             CREATE FUNCTION ins_select() RETURNS int LANGUAGE plpgsql AS $$
               DECLARE x int; y int;
               BEGIN INSERT INTO r SELECT 7, 'g' RETURNING id INTO STRICT x, y;
               RETURN x + coalesce(y, 0); END $$;
             SELECT ins(); SELECT del_none(); SELECT del_record(); SELECT ins_select();",
        );
        assert_eq!(
            data_rows(&output),
            ["3ctrue", "nullfalse0", "3c", "7"],
            "{}",
            String::from_utf8_lossy(&output)
        );
        // Errors from PostgreSQL 18.6.
        for (body, expected) in [
            (
                "DECLARE x int; BEGIN UPDATE r SET v = 'z' RETURNING id INTO x; END",
                "ERROR P0003 query returned more than one row hint: Make sure the query returns a single row, or use LIMIT 1.",
            ),
            (
                "DECLARE x int; BEGIN UPDATE r SET v = 'z' WHERE id > 100 RETURNING id INTO STRICT x; END",
                "ERROR P0002 query returned no rows",
            ),
            (
                "BEGIN UPDATE r SET v = 'q' RETURNING id; END",
                "ERROR 42601 query has no destination for result data",
            ),
            (
                "DECLARE x int; BEGIN UPDATE r SET v = 'q' INTO x; END",
                "ERROR 42601 INTO used with a command that cannot return data",
            ),
            (
                "DECLARE x int; BEGIN SELECT id INTO STRICT x FROM r; END",
                "ERROR P0003 query returned more than one row hint: Make sure the query returns a single row, or use LIMIT 1.",
            ),
            (
                "DECLARE x int; BEGIN EXECUTE 'SELECT id FROM r' INTO STRICT x; END",
                "ERROR P0003 query returned more than one row",
            ),
        ] {
            let output = run_with(engine, budget, &format!("DO $$ {body} $$"));
            assert_eq!(diagnostics(&output), [expected], "{body}");
        }
        // A query's extra columns are ignored and missing ones leave NULL.
        let output = run_with(
            engine,
            budget,
            "DO $$ DECLARE x int; y int; BEGIN
               SELECT 1, 2 INTO x; RAISE NOTICE '%', x;
               SELECT 5 INTO x, y; RAISE NOTICE '% %', x, y;
               EXECUTE 'SELECT 6' INTO x, y; RAISE NOTICE '% %', x, y;
             END $$",
        );
        assert_eq!(
            diagnostics(&output),
            [
                "NOTICE 00000 1",
                "NOTICE 00000 5 <NULL>",
                "NOTICE 00000 6 <NULL>",
                "DO"
            ]
        );
    });
}

#[test]
fn routine_name_qualifies_its_parameters() {
    with_engine("qualified-parameters", |engine, budget| {
        let output = run_with(
            engine,
            budget,
            "CREATE TABLE fq_rows (a int);
             INSERT INTO fq_rows VALUES (10);
             CREATE FUNCTION fq(a int) RETURNS int LANGUAGE sql AS $$ SELECT fq.a + 1 $$;
             CREATE FUNCTION fp(a int) RETURNS int LANGUAGE plpgsql AS $$
               DECLARE b int;
               BEGIN b := fp.a * 2;
               SELECT fp.a + b + fq_rows.a INTO b FROM fq_rows;
               UPDATE fq_rows SET a = fp.a WHERE fq_rows.a = 10;
               RETURN b; END $$;
             SELECT fq(1), fp(3); SELECT a FROM fq_rows;",
        );
        assert_eq!(
            data_rows(&output),
            ["2|19", "3"],
            "{}",
            String::from_utf8_lossy(&output)
        );
    });
}

#[test]
fn create_function_analyses_data_change_targets() {
    with_engine("create-function-dml-targets", |engine, budget| {
        let output = run_with(engine, budget, "CREATE TABLE wt (a int, b text)");
        assert_eq!(diagnostics(&output), ["CREATE TABLE"]);
        const HINT: &str = " hint: You will need to rewrite or cast the expression.";
        // Messages and positions from PostgreSQL 18.6; `^` marks the cursor.
        for (sql, expected) in [
            (
                "CREATE FUNCTION w1(x int) RETURNS void LANGUAGE sql AS $$ UPDATE wt SET ^nosuch = x $$",
                "ERROR 42703 column \"nosuch\" of relation \"wt\" does not exist".to_owned(),
            ),
            (
                "CREATE FUNCTION w2(x int) RETURNS void LANGUAGE sql AS $$ INSERT INTO wt (a, ^nosuch) VALUES (x, 1) $$",
                "ERROR 42703 column \"nosuch\" of relation \"wt\" does not exist".to_owned(),
            ),
            (
                "CREATE FUNCTION w3(x text) RETURNS void LANGUAGE sql AS $$ UPDATE wt SET a = ^x $$",
                format!(
                    "ERROR 42804 column \"a\" is of type integer but expression is of type text{HINT}"
                ),
            ),
            (
                "CREATE FUNCTION w4(x int) RETURNS void LANGUAGE sql BEGIN ATOMIC; UPDATE wt SET ^nosuch = x; END",
                "ERROR 42703 column \"nosuch\" of relation \"wt\" does not exist".to_owned(),
            ),
            (
                "CREATE FUNCTION w5(x int) RETURNS void LANGUAGE sql BEGIN ATOMIC UPDATE wt SET a = ^'q'::text; END",
                format!(
                    "ERROR 42804 column \"a\" is of type integer but expression is of type text{HINT}"
                ),
            ),
        ] {
            let at = sql.find('^').unwrap();
            let written = sql.replacen('^', "", 1);
            let output = run_with(engine, budget, &written);
            let (message, hint) = expected
                .split_once(" hint:")
                .map_or((expected.as_str(), String::new()), |(message, hint)| {
                    (message, format!(" hint:{hint}"))
                });
            assert_eq!(
                diagnostics(&output),
                [format!("{message} @{}{hint}", at + 1)],
                "{written}"
            );
        }
        let output = run_with(
            engine,
            budget,
            "CREATE FUNCTION w6(x int) RETURNS void LANGUAGE sql BEGIN ATOMIC; ; INSERT INTO wt VALUES (x, 'z'); END;
             SELECT w6(4); SELECT a, b FROM wt",
        );
        assert_eq!(
            data_rows(&output),
            ["NULL", "4|z"],
            "{}",
            String::from_utf8_lossy(&output)
        );
    });
}

#[test]
fn subquery_constants_convert_before_rows_are_read() {
    with_engine("subquery-literals", |engine, budget| {
        let output = run_with(engine, budget, "CREATE TABLE st (a int, b text)");
        assert_eq!(diagnostics(&output), ["CREATE TABLE"]);
        // Positions from PostgreSQL 18.6; `^` marks the cursor. The table is
        // empty, so only analysis can find these.
        for (sql, literal) in [
            ("SELECT (SELECT a FROM st WHERE a = ^'q')", "q"),
            (
                "SELECT 1 WHERE EXISTS (SELECT 1 FROM st WHERE a = ^'q')",
                "q",
            ),
            ("SELECT 1 WHERE 1 IN (SELECT a FROM st WHERE a > ^'x')", "x"),
            (
                "SELECT (SELECT a FROM st s2 WHERE s2.a = ^'q' UNION SELECT 1)",
                "q",
            ),
            (
                "SELECT (SELECT (SELECT a FROM st x WHERE x.a = ^'zz'))",
                "zz",
            ),
            (
                "SELECT * FROM st WHERE a IN (SELECT a FROM st t2 WHERE t2.a = ^'p')",
                "p",
            ),
        ] {
            let at = sql.find('^').unwrap();
            let written = sql.replacen('^', "", 1);
            assert_eq!(
                diagnostics(&run_with(engine, budget, &written)),
                [format!(
                    "ERROR 22P02 invalid input syntax for type integer: \"{literal}\" @{}",
                    at + 1
                )],
                "{written}"
            );
        }
        let output = run_with(
            engine,
            budget,
            "SELECT (SELECT b FROM st WHERE b = 'q'), (SELECT a FROM st WHERE a = '5')",
        );
        assert_eq!(data_rows(&output), ["NULL|NULL"]);
    });
}

#[test]
fn unsupported_system_columns_fail_loudly_and_tables_cannot_take_their_names() {
    with_engine("system-columns", |engine, budget| {
        run_with(
            engine,
            budget,
            "CREATE TABLE st (a int); CREATE VIEW sv AS SELECT 1 AS ctid",
        );
        for (sql, expected) in [
            (
                "SELECT ctid FROM st",
                "ERROR 0A000 system column \"ctid\" is not supported",
            ),
            (
                "SELECT s.xmin FROM st s",
                "ERROR 0A000 system column \"xmin\" is not supported",
            ),
            (
                "SELECT tableoid FROM st",
                "ERROR 0A000 system column \"tableoid\" is not supported",
            ),
            // Views and subqueries have no system columns.
            (
                "SELECT xmin FROM sv",
                "ERROR 42703 column \"xmin\" does not exist @8",
            ),
            (
                "SELECT ctid FROM (SELECT 1) s",
                "ERROR 42703 column \"ctid\" does not exist @8",
            ),
            (
                "CREATE TABLE sc (cmin int)",
                "ERROR 42701 column name \"cmin\" conflicts with a system column name",
            ),
            (
                "ALTER TABLE st ADD COLUMN IF NOT EXISTS xmax int",
                "ERROR 42701 column name \"xmax\" conflicts with a system column name",
            ),
            (
                "ALTER TABLE st RENAME COLUMN a TO ctid",
                "ERROR 42701 column name \"ctid\" conflicts with a system column name",
            ),
        ] {
            assert_eq!(
                diagnostics(&run_with(engine, budget, sql)),
                [expected],
                "{sql}"
            );
        }
        assert_eq!(
            data_rows(&run_with(engine, budget, "SELECT ctid FROM sv")),
            ["1"]
        );
    });
}

#[test]
fn a_plain_source_subquery_cannot_see_the_target_columns() {
    with_engine("hidden-target-columns", |engine, budget| {
        run_with(engine, budget, "CREATE TABLE lt (id int, v int)");
        for sql in [
            "UPDATE lt SET v = 1 FROM (SELECT v AS x) s",
            "DELETE FROM lt USING (SELECT v AS x) s",
        ] {
            let position = sql.find("v AS").unwrap() + 1;
            assert_eq!(
                diagnostics(&run_with(engine, budget, sql)),
                [format!(
                    "ERROR 42703 column \"v\" does not exist @{position} detail: There is a \
                     column named \"v\" in table \"lt\", but it cannot be referenced from this \
                     part of the query."
                )],
                "{sql}"
            );
        }
        // An item to the left could be read through LATERAL, also from a
        // subquery nested in the derived table or in its own FROM item.
        for sql in [
            "SELECT * FROM lt, (SELECT v) s",
            "SELECT * FROM lt, (SELECT (SELECT v)) s",
            "SELECT * FROM lt, (SELECT 1 FROM (SELECT (SELECT v)) x) s",
        ] {
            assert_eq!(
                diagnostics(&run_with(engine, budget, sql)),
                [format!(
                    "ERROR 42703 column \"v\" does not exist @{} hint: To reference that \
                     column, you must mark this subquery with LATERAL. detail: There is a \
                     column named \"v\" in table \"lt\", but it cannot be referenced from \
                     this part of the query.",
                    sql.find("v)").unwrap() + 1
                )],
                "{sql}"
            );
        }
        // A FROM item inside the derived table supplies the column.
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "SELECT * FROM lt, (SELECT (SELECT v) FROM lt l2) s"
            )),
            ["SELECT 0"]
        );
        let sql = "UPDATE lt SET v = 1 FROM (SELECT lt.v) s";
        assert_eq!(
            diagnostics(&run_with(engine, budget, sql)),
            [format!(
                "ERROR 42P01 invalid reference to FROM-clause entry for table \"lt\" @{} \
                 detail: There is an entry for table \"lt\", but it cannot be referenced \
                 from this part of the query.",
                sql.find("lt.v").unwrap() + 1
            )]
        );
    });
}

#[test]
fn a_lateral_body_reads_only_the_items_to_its_left() {
    with_engine("lateral-left-items", |engine, budget| {
        for sql in [
            "CREATE TABLE lt (id int, v int)",
            "INSERT INTO lt VALUES (1, 10), (2, 20)",
            "CREATE TABLE lu (id int, v int)",
            "INSERT INTO lu VALUES (1, 100)",
            "CREATE TABLE lw (w int)",
            "INSERT INTO lw VALUES (5)",
        ] {
            run_with(engine, budget, sql);
        }
        // The item's own columns, and later items', are not its body's.
        for (sql, rows) in [
            (
                "SELECT * FROM lt, LATERAL (SELECT v) s ORDER BY 1",
                &["1|10|10", "2|20|20"][..],
            ),
            (
                "SELECT * FROM lt a, LATERAL (SELECT v) s, lu ORDER BY 1",
                &["1|10|10|1|100", "2|20|20|1|100"],
            ),
            (
                "SELECT * FROM lt a JOIN lu b USING (v), LATERAL (SELECT v) s",
                &[],
            ),
            (
                "SELECT * FROM lt a LEFT JOIN LATERAL (SELECT v WHERE v > 10) s ON true ORDER BY 1",
                &["1|10|NULL", "2|20|20"],
            ),
            (
                "SELECT * FROM lt, unnest(ARRAY[v, v + 1]) AS v ORDER BY 1, 3",
                &["1|10|10", "1|10|11", "2|20|20", "2|20|21"],
            ),
            (
                "SELECT * FROM lt a RIGHT JOIN LATERAL (SELECT 1 AS one) s ON true ORDER BY 1",
                &["1|10|1", "2|20|1"],
            ),
            (
                "UPDATE lt SET v = v + s.w FROM lw, LATERAL (SELECT w) s WHERE lt.id = 1 \
                 RETURNING lt.id, lt.v",
                &["1|15"],
            ),
            // A comma ends the join tree: `lw` crosses `lt a RIGHT JOIN lu s`.
            (
                "SELECT count(*) FROM lw, lt a RIGHT JOIN lu s ON true",
                &["2"],
            ),
            ("DELETE FROM lt USING lw WHERE false RETURNING *", &[]),
        ] {
            assert_eq!(data_rows(&run_with(engine, budget, sql)), rows, "{sql}");
        }
        for (sql, expected) in [
            (
                "SELECT * FROM lt a JOIN lu b ON a.id = b.id, LATERAL (SELECT v) s",
                "ERROR 42702 column reference \"v\" is ambiguous @62".to_string(),
            ),
            (
                "SELECT * FROM lt a FULL JOIN LATERAL (SELECT v) s ON true",
                "ERROR 42P10 invalid reference to FROM-clause entry for table \"a\" @46 \
                 detail: The combining JOIN type must be INNER or LEFT for a LATERAL reference."
                    .to_string(),
            ),
            (
                "UPDATE lt SET v = 1 FROM lu x, LATERAL (SELECT v) s",
                "ERROR 42P10 invalid reference to FROM-clause entry for table \"lt\" @48 \
                 hint: There is an entry for table \"lt\", but it cannot be referenced from \
                 this part of the query."
                    .to_string(),
            ),
        ] {
            assert_eq!(
                diagnostics(&run_with(engine, budget, sql)),
                [expected],
                "{sql}"
            );
        }
    });
}

#[test]
fn plpgsql_labels_name_blocks_loops_and_their_variables() {
    with_engine("plpgsql-labels", |engine, budget| {
        // Results from PostgreSQL 18.6.
        let output = run_with(
            engine,
            budget,
            "CREATE FUNCTION lb_nested() RETURNS text LANGUAGE plpgsql AS $$
               <<blo>> DECLARE x int := 1;
               BEGIN
                 <<bli>> DECLARE x int := x + 10; y int := x;
                 BEGIN RETURN blo.x || ',' || bli.x || ',' || x || ',' || y; END bli;
               END blo $$;
             CREATE FUNCTION lb_loops() RETURNS int LANGUAGE plpgsql AS $$
               DECLARE n int := 0;
               BEGIN
                 <<o>> FOR i IN 1..3 LOOP
                   <<inr>> FOR j IN 1..3 LOOP
                     n := n + o.i * 10 + inr.j;
                     CONTINUE o WHEN j = 2;
                     EXIT o WHEN i = 3;
                   END LOOP inr;
                 END LOOP o;
                 <<blk>> BEGIN n := n + 1000; EXIT blk; n := 0; END blk;
                 RETURN n;
               END $$;
             CREATE FUNCTION lb_shadow(p int) RETURNS text LANGUAGE plpgsql AS $$
               <<b>> DECLARE i int := 7; t text := '';
               BEGIN
                 <<lp>> FOR i IN 1..2 LOOP b.i := b.i + lp.i; t := t || i; END LOOP;
                 FOR k IN 1..2 LOOP
                   DECLARE s int := k * 2;
                   BEGIN t := t || s; s := 0; END;
                 END LOOP;
                 PERFORM 1;
                 RETURN i || ',' || t || ',' || lb_shadow.p || ',' || lb_shadow.found;
               END $$;
             SELECT lb_nested(), lb_loops(), lb_shadow(4);",
        );
        assert_eq!(
            data_rows(&output),
            ["1,11,11,11|1097|10,1224,4,true"],
            "{}",
            String::from_utf8_lossy(&output)
        );
        for (sql, expected) in [
            (
                "DO $$ BEGIN <<b>> BEGIN CONTINUE b; END; END $$",
                "42601 block label \"b\" cannot be used in CONTINUE",
            ),
            (
                "DO $$ BEGIN BEGIN EXIT; END; END $$",
                "42601 EXIT cannot be used outside a loop, unless it has a label",
            ),
            (
                "DO $$ BEGIN EXIT nolabel; END $$",
                "42601 there is no label \"nolabel\" attached to any block or loop enclosing this statement",
            ),
            (
                "DO $$ <<a>> BEGIN NULL; END b $$",
                "42601 end label \"b\" differs from block's label \"a\"",
            ),
            (
                "DO $$ BEGIN <<lp>> LOOP EXIT lp; END LOOP wrong; END $$",
                "42601 end label \"wrong\" differs from block's label \"lp\"",
            ),
            (
                "DO $$ BEGIN NULL; END b $$",
                "42601 end label \"b\" specified for unlabeled block",
            ),
            (
                "DO $$ BEGIN <<x>> NULL; END $$",
                "42601 syntax error at or near \"NULL\"",
            ),
            (
                "DO $$ DECLARE <<x>> BEGIN NULL; END $$",
                "42601 block label must be placed before DECLARE, not after",
            ),
            (
                "DO $$ BEGIN DECLARE a int; a int; BEGIN NULL; END; END $$",
                "42601 duplicate declaration at or near \"a\" @28",
            ),
            (
                "DO $$ BEGIN DECLARE c CONSTANT int := 1; BEGIN c := 2; END; END $$",
                "22005 variable \"c\" is declared CONSTANT @48",
            ),
            (
                "DO $$ <<b>> DECLARE c CONSTANT int := 1; BEGIN b.c[1] := 2; END $$",
                "22005 variable \"c\" is declared CONSTANT @48",
            ),
            (
                "DO $$ DECLARE c CONSTANT int := 1; BEGIN SELECT c INTO c; END $$",
                "22005 variable \"c\" is declared CONSTANT @56",
            ),
            (
                "DO $$ DECLARE c CONSTANT int := 1; BEGIN GET DIAGNOSTICS c = ROW_COUNT; END $$",
                "22005 variable \"c\" is declared CONSTANT @58",
            ),
            (
                "DO $$ DECLARE c CONSTANT int := 1; BEGIN FOR c IN SELECT 1 LOOP END LOOP; END $$",
                "22005 variable \"c\" is declared CONSTANT @46",
            ),
            (
                "DO $$ <<b>> DECLARE x int := 1; BEGIN b.nope := 3; END $$",
                "42601 \"b.nope\" is not a known variable",
            ),
        ] {
            let found = diagnostics(&run_with(engine, budget, sql));
            assert!(
                found
                    .first()
                    .is_some_and(|line| line.starts_with(&format!("ERROR {expected}"))),
                "{sql}: {found:?}"
            );
        }
        let output = run_with(
            engine,
            budget,
            "DO $$ BEGIN DECLARE z int := 1/0; BEGIN NULL; EXCEPTION WHEN others THEN NULL; END; END $$",
        );
        assert_eq!(
            diagnostics(&output)[0],
            "ERROR 22012 division by zero",
            "a block's declarations are initialized outside its handlers"
        );
    });
}

#[test]
fn data_modifying_with_runs_once_under_one_snapshot_and_unread_last() {
    with_engine("data-modifying-with-order", |engine, budget| {
        // Results from PostgreSQL 18.6. A PL/pgSQL call restarts the main
        // query without repeating the WITH query's insert.
        let output = run_with(
            engine,
            budget,
            "CREATE TABLE dw (a int);
             CREATE TABLE dw_log (msg text);
             CREATE FUNCTION dw_count() RETURNS bigint LANGUAGE plpgsql VOLATILE AS $$
               BEGIN RETURN (SELECT count(*) FROM dw); END $$;
             CREATE FUNCTION dw_note(t text) RETURNS int LANGUAGE plpgsql VOLATILE AS $$
               BEGIN INSERT INTO dw_log VALUES (t); RETURN 1; END $$;
             WITH w AS (INSERT INTO dw VALUES (1)) SELECT dw_count();
             WITH w AS (INSERT INTO dw VALUES (2) RETURNING a)
               SELECT dw_count(), (SELECT count(*) FROM dw), (SELECT a FROM w);
             WITH a AS (INSERT INTO dw VALUES (dw_note('first'))),
                  b AS (INSERT INTO dw VALUES (dw_note('second')))
               SELECT dw_note('main');
             SELECT string_agg(msg, ',') FROM dw_log;
             WITH d AS (DELETE FROM dw) INSERT INTO dw VALUES (99);
             SELECT string_agg(a::text, ',') FROM dw;",
        );
        assert_eq!(
            data_rows(&output),
            ["0", "1|1|2", "1", "main,second,first", "99"],
            "{}",
            String::from_utf8_lossy(&output)
        );
    });
}

#[test]
fn plpgsql_array_elements_convert_like_variables() {
    with_engine("plpgsql-element-assignment", |engine, budget| {
        // PostgreSQL 18.6: assignment cast, otherwise through text.
        let output = run_with(
            engine,
            budget,
            "DO $$ DECLARE x int[] := '{1,2}'; BEGIN x[1] := true; END $$",
        );
        assert_eq!(
            diagnostics(&output)[0],
            "ERROR 22P02 invalid input syntax for type integer: \"t\""
        );
        let output = run_with(
            engine,
            budget,
            "CREATE FUNCTION ea() RETURNS text LANGUAGE plpgsql AS $$
               DECLARE x int[] := '{1,2}'; t text[] := '{a}'; n numeric := 2.6;
               BEGIN x[1] := '7'; x[2] := n; t[2] := 5; t[3] := true;
               RETURN x::text || t::text; END $$;
             SELECT ea();",
        );
        assert_eq!(data_rows(&output), ["{7,3}{a,5,true}"]);
    });
}

#[test]
fn strict_failures_print_the_statement_variables_when_asked() {
    with_engine("print-strict-params", |engine, budget| {
        // DETAIL texts from PostgreSQL 18.6.
        let output = run_with(
            engine,
            budget,
            "CREATE TABLE sp (a int, b text);
             INSERT INTO sp VALUES (1, 'x'), (1, 'y');
             CREATE FUNCTION sp_on(p int, q text) RETURNS text LANGUAGE plpgsql AS $$
               #print_strict_params on
               DECLARE r text; z int := 3;
               BEGIN SELECT b INTO STRICT r FROM sp WHERE a = p AND b <> q AND z = 3;
               RETURN r; END $$;
             CREATE FUNCTION sp_execute(p text) RETURNS text LANGUAGE plpgsql AS $$
               #print_strict_params on
               DECLARE r text;
               BEGIN EXECUTE 'SELECT b FROM sp WHERE a = $1' INTO STRICT r USING length(p);
               RETURN r; END $$;
",
        );
        assert!(
            !diagnostics(&output)
                .iter()
                .any(|line| line.starts_with("ERROR")),
            "{:?}",
            diagnostics(&output)
        );
        for (sql, expected) in [
            (
                "SELECT sp_on(2, 'it''s')",
                "ERROR P0002 query returned no rows detail: parameters: p = '2', q = 'it''s', z = '3'",
            ),
            (
                "SELECT sp_execute('x')",
                "ERROR P0003 query returned more than one row detail: parameters: $1 = '1'",
            ),
        ] {
            assert_eq!(
                diagnostics(&run_with(engine, budget, sql))[0],
                expected,
                "{sql}"
            );
        }
        // The creating session compiles the function with its setting.
        let output = run_with(
            engine,
            budget,
            "SET plpgsql.print_strict_params = on;
             CREATE FUNCTION sp_session(p int) RETURNS int LANGUAGE plpgsql AS $$
               DECLARE r int;
               BEGIN UPDATE sp SET a = a WHERE a = p RETURNING a INTO STRICT r;
               RETURN r; END $$;
             RESET plpgsql.print_strict_params;
             SELECT sp_session(5);",
        );
        assert!(
            diagnostics(&output).contains(
                &"ERROR P0002 query returned no rows detail: parameters: p = '5'".to_string()
            ),
            "{:?}",
            diagnostics(&output)
        );
    });
}

#[test]
fn data_change_subquery_constants_convert_before_rows_are_read() {
    with_engine("dml-subquery-literals", |engine, budget| {
        let output = run_with(
            engine,
            budget,
            "CREATE TABLE ds (a int); CREATE TABLE du (a int)",
        );
        assert_eq!(diagnostics(&output), ["CREATE TABLE", "CREATE TABLE"]);
        // Positions from PostgreSQL 18.6; `^` marks the cursor. Both tables
        // are empty, so only analysis can find these.
        for (sql, literal) in [
            (
                "DELETE FROM ds WHERE a IN (SELECT a FROM du WHERE a = ^'q')",
                "q",
            ),
            ("UPDATE ds SET a = (SELECT a FROM du WHERE a = ^'zz')", "zz"),
            (
                "INSERT INTO ds VALUES ((SELECT a FROM du WHERE a = ^'r'))",
                "r",
            ),
            ("INSERT INTO ds SELECT a FROM du WHERE a = ^'k'", "k"),
            (
                "DELETE FROM ds RETURNING (SELECT a FROM du WHERE a = ^'ret')",
                "ret",
            ),
        ] {
            let at = sql.find('^').unwrap();
            let written = sql.replacen('^', "", 1);
            assert_eq!(
                diagnostics(&run_with(engine, budget, &written)),
                [format!(
                    "ERROR 22P02 invalid input syntax for type integer: \"{literal}\" @{}",
                    at + 1
                )],
                "{written}"
            );
        }
    });
}
