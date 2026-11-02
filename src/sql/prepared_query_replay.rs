//! Restore stored query declarations without publishing a prepared transaction.

use super::eval::{SqlError, sqlstate};
use super::txn::{DdlUndo, TxnState};
use crate::mem::Arena;
use crate::sql_err;
use crate::storage::*;
use crate::util::StackStr;
use crate::wal::{TriggerTargetKind, WalOp};

pub(super) fn stage_query_record(
    storage: &mut Storage,
    transaction: &mut TxnState,
    operation: &WalOp<'_>,
    arena: &Arena,
) -> Result<bool, SqlError> {
    let txid = transaction.txid;
    match operation {
        WalOp::CreateView {
            identity,
            schema,
            name,
            columns,
            sql,
            path,
            security_invoker,
            security_barrier,
            check_option,
            dependencies,
        } => {
            let columns = columns
                .materialize()
                .ok_or_else(|| invalid("view columns"))?;
            let dependencies =
                storage.rebind_stored_query_dependencies(dependencies.materialize()?, txid)?;
            let sql = text::<VIEW_SQL_MAX>(sql)?;
            let creation_path = text::<128>(path)?;
            let publication = storage.restore_prepared_view(
                *identity,
                SqlName::parse(schema)?,
                SqlName::parse(name)?,
                ViewDefinition {
                    persistence: RelationPersistence::Permanent,
                    columns,
                    query: StoredQueryDefinition {
                        sql,
                        creation_path,
                        dependencies,
                    },
                    options: view_options(*security_invoker, *security_barrier, *check_option)?,
                },
                txid,
            )?;
            transaction.record_catalog_change(storage, DdlUndo::ViewChanged(publication))?;
        }
        WalOp::DropView { schema, name } => {
            let undo = storage
                .drop_view(schema, name, txid)?
                .ok_or_else(|| invalid("view source"))?;
            transaction.record_catalog_change(storage, DdlUndo::ViewDropped(undo))?;
        }
        WalOp::RenameView {
            schema,
            name,
            new_name,
        } => {
            let slot = storage
                .view_slot(schema, name, txid)
                .ok_or_else(|| invalid("view source"))?;
            let prior = storage.stage_view_rename(
                storage.prepare_view_edit(slot, txid)?,
                SqlName::parse(new_name)?,
                txid,
            )?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::ViewChanged(ViewPublication::Altered(prior)),
            )?;
        }
        WalOp::SetViewSchema {
            schema,
            name,
            new_schema,
        } => {
            let slot = storage
                .view_slot(schema, name, txid)
                .ok_or_else(|| invalid("view source"))?;
            let prior = storage.stage_view_schema(
                storage.prepare_view_edit(slot, txid)?,
                SqlName::parse(new_schema)?,
                txid,
            )?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::ViewChanged(ViewPublication::Altered(prior)),
            )?;
        }
        WalOp::SetViewColumns {
            schema,
            name,
            columns,
        } => {
            let slot = storage
                .view_slot(schema, name, txid)
                .ok_or_else(|| invalid("view source"))?;
            let columns = columns
                .materialize()
                .ok_or_else(|| invalid("view columns"))?;
            let prior = storage.stage_view_columns(
                storage.prepare_view_edit(slot, txid)?,
                columns,
                txid,
            )?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::ViewChanged(ViewPublication::Altered(prior)),
            )?;
        }
        WalOp::SetViewOptions {
            schema,
            name,
            security_invoker,
            security_barrier,
            check_option,
        } => {
            let slot = storage
                .view_slot(schema, name, txid)
                .ok_or_else(|| invalid("view source"))?;
            let prior = storage.stage_view_options(
                storage.prepare_view_edit(slot, txid)?,
                view_options(*security_invoker, *security_barrier, *check_option)?,
                txid,
            )?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::ViewChanged(ViewPublication::Altered(prior)),
            )?;
        }
        WalOp::SetRule {
            slot,
            created_at,
            target,
            table_schema,
            table,
            name,
            event,
            mode,
            enabled,
            source,
            condition,
            actions,
            action_count,
            returning_action,
            path,
            dependencies,
        } => {
            let target = rule_target(storage, *target, table_schema, table, txid)?;
            let dependencies =
                storage.rebind_stored_query_dependencies(dependencies.materialize()?, txid)?;
            let publication = storage.restore_prepared_rule(
                usize::from(*slot),
                *created_at,
                RuleDefinition {
                    name: SqlName::parse(name)?,
                    target,
                    event: *event,
                    mode: *mode,
                    enabled: *enabled,
                    source: text::<RULE_SQL_MAX>(source)?,
                    condition: *condition,
                    actions: *actions,
                    action_count: *action_count,
                    returning_action: *returning_action,
                    creation_path: text::<128>(path)?,
                },
                dependencies.view(),
                txid,
            )?;
            let prior_table_rule_txid = if matches!(publication, RulePublication::Created(_)) {
                match storage.begin_rule_history(target, txid) {
                    Ok(prior) => prior,
                    Err(error) => {
                        storage.discard_rule_create(publication);
                        return Err(error);
                    }
                }
            } else {
                None
            };
            transaction.record_catalog_change(
                storage,
                DdlUndo::RuleChanged {
                    publication,
                    prior_table_rule_txid,
                },
            )?;
        }
        WalOp::DropRule {
            target,
            table_schema,
            table,
            name,
        } => {
            let target = rule_target(storage, *target, table_schema, table, txid)?;
            let slot = storage
                .rule_slot(target, name, txid)
                .ok_or_else(|| invalid("rule source"))?;
            let change = storage.drop_rule(storage.prepare_rule_edit(slot, txid)?, txid)?;
            transaction.record_catalog_change(storage, DdlUndo::RuleDropped(change))?;
        }
        WalOp::CreateTrigger {
            identity,
            name,
            target,
            table_schema,
            table,
            function_schema,
            function,
            or_replace,
            constraint,
            constraint_timing,
            referenced_schema,
            referenced_table,
            timing,
            level,
            events,
            update_columns,
            old_table,
            new_table,
            when,
            arguments,
            argument_count,
        } => {
            let target = trigger_target(storage, *target, table_schema, table, txid)?;
            let function = storage
                .routine_slot_by_signature(function_schema, function, &[], txid)
                .ok_or_else(|| invalid("trigger function"))?;
            if !matches!(storage.routine(function).kind, RoutineKind::Trigger) {
                return Err(invalid("trigger function result"));
            }
            let kind = if *constraint {
                let referenced_table = match (referenced_schema, referenced_table) {
                    (Some(schema), Some(table)) => Some(
                        storage
                            .find_visible(schema, table, txid)
                            .ok_or_else(|| invalid("constraint trigger reference"))?
                            as u16,
                    ),
                    (None, None) => None,
                    _ => return Err(invalid("constraint trigger reference")),
                };
                TriggerKind::Constraint {
                    referenced_table,
                    timing: ConstraintTiming::from_code(*constraint_timing)
                        .ok_or_else(|| invalid("constraint trigger timing"))?,
                }
            } else {
                TriggerKind::Ordinary
            };
            let spec = TriggerSpec {
                name: SqlName::parse(name)?,
                target,
                kind,
                function,
                timing: super::ast::TriggerTiming::from_code(*timing)
                    .ok_or_else(|| invalid("trigger timing"))?,
                level: *level,
                events: *events,
                update_columns: *update_columns,
                transition_tables: TriggerTransitionTables::from_names(*old_table, *new_table)
                    .ok_or_else(|| invalid("trigger transition names"))?,
                when: when.map(trigger_when_stackstr).transpose()?,
                arguments: TriggerArguments::parse(
                    arguments
                        .get(..*argument_count)
                        .ok_or_else(|| invalid("trigger argument count"))?,
                )?,
            };
            if *or_replace && let Some(slot) = storage.trigger_slot_on(target, name, txid) {
                if storage.trigger_position(slot) != *identity {
                    return Err(invalid("trigger replacement lifetime"));
                }
                let change = storage.replace_trigger(slot, spec, txid)?;
                record_trigger_change(storage, transaction, slot, change)?;
            } else {
                let slot = storage.restore_prepared_trigger(*identity, spec, txid)?;
                transaction.record_catalog_change(storage, DdlUndo::TriggerCreated(slot as u32))?;
            }
        }
        WalOp::DropTrigger {
            identity,
            name,
            target,
            table_schema,
            table,
        } => {
            let target = trigger_target(storage, *target, table_schema, table, txid)?;
            let slot = checked_trigger(storage, *identity, target, name, txid)?;
            storage.drop_trigger(slot, txid)?;
            transaction.record_catalog_change(storage, DdlUndo::TriggerDropped(slot as u32))?;
        }
        WalOp::AlterTrigger {
            identity,
            name,
            target,
            table_schema,
            table,
            new_name,
            enabled,
        } => {
            let target = trigger_target(storage, *target, table_schema, table, txid)?;
            let slot = checked_trigger(storage, *identity, target, name, txid)?;
            let enabled = TriggerEnabled::from_code(*enabled)
                .ok_or_else(|| invalid("trigger enabled mode"))?;
            let change = storage.alter_trigger(slot, SqlName::parse(new_name)?, enabled, txid)?;
            record_trigger_change(storage, transaction, slot, change)?;
        }
        WalOp::SetPartitionTrigger { expected, image } => {
            let (slot, prior) =
                storage.restore_prepared_partition_trigger(*expected, *image, txid)?;
            transaction.record_catalog_change(
                storage,
                DdlUndo::PartitionTriggerAltered {
                    slot: slot as u32,
                    prior,
                },
            )?;
        }
        WalOp::SetPolicy {
            identity,
            schema,
            table,
            name,
            command,
            permissive,
            roles,
            using,
            with_check,
            dependencies,
        } => {
            let table = storage
                .find_visible(schema, table, txid)
                .ok_or_else(|| invalid("policy table"))?;
            let command =
                PolicyCommandKind::from_code(*command).ok_or_else(|| invalid("policy command"))?;
            let definition = PolicyDefinition {
                using: using.map(policy_expression).transpose()?,
                with_check: with_check.map(policy_expression).transpose()?,
            };
            if roles.len() == 0 || roles.len() > storage.policy_role_capacity() {
                return Err(invalid("policy role count"));
            }
            let role_slots = arena
                .alloc_slice_with(roles.len(), |_| PUBLIC_ROLE)
                .map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "policy recovery roles exceed statement arena"
                    )
                })?;
            for (slot, name) in role_slots.iter_mut().zip(roles.iter()) {
                *slot = if name.eq_ignore_ascii_case("public") {
                    PUBLIC_ROLE
                } else {
                    storage
                        .find_role_visible(name, txid)
                        .and_then(|slot| u16::try_from(slot).ok())
                        .ok_or_else(|| invalid("policy role"))?
                };
            }
            let dependencies =
                storage.rebind_stored_query_dependencies(dependencies.materialize()?, txid)?;
            let slot = identity.slot();
            if slot >= storage.policy_count() {
                return Err(invalid("policy position"));
            }
            let current = storage.policy(slot);
            if current.ddl_state != CatalogDdlState::Absent {
                if current.database != storage.current_database_oid()
                    || !current.visible_to(txid)
                    || current.created_at != identity.created_at()
                    || usize::from(current.table) != table
                    || current.name.as_str() != *name
                    || current.command != command
                    || current.permissive != *permissive
                {
                    return Err(invalid("policy lifetime"));
                }
                let prior =
                    storage.alter_policy(slot, definition, role_slots, &dependencies, txid)?;
                transaction.record_catalog_change(
                    storage,
                    DdlUndo::PolicyAltered {
                        slot: slot as u32,
                        prior,
                    },
                )?;
            } else {
                let slot = storage.restore_prepared_policy(
                    *identity,
                    PolicySpec {
                        name: SqlName::parse(name)?,
                        table,
                        command,
                        permissive: *permissive,
                        definition,
                        roles: role_slots,
                    },
                    &dependencies,
                    txid,
                )?;
                transaction.record_catalog_change(storage, DdlUndo::PolicyCreated(slot as u32))?;
            }
        }
        WalOp::DropPolicy {
            identity,
            schema,
            table,
            name,
        } => {
            let table = storage
                .find_visible(schema, table, txid)
                .ok_or_else(|| invalid("policy table"))?;
            let slot = identity.slot();
            if slot >= storage.policy_count() {
                return Err(invalid("policy position"));
            }
            let current = storage.policy(slot);
            if current.database != storage.current_database_oid()
                || !current.visible_to(txid)
                || current.created_at != identity.created_at()
                || usize::from(current.table) != table
                || current.name.as_str() != *name
            {
                return Err(invalid("policy lifetime"));
            }
            storage.drop_policy(slot, txid);
            transaction.record_catalog_change(storage, DdlUndo::PolicyDropped(slot as u32))?;
        }
        WalOp::SetExtendedStatistics {
            created_at,
            schema,
            name,
            table_schema,
            table,
            target,
            kinds,
            expression_only,
            keys,
            key_count,
        } => {
            let table = storage
                .find_visible(table_schema, table, txid)
                .ok_or_else(|| invalid("statistics relation"))?;
            if *key_count == 0 || *key_count > MAX_EXTENDED_STATISTICS_KEYS {
                return Err(invalid("statistics key count"));
            }
            let kinds = if *expression_only {
                super::ast::StatisticsKinds::EXPRESSION
            } else {
                super::ast::StatisticsKinds::from_code(*kinds)
                    .ok_or_else(|| invalid("statistics kinds"))?
            };
            let mut stored_keys =
                [ExtendedStatisticsKey::Column(SqlName::EMPTY); MAX_EXTENDED_STATISTICS_KEYS];
            let table_definition = storage.table_def(table, txid);
            for (stored, key) in stored_keys.iter_mut().zip(&keys[..*key_count]) {
                *stored = match key {
                    crate::wal::WalExtendedStatisticsKey::Column(name) => {
                        let column = table_definition
                            .column_index(name)
                            .ok_or_else(|| invalid("statistics column"))?;
                        ExtendedStatisticsKey::Column(table_definition.columns()[column].name)
                    }
                    crate::wal::WalExtendedStatisticsKey::Expression(source) => {
                        ExtendedStatisticsKey::Expression(extended_statistics_expression(source)?)
                    }
                };
            }
            drop(table_definition);
            let existing = (0..storage.extended_statistics_count()).find(|slot| {
                let value = storage.extended_statistics(*slot);
                value.database == storage.current_database_oid()
                    && value.ddl_state != CatalogDdlState::Absent
                    && value.created_at == *created_at
            });
            if let Some(slot) = existing {
                let current = storage.extended_statistics(slot);
                if !current.visible_to(txid)
                    || usize::from(current.table) != table
                    || usize::from(current.n_keys) != *key_count
                    || current.kinds != kinds
                    || current.expression_only != *expression_only
                {
                    return Err(invalid("statistics lifetime or shape"));
                }
                let next = ExtendedStatisticsMutableDefinition {
                    schema: SqlName::parse(schema)?,
                    name: SqlName::parse(name)?,
                    target: *target,
                };
                if current.definition_for(txid) != next {
                    let prior = storage.alter_extended_statistics(slot, next, txid)?;
                    transaction.record_catalog_change(
                        storage,
                        DdlUndo::StatisticsAltered {
                            slot: slot as u32,
                            prior,
                        },
                    )?;
                }
                if current.keys_for(txid) != &stored_keys[..*key_count] {
                    let prior = storage.alter_extended_statistics_keys(slot, stored_keys, txid)?;
                    transaction.record_catalog_change(
                        storage,
                        DdlUndo::StatisticsKeysAltered {
                            slot: slot as u32,
                            prior,
                        },
                    )?;
                }
            } else {
                let slot = storage.create_extended_statistics(
                    ExtendedStatisticsSpec {
                        creation: CatalogCreation::Restore(CatalogIdentity::require(*created_at)?),
                        schema: SqlName::parse(schema)?,
                        name: SqlName::parse(name)?,
                        table: table as u16,
                        target: *target,
                        keys: stored_keys,
                        n_keys: *key_count as u8,
                        kinds,
                        expression_only: *expression_only,
                    },
                    txid,
                )?;
                transaction
                    .record_catalog_change(storage, DdlUndo::StatisticsCreated(slot as u32))?;
            }
        }
        WalOp::DropExtendedStatistics { schema, name } => {
            let slot = storage
                .extended_statistics_slot(schema, name, txid)
                .ok_or_else(|| invalid("statistics source"))?;
            storage.drop_extended_statistics(slot, txid)?;
            transaction.record_catalog_change(storage, DdlUndo::StatisticsDropped(slot as u32))?;
        }
        WalOp::AnalyzeExtendedStatistics {
            schema,
            name,
            statistics,
        } => {
            let slot = storage
                .extended_statistics_slot(schema, name, txid)
                .ok_or_else(|| invalid("statistics source"))?;
            let mut data = statistics.materialize()?;
            if statistics.has_legacy_enum_hashes() && storage.live_enums().next().is_some() {
                let definition = storage.extended_statistics(slot);
                let table = storage.table_def(usize::from(definition.table), txid);
                if definition.keys_for(txid).iter().any(|key| match key {
                    ExtendedStatisticsKey::Column(name) => {
                        table.column_index(name.as_str()).is_some_and(|column| {
                            matches!(
                                table.columns()[column].ctype,
                                super::types::ColType::Enum(_)
                            )
                        })
                    }
                    ExtendedStatisticsKey::Expression(_) => true,
                }) {
                    data.n_mcv = 0;
                }
            }
            storage.write_extended_statistics_data(slot, txid, data)?;
            if let Err(error) = transaction.record_extended_statistics(slot as u32) {
                storage.rollback_extended_statistics_data(slot, txid);
                return Err(error);
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn invalid(part: &str) -> SqlError {
    sql_err!(
        sqlstate::DATA_EXCEPTION,
        "prepared journal {} is unavailable or invalid",
        part
    )
}

fn text<const N: usize>(value: &str) -> Result<StackStr<N>, SqlError> {
    let result = StackStr::from_str(value);
    if result.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "prepared query text exceeds its stored capacity"
        ));
    }
    Ok(result)
}

