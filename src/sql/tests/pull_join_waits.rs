//! A join plan's pull nodes resume after storage waits at every node kind:
//! a wait while a nested loop reads its outer or its inner rows (inner,
//! left, semi and anti joins), while a `Materialize` fills its store, while
//! a hash join builds its table or probes it (inner, left, full, right,
//! anti, right semi and right anti joins), while a merge join reads an
//! input in order or sorts one (inner, full, semi and anti joins), while a
//! `Memoize` fills or evicts entries, while a sorted or hashed `Unique`
//! reads its input and while a scan reads a row returns to the reader with
//! every node's position kept, and the same call resumes it. Each joined
//! row is returned once, as PostgreSQL 18.6 returns it under the query's
//! planner settings, whether the tables' rows are resident or saved in
//! object storage without a block cache.
//!
//! The test drives [`ModeledPull`] as a retained reader does, restarting
//! the statement's reads after each wait as `path_scan_waits` does.

use super::object_reads::replace_uncached_blocks;
use super::path_scan_waits::{advance_block_reads, serve};
use super::*;
use crate::sql::query::index_paths::joins::{JoinType, PathKind, UniqueMethod};
use crate::sql::query::join_exec::{ModeledPull, pulls};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Outer rows of about 3 KB, so the saved rows span two 256 KiB blocks and
/// a scan waits again after its first rows.
const OUTER_ROWS: i32 = 100;

/// The node kinds a plan holds.
#[derive(Default, Debug, Clone, Copy)]
struct Kinds {
    inner: bool,
    left: bool,
    semi: bool,
    anti: bool,
    material: bool,
    /// Hash joins: any, one returning unmatched outer rows, one returning
    /// unmatched inner rows, and a right semi join.
    hash: bool,
    hash_fill_outer: bool,
    hash_fill_inner: bool,
    right_semi: bool,
    /// Merge joins: any, one returning unmatched rows of either side, a
    /// semi and an anti join, one reading its outer input in order and one
    /// keeping an inner input in order without a sort.
    merge: bool,
    merge_fill: bool,
    merge_semi: bool,
    merge_anti: bool,
    merge_streamed: bool,
    merge_unsorted_inner: bool,
    memoize: bool,
    unique_sorted: bool,
    unique_hashed: bool,
}

impl Kinds {
    fn union(self, other: Self) -> Self {
        Self {
            inner: self.inner || other.inner,
            left: self.left || other.left,
            semi: self.semi || other.semi,
            anti: self.anti || other.anti,
            material: self.material || other.material,
            hash: self.hash || other.hash,
            hash_fill_outer: self.hash_fill_outer || other.hash_fill_outer,
            hash_fill_inner: self.hash_fill_inner || other.hash_fill_inner,
            right_semi: self.right_semi || other.right_semi,
            merge: self.merge || other.merge,
            merge_fill: self.merge_fill || other.merge_fill,
            merge_semi: self.merge_semi || other.merge_semi,
            merge_anti: self.merge_anti || other.merge_anti,
            merge_streamed: self.merge_streamed || other.merge_streamed,
            merge_unsorted_inner: self.merge_unsorted_inner || other.merge_unsorted_inner,
            memoize: self.memoize || other.memoize,
            unique_sorted: self.unique_sorted || other.unique_sorted,
            unique_hashed: self.unique_hashed || other.unique_hashed,
        }
    }

    fn all(self) -> bool {
        self.inner
            && self.left
            && self.semi
            && self.anti
            && self.material
            && self.hash
            && self.hash_fill_outer
            && self.hash_fill_inner
            && self.right_semi
            && self.merge
            && self.merge_fill
            && self.merge_semi
            && self.merge_anti
            && self.merge_streamed
            && self.merge_unsorted_inner
            && self.memoize
            && self.unique_sorted
            && self.unique_hashed
    }
}

