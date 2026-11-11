//! `pg_depend`'s rows: every recorded dependency between catalog objects.
//!
//! The same edges answer catalog queries and drive the dependency report of
//! `DROP`, so a dependency PostgreSQL records is described and dropped the same
//! way wherever pos3ql meets it. Built-in (pinned) objects have no rows.

use super::*;

/// PostgreSQL's `FirstUnpinnedObjectId`: initdb's objects below it are
/// pinned and never appear in `pg_depend`.
const FIRST_UNPINNED_OBJECT_ID: i32 = 12_000;

/// One catalog object, as PostgreSQL's `ObjectAddress` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ObjectAddress {
    pub(crate) class: i32,
    pub(crate) object: i32,
    pub(crate) sub: i32,
}

impl ObjectAddress {
    pub(crate) const fn new(class: i32, object: i32, sub: i32) -> Self {
        Self { class, object, sub }
    }
}

/// One `pg_depend` row: `dependent` depends on `referenced` with `kind`
/// (`n`, `a`, `i`, `P`, `S`, `e` or `x`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DependencyEdge {
    pub(crate) dependent: ObjectAddress,
    pub(crate) referenced: ObjectAddress,
    pub(crate) kind: u8,
}

/// Visits every dependency visible to `txid`. Temporary arena space used to
/// read stored dependencies is reclaimed only by the caller.
pub(crate) fn for_each_dependency(
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    visit: &mut dyn FnMut(DependencyEdge) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let integer = |value: Datum<'_>| match value {
        Datum::Oid(value) => Ok(value as i32),
        Datum::Int4(value) => Ok(value),
        _ => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "constraint dependency row has an unexpected identity"
        )),
    };
    for values in constraints::dependencies(storage, txid, arena)? {
        let Datum::Char(kind) = values[6] else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "constraint dependency row has an unexpected kind"
            ));
        };
        visit(DependencyEdge {
            dependent: ObjectAddress::new(
                integer(values[0])?,
                integer(values[1])?,
                integer(values[2])?,
            ),
            referenced: ObjectAddress::new(
                integer(values[3])?,
                integer(values[4])?,
                integer(values[5])?,
            ),
            kind,
        })?;
    }
    macro_rules! push {
        ($class:expr, $object:expr, $referenced_class:expr, $referenced_object:expr, $referenced_sub:expr, $kind:expr $(,)?) => {
            visit(DependencyEdge {
                dependent: ObjectAddress::new($class, $object, 0),
                referenced: ObjectAddress::new(
                    $referenced_class,
                    $referenced_object,
                    $referenced_sub,
                ),
                kind: $kind.as_bytes()[0],
            })
        };
    }

    // Sequence ownership is how pg_dump distinguishes serial/identity
    // generators from independent sequences and orders them with the owning
    // column.
    for sequence_slot in 0..storage.sequence_count() {
        let sequence = storage.sequence_for(sequence_slot, txid);
        if !storage.sequence_slot_visible_to(sequence_slot, txid) {
            continue;
        }
        let Some(owner) = sequence.owner else {
            continue;
        };
        let Some(table_slot) =
            storage.find_visible(owner.table_schema.as_str(), owner.table.as_str(), txid)
        else {
            continue;
        };
        let table = storage.table_def(table_slot, txid);
        let Some(column) = table.column_index(owner.column.as_str()) else {
            continue;
        };
        push!(
            PG_CLASS_OID,
            sequence_oid(storage, sequence_slot),
            PG_CLASS_OID,
            user_table_oid(storage, table_slot),
            i32::from(table.attribute_number(column)),
            if table.columns()[column].is_identity {
                "i"
            } else {
                "a"
            },
        )?;
    }

    relation_dependencies(storage, txid, arena, visit)?;
    policy_dependencies(storage, txid, arena, visit)?;
    // PostgreSQL's foreign-data DDL: a wrapper uses its handler and validator,
    // a server its wrapper, and user mappings and foreign tables their server.
    for (_, entry) in storage.foreign_wrappers(txid) {
        let wrapper = entry.definition_for(txid);
        for routine in [
            match wrapper.handler {
                crate::storage::foreign::ForeignDataHandler::None => None,
                crate::storage::foreign::ForeignDataHandler::Postgres => {
                    Some("postgres_fdw_handler")
                }
            },
            match wrapper.validator {
                crate::storage::foreign::ForeignDataValidator::None => None,
                crate::storage::foreign::ForeignDataValidator::Postgres => {
                    Some("postgres_fdw_validator")
                }
            },
        ]
        .into_iter()
        .flatten()
        {
            push!(
                PG_FOREIGN_DATA_WRAPPER_OID,
                entry.oid(),
                PG_PROC_OID,
                foreign_routine_oid(storage, txid, routine)?,
                0,
                "n",
            )?;
        }
    }
    for (_, entry) in storage.foreign_servers(txid) {
        let server = entry.definition_for(txid);
        push!(
            PG_FOREIGN_SERVER_OID,
            entry.oid(),
            PG_FOREIGN_DATA_WRAPPER_OID,
            storage.foreign_wrapper_entry(server.wrapper as usize).oid(),
            0,
            "n",
        )?;
    }
    for (_, entry) in storage.foreign_user_mappings(txid) {
        let mapping = entry.definition_for(txid);
        push!(
            PG_USER_MAPPING_OID,
            entry.oid(),
            PG_FOREIGN_SERVER_OID,
            storage.foreign_server_entry(mapping.server as usize).oid(),
            0,
            "n",
        )?;
    }
    for (_, entry) in storage.foreign_tables(txid) {
        let table = entry.definition_for(txid);
        push!(
            PG_CLASS_OID,
            user_table_oid(storage, table.table as usize),
            PG_FOREIGN_SERVER_OID,
            storage.foreign_server_entry(table.server as usize).oid(),
            0,
            "n",
        )?;
    }
    for (_, object) in storage.text_search_objects_visible_to(txid) {
        let class = match object.kind() {
            crate::sql::ast::TextSearchObjectKind::Parser => PG_TS_PARSER_OID,
            crate::sql::ast::TextSearchObjectKind::Template => PG_TS_TEMPLATE_OID,
            crate::sql::ast::TextSearchObjectKind::Dictionary => PG_TS_DICT_OID,
            crate::sql::ast::TextSearchObjectKind::Configuration => PG_TS_CONFIG_OID,
        };
        let dependent = ObjectAddress::new(class, object.oid(), 0);
        namespace_edge(storage, txid, dependent, object.schema().as_str(), visit)?;
        // Objects below PostgreSQL's `FirstUnpinnedObjectId` are pinned and
        // record no dependency; initdb's snowball dictionaries are not.
        let mut depend = |referenced_class: i32, referenced: i32| {
            if referenced < FIRST_UNPINNED_OBJECT_ID {
                return Ok(());
            }
            visit(DependencyEdge {
                dependent,
                referenced: ObjectAddress::new(referenced_class, referenced, 0),
                kind: b'n',
            })
        };
        match object {
            crate::storage::TextSearchDefinition::Parser {
                start,
                gettoken,
                end,
                headline,
                lextypes,
                ..
            } => {
                for function in [start, gettoken, end, lextypes, headline] {
                    if function != 0 {
                        depend(PG_PROC_OID, function)?;
                    }
                }
            }
            crate::storage::TextSearchDefinition::Template { init, lexize, .. } => {
                for function in [init, lexize] {
                    if function != 0 {
                        depend(PG_PROC_OID, function)?;
                    }
                }
            }
            crate::storage::TextSearchDefinition::Dictionary { template, .. } => {
                depend(PG_TS_TEMPLATE_OID, template)?;
            }
            crate::storage::TextSearchDefinition::Configuration {
                parser, mappings, ..
            } => {
                depend(PG_TS_PARSER_OID, parser)?;
                let mut seen = [0; crate::storage::TEXT_SEARCH_TOKEN_TYPES
                    * crate::storage::TEXT_SEARCH_DICTIONARIES_PER_TOKEN];
                let mut count = 0;
                for dictionary in mappings.dictionaries.iter().flatten().copied() {
                    if dictionary != 0 && !seen[..count].contains(&dictionary) {
                        seen[count] = dictionary;
                        count += 1;
                        depend(PG_TS_DICT_OID, dictionary)?;
                    }
                }
            }
        }
    }
    // PostgreSQL's `CreateStatistics`: an automatic dependency on each key
    // column and on each column a key expression uses, and on the whole table
    // only when no key is a plain column.
    for (_, statistics) in storage.extended_statistics_visible(txid) {
        let dependent = ObjectAddress::new(PG_STATISTIC_EXT_OID, statistics.oid(), 0);
        let table_slot = usize::from(statistics.table);
        let table_oid = user_table_oid(storage, table_slot);
        let table = storage.table_def(table_slot, txid);
        let mut columns = crate::storage::ColumnSet::EMPTY;
        let mut simple = false;
        for key in statistics.keys_for(txid) {
            match key {
                crate::storage::ExtendedStatisticsKey::Column(name) => {
                    let column = table.column_index(name.as_str()).ok_or_else(|| {
                        sql_err!(sqlstate::INTERNAL_ERROR, "statistics key column is absent")
                    })?;
                    columns.insert(column);
                    simple = true;
                }
                crate::storage::ExtendedStatisticsKey::Expression(source) => {
                    let expression = crate::sql::parser::parse_expr(source.as_str(), arena)?;
                    columns |= crate::sql::exec::check_referenced_columns(expression, &table)?;
                }
            }
        }
        if !simple {
            visit(DependencyEdge {
                dependent,
                referenced: ObjectAddress::new(PG_CLASS_OID, table_oid, 0),
                kind: b'a',
            })?;
        }
        for column in 0..table.n_columns {
            if columns.contains(column) {
                visit(DependencyEdge {
                    dependent,
                    referenced: ObjectAddress::new(
                        PG_CLASS_OID,
                        table_oid,
                        i32::from(table.attribute_number(column)),
                    ),
                    kind: b'a',
                })?;
            }
        }
        namespace_edge(
            storage,
            txid,
            dependent,
            statistics.definition_for(txid).schema.as_str(),
            visit,
        )?;
    }

    // A domain is ordered after its parent domain or direct user-defined base
    // type. pg_dump reads this graph to emit a restorable type definition.
    for domain_slot in 0..storage.domain_count() {
        let domain = storage.domain_for(domain_slot, txid);
        if !storage.domain_slot_visible_to(domain_slot, txid) {
            continue;
        }
        let typed_base = || {
            use crate::sql::types::{ArrElem, oid};
            match domain.base {
                ColType::Enum(slot) => slot.map(oid::enum_oid),
                ColType::Composite(slot) => slot.map(oid::composite_oid),
                ColType::Array(ArrElem::Enum(slot)) => slot.map(oid::enum_array_oid),
                ColType::Array(ArrElem::Composite(slot)) => slot.map(oid::composite_array_oid),
                _ => None,
            }
        };
        let referenced_type = if let Some(parent) = domain.base_domain {
            storage
                .domain_identity_slot(parent.schema.as_str(), parent.name.as_str(), txid)
                .map(|slot| domain_oid(storage, slot, txid))
        } else {
            user_type_oid(storage, txid, domain.base, domain.base_user_type).or_else(typed_base)
        };
        if let Some(referenced_type) = referenced_type {
            push!(
                PG_TYPE_OID,
                domain_oid(storage, domain_slot, txid),
                PG_TYPE_OID,
                referenced_type,
                0,
                "n",
            )?;
        }
    }

    for (_, cast) in storage.casts_visible_to(txid) {
        let source = catalog_routine_result_oid(storage, txid, cast.source)?;
        let target = catalog_routine_result_oid(storage, txid, cast.target)?;
        for type_oid in [source, target] {
            // Built-in types are pinned and have no dependency rows.
            if crate::storage::NamedTypeKind::from_oid(type_oid).is_some() {
                push!(PG_CAST_OID, cast.oid(), PG_TYPE_OID, type_oid, 0, "n")?;
            }
        }
        if let crate::storage::CastMethod::Function(function) = cast.method {
            push!(PG_CAST_OID, cast.oid(), PG_PROC_OID, function, 0, "n")?;
        }
    }

    for (slot, operator) in storage.operators_visible_to(txid) {
        let oid = storage.operator(slot).oid();
        push!(
            PG_OPERATOR_OID,
            oid,
            PG_NAMESPACE_OID,
            namespace_oid(storage, operator.schema.as_str(), txid),
            0,
            "n",
        )?;
        if let Some(function) = operator.implementation.routine() {
            push!(PG_OPERATOR_OID, oid, PG_PROC_OID, function, 0, "n")?;
        }
        for argument in [operator.signature.left, operator.signature.right]
            .into_iter()
            .flatten()
        {
            let type_oid = catalog_routine_result_oid(storage, txid, argument)?;
            if crate::storage::NamedTypeKind::from_oid(type_oid).is_some() {
                push!(PG_OPERATOR_OID, oid, PG_TYPE_OID, type_oid, 0, "n")?;
            }
        }
    }

    for (slot, collation) in storage.collations_visible_to(txid) {
        push!(
            PG_COLLATION_OID,
            storage.collation(slot).oid(),
            PG_NAMESPACE_OID,
            namespace_oid(storage, collation.schema.as_str(), txid),
            0,
            "n",
        )?;
    }
    for (slot, conversion) in storage.conversions_visible_to(txid) {
        let oid = storage.conversion(slot).oid();
        push!(
            PG_CONVERSION_OID,
            oid,
            PG_NAMESPACE_OID,
            namespace_oid(storage, conversion.schema.as_str(), txid),
            0,
            "n",
        )?;
        push!(
            PG_CONVERSION_OID,
            oid,
            PG_PROC_OID,
            conversion.procedure,
            0,
            "n",
        )?;
    }

    for (slot, family) in storage.operator_families_visible_to(txid) {
        push!(
            PG_OPFAMILY_OID,
            storage.operator_family(slot).oid(),
            PG_NAMESPACE_OID,
            namespace_oid(storage, family.schema.as_str(), txid),
            0,
            "n",
        )?;
    }

    for (class_slot, class) in storage.operator_classes_visible_to(txid) {
        let class_oid = storage.operator_class(class_slot).oid();
        push!(
            PG_OPCLASS_OID,
            class_oid,
            PG_NAMESPACE_OID,
            namespace_oid(storage, class.schema.as_str(), txid),
            0,
            "n",
        )?;
        push!(
            PG_OPCLASS_OID,
            class_oid,
            PG_OPFAMILY_OID,
            class.family,
            0,
            "a",
        )?;
        for type_definition in [class.input, class.storage] {
            let type_oid = catalog_routine_result_oid(storage, txid, type_definition)?;
            if crate::storage::NamedTypeKind::from_oid(type_oid).is_some() {
                push!(PG_OPCLASS_OID, class_oid, PG_TYPE_OID, type_oid, 0, "n")?;
            }
        }
        let family_slot = storage
            .operator_family_slot_by_oid(class.family, txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "operator class family identity is unavailable"
                )
            })?;
        let family = storage.operator_family_for(family_slot, txid);
        for member in class.operators.into_iter().filter(|member| member.used) {
            let Some((member_index, _)) = family
                .operators
                .iter()
                .enumerate()
                .find(|(_, candidate)| **candidate == member)
            else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "operator class member identity is unavailable"
                ));
            };
            let member_oid = operator_family_member_oid(class.family, member_index);
            push!(PG_AMOP_OID, member_oid, PG_OPCLASS_OID, class_oid, 0, "i")?;
            push!(
                PG_AMOP_OID,
                member_oid,
                PG_OPERATOR_OID,
                member.operator,
                0,
                "n",
            )?;
        }
        for member in class.functions.into_iter().filter(|member| member.used) {
            let Some((member_index, _)) = family
                .functions
                .iter()
                .enumerate()
                .find(|(_, candidate)| **candidate == member)
            else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "operator class support-function identity is unavailable"
                ));
            };
            let member_oid = operator_family_member_oid(class.family, member_index);
            push!(PG_AMPROC_OID, member_oid, PG_OPCLASS_OID, class_oid, 0, "i")?;
            push!(
                PG_AMPROC_OID,
                member_oid,
                PG_PROC_OID,
                member.function,
                0,
                "n",
            )?;
        }
    }
    for (family_slot, family) in storage.operator_families_visible_to(txid) {
        let family_oid = storage.operator_family(family_slot).oid();
        for (member_index, member) in family
            .operators
            .iter()
            .enumerate()
            .filter(|(_, member)| member.used)
        {
            let class_owned = storage
                .operator_classes_visible_to(txid)
                .any(|(_, class)| class.family == family_oid && class.operators.contains(member));
            if class_owned {
                continue;
            }
            let member_oid = operator_family_member_oid(family_oid, member_index);
            push!(PG_AMOP_OID, member_oid, PG_OPFAMILY_OID, family_oid, 0, "a")?;
            push!(
                PG_AMOP_OID,
                member_oid,
                PG_OPERATOR_OID,
                member.operator,
                0,
                "a",
            )?;
        }
        for (member_index, member) in family
            .functions
            .iter()
            .enumerate()
            .filter(|(_, member)| member.used)
        {
            let class_owned = storage
                .operator_classes_visible_to(txid)
                .any(|(_, class)| class.family == family_oid && class.functions.contains(member));
            if class_owned {
                continue;
            }
            let member_oid = operator_family_member_oid(family_oid, member_index);
            push!(
                PG_AMPROC_OID,
                member_oid,
                PG_OPFAMILY_OID,
                family_oid,
                0,
                "a",
            )?;
            push!(
                PG_AMPROC_OID,
                member_oid,
                PG_PROC_OID,
                member.function,
                0,
                "a",
            )?;
        }
    }

    for index_slot in 0..storage.index_count() {
        let Some(index) = storage.index_visible_to(index_slot, txid) else {
            continue;
        };
        // PostgreSQL's `index_create` records each key column's collation.
        for collation in index.collations[..index.n_cols].iter().copied() {
            collation_edge(
                ObjectAddress::new(PG_CLASS_OID, explicit_index_oid(&index), 0),
                collation,
                visit,
            )?;
        }
        for position in 0..index.n_cols {
            let Some(crate::storage::IndexOperatorClass::Catalog(class_oid)) =
                index.resolved_operator_classes[position]
            else {
                continue;
            };
            if index.resolved_operator_classes[..position].contains(&Some(
                crate::storage::IndexOperatorClass::Catalog(class_oid),
            )) {
                continue;
            }
            push!(
                PG_CLASS_OID,
                explicit_index_oid(&index),
                PG_OPCLASS_OID,
                class_oid.get(),
                0,
                "n",
            )?;
        }
    }

    let referenced_oid = |dependency: &crate::storage::StoredQueryDependency| {
        stored_reference(storage, txid, dependency)
    };
    let referenced_column = |dependency: &crate::storage::StoredQueryDependency, column: usize| {
        stored_column_number(storage, txid, dependency, column)
    };
    for (view_slot, _) in storage.views_visible_to(txid) {
        let rewrite_oid = storage.rule(storage.view_return_rule(view_slot)).oid();
        push!(
            2618,
            rewrite_oid,
            PG_CLASS_OID,
            view_oid(storage, view_slot),
            0,
            "i",
        )?;
        for dependency in storage
            .snapshot_view_dependencies(view_slot, txid, arena)?
            .entries()
        {
            let Some((referenced_class, referenced_object)) = referenced_oid(dependency) else {
                continue;
            };
            if dependency.referenced_columns.is_empty() {
                push!(
                    2618,
                    rewrite_oid,
                    referenced_class,
                    referenced_object,
                    0,
                    "n",
                )?;
            } else {
                for column in 0..MAX_COLUMNS {
                    if dependency.referenced_columns.contains(column) {
                        push!(
                            2618,
                            rewrite_oid,
                            referenced_class,
                            referenced_object,
                            referenced_column(dependency, column),
                            "n",
                        )?;
                    }
                }
            }
        }
    }
    for (slot, rule) in storage.rules_visible_to(txid) {
        let definition = rule.definition_for(txid);
        if matches!(definition.event, crate::storage::RewriteEvent::Select) {
            continue;
        }
        let relation_oid = match definition.target {
            crate::storage::RuleTarget::Table(slot) => user_table_oid(storage, usize::from(slot)),
            crate::storage::RuleTarget::View(slot) => view_oid(storage, usize::from(slot)),
        };
        push!(2618, rule.oid(), PG_CLASS_OID, relation_oid, 0, "a")?;
        // `NEW.x` and `OLD.x` read columns of the rule's own relation.
        let own = transition_columns(
            storage,
            txid,
            match definition.target {
                crate::storage::RuleTarget::Table(slot) => {
                    crate::storage::RowTypeRelation::Table(usize::from(slot))
                }
                crate::storage::RuleTarget::View(slot) => {
                    crate::storage::RowTypeRelation::View(usize::from(slot))
                }
            },
            definition.fragments(),
            arena,
        )?;
        for attribute in own.iter().copied() {
            push!(
                2618,
                rule.oid(),
                PG_CLASS_OID,
                relation_oid,
                i32::from(attribute),
                "n"
            )?;
        }
        for dependency in storage
            .snapshot_rule_dependencies(slot, txid, arena)?
            .entries()
        {
            let Some((referenced_class, referenced_object)) = referenced_oid(dependency) else {
                continue;
            };
            if dependency.referenced_columns.is_empty() {
                push!(
                    2618,
                    rule.oid(),
                    referenced_class,
                    referenced_object,
                    0,
                    "n",
                )?;
            } else {
                for column in 0..MAX_COLUMNS {
                    if dependency.referenced_columns.contains(column) {
                        push!(
                            2618,
                            rule.oid(),
                            referenced_class,
                            referenced_object,
                            referenced_column(dependency, column),
                            "n",
                        )?;
                    }
                }
            }
        }
    }
    for (materialized_slot, _) in storage.matviews_visible_to(txid) {
        let table_slot = storage.matview_table(materialized_slot);
        for dependency in storage
            .snapshot_matview_dependencies(materialized_slot, txid, arena)?
            .entries()
        {
            let Some((referenced_class, referenced_object)) = referenced_oid(dependency) else {
                continue;
            };
            push!(
                PG_CLASS_OID,
                user_table_oid(storage, table_slot),
                referenced_class,
                referenced_object,
                0,
                "n",
            )?;
        }
    }
    for (trigger_slot, trigger) in storage.triggers_with_slots_visible_to(txid) {
        let trigger_oid = crate::storage::trigger_oid(&trigger);
        let relation_oid = match trigger.target {
            crate::storage::TriggerTarget::Table(table) => {
                user_table_oid(storage, usize::from(table))
            }
            crate::storage::TriggerTarget::View(view) => view_oid(storage, usize::from(view)),
        };
        push!(
            2620,
            trigger_oid,
            PG_PROC_OID,
            crate::storage::routine_oid(&storage.routine_for(usize::from(trigger.function), txid)),
            0,
            "n",
        )?;
        push!(2620, trigger_oid, PG_CLASS_OID, relation_oid, 0, "a")?;
        // The columns a `WHEN` condition reads through `NEW` and `OLD`.
        if let (Some(when), crate::storage::TriggerTarget::Table(table)) =
            (trigger.when.as_ref(), trigger.target)
        {
            for attribute in transition_columns(
                storage,
                txid,
                crate::storage::RowTypeRelation::Table(usize::from(table)),
                core::iter::once(when.as_str()),
                arena,
            )?
            .iter()
            .copied()
            {
                push!(
                    2620,
                    trigger_oid,
                    PG_CLASS_OID,
                    relation_oid,
                    i32::from(attribute),
                    "n"
                )?;
            }
        }
        if let crate::storage::TriggerKind::Constraint {
            referenced_table, ..
        } = trigger.kind
        {
            if let Some(referenced) = referenced_table {
                push!(
                    2620,
                    trigger_oid,
                    PG_CLASS_OID,
                    user_table_oid(storage, usize::from(referenced)),
                    0,
                    "a",
                )?;
            }
            push!(
                2606,
                crate::storage::trigger_constraint_oid(trigger_oid),
                2620,
                trigger_oid,
                0,
                "i",
            )?;
        }
        for column in 0..MAX_COLUMNS {
            if trigger.update_columns.contains(column) {
                push!(
                    2620,
                    trigger_oid,
                    PG_CLASS_OID,
                    relation_oid,
                    match trigger.target {
                        crate::storage::TriggerTarget::Table(slot) => i32::from(
                            storage
                                .table_def(usize::from(slot), txid)
                                .attribute_number(column),
                        ),
                        crate::storage::TriggerTarget::View(_) => column as i32 + 1,
                    },
                    "n",
                )?;
            }
        }

        let crate::storage::TriggerTarget::Table(parent_table) = trigger.target else {
            continue;
        };
        if !matches!(trigger.level, crate::sql::ast::TriggerLevel::Row) {
            continue;
        }
        let parent_table = usize::from(parent_table);
        for child in 0..storage.table_count() {
            if !storage.table_slot_visible_to(child, txid)
                || !storage.partition_descends_from(child, parent_table, txid)
            {
                continue;
            }
            let clone_oid = storage.partition_trigger_oid_to(
                trigger.position(trigger_slot),
                storage.table_position(child),
                txid,
            )?;
            let child_relation_oid = user_table_oid(storage, child);
            push!(
                2620,
                clone_oid,
                PG_PROC_OID,
                crate::storage::routine_oid(
                    &storage.routine_for(usize::from(trigger.function), txid),
                ),
                0,
                "n",
            )?;
            if let crate::storage::TriggerKind::Constraint {
                referenced_table, ..
            } = trigger.kind
            {
                if let Some(referenced) = referenced_table {
                    push!(
                        2620,
                        clone_oid,
                        PG_CLASS_OID,
                        user_table_oid(storage, usize::from(referenced)),
                        0,
                        "a",
                    )?;
                }
                push!(
                    2606,
                    crate::storage::trigger_constraint_oid(clone_oid),
                    2620,
                    clone_oid,
                    0,
                    "i",
                )?;
            }
            for column in 0..MAX_COLUMNS {
                if trigger.update_columns.contains(column) {
                    push!(
                        2620,
                        clone_oid,
                        PG_CLASS_OID,
                        child_relation_oid,
                        {
                            let parent = storage.table_def(parent_table, txid);
                            let child = storage.table_def(child, txid);
                            let column = child
                                .column_index(parent.columns()[column].name.as_str())
                                .expect("partition trigger column remains present in its child");
                            i32::from(child.attribute_number(column))
                        },
                        "n",
                    )?;
                }
            }
            let direct_parent = usize::from(
                storage
                    .table_def(child, txid)
                    .partition
                    .attachment
                    .expect("partition descendant has a parent")
                    .parent,
            );
            let parent_oid = if direct_parent == parent_table {
                trigger_oid
            } else {
                storage.partition_trigger_oid_to(
                    trigger.position(trigger_slot),
                    storage.table_position(direct_parent),
                    txid,
                )?
            };
            push!(2620, clone_oid, 2620, parent_oid, 0, "P")?;
            push!(2620, clone_oid, PG_CLASS_OID, child_relation_oid, 0, "S")?;
        }
    }
    for (_, dependency) in storage.extension_dependencies_visible_to(txid) {
        let Some((class, object)) =
            access_object_catalog_identity(storage, txid, dependency.object)
        else {
            continue;
        };
        push!(
            class,
            object,
            3079,
            storage.extension(dependency.extension as usize).oid(),
            0,
            match dependency.kind {
                crate::storage::ExtensionDependencyKind::Member => "e",
                crate::storage::ExtensionDependencyKind::Automatic => "x",
                crate::storage::ExtensionDependencyKind::Required => "n",
            },
        )?;
    }
    type_dependencies(storage, txid, arena, visit)
}

