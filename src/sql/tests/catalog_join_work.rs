use super::*;

fn join_work(
    session: &mut ConfiguredTransactionSession,
    engine: &mut Engine,
    sql: &str,
) -> (Vec<String>, u64, u64) {
    let candidates = crate::sql::query::JOIN_CANDIDATES.with(|count| count.replace(0));
    let builds = crate::sql::query::IMAGE_BUILDS.with(|count| count.replace(0));
    let rows = data_rows(&session.success(engine, sql, true));
    let used = crate::sql::query::JOIN_CANDIDATES.with(|count| count.replace(candidates));
    let built = crate::sql::query::IMAGE_BUILDS.with(|count| count.replace(builds));
    (rows, used, built)
}

/// psql's `\d`, `\dC` and `\dT` join and correlate catalog relations, which
/// are materialized and have no index. Each level probes its rows by key and
/// each statement builds one image per catalog, so the work grows with the
/// result rather than with the product of the catalog sizes.
#[test]
fn psql_catalog_joins_probe_keys_and_reuse_statement_images() {
    let config = test_config("catalog-join-work");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE ct (a text, b text); CREATE INDEX ci ON ct (b); DROP INDEX ci;",
        true,
    );
    // `\d ct`: a comma join over two pg_class scans and a LEFT JOIN, with
    // every WHERE conjunct on the preserved side.
    let (rows, candidates, _) = join_work(
        &mut session,
        &mut engine,
        "SELECT c2.relname, con.conname \
         FROM pg_catalog.pg_class c, pg_catalog.pg_class c2, pg_catalog.pg_index i \
           LEFT JOIN pg_catalog.pg_constraint con ON (conrelid = i.indrelid \
             AND conindid = i.indexrelid AND contype IN ('p','u','x')) \
         WHERE c.oid = 'ct'::regclass AND c.oid = i.indrelid AND i.indexrelid = c2.oid \
         ORDER BY c2.relname",
    );
    assert!(rows.is_empty(), "{rows:?}");
    assert!(candidates < 2_000, "{candidates} candidate rows");
    // `\dC`: a chain of LEFT JOINs from pg_cast, one keyed probe per level.
    let (rows, candidates, _) = join_work(
        &mut session,
        &mut engine,
        "SELECT count(*), count(p.proname), count(ns.nspname), count(nt.nspname) \
         FROM pg_catalog.pg_cast c LEFT JOIN pg_catalog.pg_proc p ON c.castfunc = p.oid \
           LEFT JOIN pg_catalog.pg_type ts ON c.castsource = ts.oid \
           LEFT JOIN pg_catalog.pg_namespace ns ON ns.oid = ts.typnamespace \
           LEFT JOIN pg_catalog.pg_type tt ON c.casttarget = tt.oid \
           LEFT JOIN pg_catalog.pg_namespace nt ON nt.oid = tt.typnamespace",
    );
    let casts: usize =
        data_rows(&session.success(&mut engine, "SELECT count(*) FROM pg_catalog.pg_cast", true))
            [0]
        .parse()
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].starts_with(&format!("{casts}|")), "{rows:?}");
    assert!(candidates < 8 * casts as u64, "{candidates} candidate rows");
    // A regproc column probes pg_proc's oid key.
    let (rows, candidates, _) = join_work(
        &mut session,
        &mut engine,
        "SELECT count(*) FROM pg_catalog.pg_amproc ap \
           LEFT JOIN pg_catalog.pg_proc p ON ap.amproc = p.oid WHERE p.oid IS NULL",
    );
    assert_eq!(rows, ["0"]);
    assert!(candidates < 4_000, "{candidates} candidate rows");
    // `\dT`: correlated subqueries reuse one image and probe by the outer key.
    let (rows, candidates, builds) = join_work(
        &mut session,
        &mut engine,
        "SELECT count(*) FROM pg_catalog.pg_type t \
         WHERE (t.typrelid = 0 OR (SELECT c.relkind = 'c' FROM pg_catalog.pg_class c \
                                   WHERE c.oid = t.typrelid)) \
           AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_type el \
                           WHERE el.oid = t.typelem AND el.typarray = t.oid)",
    );
    let types: u64 =
        data_rows(&session.success(&mut engine, "SELECT count(*) FROM pg_catalog.pg_type", true))
            [0]
        .parse()
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(candidates < 4 * types, "{candidates} candidate rows");
    assert!(builds <= 3, "{builds} catalog images");
}

