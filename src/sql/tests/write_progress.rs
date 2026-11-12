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
    /// Waits during which the writer's transaction kept the write's rows.
    kept_waits: usize,
    /// Waits during which it kept the write's finished AFTER work.
    completion_waits: usize,
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
    config.max_tables = 16;
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
    // `cold_items` and the other `cold_` tables a setup may create.
    for name in [
        "cold_items",
        "cold_more",
        "cold_extra",
        "cold_gone",
        "cold_pairs",
        "cold_updates",
        "cold_deletes",
    ] {
        let Some(table) = engine.storage.find_table("public", name) else {
            continue;
        };
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
        kept_waits: 0,
        completion_waits: 0,
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
            self.kept_waits += usize::from(self.writer.transaction.has_retained_write());
            self.completion_waits +=
                usize::from(self.writer.transaction.has_pending_write_completion());
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
            ..
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

/// A `DO` body's `UPDATE` and dynamic `INSERT` call a PL/pgSQL function: the
/// cold scan yields, each call runs once with the engine, and the write
/// replays its result, as in PostgreSQL 18.6.
#[test]
fn procedural_writes_call_procedural_functions_after_waits() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for extended in [false, true] {
            let mut cold = cold_table(
                "procedural-calls",
                mode,
                "CREATE TABLE hot_log(stage text); \
                 CREATE FUNCTION tag(value text) RETURNS text LANGUAGE plpgsql AS $$ BEGIN \
                 INSERT INTO hot_log VALUES ('tag-' || value); RETURN value || '!'; END$$",
            );
            let waits = cold.run(
                "DO $$ BEGIN UPDATE cold_items SET code = tag(code) WHERE id = 7; \
                 EXECUTE 'INSERT INTO hot_log SELECT tag(code) FROM cold_items WHERE id = $1' \
                 USING 7; END$$",
                extended,
                other_client_progresses,
            );
            let context = format!("{mode:?} extended={extended}");
            assert!(waits > 0, "{context}: the scan must wait");
            let output = cold.writer.send.readable().to_vec();
            assert!(
                error_sqlstates(&output).is_empty(),
                "{context}: {}",
                String::from_utf8_lossy(&output)
            );
            assert_eq!(
                cold.rows("SELECT code FROM cold_items WHERE id = 7"),
                ["c7!"]
            );
            assert_eq!(
                cold.rows("SELECT string_agg(stage, ',' ORDER BY stage) FROM hot_log"),
                ["c7!!,other,tag-c7,tag-c7!"],
                "{context}"
            );
            cold.finish();
        }
    }
}