fn view_options(invoker: bool, barrier: u8, check: u8) -> Result<ViewOptions, SqlError> {
    Ok(ViewOptions {
        security: if invoker {
            ViewSecurity::Invoker
        } else {
            ViewSecurity::Definer
        },
        security_barrier: ViewSecurityBarrier::from_code(barrier)
            .ok_or_else(|| invalid("view security barrier"))?,
        check_option: if check == 0 {
            None
        } else {
            Some(ViewCheckOption::from_code(check).ok_or_else(|| invalid("view check option"))?)
        },
    })
}

fn rule_target(
    storage: &Storage,
    kind: TriggerTargetKind,
    schema: &str,
    name: &str,
    transaction: u32,
) -> Result<RuleTarget, SqlError> {
    match (
        kind,
        storage.resolve_relation(Some(schema), name, transaction),
    ) {
        (TriggerTargetKind::Table, Some(ResolvedRelation::Table(slot))) => {
            Ok(RuleTarget::Table(slot as u16))
        }
        (TriggerTargetKind::View, Some(ResolvedRelation::View(slot))) => {
            Ok(RuleTarget::View(slot as u16))
        }
        _ => Err(invalid("query relation")),
    }
}

fn trigger_target(
    storage: &Storage,
    kind: TriggerTargetKind,
    schema: &str,
    name: &str,
    transaction: u32,
) -> Result<TriggerTarget, SqlError> {
    Ok(
        match rule_target(storage, kind, schema, name, transaction)? {
            RuleTarget::Table(slot) => TriggerTarget::Table(slot),
            RuleTarget::View(slot) => TriggerTarget::View(slot),
        },
    )
}

fn checked_trigger(
    storage: &Storage,
    identity: CatalogPosition,
    target: TriggerTarget,
    name: &str,
    transaction: u32,
) -> Result<usize, SqlError> {
    let slot = identity.slot();
    if slot >= storage.trigger_capacity() {
        return Err(invalid("trigger position"));
    }
    let trigger = storage.trigger_to(slot, transaction);
    if trigger.database != storage.current_database_oid()
        || !trigger.visible_to(transaction)
        || storage.trigger_position(slot) != identity
        || trigger.target != target
        || trigger.name_to(transaction).as_str() != name
    {
        return Err(invalid("trigger lifetime"));
    }
    Ok(slot)
}

fn record_trigger_change(
    storage: &mut Storage,
    transaction: &mut TxnState,
    slot: usize,
    change: TriggerAlter,
) -> Result<(), SqlError> {
    if let TriggerAlter::Changed { prior } = change {
        transaction.record_catalog_change(
            storage,
            DdlUndo::TriggerAltered {
                slot: slot as u32,
                prior,
            },
        )?;
    }
    Ok(())
}
