//! Two-session row-lock conflicts compared with PostgreSQL 18: foreign key
//! checks lock their parent row `FOR KEY SHARE`, and a write that waited for
//! a row lock continues with the row's latest committed version (read
//! committed) or fails with `40001` (repeatable read and serializable).

use super::*;
use crate::config::TransactionMode;

struct Sessions {
    config: Config,
    engine: Engine,
    first: ConfiguredTransactionSession,
    second: ConfiguredTransactionSession,
}

fn sessions(label: &str, mode: TransactionMode, setup: &str) -> Sessions {
    let mut config = test_config(&format!("row-lock-{label}-{mode:?}").to_lowercase());
    config.transaction_mode = mode;
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 28));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut first = ConfiguredTransactionSession::new(&config, &mut budget);
    let second = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    first.success(&mut engine, setup, false);
    Sessions {
        config,
        engine,
        first,
        second,
    }
}

impl Sessions {
    fn first(&mut self, sql: &str) -> Vec<u8> {
        self.first.execute(&mut self.engine, sql, true)
    }

    fn second(&mut self, sql: &str) -> Vec<u8> {
        self.second.execute(&mut self.engine, sql, true)
    }

    /// Runs `sql` in the second session, which must park on a row lock.
    fn second_waits(&mut self, sql: &str) -> (usize, crate::pg::overflow::OutputMark) {
        let ExecutionStatus::Blocked {
            completed_statements,
            output_mark,
            ..
        } = self.second.execute_status(&mut self.engine, sql, true)
        else {
            panic!("{sql} must wait for the first session's row lock");
        };
        (completed_statements, output_mark)
    }

    /// Resumes the parked statement in its own connection memory, as a
    /// parked protocol message does.
    fn resume_second(
        &mut self,
        sql: &str,
        (completed_statements, output_mark): (usize, crate::pg::overflow::OutputMark),
    ) -> Vec<u8> {
        output_mark.truncate_unsent(&mut self.second.send);
        self.second.arena.recycle_front();
        let second = &mut self.second;
        let status = crate::mem::guard::forbid_alloc(|| {
            self.engine.execute_simple_from(
                sql,
                completed_statements,
                &second.arena,
                &mut second.transaction,
                &mut second.pool,
                &mut second.cursors,
                &second.guc,
                &mut Responder::new(&mut second.send),
                second.connection_id,
                false,
            )
        })
        .unwrap();
        assert_eq!(status, ExecutionStatus::Complete, "{sql}");
        second.send.readable().to_vec()
    }

    fn rows(&mut self, sql: &str) -> Vec<String> {
        data_rows(&self.first.success(&mut self.engine, sql, true))
    }

    fn finish(self) {
        let directory = self.config.data_dir.clone();
        drop(self);
        let _ = std::fs::remove_dir_all(directory);
    }
}

const PARENT_CHILD: &str = "CREATE TABLE parents (id int PRIMARY KEY, note text); \
     CREATE TABLE children (id int, parent int REFERENCES parents); \
     CREATE TABLE cascades (id int, parent int REFERENCES parents ON DELETE CASCADE ON UPDATE CASCADE); \
     INSERT INTO parents VALUES (1, 'a'), (2, 'b')";

const MODES: [TransactionMode; 2] = [TransactionMode::Mvcc, TransactionMode::Locking];

#[test]
fn child_insert_waits_for_a_parent_delete_and_never_leaves_an_orphan() {
    for mode in MODES {
        let mut sessions = sessions("child-after-delete", mode, PARENT_CHILD);
        sessions.first("BEGIN; DELETE FROM parents WHERE id = 1");
        let insert = "INSERT INTO children VALUES (10, 1)";
        let parked = sessions.second_waits(insert);
        sessions.first("COMMIT");
        let output = sessions.resume_second(insert, parked);
        assert_eq!(error_sqlstates(&output), ["23503"], "{mode:?}");
        assert_eq!(sessions.rows("SELECT count(*) FROM children"), ["0"]);

        // A rolled-back delete lets the waiting insert keep its parent.
        sessions.first("BEGIN; DELETE FROM parents WHERE id = 2");
        let insert = "INSERT INTO children VALUES (20, 2)";
        let parked = sessions.second_waits(insert);
        sessions.first("ROLLBACK");
        let output = sessions.resume_second(insert, parked);
        assert!(error_sqlstates(&output).is_empty(), "{mode:?}");
        assert_eq!(
            sessions.rows("SELECT c.id FROM children c JOIN parents p ON p.id = c.parent"),
            ["20"]
        );
        sessions.finish();
    }
}

#[test]
fn parent_delete_waits_for_a_child_insert_and_sees_it_after_commit() {
    for mode in MODES {
        let mut sessions = sessions("delete-after-child", mode, PARENT_CHILD);
        sessions.first("BEGIN; INSERT INTO children VALUES (10, 1)");
        let delete = "DELETE FROM parents WHERE id = 1";
        let parked = sessions.second_waits(delete);
        sessions.first("COMMIT");
        let output = sessions.resume_second(delete, parked);
        assert_eq!(error_sqlstates(&output), ["23503"], "{mode:?}");
        assert_eq!(
            sessions.rows("SELECT count(*) FROM children c JOIN parents p ON p.id = c.parent"),
            ["1"]
        );

        // ON DELETE CASCADE removes the child committed during the wait.
        sessions.first("BEGIN; INSERT INTO cascades VALUES (30, 2)");
        let delete = "DELETE FROM parents WHERE id = 2";
        let parked = sessions.second_waits(delete);
        sessions.first("COMMIT");
        let output = sessions.resume_second(delete, parked);
        assert!(error_sqlstates(&output).is_empty(), "{mode:?}");
        assert_eq!(sessions.rows("SELECT count(*) FROM cascades"), ["0"]);
        sessions.finish();
    }
}

