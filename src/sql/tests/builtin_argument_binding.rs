use super::*;

#[test]
fn selected_builtin_arguments_preserve_defaults_names_and_once_only_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config("selected-builtin-arguments");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture = include_str!(
            "../../../tests/external/differential_exact/63_builtin_argument_binding.sql"
        );
        let requests = fixture.split("-- next request").collect::<Vec<_>>();
        assert_eq!(
            data_rows(&session.success(&mut engine, requests[0], true)),
            [
                "107|integer",
                "1 year|interval",
                "00:00:00",
                "2 days",
                "Mixed,bare",
                "1 year 00:00:02",
                "3",
                "a",
                "a",
                "4",
                "t",
                "1",
                "t",
                "1",
            ]
        );
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, requests[1], true)),
            ["42883"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, requests[2], true)),
            ["1", "", "", "", "t|t|t|t|t|t", "null|null|t|t|t|t|t"]
        );
        assert_eq!(data_rows(&session.success(&mut engine,
            "CREATE SEQUENCE runtime_null_effects; \
             SELECT pg_catalog.make_interval(years=>CASE WHEN nextval('runtime_null_effects')=1 THEN NULL ELSE 1 END,secs=>nextval('runtime_null_effects')) IS NULL; \
             SELECT nextval('runtime_null_effects'); DROP SEQUENCE runtime_null_effects", true)), ["t", "3"]);

        let before = engine.storage.shared_statistics_reset_times();
        session.success(
            &mut engine,
            "SELECT pg_catalog.pg_stat_reset_shared(NULL)",
            true,
        );
        let explicit = engine.storage.shared_statistics_reset_times();
        assert!(explicit.background_writer > before.background_writer);
        assert_eq!(explicit.archiver, explicit.background_writer);
        assert_eq!(explicit.checkpointer, explicit.background_writer);
        assert_eq!(explicit.io, explicit.background_writer);
        assert_eq!(explicit.recovery_prefetch, explicit.background_writer);
        assert_eq!(explicit.wal, explicit.background_writer);
        assert_eq!(explicit.slru, [explicit.background_writer; 8]);
        session.success(
            &mut engine,
            "SELECT pg_catalog.pg_stat_reset_shared()",
            true,
        );
        let omitted = engine.storage.shared_statistics_reset_times();
        assert!(omitted.background_writer > explicit.background_writer);
        assert_eq!(omitted.archiver, omitted.background_writer);
        assert_eq!(omitted.checkpointer, omitted.background_writer);
        assert_eq!(omitted.io, omitted.background_writer);
        assert_eq!(omitted.recovery_prefetch, omitted.background_writer);
        assert_eq!(omitted.wal, omitted.background_writer);
        assert_eq!(omitted.slru, [omitted.background_writer; 8]);

        let vectors = session.success(
            &mut engine,
            "SELECT array_cat('1 2'::int2vector,'3 4'::int2vector), \
                    array_cat('1 2'::oidvector,'3 4'::oidvector)",
            true,
        );
        assert_eq!(row_description_type_oids(&vectors), [1005, 1028]);
        assert_eq!(data_rows(&vectors), ["[0:3]={1,2,3,4}|[0:3]={1,2,3,4}"]);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT array_dims(array_cat('1 2'::int2vector,'3 4'::int2vector)), \
                    array_dims(array_cat('1 2'::oidvector,'3 4'::oidvector)); \
             SELECT array_append('1 2'::int2vector,3::smallint), \
                    array_prepend(0::smallint,'1 2'::int2vector), \
                    array_position('1 2'::int2vector,2::smallint), \
                    array_positions('1 2'::int2vector,2::smallint), \
                    array_remove('1 2'::int2vector,2::smallint), \
                    array_replace('1 2'::int2vector,2::smallint,7::smallint); \
             CREATE SEQUENCE vector_binding_effects; \
             SELECT array_cat((nextval('vector_binding_effects')::text||' 2')::int2vector, \
                              (nextval('vector_binding_effects')::text||' 4')::int2vector); \
             SELECT nextval('vector_binding_effects'); DROP SEQUENCE vector_binding_effects",
                true
            )),
            [
                "[0:3]|[0:3]",
                "[0:2]={1,2,3}|[0:2]={0,1,2}|1|{1}|[0:0]={1}|[0:1]={1,7}",
                "[0:3]={1,2,2,4}",
                "3",
            ]
        );
    }
}
