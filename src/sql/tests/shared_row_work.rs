use super::*;

fn assert_idle(engine: &Engine, bytes: usize) {
    assert!(
        engine
            .dml_scratch
            .slots
            .iter()
            .all(|lease| lease.capacity() == 0)
    );
    crate::mem::guard::forbid_alloc(|| {
        assert!(engine.row_work_pool().lease::<u8>(bytes).is_ok());
    });
}

#[test]
fn foreground_write_leases_release_on_success_sql_error_and_wire_exhaustion() {
    let mut config = test_config("shared-row-work-release");
    config.row_work_bytes = 4096;
    config.row_work_leases = 16;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE shared_release(id integer PRIMARY KEY, amount integer); INSERT INTO shared_release VALUES(1,10),(2,20),(3,30)", true);
    session.success(
        &mut engine,
        "UPDATE shared_release SET amount = amount + 1",
        true,
    );
    assert_idle(&engine, config.row_work_bytes);
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "UPDATE shared_release SET amount = 1 / (id - id)",
            true
        )),
        ["22012"]
    );
    assert_idle(&engine, config.row_work_bytes);
    let held = engine
        .row_work_pool()
        .lease::<u8>(config.row_work_bytes)
        .unwrap();
    let output = session.execute(&mut engine, "DELETE FROM shared_release", true);
    assert_eq!(error_sqlstates(&output), ["54000"]);
    assert!(String::from_utf8_lossy(&output).contains("row work pool exhausted"));
    drop(held);
    assert_idle(&engine, config.row_work_bytes);
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT sum(amount) FROM shared_release", true)),
        ["63"]
    );

    let mut empty_output =
        crate::mem::FixedBuf::new(&mut budget, "exhausted DML response", 0).unwrap();
    let mut responder = Responder::new(&mut empty_output);
    session.arena.reset();
    let result = crate::mem::guard::forbid_alloc(|| {
        engine.execute_simple(
            "UPDATE shared_release SET amount = amount + 1 RETURNING id",
            &session.arena,
            &mut session.transaction,
            &mut session.pool,
            &mut session.cursors,
            &mut session.guc,
            &mut responder,
            session.connection_id,
        )
    });
    assert!(result.is_err());
    assert_idle(&engine, config.row_work_bytes);
    engine.rollback_txn(&mut session.transaction, &session.guc);
    session.success(&mut engine, "DELETE FROM shared_release WHERE id = 1", true);
    assert_idle(&engine, config.row_work_bytes);
}

#[test]
fn nested_trigger_updates_restore_outer_selection_after_success_and_caught_error() {
    let mut config = test_config("shared-row-work-nested");
    config.row_work_bytes = 4096;
    config.row_work_leases = 32;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE shared_outer(id integer PRIMARY KEY, amount integer); \
         CREATE TABLE shared_inner(id integer PRIMARY KEY, amount integer); \
         INSERT INTO shared_outer VALUES(1,10),(2,20),(3,30); \
         INSERT INTO shared_inner VALUES(1,0),(2,0),(3,0); \
         CREATE FUNCTION shared_nested_write() RETURNS trigger LANGUAGE plpgsql AS $$ \
           BEGIN \
             BEGIN UPDATE shared_inner SET amount = 1 / (id - id); \
               EXCEPTION WHEN division_by_zero THEN NULL; END; \
             UPDATE shared_inner SET amount = amount + 1 WHERE id = NEW.id; \
             EXECUTE 'UPDATE shared_inner SET amount = amount + 1 WHERE id = $1' USING NEW.id; \
             RETURN NEW; \
           END $$; \
         CREATE TRIGGER shared_nested_write BEFORE UPDATE ON shared_outer \
           FOR EACH ROW EXECUTE FUNCTION shared_nested_write()",
        true,
    );
    session.success(
        &mut engine,
        "UPDATE shared_outer SET amount = amount + 1",
        true,
    );
    assert_idle(&engine, config.row_work_bytes);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT id, amount FROM shared_outer ORDER BY id; SELECT id, amount FROM shared_inner ORDER BY id", true)),
        ["1|11", "2|21", "3|31", "1|2", "2|2", "3|2"]);
}

