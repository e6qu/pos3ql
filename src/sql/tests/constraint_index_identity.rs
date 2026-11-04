use super::*;

#[test]
fn deferrable_index_replacement_keeps_its_physical_immediacy_and_constraint_timing() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("deferrable-index-identities-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture = include_str!(
            "../../../tests/external/differential/223_deferrable_index_identities.sql"
        );
        let mut rows = Vec::new();
        for statement in fixture
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            rows.extend(data_rows(&session.success(&mut engine, statement, true)));
        }
        assert_eq!(
            rows,
            ["before|f", "after|f|t|t", "2", "ordinary|f|t|t"],
            "{mode:?}"
        );
    }
}

#[test]
fn toast_index_replacement_preserves_virtual_relation_metadata_and_dependencies() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("toast-index-identities-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture =
            include_str!("../../../tests/external/differential/222_toast_index_identities.sql");
        let mut rows = Vec::new();
        for statement in fixture
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            if statement.starts_with("DROP TABLE") {
                let table = engine
                    .storage
                    .find_visible("public", "reindex_toast_identity", 0)
                    .unwrap();
                let table_oid = catalog::user_table_oid(&engine.storage, table);
                let expected = format!(
                    "CREATE UNIQUE INDEX pg_toast_{table_oid}_index ON pg_toast.pg_toast_{table_oid} USING btree (chunk_id, chunk_seq)|chunk_id|chunk_seq|"
                );
                let definition = session.success(
                    &mut engine,
                    "SELECT pg_get_indexdef(i.indexrelid),pg_get_indexdef(i.indexrelid,1,true),\
                     pg_get_indexdef(i.indexrelid,2,true),pg_get_indexdef(i.indexrelid,3,true) \
                     FROM pg_class c JOIN pg_index i ON i.indrelid=c.reltoastrelid \
                     WHERE c.oid='reindex_toast_identity'::regclass",
                    true,
                );
                assert_eq!(data_rows(&definition), [expected], "{mode:?}");
            }
            rows.extend(data_rows(&session.success(&mut engine, statement, true)));
        }
        assert_eq!(
            rows,
            [
                "before|t|t|t|t|t|t|1 2|2|2|0 0|oid_ops,int4_ops",
                "attributes|1|chunk_id|26|f",
                "attributes|2|chunk_seq|23|f",
                "before-dependencies|pg_class|index|0|pg_class|toast|1|a",
                "before-dependencies|pg_class|index|0|pg_class|toast|2|a",
                "before-dependencies|pg_class|toast|0|pg_class|owner|0|i",
                "after|t|t|t|t|t|t|1 2|2|2|0 0|oid_ops,int4_ops|t|t",
                "after-dependencies|pg_class|index|0|pg_class|toast|1|a",
                "after-dependencies|pg_class|index|0|pg_class|toast|2|a",
                "after-dependencies|pg_class|toast|0|pg_class|owner|0|i",
                "t",
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn constraint_identities_survive_replaced_indexes_and_keep_exact_catalog_edges() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("constraint-index-identities-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture = include_str!(
            "../../../tests/external/differential/221_constraint_index_identities.sql"
        );
        let mut rows = Vec::new();
        // The fixture ends statements on their own lines; routine-body
        // semicolons stay inside the same statement.
        for statement in fixture
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            let output = session.success(&mut engine, statement, true);
            rows.extend(data_rows(&output));
        }
        assert_eq!(
            rows,
            [
                "t|t|t",
                "constraint retained|index retained",
                "t",
                "t|named retained",
                "t|t|t",
                "t",
                "t",
                "t",
                "t",
                "t",
                "t",
            ],
            "{mode:?}"
        );
    }
}