#[test]
fn key_share_locks_let_non_key_updates_and_other_children_proceed() {
    for mode in MODES {
        let mut sessions = sessions("key-share", mode, PARENT_CHILD);
        sessions.first("BEGIN; INSERT INTO children VALUES (10, 1)");
        // FOR KEY SHARE conflicts only with key changes and deletes.
        let output = sessions.second("UPDATE parents SET note = 'z' WHERE id = 1");
        assert!(error_sqlstates(&output).is_empty(), "{mode:?}");
        let update = "UPDATE parents SET id = 5 WHERE id = 1";
        let parked = sessions.second_waits(update);
        sessions.first("COMMIT");
        let output = sessions.resume_second(update, parked);
        assert_eq!(error_sqlstates(&output), ["23503"], "{mode:?}");
        assert_eq!(
            sessions.rows("SELECT id, note FROM parents ORDER BY id"),
            ["1|z", "2|b"]
        );
        sessions.finish();
    }
}

#[test]
fn repeatable_read_children_fail_when_the_parent_key_changed_after_the_snapshot() {
    let mut sessions = sessions("repeatable-read", TransactionMode::Mvcc, PARENT_CHILD);
    // A deleted parent is a serialization failure, not a missing key.
    sessions.second("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM parents");
    sessions.first("DELETE FROM parents WHERE id = 1");
    let output = sessions.second("INSERT INTO children VALUES (10, 1)");
    assert_eq!(error_sqlstates(&output), ["40001"]);
    sessions.second("ROLLBACK");

    // A committed non-key update keeps the key share lock available.
    sessions.second("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM parents");
    sessions.first("UPDATE parents SET note = 'q' WHERE id = 2");
    let output = sessions.second("INSERT INTO children VALUES (20, 2); COMMIT");
    assert!(error_sqlstates(&output).is_empty());

    // A parent inserted after the snapshot is not visible to it.
    sessions.second("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM parents");
    sessions.first("INSERT INTO parents VALUES (8, 'x')");
    let output = sessions.second("INSERT INTO children VALUES (80, 8)");
    assert_eq!(error_sqlstates(&output), ["23503"]);
    sessions.second("ROLLBACK");

    // NO ACTION sees a child committed after the snapshot; CASCADE cannot
    // rewrite a row its snapshot does not see.
    sessions.first("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM parents");
    sessions.second("INSERT INTO cascades VALUES (30, 8)");
    let output = sessions.first("DELETE FROM parents WHERE id = 8");
    assert_eq!(error_sqlstates(&output), ["40001"]);
    sessions.first("ROLLBACK");
    sessions.first("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM parents");
    sessions.second("INSERT INTO children VALUES (21, 2)");
    let output = sessions.first("DELETE FROM parents WHERE id = 2");
    assert_eq!(error_sqlstates(&output), ["23503"]);
    sessions.first("ROLLBACK");

    // Waiting for a deleting transaction ends in a serialization failure.
    sessions.first("BEGIN; DELETE FROM cascades; DELETE FROM parents WHERE id = 8");
    sessions.second("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM parents");
    let insert = "INSERT INTO children VALUES (81, 8)";
    let parked = sessions.second_waits(insert);
    sessions.first("COMMIT");
    let output = sessions.resume_second(insert, parked);
    assert_eq!(error_sqlstates(&output), ["40001"]);
    sessions.second("ROLLBACK");
    sessions.finish();
}

#[test]
fn restrict_violation_names_its_setting() {
    let mut sessions = sessions(
        "restrict",
        TransactionMode::Mvcc,
        "CREATE TABLE parents (id int PRIMARY KEY); \
         CREATE TABLE children (parent int REFERENCES parents ON DELETE RESTRICT); \
         INSERT INTO parents VALUES (1); INSERT INTO children VALUES (1)",
    );
    let output = sessions.first("DELETE FROM parents");
    assert_eq!(error_sqlstates(&output), ["23001"]);
    assert!(
        String::from_utf8_lossy(&output).contains(
            "update or delete on table \"parents\" violates RESTRICT setting of foreign key \
             constraint \"children_parent_fkey\" on table \"children\""
        ),
        "{}",
        String::from_utf8_lossy(&output)
    );
    sessions.finish();
}

#[test]
fn deferred_foreign_key_commit_waits_for_the_parent_lock() {
    for mode in MODES {
        let mut sessions = sessions(
            "deferred-commit",
            mode,
            "CREATE TABLE parents (id int PRIMARY KEY); \
             CREATE TABLE children (parent int REFERENCES parents DEFERRABLE INITIALLY DEFERRED); \
             INSERT INTO parents VALUES (1), (2)",
        );
        // The commit's check parks on the deleting transaction's row lock,
        // then fails against the committed delete.
        sessions.first("BEGIN; DELETE FROM parents WHERE id = 1");
        sessions.second("BEGIN; INSERT INTO children VALUES (2); INSERT INTO children VALUES (1)");
        let parked = sessions.second_waits("COMMIT");
        sessions.first("COMMIT");
        let output = sessions.resume_second("COMMIT", parked);
        assert_eq!(error_sqlstates(&output), ["23503"], "{mode:?}");
        assert_eq!(sessions.rows("SELECT count(*) FROM children"), ["0"]);

        // A rolled-back delete lets the waiting commit finish.
        sessions.first("BEGIN; DELETE FROM parents WHERE id = 2");
        sessions.second("BEGIN; INSERT INTO children VALUES (2)");
        let parked = sessions.second_waits("COMMIT");
        sessions.first("ROLLBACK");
        let output = sessions.resume_second("COMMIT", parked);
        assert!(error_sqlstates(&output).is_empty(), "{mode:?}");
        assert_eq!(sessions.rows("SELECT parent FROM children"), ["2"]);
        sessions.finish();
    }
}