#[test]
fn view_write_staging_uses_actual_rows_with_large_pool_and_small_work_arena() {
    let mut config = test_config("shared-row-work-view");
    config.row_work_bytes = 8 << 20;
    config.row_work_leases = 32;
    config.work_arena_bytes = 256 << 10;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE TABLE shared_view_base(id integer PRIMARY KEY, amount integer); \
         CREATE SEQUENCE shared_view_calls; \
         CREATE VIEW shared_view AS SELECT id, amount FROM shared_view_base; \
         CREATE FUNCTION shared_view_write() RETURNS trigger LANGUAGE plpgsql AS $$ \
           BEGIN \
             IF TG_OP = 'INSERT' THEN INSERT INTO shared_view_base VALUES(NEW.id, NEW.amount); RETURN NEW; END IF; \
             IF TG_OP = 'UPDATE' THEN UPDATE shared_view_base SET amount = NEW.amount WHERE id = OLD.id; RETURN NEW; END IF; \
             DELETE FROM shared_view_base WHERE id = OLD.id; RETURN OLD; \
           END $$; \
         CREATE TRIGGER shared_view_write INSTEAD OF INSERT OR UPDATE OR DELETE ON shared_view \
           FOR EACH ROW EXECUTE FUNCTION shared_view_write()", true);
    assert_eq!(data_rows(&session.success(&mut engine,
        "INSERT INTO shared_view SELECT nextval('shared_view_calls')::integer, 10 RETURNING id, amount; \
         UPDATE shared_view SET amount = amount + nextval('shared_view_calls')::integer WHERE id = 1 RETURNING id, amount; \
         DELETE FROM shared_view WHERE id = nextval('shared_view_calls') - 2 RETURNING id; \
         SELECT currval('shared_view_calls'), count(*) FROM shared_view_base", true)),
        ["1|10", "1|12", "1", "3|0"]);
    assert_idle(&engine, config.row_work_bytes);
}

#[test]
fn subscription_workers_share_foreground_admission_without_reserving_payload() {
    use crate::pg::subscription_apply::SubscriptionApply;

    let mut config = test_config("shared-row-work-subscription");
    config.row_work_bytes = 4096;
    config.row_work_leases = 32;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE TABLE shared_worker_source(id integer); \
         CREATE TABLE shared_worker_effect(id integer, amount integer); \
         INSERT INTO shared_worker_effect VALUES(1,0); \
         CREATE FUNCTION shared_worker_write() RETURNS trigger LANGUAGE plpgsql AS $$ \
           BEGIN UPDATE shared_worker_effect SET amount = amount + 1 WHERE id = NEW.id; RETURN NEW; END $$; \
         CREATE TRIGGER shared_worker_write BEFORE INSERT ON shared_worker_source \
           FOR EACH ROW EXECUTE FUNCTION shared_worker_write(); \
         ALTER TABLE shared_worker_source ENABLE REPLICA TRIGGER shared_worker_write; \
         CREATE SUBSCRIPTION shared_worker CONNECTION \
           'host=127.0.0.1 port=5432 user=repl dbname=publisher sslmode=disable' \
           PUBLICATION changes WITH(connect=false, slot_name=NONE)", true);
    let before = budget.used();
    let mut worker = SubscriptionApply::new(
        &mut budget,
        engine.subscription_stream("shared_worker").unwrap(),
        &config,
        0,
        crate::storage::SubscriptionBehavior::POSTGRESQL_18_DEFAULT,
        std::sync::Arc::clone(engine.row_work_pool()),
    )
    .unwrap();
    assert_eq!(
        budget.used() - before,
        SubscriptionApply::budget_bytes(&config)
    );
    let mut larger_pool = config.clone();
    larger_pool.row_work_bytes *= 2;
    assert_eq!(
        SubscriptionApply::budget_bytes(&config),
        SubscriptionApply::budget_bytes(&larger_pool)
    );
    let schema = SqlName::parse("public").unwrap();
    let table = SqlName::parse("shared_worker_source").unwrap();
    let columns = [SqlName::parse("id").unwrap()];
    crate::mem::guard::forbid_alloc(|| {
        worker.begin_bootstrap(&mut engine).unwrap();
        worker
            .register_bootstrap_relation(&mut engine, schema, table)
            .unwrap();
        let setup = worker
            .start_copy_table(&mut engine, schema, table, &columns)
            .unwrap();
        let held = engine
            .row_work_pool()
            .lease::<u8>(config.row_work_bytes)
            .unwrap();
        let error = worker.copy_line(&mut engine, &setup, b"1").unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(error.message.as_str().contains("row work pool exhausted"));
        worker.stop(&mut engine);
        drop(held);
    });
    assert_idle(&engine, config.row_work_bytes);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT (SELECT count(*) FROM shared_worker_source), (SELECT amount FROM shared_worker_effect)", true)), ["0|0"]);
    crate::mem::guard::forbid_alloc(|| {
        worker.begin_bootstrap(&mut engine).unwrap();
        worker
            .register_bootstrap_relation(&mut engine, schema, table)
            .unwrap();
        let setup = worker
            .start_copy_table(&mut engine, schema, table, &columns)
            .unwrap();
        worker.copy_line(&mut engine, &setup, b"1").unwrap();
        worker.finish_copy_table(&mut engine, &setup).unwrap();
        worker.finish_bootstrap(&mut engine, 41).unwrap();
    });
    assert_idle(&engine, config.row_work_bytes);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT (SELECT count(*) FROM shared_worker_source), (SELECT amount FROM shared_worker_effect)", true)), ["1|1"]);
}
