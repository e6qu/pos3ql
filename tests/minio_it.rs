//! Integration test against a real MinIO instance.
//!
//! Skipped unless `POS3QL_MINIO_ENDPOINT` is set (e.g. `127.0.0.1:19100`);
//! the external test harness starts the container and sets the variable.
//! Credentials default to minioadmin/minioadmin, overridable via
//! `POS3QL_MINIO_ACCESS_KEY` / `POS3QL_MINIO_SECRET_KEY`.

use pos3ql::config::Config;
use pos3ql::mem::{Arena, Budget, FixedBuf};
use pos3ql::pg::respond::Responder;
use pos3ql::s3::{Precondition, S3Client};
use pos3ql::sql::Engine;

fn client() -> Option<S3Client> {
    let endpoint = std::env::var("POS3QL_MINIO_ENDPOINT").ok()?;
    let mut config = Config::default_dev();
    config.s3_endpoint = endpoint;
    config.s3_bucket =
        std::env::var("POS3QL_MINIO_BUCKET").unwrap_or_else(|_| "pos3ql".to_string());
    config.s3_access_key =
        std::env::var("POS3QL_MINIO_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".to_string());
    config.s3_secret_key =
        std::env::var("POS3QL_MINIO_SECRET_KEY").unwrap_or_else(|_| "minioadmin".to_string());
    let mut budget = Budget::new(16 << 20);
    Some(S3Client::new(&config, &mut budget).unwrap())
}