/// The planner settings that make PostgreSQL 18.6 merge, and those that
/// make it memoize or unique-ify a semi join's inner relation.
const MERGE: &str = "SET enable_hashjoin = off; SET enable_nestloop = off";
const MERGE_SMALL: &str =
    "SET enable_hashjoin = off; SET enable_nestloop = off; SET work_mem = '64kB'";
const LOOPS: &str = "SET enable_hashjoin = off; SET enable_mergejoin = off";
const LOOPS_SMALL: &str =
    "SET enable_hashjoin = off; SET enable_mergejoin = off; SET work_mem = '64kB'";
const SORTED_UNIQUE: &str =
    "SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_hashagg = off";

/// The first six characters of `keyed_rows.w` for row `g`.
fn keyed(g: i32) -> String {
    format!("m{g}{}", ".".repeat(300))[..6].to_owned()
}

/// The rows of `keyed_rows` whose key is `k`, in their stored order.
fn keyed_rows(k: i32) -> impl Iterator<Item = i32> {
    (1..=2000).filter(move |g| g % 20 == k)
}

/// The joins the test reads, each with the settings it is planned under
/// and the rows PostgreSQL 18.6 returns (in its order) for the fixture
/// below.
fn queries() -> Vec<(&'static str, &'static str, Vec<String>)> {
    let matching = |id: i32| id % 5 == 0 && id <= 60;
    // The inner rows no outer row matches at `k = id + 50`, in the order
    // PostgreSQL's hash table returns them.
    let unmatched = [
        "w20", "w25", "w10", "w15", "w40", "w5", "w30", "w50", "w45", "w35",
    ];
    let shifted = |id: i32| match id {
        5 => format!("{id}|w55"),
        10 => format!("{id}|w60"),
        _ => format!("{id}|NULL"),
    };
    vec![
        (
            "",
            "SELECT o.id, i.w FROM outer_rows o JOIN inner_rows i \
             ON i.k <= o.id AND i.k >= o.id",
            (1..=OUTER_ROWS)
                .filter(|&id| matching(id))
                .map(|id| format!("{id}|w{id}"))
                .collect(),
        ),
        (
            "",
            "SELECT o.id, i.w FROM outer_rows o LEFT JOIN inner_rows i \
             ON i.k <= o.id AND i.k >= o.id",
            (1..=OUTER_ROWS)
                .map(|id| {
                    if matching(id) {
                        format!("{id}|w{id}")
                    } else {
                        format!("{id}|NULL")
                    }
                })
                .collect(),
        ),
        (
            "",
            "SELECT o.id FROM outer_rows o WHERE EXISTS \
             (SELECT FROM inner_rows i WHERE i.k <= o.id AND i.k >= o.id)",
            (1..=OUTER_ROWS)
                .filter(|&id| matching(id))
                .map(|id| id.to_string())
                .collect(),
        ),
        (
            "",
            "SELECT o.id FROM outer_rows o WHERE NOT EXISTS \
             (SELECT FROM inner_rows i WHERE i.k <= o.id AND i.k >= o.id)",
            (1..=OUTER_ROWS)
                .filter(|&id| !matching(id))
                .map(|id| id.to_string())
                .collect(),
        ),
        (
            "",
            "SELECT o.id, i.w FROM outer_rows o JOIN inner_rows i ON i.k = o.id",
            (1..=OUTER_ROWS)
                .filter(|&id| matching(id))
                .map(|id| format!("{id}|w{id}"))
                .collect(),
        ),
        (
            "",
            "SELECT o.id, i.w FROM outer_rows o LEFT JOIN inner_rows i ON i.k = o.id",
            (1..=OUTER_ROWS)
                .map(|id| {
                    if matching(id) {
                        format!("{id}|w{id}")
                    } else {
                        format!("{id}|NULL")
                    }
                })
                .collect(),
        ),
        (
            "",
            "SELECT o.id, i.w FROM outer_rows o FULL JOIN inner_rows i ON i.k = o.id + 50",
            (1..=OUTER_ROWS)
                .map(shifted)
                .chain(unmatched.iter().map(|w| format!("NULL|{w}")))
                .collect(),
        ),
        (
            "",
            "SELECT o.id, i.w FROM outer_rows o RIGHT JOIN inner_rows i ON i.k = o.id + 50",
            ["5|w55".to_owned(), "10|w60".to_owned()]
                .into_iter()
                .chain(unmatched.iter().map(|w| format!("NULL|{w}")))
                .collect(),
        ),
        (
            "",
            "SELECT o.id FROM outer_rows o WHERE NOT EXISTS \
             (SELECT FROM inner_rows i WHERE i.k = o.id)",
            (1..=OUTER_ROWS)
                .filter(|&id| !matching(id))
                .map(|id| id.to_string())
                .collect(),
        ),
        (
            "",
            "SELECT i.w FROM inner_rows i WHERE NOT EXISTS \
             (SELECT FROM outer_rows o WHERE o.id + 50 = i.k)",
            unmatched.iter().map(|w| (*w).to_owned()).collect(),
        ),
        (
            "",
            "SELECT i.w FROM inner_rows i WHERE EXISTS \
             (SELECT FROM outer_rows o WHERE o.id = i.k)",
            (1..=12).map(|g| format!("w{}", g * 5)).collect(),
        ),
        (
            MERGE,
            "SELECT o.id, i.w FROM outer_rows o JOIN inner_rows i ON i.k = o.id",
            (5..=60)
                .step_by(5)
                .map(|id| format!("{id}|w{id}"))
                .collect(),
        ),
        (
            MERGE,
            "SELECT o.id, i.w FROM outer_rows o FULL JOIN inner_rows i ON i.k = o.id + 50",
            (5..=50)
                .step_by(5)
                .map(|k| format!("NULL|w{k}"))
                .chain((1..=OUTER_ROWS).map(shifted))
                .collect(),
        ),
        (
            MERGE,
            "SELECT o.id FROM outer_rows o WHERE EXISTS \
             (SELECT FROM inner_rows i WHERE i.k = o.id)",
            (5..=60).step_by(5).map(|id| id.to_string()).collect(),
        ),
        (
            MERGE,
            "SELECT o.id FROM outer_rows o WHERE NOT EXISTS \
             (SELECT FROM inner_rows i WHERE i.k = o.id)",
            (1..=OUTER_ROWS)
                .filter(|&id| !matching(id))
                .map(|id| id.to_string())
                .collect(),
        ),
        (
            MERGE_SMALL,
            "SELECT i.k, left(i.w, 6) FROM keyed_rows i JOIN inner_rows n ON n.k = i.k",
            [5, 10, 15]
                .into_iter()
                .flat_map(|k| keyed_rows(k).map(move |g| format!("{k}|{}", keyed(g))))
                .collect(),
        ),
        (
            MERGE_SMALL,
            "SELECT n.w FROM inner_rows n WHERE NOT EXISTS \
             (SELECT FROM keyed_rows i WHERE i.k = n.k)",
            (20..=60).step_by(5).map(|k| format!("w{k}")).collect(),
        ),
        (
            LOOPS_SMALL,
            "SELECT d.id, left(i.w, 6) FROM dup_rows d LEFT JOIN keyed_rows i ON i.k = d.k + 15",
            (1..=OUTER_ROWS)
                .flat_map(|d| {
                    let k = d % 7 + 15;
                    if k <= 19 {
                        keyed_rows(k)
                            .map(|g| format!("{d}|{}", keyed(g)))
                            .collect::<Vec<_>>()
                    } else {
                        vec![format!("{d}|NULL")]
                    }
                })
                .collect(),
        ),
        (
            LOOPS,
            "SELECT d.id, left(i.w, 6) FROM dup_rows d JOIN keyed_rows i \
             ON i.k = d.k WHERE i.w < 'm13'",
            (1..=OUTER_ROWS)
                .flat_map(|d| {
                    keyed_rows(d % 7)
                        .filter(|g| format!("m{g}{}", ".".repeat(300)).as_str() < "m13")
                        .map(move |g| format!("{d}|{}", keyed(g)))
                })
                .collect(),
        ),
        (
            SORTED_UNIQUE,
            "SELECT left(i.w, 6) FROM keyed_rows i WHERE i.k IN (SELECT n.k FROM inner_rows n)",
            [5, 10, 15]
                .into_iter()
                .flat_map(|k| keyed_rows(k).map(keyed))
                .collect(),
        ),
        (
            LOOPS,
            "SELECT left(i.w, 6) FROM keyed_rows i WHERE i.k IN (SELECT n.k FROM inner_rows n)",
            // The hash table's order of the inner keys.
            [10, 5, 15]
                .into_iter()
                .flat_map(|k| keyed_rows(k).map(keyed))
                .collect(),
        ),
    ]
}