/// Routine calls in a write's rows, after its cold scan yields: each call
/// sees the rows the write already changed, runs once, and the write keeps
/// its `RETURNING` rows and sequence order, at top level, in `DO`, dynamic
/// `EXECUTE` and SQL-language bodies; both protocols and transaction modes.
/// Expected values from PostgreSQL 18.6.
#[test]
fn write_rows_keep_progress_across_routine_calls() {
    // Statement, whether it may yield before its first effect, and its
    // returned rows. `VALUES` rows and a top-level call start with an effect;
    // after the first statement, its transaction's own rows can be cached.
    let statements: &[(&str, bool, &[&str])] = &[
        (
            "UPDATE cold_items SET code = code || mark('u') WHERE id <= 3 RETURNING id, code",
            true,
            &["1|c1!", "2|c2!", "3|c3!"],
        ),
        (
            "DELETE FROM cold_items WHERE id > 38 RETURNING id, mark('d')",
            true,
            &["39|!", "40|!"],
        ),
        (
            "INSERT INTO cold_items VALUES (41, 'n' || mark('i')), (42, 'n' || mark('i')) \
             RETURNING id, code",
            false,
            &["41|n!", "42|n!"],
        ),
        (
            "DO $$BEGIN UPDATE cold_items SET code = code || mark('p') WHERE id BETWEEN 5 AND 6; \
             EXECUTE 'UPDATE cold_items SET code = code || mark($1) WHERE id BETWEEN 7 AND 8' \
             USING 'e'; END$$",
            true,
            &[],
        ),
        ("SELECT bump()", false, &["c10!", "c11!"]),
    ];
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for extended in [false, true] {
            let mut cold = cold_table(
                "row-progress",
                mode,
                "CREATE TABLE hot_log(stage text); CREATE SEQUENCE marks; \
                 CREATE FUNCTION mark(label text) RETURNS text LANGUAGE plpgsql AS $$ \
                 DECLARE changed bigint; total bigint; BEGIN \
                 SELECT count(*) FILTER (WHERE code LIKE '%!'), count(*) INTO changed, total \
                 FROM cold_items; \
                 INSERT INTO hot_log VALUES (label || changed || '/' || total || '#' || \
                 nextval('marks')); RETURN '!'; END$$; \
                 CREATE FUNCTION bump() RETURNS SETOF text LANGUAGE sql AS $$UPDATE cold_items \
                 SET code = code || mark('s') WHERE id BETWEEN 10 AND 11 RETURNING code$$",
            );
            cold.writer.success(&mut cold.engine, "BEGIN", true);
            for &(sql, yields, returned) in statements {
                let context = format!("{mode:?} extended={extended}: {sql}");
                let waits = cold.run_held(sql, extended, yields, other_client_progresses);
                if !yields || sql.starts_with("UPDATE") {
                    assert_eq!(waits > 0, yields, "{context}");
                }
                let output = cold.writer.send.readable().to_vec();
                assert!(
                    error_sqlstates(&output).is_empty(),
                    "{context}: {}",
                    String::from_utf8_lossy(&output)
                );
                assert_eq!(data_rows(&output), returned, "{context}");
            }
            cold.writer.success(&mut cold.engine, "COMMIT", true);
            let mut stages = cold.rows("SELECT stage FROM hot_log WHERE stage <> 'other'");
            stages.sort();
            assert_eq!(
                stages,
                [
                    "d3/38#5",
                    "d3/39#4",
                    "e7/40#10",
                    "e8/40#11",
                    "i3/38#6",
                    "i4/39#7",
                    "p5/40#8",
                    "p6/40#9",
                    "s10/40#13",
                    "s9/40#12",
                    "u0/40#1",
                    "u1/40#2",
                    "u2/40#3",
                ],
                "{mode:?} extended={extended}"
            );
            assert_eq!(
                cold.rows(
                    "SELECT count(*) FILTER (WHERE code LIKE '%!'), count(*) FROM cold_items"
                ),
                ["11|40"],
                "{mode:?} extended={extended}"
            );
            cold.finish();
        }
    }
}

/// Orders whose rows check foreign keys against three cold tables, with
/// BEFORE and AFTER row triggers that write and call a sequence, and
/// transition tables.
const ORDER_SETUP: &str = "CREATE TABLE hot_log(stage text); CREATE SEQUENCE numbers; \
     CREATE TABLE audit(stage text, n bigint); \
     CREATE TABLE cold_more(id integer PRIMARY KEY); \
     INSERT INTO cold_more SELECT generate_series(1, 40); \
     CREATE TABLE cold_extra(id integer PRIMARY KEY); \
     INSERT INTO cold_extra SELECT generate_series(1, 40); \
     CREATE TABLE orders(n bigint DEFAULT nextval('numbers'), \
     item integer REFERENCES cold_items, more integer REFERENCES cold_more, \
     extra integer REFERENCES cold_extra, note text); \
     CREATE FUNCTION audit_order() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
     INSERT INTO audit VALUES (TG_OP || '-' || NEW.note, nextval('numbers')); \
     NEW.note := NEW.note || '+'; RETURN NEW; END$$; \
     CREATE TRIGGER audit_order BEFORE INSERT OR UPDATE ON orders \
     FOR EACH ROW EXECUTE FUNCTION audit_order(); \
     CREATE FUNCTION after_order() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
     INSERT INTO audit VALUES ('after-' || NEW.note, nextval('numbers')); \
     RETURN NULL; END$$; \
     CREATE TRIGGER after_order AFTER INSERT OR UPDATE ON orders \
     FOR EACH ROW EXECUTE FUNCTION after_order(); \
     CREATE FUNCTION changed_orders() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
     INSERT INTO audit SELECT TG_OP || '-changed-' || count(*), nextval('numbers') \
     FROM changed; RETURN NULL; END$$; \
     CREATE TRIGGER changed_inserts AFTER INSERT ON orders \
     REFERENCING NEW TABLE AS changed \
     FOR EACH STATEMENT EXECUTE FUNCTION changed_orders(); \
     CREATE TRIGGER changed_updates AFTER UPDATE ON orders \
     REFERENCING NEW TABLE AS changed \
     FOR EACH STATEMENT EXECUTE FUNCTION changed_orders()";

