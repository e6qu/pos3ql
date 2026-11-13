//! An `INSERT ... SELECT` whose source reads repeatably streams it: one
//! source row at a time is held while the rows are written, so the copy runs
//! in a work arena far smaller than its source.

use super::*;

/// An engine whose work arena, where a top-level write runs, is 512 KiB.
fn streamed_engine(label: &str) -> (Engine, Budget, TxnState) {
    let mut config = test_config(label);
    config.txn_rows = 32_768;
    config.work_arena_bytes = 512 << 10;
    config.wal_bytes = 32 << 20;
    config.memtable_bytes = 16 << 20;
    config.row_overlay_slots = 32_768;
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 27));
    let engine = Engine::new(&config, &mut budget).unwrap();
    let txn = TxnState::new(&mut budget, config.txn_rows).unwrap();
    (engine, budget, txn)
}

/// Moves committed rows out of the shared row overlay.
fn checkpoint(engine: &mut Engine) {
    crate::mem::guard::forbid_alloc(|| engine.checkpoint()).unwrap();
}

fn run(engine: &mut Engine, budget: &mut Budget, txn: &mut TxnState, sql: &str) -> Vec<u8> {
    run_txn_bytes(engine, budget, txn, sql)
}

fn rows(output: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(output);
    assert!(!text.contains("ERROR"), "{text}");
    data_rows(output)
}

fn success(engine: &mut Engine, budget: &mut Budget, txn: &mut TxnState, sql: &str) -> String {
    let output = run(engine, budget, txn, sql);
    let text = String::from_utf8_lossy(&output).into_owned();
    assert!(!text.contains("ERROR"), "{sql}: {text}");
    text
}

#[test]
fn insert_select_streams_its_source_in_fixed_memory() {
    let (mut engine, mut budget, mut txn) = streamed_engine("streamed-insert-memory");
    success(
        &mut engine,
        &mut budget,
        &mut txn,
        "CREATE TABLE stream_src(id integer, label text); \
         CREATE TABLE stream_dst(id integer, label text)",
    );
    // About 620 KB of encoded source rows: more than the whole work arena.
    let text = success(
        &mut engine,
        &mut budget,
        &mut txn,
        "INSERT INTO stream_src \
         SELECT g, repeat('x', 120) || g FROM generate_series(1, 4000) g",
    );
    assert!(text.contains("INSERT 0 4000"), "{text}");
    checkpoint(&mut engine);
    let text = success(
        &mut engine,
        &mut budget,
        &mut txn,
        "INSERT INTO stream_dst SELECT id, label FROM stream_src WHERE id % 2 = 0",
    );
    assert!(text.contains("INSERT 0 2000"), "{text}");
    checkpoint(&mut engine);
    // The source reads the table as of the statement's start.
    let text = success(
        &mut engine,
        &mut budget,
        &mut txn,
        "INSERT INTO stream_src SELECT id + 4000, label FROM stream_src",
    );
    assert!(text.contains("INSERT 0 4000"), "{text}");
    checkpoint(&mut engine);
    let output = run(
        &mut engine,
        &mut budget,
        &mut txn,
        "SELECT count(*), sum(id), min(id), max(id) FROM stream_src",
    );
    assert_eq!(rows(&output), ["8000|32004000|1|8000"]);
    let output = run(
        &mut engine,
        &mut budget,
        &mut txn,
        "SELECT count(*), sum(id), min(label), max(label) FROM stream_dst",
    );
    assert_eq!(
        rows(&output),
        [format!(
            "2000|4002000|{}10|{}998",
            "x".repeat(120),
            "x".repeat(120)
        )]
    );
    // A source calling a volatile built-in reads differently each time it
    // runs, so it materializes its rows first; they do not fit this arena.
    let output = run(
        &mut engine,
        &mut budget,
        &mut txn,
        "INSERT INTO stream_dst SELECT id, label || (random() < 2)::text FROM stream_src",
    );
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("C54000"), "{text}");
    let output = run(
        &mut engine,
        &mut budget,
        &mut txn,
        "SELECT count(*) FROM stream_dst",
    );
    assert_eq!(rows(&output), ["2000"]);
}

/// A streamed source restarts after a routine call in the rows and skips the
/// rows earlier attempts wrote; each row's trigger runs once.
#[test]
fn streamed_insert_sources_resume_after_routine_calls() {
    let (mut engine, mut budget, mut txn) = streamed_engine("streamed-insert-resume");
    success(
        &mut engine,
        &mut budget,
        &mut txn,
        "CREATE TABLE resume_src(id integer, label text); \
         CREATE TABLE resume_dst(id integer, label text); \
         CREATE TABLE resume_log(id integer, seen bigint); \
         CREATE FUNCTION resume_note() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
           INSERT INTO resume_log SELECT NEW.id, count(*) FROM resume_dst; \
           RETURN NEW; \
         END $$; \
         CREATE TRIGGER resume_note BEFORE INSERT ON resume_dst \
           FOR EACH ROW EXECUTE FUNCTION resume_note(); \
         INSERT INTO resume_src SELECT g, 'r' || g FROM generate_series(1, 50) g",
    );
    let text = success(
        &mut engine,
        &mut budget,
        &mut txn,
        "INSERT INTO resume_dst SELECT id, label FROM resume_src WHERE id > 10 ORDER BY id DESC",
    );
    assert!(text.contains("INSERT 0 40"), "{text}");
    // Each trigger saw the rows inserted before its own, as in PostgreSQL.
    let output = run(
        &mut engine,
        &mut budget,
        &mut txn,
        "SELECT count(*), min(seen), max(seen), \
                bool_and(seen = 50 - id) FROM resume_log",
    );
    assert_eq!(rows(&output), ["40|0|39|t"]);
}
