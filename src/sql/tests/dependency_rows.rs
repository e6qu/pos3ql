//! `pg_depend` rows PostgreSQL 18.6 records for partition keys, PL/pgSQL
//! routines, expression indexes, statistics key columns and text search
//! objects, and the `DROP`s those rows drive.

use super::routine_identity_diagnostics::diagnostics;
use super::*;

fn rows(engine: &mut Engine, budget: &mut Budget, sql: &str) -> Vec<String> {
    let output = run_with(engine, budget, sql);
    assert!(
        error_sqlstates(&output).is_empty(),
        "{sql}: {}",
        String::from_utf8_lossy(&output)
    );
    data_rows(&output)
}

#[test]
fn pg_depend_records_postgresql_rows_for_keys_languages_and_text_search() {
    let (mut engine, mut budget) = test_engine();
    rows(
        &mut engine,
        &mut budget,
        "CREATE TABLE t (a int, c int); CREATE STATISTICS s1 ON a, c FROM t; \
         CREATE STATISTICS s2 ON (a + c) FROM t; CREATE INDEX i2 ON t ((a + 1)); \
         CREATE TABLE p (k int, v int) PARTITION BY RANGE (k); \
         CREATE FUNCTION g() RETURNS int LANGUAGE plpgsql AS 'BEGIN RETURN 1; END'; \
         CREATE TEXT SEARCH DICTIONARY td (TEMPLATE = snowball, LANGUAGE = english); \
         CREATE TEXT SEARCH CONFIGURATION tc (PARSER = default); \
         ALTER TEXT SEARCH CONFIGURATION tc ADD MAPPING FOR word WITH td",
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT pg_describe_object(classid, objid, objsubid), \
             pg_describe_object(refclassid, refobjid, refobjsubid), deptype FROM pg_depend \
             WHERE refclassid <> 'pg_namespace'::regclass AND (objid IN ('p'::regclass, 'g()'::regprocedure, \
             'i2'::regclass, 'td'::regdictionary, 'tc'::regconfig) \
             OR objid IN (SELECT oid FROM pg_statistic_ext)) ORDER BY 1, 2"
        ),
        [
            "column k of table p|table p|i",
            "function g()|language plpgsql|n",
            "index i2|column a of table t|a",
            "index i2|table t|a",
            "statistics object s1|column a of table t|a",
            "statistics object s1|column c of table t|a",
            "statistics object s2|column a of table t|a",
            "statistics object s2|column c of table t|a",
            "statistics object s2|table t|a",
            "text search configuration tc|text search dictionary td|n",
            "text search dictionary td|text search template snowball|n",
        ]
    );
    let output = run_with(&mut engine, &mut budget, "DROP TEXT SEARCH DICTIONARY td");
    assert_eq!(
        diagnostics(&output),
        [(
            'E',
            "2BP01".to_string(),
            "cannot drop text search dictionary td because other objects depend on it".to_string()
        )]
    );
    let output = run_with(
        &mut engine,
        &mut budget,
        "DROP TEXT SEARCH DICTIONARY td CASCADE",
    );
    assert_eq!(
        diagnostics(&output),
        [(
            'N',
            "00000".to_string(),
            "drop cascades to text search configuration tc".to_string()
        )]
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT count(*) FROM pg_ts_config WHERE cfgname = 'tc'"
        ),
        ["0"]
    );
    let output = run_with(&mut engine, &mut budget, "ALTER TABLE p DROP COLUMN k");
    assert_eq!(
        diagnostics(&output),
        [(
            'E',
            "42P16".to_string(),
            "cannot drop column \"k\" because it is part of the partition key of relation \"p\""
                .to_string()
        )]
    );
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE par (a int); CREATE TABLE chi () INHERITS (par); ALTER TABLE chi DROP COLUMN a",
    );
    assert_eq!(
        diagnostics(&output),
        [(
            'E',
            "42P16".to_string(),
            "cannot drop inherited column \"a\"".to_string()
        )]
    );
}
