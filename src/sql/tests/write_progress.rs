//! Writing statements yield on cold storage reads while they select and lock
//! their rows, and restart from their start without repeating an effect.

use super::object_reads::HttpObjectServer;
use super::*;
use std::sync::atomic::Ordering;
use std::time::Instant;

struct ColdTable {
    config: Config,
    server: HttpObjectServer,
    engine: Engine,
    writer: ConfiguredTransactionSession,
    other: ConfiguredTransactionSession,
}

/// `cold_items` (`id`, `code`) holds rows 1..=40 only in saved objects; GET
/// responses wait while `server.hold_response` is set. `setup` runs before
/// the rows are saved.
fn cold_table(label: &str, mode: crate::config::TransactionMode, setup: &str) -> ColdTable {
    let name = format!("write-progress-{label}-{mode:?}-{}", std::process::id()).to_lowercase();
    let mut config = test_config(&name);
    config.transaction_mode = mode;
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = name;
    config.block_cache_bytes = 0;
    config.disk_cache_bytes = 0;
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    let server = HttpObjectServer::start(&mut config);
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
    let other = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    writer.success(
        &mut engine,
        &format!(
            "CREATE TABLE cold_items(id integer PRIMARY KEY, code text); \
             INSERT INTO cold_items SELECT g, 'c' || g FROM generate_series(1, 40) g; {setup}"
        ),
        true,
    );
    assert!(crate::mem::guard::forbid_alloc(|| engine.checkpoint()).unwrap());
    let table = engine.storage.find_table("public", "cold_items").unwrap();
    {
        engine.storage.finish_row_cleanup(
            RowCleanupTask::HeapRows(HeapCleanup::AllSaved),
            RowCleanupTarget::Table(table),
        );
        engine.storage.finish_row_cleanup(
            RowCleanupTask::SavedEntries(EntryCleanup::AllSaved),
            RowCleanupTarget::Table(table),
        );
    }
    engine.enable_async_block_reads();
    ColdTable {
        config,
        server,
        engine,
        writer,
        other,
    }
}

impl ColdTable {
    /// Runs `sql` to completion for `writer`, serving object reads. With
    /// `hold`, the first GET waits while `during_wait` runs on the engine for
    /// another client. Returns the number of waits.
    fn run_held(
        &mut self,
        sql: &str,
        extended: bool,
        hold: bool,
        mut during_wait: impl FnMut(&mut Engine, &mut ConfiguredTransactionSession),
    ) -> usize {
        self.server.hold_response.store(hold, Ordering::Release);
        self.writer.send.clear();
        self.writer.arena.reset();
        let mut waits = 0;
        let mut completed_statements = 0;
        let deadline = crate::test_wait::deadline();
        loop {
            let writer = &mut self.writer;
            let engine = &mut self.engine;
            engine.bind_query_connection(writer.connection_id);
            let status = crate::mem::guard::forbid_alloc(|| {
                for slot in 0..engine.block_read_slots() {
                    engine.advance_pending_block_read(slot).unwrap();
                }
                let mut responder = Responder::new(&mut writer.send);
                if !extended {
                    return engine
                        .execute_simple_from(
                            sql,
                            completed_statements,
                            &writer.arena,
                            &mut writer.transaction,
                            &mut writer.pool,
                            &mut writer.cursors,
                            &writer.guc,
                            &mut responder,
                            writer.connection_id,
                            false,
                        )
                        .unwrap();
                }
                match engine
                    .execute_extended(
                        sql,
                        &writer.arena,
                        &[],
                        &[],
                        &mut writer.transaction,
                        &mut writer.pool,
                        &mut writer.cursors,
                        &writer.guc,
                        &mut responder,
                        writer.connection_id,
                        false,
                    )
                    .unwrap()
                {
                    ExtendedExecutionStatus::Complete(_) => ExecutionStatus::Complete,
                    ExtendedExecutionStatus::Blocked { wait } => ExecutionStatus::Blocked {
                        completed_statements: 0,
                        output_mark: responder.mark(),
                        wait,
                    },
                }
            });
            let ExecutionStatus::Blocked {
                completed_statements: completed,
                output_mark,
                wait,
            } = status
            else {
                break;
            };
            assert_eq!(wait, StatementWait::ObjectIo, "{sql}");
            output_mark.truncate_unsent(&mut self.writer.send);
            self.writer.arena.recycle_front();
            completed_statements = completed;
            if waits == 0 {
                let deadline = crate::test_wait::deadline();
                while self.server.requests.load(Ordering::Acquire) == 0 {
                    assert!(Instant::now() < deadline, "cold GET was not issued");
                    std::thread::yield_now();
                }
                during_wait(&mut self.engine, &mut self.other);
                self.server.hold_response.store(false, Ordering::Release);
            }
            waits += 1;
            assert!(Instant::now() < deadline, "{sql}: write did not finish");
            std::thread::yield_now();
        }
        self.server.hold_response.store(false, Ordering::Release);
        waits
    }