/// The catalog identity a stored query dependency names, or `None` for one
/// recorded by name only.
fn stored_reference(
    storage: &Storage,
    txid: u32,
    dependency: &crate::storage::StoredQueryDependency,
) -> Option<(i32, i32)> {
    match dependency.class {
        crate::storage::DependencyClass::Table => Some((
            PG_CLASS_OID,
            user_table_oid(storage, dependency.slot as usize),
        )),
        crate::storage::DependencyClass::View => {
            Some((PG_CLASS_OID, view_oid(storage, dependency.slot as usize)))
        }
        crate::storage::DependencyClass::Sequence => Some((
            PG_CLASS_OID,
            sequence_oid(storage, dependency.slot as usize),
        )),
        crate::storage::DependencyClass::Domain => Some((
            PG_TYPE_OID,
            domain_oid(storage, dependency.slot as usize, txid),
        )),
        crate::storage::DependencyClass::Enum => Some((
            PG_TYPE_OID,
            crate::sql::types::oid::enum_oid(
                storage
                    .enum_for(usize::from(dependency.slot), txid)
                    .creation(),
            ),
        )),
        crate::storage::DependencyClass::Composite => Some((
            PG_TYPE_OID,
            crate::sql::types::oid::composite_oid(
                storage
                    .composite_for(usize::from(dependency.slot), txid)
                    .creation(),
            ),
        )),
        crate::storage::DependencyClass::Routine => Some((
            PG_PROC_OID,
            match dependency.identity {
                crate::storage::StoredDependencyIdentity::RoutineOid(oid) => oid,
                crate::storage::StoredDependencyIdentity::Name
                | crate::storage::StoredDependencyIdentity::Table(_)
                | crate::storage::StoredDependencyIdentity::View(_)
                | crate::storage::StoredDependencyIdentity::OperatorOid(_)
                | crate::storage::StoredDependencyIdentity::Type(_)
                | crate::storage::StoredDependencyIdentity::Collation(_)
                | crate::storage::StoredDependencyIdentity::TextSearchConfigurationOid(_) => {
                    return None;
                }
            },
        )),
        crate::storage::DependencyClass::Operator => Some((
            PG_OPERATOR_OID,
            match dependency.identity {
                crate::storage::StoredDependencyIdentity::OperatorOid(oid) => oid,
                crate::storage::StoredDependencyIdentity::Name
                | crate::storage::StoredDependencyIdentity::Table(_)
                | crate::storage::StoredDependencyIdentity::View(_)
                | crate::storage::StoredDependencyIdentity::RoutineOid(_)
                | crate::storage::StoredDependencyIdentity::Type(_)
                | crate::storage::StoredDependencyIdentity::Collation(_)
                | crate::storage::StoredDependencyIdentity::TextSearchConfigurationOid(_) => {
                    return None;
                }
            },
        )),
        crate::storage::DependencyClass::Collation => Some((
            PG_COLLATION_OID,
            match dependency.identity {
                crate::storage::StoredDependencyIdentity::Collation(selected) => selected.oid(),
                _ => return None,
            },
        )),
        crate::storage::DependencyClass::TextSearchConfiguration => Some((
            PG_TS_CONFIG_OID,
            match dependency.identity {
                crate::storage::StoredDependencyIdentity::TextSearchConfigurationOid(oid) => oid,
                _ => return None,
            },
        )),
    }
}

