//! Client-visible scanner, parse-analysis and cast diagnostics compared with
//! PostgreSQL 18.6: SQLSTATE, message, cursor position and notice order.

use super::*;

/// Each ErrorResponse and NoticeResponse as `SEVERITY SQLSTATE message`,
/// followed by ` @position` when it carries a cursor and ` hint: ...` when
/// it carries a hint, interleaved with the command tags of the response.
fn diagnostics(bytes: &[u8]) -> Vec<String> {
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

fn with_engine(name: &str, run: impl FnOnce(&mut Engine, &mut Budget)) {
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
