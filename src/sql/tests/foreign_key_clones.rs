use super::*;

const CLONES: &str = "SELECT conname || ':' || conrelid::regclass || ':' || confrelid::regclass || ':' \
     || conindid::regclass || ':' || coalesce((SELECT p.conname FROM pg_constraint p WHERE p.oid = c.conparentid), '-') \
     || ':' || oid FROM pg_constraint c WHERE contype = 'f'";

#[test]
fn referenced_partition_clones_keep_names_and_identities_across_recovery() {
    for (checkpoint, mode) in [
        (true, crate::config::TransactionMode::Mvcc),
        (false, crate::config::TransactionMode::Locking),
    ] {
        let name = format!("fkey-clones-{checkpoint}-{mode:?}");
        let mut config = test_config(&name);
        config.transaction_mode = mode;
        if checkpoint {
            config.object_store_on = true;
            config.object_store_sim = true;
            config.wal_upload = false;
            config.wal_upload_sync = false;
            config.object_store_bucket = format!("{name}-{}", std::process::id());
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        }
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 32 << 20);
        session.success(
            &mut engine,
            "CREATE TABLE pk (a int PRIMARY KEY) PARTITION BY RANGE (a); \
             CREATE TABLE pk1 PARTITION OF pk FOR VALUES FROM (0) TO (10); \
             CREATE TABLE pk2 PARTITION OF pk FOR VALUES FROM (10) TO (20) PARTITION BY RANGE (a); \
             CREATE TABLE pk21 PARTITION OF pk2 FOR VALUES FROM (10) TO (15); \
             CREATE TABLE other (a int CONSTRAINT pc_a_fkey_2 CHECK (a > 0)); \
             CREATE TABLE pc (a int REFERENCES pk)",
            true,
        );
        // A name a later constraint takes does not rename an existing clone.
        session.success(&mut engine, "ALTER TABLE pk DETACH PARTITION pk1", true);
        session.success(
            &mut engine,
            "CREATE TABLE late (a int CONSTRAINT pc_a_fkey_1 CHECK (a > 0)); \
             ALTER TABLE pk ATTACH PARTITION pk1 FOR VALUES FROM (0) TO (10)",
            true,
        );
        let expected = [
            "pc_a_fkey:pc:pk:pk_pkey:-",
            "pc_a_fkey_3:pc:pk2:pk2_pkey:pc_a_fkey",
            "pc_a_fkey_4:pc:pk21:pk21_pkey:pc_a_fkey_3",
            "pc_a_fkey_5:pc:pk1:pk1_pkey:pc_a_fkey",
        ];
        let before = session.success(&mut engine, CLONES, true);
        let before = data_rows(&before);
        let names = before
            .iter()
            .map(|row| row.rsplit_once(':').unwrap().0)
            .collect::<Vec<_>>();
        assert_eq!(names, expected, "{mode:?}");
        if checkpoint {
            assert!(engine.checkpoint().unwrap());
        }
        drop(session);
        drop(engine);
        // A cold start reads the checkpoint manifest; a warm one replays the
        // journal.
        if checkpoint {
            std::fs::remove_dir_all(&config.data_dir).unwrap();
        }
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 32 << 20);
        let after = session.success(&mut engine, CLONES, true);
        assert_eq!(data_rows(&after), before, "{mode:?}");
        session.success(
            &mut engine,
            "INSERT INTO pk VALUES (1), (12); INSERT INTO pc VALUES (12)",
            true,
        );
        let failed = session.execute(&mut engine, "DELETE FROM pk WHERE a = 12", true);
        let text = String::from_utf8_lossy(&failed);
        assert!(
            text.contains("foreign key constraint \"pc_a_fkey_4\""),
            "{text}"
        );
        let failed = session.execute(&mut engine, "DROP TABLE pk21", true);
        assert_eq!(
            error_sqlstates(&failed),
            ["2BP01"],
            "{}",
            String::from_utf8_lossy(&failed)
        );
        session.success(
            &mut engine,
            "DELETE FROM pc; ALTER TABLE pk2 DETACH PARTITION pk21",
            true,
        );
        let after = data_rows(&session.success(&mut engine, CLONES, true));
        assert_eq!(
            after,
            [before[0].clone(), before[1].clone(), before[3].clone()],
            "{mode:?}"
        );
    }
}
