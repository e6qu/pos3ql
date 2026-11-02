use super::*;

#[test]
fn like_identity_copies_generator_options_and_serial_defaults_share_the_source() {
    let config = test_config("like-sequence-ownership");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes((1 << 27) + (1 << 20)));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session =
        ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 32 << 20);
    let identity = session.execute(
        &mut engine,
        "CREATE TABLE like_identity_source(id integer GENERATED ALWAYS AS IDENTITY \
           (START WITH 11 INCREMENT BY 3 CACHE 2 CYCLE MAXVALUE 100)); \
         INSERT INTO like_identity_source DEFAULT VALUES RETURNING id",
        false,
    );
    assert!(error_sqlstates(&identity).is_empty());
    assert_eq!(data_rows(&identity), ["11"]);
    let copied = run_with_fixed_memory(
        &mut engine,
        &budget,
        "CREATE TABLE like_identity_copy(LIKE like_identity_source INCLUDING IDENTITY)",
        1 << 20,
    );
    assert!(
        error_sqlstates(&copied).is_empty(),
        "{}",
        String::from_utf8_lossy(&copied)
    );
    let identity = session.execute(
        &mut engine,
        "CREATE TABLE like_identity_plain(LIKE like_identity_source INCLUDING DEFAULTS); \
         INSERT INTO like_identity_copy DEFAULT VALUES RETURNING id; \
         INSERT INTO like_identity_source DEFAULT VALUES RETURNING id; \
         INSERT INTO like_identity_copy DEFAULT VALUES RETURNING id; \
         SELECT seqstart,seqincrement,seqcache,seqcycle,seqmax FROM pg_sequence \
           JOIN pg_class ON pg_class.oid=seqrelid WHERE relname IN \
           ('like_identity_source_id_seq','like_identity_copy_id_seq') ORDER BY relname; \
         SELECT pg_get_serial_sequence('like_identity_source','id'), \
                pg_get_serial_sequence('like_identity_copy','id'), \
                pg_get_serial_sequence('like_identity_plain','id')",
        false,
    );
    assert!(
        error_sqlstates(&identity).is_empty(),
        "{}",
        String::from_utf8_lossy(&identity)
    );
    assert_eq!(
        data_rows(&identity),
        [
            "11",
            "14",
            "14",
            "11|3|2|t|100",
            "11|3|2|t|100",
            "public.like_identity_source_id_seq|public.like_identity_copy_id_seq|NULL"
        ]
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "INSERT INTO like_identity_plain DEFAULT VALUES",
            false
        )),
        ["23502"]
    );
    assert!(
        error_sqlstates(&session.execute(
            &mut engine,
            "CREATE TABLE like_identity_child() INHERITS(like_identity_source)",
            false
        ))
        .is_empty()
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "INSERT INTO like_identity_child DEFAULT VALUES",
            false
        )),
        ["23502"]
    );
    assert_eq!(
        data_rows(&session.execute(
            &mut engine,
            "INSERT INTO like_identity_child(id) VALUES(101) RETURNING id; \
         SELECT attidentity::text FROM pg_attribute WHERE attrelid='like_identity_child'::regclass \
           AND attname='id'; DROP TABLE like_identity_child",
            false
        )),
        ["101", ""]
    );
    let serial = session.execute(&mut engine, "CREATE TABLE like_serial_source(id serial); \
         CREATE TABLE like_serial_defaults(LIKE like_serial_source INCLUDING DEFAULTS); \
         CREATE TABLE like_serial_all(LIKE like_serial_source INCLUDING ALL); \
         CREATE TABLE like_serial_identity(LIKE like_serial_source INCLUDING IDENTITY); \
         CREATE TABLE like_serial_child() INHERITS(like_serial_source); \
         INSERT INTO like_serial_source DEFAULT VALUES RETURNING id; \
         INSERT INTO like_serial_defaults DEFAULT VALUES RETURNING id; \
         INSERT INTO like_serial_all DEFAULT VALUES RETURNING id; \
         INSERT INTO like_serial_child DEFAULT VALUES RETURNING id; \
         SELECT pg_get_serial_sequence('like_serial_source','id'), \
                pg_get_serial_sequence('like_serial_defaults','id'), \
                pg_get_serial_sequence('like_serial_all','id'), \
                pg_get_serial_sequence('like_serial_identity','id'), \
                pg_get_serial_sequence('like_serial_child','id'); \
         SELECT relation.relname,sequence.relname,dependency.deptype FROM pg_depend dependency \
           JOIN pg_class sequence ON sequence.oid=dependency.objid AND sequence.relkind='S' \
           JOIN pg_class relation ON relation.oid=dependency.refobjid \
           WHERE dependency.classid='pg_class'::regclass AND dependency.refclassid='pg_class'::regclass \
             AND dependency.refobjsubid=1 AND relation.relname IN \
             ('like_identity_source','like_identity_copy','like_serial_source', \
              'like_serial_defaults','like_serial_all','like_serial_identity','like_serial_child') \
           ORDER BY relation.relname", false);
    assert!(
        error_sqlstates(&serial).is_empty(),
        "{}",
        String::from_utf8_lossy(&serial)
    );
    assert_eq!(
        data_rows(&serial),
        [
            "1",
            "2",
            "3",
            "4",
            "public.like_serial_source_id_seq|NULL|NULL|NULL|NULL",
            "like_identity_copy|like_identity_copy_id_seq|i",
            "like_identity_source|like_identity_source_id_seq|i",
            "like_serial_source|like_serial_source_id_seq|a"
        ]
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "INSERT INTO like_serial_identity DEFAULT VALUES",
            false
        )),
        ["23502"]
    );
}

