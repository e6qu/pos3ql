use super::*;

#[test]
fn captured_view_lifetimes_reject_same_name_slot_reuse() {
    let (mut engine, mut budget) = test_engine();
    assert!(
        error_sqlstates(&run_with(
            &mut engine,
            &mut budget,
            "CREATE VIEW same_view AS SELECT 1 AS id"
        ))
        .is_empty()
    );
    let slot = engine.storage.view_slot("public", "same_view", 0).unwrap();
    let dependency = crate::storage::StoredQueryDependency {
        class: crate::storage::DependencyClass::View,
        slot: slot as u16,
        identity: engine
            .storage
            .capture_stored_query_relation(crate::storage::ResolvedRelation::View(slot), 0)
            .unwrap(),
        referenced_columns: crate::storage::ColumnSet::EMPTY,
        written_columns: crate::storage::ColumnSet::EMPTY,
        schema: crate::storage::SqlName::parse("public").unwrap(),
        name: crate::storage::SqlName::parse("same_view").unwrap(),
        referenced_schema: crate::storage::SqlName::EMPTY,
        referenced_name: crate::storage::SqlName::parse("same_view").unwrap(),
    };
    assert!(error_sqlstates(&run_with(&mut engine, &mut budget, "DROP VIEW same_view")).is_empty());
    assert!(
        error_sqlstates(&run_with(
            &mut engine,
            &mut budget,
            "CREATE VIEW same_view AS SELECT 2 AS id"
        ))
        .is_empty()
    );
    assert_eq!(
        engine.storage.view_slot("public", "same_view", 0),
        Some(slot)
    );
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(
            engine
                .storage
                .resolve_stored_query_relation(&dependency, 0)
                .err()
                .unwrap()
                .sqlstate,
            sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE
        );
    });
}

#[test]
fn stored_relation_lifetimes_follow_private_moves_and_cold_recovery() {
    let mut config = test_config("stored-relation-renames");
    config.object_store_on = true;
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.object_store_bucket = format!("stored-relation-renames-{}", std::process::id());
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    let server = object_reads::HttpObjectServer::start(&mut config);
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE SCHEMA moved_relations; \
         CREATE TABLE original_relation(id integer,payload text); \
         INSERT INTO original_relation VALUES(1,'retained'); \
         CREATE VIEW original_view AS SELECT original_relation.id,original_relation.payload FROM original_relation; \
         CREATE VIEW outer_view AS SELECT * FROM original_view", false);
    let query = "SELECT * FROM outer_view; \
                 SELECT attname FROM pg_attribute WHERE attrelid='outer_view'::regclass \
                   AND attnum>0 ORDER BY attnum";
    for finish in ["ROLLBACK", "COMMIT"] {
        session.success(
            &mut engine,
            "BEGIN; ALTER TABLE original_relation SET SCHEMA moved_relations; \
             ALTER TABLE moved_relations.original_relation RENAME TO current_relation; \
             ALTER VIEW original_view RENAME TO current_view",
            true,
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, query, true)),
            ["1|retained", "id", "payload"]
        );
        session.success(&mut engine, finish, true);
        assert_eq!(
            data_rows(&session.success(&mut engine, query, true)),
            ["1|retained", "id", "payload"]
        );
    }
    drop(session);
    assert!(engine.checkpoint().unwrap());
    drop(engine);
    std::fs::remove_dir_all(&config.data_dir).unwrap();
    let mut recovered_budget = test_engine_budget(&config, 1 << 29);
    let mut recovered = Engine::new(&config, &mut recovered_budget).unwrap();
    let output = run_with(&mut recovered, &mut recovered_budget, query);
    assert!(
        error_sqlstates(&output).is_empty(),
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(data_rows(&output), ["1|retained", "id", "payload"]);
    drop(recovered);
    std::fs::remove_dir_all(&config.data_dir).unwrap();
    drop(server);
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}