#[test]
fn parent_row_locks_fill_the_row_lock_registry_with_a_named_error() {
    let mut config = test_config("row-lock-registry");
    // Two references per child row lock twice as many parents as the rows
    // the transaction writes.
    config.max_connections = 1;
    config.max_prepared_transactions = 0;
    config.txn_rows = 8;
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 28));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE parents (id int PRIMARY KEY); \
         CREATE TABLE children (a int REFERENCES parents, b int REFERENCES parents)",
        false,
    );
    for first in [1, 9, 17] {
        session.success(
            &mut engine,
            &format!("INSERT INTO parents SELECT generate_series({first}, {first} + 7)"),
            false,
        );
    }
    let output = session.execute(
        &mut engine,
        "BEGIN; INSERT INTO children VALUES (1, 11), (2, 12), (3, 13), (4, 14), (5, 15)",
        true,
    );
    assert_eq!(error_sqlstates(&output), ["54000"]);
    assert!(
        String::from_utf8_lossy(&output)
            .contains("row-lock registry is full (8 locks, sized by txn_rows)"),
        "{}",
        String::from_utf8_lossy(&output)
    );
    session.success(&mut engine, "ROLLBACK", true);
    // The failed transaction released its locks.
    session.success(
        &mut engine,
        "INSERT INTO children VALUES (1, 11), (2, 12), (3, 13), (4, 14)",
        true,
    );
    drop((session, engine));
    let _ = std::fs::remove_dir_all(config.data_dir);
}

const ACCOUNTS: &str = "CREATE TABLE accounts (id int PRIMARY KEY, balance int); \
     INSERT INTO accounts VALUES (1, 100), (2, 200)";

#[test]
fn read_committed_write_after_a_wait_uses_the_latest_committed_row() {
    for mode in MODES {
        let mut sessions = sessions("read-committed-recheck", mode, ACCOUNTS);
        // The waiting update adds to the committed balance, not the one it
        // first read (no lost update).
        sessions.first("BEGIN; UPDATE accounts SET balance = balance + 1 WHERE id = 1");
        let update = "UPDATE accounts SET balance = balance + 10 WHERE id = 1";
        let parked = sessions.second_waits(update);
        sessions.first("COMMIT");
        let output = sessions.resume_second(update, parked);
        assert!(error_sqlstates(&output).is_empty(), "{mode:?}");
        assert_eq!(
            sessions.rows("SELECT balance FROM accounts WHERE id = 1"),
            ["111"]
        );

        // A row that no longer meets the condition is skipped.
        sessions.first("BEGIN; UPDATE accounts SET balance = 0 WHERE id = 2");
        let update = "UPDATE accounts SET balance = balance + 10 WHERE balance = 200";
        let parked = sessions.second_waits(update);
        sessions.first("COMMIT");
        let output = sessions.resume_second(update, parked);
        assert!(
            String::from_utf8_lossy(&output).contains("UPDATE 0"),
            "{mode:?}"
        );

        // A deleted row is neither updated back nor deleted twice.
        sessions.first("BEGIN; DELETE FROM accounts WHERE id = 2");
        let update = "UPDATE accounts SET balance = 5 WHERE id = 2";
        let parked = sessions.second_waits(update);
        sessions.first("COMMIT");
        let output = sessions.resume_second(update, parked);
        assert!(
            String::from_utf8_lossy(&output).contains("UPDATE 0"),
            "{mode:?}"
        );
        assert_eq!(
            sessions.rows("SELECT id, balance FROM accounts ORDER BY id"),
            ["1|111"]
        );

        // A key change moves the row out of `id = 1`.
        sessions.first("BEGIN; UPDATE accounts SET id = 5 WHERE id = 1");
        let delete = "DELETE FROM accounts WHERE id = 1";
        let parked = sessions.second_waits(delete);
        sessions.first("COMMIT");
        let output = sessions.resume_second(delete, parked);
        assert!(
            String::from_utf8_lossy(&output).contains("DELETE 0"),
            "{mode:?}"
        );
        assert_eq!(
            sessions.rows("SELECT id, balance FROM accounts ORDER BY id"),
            ["5|111"]
        );

        // SELECT ... FOR UPDATE skips a row deleted while it waited.
        sessions.first("BEGIN; DELETE FROM accounts WHERE id = 5");
        let select = "SELECT id FROM accounts WHERE id = 5 FOR UPDATE";
        let parked = sessions.second_waits(select);
        sessions.first("COMMIT");
        let output = sessions.resume_second(select, parked);
        assert!(data_rows(&output).is_empty(), "{mode:?}");
        sessions.finish();
    }
}

