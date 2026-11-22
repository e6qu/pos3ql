//! A join plan's pull nodes resume after storage waits at every node kind:
//! a wait while a nested loop reads its outer or its inner rows (inner,
//! left, semi and anti joins), while a `Materialize` fills its store and
//! while a scan reads a row returns to the reader with every node's
//! position kept, and the same call resumes it. Each joined row is
//! returned once, as PostgreSQL 18.6 returns it, whether the tables' rows
//! are resident or saved in object storage without a block cache.
//!
//! The test drives [`ModeledPull`] as a retained reader does, restarting
//! the statement's reads after each wait as `path_scan_waits` does.

use super::object_reads::replace_uncached_blocks;
use super::path_scan_waits::{advance_block_reads, serve};
use super::*;
use crate::sql::query::index_paths::joins::{JoinType, PathKind};
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
}

/// The joins the test reads, each with the rows PostgreSQL 18.6 returns
/// (in its order) for the fixture below.
fn queries() -> [(&'static str, Vec<String>); 4] {
    let matching = |id: i32| id % 5 == 0 && id <= 60;
    [
        (
            "SELECT o.id, i.w FROM outer_rows o JOIN inner_rows i \
             ON i.k <= o.id AND i.k >= o.id",
            (1..=OUTER_ROWS)
                .filter(|&id| matching(id))
                .map(|id| format!("{id}|w{id}"))
                .collect(),
        ),
        (
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
            "SELECT o.id FROM outer_rows o WHERE EXISTS \
             (SELECT FROM inner_rows i WHERE i.k <= o.id AND i.k >= o.id)",
            (1..=OUTER_ROWS)
                .filter(|&id| matching(id))
                .map(|id| id.to_string())
                .collect(),
        ),
        (
            "SELECT o.id FROM outer_rows o WHERE NOT EXISTS \
             (SELECT FROM inner_rows i WHERE i.k <= o.id AND i.k >= o.id)",
            (1..=OUTER_ROWS)
                .filter(|&id| !matching(id))
                .map(|id| id.to_string())
                .collect(),
        ),
    ]
}

/// The rows `sql`'s pull nodes return, the node kinds its plan holds, and
/// the waits after its first row.
fn run(engine: &mut Engine, config: &Config, sql: &str) -> (Vec<String>, Kinds, usize) {
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
                _ => {}
            }
        }
        let pull = ModeledPull::new(storage, txid, model, &scope, &[], &arena).unwrap();
        (statement, scope, pull, kinds)
    };
    let mut rows = Vec::new();
    let mut waits = 0;
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
                assert!(Instant::now() < deadline, "{sql}: the join did not advance");
                std::thread::yield_now();
            }
        }
    }
    pull.close();
    drop(pull);
    checkpointer.forget_statement_reads(owner);
    checkpointer.cancel_read_owner(owner);
    (rows, kinds, waits)
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
    config.row_overlay_slots = 512;
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
             ANALYZE outer_rows, inner_rows"
        ),
        false,
    );
    let queries = queries();
    let mut covered = Kinds::default();
    for (sql, expected) in &queries {
        let (rows, kinds, waits) = run(&mut engine, &config, sql);
        assert_eq!(waits, 0, "{sql}: resident rows never wait");
        assert_eq!(&rows, expected, "{sql}");
        covered = Kinds {
            inner: covered.inner || kinds.inner,
            left: covered.left || kinds.left,
            semi: covered.semi || kinds.semi,
            anti: covered.anti || kinds.anti,
            material: covered.material || kinds.material,
        };
    }
    assert!(
        covered.inner && covered.left && covered.semi && covered.anti && covered.material,
        "every pull node kind is read: {covered:?}"
    );
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
    for (sql, expected) in &queries {
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        replace_uncached_blocks(&mut engine, &network);
        let (rows, _, waits) = run(&mut engine, &config, sql);
        assert_eq!(&rows, expected, "{sql}");
        // The outer scan reads its second block after its first rows.
        assert!(
            waits > 0,
            "{sql}: the join must wait after returning a prefix"
        );
        assert!(!engine.block_reads_pending());
        replace_uncached_blocks(&mut engine, &config);
        drop(engine);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
    }
    stop.store(true, Ordering::Release);
    server.join().unwrap();
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}