#[test]
fn copied_identity_sequence_exhaustion_rolls_back_the_new_table() {
    let mut config = test_config("like-identity-capacity");
    config.max_tables = 2;
    config.max_sequences = 1;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    assert!(
        error_sqlstates(&run_with(
            &mut engine,
            &mut budget,
            "CREATE TABLE limited_identity(id integer GENERATED ALWAYS AS IDENTITY)"
        ))
        .is_empty()
    );
    let exhausted = run_with_fixed_memory(
        &mut engine,
        &budget,
        "CREATE TABLE limited_copy(LIKE limited_identity INCLUDING IDENTITY)",
        1 << 20,
    );
    assert_eq!(error_sqlstates(&exhausted), ["54000"]);
    assert!(
        engine
            .storage
            .find_table("public", "limited_copy")
            .is_none()
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "INSERT INTO limited_identity DEFAULT VALUES RETURNING id"
        )),
        ["1"]
    );
}

#[test]
fn partition_identity_uses_root_generator_without_inheriting_sequence_ownership() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let name = format!("partition-identity-generators-{mode:?}");
        let mut config = test_config(&name);
        config.transaction_mode = mode;
        config.object_store_on = true;
        config.object_store_sim = true;
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.object_store_bucket = format!("{name}-{}", std::process::id());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let initial = run_with(
            &mut engine,
            &mut budget,
            "CREATE TABLE identity_root(id integer GENERATED ALWAYS AS IDENTITY \
               (START WITH 7 INCREMENT BY 3),payload integer) PARTITION BY RANGE(payload); \
             CREATE TABLE identity_leaf_a PARTITION OF identity_root FOR VALUES FROM(0) TO(10); \
             CREATE TABLE identity_leaf_b PARTITION OF identity_root FOR VALUES FROM(10) TO(20); \
             INSERT INTO identity_root(payload) VALUES(1) RETURNING id; \
             INSERT INTO identity_leaf_a(payload) VALUES(2) RETURNING id; \
             INSERT INTO identity_leaf_b(payload) VALUES(12) RETURNING id; \
             INSERT INTO identity_root(payload) VALUES(13) RETURNING id; \
             CREATE TABLE identity_attached(payload integer,id integer NOT NULL)",
        );
        assert!(
            error_sqlstates(&initial).is_empty(),
            "{}",
            String::from_utf8_lossy(&initial)
        );
        assert_eq!(data_rows(&initial), ["7", "10", "13", "16"]);
        let rollback = run_with(
            &mut engine,
            &mut budget,
            "BEGIN; ALTER TABLE identity_root ATTACH PARTITION identity_attached FOR VALUES FROM(20) TO(30); \
             ROLLBACK; SELECT attidentity::text FROM pg_attribute \
               WHERE attrelid='identity_attached'::regclass AND attname='id'",
        );
        assert!(error_sqlstates(&rollback).is_empty());
        assert_eq!(data_rows(&rollback), [""]);
        let attached = run_with(
            &mut engine,
            &mut budget,
            "ALTER TABLE identity_root ATTACH PARTITION identity_attached FOR VALUES FROM(20) TO(30); \
             INSERT INTO identity_attached(payload) VALUES(21) RETURNING id; \
             INSERT INTO identity_root(payload) VALUES(22) RETURNING id; \
             CREATE TABLE identity_leaf_copy(LIKE identity_leaf_a INCLUDING IDENTITY); \
             INSERT INTO identity_leaf_copy DEFAULT VALUES RETURNING id; \
             TRUNCATE identity_leaf_a RESTART IDENTITY; \
             INSERT INTO identity_root(payload) VALUES(3) RETURNING id; \
             SELECT pg_get_serial_sequence('identity_root','id'), \
               pg_get_serial_sequence('identity_leaf_a','id'), \
               pg_get_serial_sequence('identity_attached','id'), \
               pg_get_serial_sequence('identity_leaf_copy','id'); \
             CREATE TABLE serial_root(id serial,payload integer) PARTITION BY RANGE(payload); \
             CREATE TABLE serial_leaf PARTITION OF serial_root FOR VALUES FROM(0) TO(10); \
             INSERT INTO serial_root(payload) VALUES(1) RETURNING id; \
             INSERT INTO serial_leaf(payload) VALUES(2) RETURNING id",
        );
        assert!(
            error_sqlstates(&attached).is_empty(),
            "{}",
            String::from_utf8_lossy(&attached)
        );
        assert_eq!(
            data_rows(&attached),
            [
                "19",
                "22",
                "7",
                "25",
                "public.identity_root_id_seq|NULL|NULL|public.identity_leaf_copy_id_seq",
                "1",
                "2"
            ]
        );
        assert!(engine.checkpoint().unwrap());
        drop(engine);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        let mut recovered_budget = test_engine_budget(&config, 1 << 29);
        let mut recovered = Engine::new(&config, &mut recovered_budget).unwrap();
        let continued = run_with(
            &mut recovered,
            &mut recovered_budget,
            "INSERT INTO identity_attached(payload) VALUES(23) RETURNING id; \
             INSERT INTO identity_leaf_copy DEFAULT VALUES RETURNING id; \
             INSERT INTO serial_leaf(payload) VALUES(3) RETURNING id; \
             SELECT pg_get_serial_sequence('serial_leaf','id')",
        );
        assert!(
            error_sqlstates(&continued).is_empty(),
            "{}",
            String::from_utf8_lossy(&continued)
        );
        assert_eq!(data_rows(&continued), ["28", "10", "3", "NULL"]);
        let rollback = run_with(
            &mut recovered,
            &mut recovered_budget,
            "BEGIN; ALTER TABLE identity_root DETACH PARTITION identity_attached; ROLLBACK; \
             INSERT INTO identity_attached(payload) VALUES(24) RETURNING id",
        );
        assert!(error_sqlstates(&rollback).is_empty());
        assert_eq!(data_rows(&rollback), ["31"]);
        let detached = run_with(
            &mut recovered,
            &mut recovered_budget,
            "ALTER TABLE identity_root DETACH PARTITION identity_attached; \
             SELECT attidentity::text FROM pg_attribute WHERE attrelid='identity_attached'::regclass \
               AND attname='id'",
        );
        assert!(error_sqlstates(&detached).is_empty());
        assert_eq!(data_rows(&detached), [""]);
        assert_eq!(
            error_sqlstates(&run_with(
                &mut recovered,
                &mut recovered_budget,
                "INSERT INTO identity_attached(payload) VALUES(25)"
            )),
            ["23502"]
        );
        drop(recovered);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }
}