    fn run(
        &mut self,
        sql: &str,
        extended: bool,
        during_wait: impl FnMut(&mut Engine, &mut ConfiguredTransactionSession),
    ) -> usize {
        self.run_held(sql, extended, true, during_wait)
    }

    fn rows(&mut self, sql: &str) -> Vec<String> {
        self.run(sql, false, |_, _| {});
        let output = self.writer.send.readable().to_vec();
        assert!(
            error_sqlstates(&output).is_empty(),
            "{sql}: {}",
            String::from_utf8_lossy(&output)
        );
        data_rows(&output)
    }

    fn finish(self) {
        let ColdTable {
            config,
            server,
            engine,
            writer,
            other,
        } = self;
        drop((other, writer, engine, server));
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

/// Another client runs while the write waits: a query and a write to a table
/// the waiting statement does not lock.
fn other_client_progresses(engine: &mut Engine, other: &mut ConfiguredTransactionSession) {
    assert_eq!(
        data_rows(&other.success(engine, "SELECT 40 + 2", true)),
        ["42"]
    );
    other.success(engine, "INSERT INTO hot_log VALUES ('other')", true);
}

/// Single-table `UPDATE`, `DELETE`, `INSERT ... SELECT` and `MERGE` over
/// cold rows yield the worker while their source scan waits for a delayed
/// GET, let another client run, and finish with PostgreSQL's results and
/// `RETURNING` rows once: both transaction modes, both protocols, implicit
/// and explicit transactions.
#[test]
fn writes_yield_during_source_scans() {
    /// Label, statement, `RETURNING` rows, check query and its rows.
    type Case = (
        &'static str,
        &'static str,
        &'static [&'static str],
        &'static str,
        &'static [&'static str],
    );
    let cases: &[Case] = &[
        (
            "update",
            "UPDATE cold_items SET code = code || '!' WHERE id % 4 = 0 RETURNING id",
            &["4", "8", "12", "16", "20", "24", "28", "32", "36", "40"],
            "SELECT count(*) FILTER (WHERE code LIKE '%!'), count(*) FROM cold_items",
            &["10|40"],
        ),
        (
            "delete",
            "DELETE FROM cold_items WHERE id > 37 RETURNING code",
            &["c38", "c39", "c40"],
            "SELECT count(*), max(id) FROM cold_items",
            &["37|37"],
        ),
        (
            "insert",
            "INSERT INTO cold_copy SELECT id, code FROM cold_items WHERE id <= 3 RETURNING id",
            &["1", "2", "3"],
            "SELECT count(*), sum(id) FROM cold_copy",
            &["3|6"],
        ),
        (
            "merge",
            "MERGE INTO cold_copy c USING cold_items i ON c.id = i.id \
             WHEN NOT MATCHED AND i.id > 38 THEN INSERT VALUES (i.id, i.code)",
            &[],
            "SELECT count(*), sum(id) FROM cold_copy",
            &["2|79"],
        ),
    ];
    for &(label, sql, returned, check, expected) in cases {
        for mode in [
            crate::config::TransactionMode::Mvcc,
            crate::config::TransactionMode::Locking,
        ] {
            for (extended, explicit) in [(false, false), (true, false), (false, true)] {
                let mut cold = cold_table(
                    &format!("{label}-{extended}-{explicit}"),
                    mode,
                    "CREATE TABLE cold_copy(id integer, code text); \
                     CREATE TABLE hot_log(stage text)",
                );
                if explicit {
                    cold.writer.success(&mut cold.engine, "BEGIN", true);
                }
                let waits = cold.run(sql, extended, other_client_progresses);
                let context = format!("{label} {mode:?} extended={extended} explicit={explicit}");
                assert!(waits > 0, "{context}: the source scan must wait");
                let output = cold.writer.send.readable().to_vec();
                assert!(
                    error_sqlstates(&output).is_empty(),
                    "{context}: {}",
                    String::from_utf8_lossy(&output)
                );
                let mut rows = data_rows(&output);
                rows.sort();
                let mut returned = returned.to_vec();
                returned.sort();
                assert_eq!(rows, returned, "{context}");
                if explicit {
                    cold.writer.success(&mut cold.engine, "COMMIT", true);
                }
                assert_eq!(cold.rows(check), expected, "{context}");
                assert_eq!(
                    cold.rows("SELECT count(*) FROM hot_log"),
                    ["1"],
                    "{context}"
                );
                cold.finish();
            }
        }
    }
}