/// `object`'s dependency on the schema that contains it. `pg_catalog` and
/// `pg_toast` are pinned and record none.
fn namespace_edge(
    storage: &Storage,
    txid: u32,
    object: ObjectAddress,
    schema: &str,
    visit: &mut dyn FnMut(DependencyEdge) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let namespace = namespace_oid(storage, schema, txid);
    if namespace == 0 || namespace == PG_CATALOG_NS_OID || namespace == PG_TOAST_NS_OID {
        return Ok(());
    }
    visit(DependencyEdge {
        dependent: object,
        referenced: ObjectAddress::new(PG_NAMESPACE_OID, namespace, 0),
        kind: b'n',
    })
}

/// A user-defined type a column, field or routine signature names; built-in
/// types are pinned and record no dependency.
fn user_type_oid(
    storage: &Storage,
    txid: u32,
    ctype: ColType,
    user_type: Option<crate::storage::UserTypeName>,
) -> Option<i32> {
    storage
        .routine_type_oid(ctype, user_type, txid)
        .filter(|oid| crate::storage::NamedTypeKind::from_oid(*oid).is_some())
}

/// Dependencies of tables and materialized views: their row types, columns of
/// user-defined types, inheritance, and column defaults.
fn relation_dependencies(
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    visit: &mut dyn FnMut(DependencyEdge) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    for table in 0..storage.table_count() {
        if !storage.table_slot_visible_to(table, txid) {
            continue;
        }
        let relation = user_table_oid(storage, table);
        let row_type = table_row_type_oid(storage, table, false);
        namespace_edge(
            storage,
            txid,
            ObjectAddress::new(PG_CLASS_OID, relation, 0),
            storage.table_def(table, txid).schema.as_str(),
            visit,
        )?;
        visit(DependencyEdge {
            dependent: ObjectAddress::new(PG_TYPE_OID, row_type, 0),
            referenced: ObjectAddress::new(PG_CLASS_OID, relation, 0),
            kind: b'i',
        })?;
        visit(DependencyEdge {
            dependent: ObjectAddress::new(PG_TYPE_OID, table_row_type_oid(storage, table, true), 0),
            referenced: ObjectAddress::new(PG_TYPE_OID, row_type, 0),
            kind: b'i',
        })?;
        let definition = storage.table_def(table, txid);
        if let Some(composite) = definition.type_membership.composite_slot() {
            visit(DependencyEdge {
                dependent: ObjectAddress::new(PG_CLASS_OID, relation, 0),
                referenced: ObjectAddress::new(
                    PG_TYPE_OID,
                    crate::sql::types::oid::composite_oid(
                        storage.composite_for(composite, txid).creation(),
                    ),
                    0,
                ),
                kind: b'n',
            })?;
        }
        for parent in definition.inheritance.parents_ref() {
            visit(DependencyEdge {
                dependent: ObjectAddress::new(PG_CLASS_OID, relation, 0),
                referenced: ObjectAddress::new(
                    PG_CLASS_OID,
                    user_table_oid(storage, usize::from(*parent)),
                    0,
                ),
                kind: b'n',
            })?;
        }
        // PostgreSQL's `StorePartitionKey` makes each key column an internal
        // part of its partitioned table, so the column cannot be dropped alone.
        if let Some(scheme) = definition.partition.scheme {
            for (position, key) in scheme.keys[..usize::from(scheme.n_keys)].iter().enumerate() {
                if scheme.keys[..position].contains(key) {
                    continue;
                }
                visit(DependencyEdge {
                    dependent: ObjectAddress::new(
                        PG_CLASS_OID,
                        relation,
                        i32::from(definition.attribute_number(usize::from(*key))),
                    ),
                    referenced: ObjectAddress::new(PG_CLASS_OID, relation, 0),
                    kind: b'i',
                })?;
            }
        }
        if let Some(attachment) = definition.partition.attachment {
            visit(DependencyEdge {
                dependent: ObjectAddress::new(PG_CLASS_OID, relation, 0),
                referenced: ObjectAddress::new(
                    PG_CLASS_OID,
                    user_table_oid(storage, usize::from(attachment.parent)),
                    0,
                ),
                kind: b'a',
            })?;
        }
        for (column_index, column) in definition.columns().iter().enumerate() {
            let attribute = i32::from(definition.attribute_number(column_index));
            if let Some(type_oid) = user_type_oid(storage, txid, column.ctype, column.user_type) {
                visit(DependencyEdge {
                    dependent: ObjectAddress::new(PG_CLASS_OID, relation, attribute),
                    referenced: ObjectAddress::new(PG_TYPE_OID, type_oid, 0),
                    kind: b'n',
                })?;
            }
            collation_edge(
                ObjectAddress::new(PG_CLASS_OID, relation, attribute),
                column.collation,
                visit,
            )?;
            let Some(default) = definition.attribute_default_oid(column_index) else {
                continue;
            };
            visit(DependencyEdge {
                dependent: ObjectAddress::new(PG_ATTRDEF_OID, default, 0),
                referenced: ObjectAddress::new(PG_CLASS_OID, relation, attribute),
                kind: b'a',
            })?;
            let mark = arena.mark();
            let mut references = [(0, 0); crate::storage::MAX_STORED_QUERY_DEPENDENCIES];
            let count = crate::sql::exec::column_default_dependencies(
                storage,
                txid,
                column,
                definition.type_inputs.as_ref(),
                definition.attribute_number(column_index),
                arena,
            )
            .map(|dependencies| {
                let mut count = 0;
                for dependency in dependencies.entries() {
                    if let Some(reference) = stored_reference(storage, txid, dependency)
                        .filter(|(_, object)| *object >= crate::storage::FIRST_NORMAL_OBJECT_ID)
                        && !references[..count].contains(&reference)
                    {
                        references[count] = reference;
                        count += 1;
                    }
                }
                count
            });
            // SAFETY: nothing allocated since `mark` outlives this iteration.
            unsafe { arena.rewind_to(mark) };
            for (class, object) in &references[..count?] {
                visit(DependencyEdge {
                    dependent: ObjectAddress::new(PG_ATTRDEF_OID, default, 0),
                    referenced: ObjectAddress::new(*class, *object, 0),
                    kind: b'n',
                })?;
            }
        }
    }
    Ok(())
}