/// The rows `sql`'s pull nodes return, planned under `settings`, the node
/// kinds its plan holds, the waits after its first row, and all its waits.
fn run(
    engine: &mut Engine,
    session: &mut ConfiguredTransactionSession,
    config: &Config,
    (settings, sql): (&str, &str),
) -> (Vec<String>, Kinds, usize, usize) {
    if !settings.is_empty() {
        session.success(engine, settings, false);
    }
    let mut budget = Budget::new(64 << 20);
    let arena = Arena::new(&mut budget, "pull join rows", 16 << 20).unwrap();
    let planning = Arena::new(&mut budget, "pull join planning", 16 << 20).unwrap();
    let work = Arena::new(&mut budget, "pull join work", 4 << 20).unwrap();
    let mut buffers = crate::storage::RowCursorBuffers::new(config, &mut budget).unwrap();
    let owner = ExecutionIdentity::new(43, engine.execution_identity().database_oid).read_owner();
    let prior_owner = engine.read_owner;
    let storage = &engine.storage;
    let checkpointer = engine.ckpt.as_mut().unwrap();
    let txid = 0;
    let select = crate::sql::parser::parse_query(sql, &arena).unwrap();
    let (statement, scope, mut pull, kinds) = {
        let _snapshot = storage.nested_statement_snapshot(storage.lsn());
        let _settings = crate::sql::guc::enter_eval_scope(&session.guc, &mut session.transaction);
        let statement =
            crate::sql::query::pull_up_subqueries(select, storage, txid, &arena, None).unwrap();
        let from = statement.from.as_ref().unwrap();
        let scope = crate::sql::query::QueryScope::resolve_schema_with_join_conditions(
            storage, from, txid, &arena, None,
        )
        .unwrap();
        let model = crate::sql::query::index_paths::join_query::modeled_join(
            storage,
            txid,
            statement,
            &scope,
            &[],
            &arena,
            &planning,
        )
        .unwrap()
        .expect("the join search plans the join");
        assert!(pulls(&model), "{sql}: every node pulls");
        let mut kinds = Kinds::default();
        for path in model.plan.paths {
            match path.kind {
                PathKind::Material { .. } => kinds.material = true,
                PathKind::NestLoop { jointype, .. } => match jointype {
                    JoinType::Inner => kinds.inner = true,
                    JoinType::Left => kinds.left = true,
                    JoinType::Semi => kinds.semi = true,
                    JoinType::Anti => kinds.anti = true,
                    _ => {}
                },
                PathKind::HashJoin { jointype, .. } => {
                    kinds.hash = true;
                    kinds.hash_fill_outer |= jointype.fills_outer();
                    kinds.hash_fill_inner |= jointype.fills_inner();
                    kinds.right_semi |= jointype == JoinType::RightSemi;
                }
                PathKind::MergeJoin {
                    jointype,
                    outer_sort,
                    inner_sort,
                    ..
                } => {
                    kinds.merge = true;
                    kinds.merge_fill |= jointype.fills_outer() && jointype.fills_inner();
                    kinds.merge_semi |= jointype == JoinType::Semi;
                    kinds.merge_anti |= jointype == JoinType::Anti;
                    kinds.merge_streamed |= outer_sort.as_slice().is_empty();
                    kinds.merge_unsorted_inner |= inner_sort.as_slice().is_empty();
                }
                PathKind::Memoize { .. } => kinds.memoize = true,
                PathKind::Unique { method, .. } => match method {
                    UniqueMethod::Sort => kinds.unique_sorted = true,
                    UniqueMethod::Hash => kinds.unique_hashed = true,
                    UniqueMethod::Noop => {}
                },
                _ => {}
            }
        }
        let pull = ModeledPull::new(storage, txid, model, &scope, &[], &arena).unwrap();
        (statement, scope, pull, kinds)
    };
    let mut rows = Vec::new();
    let mut waits = 0;
    let mut all_waits = 0;
    let mut progress = 0;
    let mut restarted = false;
    let mut deadline = crate::test_wait::deadline();
    checkpointer
        .begin_statement_reads(owner, false, progress)
        .unwrap();
    loop {
        advance_block_reads(checkpointer);
        if restarted {
            checkpointer
                .begin_statement_reads(owner, true, progress)
                .unwrap();
        }
        checkpointer.set_read_owner(Some(owner));
        let result = {
            let _snapshot = storage.nested_statement_snapshot(storage.lsn());
            crate::mem::guard::forbid_alloc(|| {
                pull.prepare_row(storage, &scope, &arena, &mut buffers, &work)
            })
        };
        checkpointer.set_read_owner(prior_owner);
        restarted = result.is_err();
        match result {
            Ok(true) => {
                progress += 1;
                checkpointer.statement_progressed(owner);
                let row = pull
                    .with_row(&scope, |row| {
                        let mut shown = Vec::new();
                        for item in statement.items {
                            let crate::sql::ast::SelectItem::Expr { expression, .. } = item else {
                                panic!("a listed column");
                            };
                            let value = crate::sql::eval::eval_full(
                                expression,
                                &work,
                                &[],
                                row,
                                &crate::sql::eval::NO_HOOKS,
                            )?;
                            shown.push(match value {
                                Datum::Null => "NULL".to_owned(),
                                Datum::Int4(value) => value.to_string(),
                                Datum::Text(value) => value.to_owned(),
                                other => panic!("an int or text column: {other:?}"),
                            });
                        }
                        Ok(shown.join("|"))
                    })
                    .unwrap();
                rows.push(row);
                pull.finish_row();
                deadline = crate::test_wait::deadline();
            }
            Ok(false) => break,
            Err(error) => {
                assert_eq!(
                    error.sqlstate,
                    sqlstate::INTERNAL_IO_WAIT,
                    "{sql}: {error:?}"
                );
                waits += usize::from(!rows.is_empty());
                all_waits += 1;
                assert!(Instant::now() < deadline, "{sql}: the join did not advance");
                std::thread::yield_now();
            }
        }
    }
    pull.close();
    drop(pull);
    checkpointer.forget_statement_reads(owner);
    checkpointer.cancel_read_owner(owner);
    if !settings.is_empty() {
        session.success(engine, "RESET ALL", false);
    }
    (rows, kinds, waits, all_waits)
}