/// Storage waits inside a write's rows, several rows in: a foreign key check
/// of a later row reads the cold parent table. The write keeps its earlier
/// rows, lets another client run, and continues at the waiting row: sequence
/// defaults, BEFORE ROW trigger bodies with their own writes and sequence
/// calls, AFTER ROW triggers, transition tables of AFTER STATEMENT triggers
/// and `RETURNING` rows each happen once, with PostgreSQL 18.6's
/// values; both transaction modes, both protocols, implicit and explicit
/// transactions.
#[test]
fn write_rows_resume_after_storage_waits() {
    let statements: &[(&str, &[&str])] = &[
        (
            "INSERT INTO orders(item, note) SELECT CASE WHEN g >= 4 THEN g END, 'o' || g \
             FROM generate_series(1, 6) g RETURNING n, item, note",
            &[
                "1|NULL|o1+",
                "3|NULL|o2+",
                "5|NULL|o3+",
                "7|4|o4+",
                "9|5|o5+",
                "11|6|o6+",
            ],
        ),
        (
            "UPDATE orders SET more = CASE WHEN n > 3 THEN n + 1 END, note = 'u' \
             WHERE item IS NULL RETURNING n, more, note",
            &["1|NULL|u+", "3|NULL|u+", "5|6|u+"],
        ),
        (
            "INSERT INTO orders(extra, note) VALUES (NULL, 'v1'), (NULL, 'v2'), (8, 'v3'), \
             (9, 'v4') RETURNING n, extra, note",
            &["27|NULL|v1+", "29|NULL|v2+", "31|8|v3+", "33|9|v4+"],
        ),
    ];
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for (extended, explicit) in [(false, false), (true, false), (false, true)] {
            let mut cold = cold_table(
                &format!("row-waits-{extended}-{explicit}"),
                mode,
                ORDER_SETUP,
            );
            if explicit {
                cold.writer.success(&mut cold.engine, "BEGIN", true);
            }
            for &(sql, returned) in statements {
                let context = format!("{mode:?} extended={extended} explicit={explicit}: {sql}");
                let kept = cold.kept_waits;
                let waits = cold.run(sql, extended, other_client_progresses);
                assert!(waits > 0, "{context}: a later row must wait");
                assert!(cold.kept_waits > kept, "{context}: the wait must keep rows");
                let output = cold.writer.send.readable().to_vec();
                assert!(
                    error_sqlstates(&output).is_empty(),
                    "{context}: {}",
                    String::from_utf8_lossy(&output)
                );
                assert_eq!(data_rows(&output), returned, "{context}");
            }
            if explicit {
                cold.writer.success(&mut cold.engine, "COMMIT", true);
            }
            let context = format!("{mode:?} extended={extended} explicit={explicit}");
            assert_eq!(
                cold.rows("SELECT string_agg(stage || ':' || n, ',' ORDER BY n) FROM audit"),
                [
                    "INSERT-o1:2,INSERT-o2:4,INSERT-o3:6,INSERT-o4:8,INSERT-o5:10,INSERT-o6:12,\
                     after-o1+:13,after-o2+:14,after-o3+:15,after-o4+:16,after-o5+:17,\
                     after-o6+:18,INSERT-changed-6:19,UPDATE-u:20,UPDATE-u:21,UPDATE-u:22,\
                     after-u+:23,after-u+:24,after-u+:25,UPDATE-changed-3:26,INSERT-v1:28,\
                     INSERT-v2:30,INSERT-v3:32,INSERT-v4:34,after-v1+:35,after-v2+:36,\
                     after-v3+:37,after-v4+:38,INSERT-changed-4:39"
                ],
                "{context}"
            );
            assert_eq!(
                cold.rows("SELECT count(*), sum(n) FROM orders"),
                ["10|156"],
                "{context}"
            );
            assert_eq!(
                cold.rows("SELECT count(*) FROM hot_log"),
                ["3"],
                "{context}"
            );
            cold.finish();
        }
    }
}

