//! ErrorResponse and NoticeResponse fields beyond the message, compared with
//! PostgreSQL 18.6's raw responses: detail, hint, the object fields (`s`,
//! `t`, `c`, `d`, `n`), the error context (`W`) and the internal query (`q`,
//! `p`).

use super::*;

/// Each ErrorResponse (`E`) and NoticeResponse (`N`) with every field
/// except severity, in wire order, as `code=value` joined by `|`.
fn reports(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let tag = bytes[offset];
        let length = u32::from_be_bytes(bytes[offset + 1..offset + 5].try_into().unwrap()) as usize;
        let payload = &bytes[offset + 5..offset + 1 + length];
        if matches!(tag, b'E' | b'N') {
            let mut fields = vec![String::from(tag as char)];
            let mut rest = payload;
            while let Some((&field, remaining)) = rest.split_first() {
                if field == 0 {
                    break;
                }
                let end = remaining.iter().position(|&byte| byte == 0).unwrap();
                if !matches!(field, b'S' | b'V') {
                    fields.push(format!(
                        "{}={}",
                        field as char,
                        String::from_utf8_lossy(&remaining[..end])
                    ));
                }
                rest = &remaining[end + 1..];
            }
            out.push(fields.join("|"));
        }
        offset += 1 + length;
    }
    out
}

fn with_engine(name: &str, run: impl FnOnce(&mut Engine, &mut Budget)) {
    let mut config = test_config(name);
    config.max_routines = 16;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    run(&mut engine, &mut budget);
    drop(engine);
    std::fs::remove_dir_all(config.data_dir).unwrap();
}

fn assert_reports(engine: &mut Engine, budget: &mut Budget, cases: &[(&str, &[&str])]) {
    for (sql, expected) in cases {
        let output = run_with(engine, budget, sql);
        assert_eq!(reports(&output), *expected, "{sql}");
    }
}