#[test]
fn repeatable_read_writes_fail_on_rows_changed_after_the_snapshot() {
    let mut sessions = sessions("repeatable-read-writes", TransactionMode::Mvcc, ACCOUNTS);
    for (concurrent, statement, sqlstate_message) in [
        (
            "UPDATE accounts SET balance = balance + 1 WHERE id = 1",
            "UPDATE accounts SET balance = 9 WHERE id = 1",
            "concurrent update",
        ),
        (
            "DELETE FROM accounts WHERE id = 1",
            "UPDATE accounts SET balance = 9 WHERE id = 1",
            "concurrent delete",
        ),
        (
            "UPDATE accounts SET balance = balance + 1 WHERE id = 1",
            "DELETE FROM accounts WHERE id = 1",
            "concurrent update",
        ),
        (
            "DELETE FROM accounts WHERE id = 1",
            "MERGE INTO accounts USING (SELECT 1 AS id) s ON accounts.id = s.id \
             WHEN MATCHED THEN UPDATE SET balance = 9",
            "concurrent delete",
        ),
        (
            "UPDATE accounts SET balance = balance + 1 WHERE id = 1",
            "SELECT id FROM accounts WHERE id = 1 FOR UPDATE",
            "concurrent update",
        ),
        (
            "DELETE FROM accounts WHERE id = 1",
            "SELECT id FROM accounts WHERE id = 1 FOR KEY SHARE",
            "concurrent update",
        ),
    ] {
        sessions.first("DELETE FROM accounts; INSERT INTO accounts VALUES (1, 100), (2, 200)");
        sessions.second("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM accounts");
        sessions.first(concurrent);
        let output = sessions.second(statement);
        assert_eq!(
            error_sqlstates(&output),
            ["40001"],
            "{concurrent} | {statement}"
        );
        assert!(
            String::from_utf8_lossy(&output).contains(&format!(
                "could not serialize access due to {sqlstate_message}"
            )),
            "{}",
            String::from_utf8_lossy(&output)
        );
        sessions.second("ROLLBACK");
    }
    // A key share lock survives a change that kept every key column.
    sessions.first("DELETE FROM accounts; INSERT INTO accounts VALUES (1, 100)");
    sessions.second("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM accounts");
    sessions.first("UPDATE accounts SET balance = 7 WHERE id = 1");
    let output = sessions.second("SELECT balance FROM accounts WHERE id = 1 FOR KEY SHARE");
    assert_eq!(data_rows(&output), ["100"]);
    sessions.second("ROLLBACK");
    sessions.finish();
}

const ITEMS: &str = "CREATE TABLE items (id int PRIMARY KEY, qty int, note text); \
     CREATE TABLE stock (id int, qty int); \
     INSERT INTO items VALUES (1, 1, 'a'), (2, 2, 'b'), (3, 3, 'c'); \
     INSERT INTO stock VALUES (1, 1), (2, 2), (4, 4)";

/// Runs `statement` in the second session while the first holds `change`
/// uncommitted, then commits the change and returns the statement's output.
fn after_concurrent_change(sessions: &mut Sessions, change: &str, statement: &str) -> Vec<u8> {
    sessions.first(
        "DELETE FROM items; DELETE FROM stock; \
         INSERT INTO items VALUES (1, 1, 'a'), (2, 2, 'b'), (3, 3, 'c'); \
         INSERT INTO stock VALUES (1, 1), (2, 2), (4, 4)",
    );
    sessions.first(&format!("BEGIN; {change}"));
    let parked = sessions.second_waits(statement);
    sessions.first("COMMIT");
    sessions.resume_second(statement, parked)
}

#[test]
fn read_committed_select_for_update_returns_and_rechecks_the_latest_version() {
    for mode in MODES {
        let mut sessions = sessions("select-for-update-recheck", mode, ITEMS);
        for (change, statement, expected) in [
            // The latest version is returned, not the one first read.
            (
                "UPDATE items SET qty = qty + 1 WHERE id = 1",
                "SELECT * FROM items WHERE id = 1 FOR UPDATE",
                &["1|2|a"][..],
            ),
            (
                "UPDATE items SET note = 'z' WHERE id = 1",
                "SELECT id, qty * 10, note FROM items WHERE id = 1 FOR SHARE",
                &["1|10|z"],
            ),
            // A row that no longer meets WHERE leaves the result, and LIMIT
            // takes the next row instead.
            (
                "UPDATE items SET qty = 100 WHERE id = 1",
                "SELECT id FROM items WHERE qty < 50 ORDER BY id LIMIT 1 FOR UPDATE",
                &["2"],
            ),
            // Join conditions run again with the unlocked side as read.
            (
                "UPDATE items SET qty = qty + 1",
                "SELECT i.id, s.qty FROM items i JOIN stock s ON s.id = i.id AND s.qty = i.qty \
                 ORDER BY i.id FOR UPDATE",
                &[],
            ),
            (
                "UPDATE items SET qty = qty + 1 WHERE id = 1",
                "SELECT i.id, i.qty, s.qty FROM items i LEFT JOIN stock s \
                 ON s.id = i.id AND s.qty = i.qty ORDER BY i.id FOR UPDATE OF i",
                &["1|2|NULL", "2|2|2", "3|3|NULL"],
            ),
        ] {
            let output = after_concurrent_change(&mut sessions, change, statement);
            assert!(error_sqlstates(&output).is_empty(), "{mode:?} {statement}");
            assert_eq!(data_rows(&output), expected, "{mode:?} {statement}");
        }
        sessions.finish();
    }
}