/// Storage waits inside the rows of multi-step writes, a `MERGE` and a
/// `DELETE` whose trigger reads a cold table: a rule's original statement
/// and action, a data-modifying `WITH` and its main statement, and `MERGE`'s
/// update and insert actions keep their finished steps and rows across the
/// waits, and each row, trigger write and `RETURNING` row happens once, as in
/// PostgreSQL 18.6; both transaction modes and both protocols.
#[test]
fn write_steps_resume_after_storage_waits() {
    let statements: &[(&str, &[&str])] = &[
        (
            "INSERT INTO links SELECT g, CASE WHEN g >= 3 THEN g END, 'r' || g \
             FROM generate_series(1, 4) g RETURNING id, more",
            &["1|NULL", "2|NULL", "3|3", "4|4"],
        ),
        (
            "WITH gone AS (DELETE FROM links WHERE id <= 2 RETURNING id, note) \
             INSERT INTO gone_log SELECT note || '-gone', id + 10 FROM gone RETURNING note, more",
            &["r1-gone|11", "r2-gone|12"],
        ),
        (
            "MERGE INTO pairs p USING generate_series(1, 6) g ON p.id = g \
             WHEN MATCHED THEN UPDATE SET more = g + 20 \
             WHEN NOT MATCHED THEN INSERT VALUES (g, g + 30, 'm' || g) \
             RETURNING p.id, p.more, p.note",
            &[
                "1|21|p1", "2|22|p2", "3|33|m3", "4|34|m4", "5|35|m5", "6|36|m6",
            ],
        ),
        (
            "DELETE FROM links WHERE id >= 3 RETURNING id, note",
            &["3|r3", "4|r4"],
        ),
    ];
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for extended in [false, true] {
            let mut cold = cold_table(
                &format!("row-steps-{extended}"),
                mode,
                "CREATE TABLE hot_log(stage text); \
                 CREATE TABLE cold_more(id integer PRIMARY KEY); \
                 INSERT INTO cold_more SELECT generate_series(1, 40); \
                 CREATE TABLE cold_extra(id integer PRIMARY KEY); \
                 INSERT INTO cold_extra SELECT generate_series(1, 40); \
                 CREATE TABLE links(id int, more int REFERENCES cold_more, note text); \
                 CREATE TABLE log(note text, more int REFERENCES cold_extra); \
                 CREATE TABLE cold_gone(id integer PRIMARY KEY); \
                 INSERT INTO cold_gone SELECT generate_series(1, 40); \
                 CREATE TABLE cold_pairs(id integer PRIMARY KEY); \
                 INSERT INTO cold_pairs SELECT generate_series(1, 40); \
                 CREATE TABLE gone_log(note text, more int REFERENCES cold_gone); \
                 CREATE TABLE pairs(id int, more int REFERENCES cold_pairs, note text); \
                 CREATE RULE log_links AS ON INSERT TO links \
                 DO ALSO INSERT INTO log VALUES (NEW.note, NEW.id); \
                 CREATE FUNCTION check_delete() RETURNS trigger LANGUAGE plpgsql AS $$ \
                 DECLARE seen bigint; BEGIN IF OLD.id >= 3 THEN \
                 SELECT count(*) INTO seen FROM cold_items; \
                 INSERT INTO log VALUES ('deleting-' || OLD.id || '-' || seen, NULL); \
                 END IF; RETURN OLD; END$$; \
                 CREATE TRIGGER links_delete BEFORE DELETE ON links \
                 FOR EACH ROW EXECUTE FUNCTION check_delete()",
            );
            cold.rows("INSERT INTO pairs VALUES (1, NULL, 'p1'), (2, NULL, 'p2')");
            for &(sql, returned) in statements {
                let context = format!("{mode:?} extended={extended}: {sql}");
                let kept = cold.kept_waits;
                let waits = cold.run(sql, extended, other_client_progresses);
                assert!(waits > 0, "{context}: a row must wait");
                assert!(cold.kept_waits > kept, "{context}: the wait must keep rows");
                let output = cold.writer.send.readable().to_vec();
                assert!(
                    error_sqlstates(&output).is_empty(),
                    "{context}: {}",
                    String::from_utf8_lossy(&output)
                );
                let mut rows = data_rows(&output);
                rows.sort();
                assert_eq!(rows, returned, "{context}");
            }
            let context = format!("{mode:?} extended={extended}");
            assert_eq!(
                cold.rows(
                    "SELECT string_agg(note || ':' || coalesce(more::text, '-'), ',' \
                     ORDER BY note) FROM log"
                ),
                ["deleting-3-40:-,deleting-4-40:-,r1:1,r2:2,r3:3,r4:4"],
                "{context}"
            );
            assert_eq!(
                cold.rows(
                    "SELECT count(*), coalesce(sum(id), 0) FROM links; \
                     SELECT count(*), sum(more) FROM pairs; \
                     SELECT string_agg(note || ':' || more, ',' ORDER BY note) FROM gone_log"
                ),
                ["0|0", "6|181", "r1-gone:11,r2-gone:12"],
                "{context}"
            );
            assert_eq!(
                cold.rows("SELECT count(*) FROM hot_log"),
                ["4"],
                "{context}"
            );
            cold.finish();
        }
    }
}