#[test]
fn pull_join_nodes_resume_waits() {
    let label = "pull-join-waits";
    let mut config = test_config(label);
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = format!("{label}-{}", std::process::id());
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.object_store_get_slots = 4;
    config.object_store_hedge_after_ms = 0;
    config.block_cache_bytes = 0;
    config.disk_cache_bytes = 0;
    config.max_tables = 4;
    config.row_overlay_slots = 4096;
    config.wal_bytes = 32 << 20;
    config.wal_buffer_bytes = 1 << 20;
    config.max_spill_generations_per_table = 2;
    config.memtable_bytes = 8 << 20;
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    let namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 233);
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        &format!(
            "CREATE TABLE outer_rows(id integer, pad text); \
             INSERT INTO outer_rows SELECT g, \
             (SELECT string_agg(md5(g::text || i::text), '') FROM generate_series(1, 96) i) \
             FROM generate_series(1, {OUTER_ROWS}) g; \
             CREATE TABLE inner_rows(k integer, w text); \
             INSERT INTO inner_rows SELECT g * 5, 'w' || g * 5 FROM generate_series(1, 12) g; \
             CREATE TABLE dup_rows(id integer, k integer); \
             INSERT INTO dup_rows SELECT g, g % 7 FROM generate_series(1, {OUTER_ROWS}) g"
        ),
        false,
    );
    // Its own transaction: a transaction writes at most `txn_rows` rows.
    session.success(
        &mut engine,
        "CREATE TABLE keyed_rows(k integer, w text); \
         INSERT INTO keyed_rows SELECT g % 20, 'm' || g || repeat('.', 300) \
         FROM generate_series(1, 2000) g",
        false,
    );
    session.success(
        &mut engine,
        "CREATE INDEX keyed_rows_k ON keyed_rows(k); \
         ANALYZE outer_rows, inner_rows, keyed_rows, dup_rows",
        false,
    );
    // Its pages all visible, so an anti join reads the index alone, in
    // order, as its merge join's inner input.
    session.success(&mut engine, "VACUUM keyed_rows", false);
    let queries = queries();
    let mut covered = Kinds::default();
    for (settings, sql, expected) in &queries {
        let (rows, kinds, waits, _) = run(&mut engine, &mut session, &config, (settings, sql));
        assert_eq!(waits, 0, "{sql}: resident rows never wait");
        assert_eq!(&rows, expected, "{sql}");
        covered = covered.union(kinds);
    }
    assert!(covered.all(), "every pull node kind is read: {covered:?}");
    assert!(engine.checkpoint().unwrap());
    drop((session, engine));
    std::fs::remove_dir_all(&config.data_dir).unwrap();

    // Saved rows, each block read from object storage with a wait.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut network = config.clone();
    network.object_store_sim = false;
    network.object_store_endpoint = listener.local_addr().unwrap().to_string();
    network.object_store_access_key = "test-access".to_owned();
    network.object_store_secret_key = "test-secret".to_owned();
    let stop = Arc::new(AtomicBool::new(false));
    let server = serve(
        listener,
        format!("/{}/", config.object_store_bucket),
        namespace.clone(),
        Arc::clone(&stop),
    );
    for (settings, sql, expected) in &queries {
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        replace_uncached_blocks(&mut engine, &network);
        let (rows, _, waits, all_waits) = run(&mut engine, &mut session, &config, (settings, sql));
        assert_eq!(&rows, expected, "{sql}");
        assert!(all_waits > 0, "{sql}: the join reads saved blocks");
        // A scan reads a later block after the first rows return, except
        // where a right anti join returns rows only after its outer rows
        // end, or a merge join reads both inputs whole, sorted or kept,
        // before its first row.
        let whole = sql.starts_with("SELECT i.w FROM inner_rows i WHERE NOT EXISTS")
            || *settings == MERGE
            || sql.contains("NOT EXISTS (SELECT FROM keyed_rows");
        assert!(
            waits > 0 || whole,
            "{sql}: the join must wait after returning a prefix"
        );
        assert!(!engine.block_reads_pending());
        replace_uncached_blocks(&mut engine, &config);
        drop((session, engine));
        std::fs::remove_dir_all(&config.data_dir).unwrap();
    }
    stop.store(true, Ordering::Release);
    server.join().unwrap();
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}