#[test]
fn read_committed_merge_chooses_its_action_again_for_the_latest_version() {
    let mut sessions = sessions("merge-recheck", TransactionMode::Mvcc, ITEMS);
    for (change, statement, expected) in [
        // The update applies to the committed quantity (no lost update) and
        // its WHEN conditions run again.
        (
            "UPDATE items SET qty = qty + 1 WHERE id = 1",
            "MERGE INTO items USING stock ON stock.id = items.id \
             WHEN MATCHED AND items.qty = 1 THEN UPDATE SET qty = items.qty + 10 \
             WHEN MATCHED THEN UPDATE SET note = 'm' \
             WHEN NOT MATCHED THEN INSERT VALUES (stock.id, stock.qty, 'i')",
            &["1|2|m", "2|2|m", "3|3|c", "4|4|i"][..],
        ),
        (
            "UPDATE items SET note = 'z' WHERE id = 1",
            "MERGE INTO items USING stock ON stock.id = items.id \
             WHEN MATCHED THEN UPDATE SET qty = items.qty + 10",
            &["1|11|z", "2|12|b", "3|3|c"],
        ),
        // A deleted target row turns the pair into NOT MATCHED.
        (
            "DELETE FROM items WHERE id = 1",
            "MERGE INTO items USING stock ON stock.id = items.id \
             WHEN MATCHED THEN UPDATE SET note = 'm' \
             WHEN NOT MATCHED THEN INSERT VALUES (stock.id, stock.qty, 'i')",
            &["1|1|i", "2|2|m", "3|3|c", "4|4|i"],
        ),
        // A join condition that no longer holds runs the NOT MATCHED BY
        // SOURCE action on the target row and the NOT MATCHED action for
        // the source row.
        (
            "UPDATE items SET id = 5 WHERE id = 1",
            "MERGE INTO items USING stock ON stock.id = items.id \
             WHEN MATCHED THEN UPDATE SET note = 'm' \
             WHEN NOT MATCHED BY SOURCE THEN UPDATE SET note = 's' \
             WHEN NOT MATCHED THEN INSERT VALUES (stock.id + 30, stock.qty, 'n')",
            &["2|2|m", "3|3|s", "5|1|s", "31|1|n", "34|4|n"],
        ),
    ] {
        let output = after_concurrent_change(&mut sessions, change, statement);
        assert!(error_sqlstates(&output).is_empty(), "{statement}");
        assert_eq!(
            sessions.rows("SELECT * FROM items ORDER BY id"),
            expected,
            "{statement}"
        );
    }
    sessions.finish();
}

#[test]
fn row_locks_reject_nullable_outer_join_sides_and_set_returning_targets() {
    let mut sessions = sessions("lock-validation", TransactionMode::Mvcc, ITEMS);
    for (statement, message) in [
        (
            "SELECT * FROM items LEFT JOIN stock ON stock.id = items.id FOR UPDATE",
            "FOR UPDATE cannot be applied to the nullable side of an outer join",
        ),
        (
            "SELECT * FROM items RIGHT JOIN stock ON stock.id = items.id FOR SHARE OF items",
            "FOR SHARE cannot be applied to the nullable side of an outer join",
        ),
        (
            "SELECT id, generate_series(1, 2) FROM items FOR UPDATE",
            "FOR UPDATE is not allowed with set-returning functions in the target list",
        ),
    ] {
        let output = sessions.second(statement);
        assert_eq!(error_sqlstates(&output), ["0A000"], "{statement}");
        assert!(
            String::from_utf8_lossy(&output).contains(message),
            "{}",
            String::from_utf8_lossy(&output)
        );
    }
    sessions.finish();
}

#[test]
fn only_a_changed_key_value_waits_for_key_share_locks() {
    let mut sessions = sessions(
        "key-value-lock-strength",
        TransactionMode::Mvcc,
        PARENT_CHILD,
    );
    sessions.first("BEGIN; INSERT INTO children VALUES (10, 1)");
    // Assignments that keep every key value take FOR NO KEY UPDATE strength.
    for statement in [
        "UPDATE parents SET id = id WHERE id = 1",
        "UPDATE parents SET id = 1, note = 'same' WHERE id = 1",
        "MERGE INTO parents USING (SELECT 1 AS id) s ON s.id = parents.id \
         WHEN MATCHED THEN UPDATE SET id = parents.id",
    ] {
        let output = sessions.second(statement);
        assert!(error_sqlstates(&output).is_empty(), "{statement}");
    }
    // A changed key value waits for the inserting child's key share lock.
    let update = "UPDATE parents SET id = 3 WHERE id = 1";
    let parked = sessions.second_waits(update);
    sessions.first("COMMIT");
    let output = sessions.resume_second(update, parked);
    assert_eq!(error_sqlstates(&output), ["23503"]);
    sessions.finish();
}

const PARTITIONS: &str = "CREATE TABLE pt (id int, k int, v int) PARTITION BY LIST (k); \
     CREATE TABLE pt1 PARTITION OF pt FOR VALUES IN (1); \
     CREATE TABLE pt2 PARTITION OF pt FOR VALUES IN (2); \
     CREATE TABLE ps (id int, v int)";

const MOVED_OUT: &str =
    "tuple to be locked was already moved to another partition due to concurrent update";