/// The same writes in a procedure, a `DO` body and a dynamic `EXECUTE`: a
/// storage wait inside their rows keeps the rows their frame already changed
/// and continues at the waiting row, with PostgreSQL 18.6's values; both
/// transaction modes and both protocols.
#[test]
fn procedural_write_rows_resume_after_storage_waits() {
    let statements = [
        "CALL fill()",
        "DO $$BEGIN UPDATE orders SET more = CASE WHEN n > 3 THEN n + 1 END, note = 'u' \
         WHERE item IS NULL; END$$",
        "DO $$BEGIN EXECUTE 'INSERT INTO orders(extra, note) VALUES (NULL, ''v1''), \
         (NULL, ''v2''), (8, ''v3''), (9, ''v4'')'; END$$",
    ];
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for extended in [false, true] {
            let mut cold = cold_table(
                &format!("procedural-row-waits-{extended}"),
                mode,
                &format!(
                    "{ORDER_SETUP}; CREATE PROCEDURE fill() LANGUAGE plpgsql AS $$BEGIN \
                     INSERT INTO orders(item, note) SELECT CASE WHEN g >= 4 THEN g END, \
                     'o' || g FROM generate_series(1, 6) g; END$$"
                ),
            );
            for sql in statements {
                let context = format!("{mode:?} extended={extended}: {sql}");
                let kept = crate::sql::exec::KEPT_ROW_WAITS.load(Ordering::Relaxed);
                let waits = cold.run(sql, extended, other_client_progresses);
                assert!(waits > 0, "{context}: a later row must wait");
                assert!(
                    crate::sql::exec::KEPT_ROW_WAITS.load(Ordering::Relaxed) > kept,
                    "{context}: the wait must keep the earlier rows"
                );
                let output = cold.writer.send.readable().to_vec();
                assert!(
                    error_sqlstates(&output).is_empty(),
                    "{context}: {}",
                    String::from_utf8_lossy(&output)
                );
            }
            let context = format!("{mode:?} extended={extended}");
            assert_eq!(
                cold.rows("SELECT string_agg(stage || ':' || n, ',' ORDER BY n) FROM audit"),
                [
                    "INSERT-o1:2,INSERT-o2:4,INSERT-o3:6,INSERT-o4:8,INSERT-o5:10,INSERT-o6:12,\
                     after-o1+:13,after-o2+:14,after-o3+:15,after-o4+:16,after-o5+:17,\
                     after-o6+:18,INSERT-changed-6:19,UPDATE-u:20,UPDATE-u:21,UPDATE-u:22,\
                     after-u+:23,after-u+:24,after-u+:25,UPDATE-changed-3:26,INSERT-v1:28,\
                     INSERT-v2:30,INSERT-v3:32,INSERT-v4:34,after-v1+:35,after-v2+:36,\
                     after-v3+:37,after-v4+:38,INSERT-changed-4:39"
                ],
                "{context}"
            );
            assert_eq!(
                cold.rows("SELECT count(*), sum(n), sum(more), sum(extra) FROM orders"),
                ["10|156|6|17"],
                "{context}"
            );
            assert_eq!(
                cold.rows("SELECT count(*) FROM hot_log"),
                ["3"],
                "{context}"
            );
            cold.finish();
        }
    }
}

