//! `DROP` dependency reports, compared with PostgreSQL 18.6 (`psql -c` output
//! of the same statements in a fresh database).

use super::*;

/// psql's rendering of every error and notice in a response.
fn diagnostics(bytes: &[u8]) -> String {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let tag = bytes[offset];
        let length = u32::from_be_bytes(bytes[offset + 1..offset + 5].try_into().unwrap()) as usize;
        if tag == b'E' || tag == b'N' {
            let mut fields = &bytes[offset + 5..offset + 1 + length];
            let (mut severity, mut message, mut detail, mut hint) = ("", "", None, None);
            while let Some((&field, remaining)) = fields.split_first() {
                if field == 0 {
                    break;
                }
                let end = remaining.iter().position(|&byte| byte == 0).unwrap();
                let value = core::str::from_utf8(&remaining[..end]).unwrap();
                match field {
                    b'S' => severity = value,
                    b'M' => message = value,
                    b'D' => detail = Some(value),
                    b'H' => hint = Some(value),
                    _ => {}
                }
                fields = &remaining[end + 1..];
            }
            let mut text = format!("{severity}:  {message}");
            if let Some(detail) = detail {
                text += &format!("\nDETAIL:  {detail}");
            }
            if let Some(hint) = hint {
                text += &format!("\nHINT:  {hint}");
            }
            out.push(text);
        }
        offset += 1 + length;
    }
    out.join("\n")
}

fn check_statements(statements: &[(&str, &str)]) {
    let (mut engine, mut budget) = test_engine();
    for (sql, expected) in statements {
        let output = run_with(&mut engine, &mut budget, sql);
        assert_eq!(diagnostics(&output), *expected, "{sql}");
    }
}

#[test]
fn domain_drops_report_like_postgresql() {
    check_statements(&[
        ("CREATE DOMAIN dm AS int", ""),
        ("CREATE TABLE t1 (a int, b dm, c dm)", ""),
        (
            "CREATE FUNCTION f1(dm) RETURNS int LANGUAGE sql AS 'SELECT 1'",
            "",
        ),
        ("CREATE TYPE ct AS (x dm, y int)", ""),
        ("CREATE TABLE t2 (z dm)", ""),
        ("CREATE VIEW v1 AS SELECT b FROM t1", ""),
        ("CREATE DOMAIN dm2 AS dm", ""),
        (
            "CREATE FUNCTION f2() RETURNS dm LANGUAGE sql AS 'SELECT 1::dm'",
            "",
        ),
        (
            "DROP DOMAIN dm",
            "ERROR:  cannot drop type dm because other objects depend on it\n\
             DETAIL:  column c of table t1 depends on type dm\n\
             column b of table t1 depends on type dm\n\
             view v1 depends on column b of table t1\n\
             function f1(dm) depends on type dm\n\
             column x of composite type ct depends on type dm\n\
             column z of table t2 depends on type dm\n\
             type dm2 depends on type dm\n\
             function f2() depends on type dm\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP DOMAIN dm CASCADE",
            "NOTICE:  drop cascades to 8 other objects\n\
             DETAIL:  drop cascades to column c of table t1\n\
             drop cascades to column b of table t1\n\
             drop cascades to view v1\n\
             drop cascades to function f1(dm)\n\
             drop cascades to column x of composite type ct\n\
             drop cascades to column z of table t2\n\
             drop cascades to type dm2\n\
             drop cascades to function f2()",
        ),
        (
            "SELECT count(*) FROM pg_type WHERE typname IN ('dm', 'dm2', 'ct')",
            "",
        ),
    ]);
}

