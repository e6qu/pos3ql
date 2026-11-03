use super::*;

// PostgreSQL 18.6 reference SQL SHA256:
// 93c8789a9f6223041bf340f5212685317f8510424cedcbbdec9a5e384a1f483b.
#[test]
fn catalog_oid_aliases_compare_unsigned_values_across_declared_types() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config("catalog-oid-alias-comparisons");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let aliases = [
            "regproc",
            "regprocedure",
            "regoper",
            "regoperator",
            "regclass",
            "regtype",
            "regnamespace",
            "regrole",
            "regconfig",
            "regdictionary",
            "regcollation",
        ];
        for left in aliases {
            for right in aliases {
                let sql = format!(
                    "SELECT 1::{left}=1::{right},1::{left}<2::{right},2::{left}>1::{right}"
                );
                assert_eq!(
                    data_rows(&session.success(&mut engine, &sql, true)),
                    ["t|t|t"],
                    "{sql}"
                );
            }
        }
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT 4294967295::oid>1::oid,4294967295::regproc>1::regprocedure, \
                    4294967295::regtype>1::regclass; \
             SELECT 0::oid<(-1)::integer,4294967295::oid=(-1)::integer,(-1)::integer::oid=4294967295::oid; \
             SELECT value::oid FROM (VALUES(4294967295::regproc),(1::regproc),(2147483648::regproc)) input(value) ORDER BY value; \
             SELECT count(*) FROM (VALUES(1::regproc),(2::regproc)) left_input(value) \
               JOIN (VALUES(1::regprocedure),(2::regprocedure)) right_input(value) \
                 ON left_input.value=right_input.value", true)),
            ["t|t|t", "t|t|t", "1", "2147483648", "4294967295", "2"]);
        for sql in [
            "SELECT 0::oid<(-1)::bigint",
            "SELECT (-1::bigint)::oid",
            "SELECT 0::oid<4294967296::bigint",
        ] {
            let wire = session.execute(&mut engine, sql, true);
            assert_eq!(error_sqlstates(&wire), ["22003"], "{sql}");
            assert!(String::from_utf8_lossy(&wire).contains("OID out of range"));
        }
        for sql in [
            "SELECT ARRAY[1::regproc]=ARRAY[1::regprocedure]",
            "SELECT 1::regproc=1::numeric",
        ] {
            assert_eq!(
                error_sqlstates(&session.execute(&mut engine, sql, true)),
                ["42883"],
                "{sql}"
            );
        }
    }
}
