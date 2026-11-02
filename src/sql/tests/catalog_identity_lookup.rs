use super::*;

#[test]
fn catalog_identity_casts_keep_large_catalog_definitions_out_of_row_filters() {
    let mut config = test_config("catalog-identity-row-filters");
    config.max_tables = 1;
    config.max_views = 64;
    config.max_composites = 2;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE SCHEMA identity_moved; \
         CREATE TYPE identity_state AS ENUM ('ready'); \
         CREATE TYPE identity_pair AS (x integer,y integer); \
         CREATE TYPE identity_attribute AS (value bigint); \
         CREATE TABLE identity_values(state identity_state,point identity_pair,points identity_pair[]); \
         CREATE VIEW identity_view AS SELECT state FROM identity_values; \
         INSERT INTO identity_values VALUES('ready',ROW(3,4)::identity_pair,ARRAY[ROW(5,6)::identity_pair]); \
         ALTER TYPE identity_state SET SCHEMA identity_moved; \
         ALTER TYPE identity_pair SET SCHEMA identity_moved", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT pg_typeof(points)::text || ':' || \
           (SELECT atttypid::regtype::text FROM pg_attribute \
            WHERE attrelid='identity_attribute'::regclass AND attname='value') \
         FROM identity_values; \
         SELECT 'identity_view'::regclass::text,'identity_attribute'::regclass::text; \
         SELECT count(*) FROM pg_attribute \
           WHERE attrelid='identity_view'::regclass AND attname='state'",
            true
        )),
        [
            "identity_moved.identity_pair[]:bigint",
            "identity_view|identity_attribute",
            "1"
        ]
    );
}