#[test]
fn nested_partition_detach_clears_identity_and_rejects_new_partitions_with_own_identity() {
    let (mut engine, mut budget) = test_engine();
    let setup = run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE nested_identity_root(id integer GENERATED ALWAYS AS IDENTITY \
           (START WITH 7 INCREMENT BY 3),payload integer) PARTITION BY RANGE(payload); \
         CREATE TABLE nested_identity_sub PARTITION OF nested_identity_root \
           FOR VALUES FROM(0) TO(100) PARTITION BY RANGE(payload); \
         CREATE TABLE nested_identity_leaf PARTITION OF nested_identity_sub FOR VALUES FROM(0) TO(100); \
         INSERT INTO nested_identity_root(payload) VALUES(1) RETURNING id; \
         INSERT INTO nested_identity_sub(payload) VALUES(2) RETURNING id; \
         INSERT INTO nested_identity_leaf(payload) VALUES(3) RETURNING id",
    );
    assert!(
        error_sqlstates(&setup).is_empty(),
        "{}",
        String::from_utf8_lossy(&setup)
    );
    assert_eq!(data_rows(&setup), ["7", "10", "13"]);
    let rolled_back = run_with(
        &mut engine,
        &mut budget,
        "BEGIN; ALTER TABLE nested_identity_root DETACH PARTITION nested_identity_sub; ROLLBACK; \
         INSERT INTO nested_identity_leaf(payload) VALUES(4) RETURNING id",
    );
    assert!(error_sqlstates(&rolled_back).is_empty());
    assert_eq!(data_rows(&rolled_back), ["16"]);
    let detached = run_with(
        &mut engine,
        &mut budget,
        "ALTER TABLE nested_identity_root DETACH PARTITION nested_identity_sub; \
         SELECT attidentity::text FROM pg_attribute WHERE attrelid='nested_identity_sub'::regclass AND attname='id'; \
         SELECT attidentity::text FROM pg_attribute WHERE attrelid='nested_identity_leaf'::regclass AND attname='id'",
    );
    assert!(error_sqlstates(&detached).is_empty());
    assert_eq!(data_rows(&detached), ["", ""]);
    for query in [
        "INSERT INTO nested_identity_sub(payload) VALUES(5)",
        "INSERT INTO nested_identity_leaf(payload) VALUES(6)",
    ] {
        assert_eq!(
            error_sqlstates(&run_with(&mut engine, &mut budget, query)),
            ["23502"]
        );
    }
    assert!(error_sqlstates(&run_with(&mut engine, &mut budget,
        "CREATE TABLE nested_identity_owned(payload integer,id integer GENERATED ALWAYS AS IDENTITY)")).is_empty());
    assert_eq!(
        error_sqlstates(&run_with(
            &mut engine,
            &mut budget,
            "ALTER TABLE nested_identity_root ATTACH PARTITION nested_identity_owned FOR VALUES FROM(100) TO(200)"
        )),
        ["55000"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "INSERT INTO nested_identity_owned(payload) VALUES(101) RETURNING id"
        )),
        ["1"]
    );
}