/// Keyed probes and pushed WHERE conjuncts keep outer-join results exact.
#[test]
fn keyed_outer_joins_keep_null_padding_and_where_semantics() {
    let config = test_config("catalog-join-semantics");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE l (k int, v text); CREATE TABLE r (k int, w text); \
         INSERT INTO l VALUES (1, 'a'), (2, 'b'), (NULL, 'n'); \
         INSERT INTO r VALUES (1, 'x'), (1, 'y'), (3, 'z')",
        true,
    );
    for (sql, expected) in [
        (
            "WITH a AS MATERIALIZED (SELECT * FROM l), b AS MATERIALIZED (SELECT * FROM r) \
             SELECT a.v, b.w FROM a LEFT JOIN b ON a.k = b.k ORDER BY 1, 2",
            &["a|x", "a|y", "b|NULL", "n|NULL"][..],
        ),
        (
            "WITH a AS MATERIALIZED (SELECT * FROM l), b AS MATERIALIZED (SELECT * FROM r) \
             SELECT a.v FROM a LEFT JOIN b ON a.k = b.k WHERE b.k IS NULL ORDER BY 1",
            &["b", "n"][..],
        ),
        (
            "WITH a AS MATERIALIZED (SELECT * FROM l), b AS MATERIALIZED (SELECT * FROM r), \
                  c AS MATERIALIZED (SELECT * FROM r) \
             SELECT a.v, b.w, c.w FROM a, b LEFT JOIN c ON b.k = c.k AND c.w <> b.w \
             WHERE a.k = b.k AND a.v = 'a' ORDER BY 2, 3",
            &["a|x|y", "a|y|x"][..],
        ),
        (
            "WITH a AS MATERIALIZED (SELECT * FROM l), b AS MATERIALIZED (SELECT * FROM r) \
             SELECT a.v, b.w FROM a RIGHT JOIN b ON a.k = b.k WHERE a.v IS NULL ORDER BY 2",
            &["NULL|z"][..],
        ),
        (
            "WITH a AS MATERIALIZED (SELECT * FROM l), b AS MATERIALIZED (SELECT * FROM r), \
                  c AS MATERIALIZED (SELECT * FROM r) \
             SELECT a.v, b.w, c.w FROM a LEFT JOIN b ON a.k = b.k \
               LEFT JOIN c ON c.k = b.k AND c.w <> b.w ORDER BY 1, 2, 3",
            &["a|x|y", "a|y|x", "b|NULL|NULL", "n|NULL|NULL"][..],
        ),
        (
            "SELECT l.v, (SELECT count(*) FROM pg_catalog.pg_class c \
                          WHERE c.oid = l.k::oid) FROM l ORDER BY 1",
            &["a|0", "b|0", "n|0"][..],
        ),
    ] {
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            expected,
            "{sql}"
        );
    }
}

/// A routine that changes the catalog during a statement reads its change,
/// although the statement reuses catalog images between its own scans.
// PostgreSQL 18.6 returns the same rows for this statement.
#[test]
fn statement_catalog_images_follow_changes_made_during_the_statement() {
    let config = test_config("catalog-image-changes");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE FUNCTION mk(n int) RETURNS bigint LANGUAGE plpgsql AS $$ \
         BEGIN EXECUTE format('CREATE TABLE made_%s (a int)', n); \
         RETURN (SELECT count(*) FROM pg_catalog.pg_class WHERE relname = 'made_' || n); END $$",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT g, mk(g) FROM generate_series(1, 3) g ORDER BY g",
            true,
        )),
        ["1|1", "2|1", "3|1"]
    );
}