/// Statements that lock row 1 of `pt1` while another transaction moves it to
/// `pt2`, and the message PostgreSQL 18 reports under repeatable read.
const MOVE_LOCKERS: [(&str, &str); 9] = [
    ("UPDATE pt SET v = v + 1 WHERE id = 1", "concurrent update"),
    (
        "UPDATE pt SET k = 2, v = 7 WHERE id = 1",
        "concurrent update",
    ),
    ("DELETE FROM pt WHERE id = 1", "concurrent update"),
    ("UPDATE pt1 SET v = v + 1 WHERE id = 1", "concurrent update"),
    (
        "SELECT * FROM pt WHERE id = 1 FOR UPDATE",
        "concurrent update",
    ),
    (
        "SELECT * FROM pt WHERE id = 1 FOR KEY SHARE",
        "concurrent update",
    ),
    (
        "MERGE INTO pt USING ps ON ps.id = pt.id WHEN MATCHED THEN UPDATE SET v = pt.v + 10",
        "concurrent update",
    ),
    (
        "MERGE INTO pt USING ps ON ps.id = pt.id WHEN MATCHED THEN DELETE",
        "concurrent update",
    ),
    (
        "WITH x AS (UPDATE pt SET v = v + 1 WHERE id = 1 RETURNING *) SELECT * FROM x",
        "concurrent update",
    ),
];

fn reset_partitions(sessions: &mut Sessions) {
    sessions.first(
        "DELETE FROM pt; DELETE FROM ps; \
         INSERT INTO pt VALUES (1, 1, 1), (2, 1, 2); INSERT INTO ps VALUES (1, 1)",
    );
}

#[test]
fn a_concurrent_partition_move_fails_waiting_lockers_as_postgresql_does() {
    let mut sessions = sessions("partition-move", TransactionMode::Mvcc, PARTITIONS);
    for (isolation, repeatable) in [("READ COMMITTED", false), ("REPEATABLE READ", true)] {
        for (statement, repeatable_message) in MOVE_LOCKERS {
            reset_partitions(&mut sessions);
            sessions.second(&format!(
                "BEGIN ISOLATION LEVEL {isolation}; SELECT count(*) FROM ps"
            ));
            sessions.first("BEGIN; UPDATE pt SET k = 2 WHERE id = 1");
            let parked = sessions.second_waits(statement);
            sessions.first("COMMIT");
            let output = sessions.resume_second(statement, parked);
            assert_eq!(
                error_sqlstates(&output),
                ["40001"],
                "{isolation} {statement}"
            );
            let message = if repeatable {
                format!("could not serialize access due to {repeatable_message}")
            } else {
                MOVED_OUT.to_owned()
            };
            assert!(
                String::from_utf8_lossy(&output).contains(&message),
                "{isolation} {statement}: {}",
                String::from_utf8_lossy(&output)
            );
            sessions.second("ROLLBACK");
            assert_eq!(
                sessions.rows("SELECT id, k, v FROM pt2"),
                ["1|2|1"],
                "{isolation} {statement}"
            );
        }
    }
    // An ordinary deletion is still skipped under read committed, and a
    // rolled-back move leaves the row to the waiting statement.
    reset_partitions(&mut sessions);
    sessions.first("BEGIN; DELETE FROM pt WHERE id = 1");
    let update = "UPDATE pt SET v = 50 WHERE id = 1";
    let parked = sessions.second_waits(update);
    sessions.first("COMMIT");
    let output = sessions.resume_second(update, parked);
    assert!(error_sqlstates(&output).is_empty());
    assert!(String::from_utf8_lossy(&output).contains("UPDATE 0"));
    reset_partitions(&mut sessions);
    sessions.first("BEGIN; UPDATE pt SET k = 2 WHERE id = 1");
    let parked = sessions.second_waits(update);
    sessions.first("ROLLBACK");
    let output = sessions.resume_second(update, parked);
    assert!(error_sqlstates(&output).is_empty());
    assert_eq!(
        sessions.rows("SELECT id, k, v FROM pt1 WHERE id = 1"),
        ["1|1|50"]
    );
    // MERGE moving the row marks the deletion too.
    reset_partitions(&mut sessions);
    sessions
        .first("BEGIN; MERGE INTO pt USING ps ON ps.id = pt.id WHEN MATCHED THEN UPDATE SET k = 2");
    let parked = sessions.second_waits(update);
    sessions.first("COMMIT");
    let output = sessions.resume_second(update, parked);
    assert_eq!(error_sqlstates(&output), ["40001"]);
    assert!(String::from_utf8_lossy(&output).contains(MOVED_OUT));
    sessions.finish();
}

#[test]
fn a_prepared_partition_move_keeps_its_marker_across_restart() {
    for cold in [false, true] {
        let mut config = test_config(&format!("row-lock-prepared-move-{cold}"));
        config.max_prepared_transactions = 2;
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 28));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
        writer.success(
            &mut engine,
            &format!(
                "{PARTITIONS}; INSERT INTO pt VALUES (1, 1, 1), (2, 1, 2); \
                 BEGIN; UPDATE pt SET k = 2 WHERE id = 1; PREPARE TRANSACTION 'move'"
            ),
            false,
        );
        drop(writer);
        if cold {
            drop(engine);
            budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 28));
            engine = Engine::new(&config, &mut budget).unwrap();
        }
        let first = ConfiguredTransactionSession::new(&config, &mut budget);
        let second = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        let mut sessions = Sessions {
            config,
            engine,
            first,
            second,
        };
        let update = "UPDATE pt SET v = v + 1 WHERE id = 1";
        let parked = sessions.second_waits(update);
        sessions.first("COMMIT PREPARED 'move'");
        let output = sessions.resume_second(update, parked);
        assert_eq!(error_sqlstates(&output), ["40001"], "cold={cold}");
        assert!(
            String::from_utf8_lossy(&output).contains(MOVED_OUT),
            "{}",
            String::from_utf8_lossy(&output)
        );
        assert_eq!(
            sessions.rows("SELECT id, k, v FROM pt ORDER BY id"),
            ["1|2|1", "2|1|2"]
        );
        sessions.finish();
    }
}

