//! Commit-time catalog capture compares only the tables a change reaches.

use super::*;
use crate::sql::catalog_snapshot::{TableSelection, reached_tables};

fn selected(engine: &Engine, session: &ConfiguredTransactionSession, arena: &Arena) -> Vec<String> {
    let selection = reached_tables(&engine.storage, &session.transaction, arena).unwrap();
    let TableSelection::Reached(_) = selection else {
        panic!("a table change selects its reached tables");
    };
    let mut names = (0..engine.storage.table_count())
        .filter(|&slot| selection.includes(slot))
        .map(|slot| {
            engine
                .storage
                .table_def(slot, session.transaction.txid)
                .name
                .as_str()
                .to_string()
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn table_changes_reach_their_families_and_one_foreign_key_hop() {
    let config = test_config("catalog-capture-selection");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let arena = Arena::new(&mut budget, "capture selection test", 1 << 20).unwrap();
    session.success(
        &mut engine,
        "CREATE TABLE ccs_a (id int PRIMARY KEY); \
         CREATE TABLE ccs_b (id int PRIMARY KEY, a int REFERENCES ccs_a); \
         CREATE TABLE ccs_c (id int PRIMARY KEY, b int REFERENCES ccs_b); \
         CREATE TABLE ccs_d (id int PRIMARY KEY, c int REFERENCES ccs_c); \
         CREATE TABLE ccs_p (id int, c int REFERENCES ccs_c) PARTITION BY RANGE (id); \
         CREATE TABLE ccs_p1 PARTITION OF ccs_p FOR VALUES FROM (0) TO (10); \
         CREATE TABLE ccs_p2 PARTITION OF ccs_p FOR VALUES FROM (10) TO (20); \
         CREATE TABLE ccs_unrelated (id int)",
        false,
    );
    session.success(
        &mut engine,
        "BEGIN; ALTER TABLE ccs_b ADD COLUMN note text",
        false,
    );
    assert_eq!(
        selected(&engine, &session, &arena),
        ["ccs_a", "ccs_b", "ccs_c"]
    );
    session.success(&mut engine, "ROLLBACK", false);

    // A partition reaches its whole family, and the family's keys one hop.
    session.success(
        &mut engine,
        "BEGIN; CREATE INDEX ccs_p1_c ON ccs_p1 (c)",
        false,
    );
    assert_eq!(
        selected(&engine, &session, &arena),
        ["ccs_c", "ccs_p", "ccs_p1", "ccs_p2"]
    );
    session.success(&mut engine, "ROLLBACK", false);

    // A dropped table still reaches the tables its committed keys named.
    session.success(&mut engine, "BEGIN; DROP TABLE ccs_d", false);
    assert_eq!(selected(&engine, &session, &arena), ["ccs_c", "ccs_d"]);
    session.success(&mut engine, "ROLLBACK", false);

    // An object not owned by one table selects every table.
    session.success(
        &mut engine,
        "BEGIN; ALTER TABLE ccs_a ADD COLUMN note text; \
         CREATE FUNCTION ccs_f() RETURNS int LANGUAGE sql AS 'SELECT 1'",
        false,
    );
    assert!(matches!(
        reached_tables(&engine.storage, &session.transaction, &arena).unwrap(),
        TableSelection::All
    ));
    session.success(&mut engine, "ROLLBACK", false);
}

#[test]
fn visible_table_names_resolve_as_find_visible_does() {
    let config = test_config("visible-table-names");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let arena = Arena::new(&mut budget, "visible table names test", 1 << 20).unwrap();
    session.success(
        &mut engine,
        "CREATE SCHEMA vtn; CREATE TABLE vtn_one (id int); CREATE TABLE vtn.vtn_one (id int); \
         CREATE TABLE vtn_two (id int); CREATE TABLE vtn_gone (id int)",
        false,
    );
    session.success(
        &mut engine,
        "BEGIN; ALTER TABLE vtn_two RENAME TO vtn_renamed; DROP TABLE vtn_gone; \
         CREATE TABLE vtn_new (id int)",
        false,
    );
    for txid in [0, session.transaction.txid] {
        let names = engine.storage.visible_table_names(txid, &arena).unwrap();
        for (schema, name) in [
            ("public", "vtn_one"),
            ("vtn", "vtn_one"),
            ("public", "vtn_two"),
            ("public", "vtn_renamed"),
            ("public", "vtn_gone"),
            ("public", "vtn_new"),
            ("public", "vtn_absent"),
        ] {
            assert_eq!(
                names.find(&engine.storage, schema, name),
                engine.storage.find_visible(schema, name, txid),
                "{schema}.{name} at {txid}"
            );
        }
    }
    session.success(&mut engine, "ROLLBACK", false);
}

/// A domain over a table's row type resolves its I/O routines through that
/// row type's `pg_type` row while another table's creation is captured.
#[test]
fn a_table_change_captures_domains_over_other_row_types() {
    let config = test_config("capture-domain-row-types");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE cdr_row (a int, b text); CREATE DOMAIN cdr_domain AS cdr_row; \
         CREATE TABLE cdr_user (x cdr_domain); DROP TABLE cdr_user",
        false,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT count(*) FROM pg_type WHERE typname = 'cdr_domain'",
            false,
        )),
        ["1"]
    );
}