/// Notes whose AFTER row and statement triggers read cold tables before
/// their writes, sequence calls and notices, with a deferrable foreign key
/// checked at the statement's end against a cold parent.
const AFTER_SETUP: &str = "CREATE TABLE hot_log(stage text); CREATE SEQUENCE numbers; \
     CREATE TABLE audit(stage text, n bigint); \
     CREATE TABLE cold_more(id integer PRIMARY KEY); \
     INSERT INTO cold_more SELECT generate_series(1, 40); \
     CREATE TABLE cold_gone(id integer PRIMARY KEY, code text); \
     INSERT INTO cold_gone SELECT g, 'g' || g FROM generate_series(1, 40) g; \
     CREATE TABLE cold_pairs(id integer PRIMARY KEY, code text); \
     INSERT INTO cold_pairs SELECT g, 'p' || g FROM generate_series(1, 40) g; \
     CREATE TABLE cold_extra(id integer PRIMARY KEY); \
     INSERT INTO cold_extra SELECT generate_series(1, 40); \
     CREATE TABLE cold_updates(id integer PRIMARY KEY); \
     INSERT INTO cold_updates SELECT generate_series(1, 30); \
     CREATE TABLE cold_deletes(id integer PRIMARY KEY); \
     INSERT INTO cold_deletes SELECT generate_series(1, 20); \
     CREATE TABLE notes(id integer, more integer REFERENCES cold_more \
     DEFERRABLE INITIALLY IMMEDIATE, note text); \
     CREATE FUNCTION after_note() RETURNS trigger LANGUAGE plpgsql AS $$ \
     DECLARE r record; c text; BEGIN \
     IF TG_OP = 'INSERT' THEN r := NEW; SELECT code INTO c FROM cold_items WHERE id = r.id; \
     ELSIF TG_OP = 'UPDATE' THEN r := NEW; \
     SELECT code INTO c FROM cold_gone WHERE id = r.id; \
     ELSE r := OLD; SELECT code INTO c FROM cold_pairs WHERE id = r.id; END IF; \
     RAISE NOTICE '% % %', TG_OP, r.note, c; \
     INSERT INTO audit VALUES (TG_OP || '-' || r.note || '-' || c, nextval('numbers')); \
     RETURN NULL; END$$; \
     CREATE TRIGGER after_note AFTER INSERT OR UPDATE OR DELETE ON notes \
     FOR EACH ROW EXECUTE FUNCTION after_note(); \
     CREATE FUNCTION after_notes() RETURNS trigger LANGUAGE plpgsql AS $$ \
     DECLARE seen bigint; BEGIN \
     IF TG_OP = 'INSERT' THEN SELECT count(*) INTO seen FROM cold_extra; \
     ELSIF TG_OP = 'UPDATE' THEN SELECT count(*) INTO seen FROM cold_updates; \
     ELSE SELECT count(*) INTO seen FROM cold_deletes; END IF; \
     INSERT INTO audit VALUES (TG_OP || '-all-' || seen, nextval('numbers')); \
     RAISE NOTICE '% statement', TG_OP; RETURN NULL; END$$; \
     CREATE TRIGGER after_notes AFTER INSERT OR UPDATE OR DELETE ON notes \
     FOR EACH STATEMENT EXECUTE FUNCTION after_notes()";

