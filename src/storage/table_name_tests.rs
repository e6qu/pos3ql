//! Lookup, visibility and publication checks use the storage boundary directly.

use super::tests::{make_def, test_budget, test_config};
use super::*;
use std::time::Duration;

#[test]
fn table_name_publication_remaps_index_keys_and_includes_before_rebinding() {
    for (schema, name) in [
        ("public", "binding_renamed"),
        ("binding_schema", "binding_source"),
        ("binding_schema", "binding_renamed"),
    ] {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        storage
            .create_schema(SqlName::parse("binding_schema").unwrap())
            .unwrap();
        let table = storage
            .create_table(make_def(
                "binding_source",
                &[
                    ("discarded", ColType::Int4, false),
                    ("key", ColType::Int4, false),
                    ("payload", ColType::Int4, false),
                ],
            ))
            .unwrap();
        let mut index = IndexDef {
            schema: SqlName::parse("public").unwrap(),
            name: SqlName::parse("binding_index").unwrap(),
            table: SqlName::parse("binding_source").unwrap(),
            n_cols: 1,
            n_include_cols: 1,
            ..IndexDef::EMPTY
        };
        index.columns[0] = 1;
        index.include_columns[0] = 2;
        let slot = storage.create_index(index, 7).unwrap();
        storage.commit_index_create(slot);
        crate::mem::guard::forbid_alloc(|| {
            let mut definition = make_def(
                name,
                &[
                    ("key", ColType::Int4, false),
                    ("payload", ColType::Int4, false),
                ],
            );
            definition.schema = SqlName::parse(schema).unwrap();
            let mut mapping = [None; MAX_COLUMNS];
            mapping[1] = Some(SqlName::parse("key").unwrap());
            mapping[2] = Some(SqlName::parse("payload").unwrap());
            let first_change = storage
                .write_table_def(
                    storage.select_table_definition(table, 7).0,
                    definition,
                    &mapping,
                    true,
                )
                .unwrap();
            let pending = storage.index_visible_to(slot, 7).unwrap();
            assert_eq!((pending.columns[0], pending.include_columns[0]), (0, 1));
            let committed = storage.index_visible_to(slot, 8).unwrap();
            assert_eq!((committed.columns[0], committed.include_columns[0]), (1, 2));
            storage.rollback_table_def(first_change);
            assert_eq!(storage.index_visible_to(slot, 7).unwrap().columns[0], 1);
            let second_change = storage
                .write_table_def(
                    storage.select_table_definition(table, 7).0,
                    definition,
                    &mapping,
                    true,
                )
                .unwrap();
            assert_eq!(storage.commit_table_def(second_change).unwrap(), Some(true));
            let committed = storage.index_visible_to(slot, 8).unwrap();
            assert_eq!(
                (committed.schema.as_str(), committed.table.as_str()),
                (schema, name)
            );
            assert_eq!((committed.columns[0], committed.include_columns[0]), (0, 1));
        });
    }
}

fn fixture() -> (Storage, usize, usize) {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let unrelated = storage
        .create_table(make_def(
            "unrelated_lookup",
            &[("id", ColType::Int4, false)],
        ))
        .unwrap();
    let target = storage
        .create_table(make_def("lookup_target", &[("id", ColType::Int4, false)]))
        .unwrap();
    (storage, unrelated, target)
}

fn assert_unrelated_lookup_progress(storage: &Storage, unrelated: usize, target: usize) {
    assert!(unrelated < target);
    let held = storage.tables[unrelated].definition.write();
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let result = crate::mem::guard::forbid_alloc(|| {
                (
                    storage.find_visible("public", "lookup_target", 7),
                    storage.find_table("public", "lookup_target"),
                    storage.find_visible("public", "missing_lookup", 7),
                    storage.find_table("public", "missing_lookup"),
                )
            });
            send.send(result).unwrap();
        });
        let progress = receive.recv_timeout(Duration::from_secs(5));
        let before_release = progress.is_ok();
        drop(held);
        let result = match progress {
            Ok(result) => result,
            Err(_) => receive.recv_timeout(Duration::from_secs(5)).unwrap(),
        };
        assert_eq!(result, (Some(target), Some(target), None, None));
        assert!(
            before_release,
            "name lookup waited for an unrelated table definition"
        );
    });
}

