use super::*;

const SETUP: &[&str] = &[
    "CREATE TABLE fka_pk (id int PRIMARY KEY)",
    "INSERT INTO fka_pk VALUES (1)",
    "CREATE TABLE fka (id int, ref int, CONSTRAINT fka_fk FOREIGN KEY (ref) REFERENCES fka_pk) \
     PARTITION BY LIST (id)",
    "CREATE TABLE fka_1 (id int, ref int, CONSTRAINT own_fk FOREIGN KEY (ref) REFERENCES fka_pk)",
    "ALTER TABLE fka ATTACH PARTITION fka_1 FOR VALUES IN (1)",
    "CREATE TABLE fka_rp (id int PRIMARY KEY) PARTITION BY LIST (id)",
    "CREATE TABLE fka_rp_1 PARTITION OF fka_rp FOR VALUES IN (1)",
    "CREATE TABLE fka_rc (x int REFERENCES fka_rp)",
    "CREATE TABLE fka_ne (ref int, CONSTRAINT fka_ne_fk FOREIGN KEY (ref) REFERENCES fka_pk \
     NOT ENFORCED)",
];

const CONSTRAINTS: &str = "SELECT conname, conrelid::regclass, conparentid <> 0 \
     FROM pg_constraint WHERE contype = 'f' ORDER BY oid";
const TRIGGERS: &str = "SELECT c.conname, t.tgrelid::regclass, t.tgfoid::regproc, \
     t.tgparentid <> 0 FROM pg_trigger t JOIN pg_constraint c ON c.oid = t.tgconstraint \
     WHERE c.contype = 'f' ORDER BY t.oid";

const EXPECTED_CONSTRAINTS: &[&str] = &[
    "fka_fk|fka|f",
    "own_fk|fka_1|t",
    "fka_rc_x_fkey|fka_rc|f",
    "fka_rc_x_fkey_1|fka_rc|t",
    "fka_ne_fk|fka_ne|f",
];
const EXPECTED_TRIGGERS: &[&str] = &[
    "fka_fk|fka_pk|\"RI_FKey_noaction_del\"|f",
    "fka_fk|fka_pk|\"RI_FKey_noaction_upd\"|f",
    "fka_fk|fka|\"RI_FKey_check_ins\"|f",
    "fka_fk|fka|\"RI_FKey_check_upd\"|f",
    "own_fk|fka_1|\"RI_FKey_check_ins\"|t",
    "own_fk|fka_1|\"RI_FKey_check_upd\"|t",
    "fka_rc_x_fkey|fka_rp|\"RI_FKey_noaction_del\"|f",
    "fka_rc_x_fkey|fka_rp|\"RI_FKey_noaction_upd\"|f",
    "fka_rc_x_fkey_1|fka_rp_1|\"RI_FKey_noaction_del\"|t",
    "fka_rc_x_fkey_1|fka_rp_1|\"RI_FKey_noaction_upd\"|t",
    "fka_rc_x_fkey|fka_rc|\"RI_FKey_check_ins\"|f",
    "fka_rc_x_fkey|fka_rc|\"RI_FKey_check_upd\"|f",
];

fn recovery_config(label: &str) -> Config {
    let mut config = test_config(&format!("foreign-key-attach-{label}"));
    config.object_store_on = true;
    config.object_store_bucket = format!("foreign-key-attach-{label}-{}", std::process::id());
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.block_cache_bytes = 0;
    config.disk_cache_bytes = 0;
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    config
}

fn verify(engine: &mut Engine, session: &mut ConfiguredTransactionSession, context: &str) {
    assert_eq!(
        data_rows(&session.success(engine, CONSTRAINTS, true)),
        EXPECTED_CONSTRAINTS,
        "{context}"
    );
    assert_eq!(
        data_rows(&session.success(engine, TRIGGERS, true)),
        EXPECTED_TRIGGERS,
        "{context}"
    );
    for (statement, sqlstate) in [
        ("INSERT INTO fka VALUES (1, 9)", "23503"),
        ("ALTER TABLE fka_1 DROP CONSTRAINT own_fk", "42P16"),
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(engine, statement, true)),
            [sqlstate],
            "{context}: {statement}"
        );
    }
    session.success(engine, "INSERT INTO fka_ne VALUES (9)", true);
    session.success(engine, "DELETE FROM fka_ne", true);
}

#[test]
fn an_adopted_foreign_key_and_its_trigger_order_survive_cold_recovery() {
    let mut config = recovery_config("adopt");
    let server = object_reads::HttpObjectServer::start(&mut config);
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    for statement in SETUP {
        session.success(&mut engine, statement, true);
    }
    verify(&mut engine, &mut session, "live");
    engine.commit_wal().unwrap();
    drop((session, engine, budget));
    for checkpoint in [false, true] {
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        let mut cold_budget = test_engine_budget(&config, 1 << 29);
        let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
        let mut verifier = ConfiguredTransactionSession::new(&config, &mut cold_budget);
        verify(
            &mut recovered,
            &mut verifier,
            &format!("checkpoint={checkpoint}"),
        );
        if !checkpoint {
            assert!(recovered.checkpoint().unwrap());
        }
        recovered.commit_wal().unwrap();
        drop((verifier, recovered, cold_budget));
    }
    // Detaching makes the adopted key the partition's own again, with new
    // action triggers after every existing trigger.
    let mut cold_budget = test_engine_budget(&config, 1 << 29);
    let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
    session.success(
        &mut recovered,
        "ALTER TABLE fka DETACH PARTITION fka_1",
        true,
    );
    let rows = data_rows(&session.success(&mut recovered, TRIGGERS, true));
    assert_eq!(
        rows[rows.len() - 2..],
        [
            "own_fk|fka_pk|\"RI_FKey_noaction_del\"|f",
            "own_fk|fka_pk|\"RI_FKey_noaction_upd\"|f"
        ]
    );
    drop((session, recovered, cold_budget, server));
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}