#[test]
fn constraint_violations_report_postgresql_detail_and_objects() {
    with_engine("constraint-violation-fields", |engine, budget| {
        run_with(
            engine,
            budget,
            "CREATE SCHEMA vs; \
             CREATE TABLE vs.t (a int NOT NULL, b text CHECK (b <> 'x'), c int, d date, \
                 e numeric, f text[], g bool, CONSTRAINT tc UNIQUE (c, a)); \
             CREATE TABLE vs.p (id int PRIMARY KEY); \
             CREATE TABLE vs.c (pid int REFERENCES vs.p, x int); \
             INSERT INTO vs.p VALUES (1); INSERT INTO vs.c VALUES (1, 1); \
             CREATE DOMAIN vs.pos AS int CONSTRAINT pos_ok CHECK (VALUE > 0) NOT NULL; \
             CREATE TABLE vs.ex (r int4range, EXCLUDE USING gist (r WITH &&)); \
             CREATE UNIQUE INDEX ui ON vs.t (lower(b));",
        );
        assert_reports(
            engine,
            budget,
            &[
                (
                    "INSERT INTO vs.t VALUES (NULL, 'y z', 1, '2020-01-02', 1.50, '{a,\"b c\"}', true)",
                    &[
                        "E|C=23502|M=null value in column \"a\" of relation \"t\" violates not-null constraint|D=Failing row contains (null, y z, 1, 2020-01-02, 1.50, {a,\"b c\"}, t).|s=vs|t=t|c=a",
                    ],
                ),
                (
                    "INSERT INTO vs.t VALUES (NULL, repeat('é', 70), 1, NULL, NULL, NULL, NULL)",
                    &[
                        "E|C=23502|M=null value in column \"a\" of relation \"t\" violates not-null constraint|D=Failing row contains (null, éééééééééééééééééééééééééééééééé..., 1, null, null, null, null).|s=vs|t=t|c=a",
                    ],
                ),
                (
                    "INSERT INTO vs.t VALUES (1, 'x', 1, NULL, NULL, NULL, NULL)",
                    &[
                        "E|C=23514|M=new row for relation \"t\" violates check constraint \"t_b_check\"|D=Failing row contains (1, x, 1, null, null, null, null).|s=vs|t=t|n=t_b_check",
                    ],
                ),
                (
                    "INSERT INTO vs.t VALUES (1, 'y', 1, NULL, NULL, NULL, NULL)",
                    &[],
                ),
                (
                    "INSERT INTO vs.t VALUES (1, 'z', 1, NULL, NULL, NULL, NULL)",
                    &[
                        "E|C=23505|M=duplicate key value violates unique constraint \"tc\"|D=Key (c, a)=(1, 1) already exists.|s=vs|t=t|n=tc",
                    ],
                ),
                (
                    "INSERT INTO vs.t VALUES (2, 'Y', 2, NULL, NULL, NULL, NULL)",
                    &[
                        "E|C=23505|M=duplicate key value violates unique constraint \"ui\"|D=Key (lower(b))=(y) already exists.|s=vs|t=t|n=ui",
                    ],
                ),
                (
                    "INSERT INTO vs.c VALUES (5, 1)",
                    &[
                        "E|C=23503|M=insert or update on table \"c\" violates foreign key constraint \"c_pid_fkey\"|D=Key (pid)=(5) is not present in table \"p\".|s=vs|t=c|n=c_pid_fkey",
                    ],
                ),
                (
                    "DELETE FROM vs.p",
                    &[
                        "E|C=23503|M=update or delete on table \"p\" violates foreign key constraint \"c_pid_fkey\" on table \"c\"|D=Key (id)=(1) is still referenced from table \"c\".|s=vs|t=c|n=c_pid_fkey",
                    ],
                ),
                (
                    "SELECT (-1)::vs.pos",
                    &[
                        "E|C=23514|M=value for domain vs.pos violates check constraint \"pos_ok\"|s=vs|d=pos|n=pos_ok",
                    ],
                ),
                (
                    "SELECT NULL::vs.pos",
                    &["E|C=23502|M=domain vs.pos does not allow null values|s=vs|d=pos"],
                ),
                (
                    "INSERT INTO vs.ex VALUES ('[1,5)'), ('[3,7)')",
                    &[
                        "E|C=23P01|M=conflicting key value violates exclusion constraint \"ex_r_excl\"|D=Key (r)=([3,7)) conflicts with existing key (r)=([1,5)).|s=vs|t=ex|n=ex_r_excl",
                    ],
                ),
            ],
        );
    });
}

#[test]
fn raise_options_and_stacked_diagnostics_carry_object_fields() {
    with_engine("raise-object-fields", |engine, budget| {
        run_with(
            engine,
            budget,
            "CREATE TABLE rt (a int NOT NULL); \
             CREATE FUNCTION rf() RETURNS text LANGUAGE plpgsql AS $$ \
             DECLARE s text; t text; c text; d text; n text; m text; \
             BEGIN \
               BEGIN INSERT INTO rt VALUES (NULL); \
               EXCEPTION WHEN OTHERS THEN \
                 GET STACKED DIAGNOSTICS s = SCHEMA_NAME, t = TABLE_NAME, c = COLUMN_NAME, \
                   d = PG_DATATYPE_NAME, n = CONSTRAINT_NAME, m = PG_EXCEPTION_DETAIL; \
                 RETURN concat_ws('|', s, t, c, d, n, m); \
               END; \
             END $$;",
        );
        let output = run_with(engine, budget, "SELECT rf()");
        assert_eq!(
            data_rows(&output),
            ["public|rt|a|||Failing row contains (null)."]
        );
        let output = run_with(
            engine,
            budget,
            "DO $$ BEGIN RAISE NOTICE 'x' USING COLUMN = 'cc', DETAIL = 'dd'; \
             RAISE EXCEPTION USING MESSAGE = 'm', COLUMN = 'c', CONSTRAINT = 'n', \
               DATATYPE = 'd', TABLE = 't', SCHEMA = 's'; END $$",
        );
        let reports = reports(&output);
        assert_eq!(
            reports[0],
            "N|C=00000|M=x|D=dd|W=PL/pgSQL function inline_code_block line 1 at RAISE|c=cc"
        );
        assert_eq!(
            reports[1],
            "E|C=P0001|M=m|W=PL/pgSQL function inline_code_block line 1 at RAISE|s=s|t=t|c=c|d=d|n=n"
        );
    });
}