/// Reads after a write's last row yield: AFTER ROW and AFTER STATEMENT
/// trigger bodies and deferrable foreign key checks wait for delayed GETs
/// while another client runs. The finished events are kept, so notices,
/// audit rows, sequence values and the command tag each happen once, with
/// PostgreSQL 18.6's values; both transaction modes, both protocols,
/// implicit and explicit transactions.
#[test]
fn after_row_reads_yield_after_storage_waits() {
    let statements: &[(&str, &[&str])] = &[
        (
            "INSERT INTO notes VALUES (1, 1, 'a'), (2, NULL, 'b'), (3, 3, 'c')",
            &[
                "INSERT a c1",
                "INSERT b c2",
                "INSERT c c3",
                "INSERT statement",
            ],
        ),
        (
            "UPDATE notes SET more = id + 4, note = note || 'u' WHERE id >= 2",
            &["UPDATE bu g2", "UPDATE cu g3", "UPDATE statement"],
        ),
        (
            "DELETE FROM notes WHERE id = 1",
            &["DELETE a p1", "DELETE statement"],
        ),
    ];
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for (extended, explicit) in [(false, false), (true, false), (false, true)] {
            let mut cold = cold_table(
                &format!("after-waits-{extended}-{explicit}"),
                mode,
                AFTER_SETUP,
            );
            if explicit {
                cold.writer.success(&mut cold.engine, "BEGIN", true);
            }
            for &(sql, notices) in statements {
                let context = format!("{mode:?} extended={extended} explicit={explicit}: {sql}");
                let kept = cold.completion_waits;
                let waits = cold.run(sql, extended, other_client_progresses);
                assert!(waits > 0, "{context}: AFTER work must wait");
                assert!(
                    cold.completion_waits > kept,
                    "{context}: the wait must keep the finished AFTER work"
                );
                let output = cold.writer.send.readable().to_vec();
                assert!(
                    error_sqlstates(&output).is_empty(),
                    "{context}: {}",
                    String::from_utf8_lossy(&output)
                );
                assert_eq!(notice_messages(&output), notices, "{context}");
                assert_eq!(command_tags(&output).len(), 1, "{context}");
            }
            if explicit {
                cold.writer.success(&mut cold.engine, "COMMIT", true);
            }
            let context = format!("{mode:?} extended={extended} explicit={explicit}");
            // A missing parent fails the statement once and rolls back its
            // AFTER work. (Without a Sync, an extended-protocol error would
            // also end the implicit block of the statements before it.)
            if !extended {
                cold.run("INSERT INTO notes VALUES (9, 99, 'z')", false, |_, _| {});
                let output = cold.writer.send.readable().to_vec();
                assert_eq!(error_sqlstates(&output), ["23503"], "{context}");
                assert!(command_tags(&output).is_empty(), "{context}");
            }
            assert_eq!(
                cold.rows("SELECT string_agg(stage || ':' || n, ',' ORDER BY n) FROM audit"),
                ["INSERT-a-c1:1,INSERT-b-c2:2,INSERT-c-c3:3,INSERT-all-40:4,\
                     UPDATE-bu-g2:5,UPDATE-cu-g3:6,UPDATE-all-30:7,DELETE-a-p1:8,\
                     DELETE-all-20:9"],
                "{context}"
            );
            assert_eq!(
                cold.rows(
                    "SELECT string_agg(id || ':' || more || ':' || note, ',' ORDER BY id) \
                     FROM notes"
                ),
                ["2:6:bu,3:7:cu"],
                "{context}"
            );
            assert_eq!(
                cold.rows("SELECT count(*) FROM hot_log"),
                ["3"],
                "{context}"
            );
            cold.finish();
        }
    }
}