#[test]
fn partition_identity_modes_follow_the_root_through_rollback_and_cold_recovery() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("partition-identity-mode-{mode:?}"));
        config.transaction_mode = mode;
        config.object_store_on = true;
        config.wal_upload = false;
        config.wal_upload_sync = false;
        config.object_store_bucket =
            format!("partition-identity-mode-{mode:?}-{}", std::process::id());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let server = object_reads::HttpObjectServer::start(&mut config);
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE TABLE mode_root(id bigint GENERATED BY DEFAULT AS IDENTITY (START WITH 100),payload integer NOT NULL) PARTITION BY RANGE(payload); \
             CREATE TABLE mode_sub PARTITION OF mode_root FOR VALUES FROM(0) TO(100) PARTITION BY RANGE(payload); \
             CREATE TABLE mode_leaf PARTITION OF mode_sub FOR VALUES FROM(0) TO(100); \
             CREATE TABLE mode_attached(payload integer NOT NULL,id bigint NOT NULL); \
             ALTER TABLE mode_root ATTACH PARTITION mode_attached FOR VALUES FROM(100) TO(200)", true);
        let identities = "SELECT c.relname,a.attidentity::text FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid \
            WHERE c.relname IN ('mode_root','mode_sub','mode_leaf','mode_attached') AND a.attname='id' ORDER BY c.relname";
        assert_eq!(
            data_rows(&session.success(&mut engine, identities, true)),
            [
                "mode_attached|d",
                "mode_leaf|d",
                "mode_root|d",
                "mode_sub|d"
            ]
        );
        for invalid in [
            "ALTER TABLE ONLY mode_root ALTER COLUMN id SET GENERATED ALWAYS",
            "ALTER TABLE mode_leaf ALTER COLUMN id SET GENERATED BY DEFAULT",
            "ALTER TABLE mode_attached ALTER COLUMN id SET GENERATED ALWAYS",
            "ALTER TABLE mode_leaf ALTER COLUMN payload SET GENERATED ALWAYS",
        ] {
            assert_eq!(
                error_sqlstates(&session.execute(&mut engine, invalid, true)),
                ["42P16"]
            );
        }
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                "ALTER TABLE mode_leaf ALTER COLUMN missing SET GENERATED ALWAYS",
                true
            )),
            ["42703"]
        );
        let not_identity = session.execute(
            &mut engine,
            "ALTER TABLE mode_root ALTER COLUMN payload SET GENERATED ALWAYS",
            true,
        );
        assert_eq!(error_sqlstates(&not_identity), ["55000"]);
        assert!(String::from_utf8_lossy(&not_identity).contains("column \"payload\""));
        session.success(
            &mut engine,
            "BEGIN; ALTER TABLE mode_root ALTER COLUMN id SET GENERATED ALWAYS",
            true,
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, identities, true)),
            [
                "mode_attached|a",
                "mode_leaf|a",
                "mode_root|a",
                "mode_sub|a"
            ]
        );
        session.success(&mut engine, "ROLLBACK", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, identities, true)),
            [
                "mode_attached|d",
                "mode_leaf|d",
                "mode_root|d",
                "mode_sub|d"
            ]
        );
        session.success(&mut engine,
            "INSERT INTO mode_leaf(id,payload) VALUES(900,1); INSERT INTO mode_attached(id,payload) VALUES(901,101); \
             ALTER TABLE mode_root ALTER COLUMN id SET GENERATED ALWAYS,ALTER COLUMN payload SET NOT NULL", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, identities, true)),
            [
                "mode_attached|a",
                "mode_leaf|a",
                "mode_root|a",
                "mode_sub|a"
            ]
        );
        session.success(
            &mut engine,
            "ALTER TABLE mode_root ALTER COLUMN id SET GENERATED BY DEFAULT",
            true,
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, identities, true)),
            [
                "mode_attached|d",
                "mode_leaf|d",
                "mode_root|d",
                "mode_sub|d"
            ]
        );
        session.success(
            &mut engine,
            "ALTER TABLE mode_root ALTER COLUMN id SET GENERATED ALWAYS",
            true,
        );
        assert!(engine.checkpoint().unwrap());
        drop(session);
        drop(engine);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        assert_eq!(
            data_rows(&session.success(&mut engine, identities, true)),
            [
                "mode_attached|a",
                "mode_leaf|a",
                "mode_root|a",
                "mode_sub|a"
            ]
        );
        for insert in [
            "INSERT INTO mode_leaf(id,payload) VALUES(902,2)",
            "INSERT INTO mode_attached(id,payload) VALUES(903,102)",
        ] {
            assert_eq!(
                error_sqlstates(&session.execute(&mut engine, insert, true)),
                ["428C9"]
            );
        }
        assert_eq!(data_rows(&session.success(&mut engine,
            "INSERT INTO mode_leaf(payload) VALUES(3) RETURNING id; INSERT INTO mode_attached(payload) VALUES(103) RETURNING id", true)), ["100", "101"]);
        drop(session);
        drop(engine);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        drop(server);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }
}