/// Dependencies of user-defined types and of routines on them: row types of
/// views and composite types, array types, composite fields and routine
/// signatures, plus the objects a `BEGIN ATOMIC` body uses.
fn type_dependencies(
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    visit: &mut dyn FnMut(DependencyEdge) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    use crate::sql::types::oid;
    let internal = |dependent: i32, referenced_class: i32, referenced: i32| DependencyEdge {
        dependent: ObjectAddress::new(PG_TYPE_OID, dependent, 0),
        referenced: ObjectAddress::new(referenced_class, referenced, 0),
        kind: b'i',
    };
    for slot in 0..storage.sequence_count() {
        if storage.sequence_slot_visible_to(slot, txid) {
            namespace_edge(
                storage,
                txid,
                ObjectAddress::new(PG_CLASS_OID, sequence_oid(storage, slot), 0),
                storage.sequence_for(slot, txid).schema.as_str(),
                visit,
            )?;
        }
    }
    for (view, _) in storage.views_visible_to(txid) {
        namespace_edge(
            storage,
            txid,
            ObjectAddress::new(PG_CLASS_OID, view_oid(storage, view), 0),
            storage.view(view).schema_for(txid).as_str(),
            visit,
        )?;
        let row_type = view_row_type_oid(storage, view, false);
        visit(internal(row_type, PG_CLASS_OID, view_oid(storage, view)))?;
        // A view's columns depend on their user-defined types and collations,
        // as a table's do.
        let (_, columns) = storage.view_column_shape(view, txid);
        for position in 0..columns {
            let Some((_, column_type)) = storage.view_column(view, txid, position) else {
                break;
            };
            let column = ObjectAddress::new(
                PG_CLASS_OID,
                view_oid(storage, view),
                i32::try_from(position + 1).expect("view attribute fits"),
            );
            if crate::storage::NamedTypeKind::from_oid(column_type.oid()).is_some() {
                visit(DependencyEdge {
                    dependent: column,
                    referenced: ObjectAddress::new(PG_TYPE_OID, column_type.oid(), 0),
                    kind: b'n',
                })?;
            }
            collation_edge(column, column_type.collation(), visit)?;
        }
        visit(internal(
            view_row_type_oid(storage, view, true),
            PG_TYPE_OID,
            row_type,
        ))?;
    }
    for slot in 0..storage.domain_count() {
        if storage.domain_slot_visible_to(slot, txid) {
            let domain = storage.domain_for(slot, txid);
            let creation = domain.creation();
            namespace_edge(
                storage,
                txid,
                ObjectAddress::new(PG_TYPE_OID, oid::domain_oid(creation), 0),
                domain.schema.as_str(),
                visit,
            )?;
            visit(internal(
                oid::domain_array_oid(creation),
                PG_TYPE_OID,
                oid::domain_oid(creation),
            ))?;
        }
    }
    for slot in 0..storage.enum_count() {
        if storage.enum_slot_visible_to(slot, txid) {
            let definition = storage.enum_for(slot, txid);
            let creation = definition.creation();
            namespace_edge(
                storage,
                txid,
                ObjectAddress::new(PG_TYPE_OID, oid::enum_oid(creation), 0),
                definition.schema.as_str(),
                visit,
            )?;
            visit(internal(
                oid::enum_array_oid(creation),
                PG_TYPE_OID,
                oid::enum_oid(creation),
            ))?;
        }
    }
    for slot in 0..storage.composite_count() {
        if !storage.composite_slot_visible_to(slot, txid) {
            continue;
        }
        let definition = storage.composite_for(slot, txid);
        let type_oid = oid::composite_oid(definition.creation());
        let relation = named_composite_relation_oid(storage, slot);
        namespace_edge(
            storage,
            txid,
            ObjectAddress::new(PG_TYPE_OID, type_oid, 0),
            definition.schema.as_str(),
            visit,
        )?;
        visit(DependencyEdge {
            dependent: ObjectAddress::new(PG_CLASS_OID, relation, 0),
            referenced: ObjectAddress::new(PG_TYPE_OID, type_oid, 0),
            kind: b'i',
        })?;
        visit(DependencyEdge {
            dependent: ObjectAddress::new(
                PG_TYPE_OID,
                oid::composite_array_oid(definition.creation()),
                0,
            ),
            referenced: ObjectAddress::new(PG_TYPE_OID, type_oid, 0),
            kind: b'i',
        })?;
        for field in definition.active_fields_for(txid) {
            collation_edge(
                ObjectAddress::new(PG_CLASS_OID, relation, i32::from(field.attribute_number)),
                field.collation,
                visit,
            )?;
            if let Some(field_type) = user_type_oid(storage, txid, field.ctype, field.user_type) {
                visit(DependencyEdge {
                    dependent: ObjectAddress::new(
                        PG_CLASS_OID,
                        relation,
                        i32::from(field.attribute_number),
                    ),
                    referenced: ObjectAddress::new(PG_TYPE_OID, field_type, 0),
                    kind: b'n',
                })?;
            }
        }
    }
    for slot in 0..storage.routine_count() {
        if storage.routine_identity_for(slot, txid).is_none() {
            continue;
        }
        let routine = storage.routine_for(slot, txid);
        let routine_oid = crate::storage::routine_oid(&routine);
        namespace_edge(
            storage,
            txid,
            ObjectAddress::new(PG_PROC_OID, routine_oid, 0),
            routine.schema_for(txid).as_str(),
            visit,
        )?;
        // `internal`, `c` and `sql` are pinned; `plpgsql` is an ordinary
        // catalog object every PL/pgSQL routine depends on.
        if routine.language == crate::storage::RoutineLanguage::PlPgSql {
            visit(DependencyEdge {
                dependent: ObjectAddress::new(PG_PROC_OID, routine_oid, 0),
                referenced: ObjectAddress::new(
                    PG_LANGUAGE_OID,
                    routine_language_oid(routine.language),
                    0,
                ),
                kind: b'n',
            })?;
        }
        let mut types = [0; 2 * crate::storage::MAX_ROUTINE_ARGUMENTS + 3];
        let mut count = 0;
        let mut note = |ctype, user_type| {
            if let Some(type_oid) = user_type_oid(storage, txid, ctype, user_type)
                && !types[..count].contains(&type_oid)
                && count < types.len()
            {
                types[count] = type_oid;
                count += 1;
            }
        };
        if let crate::storage::RoutineKind::Function { result }
        | crate::storage::RoutineKind::SetFunction { result } = routine.kind
        {
            note(result.ctype, result.user_type);
        }
        for argument in routine
            .arguments()
            .iter()
            .chain(routine.result_columns[..routine.result_column_count].iter())
        {
            note(argument.ctype, argument.user_type);
        }
        if let crate::storage::RoutineKind::Aggregate(aggregate) = routine.kind {
            for result in [aggregate.state_type, aggregate.result_type]
                .into_iter()
                .chain(aggregate.moving.map(|moving| moving.state_type))
            {
                note(result.ctype, result.user_type);
            }
        }
        for type_oid in &types[..count] {
            visit(DependencyEdge {
                dependent: ObjectAddress::new(PG_PROC_OID, routine_oid, 0),
                referenced: ObjectAddress::new(PG_TYPE_OID, *type_oid, 0),
                kind: b'n',
            })?;
        }
        if let crate::storage::RoutineKind::Aggregate(aggregate) = routine.kind {
            let mut functions = [0; 8];
            let mut function_count = 0;
            for function in aggregate.support_function_oids().into_iter().flatten() {
                if function >= crate::storage::FIRST_NORMAL_OBJECT_ID
                    && !functions[..function_count].contains(&function)
                {
                    functions[function_count] = function;
                    function_count += 1;
                }
            }
            for function in &functions[..function_count] {
                visit(DependencyEdge {
                    dependent: ObjectAddress::new(PG_PROC_OID, routine_oid, 0),
                    referenced: ObjectAddress::new(PG_PROC_OID, *function, 0),
                    kind: b'n',
                })?;
            }
            if let Some(operator) = aggregate
                .sort_operator_oid
                .filter(|operator| *operator >= crate::storage::FIRST_NORMAL_OBJECT_ID)
            {
                visit(DependencyEdge {
                    dependent: ObjectAddress::new(PG_PROC_OID, routine_oid, 0),
                    referenced: ObjectAddress::new(PG_OPERATOR_OID, operator, 0),
                    kind: b'n',
                })?;
            }
        }
        if routine.body_kind == crate::storage::RoutineBodyKind::String {
            continue;
        }
        let mark = arena.mark();
        let mut references = [(0, 0, 0); 4 * crate::storage::MAX_STORED_QUERY_DEPENDENCIES];
        let mut count = 0;
        for dependency in storage
            .snapshot_routine_dependencies_for(slot, txid, arena)?
            .entries()
        {
            let Some((class, object)) = stored_reference(storage, txid, dependency)
                .filter(|(_, object)| *object >= crate::storage::FIRST_NORMAL_OBJECT_ID)
            else {
                continue;
            };
            let mut add = |sub: i32| {
                if !references[..count].contains(&(class, object, sub)) && count < references.len()
                {
                    references[count] = (class, object, sub);
                    count += 1;
                }
            };
            if dependency.referenced_columns.is_empty() {
                add(0);
            } else {
                for column in 0..MAX_COLUMNS {
                    if dependency.referenced_columns.contains(column) {
                        add(stored_column_number(storage, txid, dependency, column));
                    }
                }
            }
        }
        // SAFETY: the snapshot is not used past this point.
        unsafe { arena.rewind_to(mark) };
        for (class, object, sub) in &references[..count] {
            visit(DependencyEdge {
                dependent: ObjectAddress::new(PG_PROC_OID, routine_oid, 0),
                referenced: ObjectAddress::new(*class, *object, *sub),
                kind: b'n',
            })?;
        }
    }
    Ok(())
}