/// The PL/pgSQL and SQL function definitions the context cases call.
const CONTEXT_ROUTINES: &str = r#"CREATE TABLE t (a int)
---
INSERT INTO t VALUES (0)
---
CREATE FUNCTION f(a integer) RETURNS int LANGUAGE plpgsql AS $$
DECLARE x int;
BEGIN
  IF a = 1 THEN
    RAISE EXCEPTION 'boom %', a;
  ELSIF a = 2 THEN
    x := 1/0;
  ELSIF a = 3 THEN
    PERFORM 1/0;
  ELSIF a = 4 THEN
    EXECUTE 'SELEC 1';
  ELSIF a = 5 THEN
    EXECUTE 'SELECT 1/0';
  ELSIF a = 6 THEN
    SELECT 1/0 INTO x;
  ELSIF a = 7 THEN
    RETURN 'abc';
  ELSIF a = 8 THEN
    RAISE NOTICE 'note';
  ELSIF a = 9 THEN
    WHILE 1/(a-9) = 1 LOOP END LOOP;
  ELSIF a = 10 THEN
    FOR i IN 1..1/(a-10) LOOP END LOOP;
  ELSIF a = 11 THEN
    CASE 1/(a-11) WHEN 1 THEN NULL; END CASE;
  ELSIF a = 12 THEN
    ASSERT a = 0, 'assert msg';
  ELSIF a = 13 THEN
    RETURN 1/(a-13);
  ELSIF a = 14 THEN
    FOR x IN EXECUTE 'SELECT 1/0' LOOP END LOOP;
  ELSIF a = 15 THEN
    UPDATE t SET a = 1/(a-15);
  ELSIF a = 16 THEN
    LOOP EXIT WHEN 1/(a-16) = 1; END LOOP;
  ELSIF a = 17 THEN
    x := nocol;
  ELSIF a = 18 THEN
    EXECUTE 'SELECT nocol FROM t';
  ELSIF a = 19 THEN
    SELECT nocol INTO x FROM t;
  ELSIF a = 20 THEN
    INSERT INTO nosuch VALUES (1);
  ELSIF a = 21 THEN
    PERFORM nofunc(a);
  ELSIF a = 22 THEN
    RAISE NOTICE '%', 1/(a-22);
  ELSIF a = 23 THEN
    x := 'abc';
  ELSIF a = 24 THEN
    IF 'xyz' THEN NULL; END IF;
  END IF;
  RETURN 1;