#[test]
fn enum_drops_report_like_postgresql() {
    check_statements(&[
        ("CREATE TYPE en AS ENUM ('a')", ""),
        ("CREATE TABLE t (a en[], b en)", ""),
        ("CREATE TYPE ct AS (f en[])", ""),
        ("CREATE DOMAIN dd AS en[]", ""),
        (
            "CREATE FUNCTION fe() RETURNS en[] LANGUAGE sql AS $$SELECT '{a}'::en[]$$",
            "",
        ),
        (
            "DROP TYPE en",
            "ERROR:  cannot drop type en because other objects depend on it\n\
             DETAIL:  column a of table t depends on type en[]\n\
             column f of composite type ct depends on type en[]\n\
             type dd depends on type en[]\n\
             function fe() depends on type en[]\n\
             column b of table t depends on type en\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP TYPE en CASCADE",
            "NOTICE:  drop cascades to 5 other objects\n\
             DETAIL:  drop cascades to column a of table t\n\
             drop cascades to column f of composite type ct\n\
             drop cascades to type dd\n\
             drop cascades to function fe()\n\
             drop cascades to column b of table t",
        ),
    ]);
}

#[test]
fn table_drops_report_like_postgresql() {
    check_statements(&[
        ("CREATE TABLE t1 (a int PRIMARY KEY, b int)", ""),
        (
            "CREATE FUNCTION f1(t1) RETURNS int LANGUAGE sql AS 'SELECT 1'",
            "",
        ),
        ("CREATE TABLE t2 (z t1, w t1)", ""),
        ("CREATE VIEW v1 AS SELECT b FROM t1", ""),
        ("CREATE TABLE t3 (x int REFERENCES t1)", ""),
        ("CREATE VIEW v2 AS SELECT * FROM v1", ""),
        (
            "CREATE FUNCTION f2() RETURNS SETOF t1 LANGUAGE sql AS 'SELECT * FROM t1'",
            "",
        ),
        ("CREATE SEQUENCE s1 OWNED BY t1.b", ""),
        ("CREATE TABLE t4 (q int DEFAULT nextval('s1'))", ""),
        (
            "DROP TABLE t1",
            "ERROR:  cannot drop table t1 because other objects depend on it\n\
             DETAIL:  function f1(t1) depends on type t1\n\
             column w of table t2 depends on type t1\n\
             column z of table t2 depends on type t1\n\
             function f2() depends on type t1\n\
             view v1 depends on table t1\n\
             view v2 depends on view v1\n\
             constraint t3_x_fkey on table t3 depends on table t1\n\
             default value for column q of table t4 depends on sequence s1\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP TABLE t1 CASCADE",
            "NOTICE:  drop cascades to 8 other objects\n\
             DETAIL:  drop cascades to function f1(t1)\n\
             drop cascades to column w of table t2\n\
             drop cascades to column z of table t2\n\
             drop cascades to function f2()\n\
             drop cascades to view v1\n\
             drop cascades to view v2\n\
             drop cascades to constraint t3_x_fkey on table t3\n\
             drop cascades to default value for column q of table t4",
        ),
        ("CREATE TABLE a (x int)", ""),
        ("CREATE TABLE b (y a)", ""),
        ("CREATE VIEW va AS SELECT * FROM a", ""),
        ("CREATE TABLE c (x int)", ""),
        ("CREATE VIEW vc AS SELECT * FROM c", ""),
        (
            "CREATE FUNCTION fc(c) RETURNS int LANGUAGE sql AS 'SELECT 1'",
            "",
        ),
        (
            "DROP TABLE a, c",
            "ERROR:  cannot drop desired object(s) because other objects depend on them\n\
             DETAIL:  function fc(c) depends on type c\n\
             view vc depends on table c\n\
             column y of table b depends on type a\n\
             view va depends on table a\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP TABLE a, c CASCADE",
            "NOTICE:  drop cascades to 4 other objects\n\
             DETAIL:  drop cascades to function fc(c)\n\
             drop cascades to view vc\n\
             drop cascades to column y of table b\n\
             drop cascades to view va",
        ),
        ("CREATE TABLE ih (a int)", ""),
        ("CREATE TABLE ic () INHERITS (ih)", ""),
        (
            "DROP TABLE ih",
            "ERROR:  cannot drop table ih because other objects depend on it\n\
             DETAIL:  table ic depends on table ih\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        ("CREATE TABLE pt (a int) PARTITION BY RANGE (a)", ""),
        (
            "CREATE TABLE pc PARTITION OF pt FOR VALUES FROM (1) TO (2)",
            "",
        ),
        ("DROP TABLE pt", ""),
    ]);
}

#[test]
fn schema_drops_report_like_postgresql() {
    check_statements(&[
        ("CREATE SCHEMA s", ""),
        ("CREATE TABLE s.t (a int)", ""),
        ("CREATE VIEW s.v AS SELECT * FROM s.t", ""),
        (
            "CREATE FUNCTION s.f() RETURNS int LANGUAGE sql AS 'SELECT 1'",
            "",
        ),
        ("CREATE TABLE other (x s.t)", ""),
        ("CREATE VIEW ov AS SELECT * FROM s.v", ""),
        (
            "DROP SCHEMA s",
            "ERROR:  cannot drop schema s because other objects depend on it\n\
             DETAIL:  table s.t depends on schema s\n\
             column x of table other depends on type s.t\n\
             view s.v depends on schema s\n\
             view ov depends on view s.v\n\
             function s.f() depends on schema s\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP SCHEMA s CASCADE",
            "NOTICE:  drop cascades to 5 other objects\n\
             DETAIL:  drop cascades to table s.t\n\
             drop cascades to column x of table other\n\
             drop cascades to view s.v\n\
             drop cascades to view ov\n\
             drop cascades to function s.f()",
        ),
    ]);
}

#[test]
fn sequence_drops_report_like_postgresql() {
    check_statements(&[
        ("CREATE SEQUENCE sq", ""),
        ("CREATE TABLE ts (a int DEFAULT nextval('sq'))", ""),
        ("CREATE VIEW vs AS SELECT * FROM sq", ""),
        (
            "DROP SEQUENCE sq",
            "ERROR:  cannot drop sequence sq because other objects depend on it\n\
             DETAIL:  default value for column a of table ts depends on sequence sq\n\
             view vs depends on sequence sq\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP SEQUENCE sq CASCADE",
            "NOTICE:  drop cascades to 2 other objects\n\
             DETAIL:  drop cascades to default value for column a of table ts\n\
             drop cascades to view vs",
        ),
        (
            "CREATE TABLE t (a int GENERATED ALWAYS AS IDENTITY, b serial)",
            "",
        ),
        (
            "DROP SEQUENCE t_a_seq",
            "ERROR:  cannot drop sequence t_a_seq because column a of table t requires it\n\
             HINT:  You can drop column a of table t instead.",
        ),
        (
            "DROP SEQUENCE t_b_seq",
            "ERROR:  cannot drop sequence t_b_seq because other objects depend on it\n\
             DETAIL:  default value for column b of table t depends on sequence t_b_seq\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
    ]);
}

#[test]
fn function_drops_report_like_postgresql() {
    check_statements(&[
        (
            "CREATE FUNCTION g() RETURNS int LANGUAGE sql AS 'SELECT 1'",
            "",
        ),
        ("CREATE VIEW vg AS SELECT g()", ""),
        ("CREATE TABLE tg (a int DEFAULT g())", ""),
        (
            "CREATE FUNCTION g2() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT g(); END",
            "",
        ),
        (
            "DROP FUNCTION g",
            "ERROR:  cannot drop function g() because other objects depend on it\n\
             DETAIL:  view vg depends on function g()\n\
             default value for column a of table tg depends on function g()\n\
             function g2() depends on function g()\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP FUNCTION g CASCADE",
            "NOTICE:  drop cascades to 3 other objects\n\
             DETAIL:  drop cascades to view vg\n\
             drop cascades to default value for column a of table tg\n\
             drop cascades to function g2()",
        ),
        (
            "CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN new; END'",
            "",
        ),
        ("CREATE TABLE tt (a int)", ""),
        (
            "CREATE TRIGGER tr BEFORE INSERT ON tt FOR EACH ROW EXECUTE FUNCTION tf()",
            "",
        ),
        (
            "DROP FUNCTION tf()",
            "ERROR:  cannot drop function tf() because other objects depend on it\n\
             DETAIL:  trigger tr on table tt depends on function tf()\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP FUNCTION tf() CASCADE",
            "NOTICE:  drop cascades to trigger tr on table tt",
        ),
    ]);
}

#[test]
fn view_drops_report_like_postgresql() {
    check_statements(&[
        ("CREATE TABLE vt (a int)", ""),
        ("CREATE VIEW vv AS SELECT * FROM vt", ""),
        ("CREATE VIEW vv2 AS SELECT * FROM vv", ""),
        ("CREATE VIEW vv3 AS SELECT * FROM vv2", ""),
        (
            "DROP VIEW vv",
            "ERROR:  cannot drop view vv because other objects depend on it\n\
             DETAIL:  view vv2 depends on view vv\n\
             view vv3 depends on view vv2\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP VIEW vv CASCADE",
            "NOTICE:  drop cascades to 2 other objects\n\
             DETAIL:  drop cascades to view vv2\n\
             drop cascades to view vv3",
        ),
        ("CREATE TABLE r (a int)", ""),
        ("CREATE VIEW rv AS SELECT * FROM r", ""),
        (
            "CREATE RULE rr AS ON INSERT TO rv DO INSTEAD INSERT INTO r VALUES (new.a)",
            "",
        ),
        ("CREATE TABLE r2 (a int)", ""),
        (
            "CREATE RULE rr2 AS ON INSERT TO r2 DO ALSO INSERT INTO r VALUES (new.a)",
            "",
        ),
        (
            "DROP TABLE r",
            "ERROR:  cannot drop table r because other objects depend on it\n\
             DETAIL:  view rv depends on table r\n\
             rule rr2 on table r2 depends on table r\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP TABLE r CASCADE",
            "NOTICE:  drop cascades to 2 other objects\n\
             DETAIL:  drop cascades to view rv\n\
             drop cascades to rule rr2 on table r2",
        ),
        ("CREATE TABLE m (a int)", ""),
        ("CREATE MATERIALIZED VIEW mv AS SELECT * FROM m", ""),
        ("CREATE INDEX ON m (a)", ""),
        (
            "DROP TABLE m",
            "ERROR:  cannot drop table m because other objects depend on it\n\
             DETAIL:  materialized view mv depends on table m\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP TABLE m CASCADE",
            "NOTICE:  drop cascades to materialized view mv",
        ),
        ("CREATE TABLE tt2 (a int)", ""),
        (
            "DROP TYPE tt2",
            "ERROR:  cannot drop type tt2 because table tt2 requires it\n\
             HINT:  You can drop table tt2 instead.",
        ),
        ("CREATE TYPE ctt AS (x int)", ""),
        (
            "DROP TABLE ctt",
            "ERROR:  \"ctt\" is not a table\n\
             HINT:  Use DROP TYPE to remove a type.",
        ),
    ]);
}

/// `pg_depend` records PostgreSQL's rows for row types, array types, columns
/// and fields of user-defined types, defaults, inheritance, partitions, owned
/// sequences, schemas, routine signatures and `BEGIN ATOMIC` bodies.
#[test]
fn recorded_dependencies_include_postgresql_rows() {
    let (mut engine, mut budget) = test_engine();
    let setup = run_with(
        &mut engine,
        &mut budget,
        "CREATE DOMAIN dm AS int; CREATE TYPE en AS ENUM ('a'); \
         CREATE TYPE ct AS (x dm, y en[]); CREATE SEQUENCE sq; \
         CREATE FUNCTION g(en, ct) RETURNS dm LANGUAGE sql AS 'SELECT 1'; \
         CREATE TABLE t (a dm DEFAULT nextval('sq'), b en[], c int DEFAULT g('a', NULL)); \
         CREATE TABLE child () INHERITS (t); \
         CREATE FUNCTION h() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT g('a', NULL); END; \
         CREATE TABLE p (k int) PARTITION BY RANGE (k); \
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (1) TO (2); \
         CREATE TABLE s (id serial); \
         CREATE TABLE r (a int); CREATE TABLE r2 (a int); \
         CREATE RULE rr2 AS ON INSERT TO r2 DO ALSO INSERT INTO r VALUES (new.a)",
    );
    assert!(
        error_sqlstates(&setup).is_empty(),
        "{}",
        diagnostics(&setup)
    );
    let rows = data_rows(&run_with(
        &mut engine,
        &mut budget,
        "SELECT pg_describe_object(classid, objid, objsubid), \
         pg_describe_object(refclassid, refobjid, refobjsubid), deptype \
         FROM pg_depend WHERE objid >= 16384 ORDER BY 1, 2, 3",
    ));
    for expected in [
        "column a of table child|type dm|n",
        "column a of table t|type dm|n",
        "column b of table child|type en[]|n",
        "column b of table t|type en[]|n",
        "column x of composite type ct|type dm|n",
        "column y of composite type ct|type en[]|n",
        "composite type ct|type ct|i",
        "default value for column a of table t|column a of table t|a",
        "default value for column a of table t|sequence sq|n",
        "default value for column c of table t|column c of table t|a",
        "default value for column c of table t|function g(en,ct)|n",
        "default value for column id of table s|column id of table s|a",
        "default value for column id of table s|sequence s_id_seq|n",
        "function g(en,ct)|schema public|n",
        "function g(en,ct)|type ct|n",
        "function g(en,ct)|type dm|n",
        "function g(en,ct)|type en|n",
        "function h()|function g(en,ct)|n",
        "function h()|schema public|n",
        "rule rr2 on table r2|table r2|a",
        "sequence s_id_seq|column id of table s|a",
        "sequence s_id_seq|schema public|n",
        "sequence sq|schema public|n",
        "table child|schema public|n",
        "table child|table t|n",
        "table p1|table p|a",
        "table t|schema public|n",
        "type ct|schema public|n",
        "type ct[]|type ct|i",
        "type dm|schema public|n",
        "type dm[]|type dm|i",
        "type en[]|type en|i",
        "type t|table t|i",
        "type t[]|type t|i",
    ] {
        assert!(
            rows.iter().any(|row| row == expected),
            "{expected}: {rows:#?}"
        );
    }
}
#[test]
fn wrong_kind_drops_report_like_postgresql() {
    check_statements(&[
        ("CREATE TABLE t (a int)", ""),
        ("CREATE SEQUENCE sq", ""),
        ("CREATE VIEW v AS SELECT 1 AS x", ""),
        ("CREATE MATERIALIZED VIEW mv AS SELECT 1 AS x", ""),
        ("CREATE INDEX ti ON t (a)", ""),
        ("CREATE TYPE ct AS (x int)", ""),
        (
            "DROP TABLE sq",
            "ERROR:  \"sq\" is not a table\n\
             HINT:  Use DROP SEQUENCE to remove a sequence.",
        ),
        (
            "DROP TABLE v",
            "ERROR:  \"v\" is not a table\n\
             HINT:  Use DROP VIEW to remove a view.",
        ),
        (
            "DROP TABLE mv",
            "ERROR:  \"mv\" is not a table\n\
             HINT:  Use DROP MATERIALIZED VIEW to remove a materialized view.",
        ),
        (
            "DROP TABLE ti",
            "ERROR:  \"ti\" is not a table\n\
             HINT:  Use DROP INDEX to remove an index.",
        ),
        (
            "DROP TABLE ct",
            "ERROR:  \"ct\" is not a table\n\
             HINT:  Use DROP TYPE to remove a type.",
        ),
        (
            "DROP TABLE IF EXISTS sq",
            "ERROR:  \"sq\" is not a table\n\
             HINT:  Use DROP SEQUENCE to remove a sequence.",
        ),
        (
            "DROP VIEW t",
            "ERROR:  \"t\" is not a view\n\
             HINT:  Use DROP TABLE to remove a table.",
        ),
        (
            "DROP VIEW ct",
            "ERROR:  \"ct\" is not a view\n\
             HINT:  Use DROP TYPE to remove a type.",
        ),
        (
            "DROP VIEW sq",
            "ERROR:  \"sq\" is not a view\n\
             HINT:  Use DROP SEQUENCE to remove a sequence.",
        ),
        (
            "DROP MATERIALIZED VIEW t",
            "ERROR:  \"t\" is not a materialized view\n\
             HINT:  Use DROP TABLE to remove a table.",
        ),
        (
            "DROP MATERIALIZED VIEW v",
            "ERROR:  \"v\" is not a materialized view\n\
             HINT:  Use DROP VIEW to remove a view.",
        ),
        (
            "DROP FOREIGN TABLE t",
            "ERROR:  \"t\" is not a foreign table\n\
             HINT:  Use DROP TABLE to remove a table.",
        ),
    ]);
}

#[test]
fn composite_drops_report_like_postgresql() {
    check_statements(&[
        ("CREATE TYPE pair AS (a int, b text)", ""),
        ("CREATE TYPE outer_pair AS (p pair, q pair[])", ""),
        ("CREATE TABLE holder (id int, p pair)", ""),
        (
            "CREATE FUNCTION takes(pair) RETURNS int LANGUAGE sql AS 'SELECT 1'",
            "",
        ),
        ("CREATE VIEW pv AS SELECT p FROM holder", ""),
        ("CREATE DOMAIN pd AS pair", ""),
        (
            "DROP TYPE pair",
            "ERROR:  cannot drop type pair because other objects depend on it\n\
             DETAIL:  column q of composite type outer_pair depends on type pair[]\n\
             column p of composite type outer_pair depends on type pair\n\
             column p of table holder depends on type pair\n\
             view pv depends on column p of table holder\n\
             function takes(pair) depends on type pair\n\
             type pd depends on type pair\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP TYPE pair CASCADE",
            "NOTICE:  drop cascades to 6 other objects\n\
             DETAIL:  drop cascades to column q of composite type outer_pair\n\
             drop cascades to column p of composite type outer_pair\n\
             drop cascades to column p of table holder\n\
             drop cascades to view pv\n\
             drop cascades to function takes(pair)\n\
             drop cascades to type pd",
        ),
        (
            "DROP TYPE IF EXISTS pair, outer_pair",
            "NOTICE:  type \"pair\" does not exist, skipping",
        ),
    ]);
}

#[test]
fn silent_drops_report_like_postgresql() {
    check_statements(&[
        (
            "CREATE TABLE t (a int PRIMARY KEY, b serial, c int CHECK (c > 0))",
            "",
        ),
        ("CREATE INDEX t_c ON t (c)", ""),
        (
            "CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN new; END'",
            "",
        ),
        (
            "CREATE TRIGGER t_trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg()",
            "",
        ),
        ("ALTER TABLE t ENABLE ROW LEVEL SECURITY", ""),
        ("CREATE POLICY t_pol ON t USING (a > 0)", ""),
        ("CREATE RULE t_rule AS ON UPDATE TO t DO ALSO NOTIFY t", ""),
        ("COMMENT ON TABLE t IS 'x'", ""),
        ("DROP TABLE t", ""),
        ("SELECT count(*) FROM pg_proc WHERE proname = 'trg'", ""),
    ]);
}

#[test]
fn routines_drops_report_like_postgresql() {
    check_statements(&[
        (
            "CREATE FUNCTION add1(int) RETURNS int LANGUAGE sql AS 'SELECT $1 + 1'",
            "",
        ),
        (
            "CREATE FUNCTION sfunc(int, int) RETURNS int LANGUAGE sql AS 'SELECT $1 + $2'",
            "",
        ),
        ("CREATE AGGREGATE agg(int) (SFUNC = sfunc, STYPE = int)", ""),
        (
            "CREATE VIEW av AS SELECT agg(x) FROM generate_series(1, 3) x",
            "",
        ),
        ("CREATE TABLE dt (a int DEFAULT add1(1), b int)", ""),
        (
            "CREATE PROCEDURE pr() LANGUAGE sql BEGIN ATOMIC SELECT add1(2); END",
            "",
        ),
        (
            "DROP FUNCTION sfunc(int, int)",
            "ERROR:  cannot drop function sfunc(integer,integer) because other objects depend on it\n\
             DETAIL:  function agg(integer) depends on function sfunc(integer,integer)\n\
             view av depends on function agg(integer)\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP FUNCTION add1(int), sfunc(int, int)",
            "ERROR:  cannot drop desired object(s) because other objects depend on them\n\
             DETAIL:  function agg(integer) depends on function sfunc(integer,integer)\n\
             view av depends on function agg(integer)\n\
             default value for column a of table dt depends on function add1(integer)\n\
             function pr() depends on function add1(integer)\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP FUNCTION add1(int), sfunc(int, int) CASCADE",
            "NOTICE:  drop cascades to 4 other objects\n\
             DETAIL:  drop cascades to function agg(integer)\n\
             drop cascades to view av\n\
             drop cascades to default value for column a of table dt\n\
             drop cascades to function pr()",
        ),
        (
            "DROP FUNCTION IF EXISTS missing(int)",
            "NOTICE:  function missing(pg_catalog.int4) does not exist, skipping",
        ),
    ]);
}

#[test]
fn operator_class_drops_report_like_postgresql() {
    check_statements(&[
        ("CREATE TYPE mood AS ENUM ('sad', 'ok')", ""),
        (
            "CREATE FUNCTION mood_text(mood) RETURNS text LANGUAGE SQL RETURN 'mood'",
            "",
        ),
        (
            "CREATE FUNCTION mood_same(mood, mood) RETURNS boolean LANGUAGE SQL RETURN $1 = $2",
            "",
        ),
        (
            "CREATE FUNCTION mood_compare(mood, mood) RETURNS integer LANGUAGE SQL RETURN CASE WHEN $1 < $2 THEN -1 WHEN $1 > $2 THEN 1 ELSE 0 END",
            "",
        ),
        (
            "CREATE CAST (mood AS text) WITH FUNCTION mood_text(mood)",
            "",
        ),
        (
            "CREATE OPERATOR === (FUNCTION = mood_same, LEFTARG = mood, RIGHTARG = mood)",
            "",
        ),
        ("CREATE OPERATOR FAMILY mood_family USING btree", ""),
        (
            "CREATE OPERATOR CLASS mood_class FOR TYPE mood USING btree FAMILY mood_family AS OPERATOR 3 ===, FUNCTION 1 mood_compare(mood, mood)",
            "",
        ),
        (
            "DROP FUNCTION mood_compare(mood, mood)",
            "ERROR:  cannot drop function mood_compare(mood,mood) because other objects depend on it\n\
             DETAIL:  operator class mood_class for access method btree depends on function mood_compare(mood,mood)\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP FUNCTION mood_same(mood, mood)",
            "ERROR:  cannot drop function mood_same(mood,mood) because other objects depend on it\n\
             DETAIL:  operator ===(mood,mood) depends on function mood_same(mood,mood)\n\
             operator class mood_class for access method btree depends on operator ===(mood,mood)\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP TYPE mood",
            "ERROR:  cannot drop type mood because other objects depend on it\n\
             DETAIL:  function mood_text(mood) depends on type mood\n\
             function mood_same(mood,mood) depends on type mood\n\
             function mood_compare(mood,mood) depends on type mood\n\
             cast from mood to text depends on type mood\n\
             operator ===(mood,mood) depends on type mood\n\
             operator class mood_class for access method btree depends on type mood\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP TYPE mood CASCADE",
            "NOTICE:  drop cascades to 6 other objects\n\
             DETAIL:  drop cascades to function mood_text(mood)\n\
             drop cascades to function mood_same(mood,mood)\n\
             drop cascades to function mood_compare(mood,mood)\n\
             drop cascades to cast from mood to text\n\
             drop cascades to operator ===(mood,mood)\n\
             drop cascades to operator class mood_class for access method btree",
        ),
    ]);
}
#[test]
fn typed_table_drops_report_like_postgresql() {
    check_statements(&[
        ("CREATE TYPE pt AS (a int, b text)", ""),
        ("CREATE TABLE typed OF pt", ""),
        ("CREATE TABLE holder (p pt)", ""),
        (
            "DROP TYPE pt",
            "ERROR:  cannot drop type pt because other objects depend on it\n\
             DETAIL:  table typed depends on type pt\n\
             column p of table holder depends on type pt\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP TYPE pt CASCADE",
            "NOTICE:  drop cascades to 2 other objects\n\
             DETAIL:  drop cascades to table typed\n\
             drop cascades to column p of table holder",
        ),
        ("SELECT count(*) FROM pg_class WHERE relname = 'typed'", ""),
    ]);
}

#[test]
fn text_search_drops_report_like_postgresql() {
    check_statements(&[
        ("CREATE SCHEMA ts", ""),
        (
            "CREATE TEXT SEARCH CONFIGURATION ts.cfg (COPY = simple)",
            "",
        ),
        (
            "CREATE TEXT SEARCH DICTIONARY ts.dict (TEMPLATE = simple)",
            "",
        ),
        ("CREATE TABLE ts.t (a int, b int)", ""),
        ("CREATE STATISTICS ts.st ON a, b FROM ts.t", ""),
        (
            "DROP SCHEMA ts",
            "ERROR:  cannot drop schema ts because other objects depend on it\n\
             DETAIL:  text search configuration ts.cfg depends on schema ts\n\
             text search dictionary ts.dict depends on schema ts\n\
             table ts.t depends on schema ts\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP SCHEMA ts CASCADE",
            "NOTICE:  drop cascades to 3 other objects\n\
             DETAIL:  drop cascades to text search configuration ts.cfg\n\
             drop cascades to text search dictionary ts.dict\n\
             drop cascades to table ts.t",
        ),
    ]);
}

/// A foreign key depends on the one unique index it was created against
/// (`conindid`): the oldest immediate one over its referenced columns,
/// whether a constraint owns it or not.
#[test]
fn foreign_keys_depend_on_their_referenced_index() {
    check_statements(&[
        ("CREATE TABLE p (a int, b int)", ""),
        ("CREATE UNIQUE INDEX p_idx ON p (a)", ""),
        ("ALTER TABLE p ADD CONSTRAINT p_uq UNIQUE (a)", ""),
        ("CREATE TABLE c (a int REFERENCES p (a))", ""),
        ("CREATE TABLE d (a int REFERENCES p (a))", ""),
        ("ALTER TABLE p DROP CONSTRAINT p_uq", ""),
        (
            "DROP INDEX p_idx",
            "ERROR:  cannot drop index p_idx because other objects depend on it\n\
             DETAIL:  constraint c_a_fkey on table c depends on index p_idx\n\
             constraint d_a_fkey on table d depends on index p_idx\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "ALTER TABLE p DROP COLUMN a",
            "ERROR:  cannot drop column a of table p because other objects depend on it\n\
             DETAIL:  constraint c_a_fkey on table c depends on column a of table p\n\
             constraint d_a_fkey on table d depends on column a of table p\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "DROP INDEX p_idx CASCADE",
            "NOTICE:  drop cascades to 2 other objects\n\
             DETAIL:  drop cascades to constraint c_a_fkey on table c\n\
             drop cascades to constraint d_a_fkey on table d",
        ),
        ("ALTER TABLE p ADD CONSTRAINT p_uq UNIQUE (a)", ""),
        ("CREATE UNIQUE INDEX p_idx ON p (a)", ""),
        ("ALTER TABLE c ADD FOREIGN KEY (a) REFERENCES p (a)", ""),
        ("DROP INDEX p_idx", ""),
        (
            "ALTER TABLE p DROP CONSTRAINT p_uq",
            "ERROR:  cannot drop constraint p_uq on table p because other objects depend on it\n\
             DETAIL:  constraint c_a_fkey on table c depends on index p_uq\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "ALTER TABLE p DROP CONSTRAINT p_uq CASCADE",
            "NOTICE:  drop cascades to constraint c_a_fkey on table c",
        ),
        ("CREATE UNIQUE INDEX p_cover ON p (a) INCLUDE (b)", ""),
        ("ALTER TABLE c ADD FOREIGN KEY (a) REFERENCES p (a)", ""),
        (
            "ALTER TABLE p DROP COLUMN b",
            "ERROR:  cannot drop column b of table p because other objects depend on it\n\
             DETAIL:  constraint c_a_fkey on table c depends on index p_cover\n\
             HINT:  Use DROP ... CASCADE to drop the dependent objects too.",
        ),
        (
            "ALTER TABLE p DROP COLUMN b CASCADE",
            "NOTICE:  drop cascades to constraint c_a_fkey on table c",
        ),
    ]);
}