/// The attribute number of a column position a stored dependency records.
fn stored_column_number(
    storage: &Storage,
    txid: u32,
    dependency: &crate::storage::StoredQueryDependency,
    column: usize,
) -> i32 {
    match dependency.class {
        crate::storage::DependencyClass::Table => i32::from(
            storage
                .table_def(usize::from(dependency.slot), txid)
                .attribute_number(column),
        ),
        crate::storage::DependencyClass::View => column as i32 + 1,
        _ => unreachable!("only relation dependencies contain column positions"),
    }
}

/// A policy belongs to its table and depends on what its expressions use.
fn policy_dependencies(
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    visit: &mut dyn FnMut(DependencyEdge) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    for (slot, policy) in storage.policies_with_slots_visible_to(txid) {
        let dependent = ObjectAddress::new(PG_POLICY_OID, crate::storage::policy_oid(&policy), 0);
        visit(DependencyEdge {
            dependent,
            referenced: ObjectAddress::new(
                PG_CLASS_OID,
                user_table_oid(storage, usize::from(policy.table)),
                0,
            ),
            kind: b'a',
        })?;
        let mark = arena.mark();
        let mut references = [(0, 0, 0); 4 * crate::storage::MAX_STORED_QUERY_DEPENDENCIES];
        let mut count = 0;
        for dependency in storage
            .snapshot_policy_dependencies(slot, txid, arena)?
            .entries()
        {
            let Some((class, object)) = stored_reference(storage, txid, dependency)
                .filter(|(_, object)| *object >= crate::storage::FIRST_NORMAL_OBJECT_ID)
            else {
                continue;
            };
            let mut add = |sub: i32| {
                if !references[..count].contains(&(class, object, sub)) && count < references.len()
                {
                    references[count] = (class, object, sub);
                    count += 1;
                }
            };
            if dependency.referenced_columns.is_empty() {
                add(0);
            } else {
                for column in 0..MAX_COLUMNS {
                    if dependency.referenced_columns.contains(column) {
                        add(stored_column_number(storage, txid, dependency, column));
                    }
                }
            }
        }
        // SAFETY: the snapshot is not used past this point.
        unsafe { arena.rewind_to(mark) };
        for (class, object, sub) in &references[..count] {
            visit(DependencyEdge {
                dependent,
                referenced: ObjectAddress::new(*class, *object, *sub),
                kind: b'n',
            })?;
        }
    }
    Ok(())
}