#[test]
fn table_name_lookup_skips_unrelated_definition_writers() {
    let (storage, unrelated, target) = fixture();
    assert_unrelated_lookup_progress(&storage, unrelated, target);
}

#[test]
fn table_name_lookup_skips_same_name_in_another_database() {
    let (storage, unrelated, target) = fixture();
    {
        let mut definition = storage.tables[unrelated].definition.write();
        definition
            .edit_committed(storage.reserve_table_definition_revision().unwrap())
            .name = SqlName::parse("lookup_target").unwrap();
        definition.identity.database = DatabaseOid::TEMPLATE1;
    }
    assert_unrelated_lookup_progress(&storage, unrelated, target);
}

#[test]
fn table_name_lookup_selected_writer_revalidates_name_and_incarnation() {
    let (storage, _, target) = fixture();
    let mut held = storage.tables[target].definition.write();
    let (started_send, started_receive) = std::sync::mpsc::sync_channel(1);
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let storage = &storage;
        scope.spawn(move || {
            started_send.send(()).unwrap();
            let result = crate::mem::guard::forbid_alloc(|| {
                storage.find_visible("public", "lookup_target", 7)
            });
            send.send(result).unwrap();
        });
        started_receive
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        let premature = receive.recv_timeout(Duration::from_millis(100));
        held.edit_committed(storage.reserve_table_definition_revision().unwrap())
            .name = SqlName::parse("lookup_replacement").unwrap();
        held.identity.created_at = held.identity.created_at.checked_add(1).unwrap();
        drop(held);
        let result = premature.ok();
        let waited = result.is_none();
        let selected = if let Some(result) = result {
            result
        } else {
            receive.recv_timeout(Duration::from_secs(5)).unwrap()
        };
        assert!(
            waited,
            "a name candidate escaped before definition validation"
        );
        assert_eq!(selected, None);
    });
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(storage.find_visible("public", "lookup_target", 7), None);
        assert_eq!(
            storage.find_table("public", "lookup_replacement"),
            Some(target)
        );
    });
}

