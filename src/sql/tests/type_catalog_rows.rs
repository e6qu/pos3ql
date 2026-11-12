//! `pg_type` lists PostgreSQL 18.6's 32 columns, every pg_catalog type and
//! the columns PostgreSQL derives for user-defined types, compared with
//! `psql -At` output of the same queries.

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
fn pg_type_lists_postgresql_columns_and_builtin_rows() {
    let (mut engine, mut budget) = test_engine();
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT * FROM pg_type WHERE oid IN (23, 1007, 2275, 2276, 2249, 71) ORDER BY oid"
        ),
        [
            "23|int4|11|10|4|t|b|N|f|t|,|0|-|0|1007|int4in|int4out|int4recv|int4send|-|-|-|i|p|f|0|-1|0|0|NULL|NULL|NULL",
            "71|pg_type|11|10|-1|f|c|C|f|t|,|1247|-|0|210|record_in|record_out|record_recv|record_send|-|-|-|d|x|f|0|-1|0|0|NULL|NULL|NULL",
            "1007|_int4|11|10|-1|f|b|A|f|t|,|0|array_subscript_handler|23|0|array_in|array_out|array_recv|array_send|-|-|array_typanalyze|i|x|f|0|-1|0|0|NULL|NULL|NULL",
            "2249|record|11|10|-1|f|p|P|f|t|,|0|-|0|2287|record_in|record_out|record_recv|record_send|-|-|-|d|x|f|0|-1|0|0|NULL|NULL|NULL",
            "2275|cstring|11|10|-2|f|p|P|f|t|,|0|-|0|1263|cstring_in|cstring_out|cstring_recv|cstring_send|-|-|-|c|p|f|0|-1|0|0|NULL|NULL|NULL",
            "2276|any|11|10|4|t|p|P|f|t|,|0|-|0|0|any_in|any_out|-|-|-|-|-|i|p|f|0|-1|0|0|NULL|NULL|NULL",
        ]
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT count(*) FROM pg_type WHERE typnamespace = 11"
        ),
        ["473"]
    );
}

#[test]
fn pg_type_derives_user_type_columns_like_postgresql() {
    let (mut engine, mut budget) = test_engine();
    rows(
        &mut engine,
        &mut budget,
        "CREATE DOMAIN dm AS int NOT NULL DEFAULT 3; CREATE DOMAIN dv AS varchar(5); \
         CREATE TYPE en AS ENUM ('a'); CREATE DOMAIN da AS en[]; CREATE DOMAIN dd AS dm; \
         CREATE TYPE ct AS (x int); CREATE TABLE t (a int); CREATE VIEW v AS SELECT 1 AS x",
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT typname, typlen, typbyval, typtype, typcategory, typispreferred, \
             typisdefined, typdelim, typrelid <> 0, typsubscript, typelem::regtype, \
             typarray::regtype, typinput, typoutput, typreceive, typsend, typmodin, \
             typmodout, typanalyze, typalign, typstorage, typnotnull, typbasetype::regtype, \
             typtypmod, typndims, typcollation, typdefault FROM pg_type WHERE oid >= 16384 ORDER BY typname"
        ),
        [
            "_ct|-1|f|b|A|f|t|,|f|array_subscript_handler|ct|-|array_in|array_out|array_recv|array_send|-|-|array_typanalyze|d|x|f|-|-1|0|0|NULL",
            "_da|-1|f|b|A|f|t|,|f|array_subscript_handler|da|-|array_in|array_out|array_recv|array_send|-|-|array_typanalyze|i|x|f|-|-1|0|0|NULL",
            "_dd|-1|f|b|A|f|t|,|f|array_subscript_handler|dd|-|array_in|array_out|array_recv|array_send|-|-|array_typanalyze|i|x|f|-|-1|0|0|NULL",
            "_dm|-1|f|b|A|f|t|,|f|array_subscript_handler|dm|-|array_in|array_out|array_recv|array_send|-|-|array_typanalyze|i|x|f|-|-1|0|0|NULL",
            "_dv|-1|f|b|A|f|t|,|f|array_subscript_handler|dv|-|array_in|array_out|array_recv|array_send|-|-|array_typanalyze|i|x|f|-|-1|0|100|NULL",
            "_en|-1|f|b|A|f|t|,|f|array_subscript_handler|en|-|array_in|array_out|array_recv|array_send|-|-|array_typanalyze|i|x|f|-|-1|0|0|NULL",
            "_t|-1|f|b|A|f|t|,|f|array_subscript_handler|t|-|array_in|array_out|array_recv|array_send|-|-|array_typanalyze|d|x|f|-|-1|0|0|NULL",
            "_v|-1|f|b|A|f|t|,|f|array_subscript_handler|v|-|array_in|array_out|array_recv|array_send|-|-|array_typanalyze|d|x|f|-|-1|0|0|NULL",
            "ct|-1|f|c|C|f|t|,|t|-|-|ct[]|record_in|record_out|record_recv|record_send|-|-|-|d|x|f|-|-1|0|0|NULL",
            "da|-1|f|d|A|f|t|,|f|-|-|da[]|domain_in|array_out|domain_recv|array_send|-|-|array_typanalyze|i|x|f|en[]|-1|1|0|NULL",
            "dd|4|t|d|N|f|t|,|f|-|-|dd[]|domain_in|int4out|domain_recv|int4send|-|-|-|i|p|f|dm|-1|0|0|3",
            "dm|4|t|d|N|f|t|,|f|-|-|dm[]|domain_in|int4out|domain_recv|int4send|-|-|-|i|p|t|integer|-1|0|0|3",
            "dv|-1|f|d|S|f|t|,|f|-|-|dv[]|domain_in|varcharout|domain_recv|varcharsend|-|-|-|i|x|f|character varying|9|0|100|NULL",
            "en|4|t|e|E|f|t|,|f|-|-|en[]|enum_in|enum_out|enum_recv|enum_send|-|-|-|i|p|f|-|-1|0|0|NULL",
            "t|-1|f|c|C|f|t|,|t|-|-|t[]|record_in|record_out|record_recv|record_send|-|-|-|d|x|f|-|-1|0|0|NULL",
            "v|-1|f|c|C|f|t|,|t|-|-|v[]|record_in|record_out|record_recv|record_send|-|-|-|d|x|f|-|-1|0|0|NULL",
        ]
    );
}