END $$
---
CREATE FUNCTION g(a int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  RETURN f(a);
END $$
---
CREATE FUNCTION g2(a int) RETURNS int LANGUAGE plpgsql AS $$
DECLARE r int;
BEGIN
  SELECT f(a) INTO r;
  RETURN r;
END $$
---
CREATE FUNCTION s(a int) RETURNS int LANGUAGE sql AS $$ SELECT 1; SELECT f(a) $$
---
CREATE FUNCTION s1(a int) RETURNS int LANGUAGE sql AS $$ SELECT f(a) $$
---
CREATE FUNCTION s3(a int) RETURNS int LANGUAGE sql AS $$ SELECT 1/a $$
---
CREATE FUNCTION s4(a int) RETURNS int LANGUAGE sql AS $$ SELECT 1/a FROM t $$
---
CREATE FUNCTION nr() RETURNS int LANGUAGE plpgsql AS $$ BEGIN END $$
---
CREATE FUNCTION di(a int) RETURNS int LANGUAGE plpgsql AS $$
DECLARE
  x int := 1/a;
BEGIN
  RETURN x;
END $$
---
CREATE PROCEDURE pr(a int) LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'p%', a; END $$
---
CREATE SCHEMA other
---
CREATE FUNCTION other.of(a int, b text) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1/a; END $$
---
CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'trig'; END $$
---
CREATE TABLE tt (a int)
---
CREATE TRIGGER tt_t BEFORE INSERT ON tt FOR EACH ROW EXECUTE FUNCTION trg()"#;

#[test]
fn procedural_errors_report_postgresql_context_and_internal_query() {
    with_engine("procedural-error-context", |engine, budget| {
        for statement in CONTEXT_ROUTINES.split("\n---\n") {
            let output = run_with(engine, budget, statement);
            assert_eq!(reports(&output), Vec::<String>::new(), "{statement}");
        }
        assert_reports(
            engine,
            budget,
            &[
                ("INSERT INTO t VALUES (0)", &[]),
                (
                    "SELECT f(1)",
                    &["E|C=P0001|M=boom 1|W=PL/pgSQL function f(integer) line 5 at RAISE"],
                ),
                (
                    "SELECT f(2)",
                    &[
                        "E|C=22012|M=division by zero|W=PL/pgSQL assignment \"x := 1/0\"\nPL/pgSQL function f(integer) line 7 at assignment",
                    ],
                ),
                (
                    "SELECT f(3)",
                    &[
                        "E|C=22012|M=division by zero|W=SQL statement \"SELECT 1/0\"\nPL/pgSQL function f(integer) line 9 at PERFORM",
                    ],
                ),
                (
                    "SELECT f(4)",
                    &[
                        "E|C=42601|M=syntax error at or near \"SELEC\"|W=PL/pgSQL function f(integer) line 11 at EXECUTE|p=1|q=SELEC 1",
                    ],
                ),
                (
                    "SELECT f(5)",
                    &[
                        "E|C=22012|M=division by zero|W=SQL statement \"SELECT 1/0\"\nPL/pgSQL function f(integer) line 13 at EXECUTE",
                    ],
                ),
                (
                    "SELECT f(6)",
                    &[
                        "E|C=22012|M=division by zero|W=SQL statement \"SELECT 1/0\"\nPL/pgSQL function f(integer) line 15 at SQL statement",
                    ],
                ),
                (
                    "SELECT f(7)",
                    &[
                        "E|C=22P02|M=invalid input syntax for type integer: \"abc\"|W=PL/pgSQL function f(integer) while casting return value to function's return type",
                    ],
                ),
                (
                    "SELECT f(8)",
                    &["N|C=00000|M=note|W=PL/pgSQL function f(integer) line 19 at RAISE"],
                ),
                (
                    "SELECT f(9)",
                    &[
                        "E|C=22012|M=division by zero|W=PL/pgSQL function f(integer) line 21 at WHILE",
                    ],
                ),
                (
                    "SELECT f(10)",
                    &[
                        "E|C=22012|M=division by zero|W=PL/pgSQL function f(integer) line 23 at FOR with integer loop variable",
                    ],
                ),
                (
                    "SELECT f(11)",
                    &[
                        "E|C=22012|M=division by zero|W=PL/pgSQL function f(integer) line 25 at CASE",
                    ],
                ),
                (
                    "SELECT f(12)",
                    &["E|C=P0004|M=assert msg|W=PL/pgSQL function f(integer) line 27 at ASSERT"],
                ),
                (
                    "SELECT f(13)",
                    &[
                        "E|C=22012|M=division by zero|W=PL/pgSQL function f(integer) line 29 at RETURN",
                    ],
                ),
                (
                    "SELECT f(14)",
                    &[
                        "E|C=22012|M=division by zero|W=SQL statement \"SELECT 1/0\"\nPL/pgSQL function f(integer) line 31 at FOR over EXECUTE statement",
                    ],
                ),
                (
                    "SELECT f(16)",
                    &[
                        "E|C=22012|M=division by zero|W=PL/pgSQL function f(integer) line 35 at EXIT",
                    ],
                ),
                (
                    "SELECT f(17)",
                    &[
                        "E|C=42703|M=column \"nocol\" does not exist|W=PL/pgSQL function f(integer) line 37 at assignment|p=6|q=x := nocol",
                    ],
                ),
                (
                    "SELECT f(18)",
                    &[
                        "E|C=42703|M=column \"nocol\" does not exist|W=PL/pgSQL function f(integer) line 39 at EXECUTE|p=8|q=SELECT nocol FROM t",
                    ],
                ),
                (
                    "SELECT f(19)",
                    &[
                        "E|C=42703|M=column \"nocol\" does not exist|W=PL/pgSQL function f(integer) line 41 at SQL statement|p=8|q=SELECT nocol        FROM t",
                    ],
                ),
                (
                    "SELECT f(20)",
                    &[
                        "E|C=42P01|M=relation \"nosuch\" does not exist|W=PL/pgSQL function f(integer) line 43 at SQL statement|p=13|q=INSERT INTO nosuch VALUES (1)",
                    ],
                ),
                (
                    "SELECT f(21)",
                    &[
                        "E|C=42883|M=function nofunc(integer) does not exist|H=No function matches the given name and argument types. You might need to add explicit type casts.|W=PL/pgSQL function f(integer) line 45 at PERFORM|p=8|q=SELECT nofunc(a)",
                    ],
                ),
                (
                    "SELECT f(22)",
                    &[
                        "E|C=22012|M=division by zero|W=PL/pgSQL function f(integer) line 47 at RAISE",
                    ],
                ),
                (
                    "SELECT f(23)",
                    &[
                        "E|C=22P02|M=invalid input syntax for type integer: \"abc\"|W=PL/pgSQL function f(integer) line 49 at assignment|p=6|q=x := 'abc'",
                    ],
                ),
                (
                    "SELECT f(24)",
                    &[
                        "E|C=22P02|M=invalid input syntax for type boolean: \"xyz\"|W=PL/pgSQL function f(integer) line 51 at IF",
                    ],
                ),
                (
                    "SELECT g(1)",
                    &[
                        "E|C=P0001|M=boom 1|W=PL/pgSQL function f(integer) line 5 at RAISE\nPL/pgSQL function g(integer) line 3 at RETURN",
                    ],
                ),
                (
                    "SELECT g2(5)",
                    &[
                        "E|C=22012|M=division by zero|W=SQL statement \"SELECT 1/0\"\nPL/pgSQL function f(integer) line 13 at EXECUTE\nSQL statement \"SELECT f(a)\"\nPL/pgSQL function g2(integer) line 4 at SQL statement",
                    ],
                ),
                (
                    "SELECT g(4)",
                    &[
                        "E|C=42601|M=syntax error at or near \"SELEC\"|W=PL/pgSQL function f(integer) line 11 at EXECUTE\nPL/pgSQL function g(integer) line 3 at RETURN|p=1|q=SELEC 1",
                    ],
                ),
                (
                    "SELECT s(2)",
                    &[
                        "E|C=22012|M=division by zero|W=PL/pgSQL assignment \"x := 1/0\"\nPL/pgSQL function f(integer) line 7 at assignment\nSQL function \"s\" statement 2",
                    ],
                ),
                (
                    "SELECT s1(2)",
                    &[
                        "E|C=22012|M=division by zero|W=PL/pgSQL assignment \"x := 1/0\"\nPL/pgSQL function f(integer) line 7 at assignment",
                    ],
                ),
                (
                    "SELECT s4(0)",
                    &["E|C=22012|M=division by zero|W=SQL function \"s4\" statement 1"],
                ),
                (
                    "SELECT nr()",
                    &[
                        "E|C=2F005|M=control reached end of function without RETURN|W=PL/pgSQL function nr()",
                    ],
                ),
                (
                    "SELECT di(0)",
                    &[
                        "E|C=22012|M=division by zero|W=PL/pgSQL function di(integer) line 3 during statement block local variable initialization",
                    ],
                ),
                (
                    "CALL pr(3)",
                    &["E|C=P0001|M=p3|W=PL/pgSQL function pr(integer) line 1 at RAISE"],
                ),
                (
                    "SELECT other.of(0, 'x')",
                    &[
                        "E|C=22012|M=division by zero|W=PL/pgSQL function other.of(integer,text) line 1 at RETURN",
                    ],
                ),
                (
                    "INSERT INTO tt VALUES (1)",
                    &["E|C=P0001|M=trig|W=PL/pgSQL function trg() line 1 at RAISE"],
                ),
                (
                    "DO $$ BEGIN RAISE EXCEPTION 'x'; END $$",
                    &["E|C=P0001|M=x|W=PL/pgSQL function inline_code_block line 1 at RAISE"],
                ),
                (
                    "DO $$\nDECLARE c text;\nBEGIN\n  GET DIAGNOSTICS c = PG_CONTEXT;\n  RAISE NOTICE '%', c;\nEND $$",
                    &[
                        "N|C=00000|M=PL/pgSQL function inline_code_block line 4 at GET DIAGNOSTICS|W=PL/pgSQL function inline_code_block line 5 at RAISE",
                    ],
                ),
                (
                    "DO $$\nDECLARE c text;\nBEGIN\n  PERFORM f(1);\nEXCEPTION WHEN OTHERS THEN\n  GET STACKED DIAGNOSTICS c = PG_EXCEPTION_CONTEXT;\n  RAISE NOTICE '[%]', c;\nEND $$",
                    &[
                        "N|C=00000|M=[PL/pgSQL function f(integer) line 5 at RAISE\nSQL statement \"SELECT f(1)\"\nPL/pgSQL function inline_code_block line 4 at PERFORM]|W=PL/pgSQL function inline_code_block line 7 at RAISE",
                    ],
                ),
                (
                    "SELECT f(a+3) FROM t",
                    &[
                        "E|C=22012|M=division by zero|W=SQL statement \"SELECT 1/0\"\nPL/pgSQL function f(integer) line 9 at PERFORM",
                    ],
                ),
            ],
        );
    });
}

/// Each response message as the client sees it: `SEVERITY SQLSTATE message`
/// with ` @position`, ` hint: ...` and ` detail: ...` when present, each
/// command tag, and each data row as `data|value|...`.
fn transcript(bytes: &[u8]) -> Vec<String> {
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
                for (code, label) in [(b'P', " @"), (b'H', " hint: "), (b'D', " detail: ")] {
                    if let Some(value) = fields.get(&code) {
                        line.push_str(label);
                        line.push_str(value);
                    }
                }
                out.push(line);
            }
            b'C' => out.push(String::from_utf8_lossy(&payload[..payload.len() - 1]).into_owned()),
            b'D' => {
                let columns = u16::from_be_bytes([payload[0], payload[1]]) as usize;
                let mut at = 2;
                let mut values = vec![String::from("data")];
                for _ in 0..columns {
                    let length = i32::from_be_bytes(payload[at..at + 4].try_into().unwrap());
                    at += 4;
                    if length < 0 {
                        values.push(String::from("NULL"));
                    } else {
                        let length = length as usize;
                        values
                            .push(String::from_utf8_lossy(&payload[at..at + length]).into_owned());
                        at += length;
                    }
                }
                out.push(values.join("|"));
            }
            _ => {}
        }
        offset += 1 + length;
    }
    out
}