#[test]
fn read_committed_rechecks_run_lateral_sources_as_postgresql_plans_them() {
    let mut sessions = sessions("lateral-recheck", TransactionMode::Mvcc, ITEMS);
    for (statement, expected) in [
        // A simple subquery without FROM is pulled up and runs again.
        (
            "SELECT i.id, i.qty, s.x FROM items i, LATERAL (SELECT i.qty * 10 AS x) s \
             WHERE i.id = 1 FOR UPDATE OF i",
            &["1|2|20"][..],
        ),
        (
            "SELECT i.id, s.y FROM items i, LATERAL (VALUES (i.qty + 1)) s(y) \
             WHERE i.id = 1 FOR UPDATE OF i",
            &["1|3"],
        ),
        (
            "SELECT i.id, s.x FROM items i, LATERAL (SELECT i.qty * 10 AS x WHERE i.qty < 2) s \
             ORDER BY i.id FOR UPDATE OF i",
            &[],
        ),
        // Functions and subqueries the planner keeps separate return the
        // row first read.
        (
            "SELECT i.id, g FROM items i, LATERAL generate_series(i.qty, 2) g \
             WHERE i.id = 1 ORDER BY g FOR UPDATE OF i",
            &["1|1", "1|2"],
        ),
        (
            "SELECT i.id, s.x FROM items i, LATERAL (SELECT i.qty * 10 AS x LIMIT 1) s \
             WHERE i.id = 1 FOR UPDATE OF i",
            &["1|10"],
        ),
        (
            "SELECT i.id, s.m FROM items i, LATERAL (SELECT max(qty) + i.qty AS m FROM stock \
             WHERE stock.id = i.id) s WHERE i.id = 1 FOR UPDATE OF i",
            &["1|2"],
        ),
        // A flattened UNION ALL returns its first branch again.
        (
            "SELECT i.id, s.y FROM items i, LATERAL (SELECT i.qty + 1 AS y UNION ALL \
             SELECT i.qty + 2) s WHERE i.id = 1 FOR UPDATE OF i",
            &["1|3", "1|3"],
        ),
    ] {
        let output = after_concurrent_change(
            &mut sessions,
            "UPDATE items SET qty = qty + 1 WHERE id = 1",
            statement,
        );
        assert!(error_sqlstates(&output).is_empty(), "{statement}");
        assert_eq!(data_rows(&output), expected, "{statement}");
    }
    // A pulled-up subquery reading tables keeps rows pos3ql does not retain.
    let output = after_concurrent_change(
        &mut sessions,
        "UPDATE items SET qty = qty + 1 WHERE id = 1",
        "SELECT i.id, s.y FROM items i, LATERAL (SELECT stock.qty + i.qty AS y FROM stock \
         WHERE stock.id = i.id) s WHERE i.id = 1 FOR UPDATE OF i",
    );
    assert_eq!(error_sqlstates(&output), ["0A000"]);
    sessions.finish();
}

#[test]
fn read_committed_semi_joins_recheck_the_subquery_row_first_matched() {
    let mut sessions = sessions("semi-join-recheck", TransactionMode::Mvcc, ITEMS);
    for (statement, expected) in [
        (
            "SELECT id FROM items WHERE EXISTS (SELECT 1 FROM stock WHERE stock.qty = items.qty) \
             ORDER BY id FOR UPDATE",
            &["2"][..],
        ),
        (
            "SELECT id FROM items WHERE EXISTS (SELECT 1 FROM stock WHERE stock.qty >= items.qty) \
             ORDER BY id FOR UPDATE",
            &["2", "3"],
        ),
        // A condition PostgreSQL keeps as a subplan runs again.
        (
            "SELECT id FROM items WHERE EXISTS (SELECT 1 FROM stock WHERE stock.qty = items.qty) \
             OR id = 3 ORDER BY id FOR UPDATE",
            &["1", "2", "3"],
        ),
        (
            "UPDATE items SET note = 'u' WHERE EXISTS \
             (SELECT 1 FROM stock WHERE stock.qty = items.qty) RETURNING id",
            &["2"],
        ),
        (
            "DELETE FROM items WHERE qty IN (SELECT qty FROM stock) RETURNING id",
            &["2"],
        ),
    ] {
        let output = after_concurrent_change(
            &mut sessions,
            "UPDATE items SET qty = qty + 1 WHERE id = 1",
            statement,
        );
        assert!(error_sqlstates(&output).is_empty(), "{statement}");
        assert_eq!(data_rows(&output), expected, "{statement}");
    }
    sessions.finish();
}

