//! Renames rewrite stored expression text; recovery must rebuild the same text
//! from the journal alone and from a checkpoint.

use super::*;

const SETUP: &str = r#"
CREATE SCHEMA rw;
CREATE FUNCTION rw.f(integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT $1 + 1';
CREATE DOMAIN rw.d AS integer CHECK (VALUE > rw.f(1));
CREATE TABLE rw.t (
  k integer PRIMARY KEY,
  v integer DEFAULT rw.f(0) CHECK (rw.f(v) > 0),
  w integer CHECK (w >= 0),
  g integer GENERATED ALWAYS AS (rw.f(k)) STORED,
  dd rw.d
);
CREATE INDEX t_expr ON rw.t (rw.f(v)) WHERE rw.f(k) > 0;
CREATE FUNCTION rw.trg() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$;
CREATE TRIGGER t_when BEFORE UPDATE ON rw.t FOR EACH ROW WHEN (rw.f(NEW.w) > OLD.w) EXECUTE FUNCTION rw.trg();
CREATE POLICY t_pol ON rw.t USING (rw.f(w) > 0);
CREATE STATISTICS rw.t_stat ON rw.f(w), v FROM rw.t;
CREATE RULE t_rule AS ON DELETE TO rw.t WHERE rw.f(OLD.w) > 100 DO INSTEAD NOTHING;
INSERT INTO rw.t (k, v, w, dd) VALUES (1, 5, 1, 3), (2, 6, 200, 4);
ALTER SCHEMA rw RENAME TO rw2;
ALTER TABLE rw2.t RENAME COLUMN w TO w2;
ALTER TABLE rw2.t RENAME COLUMN v TO v2;
CREATE INDEX t_qual ON rw2.t ((t.k + 1));
ALTER TABLE rw2.t RENAME TO t2;"#;

const PROBE: &str = "
SELECT indexdef FROM pg_indexes WHERE schemaname = 'rw2' ORDER BY indexname;
SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'rw2.t2'::regclass AND contype = 'c' ORDER BY conname;
SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE contypid = 'rw2.d'::regtype;
SELECT attname, pg_get_expr(adbin, adrelid) FROM pg_attrdef JOIN pg_attribute ON attrelid = adrelid AND attnum = adnum WHERE adrelid = 'rw2.t2'::regclass ORDER BY adnum;
SELECT pg_get_triggerdef(oid) FROM pg_trigger WHERE tgname = 't_when';
SELECT pg_get_statisticsobjdef(oid) FROM pg_statistic_ext WHERE stxname = 't_stat';
SELECT pg_get_ruledef(oid) FROM pg_rewrite WHERE rulename = 't_rule';
SELECT k FROM rw2.t2 WHERE rw2.f(v2) = 7 AND rw2.f(k) > 0;";

const EXPECTED: [&str; 14] = [
    "CREATE INDEX t_expr ON rw2.t2 USING btree (rw2.f(v2)) WHERE (rw2.f(k) > 0)",
    "CREATE UNIQUE INDEX t_pkey ON rw2.t2 USING btree (k)",
    "CREATE INDEX t_qual ON rw2.t2 USING btree (((k + 1)))",
    "CHECK ((rw2.f(v2) > 0))",
    "CHECK ((w2 >= 0))",
    "CHECK ((VALUE > rw2.f(1)))",
    "v2|rw2.f(0)",
    "g|rw2.f(k)",
    "CREATE TRIGGER t_when BEFORE UPDATE ON rw2.t2 FOR EACH ROW WHEN ((rw2.f(new.w2) > old.w2)) EXECUTE FUNCTION rw2.trg()",
    "CREATE STATISTICS rw2.t_stat ON v2, rw2.f(w2) FROM rw2.t2",
    "CREATE RULE t_rule AS\n    ON DELETE TO rw2.t2\n   WHERE (rw2.f(old.w2) > 100) DO INSTEAD NOTHING;",
    "2",
    "1|5|1",
    // The rule's condition keeps the second row from being deleted.
    "2|6|200",
];

#[test]
fn renamed_stored_expressions_survive_journal_and_checkpoint_recovery() {
    for checkpoint in [false, true] {
        for single_transaction in [false, true] {
            for mode in [
                crate::config::TransactionMode::Mvcc,
                crate::config::TransactionMode::Locking,
            ] {
                let name =
                    format!("renamed-expressions-{checkpoint}-{single_transaction}-{mode:?}");
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
                let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
                if single_transaction {
                    session.success(&mut engine, SETUP, false);
                } else {
                    for statement in SETUP
                        .split(
                            ";
",
                        )
                        .filter(|sql| !sql.trim().is_empty())
                    {
                        session.success(&mut engine, statement, false);
                    }
                }
                let probe = |session: &mut ConfiguredTransactionSession, engine: &mut Engine| {
                    let mut rows = data_rows(&session.success(engine, PROBE, false));
                    // Rule and trigger conditions fire after recovery.
                    session.success(
                        engine,
                        "UPDATE rw2.t2 SET w2 = w2 WHERE k = 1; DELETE FROM rw2.t2 WHERE k = 2",
                        false,
                    );
                    rows.extend(data_rows(&session.success(
                        engine,
                        "SELECT k, v2, w2 FROM rw2.t2 ORDER BY k",
                        false,
                    )));
                    rows
                };
                assert_eq!(probe(&mut session, &mut engine), EXPECTED, "{name}");
                if checkpoint {
                    assert!(engine.checkpoint().unwrap());
                }
                drop(session);
                drop(engine);
                std::fs::remove_dir_all(&config.data_dir).unwrap();
                let mut cold_budget = test_engine_budget(&config, 1 << 29);
                let mut cold = Engine::new(&config, &mut cold_budget).unwrap();
                let mut session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
                assert_eq!(probe(&mut session, &mut cold), EXPECTED, "{name}");
                drop(session);
                drop(cold);
                crate::object_store::sim::drop_namespace(&config.object_store_bucket);
                std::fs::remove_dir_all(&config.data_dir).unwrap();
            }
        }
    }
}