/// Triggers and sequence defaults run once: row triggers, defaults and
/// transition tables run after the scan's waits, while a BEFORE STATEMENT
/// trigger runs before the scan and makes it read synchronously. Each audit row
/// and sequence value appears once, as in PostgreSQL 18.6.
#[test]
fn write_waits_keep_trigger_and_sequence_effects_once() {
    for (label, statement_trigger) in [("row", false), ("statement", true)] {
        for mode in [
            crate::config::TransactionMode::Mvcc,
            crate::config::TransactionMode::Locking,
        ] {
            let statement_setup = if statement_trigger {
                "CREATE FUNCTION audit_statement() RETURNS trigger LANGUAGE plpgsql AS $$ \
                 BEGIN INSERT INTO audit VALUES ('statement', nextval('audit_numbers')); \
                 RETURN NULL; END$$; \
                 CREATE TRIGGER audit_statement BEFORE UPDATE ON cold_items \
                 FOR EACH STATEMENT EXECUTE FUNCTION audit_statement();"
            } else {
                ""
            };
            let mut cold = cold_table(
                &format!("triggers-{label}"),
                mode,
                &format!(
                    "CREATE TABLE hot_log(stage text); \
                     CREATE SEQUENCE audit_numbers; \
                     CREATE TABLE audit(stage text, n bigint DEFAULT nextval('audit_numbers')); \
                     CREATE FUNCTION audit_row() RETURNS trigger LANGUAGE plpgsql AS $$ \
                     BEGIN INSERT INTO audit VALUES ('row-' || NEW.id, nextval('audit_numbers')); \
                     NEW.code := NEW.code || '+'; RETURN NEW; END$$; \
                     CREATE TRIGGER audit_row BEFORE UPDATE ON cold_items \
                     FOR EACH ROW EXECUTE FUNCTION audit_row(); \
                     CREATE FUNCTION audit_changed() RETURNS trigger LANGUAGE plpgsql AS $$ \
                     BEGIN INSERT INTO audit(stage) \
                     SELECT 'changed-' || count(*) FROM changed; RETURN NULL; END$$; \
                     CREATE TRIGGER audit_changed AFTER UPDATE ON cold_items \
                     REFERENCING NEW TABLE AS changed \
                     FOR EACH STATEMENT EXECUTE FUNCTION audit_changed(); {statement_setup}"
                ),
            );
            // A BEFORE STATEMENT trigger is an effect before the scan, so the
            // scan blocks; its GET is not held.
            let waits = cold.run_held(
                "UPDATE cold_items SET code = 'u' WHERE id IN (5, 6)",
                false,
                !statement_trigger,
                other_client_progresses,
            );
            let context = format!("{label} {mode:?}");
            assert_eq!(waits > 0, !statement_trigger, "{context}");
            let output = cold.writer.send.readable().to_vec();
            assert!(
                error_sqlstates(&output).is_empty(),
                "{context}: {}",
                String::from_utf8_lossy(&output)
            );
            assert_eq!(command_tags(&output), ["UPDATE 2"], "{context}");
            // Without ORDER BY: a sort would spill to the unserved store.
            let audit = if statement_trigger {
                "1|2|3|4|4"
            } else {
                "NULL|1|2|3|3"
            };
            assert_eq!(
                cold.rows(
                    "SELECT max(n) FILTER (WHERE stage = 'statement'), \
                     max(n) FILTER (WHERE stage = 'row-5'), \
                     max(n) FILTER (WHERE stage = 'row-6'), \
                     max(n) FILTER (WHERE stage = 'changed-2'), count(*) FROM audit"
                ),
                [audit],
                "{context}"
            );
            assert_eq!(
                cold.rows("SELECT string_agg(code, ',') FROM cold_items WHERE id IN (5, 6)"),
                ["u+,u+"],
                "{context}"
            );
            cold.finish();
        }
    }
}

/// A PL/pgSQL procedure's `UPDATE` waits for its cold source scan after the
/// procedure's own `COMMIT`, lets another client run, and resumes at that
/// statement: the committed insert and the update each happen once.
#[test]
fn procedure_writes_yield_during_source_scans() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut cold = cold_table(
            "procedure",
            mode,
            "CREATE TABLE hot_log(stage text); \
             CREATE PROCEDURE cold_update() LANGUAGE plpgsql AS $$ BEGIN \
             INSERT INTO hot_log VALUES ('before'); COMMIT; \
             UPDATE cold_items SET code = 'p' WHERE id = 7; \
             INSERT INTO hot_log VALUES ('after'); END$$",
        );
        let waits = cold.run("CALL cold_update()", false, |engine, other| {
            assert_eq!(
                data_rows(&other.success(engine, "SELECT stage FROM hot_log", true)),
                ["before"]
            );
        });
        assert!(waits > 0, "{mode:?}: the procedure's update must wait");
        let output = cold.writer.send.readable().to_vec();
        assert!(
            error_sqlstates(&output).is_empty(),
            "{mode:?}: {}",
            String::from_utf8_lossy(&output)
        );
        assert_eq!(command_tags(&output), ["CALL"]);
        assert_eq!(
            cold.rows("SELECT count(*) FILTER (WHERE stage = 'before'), count(*) FROM hot_log"),
            ["1|2"]
        );
        assert_eq!(cold.rows("SELECT code FROM cold_items WHERE id = 7"), ["p"]);
        cold.finish();
    }
}