fn engine_config(run: &str, data_dir: &str) -> Option<Config> {
    let endpoint = std::env::var("POS3QL_MINIO_ENDPOINT").ok()?;
    let dir = std::env::temp_dir().join(format!(
        "pos3ql-ckpt-{}-{run}-{data_dir}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut config = Config::default_dev();
    config.data_dir = dir.to_str().unwrap().to_string();
    config.memtable_bytes = 1 << 20;
    config.max_tables = 8;
    config.table_rows = 4096;
    config.wal_bytes = 1 << 20;
    config.wal_buffer_bytes = 1 << 14;
    config.s3_on = true;
    config.s3_endpoint = endpoint;
    config.s3_bucket =
        std::env::var("POS3QL_MINIO_BUCKET").unwrap_or_else(|_| "pos3ql".to_string());
    config.s3_prefix = format!("ckpt-it/{}-{run}/", std::process::id());
    config.s3_access_key =
        std::env::var("POS3QL_MINIO_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".to_string());
    config.s3_secret_key =
        std::env::var("POS3QL_MINIO_SECRET_KEY").unwrap_or_else(|_| "minioadmin".to_string());
    Some(config)
}

fn run_sql(engine: &mut Engine, budget: &mut Budget, sql_text: &str) -> String {
    let mut buf = FixedBuf::new(budget, "send", 1 << 18).unwrap();
    let arena = Arena::new(budget, "sql", 1 << 18).unwrap();
    let mut txn = pos3ql::sql::txn::TxnState::new(budget, 1024).unwrap();
    let mut pool = {
        let mut c = Config::default_dev();
        c.max_prepared = 2;
        c.prepared_bytes = 512;
        pos3ql::sql::prep::SqlPreparedPool::new(&c, budget).unwrap()
    };
    let mut guc = pos3ql::sql::guc::GucState::new();
    let mut resp = Responder::new(&mut buf);
    engine
        .execute_simple(sql_text, &arena, &mut txn, &mut pool, &mut guc, &mut resp)
        .unwrap();
    String::from_utf8_lossy(buf.readable()).to_string()
}

#[test]
fn rpo_zero_disk_loss_recovery() {
    // wal_upload = on: writes after a checkpoint survive TOTAL disk loss
    // (no local WAL), because committed batches were uploaded.
    let Some(mut cfg) = engine_config("rpo", "a") else {
        eprintln!("POS3QL_MINIO_ENDPOINT not set; skipping");
        return;
    };
    cfg.wal_upload = true;

    {
        let mut budget = Budget::new(64 << 20);
        let mut e = Engine::new(&cfg, &mut budget).unwrap();
        run_sql(&mut e, &mut budget, "CREATE TABLE ledger (id int, amount int)");
        run_sql(&mut e, &mut budget, "INSERT INTO ledger VALUES (1, 100)");
        run_sql(&mut e, &mut budget, "CHECKPOINT");
        // These commits land only in uploaded WAL segments, not any SST.
        run_sql(&mut e, &mut budget, "INSERT INTO ledger VALUES (2, 200)");
        run_sql(&mut e, &mut budget, "UPDATE ledger SET amount = 150 WHERE id = 1");
        run_sql(&mut e, &mut budget, "INSERT INTO ledger VALUES (3, 300)");
    }
    // Total disk loss: brand-new empty data dir, same bucket+prefix.
    let mut cfg2 = cfg.clone();
    let dir = std::env::temp_dir().join(format!("pos3ql-rpo-{}-wiped", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    cfg2.data_dir = dir.to_str().unwrap().to_string();
    {
        let mut budget = Budget::new(64 << 20);
        let mut e = Engine::new(&cfg2, &mut budget).unwrap();
        let out = run_sql(&mut e, &mut budget, "SELECT id, amount FROM ledger ORDER BY id");
        // The post-checkpoint tail must be present despite the wipe.
        assert!(out.contains("150"), "updated row lost: {out}");
        assert!(out.contains("200") && out.contains("300"), "inserts lost: {out}");
    }
}

#[test]
fn delta_checkpoint_carries_clean_tables() {
    let Some(cfg) = engine_config("delta", "a") else {
        eprintln!("POS3QL_MINIO_ENDPOINT not set; skipping");
        return;
    };
    let mut budget = Budget::new(64 << 20);
    let mut e = Engine::new(&cfg, &mut budget).unwrap();
    run_sql(&mut e, &mut budget, "CREATE TABLE stable (id int, v text)");
    run_sql(&mut e, &mut budget, "CREATE TABLE churn (id int, v text)");
    run_sql(&mut e, &mut budget, "INSERT INTO stable VALUES (1, 'permanent')");
    run_sql(&mut e, &mut budget, "INSERT INTO churn VALUES (1, 'a')");
    run_sql(&mut e, &mut budget, "CHECKPOINT");
    // Only `churn` changes; the next checkpoint must carry `stable`'s SST
    // forward and still cold-start correctly.
    for i in 2..10 {
        run_sql(&mut e, &mut budget, &format!("INSERT INTO churn VALUES ({i}, 'x')"));
    }
    run_sql(&mut e, &mut budget, "CHECKPOINT");

    // Cold start from the bucket: both tables intact.
    let mut cfg2 = cfg.clone();
    let dir = std::env::temp_dir().join(format!("pos3ql-delta-{}-b", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    cfg2.data_dir = dir.to_str().unwrap().to_string();
    let mut budget2 = Budget::new(64 << 20);
    let mut e2 = Engine::new(&cfg2, &mut budget2).unwrap();
    let out = run_sql(&mut e2, &mut budget2, "SELECT v FROM stable");
    assert!(out.contains("permanent"), "carried-forward table lost: {out}");
    let out = run_sql(&mut e2, &mut budget2, "SELECT count(*) FROM churn");
    assert!(out.contains('9'), "churned table wrong: {out}");
}

#[test]
fn checkpoint_and_cold_start_from_bucket() {
    let Some(config_a) = engine_config("cold", "a") else {
        eprintln!("POS3QL_MINIO_ENDPOINT not set; skipping");
        return;
    };

    // Node A: write data, checkpoint, write a WAL tail after the checkpoint.
    {
        let mut budget = Budget::new(64 << 20);
        let mut e = Engine::new(&config_a, &mut budget).unwrap();
        run_sql(&mut e, &mut budget, "CREATE TABLE inventory (id int NOT NULL, name text, qty int)");
        run_sql(&mut e, &mut budget, "INSERT INTO inventory VALUES (1,'bolt',100),(2,'nut',200),(3,'washer',300)");
        run_sql(&mut e, &mut budget, "UPDATE inventory SET qty = 150 WHERE id = 1");
        // A view created before the checkpoint must persist via the manifest.
        run_sql(&mut e, &mut budget, "CREATE VIEW cheap AS SELECT name FROM inventory WHERE qty < 250");
        let out = run_sql(&mut e, &mut budget, "CHECKPOINT");
        assert!(out.contains("CHECKPOINT"), "{out}");
        // Tail after the checkpoint, covered only by the local WAL.
        run_sql(&mut e, &mut budget, "INSERT INTO inventory VALUES (4,'screw',400)");
        run_sql(&mut e, &mut budget, "DELETE FROM inventory WHERE id = 2");
    }

    // Node A restarts with its disk intact: manifest + WAL tail replay.
    {
        let mut budget = Budget::new(64 << 20);
        let mut e = Engine::new(&config_a, &mut budget).unwrap();
        let out = run_sql(&mut e, &mut budget, "SELECT id, name, qty FROM inventory ORDER BY id");
        assert!(out.contains("bolt") && out.contains("150"), "{out}");
        assert!(out.contains("screw"), "tail insert lost: {out}");
        assert!(!out.contains("nut"), "tail delete lost: {out}");
        // Checkpoint everything so the bucket alone carries full state.
        run_sql(&mut e, &mut budget, "CHECKPOINT");
    }

    // Node B: same bucket+prefix, EMPTY data dir — cold start.
    let mut config_b = config_a.clone();
    let dir_b = std::env::temp_dir().join(format!("pos3ql-ckpt-{}-cold-b", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir_b);
    config_b.data_dir = dir_b.to_str().unwrap().to_string();
    {
        let mut budget = Budget::new(64 << 20);
        let mut e = Engine::new(&config_b, &mut budget).unwrap();
        let out = run_sql(&mut e, &mut budget, "SELECT id, name, qty FROM inventory ORDER BY id");
        assert!(out.contains("bolt") && out.contains("150"), "{out}");
        assert!(out.contains("washer") && out.contains("screw"), "{out}");
        assert!(!out.contains("nut"), "{out}");
        // The view cold-started from the manifest and still expands (evaluated
        // over the current rows: only bolt has qty < 250).
        let vout = run_sql(&mut e, &mut budget, "SELECT name FROM cheap ORDER BY name");
        assert!(vout.contains("bolt"), "view lost on cold start: {vout}");
        assert!(!vout.contains("washer") && !vout.contains("screw"), "view filter wrong: {vout}");
        // New writes on the cold-started node keep working, and rowids
        // continue past the replayed ones.
        run_sql(&mut e, &mut budget, "INSERT INTO inventory VALUES (5,'rivet',500)");
        let out = run_sql(&mut e, &mut budget, "SELECT name FROM inventory ORDER BY id");
        assert!(out.contains("rivet"), "{out}");
    }
}

#[test]
fn put_get_range_list_cas_delete() {
    let Some(mut c) = client() else {
        eprintln!("POS3QL_MINIO_ENDPOINT not set; skipping MinIO integration test");
        return;
    };
    let run = std::process::id();

    // PUT + GET roundtrip.
    let key = format!("it/{run}/hello.txt");
    let etag = c.put(&key, b"hello object storage", Precondition::None).unwrap();
    assert!(!etag.as_str().is_empty());
    let got = c.get(&key, None).unwrap();
    assert_eq!(got.len, 20);
    assert_eq!(c.body_bytes(), b"hello object storage");

    // Ranged GET (inclusive).
    let got = c.get(&key, Some((6, 11))).unwrap();
    assert_eq!(c.body_bytes(), b"object");
    assert_eq!(got.len, 6);

    // Create-only precondition: second PUT must fail.
    let cas_key = format!("it/{run}/create-once");
    c.put(&cas_key, b"first", Precondition::IfNoneMatchAny).unwrap();
    let err = c
        .put(&cas_key, b"second", Precondition::IfNoneMatchAny)
        .unwrap_err();
    assert!(err.is_precondition_failed(), "{err}");

    // If-Match CAS: succeeds with the right etag, fails with a stale one.
    let etag1 = c.put(&cas_key, b"v1", Precondition::None).unwrap();
    let etag2 = c
        .put(&cas_key, b"v2", Precondition::IfMatch(etag1.as_str()))
        .unwrap();
    let err = c
        .put(&cas_key, b"v3", Precondition::IfMatch(etag1.as_str()))
        .unwrap_err();
    assert!(err.is_precondition_failed(), "{err}");
    assert_ne!(etag1.as_str(), etag2.as_str());

    // LIST sees both keys under the run prefix.
    let mut keys = Vec::new();
    let prefix = format!("it/{run}/");
    c.list(&prefix, |k| keys.push(k.to_string())).unwrap();
    assert!(keys.contains(&key), "{keys:?}");
    assert!(keys.contains(&cas_key), "{keys:?}");

    // DELETE; a second delete is idempotent; GET now 404s.
    c.delete(&key).unwrap();
    c.delete(&key).unwrap();
    let err = c.get(&key, None).unwrap_err();
    assert!(err.is_not_found(), "{err}");
    c.delete(&cas_key).unwrap();
}