/// Attribute numbers of `relation` that `NEW.column` or `OLD.column` names in
/// `fragments`, each once, in attribute order.
fn transition_columns<'a, 'f>(
    storage: &Storage,
    txid: u32,
    relation: crate::storage::RowTypeRelation,
    fragments: impl Iterator<Item = &'f str>,
    arena: &'a Arena,
) -> Result<&'a [u16], SqlError> {
    let attribute = |name: &str| -> Option<u16> {
        match relation {
            crate::storage::RowTypeRelation::Table(slot) => {
                let definition = storage.table_def(slot, txid);
                definition
                    .column_index(name)
                    .map(|column| definition.attribute_number(column))
            }
            crate::storage::RowTypeRelation::View(slot) => {
                let (_, count) = storage.view_column_shape(slot, txid);
                (0..count)
                    .find(|index| {
                        storage
                            .view_column(slot, txid, *index)
                            .is_some_and(|(column, _)| column.as_str() == name)
                    })
                    .and_then(|index| u16::try_from(index + 1).ok())
            }
        }
    };
    let mut found = ColumnSet::EMPTY;
    for fragment in fragments {
        let mut lexer = crate::sql::lexer::Lexer::new(fragment, arena);
        let mut previous = [crate::sql::lexer::Tok::Eof; 2];
        loop {
            let token = lexer.next_token().map_err(|_| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "stored trigger or rule text does not scan"
                )
            })?;
            if token == crate::sql::lexer::Tok::Eof {
                break;
            }
            if let (
                [
                    crate::sql::lexer::Tok::Ident(pseudo),
                    crate::sql::lexer::Tok::Op("."),
                ],
                crate::sql::lexer::Tok::Ident(column) | crate::sql::lexer::Tok::QuotedIdent(column),
            ) = (previous, token)
                && matches!(pseudo, "new" | "old")
                && let Some(number) = attribute(column)
            {
                found.insert(usize::from(number) - 1);
            }
            previous = [previous[1], token];
        }
    }
    let count = (0..MAX_COLUMNS)
        .filter(|column| found.contains(*column))
        .count();
    let attributes = arena
        .alloc_slice_with(count, |_| 0u16)
        .map_err(|_| arena_full())?;
    for (slot, column) in attributes
        .iter_mut()
        .zip((0..MAX_COLUMNS).filter(|column| found.contains(*column)))
    {
        *slot = u16::try_from(column + 1).expect("attribute number fits");
    }
    Ok(attributes)
}

/// The dependency of a column, index or attribute on a user-defined
/// collation; built-in collations are pinned and have no row.
fn collation_edge(
    dependent: ObjectAddress,
    collation: crate::sql::ast::Collation,
    visit: &mut dyn FnMut(DependencyEdge) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    if !matches!(collation, crate::sql::ast::Collation::Catalog(_)) {
        return Ok(());
    }
    visit(DependencyEdge {
        dependent,
        referenced: ObjectAddress::new(PG_COLLATION_OID, collation.oid(), 0),
        kind: b'n',
    })
}