/// Analysis PostgreSQL makes before running a statement: its order across
/// the FROM clause, types and output columns; WITH queries nothing reads;
/// constants converted to the type they are compared with; SQL function
/// bodies at `CREATE FUNCTION`; subscripted `INSERT` targets; and PL/pgSQL
/// conditions converted to boolean.
#[test]
fn statements_are_analysed_as_postgresql_analyses_them() {
    with_engine("statement-analysis-order", |engine, budget| {
        let cases: &[(&str, &[&str])] = &[
            ("CREATE TABLE e (a int)", &["CREATE TABLE"]),
            (
                "SELECT 1::numeric(5000) FROM missing",
                &["ERROR 42P01 relation \"missing\" does not exist @30"],
            ),
            (
                "SELECT 1::nosuchtype(5)",
                &["ERROR 42704 type \"nosuchtype\" does not exist @11"],
            ),
            (
                "SELECT 1::numeric(5000), nocol FROM e",
                &["ERROR 22023 NUMERIC precision 5000 must be between 1 and 1000 @11"],
            ),
            (
                "SELECT nocol, 1::numeric(5000) FROM e",
                &["ERROR 42703 column \"nocol\" does not exist @8"],
            ),
            (
                "SELECT 1::nosuch(1) FROM missing",
                &["ERROR 42P01 relation \"missing\" does not exist @26"],
            ),
            (
                "SELECT x::nosuch(1) FROM e",
                &["ERROR 42704 type \"nosuch\" does not exist @11"],
            ),
            (
                "WITH x AS (SELECT * FROM missing) SELECT 1",
                &["ERROR 42P01 relation \"missing\" does not exist @26"],
            ),
            (
                "WITH w AS (SELECT nocol FROM e) SELECT 1",
                &["ERROR 42703 column \"nocol\" does not exist @19"],
            ),
            ("WITH w AS (SELECT 1/0) SELECT 1", &["data|1", "SELECT 1"]),
            (
                "SELECT a FROM e WHERE a = 'abc'",
                &["ERROR 22P02 invalid input syntax for type integer: \"abc\" @27"],
            ),
            (
                "SELECT a FROM e WHERE 'abc' = a",
                &["ERROR 22P02 invalid input syntax for type integer: \"abc\" @23"],
            ),
            (
                "SELECT a FROM e WHERE a < 'abc'",
                &["ERROR 22P02 invalid input syntax for type integer: \"abc\" @27"],
            ),
            (
                "SELECT a FROM e WHERE a IN ('x')",
                &["ERROR 22P02 invalid input syntax for type integer: \"x\" @29"],
            ),
            (
                "SELECT a + 'x' FROM e",
                &["ERROR 22P02 invalid input syntax for type integer: \"x\" @12"],
            ),
            ("SELECT a FROM e WHERE a = '1'", &["SELECT 0"]),
            (
                "CREATE FUNCTION sf() RETURNS int LANGUAGE sql AS 'SELECT a FROM missing'",
                &["ERROR 42P01 relation \"missing\" does not exist @65"],
            ),
            (
                "CREATE FUNCTION sf2() RETURNS int LANGUAGE sql AS 'SELECT nocol FROM e'",
                &["ERROR 42703 column \"nocol\" does not exist @59"],
            ),
            (
                "CREATE FUNCTION sf4(x int) RETURNS int LANGUAGE sql AS $$ INSERT INTO nosuch VALUES (x); SELECT 1 $$",
                &["ERROR 42P01 relation \"nosuch\" does not exist @71"],
            ),
            (
                "CREATE FUNCTION sf5(x anyelement) RETURNS int LANGUAGE sql AS $$ SELECT 1 FROM missing $$",
                &["CREATE FUNCTION"],
            ),
            (
                "CREATE FUNCTION sf6() RETURNS int LANGUAGE sql AS $$ SELECT nofunc(1) $$",
                &[
                    "ERROR 42883 function nofunc(integer) does not exist @61 hint: No function matches the given name and argument types. You might need to add explicit type casts.",
                ],
            ),
            (
                "CREATE FUNCTION sf10() RETURNS int LANGUAGE sql AS $$\n  SELECT 1;\n  SELECT\n    nocol FROM e $$",
                &["ERROR 42703 column \"nocol\" does not exist @80"],
            ),
            (
                "CREATE FUNCTION sf11() RETURNS int LANGUAGE sql AS 'SELECT ''x'' FROM missing'",
                &["ERROR 42P01 relation \"missing\" does not exist @71"],
            ),
            (
                "CREATE FUNCTION sf12() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT a FROM missing; END",
                &["ERROR 42P01 relation \"missing\" does not exist @76"],
            ),
            (
                "SET check_function_bodies = off; \
                 CREATE FUNCTION sf8() RETURNS int LANGUAGE sql AS 'SELECT a FROM missing'",
                &["SET", "CREATE FUNCTION"],
            ),
            (
                "CREATE TABLE ar (a int[], j jsonb, m int[], t text)",
                &["CREATE TABLE"],
            ),
            ("INSERT INTO ar (a[1]) SELECT 5", &["INSERT 0 1"]),
            ("INSERT INTO ar (j['x']) VALUES ('1')", &["INSERT 0 1"]),
            (
                "INSERT INTO ar (j['x'], a[2]) SELECT '{\"y\":2}', 7",
                &["INSERT 0 1"],
            ),
            ("INSERT INTO ar (j[0]) VALUES ('\"z\"')", &["INSERT 0 1"]),
            (
                "INSERT INTO ar (j['a']['b']) VALUES ('true')",
                &["INSERT 0 1"],
            ),
            (
                "SELECT a, j FROM ar ORDER BY a, j::text",
                &[
                    "data|{5}|NULL",
                    "data|[2:2]={7}|{\"x\": {\"y\": 2}}",
                    "data|NULL|[\"z\"]",
                    "data|NULL|{\"a\": {\"b\": true}}",
                    "data|NULL|{\"x\": 1}",
                    "SELECT 5",
                ],
            ),
            (
                "DO $$ BEGIN IF 1 THEN RAISE NOTICE 'one'; END IF; IF '1'::int2 THEN RAISE NOTICE 'int2'; END IF; END $$",
                &["NOTICE 00000 one", "NOTICE 00000 int2", "DO"],
            ),
            (
                "DO $$ BEGIN IF 5 THEN NULL; END IF; END $$",
                &["ERROR 22P02 invalid input syntax for type boolean: \"5\""],
            ),
            (
                "DO $$ BEGIN WHILE 'x' LOOP END LOOP; END $$",
                &["ERROR 22P02 invalid input syntax for type boolean: \"x\""],
            ),
            (
                "DO $$ BEGIN RAISE NOTICE '[%]', 'a'; END $$\n",
                &["NOTICE 00000 [a]", "DO"],
            ),
            // Errors about a whole expression point at its start.
            (
                "SELECT 1 WHERE 1",
                &["ERROR 42804 argument of WHERE must be type boolean, not type integer @16"],
            ),
            (
                "SELECT 1 WHERE 'a'::text",
                &["ERROR 42804 argument of WHERE must be type boolean, not type text @16"],
            ),
            (
                "SELECT CASE WHEN 1 THEN 1 END",
                &["ERROR 42804 argument of CASE/WHEN must be type boolean, not type integer @18"],
            ),
            (
                "SELECT 1 AND true",
                &["ERROR 42804 argument of AND must be type boolean, not type integer @8"],
            ),
            (
                "SELECT true AND 2",
                &["ERROR 42804 argument of AND must be type boolean, not type integer @17"],
            ),
            (
                "SELECT NOT 1",
                &["ERROR 42804 argument of NOT must be type boolean, not type integer @12"],
            ),
            (
                "SELECT 1 WHERE 1.5",
                &["ERROR 42804 argument of WHERE must be type boolean, not type numeric @16"],
            ),
            ("CREATE TABLE pos_e (a int, b text)", &["CREATE TABLE"]),
            (
                "SELECT a FROM pos_e WHERE b",
                &["ERROR 42804 argument of WHERE must be type boolean, not type text @27"],
            ),
            (
                "SELECT CASE WHEN a THEN 1 END FROM pos_e",
                &["ERROR 42804 argument of CASE/WHEN must be type boolean, not type integer @18"],
            ),
            (
                "SELECT a FROM pos_e WHERE a + 1",
                &["ERROR 42804 argument of WHERE must be type boolean, not type integer @27"],
            ),
            (
                "SELECT 1 WHERE NULL::int",
                &["ERROR 42804 argument of WHERE must be type boolean, not type integer @16"],
            ),
            (
                "UPDATE pos_e SET a = 1 WHERE a = 'abc'",
                &["ERROR 22P02 invalid input syntax for type integer: \"abc\" @34"],
            ),
            (
                "DELETE FROM pos_e WHERE a = 'abc'",
                &["ERROR 22P02 invalid input syntax for type integer: \"abc\" @29"],
            ),
            (
                "SELECT * FROM pos_e x JOIN pos_e y ON x.a = 'abc'\n",
                &["ERROR 22P02 invalid input syntax for type integer: \"abc\" @45"],
            ),
        ];
        for (sql, expected) in cases {
            let output = run_with(engine, budget, sql);
            assert_eq!(transcript(&output), *expected, "{sql}");
        }
    });
}