#[test]
fn a_merge_source_row_runs_not_matched_for_every_lost_pair() {
    let mut sessions = sessions(
        "merge-lost-pairs",
        TransactionMode::Mvcc,
        "CREATE TABLE mt (id int, k int, v int); CREATE TABLE ms (k int, w int)",
    );
    let merge = "MERGE INTO mt USING ms ON ms.k = mt.k \
         WHEN MATCHED THEN UPDATE SET v = mt.v + ms.w \
         WHEN NOT MATCHED THEN INSERT VALUES (100 + ms.w, ms.k, ms.w) \
         RETURNING merge_action(), mt.id";
    for (change, expected) in [
        (
            "UPDATE mt SET k = k + 10",
            &["INSERT|101", "INSERT|101", "INSERT|102", "INSERT|103"][..],
        ),
        // The stage runs right after the lost pair, as PostgreSQL's does.
        (
            "UPDATE mt SET k = 11 WHERE id = 1",
            &["INSERT|101", "UPDATE|2", "UPDATE|3", "INSERT|103"],
        ),
        (
            "DELETE FROM mt WHERE id = 2",
            &["UPDATE|1", "INSERT|101", "UPDATE|3", "INSERT|103"],
        ),
    ] {
        sessions.first(
            "DELETE FROM mt; DELETE FROM ms; INSERT INTO mt VALUES (1, 1, 10), (2, 1, 20), \
             (3, 2, 30); INSERT INTO ms VALUES (1, 1), (2, 2), (3, 3)",
        );
        sessions.first(&format!("BEGIN; {change}"));
        let parked = sessions.second_waits(merge);
        sessions.first("COMMIT");
        let output = sessions.resume_second(merge, parked);
        assert!(error_sqlstates(&output).is_empty(), "{change}");
        assert_eq!(data_rows(&output), expected, "{change}");
    }
    sessions.finish();
}

#[test]
fn updates_and_deletes_run_correlated_where_subqueries_per_row() {
    let mut sessions = sessions(
        "correlated-dml-where",
        TransactionMode::Mvcc,
        "CREATE TABLE aa (id int, v int); CREATE TABLE bb (id int, w int); \
         INSERT INTO aa VALUES (1, 1), (2, 2), (3, 3); INSERT INTO bb VALUES (1, 1), (3, 9)",
    );
    for (statement, expected) in [
        (
            "UPDATE aa SET v = v + 10 WHERE EXISTS (SELECT 1 FROM bb WHERE bb.id = aa.id) \
             RETURNING id, v",
            &["1|11", "3|13"][..],
        ),
        (
            "UPDATE aa t SET v = 0 WHERE t.id = (SELECT max(id) FROM bb WHERE bb.w > t.v - 100) \
             RETURNING id",
            &["3"],
        ),
        (
            "DELETE FROM aa WHERE v = (SELECT aa.v) AND id = 2 RETURNING id",
            &["2"],
        ),
        (
            "UPDATE aa SET v = 5 FROM bb WHERE bb.id = aa.id AND EXISTS \
             (SELECT 1 FROM bb b2 WHERE b2.w = aa.v + 0 * bb.w) RETURNING aa.id",
            &[],
        ),
        (
            "MERGE INTO aa USING bb ON bb.id = aa.id WHEN MATCHED AND EXISTS \
             (SELECT 1 FROM bb b2 WHERE b2.id = aa.id AND b2.w = 9) THEN DELETE \
             RETURNING aa.id",
            &["3"],
        ),
    ] {
        let output = sessions.second(statement);
        assert!(error_sqlstates(&output).is_empty(), "{statement}");
        assert_eq!(data_rows(&output), expected, "{statement}");
    }
    // A LATERAL set operation reads the tables to its left.
    assert_eq!(
        sessions.rows(
            "SELECT aa.id, s.y FROM aa, LATERAL (SELECT aa.v + 1 AS y UNION ALL \
             SELECT aa.v + 2) s ORDER BY 1, 2"
        ),
        ["1|12", "1|13"]
    );
    sessions.finish();
}

const PARTITIONED_PARENT: &str = "CREATE TABLE fp (id int, k int, PRIMARY KEY (id, k)) PARTITION BY LIST (k); \
     CREATE TABLE fp1 PARTITION OF fp FOR VALUES IN (1); \
     CREATE TABLE fp2 PARTITION OF fp FOR VALUES IN (2); \
     CREATE TABLE fc (id int, pid int, pk int, FOREIGN KEY (pid, pk) REFERENCES fp)";

#[test]
fn foreign_key_checks_follow_partitioned_parents_as_postgresql_does() {
    let mut sessions = sessions(
        "partitioned-parent",
        TransactionMode::Mvcc,
        PARTITIONED_PARENT,
    );
    // A waiting check cannot follow a parent moved to another partition.
    sessions.first("INSERT INTO fp VALUES (1, 1)");
    sessions.first("BEGIN; UPDATE fp SET k = 2 WHERE id = 1");
    let insert = "INSERT INTO fc VALUES (1, 1, 1)";
    let parked = sessions.second_waits(insert);
    sessions.first("COMMIT");
    let output = sessions.resume_second(insert, parked);
    assert_eq!(error_sqlstates(&output), ["40001"]);
    assert!(String::from_utf8_lossy(&output).contains(MOVED_OUT));
    // After the move, a new statement finds no parent.
    let output = sessions.second(insert);
    assert_eq!(error_sqlstates(&output), ["23503"]);
    // Repeatable read checks a partitioned parent with a current snapshot.
    for (change, expected) in [
        ("INSERT INTO fp VALUES (5, 1)", &[][..]),
        ("DELETE FROM fp WHERE id = 5", &["23503"][..]),
    ] {
        sessions.second("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM fc");
        sessions.first(change);
        let output = sessions.second("INSERT INTO fc VALUES (5, 5, 1)");
        assert_eq!(error_sqlstates(&output), expected, "{change}");
        sessions.second("ROLLBACK");
    }
    sessions.finish();
}