#[test]
fn table_name_lookup_tracks_create_rename_rollback_and_drop_visibility() {
    let (storage, _, target) = fixture();
    crate::mem::guard::forbid_alloc(|| {
        let created = storage
            .create_table_in(make_def("private_lookup", &[]), 7)
            .unwrap();
        assert_eq!(
            storage.find_visible("public", "private_lookup", 7),
            Some(created)
        );
        assert_eq!(storage.find_visible("public", "private_lookup", 8), None);
        assert_eq!(storage.find_table("public", "private_lookup"), None);
        let mut updated = *storage.table_def(created, 7);
        updated.name = SqlName::parse("first_lookup").unwrap();
        let first_change = storage
            .write_table_def(
                storage.select_table_definition(created, 7).0,
                updated,
                &[None; MAX_COLUMNS],
                false,
            )
            .unwrap();
        updated.name = SqlName::parse("second_lookup").unwrap();
        let second_change = storage
            .write_table_def(
                storage.select_table_definition(created, 7).0,
                updated,
                &[None; MAX_COLUMNS],
                false,
            )
            .unwrap();
        assert_eq!(storage.find_visible("public", "private_lookup", 7), None);
        assert_eq!(
            storage.find_visible("public", "second_lookup", 7),
            Some(created)
        );
        storage.rollback_table_def(second_change);
        assert_eq!(storage.find_visible("public", "second_lookup", 7), None);
        assert_eq!(
            storage.find_visible("public", "first_lookup", 7),
            Some(created)
        );
        storage.rollback_table_def(first_change);
        storage.commit_create(created);
        assert_eq!(
            storage.find_table("public", "private_lookup"),
            Some(created)
        );
        let dropped = storage.drop_table_in(created, 8).unwrap();
        assert_eq!(storage.find_visible("public", "private_lookup", 8), None);
        assert_eq!(
            storage.find_visible("public", "private_lookup", 7),
            Some(created)
        );
        assert_eq!(
            storage.find_table("public", "private_lookup"),
            Some(created)
        );
        storage.finish_table_drop(dropped, crate::storage::CatalogDdlDecision::Rollback);
        storage.commit_drop(created);
        assert_eq!(storage.find_table("public", "private_lookup"), None);
        let replacement = storage
            .create_table_in(make_def("reused_lookup", &[]), 9)
            .unwrap();
        assert_eq!(replacement, created);
        assert_eq!(storage.find_visible("public", "private_lookup", 9), None);
        assert_eq!(
            storage.find_visible("public", "reused_lookup", 9),
            Some(replacement)
        );
        storage.rollback_create(replacement);

        let mut committed = *storage.table_def(target, 9);
        committed.name = SqlName::parse("committed_lookup").unwrap();
        let mut mapping = [None; MAX_COLUMNS];
        mapping[0] = Some(SqlName::parse("id").unwrap());
        let third_change = storage
            .write_table_def(
                storage.select_table_definition(target, 9).0,
                committed,
                &mapping,
                false,
            )
            .unwrap();
        assert_eq!(
            storage.find_visible("public", "lookup_target", 8),
            Some(target)
        );
        assert_eq!(storage.find_visible("public", "committed_lookup", 8), None);
        assert_eq!(storage.find_visible("public", "lookup_target", 9), None);
        assert_eq!(
            storage.find_visible("public", "committed_lookup", 9),
            Some(target)
        );
        assert_eq!(storage.commit_table_def(third_change).unwrap(), Some(false));
        assert_eq!(storage.find_table("public", "lookup_target"), None);
        assert_eq!(
            storage.find_table("public", "committed_lookup"),
            Some(target)
        );
    });
}

#[test]
fn table_name_lookup_exclusive_edits_publish_names_database_and_stamp() {
    let (mut storage, _, target) = fixture();
    crate::mem::guard::forbid_alloc(|| {
        {
            let revision = storage.reserve_table_definition_revision().unwrap();
            let mut definition = storage.tables[target].definition_mut(revision);
            definition.schema = SqlName::parse("lookup_schema").unwrap();
            definition.name = SqlName::parse("exclusive_lookup").unwrap();
        }
        assert_eq!(storage.find_table("public", "lookup_target"), None);
        assert_eq!(
            storage.find_table("lookup_schema", "exclusive_lookup"),
            Some(target)
        );
        let prior_database = current_database();
        {
            let mut identity = storage.tables[target].identity_mut();
            identity.database = DatabaseOid::TEMPLATE1;
            identity.created_at = identity.created_at.checked_add(1).unwrap();
        }
        assert_eq!(
            storage.find_table("lookup_schema", "exclusive_lookup"),
            None
        );
        crate::sql::set_execution_database_oid(DatabaseOid::TEMPLATE1);
        let other = storage.find_table("lookup_schema", "exclusive_lookup");
        crate::sql::set_execution_database_oid(prior_database);
        assert_eq!(other, Some(target));
        storage.tables[target].ownership_mut().owner = BOOTSTRAP_ROLE;
        storage.tables[target].identity_mut().existence = CatalogDdlState::Absent;
        assert_eq!(
            storage.find_table("lookup_schema", "exclusive_lookup"),
            None
        );
    });
}

#[test]
fn table_name_lookup_views_are_included_in_exact_startup_memory() {
    let mut config = test_config();
    config.query_workspace_slots = 2;
    let expected = config.memtable_bytes + Storage::extra_budget_bytes(&config);
    let mut budget = Budget::new(expected);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    storage.configure_collation(&config, &mut budget).unwrap();
    assert_eq!(budget.remaining(), 0);
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(storage.find_table("public", "not_created"), None);
        assert_eq!(storage.find_visible("public", "not_created", 7), None);
    });
}
