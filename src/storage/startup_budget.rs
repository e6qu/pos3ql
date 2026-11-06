//! Overflow estimates must reach named startup validation without wrapping.

use super::*;
use crate::sql::Engine;

fn saturated(config: &Config) {
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(Storage::extra_budget_bytes(config), usize::MAX);
        assert_eq!(Engine::extra_budget_bytes(config), usize::MAX);
    });
}

#[test]
fn excessive_shared_resource_estimates_saturate_and_fail_named_startup() {
    for resource in [
        "row_work_bytes",
        "row_work_leases",
        "execution_control_bytes",
        "execution_control_leases",
        "definition_image_slots",
        "row_overlay_slots",
    ] {
        let mut config = tests::test_config();
        match resource {
            "row_work_bytes" => config.row_work_bytes = usize::MAX & !63,
            "row_work_leases" => config.row_work_leases = usize::MAX,
            "execution_control_bytes" => config.execution_control_bytes = usize::MAX & !63,
            "execution_control_leases" => config.execution_control_leases = usize::MAX,
            "definition_image_slots" => config.definition_image_slots = usize::MAX,
            "row_overlay_slots" => config.row_overlay_slots = usize::MAX,
            _ => unreachable!(),
        }
        saturated(&config);
        let mut budget = Budget::new(0);
        let error = Storage::new(&config, &mut budget, 0).err().unwrap();
        assert!(error.to_string().contains(resource), "{error}");
        assert_eq!(budget.used(), 0);
        let error = Engine::new(&config, &mut budget).err().unwrap();
        assert!(error.to_string().contains(resource), "{error}");
        assert_eq!(budget.used(), 0);
    }
}

#[test]
fn named_type_index_sentinel_survives_storage_and_engine_composition() {
    let mut config = tests::test_config();
    config.max_composites = usize::MAX;
    saturated(&config);
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(
            type_identities::TypeIdentityIndex::budget_bytes(0, 0, usize::MAX),
            usize::MAX
        );
        assert_eq!(
            crate::sql::exec::record_shape_pool_bytes(usize::MAX),
            usize::MAX
        );
    });
}

#[test]
fn component_counts_and_cross_products_saturate_before_composition() {
    let mut config = tests::test_config();
    config.max_prepared_transactions = usize::MAX;
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(object_lock_capacity(&config), usize::MAX);
        assert_eq!(
            crate::sql::two_phase::PreparedTransactions::budget_bytes(&config),
            usize::MAX
        );
        assert_eq!(
            crate::sql::lock::LockManager::budget_bytes(usize::MAX, usize::MAX),
            usize::MAX
        );
        assert_eq!(
            crate::sql::lock::AdvisoryLockManager::budget_bytes(usize::MAX, usize::MAX),
            usize::MAX
        );
        assert_eq!(
            crate::sql::notify::NotifyState::budget_bytes(usize::MAX, usize::MAX),
            usize::MAX
        );
        assert_eq!(
            spill_generations::SpillGenerationRoster::member_budget_bytes(usize::MAX),
            usize::MAX
        );
        assert_eq!(
            spill_generations::SpillGenerationSnapshot::budget_bytes(usize::MAX),
            usize::MAX
        );
        assert_eq!(
            HeapRelocationWorkspace::budget_bytes(usize::MAX),
            usize::MAX
        );
    });
    config = tests::test_config();
    config.query_workspace_slots = usize::MAX;
    assert_eq!(
        spill_generations::QueryRootPool::budget_bytes(&config),
        usize::MAX
    );
    config = tests::test_config();
    config.max_savepoints_per_transaction = usize::MAX;
    assert_eq!(
        foreign_statement_context_workspace_bytes(&config),
        usize::MAX
    );
    config = tests::test_config();
    config.max_routines = usize::MAX;
    assert_eq!(stored_query_dependency_budget_bytes(&config), usize::MAX);
    for resource in ["subscription_relation_capacity", "subscription_arena_bytes"] {
        config = tests::test_config();
        match resource {
            "subscription_relation_capacity" => config.subscription_relation_capacity = usize::MAX,
            "subscription_arena_bytes" => config.subscription_arena_bytes = usize::MAX,
            _ => unreachable!(),
        }
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                crate::pg::subscription_apply::RelationMap::budget_bytes(usize::MAX),
                usize::MAX
            );
            assert_eq!(
                crate::pg::subscription_apply::SubscriptionApply::budget_bytes(&config),
                usize::MAX
            );
            assert_eq!(
                config
                    .memory_plan(
                        crate::server::Server::budget_bytes(&config),
                        crate::sql::Engine::extra_budget_bytes(&config)
                    )
                    .total(),
                usize::MAX
            );
        });
    }
}

#[test]
fn valid_shared_capacity_changes_preserve_exact_component_costs() {
    let config = tests::test_config();
    let storage = Storage::extra_budget_bytes(&config);
    let engine = Engine::extra_budget_bytes(&config);
    assert_ne!(storage, usize::MAX);
    assert_ne!(engine, usize::MAX);
    let mut larger = config.clone();
    larger.row_work_bytes += 64;
    assert_eq!(Storage::extra_budget_bytes(&larger) - storage, 64);
    assert_eq!(Engine::extra_budget_bytes(&larger) - engine, 64);
    larger = config.clone();
    larger.row_work_leases += 1;
    let controls = crate::mem::lease_pool::LeasePool::budget_bytes(
        larger.row_work_bytes,
        larger.row_work_leases,
    ) - crate::mem::lease_pool::LeasePool::budget_bytes(
        config.row_work_bytes,
        config.row_work_leases,
    );
    assert_eq!(Storage::extra_budget_bytes(&larger) - storage, controls);
    assert_eq!(Engine::extra_budget_bytes(&larger) - engine, controls);
    larger = config.clone();
    larger.execution_control_bytes += 64;
    assert_eq!(Storage::extra_budget_bytes(&larger) - storage, 64);
    assert_eq!(Engine::extra_budget_bytes(&larger) - engine, 64);
    larger = config.clone();
    larger.execution_control_leases += 1;
    let controls = crate::mem::lease_pool::LeasePool::budget_bytes(
        larger.execution_control_bytes,
        larger.execution_control_leases,
    ) - crate::mem::lease_pool::LeasePool::budget_bytes(
        config.execution_control_bytes,
        config.execution_control_leases,
    );
    assert_eq!(Storage::extra_budget_bytes(&larger) - storage, controls);
    assert_eq!(Engine::extra_budget_bytes(&larger) - engine, controls);
    larger = config.clone();
    larger.definition_image_slots += 1;
    let image = Storage::table_definition_image_budget_bytes(&larger)
        - Storage::table_definition_image_budget_bytes(&config);
    assert_eq!(Storage::extra_budget_bytes(&larger) - storage, image);
    assert_eq!(Engine::extra_budget_bytes(&larger) - engine, image);
}
