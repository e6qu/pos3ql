//! Set-returning functions: the ones written in the select list, and the ones
//! written in FROM.
//!
//! A set-returning function in the select list makes each input row produce as
//! many output rows as the function yields, so the projection has to know how
//! many that is before it can emit any of them. In FROM the same function is a
//! table, and needs a definition — column names and types — that the ordinary
//! scan machinery can resolve against, which is what is synthesized here.

use crate::mem::arena::Arena;
use crate::sql::ast::{Expr, Select, SelectItem, TableRef};
use crate::sql::eval::{ColumnLookup, EvalHooks, ProjectSetValue, SqlError, eval_full, sqlstate};
use crate::sql::exec::MAX_PROJ;

/// Pieces one `string_to_table` call may split into.
const MAX_PIECES: usize = 1024;
use crate::sql::types::{ColDesc, ColType, Datum};
use crate::sql_err;
use crate::storage::{ColumnMeta, MAX_COLUMNS, RoutineDef, SqlName, Storage, TableDef};
use crate::util::StackStr;

use super::setops::describe_set_body;
use super::subquery::subquery_witness;

use super::{QueryScope, arena_full, describe_scope_items, record_star_width};

/// Whether `name` is one of the supported set-returning functions.
pub(crate) fn is_srf_name(name: &str) -> bool {
    is_event_trigger_introspection(name)
        || name.eq_ignore_ascii_case("_pg_expandarray")
        || name.eq_ignore_ascii_case("unnest")
        || name.eq_ignore_ascii_case("generate_series")
        || name.eq_ignore_ascii_case("regexp_matches")
        || name.eq_ignore_ascii_case("jsonb_object_keys")
        || name.eq_ignore_ascii_case("json_object_keys")
        || name.eq_ignore_ascii_case("jsonb_array_elements")
        || name.eq_ignore_ascii_case("json_array_elements")
        || name.eq_ignore_ascii_case("jsonb_array_elements_text")
        || name.eq_ignore_ascii_case("json_array_elements_text")
        || name.eq_ignore_ascii_case("jsonb_path_query")
        || name.eq_ignore_ascii_case("jsonb_path_query_tz")
        || name.eq_ignore_ascii_case("json_populate_recordset")
        || name.eq_ignore_ascii_case("jsonb_populate_recordset")
        || name.eq_ignore_ascii_case("regexp_split_to_table")
        || name.eq_ignore_ascii_case("string_to_table")
        || name.eq_ignore_ascii_case("generate_subscripts")
        || name.eq_ignore_ascii_case("pg_options_to_table")
        || name.eq_ignore_ascii_case("pg_get_sequence_data")
        || name.eq_ignore_ascii_case("pg_get_publication_tables")
        || name.eq_ignore_ascii_case("ts_parse")
        || name.eq_ignore_ascii_case("ts_token_type")
        || name.eq_ignore_ascii_case("ts_debug")
        || name.eq_ignore_ascii_case("ts_stat")
        || is_json_each_name(name)
}

fn is_event_trigger_introspection(name: &str) -> bool {
    name.eq_ignore_ascii_case("pg_event_trigger_ddl_commands")
        || name.eq_ignore_ascii_case("pg_event_trigger_dropped_objects")
}

fn is_logical_slot_record_function(name: &str) -> bool {
    name.eq_ignore_ascii_case("pg_create_logical_replication_slot")
        || name.eq_ignore_ascii_case("pg_copy_logical_replication_slot")
        || name.eq_ignore_ascii_case("pg_replication_slot_advance")
}

fn require_no_arguments(name: &str, arguments: &[&Expr<'_>]) -> Result<(), SqlError> {
    if arguments.is_empty() {
        Ok(())
    } else {
        Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "function {}(...) does not exist",
            name
        ))
    }
}

fn event_trigger_column_names(name: &str) -> &'static [&'static str] {
    if name.eq_ignore_ascii_case("pg_event_trigger_ddl_commands") {
        &[
            "classid",
            "objid",
            "objsubid",
            "command_tag",
            "object_type",
            "schema_name",
            "object_identity",
            "in_extension",
            "command",
        ]
    } else {
        &[
            "classid",
            "objid",
            "objsubid",
            "original",
            "normal",
            "is_temporary",
            "object_type",
            "schema_name",
            "object_name",
            "object_identity",
            "address_names",
            "address_args",
        ]
    }
}

fn event_trigger_column_types(name: &str) -> &'static [ColType] {
    if name.eq_ignore_ascii_case("pg_event_trigger_ddl_commands") {
        &[
            ColType::Oid,
            ColType::Oid,
            ColType::Int4,
            ColType::Text,
            ColType::Text,
            ColType::Text,
            ColType::Text,
            ColType::Bool,
            ColType::PgDdlCommand,
        ]
    } else {
        &[
            ColType::Oid,
            ColType::Oid,
            ColType::Int4,
            ColType::Bool,
            ColType::Bool,
            ColType::Bool,
            ColType::Text,
            ColType::Text,
            ColType::Text,
            ColType::Text,
            ColType::Array(crate::sql::types::ArrElem::Text),
            ColType::Array(crate::sql::types::ArrElem::Text),
        ]
    }
}

fn event_text_array<'a, const N: usize>(
    parts: &[StackStr<N>],
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let mut values = [Datum::Null; crate::sql::event_trigger::MAX_ADDRESS_PARTS];
    for (value, part) in values.iter_mut().zip(parts) {
        *value = Datum::Text(part.as_str());
    }
    Ok(Datum::Array {
        element: crate::sql::types::ArrElem::Text,
        raw: crate::sql::array::build(&values[..parts.len()], arena)?,
    })
}

fn event_trigger_rows<'a>(
    name: &str,
    arguments: &[&Expr<'_>],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a [&'a [u8]], SqlError> {
    require_no_arguments(name, arguments)?;
    const EMPTY: &[u8] = &[];
    if name.eq_ignore_ascii_case("pg_event_trigger_ddl_commands") {
        return crate::sql::event_trigger::with_ddl_commands(|commands| {
            let rows = arena
                .alloc_slice_with(commands.len(), |_| EMPTY)
                .map_err(|_| arena_full())?;
            for (slot, command) in rows.iter_mut().zip(commands) {
                let object = command.object(storage, txid)?;
                let addressed = command.addressed();
                let class_id = if addressed {
                    Datum::Oid(object.class_id as u32)
                } else {
                    Datum::Null
                };
                let object_id = if addressed {
                    Datum::Oid(object.object_id as u32)
                } else {
                    Datum::Null
                };
                let sub_id = if addressed {
                    Datum::Int4(object.sub_id)
                } else {
                    Datum::Null
                };
                let identity = if addressed {
                    Datum::Text(object.identity.as_str())
                } else {
                    Datum::Null
                };
                *slot = crate::sql::exec::encode_projected_pub(
                    &[
                        class_id,
                        object_id,
                        sub_id,
                        Datum::Text(command.command_tag.as_str()),
                        Datum::Text(object.object_type.as_str()),
                        object
                            .schema_name
                            .as_ref()
                            .map_or(Datum::Null, |v| Datum::Text(v.as_str())),
                        identity,
                        Datum::Bool(command.in_extension),
                        Datum::PgDdlCommand,
                    ],
                    arena,
                )?;
            }
            Ok(&*rows)
        })?;
    }
    crate::sql::event_trigger::with_dropped_objects(|objects| {
        let rows = arena
            .alloc_slice_with(objects.len(), |_| EMPTY)
            .map_err(|_| arena_full())?;
        for (slot, dropped) in rows.iter_mut().zip(objects) {
            let object = dropped.object(storage, txid)?;
            *slot = crate::sql::exec::encode_projected_pub(
                &[
                    Datum::Oid(object.class_id as u32),
                    Datum::Oid(object.object_id as u32),
                    Datum::Int4(object.sub_id),
                    Datum::Bool(dropped.original),
                    Datum::Bool(dropped.normal),
                    Datum::Bool(dropped.temporary),
                    Datum::Text(object.object_type.as_str()),
                    object
                        .schema_name
                        .as_ref()
                        .map_or(Datum::Null, |v| Datum::Text(v.as_str())),
                    object
                        .object_name
                        .as_ref()
                        .map_or(Datum::Null, |v| Datum::Text(v.as_str())),
                    Datum::Text(object.identity.as_str()),
                    event_text_array(&object.address_names[..object.address_name_count], arena)?,
                    event_text_array(&object.address_args[..object.address_arg_count], arena)?,
                ],
                arena,
            )?;
        }
        Ok(&*rows)
    })?
}

fn srf_signature_error(name: &str) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "function {}(...) does not exist",
        name
    )
}

fn evaluate_publication_arguments<'a, R: ColumnLookup<'a>>(
    arguments: &'a [&'a Expr<'a>],
    variadic: bool,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &R,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<&'a [Datum<'a>], SqlError> {
    evaluate_publication_arguments_with(arguments, variadic, arena, |argument| {
        eval_full(argument, arena, params, row, hooks)
    })
}

fn evaluate_publication_arguments_with<'a, F>(
    arguments: &'a [&'a Expr<'a>],
    variadic: bool,
    arena: &'a Arena,
    mut evaluate: F,
) -> Result<&'a [Datum<'a>], SqlError>
where
    F: FnMut(&'a Expr<'a>) -> Result<Datum<'a>, SqlError>,
{
    if arguments.is_empty() {
        return Err(srf_signature_error("pg_get_publication_tables"));
    }
    if variadic {
        if arguments.len() != 1 {
            return Err(srf_signature_error("pg_get_publication_tables"));
        }
        return match evaluate(arguments[0])? {
            Datum::Null => Ok(&[]),
            Datum::Array {
                element: crate::sql::types::ArrElem::Text,
                raw,
            } => {
                let values = arena
                    .alloc_slice_with(crate::sql::array::len(raw), |_| Datum::Null)
                    .map_err(|_| arena_full())?;
                for (index, value) in values.iter_mut().enumerate() {
                    *value = match crate::sql::array::get(
                        raw,
                        crate::sql::types::ArrElem::Text,
                        index,
                    ) {
                        Some(Datum::Text(value)) => Datum::Text(value),
                        Some(Datum::Null) => {
                            return Err(sql_err!(
                                sqlstate::NULL_VALUE_NOT_ALLOWED,
                                "null array element not allowed in this context"
                            ));
                        }
                        Some(_) => unreachable!("text array has text elements"),
                        None => unreachable!("array length fixes every publication index"),
                    };
                }
                Ok(values)
            }
            _ => Err(srf_signature_error("pg_get_publication_tables")),
        };
    }
    let values = arena
        .alloc_slice_with(arguments.len(), |_| Datum::Null)
        .map_err(|_| arena_full())?;
    for (value, argument) in values.iter_mut().zip(arguments) {
        *value = evaluate(argument)?;
        match value {
            Datum::Text(_) | Datum::Bpchar(_) => {}
            Datum::Null => {
                return Err(sql_err!(
                    sqlstate::NULL_VALUE_NOT_ALLOWED,
                    "null array element not allowed in this context"
                ));
            }
            _ => return Err(srf_signature_error("pg_get_publication_tables")),
        }
    }
    Ok(values)
}

#[derive(Clone, Copy)]
struct PublishedRelation {
    publication_slot: usize,
    relation_slot: usize,
}

fn publication_member_for(
    storage: &Storage,
    txid: u32,
    definition: crate::storage::PublicationDefinition,
    table_slot: usize,
) -> Option<usize> {
    if let Some(index) = definition.tables[..definition.table_count]
        .iter()
        .position(|member| usize::from(*member) == table_slot)
    {
        return Some(index);
    }
    let mut partition = table_slot;
    while let Some(attachment) = storage.table_def(partition, txid).partition.attachment {
        partition = usize::from(attachment.parent);
        if let Some(index) = definition.tables[..definition.table_count]
            .iter()
            .position(|member| usize::from(*member) == partition)
        {
            return Some(index);
        }
    }
    definition.tables[..definition.table_count]
        .iter()
        .enumerate()
        .find_map(|(index, member)| {
            let member = usize::from(*member);
            (definition.table_include_descendants[index]
                && storage.relation_descends_from(table_slot, member, txid))
            .then_some(index)
        })
}

fn publication_schema_member_for(
    storage: &Storage,
    txid: u32,
    definition: crate::storage::PublicationDefinition,
    table_slot: usize,
) -> bool {
    let mut current = table_slot;
    loop {
        let schema_selected = storage
            .find_schema(storage.table_def(current, txid).schema.as_str())
            .is_some_and(|slot| {
                definition.schemas[..definition.schema_count].contains(&(slot as u8))
            });
        if schema_selected {
            return true;
        }
        let Some(attachment) = storage.table_def(current, txid).partition.attachment else {
            return false;
        };
        current = usize::from(attachment.parent);
    }
}

fn publication_partition_root_for(storage: &Storage, txid: u32, table_slot: usize) -> usize {
    let mut current = table_slot;
    while let Some(attachment) = storage.table_def(current, txid).partition.attachment {
        current = usize::from(attachment.parent);
    }
    current
}

fn publication_output_relation_for(
    storage: &Storage,
    txid: u32,
    definition: crate::storage::PublicationDefinition,
    table_slot: usize,
    explicit: Option<usize>,
) -> usize {
    if !definition.publish_via_partition_root {
        return table_slot;
    }
    if definition.all_tables {
        return publication_partition_root_for(storage, txid, table_slot);
    }
    if let Some(index) = explicit {
        return usize::from(definition.tables[index]);
    }
    let mut current = table_slot;
    let mut selected = table_slot;
    loop {
        let schema_selected = storage
            .find_schema(storage.table_def(current, txid).schema.as_str())
            .is_some_and(|slot| {
                definition.schemas[..definition.schema_count].contains(&(slot as u8))
            });
        if schema_selected {
            selected = current;
        }
        let Some(attachment) = storage.table_def(current, txid).partition.attachment else {
            return selected;
        };
        current = usize::from(attachment.parent);
    }
}

fn publication_int2vector<'a>(mask: u64, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let count = mask.count_ones() as usize;
    let raw = arena
        .alloc_slice_with(count * 2, |_| 0u8)
        .map_err(|_| arena_full())?;
    let mut output = 0usize;
    for column in 0..MAX_COLUMNS {
        if mask & (1u64 << column) == 0 {
            continue;
        }
        let attribute_number = i16::try_from(column + 1).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "publication column number is too large"
            )
        })?;
        raw[output * 2..output * 2 + 2].copy_from_slice(&attribute_number.to_le_bytes());
        output += 1;
    }
    Ok(Datum::Int2Vector(raw))
}

fn publication_table_rows<'a>(
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    arguments: &[Datum<'a>],
) -> Result<&'a [&'a [u8]], SqlError> {
    const MAX_RESULTS: usize = crate::sql::parser::MAX_ROWS;
    let mut names = [""; MAX_RESULTS];
    let mut name_count = 0usize;
    for argument in arguments {
        match argument {
            Datum::Text(name) | Datum::Bpchar(name) => {
                if name_count == names.len() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "pg_get_publication_tables has too many publication names"
                    ));
                }
                names[name_count] = name;
                name_count += 1;
            }
            _ => return Err(srf_signature_error("pg_get_publication_tables")),
        }
    }

    let empty = PublishedRelation {
        publication_slot: usize::MAX,
        relation_slot: usize::MAX,
    };
    let mut published = [empty; MAX_RESULTS];
    let mut published_count = 0usize;
    let mut any_via_root = false;
    for name in &names[..name_count] {
        let (publication_slot, publication) = storage
            .publications_with_slots_visible_to(txid)
            .find(|(_, publication)| publication.name_for(txid).as_str() == *name)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "publication \"{}\" does not exist",
                    name
                )
            })?;
        let definition = publication.definition_for(txid);
        any_via_root |= definition.publish_via_partition_root;
        let publication_start = published_count;
        for (table_slot, table) in storage.live_tables() {
            if !table.visible_to(txid) {
                continue;
            }
            let explicit = publication_member_for(storage, txid, definition, table_slot);
            let schema = publication_schema_member_for(storage, txid, definition, table_slot);
            if !definition.all_tables && !schema && explicit.is_none() {
                continue;
            }
            if !definition.publish_via_partition_root
                && storage
                    .table_def(table_slot, txid)
                    .partition
                    .is_partitioned()
            {
                continue;
            }
            let relation_slot =
                publication_output_relation_for(storage, txid, definition, table_slot, explicit);
            if published[publication_start..published_count]
                .iter()
                .any(|entry| entry.relation_slot == relation_slot)
            {
                continue;
            }
            if published_count == published.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "pg_get_publication_tables exceeds {} rows",
                    published.len()
                ));
            }
            published[published_count] = PublishedRelation {
                publication_slot,
                relation_slot,
            };
            published_count += 1;
        }
    }

    const EMPTY_ROW: &[u8] = &[];
    let rows = arena
        .alloc_slice_with(published_count, |_| EMPTY_ROW)
        .map_err(|_| arena_full())?;
    let mut row_count = 0usize;
    for info in &published[..published_count] {
        if any_via_root {
            let mut ancestor = storage
                .table_def(info.relation_slot, txid)
                .partition
                .attachment;
            let mut shadowed = false;
            while let Some(attachment) = ancestor {
                let parent = usize::from(attachment.parent);
                if published[..published_count]
                    .iter()
                    .any(|other| other.relation_slot == parent)
                {
                    shadowed = true;
                    break;
                }
                ancestor = storage.table_def(parent, txid).partition.attachment;
            }
            if shadowed {
                continue;
            }
        }
        let (_, publication) = storage
            .publications_with_slots_visible_to(txid)
            .find(|(slot, _)| *slot == info.publication_slot)
            .expect("published relation retains a visible publication");
        let definition = publication.definition_for(txid);
        let table = storage.table_def(info.relation_slot, txid);
        let explicit = definition.tables[..definition.table_count]
            .iter()
            .position(|member| usize::from(*member) == info.relation_slot);
        let schema = storage
            .find_schema(table.schema.as_str())
            .is_some_and(|slot| {
                definition.schemas[..definition.schema_count].contains(&(slot as u8))
            });
        let implicit_mask = || {
            table
                .columns()
                .iter()
                .enumerate()
                .filter(|(_, column)| {
                    !column.default.is_generated()
                        || definition.publish_generated_columns
                            == crate::storage::PublishGeneratedColumns::Stored
                })
                .fold(0u64, |mask, (column, _)| mask | (1u64 << column))
        };
        let (column_mask, filter) = if definition.all_tables || schema {
            (implicit_mask(), None)
        } else if let Some(index) = explicit {
            let selected = definition.table_column_masks[index];
            (
                if selected == 0 {
                    implicit_mask()
                } else {
                    selected
                },
                (!definition.table_filters.get(index).is_empty())
                    .then(|| definition.table_filters.get(index)),
            )
        } else {
            (implicit_mask(), None)
        };
        let qual = match filter {
            Some(filter) => {
                let rendered = crate::stack_format!(66, "({filter})");
                Datum::Text(
                    arena
                        .alloc_str(rendered.as_str())
                        .map_err(|_| arena_full())?,
                )
            }
            None => Datum::Null,
        };
        rows[row_count] = crate::sql::exec::encode_projected_pub(
            &[
                Datum::Oid(crate::sql::catalog::publication_oid(info.publication_slot) as u32),
                Datum::Oid(crate::sql::catalog::user_table_oid(info.relation_slot) as u32),
                publication_int2vector(column_mask, arena)?,
                qual,
            ],
            arena,
        )?;
        row_count += 1;
    }
    Ok(&rows[..row_count])
}

/// The set-returning function call (if any) driving a single expression's
/// expansion — the outermost SRF reachable through wrapping expressions.
pub(super) fn srf_in_expr<'a>(e: &'a Expr<'a>) -> Option<&'a Expr<'a>> {
    let mut found = None;
    let _ = for_each_srf(e, &mut |call| {
        if found.is_none() {
            found = Some(call);
        }
        Ok(())
    });
    found
}

/// Visits the independently lockstepped outer SRFs in one target expression.
/// Nested calls belong to the lower project-set level owned by their parent.
fn for_each_srf<'a>(
    expression: &'a Expr<'a>,
    visit: &mut dyn FnMut(&'a Expr<'a>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    if matches!(expression, Expr::Call { name, .. } if is_srf_name(name)) {
        return visit(expression);
    }
    super::walk_children(expression, &mut |child| for_each_srf(child, visit))
}

/// The SRF (if any) driving a single select item's expansion.
pub(super) fn srf_in_item<'a>(item: &'a SelectItem<'a>) -> Option<&'a Expr<'a>> {
    match item {
        SelectItem::Expr { expression, .. } => srf_in_expr(expression),
        SelectItem::RecordStar(base) => srf_in_expr(base),
        SelectItem::Wildcard | SelectItem::TableWildcard(_) => None,
    }
}

/// Finds a set-returning function call among the SELECT items (the whole call
/// node, so the caller can compute its row count), or None for a single row.
pub(super) fn find_srf<'a>(items: &'a [SelectItem<'a>]) -> Option<&'a Expr<'a>> {
    items.iter().find_map(srf_in_item)
}

/// Whether a SELECT list contains a built-in or catalog set-returning call.
/// This is a planning predicate: overload selection still happens at the
/// typed execution boundary once the input row is available.
pub(crate) fn expression_has_project_set(
    expression: &Expr<'_>,
    storage: &Storage,
    txid: u32,
) -> bool {
    if let Expr::Call { name, args, .. } = expression
        && (is_srf_name(name) || storage.has_set_routine_candidate(name, args.len(), txid))
    {
        return true;
    }
    let mut found = false;
    let _ = super::walk_children(expression, &mut |child| {
        found |= expression_has_project_set(child, storage, txid);
        Ok(())
    });
    found
}

pub(super) fn has_project_set(items: &[SelectItem<'_>], storage: &Storage, txid: u32) -> bool {
    items.iter().any(|item| match item {
        SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
            expression_has_project_set(expression, storage, txid)
        }
        SelectItem::Wildcard | SelectItem::TableWildcard(_) => false,
    })
}

pub(super) struct ProjectSet<'a> {
    pub count: usize,
    pub values: &'a [ProjectSetValue<'a>],
    pub any: bool,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_project_set<'a, R: ColumnLookup<'a>>(
    items: &'a [SelectItem<'a>],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &R,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<ProjectSet<'a>, SqlError> {
    let empty = ProjectSetValue {
        node: core::ptr::null(),
        values: &[],
        fixed_index: None,
    };
    let mut materialized = [empty; MAX_PROJ];
    let mut materialized_count = 0usize;
    let mut max = 0usize;
    let mut any = false;

    fn visit<'a, R: ColumnLookup<'a>>(
        expression: &'a Expr<'a>,
        storage: &'a Storage,
        txid: u32,
        arena: &'a Arena,
        params: &[Datum<'a>],
        row: &R,
        hooks: &EvalHooks<'_, 'a>,
        materialized: &mut [ProjectSetValue<'a>; MAX_PROJ],
        materialized_count: &mut usize,
        max: &mut usize,
        any: &mut bool,
    ) -> Result<(), SqlError> {
        let Expr::Call { name, args, .. } = expression else {
            return super::walk_children(expression, &mut |child| {
                visit(
                    child,
                    storage,
                    txid,
                    arena,
                    params,
                    row,
                    hooks,
                    materialized,
                    materialized_count,
                    max,
                    any,
                )
            });
        };
        let built_in = is_srf_name(name);
        let catalog = storage.has_set_routine_candidate(name, args.len(), txid);
        if !built_in && !catalog {
            return super::walk_children(expression, &mut |child| {
                visit(
                    child,
                    storage,
                    txid,
                    arena,
                    params,
                    row,
                    hooks,
                    materialized,
                    materialized_count,
                    max,
                    any,
                )
            });
        }
        if *materialized_count == materialized.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "select list has too many set-returning functions"
            ));
        }
        let values = materialize_call(expression, storage, txid, arena, params, row, hooks)?;
        materialized[*materialized_count] = ProjectSetValue {
            node: expression as *const Expr<'a> as *const (),
            values,
            fixed_index: None,
        };
        *materialized_count += 1;
        *max = (*max).max(values.len());
        *any = true;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn materialize_call<'a, R: ColumnLookup<'a>>(
        expression: &'a Expr<'a>,
        storage: &'a Storage,
        txid: u32,
        arena: &'a Arena,
        params: &[Datum<'a>],
        row: &R,
        hooks: &EvalHooks<'_, 'a>,
    ) -> Result<&'a [Datum<'a>], SqlError> {
        let Expr::Call { args, .. } = expression else {
            unreachable!("project-set materialization requires a call")
        };
        let empty = ProjectSetValue {
            node: core::ptr::null(),
            values: &[],
            fixed_index: None,
        };
        let mut nested = [empty; MAX_PROJ];
        let mut nested_count = 0usize;
        let mut nested_max = 0usize;
        let mut nested_any = false;
        for argument in *args {
            visit(
                argument,
                storage,
                txid,
                arena,
                params,
                row,
                hooks,
                &mut nested,
                &mut nested_count,
                &mut nested_max,
                &mut nested_any,
            )?;
        }
        if !nested_any {
            return materialize_direct(expression, storage, txid, arena, params, row, hooks);
        }

        const EMPTY_VALUES: &[Datum<'_>] = &[];
        let chunks = arena
            .alloc_slice_with(nested_max, |_| EMPTY_VALUES)
            .map_err(|_| arena_full())?;
        let mut total = 0usize;
        for (offset, chunk) in chunks.iter_mut().enumerate() {
            let mut selected = nested;
            for value in &mut selected[..nested_count] {
                value.fixed_index = Some(offset + 1);
            }
            let selected_hooks = EvalHooks {
                project_sets: Some(&selected[..nested_count]),
                ..*hooks
            };
            *chunk = materialize_direct(
                expression,
                storage,
                txid,
                arena,
                params,
                row,
                &selected_hooks,
            )?;
            total = total.checked_add(chunk.len()).ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "nested set-returning function result is too large"
                )
            })?;
        }
        let values = arena
            .alloc_slice_with(total, |_| Datum::Null)
            .map_err(|_| arena_full())?;
        let mut at = 0usize;
        for chunk in chunks {
            values[at..at + chunk.len()].copy_from_slice(chunk);
            at += chunk.len();
        }
        Ok(values)
    }

    #[allow(clippy::too_many_arguments)]
    fn materialize_direct<'a, R: ColumnLookup<'a>>(
        expression: &'a Expr<'a>,
        storage: &'a Storage,
        txid: u32,
        arena: &'a Arena,
        params: &[Datum<'a>],
        row: &R,
        hooks: &EvalHooks<'_, 'a>,
    ) -> Result<&'a [Datum<'a>], SqlError> {
        let Expr::Call {
            name,
            args,
            variadic,
            ..
        } = expression
        else {
            unreachable!("project-set materialization requires a call")
        };
        if is_event_trigger_introspection(name) {
            let rows = event_trigger_rows(name, args, storage, txid, arena)?;
            let names = event_trigger_column_names(name);
            let types = event_trigger_column_types(name);
            let values = arena
                .alloc_slice_with(rows.len(), |_| Datum::Null)
                .map_err(|_| arena_full())?;
            for (value, encoded) in values.iter_mut().zip(rows) {
                let fields = arena
                    .alloc_slice_with(names.len(), |index| crate::sql::types::RecordField {
                        name: names[index],
                        type_oid: types[index].oid(),
                        value: Datum::Null,
                    })
                    .map_err(|_| arena_full())?;
                for (index, field) in fields.iter_mut().enumerate() {
                    field.value =
                        crate::sql::exec::decode_projected_col_record(encoded, index, arena)?;
                }
                *value = Datum::Record(fields);
            }
            return Ok(values);
        }
        if name.eq_ignore_ascii_case("pg_get_publication_tables") {
            let evaluated =
                evaluate_publication_arguments(args, *variadic, arena, params, row, hooks)?;
            let rows = publication_table_rows(storage, txid, arena, evaluated)?;
            let values = arena
                .alloc_slice_with(rows.len(), |_| Datum::Null)
                .map_err(|_| arena_full())?;
            let fields = [
                ("pubid", ColType::Oid),
                ("relid", ColType::Oid),
                ("attrs", ColType::Int2Vector),
                ("qual", ColType::PgNodeTree),
            ];
            for (value, encoded) in values.iter_mut().zip(rows) {
                let decoded = arena
                    .alloc_slice_with(fields.len(), |index| crate::sql::types::RecordField {
                        name: fields[index].0,
                        type_oid: fields[index].1.oid(),
                        value: Datum::Null,
                    })
                    .map_err(|_| arena_full())?;
                for (index, field) in decoded.iter_mut().enumerate() {
                    field.value =
                        crate::sql::exec::decode_projected_col_record(encoded, index, arena)?;
                }
                *value = Datum::Record(decoded);
            }
            return Ok(values);
        }
        if name.eq_ignore_ascii_case("jsonb_path_query")
            || name.eq_ignore_ascii_case("jsonb_path_query_tz")
        {
            if !(2..=4).contains(&args.len()) {
                return Err(srf_signature_error(name));
            }
            let target = eval_full(args[0], arena, params, row, hooks)?;
            let path = eval_full(args[1], arena, params, row, hooks)?;
            let variables = if args.len() >= 3 {
                Some(eval_full(args[2], arena, params, row, hooks)?)
            } else {
                None
            };
            let silent = if args.len() == 4 {
                Some(eval_full(args[3], arena, params, row, hooks)?)
            } else {
                None
            };
            let Some(path_values) = crate::sql::eval::funcs::json::path_query_values(
                target,
                path,
                variables,
                silent,
                name.ends_with("_tz"),
                arena,
            )?
            else {
                return Ok(&[]);
            };
            let values = arena
                .alloc_slice_with(path_values.len(), |_| Datum::Null)
                .map_err(|_| arena_full())?;
            for (value, path_value) in values.iter_mut().zip(path_values) {
                *value = Datum::Json {
                    text: crate::sql::eval::json_to_text_pub(path_value, arena)?,
                    jsonb: true,
                };
            }
            return Ok(values);
        }
        if name.eq_ignore_ascii_case("json_populate_recordset")
            || name.eq_ignore_ascii_case("jsonb_populate_recordset")
        {
            let valid_arity = if name.eq_ignore_ascii_case("json_populate_recordset") {
                matches!(args.len(), 2 | 3)
            } else {
                args.len() == 2
            };
            if !valid_arity {
                return Err(srf_signature_error(name));
            }
            let base = eval_full(args[0], arena, params, row, hooks)?;
            let source = eval_full(args[1], arena, params, row, hooks)?;
            if let Some(option) = args.get(2) {
                crate::sql::eval::cast_to(
                    eval_full(option, arena, params, row, hooks)?,
                    ColType::Bool,
                    arena,
                )?;
            }
            let source = match source {
                Datum::Json { text, .. } | Datum::Text(text) => text,
                Datum::Null => return Ok(&[]),
                other => {
                    return Err(crate::sql::eval::type_mismatch_pub(
                        "populate_recordset requires JSON",
                        &other,
                    ));
                }
            };
            let crate::sql::json::Json::Array(items) = crate::sql::json::parse(source, arena)?
            else {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "cannot call populate_recordset on a non-array"
                ));
            };
            let values = arena
                .alloc_slice_with(items.len(), |_| Datum::Null)
                .map_err(|_| arena_full())?;
            for (value, item) in values.iter_mut().zip(items) {
                *value = crate::sql::eval::funcs::json::populate_record_from_json(
                    args[0], base, *item, arena, row, hooks,
                )?;
            }
            return Ok(values);
        }
        if is_srf_name(name) {
            let count = srf_count(expression, arena, params, row, hooks)?;
            let values = arena
                .alloc_slice_with(count, |_| Datum::Null)
                .map_err(|_| arena_full())?;
            for (offset, value) in values.iter_mut().enumerate() {
                let indexed_hooks = EvalHooks {
                    srf_index: Some(offset + 1),
                    ..*hooks
                };
                *value = eval_full(expression, arena, params, row, &indexed_hooks)?;
            }
            return Ok(values);
        }

        let (schema, table) = name
            .split_once('.')
            .map_or((None, *name), |(schema, table)| (Some(schema), table));
        let source = arena
            .alloc(TableRef {
                schema,
                table,
                alias: None,
                subquery: None,
                func_args: Some(args),
                func_argument_names: &[],
                func_variadic: false,
                rows_from: None,
                col_alias: None,
                inheritance: crate::sql::ast::RelationInheritance::Descendants,
                sample: None,
                cte: None,
                with_ordinality: false,
                lateral: false,
                authorization_role: None,
                bound_table: None,
                view_access: None,
            })
            .map_err(|_| arena_full())?;
        let Some((_, routine)) =
            table_func_routine(source, storage, txid, arena, params, row, Some(hooks))?
        else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {}(...) does not exist",
                name
            ));
        };
        let rows = table_func_rows_outer(
            source,
            storage,
            txid,
            arena,
            params,
            row,
            Some(hooks),
            None,
            None,
        )?;
        let values = arena
            .alloc_slice_with(rows.len(), |_| Datum::Null)
            .map_err(|_| arena_full())?;
        if let Some(columns) = routine.table_columns() {
            for (value, encoded) in values.iter_mut().zip(rows) {
                let mut decoded = [crate::sql::types::RecordField {
                    name: "",
                    type_oid: 0,
                    value: Datum::Null,
                }; crate::storage::MAX_ROUTINE_ARGUMENTS];
                for (index, column) in columns.iter().enumerate() {
                    decoded[index] = crate::sql::types::RecordField {
                        name: column.name.as_str(),
                        type_oid: storage
                            .routine_type_oid(column.ctype, column.user_type, txid)
                            .ok_or_else(|| {
                                sql_err!(
                                    sqlstate::INTERNAL_ERROR,
                                    "routine result type is absent from the catalog"
                                )
                            })?,
                        value: crate::sql::exec::decode_projected_col_record(
                            encoded, index, arena,
                        )?,
                    };
                }
                let fields = arena
                    .alloc_slice_copy(&decoded[..columns.len()])
                    .map_err(|_| arena_full())?;
                *value = Datum::Record(fields);
            }
        } else {
            for (value, encoded) in values.iter_mut().zip(rows) {
                *value = crate::sql::exec::decode_projected_col_record(encoded, 0, arena)?;
            }
        }
        Ok(values)
    }

    for item in items {
        let expression = match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => expression,
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => continue,
        };
        visit(
            expression,
            storage,
            txid,
            arena,
            params,
            row,
            hooks,
            &mut materialized,
            &mut materialized_count,
            &mut max,
            &mut any,
        )?;
    }
    let values = arena
        .alloc_slice_copy(&materialized[..materialized_count])
        .map_err(|_| arena_full())?;
    Ok(ProjectSet {
        count: if any { max } else { 1 },
        values,
        any,
    })
}

/// Number of output rows a set-returning call yields for the current source row.
pub(super) fn srf_count<'a, R: ColumnLookup<'a>>(
    call: &'a Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &R,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<usize, SqlError> {
    let Expr::Call { name, args, .. } = call else {
        return Ok(1);
    };
    if is_event_trigger_introspection(name) {
        require_no_arguments(name, args)?;
        return if name.eq_ignore_ascii_case("pg_event_trigger_ddl_commands") {
            crate::sql::event_trigger::with_ddl_commands(|rows| rows.len())
        } else {
            crate::sql::event_trigger::with_dropped_objects(|rows| rows.len())
        };
    }
    let as_i64 = |d: &Datum| -> Option<i64> {
        match d {
            Datum::Int4(v) => Some(*v as i64),
            Datum::Int8(v) => Some(*v),
            _ => None,
        }
    };
    if name.eq_ignore_ascii_case("generate_series") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "generate_series(...) argument count"
            ));
        }
        let start = crate::sql::eval::text_view(eval_full(args[0], arena, params, row, hooks)?);
        let stop = crate::sql::eval::text_view(eval_full(args[1], arena, params, row, hooks)?);
        let step = if args.len() == 3 {
            crate::sql::eval::text_view(eval_full(args[2], arena, params, row, hooks)?)
        } else {
            Datum::Int4(1)
        };
        if start.is_null() || stop.is_null() || step.is_null() {
            return Ok(0);
        }
        if let (Some(s), Some(e), Some(st)) = (as_i64(&start), as_i64(&stop), as_i64(&step)) {
            if st == 0 {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "step size cannot equal zero"
                ));
            }
            let n = if st > 0 {
                if e < s { 0 } else { (e - s) / st + 1 }
            } else if e > s {
                0
            } else {
                (s - e) / (-st) + 1
            };
            return Ok(n as usize);
        }
        if let Some((base, kind)) = crate::sql::eval::timestamp_series_start(&start) {
            if args.len() != 3 {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "generate_series over timestamps requires a step"
                ));
            }
            // Temporal series: date/timestamp[tz] bounds with an interval
            // step. Coerce bare strings according to the chosen overload.
            let stop = crate::sql::eval::cast_to(stop, kind.coltype(), arena)?;
            let step = crate::sql::eval::cast_to(step, ColType::Interval, arena)?;
            let (Some((stop_micros, _)), Datum::Interval(step_iv)) =
                (crate::sql::eval::timestamp_series_start(&stop), step)
            else {
                return Ok(0);
            };
            crate::sql::eval::timestamp_series_count(base, stop_micros, step_iv)
        } else {
            let start = crate::sql::eval::cast_to(start, ColType::Numeric, arena)?;
            let stop = crate::sql::eval::cast_to(stop, ColType::Numeric, arena)?;
            let step = crate::sql::eval::cast_to(step, ColType::Numeric, arena)?;
            let (Datum::Numeric(start), Datum::Numeric(stop), Datum::Numeric(step)) =
                (start, stop, step)
            else {
                return Ok(0);
            };
            crate::sql::eval::numeric_series_count(start, stop, step, arena)
        }
    } else if name.eq_ignore_ascii_case("pg_options_to_table") {
        if args.len() != 1 {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "pg_options_to_table(...) argument count"
            ));
        }
        match eval_full(args[0], arena, params, row, hooks)? {
            Datum::Array {
                element: crate::sql::types::ArrElem::Text,
                raw,
            } => Ok(crate::sql::array::len(raw)),
            Datum::Null => Ok(0),
            _ => Err(srf_signature_error(name)),
        }
    } else if name.eq_ignore_ascii_case("pg_get_sequence_data") {
        if args.len() != 1 {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "pg_get_sequence_data(...) argument count"
            ));
        }
        let oid = match eval_full(args[0], arena, params, row, hooks)? {
            Datum::Int4(oid) => oid,
            Datum::Null => return Ok(0),
            _ => return Err(srf_signature_error(name)),
        };
        Ok(usize::from(
            hooks
                .catalog
                .and_then(|catalog| catalog.sequence_state_by_oid(oid))
                .is_some(),
        ))
    } else if name.eq_ignore_ascii_case("regexp_matches") {
        // Number of matches: 0/1 without the `g` flag, else all non-overlapping.
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "regexp_matches(...) argument count"
            ));
        }
        let string = crate::sql::eval::text_view(eval_full(args[0], arena, params, row, hooks)?);
        let pattern = crate::sql::eval::text_view(eval_full(args[1], arena, params, row, hooks)?);
        let (string, pattern) = match (string, pattern) {
            (Datum::Text(string), Datum::Text(pattern)) => (string, pattern),
            (Datum::Null, _) | (_, Datum::Null) => return Ok(0),
            _ => return Err(srf_signature_error(name)),
        };
        let flags = if args.len() == 3 {
            match crate::sql::eval::text_view(eval_full(args[2], arena, params, row, hooks)?) {
                Datum::Text(f) => f,
                Datum::Null => return Ok(0),
                _ => return Err(srf_signature_error(name)),
            }
        } else {
            ""
        };
        let (global, ci) = crate::sql::eval::regexp_flags(flags)?;
        let mut spans = [(-1i64, -1i64); crate::sql::regex::MAX_GROUPS];
        let mut from = 0usize;
        let mut n = 0usize;
        while let Some(((mstart, mend), _)) =
            crate::sql::regex::find_captures(pattern, string, from, ci, &mut spans)?
        {
            n += 1;
            if !global {
                break;
            }
            from = if mend > mstart { mend } else { mend + 1 };
            if from > string.len() {
                break;
            }
        }
        Ok(n)
    } else if name.eq_ignore_ascii_case("jsonb_path_query")
        || name.eq_ignore_ascii_case("jsonb_path_query_tz")
    {
        if !(2..=4).contains(&args.len()) {
            return Err(srf_signature_error(name));
        }
        let target = eval_full(args[0], arena, params, row, hooks)?;
        let path = eval_full(args[1], arena, params, row, hooks)?;
        let variables = if args.len() >= 3 {
            Some(eval_full(args[2], arena, params, row, hooks)?)
        } else {
            None
        };
        let silent = if args.len() == 4 {
            Some(eval_full(args[3], arena, params, row, hooks)?)
        } else {
            None
        };
        Ok(crate::sql::eval::funcs::json::path_query_values(
            target,
            path,
            variables,
            silent,
            name.ends_with("_tz"),
            arena,
        )?
        .map_or(0, <[_]>::len))
    } else if name.eq_ignore_ascii_case("json_populate_recordset")
        || name.eq_ignore_ascii_case("jsonb_populate_recordset")
    {
        let valid_arity = if name.eq_ignore_ascii_case("json_populate_recordset") {
            matches!(args.len(), 2 | 3)
        } else {
            args.len() == 2
        };
        if !valid_arity {
            return Err(srf_signature_error(name));
        }
        if let Some(option) = args.get(2) {
            crate::sql::eval::cast_to(
                eval_full(option, arena, params, row, hooks)?,
                ColType::Bool,
                arena,
            )?;
        }
        let source = match eval_full(args[1], arena, params, row, hooks)? {
            Datum::Json { text, .. } | Datum::Text(text) => text,
            Datum::Null => return Ok(0),
            other => {
                return Err(crate::sql::eval::type_mismatch_pub(
                    "populate_recordset requires JSON",
                    &other,
                ));
            }
        };
        match crate::sql::json::parse(source, arena)? {
            crate::sql::json::Json::Array(items) => Ok(items.len()),
            _ => Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "cannot call populate_recordset on a non-array"
            )),
        }
    } else if name.eq_ignore_ascii_case("jsonb_object_keys")
        || name.eq_ignore_ascii_case("json_object_keys")
    {
        if args.len() != 1 {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "{}(...) argument count",
                name
            ));
        }
        let text = match crate::sql::eval::text_view(eval_full(args[0], arena, params, row, hooks)?)
        {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(0),
            _ => {
                return Err(crate::sql::json::object_keys_error(
                    name,
                    crate::sql::json::Kind::Scalar,
                ));
            }
        };
        let kind = crate::sql::json::kind_of(text);
        if kind != crate::sql::json::Kind::Object {
            return Err(crate::sql::json::object_keys_error(name, kind));
        }
        if name.eq_ignore_ascii_case("jsonb_object_keys") {
            return match crate::sql::json::parse(text, arena)? {
                crate::sql::json::Json::Object(members) => Ok(members.len()),
                _ => Err(crate::sql::json::object_keys_error(name, kind)),
            };
        }
        Ok(crate::sql::json::object_members_source(text, arena)?.len())
    } else if name.eq_ignore_ascii_case("jsonb_array_elements")
        || name.eq_ignore_ascii_case("json_array_elements")
        || name.eq_ignore_ascii_case("jsonb_array_elements_text")
        || name.eq_ignore_ascii_case("json_array_elements_text")
    {
        if args.len() != 1 {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "{}(...) argument count",
                name
            ));
        }
        let jsonb = name.eq_ignore_ascii_case("jsonb_array_elements")
            || name.eq_ignore_ascii_case("jsonb_array_elements_text");
        let text = match crate::sql::eval::text_view(eval_full(args[0], arena, params, row, hooks)?)
        {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(0),
            _ => {
                return Err(crate::sql::json::array_elements_error(
                    name,
                    jsonb,
                    crate::sql::json::Kind::Scalar,
                ));
            }
        };
        let kind = crate::sql::json::kind_of(text);
        if kind != crate::sql::json::Kind::Array {
            return Err(crate::sql::json::array_elements_error(name, jsonb, kind));
        }
        if jsonb {
            return match crate::sql::json::parse(text, arena)? {
                crate::sql::json::Json::Array(items) => Ok(items.len()),
                _ => Err(crate::sql::json::array_elements_error(name, jsonb, kind)),
            };
        }
        Ok(crate::sql::json::array_elements_source(text, arena)?.len())
    } else if is_json_each_name(name) {
        if args.len() != 1 {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "{}(...) argument count",
                name
            ));
        }
        let jsonb =
            name.eq_ignore_ascii_case("jsonb_each") || name.eq_ignore_ascii_case("jsonb_each_text");
        let as_text = name.eq_ignore_ascii_case("json_each_text")
            || name.eq_ignore_ascii_case("jsonb_each_text");
        let text = match crate::sql::eval::text_view(eval_full(args[0], arena, params, row, hooks)?)
        {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(0),
            _ => {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "cannot deconstruct a scalar"
                ));
            }
        };
        Ok(crate::sql::eval::json_each_pairs(text, jsonb, as_text, arena)?.len())
    } else if name.eq_ignore_ascii_case("regexp_split_to_table") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "regexp_split_to_table(...) argument count"
            ));
        }
        let (src, pat) = match (
            crate::sql::eval::text_view(eval_full(args[0], arena, params, row, hooks)?),
            crate::sql::eval::text_view(eval_full(args[1], arena, params, row, hooks)?),
        ) {
            (Datum::Text(s), Datum::Text(p)) => (s, p),
            (Datum::Null, _) | (_, Datum::Null) => return Ok(0),
            _ => return Err(srf_signature_error(name)),
        };
        let ci = if args.len() == 3 {
            match crate::sql::eval::text_view(eval_full(args[2], arena, params, row, hooks)?) {
                Datum::Text(f) => crate::sql::eval::regexp_flags(f)?.1,
                Datum::Null => return Ok(0),
                _ => return Err(srf_signature_error(name)),
            }
        } else {
            false
        };
        Ok(crate::sql::eval::regex_split_pub(src, pat, ci, arena)?.len())
    } else if name.eq_ignore_ascii_case("string_to_table") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "string_to_table(...) argument count"
            ));
        }
        let (src, delimiter) = match (
            crate::sql::eval::text_view(eval_full(args[0], arena, params, row, hooks)?),
            crate::sql::eval::text_view(eval_full(args[1], arena, params, row, hooks)?),
        ) {
            (Datum::Text(s), Datum::Text(d)) => (s, Some(d)),
            // A NULL delimiter splits into characters; a NULL input yields nothing.
            (Datum::Text(s), Datum::Null) => (s, None),
            (Datum::Null, _) => return Ok(0),
            _ => return Err(srf_signature_error(name)),
        };
        let mut pieces: [&str; MAX_PIECES] = [""; MAX_PIECES];
        Ok(crate::sql::eval::split_pieces(src, delimiter, &mut pieces)?)
    } else if name.eq_ignore_ascii_case("generate_subscripts") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "generate_subscripts(...) argument count"
            ));
        }
        let raw = match crate::sql::eval::text_view(eval_full(args[0], arena, params, row, hooks)?)
        {
            Datum::Array { raw, .. } => raw,
            Datum::Null => return Ok(0),
            _ => return Err(srf_signature_error(name)),
        };
        let dim = match crate::sql::eval::text_view(eval_full(args[1], arena, params, row, hooks)?)
        {
            Datum::Int4(v) => v as i64,
            Datum::Int8(v) => v,
            Datum::Null => return Ok(0),
            _ => return Err(srf_signature_error(name)),
        };
        if args.len() == 3 {
            match eval_full(args[2], arena, params, row, hooks)? {
                Datum::Bool(_) => {}
                Datum::Null => return Ok(0),
                _ => return Err(srf_signature_error(name)),
            }
        }
        let dimension = usize::try_from(dim).ok().and_then(|dim| dim.checked_sub(1));
        Ok(dimension
            .and_then(|dimension| crate::sql::array::shape(raw)?.dimension(dimension))
            .unwrap_or(0))
    } else if name.eq_ignore_ascii_case("unnest") || name.eq_ignore_ascii_case("_pg_expandarray") {
        if args.len() != 1 {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "{}(...) argument count",
                name
            ));
        }
        match crate::sql::eval::text_view(eval_full(args[0], arena, params, row, hooks)?) {
            Datum::Array { raw, .. } => Ok(crate::sql::array::len(raw)),
            Datum::Int2Vector(raw) => Ok(raw.len() / 2),
            Datum::OidVector(raw) => Ok(raw.len() / 4),
            Datum::Null => Ok(0),
            _ => Err(srf_signature_error(name)),
        }
    } else {
        unreachable!("accepted set-returning routine has an explicit cardinality branch")
    }
}

/// Synthesizes a `TableDef` for a derived table (`FROM (SELECT ...) exposed`)
/// from the subquery's output column names and inferred types. Schema only —
/// no rows are produced, so it needs neither a txid nor bound parameters.
pub(crate) fn synth_derived_def<'a>(
    storage: &'a Storage,
    sub: &'a Select<'a>,
    exposed: &'a str,
    col_alias: Option<&'a [&'a str]>,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a TableDef, SqlError> {
    synth_derived_def_outer(storage, sub, exposed, col_alias, txid, arena, None)
}

fn preserve_projection_type_identities(
    items: &[SelectItem<'_>],
    scope: Option<&QueryScope<'_>>,
    resolver: &dyn crate::sql::exec::ColTypeResolver,
    descriptors: &[ColDesc<'_>],
    type_oids: &mut [i32],
) -> Result<(), SqlError> {
    for (output, description) in type_oids.iter_mut().zip(descriptors) {
        *output = description.type_oid;
    }
    let mut output = 0usize;
    for item in items {
        match item {
            SelectItem::Wildcard => {
                output += scope.map_or(0, QueryScope::star_columns);
            }
            SelectItem::TableWildcard(name) => {
                output += match scope {
                    Some(scope) => scope.qualified_star_columns(name)?,
                    None => 0,
                };
            }
            SelectItem::RecordStar(base) => {
                output += scope.map_or(1, |scope| record_star_width(base, scope));
            }
            SelectItem::Expr { expression, .. } => {
                let semantic = crate::sql::exec::infer_routine_argument_oid(expression, resolver)?;
                if semantic != crate::sql::types::oid::UNKNOWN
                    && let Some(type_oid) = type_oids.get_mut(output)
                {
                    *type_oid = semantic;
                }
                output += 1;
            }
        }
    }
    Ok(())
}

/// [`synth_derived_def`] with an optional outer scope, for a `LATERAL` body: a
/// FROM-less lateral projection (`SELECT t.a * 2`) types its columns against the
/// tables to the item's left. A lateral body with its own FROM is typed from
/// that FROM (its outer references are correlation in WHERE/ON, not in the
/// projected column types).
pub(crate) fn synth_derived_def_outer<'a>(
    storage: &'a Storage,
    sub: &'a Select<'a>,
    exposed: &'a str,
    col_alias: Option<&'a [&'a str]>,
    txid: u32,
    arena: &'a Arena,
    outer: Option<&QueryScope<'a>>,
) -> Result<&'a TableDef, SqlError> {
    let mut descriptors = [ColDesc::new("", 0, 0); MAX_PROJ];
    let mut type_oids = [0_i32; MAX_PROJ];
    let mut output_collations = [crate::sql::ast::Collation::None; MAX_PROJ];
    let n_cols = match sub.set_body {
        Some(tree) => {
            let count = describe_set_body(storage, tree, txid, &mut descriptors, arena)?;
            for (output, description) in type_oids[..count].iter_mut().zip(&descriptors[..count]) {
                *output = description.type_oid;
            }
            count
        }
        None => match &sub.from {
            Some(f) => {
                let ss = QueryScope::resolve_schema(storage, f, txid, arena)?;
                let n = describe_scope_items(
                    sub.items,
                    &ss,
                    outer,
                    storage,
                    txid,
                    arena,
                    &mut descriptors,
                )?;
                // A bare scalar/array subquery item (possibly correlated) has
                // no static type from the scope and describes as text; infer
                // its real type from the inner select's projection so the
                // derived-table column is typed correctly.
                let mut slot = 0usize;
                for item in sub.items {
                    match item {
                        SelectItem::Wildcard => slot += ss.star_columns(),
                        SelectItem::TableWildcard(q) => {
                            slot += ss.qualified_star_columns(q)?;
                        }
                        SelectItem::RecordStar(base) => slot += record_star_width(base, &ss),
                        SelectItem::Expr { expression, .. } => {
                            // A record-valued item registers its field shape;
                            // the column's type_mod carries the handle so
                            // later field access is typed statically.
                            if slot < n
                                && descriptors[slot].type_oid == crate::sql::types::oid::RECORD
                                && let Some(handle) = crate::sql::exec::register_shape_for(
                                    expression,
                                    &super::CatalogScopeCols {
                                        scope: &ss,
                                        outer_scope: outer,
                                        storage,
                                        txid,
                                    },
                                )
                            {
                                descriptors[slot].type_mod = handle;
                            }
                            if slot < n
                                && descriptors[slot].type_oid == crate::sql::types::oid::TEXT
                                && let Expr::Subquery(inner_sub) = &**expression
                                && let Some(SelectItem::Expr {
                                    expression: inner, ..
                                }) = inner_sub.items.first()
                            {
                                let inner_scope = inner_sub.from.as_ref().and_then(|inf| {
                                    QueryScope::resolve_schema(storage, inf, txid, arena).ok()
                                });
                                let witness = subquery_witness(
                                    storage,
                                    txid,
                                    inner,
                                    inner_scope.as_ref().or(Some(&ss)),
                                )?;
                                if !witness.is_null() {
                                    descriptors[slot] = ColDesc::new(
                                        descriptors[slot].name,
                                        witness.type_oid(),
                                        -1,
                                    );
                                }
                            }
                            slot += 1;
                        }
                    }
                }
                let mut slot = 0usize;
                for item in sub.items {
                    match item {
                        SelectItem::Wildcard => {
                            for position in 0..ss.star_columns() {
                                output_collations[slot] =
                                    ss.output_collation(ss.star_entry(position));
                                slot += 1;
                            }
                        }
                        SelectItem::TableWildcard(name) => {
                            for index in 0..ss.qualified_star_columns(name)? {
                                output_collations[slot] =
                                    ss.output_collation(ss.qualified_star_entry(name, index)?);
                                slot += 1;
                            }
                        }
                        SelectItem::RecordStar(base) => {
                            slot += record_star_width(base, &ss);
                        }
                        SelectItem::Expr { expression, .. } => {
                            output_collations[slot] = ss.expression_collation(expression)?;
                            slot += 1;
                        }
                    }
                }
                preserve_projection_type_identities(
                    sub.items,
                    Some(&ss),
                    &super::CatalogScopeCols {
                        scope: &ss,
                        outer_scope: outer,
                        storage,
                        txid,
                    },
                    &descriptors[..n],
                    &mut type_oids[..n],
                )?;
                n
            }
            // A FROM-less lateral body types its projection against the outer
            // scope (`SELECT t.a * 2` sees the enclosing `t`).
            None if outer.is_some() => {
                let outer = outer.expect("checked");
                let n = describe_scope_items(
                    sub.items,
                    outer,
                    None,
                    storage,
                    txid,
                    arena,
                    &mut descriptors,
                )?;
                preserve_projection_type_identities(
                    sub.items,
                    Some(outer),
                    &super::CatalogScopeCols {
                        scope: outer,
                        outer_scope: None,
                        storage,
                        txid,
                    },
                    &descriptors[..n],
                    &mut type_oids[..n],
                )?;
                n
            }
            None => {
                let n = super::describe_catalog_items(
                    sub.items,
                    None,
                    storage,
                    txid,
                    &mut descriptors,
                )?;
                let mut slot = 0usize;
                for item in sub.items {
                    if let SelectItem::Expr { expression, .. } = item {
                        if slot < n
                            && descriptors[slot].type_oid == crate::sql::types::oid::RECORD
                            && let Some(handle) = crate::sql::exec::register_shape_for(
                                expression,
                                &crate::sql::exec::CatalogCols {
                                    definition: None,
                                    alias: None,
                                    output_aliases: &[],
                                    storage,
                                    txid,
                                },
                            )
                        {
                            descriptors[slot].type_mod = handle;
                        }
                        slot += 1;
                    }
                }
                preserve_projection_type_identities(
                    sub.items,
                    None,
                    &crate::sql::exec::CatalogCols {
                        definition: None,
                        alias: None,
                        output_aliases: &[],
                        storage,
                        txid,
                    },
                    &descriptors[..n],
                    &mut type_oids[..n],
                )?;
                n
            }
        },
    };
    for (output, description) in output_collations[..n_cols]
        .iter_mut()
        .zip(&descriptors[..n_cols])
    {
        *output = description.collation;
    }
    if n_cols > MAX_COLUMNS {
        return Err(sql_err!(
            sqlstate::TOO_MANY_COLUMNS,
            "derived table \"{}\" has too many columns",
            exposed
        ));
    }
    // A column-alias list renames the output columns; PostgreSQL requires it to
    // supply no more names than the derived table has columns.
    if let Some(aliases) = col_alias {
        if aliases.len() > n_cols {
            return Err(sql_err!(
                sqlstate::INVALID_COLUMN_REFERENCE,
                "table \"{}\" has {} columns available but {} columns specified",
                exposed,
                n_cols,
                aliases.len()
            ));
        }
        for (i, alias) in aliases.iter().enumerate() {
            descriptors[i].name = alias;
        }
    }
    let blank = ColumnMeta {
        name: SqlName::parse("").expect("empty name is valid"),
        ctype: ColType::Bool,
        type_mod: -1,
        collation: crate::sql::ast::Collation::None,
        storage: crate::sql::ast::ColumnStorage::Plain,
        compression: crate::sql::ast::ColumnCompression::Default,
        not_null: crate::storage::NotNullOrigin::Nullable,
        not_null_inheritable: true,
        unique: false,
        primary: false,
        auto_increment: false,
        default: crate::storage::ColumnDefault::NONE,
        is_identity: false,
        identity_always: false,
        auto_increment_step: 1,
        user_type: None,
        statistics_target: -1,
        statistics_options: crate::sql::ast::ColumnStatisticsOptions::DEFAULT,
    };
    let mut columns = [blank; MAX_COLUMNS];
    for i in 0..n_cols {
        let (ctype, user_type) = crate::sql::exec::catalog_column_type(storage, txid, type_oids[i])
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "derived table column \"{}\" type (oid {}) is not supported",
                    descriptors[i].name,
                    type_oids[i]
                )
            })?;
        columns[i] = ColumnMeta {
            name: SqlName::parse(descriptors[i].name)?,
            ctype,
            type_mod: descriptors[i].type_mod,
            collation: if ctype.is_collatable() {
                match output_collations[i] {
                    crate::sql::ast::Collation::None => crate::sql::ast::Collation::Default,
                    collation => collation,
                }
            } else {
                crate::sql::ast::Collation::None
            },
            user_type,
            ..blank
        };
    }
    let def = TableDef {
        name: SqlName::parse(exposed)?,
        columns,
        n_columns: n_cols,
        ..TableDef::empty()
    };
    Ok(&*arena.alloc(def).map_err(|_| arena_full())?)
}

/// Synthesizes the single-column `TableDef` for a supported table function
/// (`FROM func(args) alias`). The output column is named after the alias (or the
/// function name), so a bare reference to the alias resolves to the value.
pub(super) fn table_func_def<'a>(
    tref: &'a TableRef<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<&'a TableDef, SqlError> {
    table_func_def_outer(
        tref,
        storage,
        txid,
        arena,
        params,
        &crate::sql::eval::NoColumns,
    )
}

fn table_function_column(
    name: SqlName,
    ctype: ColType,
    user_type: Option<crate::storage::UserTypeName>,
    type_mod: i32,
    collation: crate::sql::ast::Collation,
) -> ColumnMeta {
    ColumnMeta {
        name,
        ctype,
        user_type,
        type_mod,
        collation: if ctype.is_collatable() {
            match collation {
                crate::sql::ast::Collation::None => crate::sql::ast::Collation::Default,
                resolved => resolved,
            }
        } else {
            crate::sql::ast::Collation::None
        },
        ..ColumnMeta::EMPTY
    }
}

fn json_table_append_columns(
    specifications: &[&Expr<'_>],
    storage: &Storage,
    txid: u32,
    output: &mut [ColumnMeta; MAX_COLUMNS],
    count: &mut usize,
) -> Result<(), SqlError> {
    for specification in specifications {
        let Expr::Call { name, args, .. } = specification else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid JSON_TABLE column specification"
            ));
        };
        if *name == "__json_table_nested" {
            let Some(Expr::Call {
                name: "__json_table_columns",
                args: children,
                ..
            }) = args.get(2).copied()
            else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "invalid JSON_TABLE nested column specification"
                ));
            };
            json_table_append_columns(children, storage, txid, output, count)?;
            continue;
        }
        let Some(Expr::Str(column_name)) = args.first().copied() else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid JSON_TABLE column name"
            ));
        };
        if output[..*count]
            .iter()
            .any(|column| column.name.as_str() == *column_name)
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_COLUMN,
                "column name \"{}\" specified more than once",
                column_name
            ));
        }
        if *count == output.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "JSON_TABLE output exceeds configured column capacity"
            ));
        }
        let (ctype, user_type, type_mod) = if *name == "__json_table_ordinality" {
            (ColType::Int8, None, -1)
        } else if matches!(*name, "__json_table_value" | "__json_table_exists") {
            let (Some(Expr::Str(type_name)), Some(Expr::Int(type_mod))) =
                (args.get(1).copied(), args.get(2).copied())
            else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "invalid JSON_TABLE column type"
                ));
            };
            let resolved = crate::sql::exec::resolve_routine_type(storage, txid, type_name)?;
            (
                resolved.ctype,
                resolved.user_type,
                i32::try_from(*type_mod).map_err(|_| {
                    sql_err!(sqlstate::INTERNAL_ERROR, "invalid JSON_TABLE type modifier")
                })?,
            )
        } else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid JSON_TABLE column kind"
            ));
        };
        output[*count] = table_function_column(
            SqlName::parse(column_name)?,
            ctype,
            user_type,
            type_mod,
            if ctype.is_collatable() {
                crate::sql::ast::Collation::Default
            } else {
                crate::sql::ast::Collation::None
            },
        );
        *count += 1;
    }
    Ok(())
}

fn json_table_validate_names(args: &[&Expr<'_>]) -> Result<(), SqlError> {
    if !matches!(args.get(1).copied(), Some(Expr::Str(_))) {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "only string constants are supported in JSON_TABLE path specification"
        ));
    }
    let mut names = [SqlName::EMPTY; MAX_COLUMNS * 2];
    let mut count = 0usize;
    let mut add = |name: &str| -> Result<(), SqlError> {
        let parsed = SqlName::parse(name)?;
        if names[..count].contains(&parsed) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "duplicate JSON_TABLE column or path name: {}",
                name
            ));
        }
        if count == names.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "JSON_TABLE has too many column and path names"
            ));
        }
        names[count] = parsed;
        count += 1;
        Ok(())
    };

    fn columns<'a>(
        specifications: &[&'a Expr<'a>],
        add: &mut impl FnMut(&'a str) -> Result<(), SqlError>,
    ) -> Result<(), SqlError> {
        for specification in specifications {
            let Expr::Call { name, args, .. } = specification else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "invalid JSON_TABLE column specification"
                ));
            };
            if *name == "__json_table_nested" {
                if let Some(Expr::Str(path_name)) = args.get(1).copied() {
                    add(path_name)?;
                }
                let Some(Expr::Call {
                    name: "__json_table_columns",
                    args: children,
                    ..
                }) = args.get(2).copied()
                else {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "invalid JSON_TABLE nested column specification"
                    ));
                };
                columns(children, add)?;
            } else if let Some(Expr::Str(column_name)) = args.first().copied() {
                add(column_name)?;
            } else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "invalid JSON_TABLE column name"
                ));
            }
        }
        Ok(())
    }

    if let Some(Expr::Str(root_name)) = args.get(2).copied() {
        add(root_name)?;
    }
    let Some(Expr::Call {
        name: "__json_table_columns",
        args: specifications,
        ..
    }) = args.get(4).copied()
    else {
        return Err(srf_signature_error("json_table"));
    };
    columns(specifications, &mut add)
}

fn populate_record_append_columns<'a, C: ColumnLookup<'a>>(
    args: &[&'a Expr<'a>],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    columns: &C,
    output: &mut [ColumnMeta; MAX_COLUMNS],
) -> Result<usize, SqlError> {
    if args.len() != 2 {
        return Err(srf_signature_error("populate_record"));
    }
    let catalog = super::storage_catalog(storage, arena, txid);
    let hooks = EvalHooks {
        catalog: Some(&catalog),
        ..crate::sql::eval::NO_HOOKS
    };
    let type_oid = match crate::sql::eval::expression_type_identity(args[0], columns, &hooks)? {
        crate::sql::eval::ExpressionTypeIdentity::Known(oid) => oid,
        crate::sql::eval::ExpressionTypeIdentity::Unresolved => {
            crate::sql::eval::eval_full(args[0], arena, params, columns, &hooks)?.type_oid()
        }
    };
    let Some(composite_oid) =
        crate::sql::eval::CatalogAccess::composite_base_type_oid(&catalog, type_oid)
    else {
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "first argument of populate_record must be a row type"
        ));
    };
    let Some(ColType::Composite(slot)) = ColType::from_oid(composite_oid) else {
        unreachable!("catalog composite base OID is a composite")
    };
    let definition = storage.composite_for(slot as usize, txid);
    let mut count = 0usize;
    for field in definition.active_fields() {
        if count == output.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "populate_record output exceeds configured column capacity"
            ));
        }
        output[count] = table_function_column(
            field.name,
            field.ctype,
            field.user_type,
            field.type_mod,
            field.collation,
        );
        count += 1;
    }
    Ok(count)
}

fn json_to_record_append_columns(
    args: &[&Expr<'_>],
    storage: &Storage,
    txid: u32,
    output: &mut [ColumnMeta; MAX_COLUMNS],
) -> Result<usize, SqlError> {
    if args.len() != 2 {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "a column definition list is required for functions returning record"
        ));
    }
    let Expr::Call {
        name: "__json_record_columns",
        args: definitions,
        ..
    } = args[1]
    else {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "a column definition list is required for functions returning record"
        ));
    };
    for (index, definition) in definitions.iter().enumerate() {
        if index == output.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "record definition exceeds configured column capacity"
            ));
        }
        let Expr::Call {
            name: "__json_record_column",
            args: [Expr::Str(name), Expr::Str(type_name), Expr::Int(type_mod)],
            ..
        } = definition
        else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid JSON record column definition"
            ));
        };
        if output[..index]
            .iter()
            .any(|column| column.name.as_str() == *name)
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_COLUMN,
                "column name \"{}\" specified more than once",
                name
            ));
        }
        let resolved = crate::sql::exec::resolve_routine_type(storage, txid, type_name)?;
        output[index] = table_function_column(
            SqlName::parse(name)?,
            resolved.ctype,
            resolved.user_type,
            i32::try_from(*type_mod)
                .map_err(|_| sql_err!(sqlstate::INTERNAL_ERROR, "invalid record type modifier"))?,
            if resolved.ctype.is_collatable() {
                crate::sql::ast::Collation::Default
            } else {
                crate::sql::ast::Collation::None
            },
        );
    }
    Ok(definitions.len())
}

fn table_function_array_element_type(
    storage: &Storage,
    txid: u32,
    element: crate::sql::types::ArrElem,
) -> (ColType, Option<crate::storage::UserTypeName>) {
    use crate::sql::types::ArrElem;
    let user_type = match element {
        ArrElem::Domain { slot, .. } => {
            let domain = storage.domain(slot as usize).definition_for(txid);
            Some(crate::storage::UserTypeName {
                schema: domain.schema,
                name: domain.name,
            })
        }
        ArrElem::Enum(slot) => {
            let definition = storage.enum_for(slot as usize, txid);
            Some(crate::storage::UserTypeName {
                schema: definition.schema,
                name: definition.name,
            })
        }
        ArrElem::Composite(slot) => {
            let definition = storage.composite_for(slot as usize, txid);
            Some(crate::storage::UserTypeName {
                schema: definition.schema,
                name: definition.name,
            })
        }
        _ => None,
    };
    (element.to_coltype(), user_type)
}

/// [`table_func_def`] with column types supplied by an enclosing row or the
/// preceding FROM items. PostgreSQL table-function arguments are implicitly
/// lateral, so their result type must be resolvable before a value exists for
/// every referenced column.
pub(super) fn table_func_def_outer<'a, C: ColumnLookup<'a>>(
    tref: &'a TableRef<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    columns: &C,
) -> Result<&'a TableDef, SqlError> {
    if let Some(functions) = tref.rows_from {
        return rows_from_def_outer(tref, functions, storage, txid, arena, params, columns);
    }
    let is_gs = tref.table.eq_ignore_ascii_case("generate_series");
    let is_unnest = tref.table.eq_ignore_ascii_case("unnest");
    let is_re = tref.table.eq_ignore_ascii_case("regexp_matches");
    let is_keys = tref.table.eq_ignore_ascii_case("jsonb_object_keys")
        || tref.table.eq_ignore_ascii_case("json_object_keys");
    let is_elems = tref.table.eq_ignore_ascii_case("jsonb_array_elements")
        || tref.table.eq_ignore_ascii_case("json_array_elements")
        || tref.table.eq_ignore_ascii_case("jsonb_array_elements_text")
        || tref.table.eq_ignore_ascii_case("json_array_elements_text");
    let is_jsonpath_query = tref.table.eq_ignore_ascii_case("jsonb_path_query")
        || tref.table.eq_ignore_ascii_case("jsonb_path_query_tz");
    let is_json_table = tref.table.eq_ignore_ascii_case("json_table");
    let is_xml_table = tref.table.eq_ignore_ascii_case("xmltable");
    let is_populate_record = matches!(
        tref.table,
        "json_populate_record"
            | "jsonb_populate_record"
            | "json_populate_recordset"
            | "jsonb_populate_recordset"
    );
    let is_json_to_record = matches!(
        tref.table,
        "json_to_record" | "jsonb_to_record" | "json_to_recordset" | "jsonb_to_recordset"
    );
    let is_each = is_json_each_name(tref.table);
    let is_rstt = tref.table.eq_ignore_ascii_case("regexp_split_to_table");
    let is_gsub = tref.table.eq_ignore_ascii_case("generate_subscripts");
    let is_stt = tref.table.eq_ignore_ascii_case("string_to_table");
    let is_options = tref.table.eq_ignore_ascii_case("pg_options_to_table");
    let is_sequence_data = tref.table.eq_ignore_ascii_case("pg_get_sequence_data");
    let is_publication_tables = tref.table.eq_ignore_ascii_case("pg_get_publication_tables");
    let is_ts_parse = tref.table.eq_ignore_ascii_case("ts_parse");
    let is_ts_token_type = tref.table.eq_ignore_ascii_case("ts_token_type");
    let is_ts_debug = tref.table.eq_ignore_ascii_case("ts_debug");
    let is_ts_stat = tref.table.eq_ignore_ascii_case("ts_stat");
    let is_event_introspection = is_event_trigger_introspection(tref.table);
    let is_logical_slot_record = is_logical_slot_record_function(tref.table);
    let built_in = is_gs
        || is_unnest
        || is_re
        || is_keys
        || is_elems
        || is_jsonpath_query
        || is_json_table
        || is_xml_table
        || is_populate_record
        || is_json_to_record
        || is_each
        || is_rstt
        || is_gsub
        || is_stt
        || is_options
        || is_sequence_data
        || is_publication_tables
        || is_ts_parse
        || is_ts_token_type
        || is_ts_debug
        || is_ts_stat
        || is_event_introspection
        || is_logical_slot_record;
    let routine = if built_in {
        None
    } else {
        table_func_routine(tref, storage, txid, arena, params, columns, None)?
    };
    if !built_in && routine.is_none() {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "table function \"{}\" is not supported",
            tref.table
        ));
    }
    let name = tref.alias.unwrap_or(tref.table);
    // Each supported function's output columns: `key`/`value` for the `each`
    // family (two columns), a single column named per the function otherwise.
    // generate_series yields its resolved overload type; regexp_matches yields
    // text[]; unnest yields the array's element type; array_elements' default
    // column is `value`.
    let mut default_cols = [ColumnMeta::EMPTY; MAX_COLUMNS];
    let n_default = if is_json_to_record {
        json_to_record_append_columns(
            tref.func_args.unwrap_or(&[]),
            storage,
            txid,
            &mut default_cols,
        )?
    } else if is_populate_record {
        populate_record_append_columns(
            tref.func_args.unwrap_or(&[]),
            storage,
            txid,
            arena,
            params,
            columns,
            &mut default_cols,
        )?
    } else if is_json_table {
        let args = tref.func_args.unwrap_or(&[]);
        if args.len() != 6 {
            return Err(srf_signature_error(tref.table));
        }
        json_table_validate_names(args)?;
        let Expr::Call {
            name: "__json_table_columns",
            args: specifications,
            ..
        } = args[4]
        else {
            return Err(srf_signature_error(tref.table));
        };
        let mut count = 0usize;
        json_table_append_columns(specifications, storage, txid, &mut default_cols, &mut count)?;
        count
    } else if is_xml_table {
        let args = tref.func_args.unwrap_or(&[]);
        if args.len() != 4 {
            return Err(srf_signature_error(tref.table));
        }
        let Expr::Call {
            name: "__xml_table_columns",
            args: specifications,
            ..
        } = *args[2]
        else {
            return Err(srf_signature_error(tref.table));
        };
        let mut count = 0usize;
        for specification in specifications {
            let Expr::Call { name, args, .. } = specification else {
                return Err(srf_signature_error(tref.table));
            };
            let Some(Expr::Str(column_name)) = args.first().copied() else {
                return Err(srf_signature_error(tref.table));
            };
            if default_cols[..count]
                .iter()
                .any(|column| column.name.as_str() == *column_name)
            {
                return Err(sql_err!(
                    sqlstate::DUPLICATE_COLUMN,
                    "column name \"{}\" specified more than once",
                    column_name
                ));
            }
            if count == default_cols.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "XMLTABLE output exceeds configured column capacity"
                ));
            }
            let (ctype, user_type, type_mod) = if *name == "__xml_table_ordinality" {
                (ColType::Int8, None, -1)
            } else if *name == "__xml_table_column" {
                let (Some(Expr::Str(type_name)), Some(Expr::Int(type_mod))) =
                    (args.get(1).copied(), args.get(2).copied())
                else {
                    return Err(srf_signature_error(tref.table));
                };
                let resolved = crate::sql::exec::resolve_routine_type(storage, txid, type_name)?;
                (resolved.ctype, resolved.user_type, *type_mod as i32)
            } else {
                return Err(srf_signature_error(tref.table));
            };
            default_cols[count] = table_function_column(
                SqlName::parse(column_name)?,
                ctype,
                user_type,
                type_mod,
                if ctype.is_collatable() {
                    crate::sql::ast::Collation::Default
                } else {
                    crate::sql::ast::Collation::None
                },
            );
            count += 1;
        }
        count
    } else if is_logical_slot_record {
        let argument_count = tref.func_args.unwrap_or(&[]).len();
        if crate::sql::logical_replication::result_type(tref.table, argument_count)
            != Some((crate::sql::types::oid::RECORD, -1))
        {
            return Err(srf_signature_error(tref.table));
        }
        default_cols[0] = table_function_column(
            SqlName::parse("slot_name")?,
            ColType::Name,
            None,
            -1,
            crate::sql::ast::Collation::Default,
        );
        default_cols[1] = table_function_column(
            SqlName::parse(
                if tref
                    .table
                    .eq_ignore_ascii_case("pg_replication_slot_advance")
                {
                    "end_lsn"
                } else {
                    "lsn"
                },
            )?,
            ColType::PgLsn,
            None,
            -1,
            crate::sql::ast::Collation::None,
        );
        2
    } else if is_publication_tables {
        if tref.func_args.unwrap_or(&[]).is_empty() {
            return Err(srf_signature_error(tref.table));
        }
        for (index, (name, ctype)) in [
            ("pubid", ColType::Oid),
            ("relid", ColType::Oid),
            ("attrs", ColType::Int2Vector),
            ("qual", ColType::PgNodeTree),
        ]
        .into_iter()
        .enumerate()
        {
            default_cols[index] = table_function_column(
                SqlName::parse(name)?,
                ctype,
                None,
                -1,
                crate::sql::ast::Collation::None,
            );
        }
        4
    } else if is_ts_stat {
        if !(1..=2).contains(&tref.func_args.unwrap_or(&[]).len()) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function ts_stat(...) does not exist"
            ));
        }
        for (index, (name, ctype)) in [
            ("word", ColType::Text),
            ("ndoc", ColType::Int4),
            ("nentry", ColType::Int4),
        ]
        .into_iter()
        .enumerate()
        {
            default_cols[index] = table_function_column(
                SqlName::parse(name)?,
                ctype,
                None,
                -1,
                crate::sql::ast::Collation::None,
            );
        }
        3
    } else if is_ts_debug {
        if !(1..=2).contains(&tref.func_args.unwrap_or(&[]).len()) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function ts_debug(...) does not exist"
            ));
        }
        for (index, (name, ctype)) in [
            ("alias", ColType::Text),
            ("description", ColType::Text),
            ("token", ColType::Text),
            (
                "dictionaries",
                ColType::Array(crate::sql::types::ArrElem::Regdictionary),
            ),
            ("dictionary", ColType::Regdictionary),
            ("lexemes", ColType::Array(crate::sql::types::ArrElem::Text)),
        ]
        .into_iter()
        .enumerate()
        {
            default_cols[index] = table_function_column(
                SqlName::parse(name)?,
                ctype,
                None,
                -1,
                crate::sql::ast::Collation::None,
            );
        }
        6
    } else if is_ts_parse {
        if tref.func_args.unwrap_or(&[]).len() != 2 {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function ts_parse(...) does not exist"
            ));
        }
        default_cols[0] = table_function_column(
            SqlName::parse("tokid")?,
            ColType::Int4,
            None,
            -1,
            crate::sql::ast::Collation::None,
        );
        default_cols[1] = table_function_column(
            SqlName::parse("token")?,
            ColType::Text,
            None,
            -1,
            crate::sql::ast::Collation::Default,
        );
        2
    } else if is_ts_token_type {
        if tref.func_args.unwrap_or(&[]).len() != 1 {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function ts_token_type(...) does not exist"
            ));
        }
        for (index, (name, ctype)) in [
            ("tokid", ColType::Int4),
            ("alias", ColType::Text),
            ("description", ColType::Text),
        ]
        .into_iter()
        .enumerate()
        {
            default_cols[index] = table_function_column(
                SqlName::parse(name)?,
                ctype,
                None,
                -1,
                crate::sql::ast::Collation::None,
            );
        }
        3
    } else if is_event_introspection {
        require_no_arguments(tref.table, tref.func_args.unwrap_or(&[]))?;
        let names = event_trigger_column_names(tref.table);
        let types = event_trigger_column_types(tref.table);
        for index in 0..names.len() {
            default_cols[index] = table_function_column(
                SqlName::parse(names[index])?,
                types[index],
                None,
                -1,
                crate::sql::ast::Collation::None,
            );
        }
        names.len()
    } else if let Some((_, routine)) = routine {
        if let Some(output) = routine.table_columns() {
            for (slot, column) in output.iter().enumerate() {
                default_cols[slot] = table_function_column(
                    column.name,
                    column.ctype,
                    column.user_type,
                    -1,
                    crate::sql::ast::Collation::None,
                );
            }
            output.len()
        } else {
            default_cols[0] = table_function_column(
                SqlName::parse(name)?,
                routine.kind.function_result().expect("set routine result"),
                routine.kind.function_result().and(match routine.kind {
                    crate::storage::RoutineKind::SetFunction { result } => result.user_type,
                    _ => None,
                }),
                -1,
                crate::sql::ast::Collation::None,
            );
            1
        }
    } else if is_sequence_data {
        default_cols[0] = table_function_column(
            SqlName::parse("last_value")?,
            ColType::Int8,
            None,
            -1,
            crate::sql::ast::Collation::None,
        );
        default_cols[1] = table_function_column(
            SqlName::parse("is_called")?,
            ColType::Bool,
            None,
            -1,
            crate::sql::ast::Collation::None,
        );
        2
    } else if is_options {
        default_cols[0] = table_function_column(
            SqlName::parse("option_name")?,
            ColType::Text,
            None,
            -1,
            crate::sql::ast::Collation::Default,
        );
        default_cols[1] = table_function_column(
            SqlName::parse("option_value")?,
            ColType::Text,
            None,
            -1,
            crate::sql::ast::Collation::Default,
        );
        2
    } else if is_each {
        let value_type = if tref.table.eq_ignore_ascii_case("json_each") {
            ColType::Json
        } else if tref.table.eq_ignore_ascii_case("jsonb_each") {
            ColType::Jsonb
        } else {
            ColType::Text // json_each_text / jsonb_each_text
        };
        default_cols[0] = table_function_column(
            SqlName::parse("key")?,
            ColType::Text,
            None,
            -1,
            crate::sql::ast::Collation::Default,
        );
        default_cols[1] = table_function_column(
            SqlName::parse("value")?,
            value_type,
            None,
            -1,
            crate::sql::ast::Collation::Default,
        );
        2
    } else if is_unnest {
        let args = tref.func_args.unwrap_or(&[]);
        if args.is_empty() {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function unnest() does not exist"
            ));
        }
        let vector_unnest = args.len() == 1
            && (crate::sql::eval::static_type_pub(args[0], columns) == Some(ColType::TsVector)
                || matches!(
                    crate::sql::eval::eval(args[0], arena, params, columns),
                    Ok(Datum::TsVector(_))
                ));
        if vector_unnest {
            for (index, (name, ctype)) in [
                ("lexeme", ColType::Text),
                (
                    "positions",
                    ColType::Array(crate::sql::types::ArrElem::Int2),
                ),
                ("weights", ColType::Array(crate::sql::types::ArrElem::Text)),
            ]
            .into_iter()
            .enumerate()
            {
                default_cols[index] = table_function_column(
                    SqlName::parse(name)?,
                    ctype,
                    None,
                    -1,
                    crate::sql::ast::Collation::None,
                );
            }
            3
        } else {
            if args.len() > MAX_COLUMNS {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "UNNEST output exceeds configured column capacity"
                ));
            }
            for (index, argument) in args.iter().enumerate() {
                let element = match crate::sql::eval::static_type_pub(argument, columns) {
                    Some(ColType::Array(element)) => element,
                    Some(_) => crate::sql::types::ArrElem::Text,
                    None => match crate::sql::eval::eval(argument, arena, params, columns)? {
                        Datum::Array { element, .. } => element,
                        _ => crate::sql::types::ArrElem::Text,
                    },
                };
                let (ctype, user_type) = table_function_array_element_type(storage, txid, element);
                let type_mod = match argument {
                    Expr::Cast { type_mod, .. } => *type_mod,
                    _ => -1,
                };
                default_cols[index] = table_function_column(
                    SqlName::parse("unnest")?,
                    ctype,
                    user_type,
                    type_mod,
                    if ctype.is_collatable() {
                        let catalog = super::storage_catalog(storage, arena, txid);
                        crate::sql::eval::described_expression_collation(
                            argument,
                            columns,
                            Some(&catalog),
                        )?
                        .0
                    } else {
                        crate::sql::ast::Collation::None
                    },
                );
            }
            args.len()
        }
    } else {
        let single_type = if is_gs {
            let arguments = tref.func_args.unwrap_or(&[]);
            let start = arguments
                .first()
                .and_then(|argument| crate::sql::eval::static_type_pub(argument, columns));
            let has_numeric = arguments.iter().any(|argument| {
                crate::sql::eval::static_type_pub(argument, columns) == Some(ColType::Numeric)
            });
            let has_int8 = arguments.iter().any(|argument| {
                crate::sql::eval::static_type_pub(argument, columns) == Some(ColType::Int8)
            });
            crate::sql::eval::generate_series_result_type(start, has_numeric, has_int8)
        } else if is_gsub {
            ColType::Int4
        } else if is_re {
            ColType::Array(crate::sql::types::ArrElem::Text)
        } else if is_keys || is_rstt || is_stt {
            ColType::Text
        } else if is_elems {
            if tref.table.eq_ignore_ascii_case("json_array_elements") {
                ColType::Json
            } else if tref.table.eq_ignore_ascii_case("jsonb_array_elements") {
                ColType::Jsonb
            } else {
                ColType::Text
            }
        } else if is_jsonpath_query {
            if !(2..=4).contains(&tref.func_args.unwrap_or(&[]).len()) {
                return Err(srf_signature_error(tref.table));
            }
            ColType::Jsonb
        } else {
            let args = tref.func_args.unwrap_or(&[]);
            match args.first() {
                Some(e) => match crate::sql::eval::static_type_pub(e, columns) {
                    Some(ColType::Array(element)) => element.to_coltype(),
                    Some(_) => ColType::Text,
                    None => match crate::sql::eval::eval(e, arena, params, columns)? {
                        Datum::Array { element, .. } => element.to_coltype(),
                        _ => ColType::Text,
                    },
                },
                None => ColType::Text,
            }
        };
        // A single-column function's default column name is `value` for
        // array_elements, else the (aliased) function name.
        default_cols[0] = table_function_column(
            SqlName::parse(if is_elems { "value" } else { name })?,
            single_type,
            None,
            -1,
            if single_type.is_collatable() {
                crate::sql::ast::Collation::Default
            } else {
                crate::sql::ast::Collation::None
            },
        );
        1
    };
    // `WITH ORDINALITY` appends a `bigint` ordinality column.
    let n_out = if tref.with_ordinality {
        n_default + 1
    } else {
        n_default
    };
    // Column aliases rename the columns positionally; too many is an error.
    if let Some(aliases) = tref.col_alias
        && aliases.len() > n_out
    {
        return Err(sql_err!(
            sqlstate::INVALID_COLUMN_REFERENCE,
            "table \"{}\" has {} columns available but {} columns specified",
            name,
            n_out,
            aliases.len()
        ));
    }
    let mut columns = [ColumnMeta::EMPTY; MAX_COLUMNS];
    for (i, default) in default_cols[..n_default].iter().enumerate() {
        let col_name = tref
            .col_alias
            .and_then(|a| a.get(i).copied())
            .map(SqlName::parse)
            .transpose()?
            .unwrap_or(default.name);
        columns[i] = ColumnMeta {
            name: col_name,
            ..*default
        };
    }
    if tref.with_ordinality {
        let col_name = tref
            .col_alias
            .and_then(|a| a.get(n_default).copied())
            .map(SqlName::parse)
            .transpose()?
            .unwrap_or(SqlName::parse("ordinality")?);
        columns[n_default] = ColumnMeta {
            name: col_name,
            ctype: ColType::Int8,
            ..ColumnMeta::EMPTY
        };
    }
    let def = TableDef {
        name: SqlName::parse(name)?,
        columns,
        n_columns: n_out,
        ..TableDef::empty()
    };
    Ok(&*arena.alloc(def).map_err(|_| arena_full())?)
}

fn rows_from_def_outer<'a, C: ColumnLookup<'a>>(
    tref: &'a TableRef<'a>,
    functions: &'a [TableRef<'a>],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    columns: &C,
) -> Result<&'a TableDef, SqlError> {
    if functions.is_empty() {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "ROWS FROM must contain at least one function"
        ));
    }
    let mut output = [ColumnMeta::EMPTY; MAX_COLUMNS];
    let mut count = 0usize;
    for function in functions {
        let definition = table_func_def_outer(function, storage, txid, arena, params, columns)?;
        if count + definition.n_columns > MAX_COLUMNS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "ROWS FROM output exceeds configured column capacity"
            ));
        }
        output[count..count + definition.n_columns].copy_from_slice(definition.columns());
        count += definition.n_columns;
    }
    if tref.with_ordinality {
        if count == MAX_COLUMNS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "ROWS FROM output exceeds configured column capacity"
            ));
        }
        output[count] = ColumnMeta {
            name: SqlName::parse("ordinality")?,
            ctype: ColType::Int8,
            ..ColumnMeta::EMPTY
        };
        count += 1;
    }
    if let Some(aliases) = tref.col_alias {
        if aliases.len() > count {
            return Err(sql_err!(
                sqlstate::INVALID_COLUMN_REFERENCE,
                "table has {} columns available but {} columns specified",
                count,
                aliases.len()
            ));
        }
        for (column, alias) in output.iter_mut().zip(aliases) {
            column.name = SqlName::parse(alias)?;
        }
    }
    let definition = TableDef {
        name: SqlName::parse(tref.alias.unwrap_or(""))?,
        columns: output,
        n_columns: count,
        ..TableDef::empty()
    };
    Ok(&*arena.alloc(definition).map_err(|_| arena_full())?)
}

/// Whether `name` is one of the two-column `json_each` set-returning functions.
pub(super) fn is_json_each_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("json_each")
        || name.eq_ignore_ascii_case("jsonb_each")
        || name.eq_ignore_ascii_case("json_each_text")
        || name.eq_ignore_ascii_case("jsonb_each_text")
}

fn table_func_routine<'a, C: ColumnLookup<'a>>(
    tref: &'a TableRef<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    _params: &[Datum<'a>],
    columns: &C,
    eval_hooks: Option<&EvalHooks<'_, 'a>>,
) -> Result<Option<(usize, &'a RoutineDef)>, SqlError> {
    use core::fmt::Write as _;

    let args = tref.func_args.expect("table function carries arguments");
    if args.len() > crate::storage::MAX_ROUTINE_ARGUMENTS {
        return Ok(None);
    }
    let catalog = super::storage_catalog(storage, arena, txid);
    let hooks = crate::sql::eval::EvalHooks {
        catalog: Some(&catalog),
        ..eval_hooks.copied().unwrap_or(crate::sql::eval::NO_HOOKS)
    };
    let mut argument_type_oids = [0_i32; crate::storage::MAX_ROUTINE_ARGUMENTS];
    for (slot, argument) in args.iter().enumerate() {
        let statically_known =
            match crate::sql::eval::expression_type_identity(argument, columns, &hooks)? {
                crate::sql::eval::ExpressionTypeIdentity::Known(oid) => Some(oid),
                crate::sql::eval::ExpressionTypeIdentity::Unresolved => None,
            };
        // An unresolved literal or protocol parameter remains UNKNOWN while
        // selecting the routine overload. Evaluating it here both converts a
        // string literal to the implementation Text datum too early and can
        // evaluate volatile table-function arguments twice.
        argument_type_oids[slot] = statically_known.unwrap_or(crate::sql::types::oid::UNKNOWN);
    }
    let mut qualified = StackStr::<128>::new();
    let name = if let Some(schema) = tref.schema {
        write!(qualified, "{schema}.{}", tref.table).map_err(|_| arena_full())?;
        qualified.as_str()
    } else {
        tref.table
    };
    let slot = if tref.func_argument_names.is_empty() {
        storage.routine_slot_for_function_call_syntax_oids(
            name,
            &argument_type_oids[..args.len()],
            tref.func_variadic,
            txid,
        )
    } else {
        storage.routine_slot_for_named_function_call_oids(
            name,
            tref.func_argument_names,
            &argument_type_oids[..args.len()],
            txid,
        )
    };
    let Some(slot) = slot else {
        return Ok(None);
    };
    storage.require_routine_execute(slot, txid)?;
    let routine = storage.routine_for(slot, txid);
    Ok(Some((
        slot,
        arena.alloc(routine).map_err(|_| arena_full())?,
    )))
}

/// Evaluates a table function's arguments against `columns` — an
/// outer row, for a `LATERAL func(outer.col)`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn table_func_rows_outer<'a, C: ColumnLookup<'a>>(
    tref: &'a TableRef<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    columns: &C,
    eval_hooks: Option<&EvalHooks<'_, 'a>>,
    invocations: Option<&super::RoutineInvocationState<'a>>,
    statement_arena: Option<&'a Arena>,
) -> Result<&'a [&'a [u8]], SqlError> {
    let rows = table_func_base_rows_outer(
        tref,
        storage,
        txid,
        arena,
        params,
        columns,
        eval_hooks,
        invocations,
        statement_arena,
    )?;
    if !tref.with_ordinality {
        return Ok(rows);
    }
    let def = table_func_def_outer(tref, storage, txid, arena, params, columns)?;
    let base_columns = def.n_columns - 1;
    const EMPTY: &[u8] = &[];
    let wrapped = arena
        .alloc_slice_with(rows.len(), |_| EMPTY)
        .map_err(|_| arena_full())?;
    for (index, row) in rows.iter().enumerate() {
        let mut values = [Datum::Null; MAX_COLUMNS];
        for (column, slot) in values[..base_columns].iter_mut().enumerate() {
            *slot = crate::sql::exec::decode_projected_col_record(row, column, arena)?;
        }
        values[base_columns] = Datum::Int8((index + 1) as i64);
        wrapped[index] =
            crate::sql::exec::encode_projected_pub(&values[..base_columns + 1], arena)?;
    }
    Ok(&*wrapped)
}

#[allow(clippy::too_many_arguments)]
fn table_func_base_rows_outer<'a, C: ColumnLookup<'a>>(
    tref: &'a TableRef<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    columns: &C,
    eval_hooks: Option<&EvalHooks<'_, 'a>>,
    invocations: Option<&super::RoutineInvocationState<'a>>,
    statement_arena: Option<&'a Arena>,
) -> Result<&'a [&'a [u8]], SqlError> {
    if let Some(functions) = tref.rows_from {
        return rows_from_base_rows_outer(
            functions,
            storage,
            txid,
            arena,
            params,
            columns,
            eval_hooks,
            invocations,
            statement_arena,
        );
    }
    let args = tref.func_args.expect("table function carries arguments");
    if is_event_trigger_introspection(tref.table) {
        return event_trigger_rows(tref.table, args, storage, txid, arena);
    }
    let eval_argument = |argument| match eval_hooks {
        Some(hooks) => crate::sql::eval::eval_full(argument, arena, params, columns, hooks),
        None => crate::sql::eval::eval(argument, arena, params, columns),
    };
    if tref.table.eq_ignore_ascii_case("json_table") {
        let catalog = super::storage_catalog(storage, arena, txid);
        let hooks = EvalHooks {
            catalog: Some(&catalog),
            ..eval_hooks.copied().unwrap_or(crate::sql::eval::NO_HOOKS)
        };
        return json_table_rows(args, arena, params, columns, &hooks);
    }
    if tref.table.eq_ignore_ascii_case("xmltable") {
        let catalog = super::storage_catalog(storage, arena, txid);
        let hooks = EvalHooks {
            catalog: Some(&catalog),
            ..eval_hooks.copied().unwrap_or(crate::sql::eval::NO_HOOKS)
        };
        return xml_table_rows(args, arena, params, columns, &hooks);
    }
    if matches!(
        tref.table,
        "json_populate_record"
            | "jsonb_populate_record"
            | "json_populate_recordset"
            | "jsonb_populate_recordset"
    ) {
        let catalog = super::storage_catalog(storage, arena, txid);
        let hooks = EvalHooks {
            catalog: Some(&catalog),
            ..eval_hooks.copied().unwrap_or(crate::sql::eval::NO_HOOKS)
        };
        return populate_record_rows(tref.table, args, arena, params, columns, &hooks);
    }
    if matches!(
        tref.table,
        "json_to_record" | "jsonb_to_record" | "json_to_recordset" | "jsonb_to_recordset"
    ) {
        let catalog = super::storage_catalog(storage, arena, txid);
        let hooks = EvalHooks {
            catalog: Some(&catalog),
            ..eval_hooks.copied().unwrap_or(crate::sql::eval::NO_HOOKS)
        };
        return json_to_record_rows(
            tref.table, args, storage, txid, arena, params, columns, &hooks,
        );
    }
    if is_logical_slot_record_function(tref.table) {
        let hooks = eval_hooks.copied().unwrap_or(crate::sql::eval::NO_HOOKS);
        let value = crate::sql::logical_replication::dispatch(
            tref.table, args, false, arena, params, columns, &hooks,
        )
        .ok_or_else(|| srf_signature_error(tref.table))??;
        let values = match value {
            Datum::Record(fields) if fields.len() == 2 => [fields[0].value, fields[1].value],
            Datum::Null => [Datum::Null, Datum::Null],
            _ => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "logical slot function returned an invalid record"
                ));
            }
        };
        let encoded = crate::sql::exec::encode_projected_pub(&values, arena)?;
        let rows = arena
            .alloc_slice_with(1, |_| encoded)
            .map_err(|_| arena_full())?;
        return Ok(&*rows);
    }
    if tref.table.eq_ignore_ascii_case("pg_get_publication_tables") {
        let evaluated =
            evaluate_publication_arguments_with(args, tref.func_variadic, arena, eval_argument)?;
        return publication_table_rows(storage, txid, arena, evaluated);
    }
    if tref.table.eq_ignore_ascii_case("ts_stat") {
        if !(1..=2).contains(&args.len()) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function ts_stat(...) does not exist"
            ));
        }
        let source = match eval_argument(args[0])? {
            Datum::Null => return Ok(&[]),
            Datum::Text(source) | Datum::Bpchar(source) => source,
            _ => return Err(srf_signature_error(tref.table)),
        };
        let mut selected_weights = 0b1111u8;
        if args.len() == 2 {
            let weights = match eval_argument(args[1])? {
                Datum::Null => return Ok(&[]),
                Datum::Text(weights) | Datum::Bpchar(weights) => weights,
                _ => return Err(srf_signature_error(tref.table)),
            };
            selected_weights = 0;
            for weight in weights.bytes() {
                selected_weights |= match weight {
                    b'A' => 1 << 3,
                    b'B' => 1 << 2,
                    b'C' => 1 << 1,
                    b'D' => 1,
                    _ => {
                        return Err(sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "unrecognized weight: \"{}\"",
                            weight as char
                        ));
                    }
                };
            }
        }
        let query = crate::sql::parser::parse_query(source, arena)?;
        let routine_query = super::RoutineQuery::Select(query);
        let mut words = [""; crate::sql::full_text::MAX_LEXEMES];
        let mut documents = [0i32; crate::sql::full_text::MAX_LEXEMES];
        let mut entries = [0i32; crate::sql::full_text::MAX_LEXEMES];
        let mut count = 0usize;
        super::execute_routine_query(
            &routine_query,
            storage,
            txid,
            arena,
            &[],
            false,
            &mut |row| {
                if row.len() != 1 {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "ts_stat query must return one tsvector column"
                    ));
                }
                let vector = match row[0] {
                    Datum::Null => return Ok(()),
                    Datum::TsVector(vector) => vector,
                    _ => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "ts_stat query must return one tsvector column"
                        ));
                    }
                };
                let parsed = crate::sql::full_text::parse_vector(vector.as_str(), arena)?;
                for lexeme_index in 0..parsed.lexeme_count() {
                    let (word, positions) = parsed.lexeme(lexeme_index).expect("vector index");
                    let occurrences = if positions.is_empty() {
                        i32::from(selected_weights & 1 != 0)
                    } else {
                        positions
                            .iter()
                            .filter(|position| selected_weights & (1 << position.weight) != 0)
                            .count() as i32
                    };
                    if occurrences == 0 {
                        continue;
                    }
                    let slot = match words[..count]
                        .iter()
                        .position(|candidate| *candidate == word)
                    {
                        Some(slot) => slot,
                        None => {
                            if count == words.len() {
                                return Err(sql_err!(
                                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                    "ts_stat exceeds {} distinct lexemes",
                                    words.len()
                                ));
                            }
                            words[count] = arena.alloc_str(word).map_err(|_| arena_full())?;
                            count += 1;
                            count - 1
                        }
                    };
                    documents[slot] = documents[slot].saturating_add(1);
                    entries[slot] = entries[slot].saturating_add(occurrences);
                }
                Ok(())
            },
        )?;
        for left in 1..count {
            let mut right = left;
            while right > 0 && words[right - 1] < words[right] {
                words.swap(right - 1, right);
                documents.swap(right - 1, right);
                entries.swap(right - 1, right);
                right -= 1;
            }
        }
        const EMPTY: &[u8] = &[];
        let rows = arena
            .alloc_slice_with(count, |_| EMPTY)
            .map_err(|_| arena_full())?;
        for (index, row) in rows.iter_mut().enumerate() {
            *row = crate::sql::exec::encode_projected_pub(
                &[
                    Datum::Text(words[index]),
                    Datum::Int4(documents[index]),
                    Datum::Int4(entries[index]),
                ],
                arena,
            )?;
        }
        return Ok(&*rows);
    }
    if tref.table.eq_ignore_ascii_case("ts_debug") {
        if !(1..=2).contains(&args.len()) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function ts_debug(...) does not exist"
            ));
        }
        let catalog = super::storage_catalog(storage, arena, txid);
        let (configuration, document_index) = if args.len() == 2 {
            (eval_argument(args[0])?, 1)
        } else {
            let setting =
                crate::sql::eval::funcs::system::session_setting("default_text_search_config")
                    .unwrap_or_else(|| StackStr::from_str("pg_catalog.english"));
            (
                Datum::Text(
                    arena
                        .alloc_str(setting.as_str())
                        .map_err(|_| arena_full())?,
                ),
                0,
            )
        };
        let configuration_oid = match configuration {
            Datum::Null => return Ok(&[]),
            Datum::RegObject {
                type_oid,
                referenced_oid,
                ..
            } if type_oid == crate::sql::types::oid::REGCONFIG => referenced_oid,
            Datum::Text(name) | Datum::Bpchar(name) => {
                let (schema, name) = name
                    .rsplit_once('.')
                    .map_or((None, name), |(schema, name)| {
                        (Some(schema.trim_matches('"')), name.trim_matches('"'))
                    });
                crate::sql::eval::CatalogAccess::resolve_text_search_configuration(
                    &catalog, schema, name,
                )
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "text search configuration does not exist"
                    )
                })?
            }
            _ => return Err(srf_signature_error(tref.table)),
        };
        let configuration_slot = storage
            .text_search_slot_by_oid(
                crate::sql::ast::TextSearchObjectKind::Configuration,
                configuration_oid,
                txid,
            )
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "text search configuration does not exist"
                )
            })?;
        let crate::storage::TextSearchDefinition::Configuration { mappings, .. } = storage
            .text_search_object(configuration_slot)
            .definition_for(txid)
        else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid text-search configuration catalog entry"
            ));
        };
        let document = match eval_argument(args[document_index])? {
            Datum::Null => return Ok(&[]),
            Datum::Text(document) | Datum::Bpchar(document) => document,
            _ => return Err(srf_signature_error(tref.table)),
        };
        let (tokens, count) = crate::sql::full_text::parse_document(document, arena)?;
        const EMPTY: &[u8] = &[];
        let rows = arena
            .alloc_slice_with(count, |_| EMPTY)
            .map_err(|_| arena_full())?;
        for (token, row) in tokens[..count].iter().zip(rows.iter_mut()) {
            let token_index = usize::from(token.kind.saturating_sub(1));
            let mapping_count = usize::from(mappings.counts[token_index]);
            let mut dictionaries =
                [Datum::Null; crate::storage::TEXT_SEARCH_DICTIONARIES_PER_TOKEN];
            let mut selected = Datum::Null;
            let mut lexemes = Datum::Null;
            for (index, dictionary_oid) in mappings.dictionaries[token_index][..mapping_count]
                .iter()
                .enumerate()
            {
                let dictionary_name = crate::sql::eval::CatalogAccess::text_search_dictionary_name(
                    &catalog,
                    *dictionary_oid,
                )
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "text-search mapping dictionary is missing"
                    )
                })?;
                let dictionary_name = arena
                    .alloc_str(dictionary_name.as_str())
                    .map_err(|_| arena_full())?;
                let dictionary = Datum::RegObject {
                    type_oid: crate::sql::types::oid::REGDICTIONARY,
                    referenced_oid: *dictionary_oid,
                    name: dictionary_name,
                };
                dictionaries[index] = dictionary;
                match crate::sql::eval::CatalogAccess::lexize_text_search_dictionary(
                    &catalog,
                    *dictionary_oid,
                    token.text,
                    arena,
                )? {
                    crate::sql::full_text::TextSearchLexeme::Unmapped => {}
                    crate::sql::full_text::TextSearchLexeme::StopWord => {
                        selected = dictionary;
                        lexemes = Datum::Array {
                            element: crate::sql::types::ArrElem::Text,
                            raw: crate::sql::array::build(&[], arena)?,
                        };
                        break;
                    }
                    crate::sql::full_text::TextSearchLexeme::Lexeme(value) => {
                        selected = dictionary;
                        lexemes = Datum::Array {
                            element: crate::sql::types::ArrElem::Text,
                            raw: crate::sql::array::build(&[Datum::Text(value)], arena)?,
                        };
                        break;
                    }
                }
            }
            let (alias, description) = crate::sql::full_text::TOKEN_TYPES[token_index];
            let values = [
                Datum::Text(alias),
                Datum::Text(description),
                Datum::Text(token.text),
                Datum::Array {
                    element: crate::sql::types::ArrElem::Regdictionary,
                    raw: crate::sql::array::build(&dictionaries[..mapping_count], arena)?,
                },
                selected,
                lexemes,
            ];
            *row = crate::sql::exec::encode_projected_pub(&values, arena)?;
        }
        return Ok(&*rows);
    }
    if tref.table.eq_ignore_ascii_case("ts_parse")
        || tref.table.eq_ignore_ascii_case("ts_token_type")
    {
        let expected = if tref.table.eq_ignore_ascii_case("ts_parse") {
            2
        } else {
            1
        };
        if args.len() != expected {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {}(...) does not exist",
                tref.table
            ));
        }
        let parser = eval_argument(args[0])?;
        let parser_slot = match parser {
            Datum::Null => return Ok(&[]),
            Datum::Oid(oid) => storage.text_search_slot_by_oid(
                crate::sql::ast::TextSearchObjectKind::Parser,
                i32::try_from(oid).map_err(|_| srf_signature_error(tref.table))?,
                txid,
            ),
            Datum::Int4(oid) => storage.text_search_slot_by_oid(
                crate::sql::ast::TextSearchObjectKind::Parser,
                oid,
                txid,
            ),
            Datum::Text(name) | Datum::Bpchar(name) => {
                let (schema, name) = name
                    .rsplit_once('.')
                    .map_or((None, name), |(schema, name)| {
                        (Some(schema.trim_matches('"')), name.trim_matches('"'))
                    });
                storage.text_search_slot_on_path(
                    crate::sql::ast::TextSearchObjectKind::Parser,
                    schema,
                    name,
                    txid,
                )
            }
            _ => return Err(srf_signature_error(tref.table)),
        }
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "text search parser does not exist"
            )
        })?;
        let _definition = storage.text_search_object(parser_slot).definition_for(txid);
        const EMPTY: &[u8] = &[];
        if tref.table.eq_ignore_ascii_case("ts_token_type") {
            let rows = arena
                .alloc_slice_with(crate::sql::full_text::TOKEN_TYPES.len(), |_| EMPTY)
                .map_err(|_| arena_full())?;
            for (index, row) in rows.iter_mut().enumerate() {
                let (alias, description) = crate::sql::full_text::TOKEN_TYPES[index];
                *row = crate::sql::exec::encode_projected_pub(
                    &[
                        Datum::Int4((index + 1) as i32),
                        Datum::Text(alias),
                        Datum::Text(description),
                    ],
                    arena,
                )?;
            }
            return Ok(&*rows);
        }
        let document = match eval_argument(args[1])? {
            Datum::Null => return Ok(&[]),
            Datum::Text(document) | Datum::Bpchar(document) => document,
            _ => return Err(srf_signature_error(tref.table)),
        };
        let (tokens, count) = crate::sql::full_text::parse_document(document, arena)?;
        let rows = arena
            .alloc_slice_with(count, |_| EMPTY)
            .map_err(|_| arena_full())?;
        for (token, row) in tokens[..count].iter().zip(rows.iter_mut()) {
            *row = crate::sql::exec::encode_projected_pub(
                &[Datum::Int4(i32::from(token.kind)), Datum::Text(token.text)],
                arena,
            )?;
        }
        return Ok(&*rows);
    }
    if tref.table.eq_ignore_ascii_case("pg_get_sequence_data") {
        if args.len() != 1 {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "pg_get_sequence_data(...) argument count"
            ));
        }
        let oid = match eval_argument(args[0])? {
            Datum::Int4(oid) => oid,
            Datum::Null => return Ok(&[]),
            _ => return Err(srf_signature_error(tref.table)),
        };
        let Some((last_value, is_called)) =
            crate::sql::catalog::sequence_state_by_oid(storage, oid)
        else {
            return Ok(&[]);
        };
        let encoded = crate::sql::exec::encode_projected_pub(
            &[Datum::Int8(last_value), Datum::Bool(is_called)],
            arena,
        )?;
        return arena
            .alloc_slice_copy(&[encoded])
            .map(|rows| &*rows)
            .map_err(|_| arena_full());
    }
    // pg_options_to_table(text[]): split each `name=value` option into the
    // two catalog columns used by pg_dump for FDW and per-column options.
    if tref.table.eq_ignore_ascii_case("pg_options_to_table") {
        if args.len() != 1 {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "pg_options_to_table(...) argument count"
            ));
        }
        let raw = match eval_argument(args[0])? {
            Datum::Array {
                element: crate::sql::types::ArrElem::Text,
                raw,
            } => raw,
            Datum::Null => return Ok(&[]),
            _ => return Err(srf_signature_error(tref.table)),
        };
        let count = crate::sql::array::len(raw);
        const EMPTY: &[u8] = &[];
        let rows = arena
            .alloc_slice_with(count, |_| EMPTY)
            .map_err(|_| arena_full())?;
        for (index, slot) in rows.iter_mut().enumerate() {
            let option = match crate::sql::array::get(raw, crate::sql::types::ArrElem::Text, index)
            {
                Some(Datum::Text(option)) => option,
                Some(Datum::Null) => {
                    return Err(sql_err!(
                        sqlstate::NULL_VALUE_NOT_ALLOWED,
                        "null value not allowed"
                    ));
                }
                None => unreachable!("array length fixes every option index"),
                Some(_) => return Err(srf_signature_error(tref.table)),
            };
            let (name, value) = match option.split_once('=') {
                Some((name, value)) => (Datum::Text(name), Datum::Text(value)),
                None => (Datum::Text(option), Datum::Null),
            };
            *slot = crate::sql::exec::encode_projected_pub(&[name, value], arena)?;
        }
        return Ok(&*rows);
    }
    // json_each / jsonb_each[_text]: one (key, value) row per object member.
    if is_json_each_name(tref.table) {
        let jsonb = tref.table.eq_ignore_ascii_case("jsonb_each")
            || tref.table.eq_ignore_ascii_case("jsonb_each_text");
        let as_text = tref.table.eq_ignore_ascii_case("json_each_text")
            || tref.table.eq_ignore_ascii_case("jsonb_each_text");
        let text = match crate::sql::eval::text_view(eval_argument(args[0])?) {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(&[]),
            _ => {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "cannot deconstruct a scalar"
                ));
            }
        };
        let pairs = crate::sql::eval::json_each_pairs(text, jsonb, as_text, arena)?;
        const EMPTY: &[u8] = &[];
        let rows = arena
            .alloc_slice_with(pairs.len(), |_| EMPTY)
            .map_err(|_| arena_full())?;
        for (slot, (key, value)) in rows.iter_mut().zip(pairs.iter()) {
            *slot = crate::sql::exec::encode_projected_pub(&[Datum::Text(key), *value], arena)?;
        }
        return Ok(&*rows);
    }
    // string_to_table(string, delimiter [, null_string]): one text row per
    // piece. A NULL delimiter splits into characters and a piece equal to
    // null_string is NULL, both as `string_to_array` has it — the split rule
    // itself is shared, so the two cannot drift apart.
    if tref.table.eq_ignore_ascii_case("string_to_table") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function string_to_table(...) with {} arguments does not exist",
                args.len()
            ));
        }
        let evaluate = |i: usize| eval_argument(args[i]).map(crate::sql::eval::text_view);
        let source = match evaluate(0)? {
            Datum::Text(s) => s,
            Datum::Null => return Ok(&[]),
            _ => return Err(srf_signature_error(tref.table)),
        };
        let delimiter = match evaluate(1)? {
            Datum::Text(d) => Some(d),
            Datum::Null => None,
            _ => return Err(srf_signature_error(tref.table)),
        };
        let null_string = if args.len() == 3 {
            match evaluate(2)? {
                Datum::Text(t) => Some(t),
                Datum::Null => None,
                _ => return Err(srf_signature_error(tref.table)),
            }
        } else {
            None
        };
        let mut pieces: [&str; MAX_PIECES] = [""; MAX_PIECES];
        let n = crate::sql::eval::split_pieces(source, delimiter, &mut pieces)?;
        const EMPTY: &[u8] = &[];
        let rows = arena
            .alloc_slice_with(n, |_| EMPTY)
            .map_err(|_| arena_full())?;
        for (slot, piece) in rows.iter_mut().zip(pieces[..n].iter()) {
            let value = if null_string == Some(*piece) {
                Datum::Null
            } else {
                Datum::Text(piece)
            };
            *slot = crate::sql::exec::encode_projected_pub(&[value], arena)?;
        }
        return Ok(&*rows);
    }
    // regexp_split_to_table(string, pattern [, flags]): one text row per piece.
    if tref.table.eq_ignore_ascii_case("regexp_split_to_table") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "regexp_split_to_table(...) argument count"
            ));
        }
        let (src, pat) = match (
            crate::sql::eval::text_view(eval_argument(args[0])?),
            crate::sql::eval::text_view(eval_argument(args[1])?),
        ) {
            (Datum::Text(s), Datum::Text(p)) => (s, p),
            (Datum::Null, _) | (_, Datum::Null) => return Ok(&[]),
            _ => return Err(srf_signature_error(tref.table)),
        };
        let case_insensitive = if args.len() == 3 {
            match crate::sql::eval::text_view(eval_argument(args[2])?) {
                Datum::Text(f) => crate::sql::eval::regexp_flags(f)?.1,
                Datum::Null => return Ok(&[]),
                _ => return Err(srf_signature_error(tref.table)),
            }
        } else {
            false
        };
        let pieces = crate::sql::eval::regex_split_pub(src, pat, case_insensitive, arena)?;
        const EMPTY: &[u8] = &[];
        let rows = arena
            .alloc_slice_with(pieces.len(), |_| EMPTY)
            .map_err(|_| arena_full())?;
        for (slot, piece) in rows.iter_mut().zip(pieces.iter()) {
            *slot = crate::sql::exec::encode_projected_pub(&[*piece], arena)?;
        }
        return Ok(&*rows);
    }
    // generate_subscripts(array, dim [, reverse]): declared indices along one
    // dimension, including non-default lower bounds.
    if tref.table.eq_ignore_ascii_case("generate_subscripts") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "generate_subscripts(...) argument count"
            ));
        }
        let raw = match crate::sql::eval::text_view(eval_argument(args[0])?) {
            Datum::Array { raw, .. } => raw,
            Datum::Null => return Ok(&[]),
            _ => return Err(srf_signature_error(tref.table)),
        };
        let dim = match crate::sql::eval::text_view(eval_argument(args[1])?) {
            Datum::Int4(v) => v as i64,
            Datum::Int8(v) => v,
            Datum::Null => return Ok(&[]),
            _ => return Err(srf_signature_error(tref.table)),
        };
        let reverse = if args.len() == 3 {
            match eval_argument(args[2])? {
                Datum::Bool(reverse) => reverse,
                Datum::Null => return Ok(&[]),
                _ => return Err(srf_signature_error(tref.table)),
            }
        } else {
            false
        };
        let dimension = usize::try_from(dim).ok().and_then(|dim| dim.checked_sub(1));
        let shape = crate::sql::array::shape(raw).expect("array datum invariant");
        let count = dimension
            .and_then(|dimension| shape.dimension(dimension))
            .unwrap_or(0);
        let lower = dimension.and_then(|dimension| shape.lower_bound(dimension));
        let upper = dimension.and_then(|dimension| shape.upper_bound(dimension));
        const EMPTY: &[u8] = &[];
        let rows = arena
            .alloc_slice_with(count, |_| EMPTY)
            .map_err(|_| arena_full())?;
        for (i, slot) in rows.iter_mut().enumerate() {
            let offset = i32::try_from(i).expect("array dimension fits i32");
            let subscript = if reverse {
                upper.expect("nonempty dimension") - offset
            } else {
                lower.expect("nonempty dimension") + offset
            };
            *slot = crate::sql::exec::encode_projected_pub(&[Datum::Int4(subscript)], arena)?;
        }
        return Ok(&*rows);
    }
    // regexp_matches(string, pattern [, flags]): one row per match, each a
    // text[] of the capture groups (or the whole match when there are no groups).
    if tref.table.eq_ignore_ascii_case("regexp_matches") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "regexp_matches(...) argument count"
            ));
        }
        let string = crate::sql::eval::text_view(eval_argument(args[0])?);
        let pattern = crate::sql::eval::text_view(eval_argument(args[1])?);
        let (string, pattern) = match (string, pattern) {
            (Datum::Text(string), Datum::Text(pattern)) => (string, pattern),
            (Datum::Null, _) | (_, Datum::Null) => return Ok(&[]),
            _ => return Err(srf_signature_error(tref.table)),
        };
        let flags = if args.len() == 3 {
            match crate::sql::eval::text_view(eval_argument(args[2])?) {
                Datum::Text(f) => f,
                Datum::Null => return Ok(&[]),
                _ => return Err(srf_signature_error(tref.table)),
            }
        } else {
            ""
        };
        let (global, ci) = crate::sql::eval::regexp_flags(flags)?;
        // Collect each match's encoded text[] row.
        const EMPTY: &[u8] = &[];
        let mut rows = [EMPTY; crate::sql::parser::MAX_LIST];
        let mut n = 0usize;
        let mut spans = [(-1i64, -1i64); crate::sql::regex::MAX_GROUPS];
        let mut from = 0usize;
        while let Some(((mstart, mend), ng)) =
            crate::sql::regex::find_captures(pattern, string, from, ci, &mut spans)?
        {
            let mut elems = [Datum::Null; crate::sql::regex::MAX_GROUPS];
            let count = if ng == 0 {
                elems[0] = Datum::Text(&string[mstart..mend]);
                1
            } else {
                for (i, span) in spans[..ng].iter().enumerate() {
                    elems[i] = if span.0 < 0 {
                        Datum::Null
                    } else {
                        Datum::Text(&string[span.0 as usize..span.1 as usize])
                    };
                }
                ng
            };
            let arr = Datum::Array {
                element: crate::sql::types::ArrElem::Text,
                raw: crate::sql::array::build(&elems[..count], arena)?,
            };
            if n == crate::sql::parser::MAX_LIST {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many regexp_matches rows"
                ));
            }
            rows[n] = crate::sql::exec::encode_projected_pub(&[arr], arena)?;
            n += 1;
            if !global {
                break;
            }
            from = if mend > mstart { mend } else { mend + 1 };
            if from > string.len() {
                break;
            }
        }
        let out = arena
            .alloc_slice_with(n, |i| rows[i])
            .map_err(|_| arena_full())?;
        return Ok(&*out);
    }
    // jsonb_object_keys(obj) / json_object_keys(obj): one text row per key.
    if tref.table.eq_ignore_ascii_case("jsonb_object_keys")
        || tref.table.eq_ignore_ascii_case("json_object_keys")
    {
        let jsonb = tref.table.eq_ignore_ascii_case("jsonb_object_keys");
        let text = match crate::sql::eval::text_view(eval_argument(args[0])?) {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(&[]),
            _ => {
                return Err(crate::sql::json::object_keys_error(
                    tref.table,
                    crate::sql::json::Kind::Scalar,
                ));
            }
        };
        let kind = crate::sql::json::kind_of(text);
        if kind != crate::sql::json::Kind::Object {
            return Err(crate::sql::json::object_keys_error(tref.table, kind));
        }
        const EMPTY: &[u8] = &[];
        // jsonb: normalized/sorted keys; json: source order with duplicates.
        if jsonb {
            let crate::sql::json::Json::Object(members) = crate::sql::json::parse(text, arena)?
            else {
                return Err(crate::sql::json::object_keys_error(tref.table, kind));
            };
            let rows = arena
                .alloc_slice_with(members.len(), |_| EMPTY)
                .map_err(|_| arena_full())?;
            for (slot, (key, _)) in rows.iter_mut().zip(members.iter()) {
                *slot = crate::sql::exec::encode_projected_pub(&[Datum::Text(key)], arena)?;
            }
            return Ok(&*rows);
        }
        let members = crate::sql::json::object_members_source(text, arena)?;
        let rows = arena
            .alloc_slice_with(members.len(), |_| EMPTY)
            .map_err(|_| arena_full())?;
        for (slot, (key, _)) in rows.iter_mut().zip(members.iter()) {
            *slot = crate::sql::exec::encode_projected_pub(&[Datum::Text(key)], arena)?;
        }
        return Ok(&*rows);
    }
    if tref.table.eq_ignore_ascii_case("jsonb_path_query")
        || tref.table.eq_ignore_ascii_case("jsonb_path_query_tz")
    {
        if !(2..=4).contains(&args.len()) {
            return Err(srf_signature_error(tref.table));
        }
        let target = eval_argument(args[0])?;
        let path = eval_argument(args[1])?;
        let variables = if args.len() >= 3 {
            Some(eval_argument(args[2])?)
        } else {
            None
        };
        let silent = if args.len() == 4 {
            Some(eval_argument(args[3])?)
        } else {
            None
        };
        let Some(values) = crate::sql::eval::funcs::json::path_query_values(
            target,
            path,
            variables,
            silent,
            tref.table.ends_with("_tz"),
            arena,
        )?
        else {
            return Ok(&[]);
        };
        const EMPTY: &[u8] = &[];
        let rows = arena
            .alloc_slice_with(values.len(), |_| EMPTY)
            .map_err(|_| arena_full())?;
        for (slot, value) in rows.iter_mut().zip(values) {
            let datum = Datum::Json {
                text: crate::sql::eval::json_to_text_pub(value, arena)?,
                jsonb: true,
            };
            *slot = crate::sql::exec::encode_projected_pub(&[datum], arena)?;
        }
        return Ok(&*rows);
    }
    // jsonb_array_elements / json_array_elements[_text]: one row per element.
    if tref.table.eq_ignore_ascii_case("jsonb_array_elements")
        || tref.table.eq_ignore_ascii_case("json_array_elements")
        || tref.table.eq_ignore_ascii_case("jsonb_array_elements_text")
        || tref.table.eq_ignore_ascii_case("json_array_elements_text")
    {
        let jsonb = tref.table.eq_ignore_ascii_case("jsonb_array_elements")
            || tref.table.eq_ignore_ascii_case("jsonb_array_elements_text");
        let as_text = tref.table.eq_ignore_ascii_case("jsonb_array_elements_text")
            || tref.table.eq_ignore_ascii_case("json_array_elements_text");
        let text = match crate::sql::eval::text_view(eval_argument(args[0])?) {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(&[]),
            _ => {
                return Err(crate::sql::json::array_elements_error(
                    tref.table,
                    jsonb,
                    crate::sql::json::Kind::Scalar,
                ));
            }
        };
        let kind = crate::sql::json::kind_of(text);
        if kind != crate::sql::json::Kind::Array {
            return Err(crate::sql::json::array_elements_error(
                tref.table, jsonb, kind,
            ));
        }
        const EMPTY: &[u8] = &[];
        if jsonb {
            let crate::sql::json::Json::Array(items) = crate::sql::json::parse(text, arena)? else {
                return Err(crate::sql::json::array_elements_error(
                    tref.table, jsonb, kind,
                ));
            };
            let rows = arena
                .alloc_slice_with(items.len(), |_| EMPTY)
                .map_err(|_| arena_full())?;
            for (slot, element) in rows.iter_mut().zip(items.iter()) {
                let datum = if as_text {
                    match *element {
                        crate::sql::json::Json::Str(s) => Datum::Text(s),
                        crate::sql::json::Json::Null => Datum::Null,
                        _ => Datum::Text(crate::sql::eval::json_to_text_pub(element, arena)?),
                    }
                } else {
                    Datum::Json {
                        text: crate::sql::eval::json_to_text_pub(element, arena)?,
                        jsonb,
                    }
                };
                *slot = crate::sql::exec::encode_projected_pub(&[datum], arena)?;
            }
            return Ok(&*rows);
        }
        // json: each element's verbatim source text.
        let items = crate::sql::json::array_elements_source(text, arena)?;
        let rows = arena
            .alloc_slice_with(items.len(), |_| EMPTY)
            .map_err(|_| arena_full())?;
        for (slot, element) in rows.iter_mut().zip(items.iter()) {
            let datum = if as_text {
                match crate::sql::json::parse(element, arena)? {
                    crate::sql::json::Json::Str(s) => Datum::Text(s),
                    crate::sql::json::Json::Null => Datum::Null,
                    _ => Datum::Text(element),
                }
            } else {
                Datum::Json {
                    text: element,
                    jsonb,
                }
            };
            *slot = crate::sql::exec::encode_projected_pub(&[datum], arena)?;
        }
        return Ok(&*rows);
    }
    // PostgreSQL's FROM-only multi-argument UNNEST advances all arrays in
    // lockstep and NULL-pads the shorter inputs. A NULL array is empty; it
    // does not suppress rows produced by another argument.
    if tref.table.eq_ignore_ascii_case("unnest") {
        if args.is_empty() {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function unnest() does not exist"
            ));
        }
        if args.len() > MAX_COLUMNS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "UNNEST output exceeds configured column capacity"
            ));
        }
        let mut arrays = [Datum::Null; MAX_COLUMNS];
        for (slot, argument) in arrays.iter_mut().zip(args) {
            *slot = crate::sql::eval::text_view(eval_argument(argument)?);
        }
        if args.len() == 1
            && let Datum::TsVector(vector) = arrays[0]
        {
            let parsed = crate::sql::full_text::parse_vector(vector.as_str(), arena)?;
            const EMPTY: &[u8] = &[];
            let rows = arena
                .alloc_slice_with(parsed.lexeme_count(), |_| EMPTY)
                .map_err(|_| arena_full())?;
            for (index, row) in rows.iter_mut().enumerate() {
                let (lexeme, positions) = parsed.lexeme(index).expect("vector index");
                let mut position_values = [Datum::Null; crate::sql::full_text::MAX_POSITIONS];
                let mut weight_values = [Datum::Null; crate::sql::full_text::MAX_POSITIONS];
                for (position_index, position) in positions.iter().enumerate() {
                    position_values[position_index] = Datum::Int2(position.number as i16);
                    weight_values[position_index] = Datum::Text(match position.weight {
                        3 => "A",
                        2 => "B",
                        1 => "C",
                        _ => "D",
                    });
                }
                let values = [
                    Datum::Text(lexeme),
                    Datum::Array {
                        element: crate::sql::types::ArrElem::Int2,
                        raw: crate::sql::array::build(&position_values[..positions.len()], arena)?,
                    },
                    Datum::Array {
                        element: crate::sql::types::ArrElem::Text,
                        raw: crate::sql::array::build(&weight_values[..positions.len()], arena)?,
                    },
                ];
                *row = crate::sql::exec::encode_projected_pub(&values, arena)?;
            }
            return Ok(&*rows);
        }
        let mut count = 0usize;
        for value in &arrays[..args.len()] {
            match *value {
                Datum::Array { raw, .. } => count = count.max(crate::sql::array::len(raw)),
                Datum::Null => {}
                _ => {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "unnest requires an array argument"
                    ));
                }
            }
        }
        const EMPTY: &[u8] = &[];
        let rows = arena
            .alloc_slice_with(count, |_| EMPTY)
            .map_err(|_| arena_full())?;
        for (row_index, row) in rows.iter_mut().enumerate() {
            let mut values = [Datum::Null; MAX_COLUMNS];
            for (column, array) in arrays[..args.len()].iter().enumerate() {
                values[column] = match *array {
                    Datum::Array { element, raw } => {
                        crate::sql::array::get(raw, element, row_index).unwrap_or(Datum::Null)
                    }
                    Datum::Null => Datum::Null,
                    _ => {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "validated UNNEST input changed type"
                        ));
                    }
                };
            }
            *row = crate::sql::exec::encode_projected_pub(&values[..args.len()], arena)?;
        }
        return Ok(&*rows);
    }
    if !tref.table.eq_ignore_ascii_case("generate_series") {
        let Some((routine_slot, routine)) =
            table_func_routine(tref, storage, txid, arena, params, columns, eval_hooks)?
        else {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "table function \"{}\" is not supported",
                tref.table
            ));
        };
        let table_columns = routine.table_columns();
        let scalar_result = routine.kind.function_result();
        let _formal_scope = crate::sql::exec::enter_routine_parameter_types(routine.arguments());
        let catalog = super::storage_catalog(storage, arena, txid);
        let hooks = crate::sql::eval::EvalHooks {
            catalog: Some(&catalog),
            ..eval_hooks.copied().unwrap_or(crate::sql::eval::NO_HOOKS)
        };
        let mut routine_params = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mapping = routine
            .call_input_mapping(tref.func_argument_names, args.len(), tref.func_variadic)
            .expect("resolved table routine call has a valid argument mapping");
        let mut provided = [false; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut variadic_values = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut variadic_count = 0usize;
        for (call_index, argument) in args.iter().enumerate() {
            let value = crate::sql::eval::eval_full(argument, arena, params, columns, &hooks)?;
            let encoded = crate::sql::exec::encode_projected_pub(&[value], arena)?;
            let value = crate::sql::exec::decode_projected_col_record(encoded, 0, arena)?;
            let input_index = usize::from(mapping[call_index]);
            if !tref.func_variadic
                && matches!(
                    routine
                        .parameter_for_input(input_index)
                        .expect("mapped table routine input has a declared parameter")
                        .mode,
                    crate::storage::RoutineParameterMode::Variadic { .. }
                )
            {
                variadic_values[variadic_count] = value;
                variadic_count += 1;
                provided[input_index] = true;
            } else {
                routine_params[input_index] = value;
                provided[input_index] = true;
            }
        }
        if variadic_count != 0 {
            let input_index = routine.argument_count - 1;
            let parameter = routine
                .parameter_for_input(input_index)
                .expect("variadic table routine input has a declared parameter");
            let ColType::Array(element) = parameter.ctype else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "variadic table routine parameter is not an array"
                ));
            };
            routine_params[input_index] = Datum::Array {
                element,
                raw: crate::sql::array::build(&variadic_values[..variadic_count], arena)?,
            };
        }
        for input_index in 0..routine.argument_count {
            if provided[input_index] {
                continue;
            }
            let parameter = routine
                .parameter_for_input(input_index)
                .expect("table routine input has a declared parameter");
            let Some(default) = parameter.mode.default() else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "resolved table routine is missing a required argument"
                ));
            };
            let source = arena
                .alloc_str(default.as_str())
                .map_err(|_| arena_full())?;
            let expression = crate::sql::parser::parse_expression(source, arena)?;
            let value = crate::sql::eval::eval_full(expression, arena, params, columns, &hooks)?;
            routine_params[input_index] = crate::sql::eval::cast_to(value, parameter.ctype, arena)?;
        }
        let owner = storage.role_name(routine.ownership.owner_to(txid).into(), txid);
        let _security = routine
            .attributes
            .security_definer
            .then(|| crate::sql::eval::funcs::system::enter_current_user(owner.as_str()));
        let _config = (!routine.configs().is_empty())
            .then(|| crate::sql::guc::enter_active_routine_configs(routine.configs()))
            .transpose()?;
        if routine.language == crate::storage::RoutineLanguage::PlPgSql {
            let (invocations, statement_arena) = match (invocations, statement_arena) {
                (Some(invocations), Some(statement_arena)) => (invocations, statement_arena),
                (None, None) => super::active_routine_invocations().ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "PL/pgSQL table functions require a resumable query executor"
                    )
                })?,
                _ => {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "routine invocation state is missing its statement arena"
                    ));
                }
            };
            return invocations
                .resolve_rows(
                    routine_slot,
                    &routine_params[..routine.argument_count],
                    statement_arena,
                )?
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "pending PL/pgSQL table routine did not suspend query execution"
                    )
                });
        }
        let program = super::parse_stored_routine_function_program(
            routine.body_kind,
            routine.body.as_str(),
            arena,
            scalar_result == Some(ColType::Void),
            routine.name.as_str(),
            routine.arguments(),
        )?;
        if super::routine_program_requires_mutable_execution(&program) {
            let (invocations, statement_arena) = match (invocations, statement_arena) {
                (Some(invocations), Some(statement_arena)) => (invocations, statement_arena),
                (None, None) => super::active_routine_invocations().ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "data-modifying SQL table functions require a resumable query executor"
                    )
                })?,
                _ => {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "routine invocation state is missing its statement arena"
                    ));
                }
            };
            return invocations
                .resolve_rows(
                    routine_slot,
                    &routine_params[..routine.argument_count],
                    statement_arena,
                )?
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "pending SQL table routine did not suspend query execution"
                    )
                });
        }
        for query in program.preceding {
            let super::RoutinePrelude::Statement(statement) = query else {
                let super::RoutinePrelude::Forbidden(statement) = query else {
                    unreachable!("routine prelude has two variants");
                };
                return Err(super::routine_forbidden_statement_error(statement));
            };
            let query = match statement {
                crate::sql::ast::Stmt::Select(query) => super::RoutineQuery::Select(query),
                crate::sql::ast::Stmt::SetQuery(query) => super::RoutineQuery::Set(query),
                _ => unreachable!("mutable table routine was classified before execution"),
            };
            super::execute_bound_routine_query(
                &query,
                storage,
                routine_slot,
                *routine,
                txid,
                arena,
                &routine_params[..routine.argument_count],
                false,
                &mut |_| Ok(()),
            )?;
        }
        let result_query = match program.result {
            super::RoutineFunctionResult::Query(query) => query,
            super::RoutineFunctionResult::Void(statement) => match statement {
                crate::sql::ast::Stmt::Select(query) => arena
                    .alloc(super::RoutineQuery::Select(query))
                    .map_err(|_| arena_full())?,
                crate::sql::ast::Stmt::SetQuery(query) => arena
                    .alloc(super::RoutineQuery::Set(query))
                    .map_err(|_| arena_full())?,
                _ => unreachable!("mutable table routine was classified before execution"),
            },
            _ => unreachable!("mutable table routine was classified before execution"),
        };
        if scalar_result == Some(ColType::Void) {
            super::execute_bound_routine_query(
                result_query,
                storage,
                routine_slot,
                *routine,
                txid,
                arena,
                &routine_params[..routine.argument_count],
                false,
                &mut |_| Ok(()),
            )?;
            return Ok(&[]);
        }
        const EMPTY: &[u8] = &[];
        let mut rows: *mut &[u8] = core::ptr::null_mut();
        let mut len = 0usize;
        let mut cap = 0usize;
        super::execute_bound_routine_query(
            result_query,
            storage,
            routine_slot,
            *routine,
            txid,
            arena,
            &routine_params[..routine.argument_count],
            false,
            &mut |values| {
                let expected_columns = table_columns.map_or(1, <[_]>::len);
                if values.len() != expected_columns {
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "SQL function query must return {} column{}",
                        expected_columns,
                        if expected_columns == 1 { "" } else { "s" }
                    ));
                }
                let encoded = if let Some(output) = table_columns {
                    let mut cast = [Datum::Null; MAX_COLUMNS];
                    for (slot, column) in output.iter().enumerate() {
                        let projected =
                            crate::sql::exec::encode_projected_pub(&[values[slot]], arena)?;
                        cast[slot] = crate::sql::eval::cast_to(
                            crate::sql::exec::decode_projected_pub(projected, 0),
                            column.ctype,
                            arena,
                        )?;
                    }
                    crate::sql::exec::encode_projected_pub(&cast[..output.len()], arena)?
                } else {
                    let projected = crate::sql::exec::encode_projected_pub(values, arena)?;
                    let value = crate::sql::exec::decode_projected_pub(projected, 0);
                    let value = crate::sql::eval::cast_to(
                        value,
                        scalar_result.expect("set routine result"),
                        arena,
                    )?;
                    crate::sql::exec::encode_projected_pub(&[value], arena)?
                };
                if len == cap {
                    let new_cap = if cap == 0 { 8 } else { cap * 2 };
                    let fresh = arena
                        .alloc_slice_with(new_cap, |_| EMPTY)
                        .map_err(|_| arena_full())?;
                    if len > 0 {
                        let prior = unsafe { core::slice::from_raw_parts(rows, len) };
                        fresh[..len].copy_from_slice(prior);
                    }
                    rows = fresh.as_mut_ptr();
                    cap = new_cap;
                }
                unsafe { rows.add(len).write(encoded) };
                len += 1;
                Ok(())
            },
        )?;
        return Ok(if len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(rows, len) }
        });
    }
    if args.len() != 2 && args.len() != 3 {
        return Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "generate_series expects 2 or 3 arguments"
        ));
    }
    // Temporal series: a date/timestamp start with an interval step.
    let start_val = crate::sql::eval::text_view(eval_argument(args[0])?);
    let stop_raw = eval_argument(args[1])?;
    let step_raw = if args.len() == 3 {
        eval_argument(args[2])?
    } else {
        Datum::Int4(1)
    };
    if let Some((base, kind)) = crate::sql::eval::timestamp_series_start(&start_val) {
        if args.len() != 3 {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "generate_series over timestamps requires a step"
            ));
        }
        // Coerce bare string literals for the stop and step (function resolution).
        let stop_val = crate::sql::eval::cast_to(
            crate::sql::eval::text_view(stop_raw),
            kind.coltype(),
            arena,
        )?;
        let step_val = crate::sql::eval::cast_to(
            crate::sql::eval::text_view(step_raw),
            ColType::Interval,
            arena,
        )?;
        let (Some((stop_micros, _)), Datum::Interval(step_iv)) = (
            crate::sql::eval::timestamp_series_start(&stop_val),
            step_val,
        ) else {
            return Ok(&[]);
        };
        let count = crate::sql::eval::timestamp_series_count(base, stop_micros, step_iv)?;
        const EMPTY: &[u8] = &[];
        let rows = arena
            .alloc_slice_with(count, |_| EMPTY)
            .map_err(|_| arena_full())?;
        let mut v = base;
        for slot in rows.iter_mut() {
            *slot = crate::sql::exec::encode_projected_pub(&[kind.datum(v)], arena)?;
            v = crate::sql::datetime::add_interval(v, step_iv);
        }
        return Ok(&*rows);
    }
    if start_val.is_null() {
        return Ok(&[]);
    }
    if matches!(start_val, Datum::Numeric(_))
        || matches!(stop_raw, Datum::Numeric(_))
        || matches!(step_raw, Datum::Numeric(_))
    {
        if stop_raw.is_null() || step_raw.is_null() {
            return Ok(&[]);
        }
        let (Datum::Numeric(start), Datum::Numeric(stop), Datum::Numeric(step)) = (
            crate::sql::eval::cast_to(start_val, ColType::Numeric, arena)?,
            crate::sql::eval::cast_to(stop_raw, ColType::Numeric, arena)?,
            crate::sql::eval::cast_to(step_raw, ColType::Numeric, arena)?,
        ) else {
            return Ok(&[]);
        };
        let count = crate::sql::eval::numeric_series_count(start, stop, step, arena)?;
        const EMPTY: &[u8] = &[];
        let rows = arena
            .alloc_slice_with(count, |_| EMPTY)
            .map_err(|_| arena_full())?;
        for (index, slot) in rows.iter_mut().enumerate() {
            let value = crate::sql::eval::numeric_series_at(start, stop, step, index + 1, arena)?
                .expect("numeric series count and value share one boundary");
            *slot = crate::sql::exec::encode_projected_pub(&[Datum::Numeric(value)], arena)?;
        }
        return Ok(&*rows);
    }
    let as_i64 = |value: Datum<'a>| match value {
        Datum::Int4(value) => Ok(Some(value as i64)),
        Datum::Int8(value) => Ok(Some(value)),
        Datum::Null => Ok(None),
        _ => Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "generate_series requires integer arguments"
        )),
    };
    let Some(start) = as_i64(start_val)? else {
        return Ok(&[]);
    };
    let Some(stop) = as_i64(stop_raw)? else {
        return Ok(&[]);
    };
    let step = if args.len() == 3 {
        let Some(step) = as_i64(step_raw)? else {
            return Ok(&[]);
        };
        step
    } else {
        1
    };
    if step == 0 {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "step size cannot equal zero"
        ));
    }
    let count = if step > 0 {
        if stop < start {
            0
        } else {
            ((stop - start) / step) as usize + 1
        }
    } else if stop > start {
        0
    } else {
        ((start - stop) / (-step)) as usize + 1
    };
    const EMPTY: &[u8] = &[];
    let rows = arena
        .alloc_slice_with(count, |_| EMPTY)
        .map_err(|_| arena_full())?;
    let wide = matches!(start_val, Datum::Int8(_))
        || matches!(stop_raw, Datum::Int8(_))
        || matches!(step_raw, Datum::Int8(_));
    let mut v = start;
    for slot in rows.iter_mut() {
        let value = if wide {
            Datum::Int8(v)
        } else {
            Datum::Int4(v as i32)
        };
        *slot = crate::sql::exec::encode_projected_pub(&[value], arena)?;
        v += step;
    }
    Ok(&*rows)
}

#[allow(clippy::too_many_arguments)]
fn json_to_record_rows<'a, C: ColumnLookup<'a>>(
    name: &str,
    args: &[&'a Expr<'a>],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    columns: &C,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<&'a [&'a [u8]], SqlError> {
    if args.len() != 2 {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "a column definition list is required for functions returning record"
        ));
    }
    let recordset = name.ends_with("recordset");
    let source = match eval_full(args[0], arena, params, columns, hooks)? {
        Datum::Json { text, .. } | Datum::Text(text) => text,
        Datum::Null if recordset => return Ok(&[]),
        Datum::Null => {
            let Expr::Call {
                name: "__json_record_columns",
                args: definitions,
                ..
            } = args[1]
            else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "invalid JSON record definition"
                ));
            };
            let values = arena
                .alloc_slice_with(definitions.len(), |_| Datum::Null)
                .map_err(|_| arena_full())?;
            let encoded = crate::sql::exec::encode_projected_pub(values, arena)?;
            return arena
                .alloc_slice_copy(&[encoded])
                .map(|rows| &*rows)
                .map_err(|_| arena_full());
        }
        other => {
            return Err(crate::sql::eval::type_mismatch_pub(
                "JSON record conversion requires JSON",
                &other,
            ));
        }
    };
    let parsed = crate::sql::json::parse(source, arena)?;
    let items = if recordset {
        let crate::sql::json::Json::Array(items) = parsed else {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "cannot call json_to_recordset on a non-array"
            ));
        };
        items
    } else {
        arena
            .alloc_slice_copy(&[parsed])
            .map_err(|_| arena_full())?
    };
    let Expr::Call {
        name: "__json_record_columns",
        args: definitions,
        ..
    } = args[1]
    else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "invalid JSON record definition"
        ));
    };
    const EMPTY: &[u8] = &[];
    let rows = arena
        .alloc_slice_with(items.len(), |_| EMPTY)
        .map_err(|_| arena_full())?;
    for (row_slot, item) in rows.iter_mut().zip(items) {
        let crate::sql::json::Json::Object(members) = item else {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "cannot call populate_record on a scalar"
            ));
        };
        let values = arena
            .alloc_slice_with(definitions.len(), |_| Datum::Null)
            .map_err(|_| arena_full())?;
        for (value, definition) in values.iter_mut().zip(*definitions) {
            let Expr::Call {
                name: "__json_record_column",
                args:
                    [
                        Expr::Str(column_name),
                        Expr::Str(type_name),
                        Expr::Int(type_mod),
                    ],
                ..
            } = definition
            else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "invalid JSON record column definition"
                ));
            };
            let Some((_, member)) = members.iter().find(|(name, _)| *name == *column_name) else {
                continue;
            };
            let resolved = crate::sql::exec::resolve_routine_type(storage, txid, type_name)?;
            let type_oid = storage
                .routine_type_oid(resolved.ctype, resolved.user_type, txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "record column type is absent from the catalog"
                    )
                })?;
            *value = crate::sql::eval::funcs::json::json_value_to_type(
                *member,
                type_oid,
                i32::try_from(*type_mod).map_err(|_| {
                    sql_err!(sqlstate::INTERNAL_ERROR, "invalid record type modifier")
                })?,
                Datum::Null,
                arena,
                hooks,
            )?;
        }
        *row_slot = crate::sql::exec::encode_projected_pub(values, arena)?;
    }
    Ok(&*rows)
}

fn populate_record_rows<'a, C: ColumnLookup<'a>>(
    name: &str,
    args: &[&'a Expr<'a>],
    arena: &'a Arena,
    params: &[Datum<'a>],
    columns: &C,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<&'a [&'a [u8]], SqlError> {
    let valid_arity = if name.eq_ignore_ascii_case("json_populate_record")
        || name.eq_ignore_ascii_case("json_populate_recordset")
    {
        matches!(args.len(), 2 | 3)
    } else {
        args.len() == 2
    };
    if !valid_arity {
        return Err(srf_signature_error(name));
    }
    let base = eval_full(args[0], arena, params, columns, hooks)?;
    let source = eval_full(args[1], arena, params, columns, hooks)?;
    if let Some(option) = args.get(2) {
        crate::sql::eval::cast_to(
            eval_full(option, arena, params, columns, hooks)?,
            ColType::Bool,
            arena,
        )?;
    }
    let source = match source {
        Datum::Json { text, .. } | Datum::Text(text) => text,
        Datum::Null => return Ok(&[]),
        other => {
            return Err(crate::sql::eval::type_mismatch_pub(
                "populate_record requires JSON",
                &other,
            ));
        }
    };
    let parsed = crate::sql::json::parse(source, arena)?;
    let recordset = name.ends_with("recordset");
    let items = if recordset {
        let crate::sql::json::Json::Array(items) = parsed else {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "cannot call populate_recordset on a non-array"
            ));
        };
        items
    } else {
        arena
            .alloc_slice_copy(&[parsed])
            .map_err(|_| arena_full())?
    };
    const EMPTY: &[u8] = &[];
    let rows = arena
        .alloc_slice_with(items.len(), |_| EMPTY)
        .map_err(|_| arena_full())?;
    for (row_slot, item) in rows.iter_mut().zip(items) {
        let record = crate::sql::eval::funcs::json::populate_record_from_json(
            args[0], base, *item, arena, columns, hooks,
        )?;
        let fields = match record {
            Datum::Composite { fields, .. } | Datum::Record(fields) => fields,
            Datum::Null => continue,
            _ => unreachable!("populate_record returns a composite"),
        };
        let values = arena
            .alloc_slice_with(fields.len(), |_| Datum::Null)
            .map_err(|_| arena_full())?;
        for (value, field) in values.iter_mut().zip(fields) {
            *value = field.value;
        }
        *row_slot = crate::sql::exec::encode_projected_pub(values, arena)?;
    }
    Ok(&*rows)
}

fn xml_table_rows<'a, C: ColumnLookup<'a>>(
    args: &[&'a Expr<'a>],
    arena: &'a Arena,
    params: &[Datum<'a>],
    columns: &C,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<&'a [&'a [u8]], SqlError> {
    if args.len() != 4 {
        return Err(srf_signature_error("xmltable"));
    }
    let row_path = eval_full(args[0], arena, params, columns, hooks)?;
    let document = eval_full(args[1], arena, params, columns, hooks)?;
    if row_path.is_null() || document.is_null() {
        return Ok(&[]);
    }
    let row_path = crate::sql::eval::cast_to_text(row_path, arena)?;
    let document = match document {
        Datum::Xml(text) => text,
        other => {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "XMLTABLE document must be xml, got {:?}",
                other
            ));
        }
    };
    let Expr::Call {
        name: "__xml_namespaces",
        args: namespace_specs,
        ..
    } = *args[3]
    else {
        return Err(srf_signature_error("xmltable"));
    };
    if !namespace_specs.len().is_multiple_of(2) {
        return Err(srf_signature_error("xmltable"));
    }
    let mut namespaces = [("", ""); crate::sql::parser::MAX_LIST / 2];
    let mut namespace_count = 0usize;
    for pair in namespace_specs.as_chunks::<2>().0 {
        let uri = eval_full(pair[0], arena, params, columns, hooks)?;
        if uri.is_null() {
            return Err(sql_err!(
                sqlstate::NULL_VALUE_NOT_ALLOWED,
                "null XML namespace URI"
            ));
        }
        let uri = crate::sql::eval::cast_to_text(uri, arena)?;
        let Expr::Str(prefix) = pair[1] else {
            return Err(srf_signature_error("xmltable"));
        };
        if namespaces[..namespace_count]
            .iter()
            .any(|(existing, _)| existing == prefix)
        {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "duplicate XML namespace prefix \"{}\"",
                prefix
            ));
        }
        namespaces[namespace_count] = (prefix, uri);
        namespace_count += 1;
    }
    let row_path = crate::sql::xml::rewrite_namespaces(
        row_path,
        document,
        &namespaces[..namespace_count],
        arena,
    )?;
    let roots = crate::sql::xml::xpath(row_path, document, arena)?;
    let Expr::Call {
        name: "__xml_table_columns",
        args: specifications,
        ..
    } = *args[2]
    else {
        return Err(srf_signature_error("xmltable"));
    };
    const EMPTY: &[u8] = &[];
    let mut encoded = [EMPTY; crate::sql::parser::MAX_ROWS];
    let mut count = 0usize;
    for (ordinal, root) in roots.iter().enumerate() {
        let Datum::Xml(context) = root else {
            continue;
        };
        if count == encoded.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "XMLTABLE produced too many rows"
            ));
        }
        let mut values = [Datum::Null; MAX_COLUMNS];
        for (column, specification) in specifications.iter().enumerate() {
            let Expr::Call { name, args, .. } = specification else {
                return Err(srf_signature_error("xmltable"));
            };
            if *name == "__xml_table_ordinality" {
                values[column] = Datum::Int8((ordinal + 1) as i64);
                continue;
            }
            if *name != "__xml_table_column" || args.len() != 6 {
                return Err(srf_signature_error("xmltable"));
            }
            let (
                Expr::Str(column_name),
                Expr::Str(type_name),
                Expr::Int(type_mod),
                Expr::Bool(not_null),
            ) = (args[0], args[1], args[2], args[5])
            else {
                return Err(srf_signature_error("xmltable"));
            };
            let path = eval_full(args[3], arena, params, columns, hooks)?;
            let path = if path.is_null() {
                column_name
            } else {
                crate::sql::eval::cast_to_text(path, arena)?
            };
            let path = crate::sql::xml::rewrite_namespaces(
                path,
                document,
                &namespaces[..namespace_count],
                arena,
            )?;
            let selected = crate::sql::xml::xpath_relative(path, context, arena)?;
            let target_is_xml = ColType::from_sql_name(type_name) == Some(ColType::Xml);
            let mut value = if selected.is_empty() {
                if matches!(args[4], Expr::Null) {
                    Datum::Null
                } else {
                    eval_full(args[4], arena, params, columns, hooks)?
                }
            } else if target_is_xml {
                let text = arena
                    .alloc_str_display(XmlTableConcat(selected))
                    .map_err(|_| arena_full())?;
                Datum::Xml(text)
            } else {
                if selected.len() != 1 {
                    return Err(sql_err!(
                        sqlstate::CARDINALITY_VIOLATION,
                        "more than one value returned by XMLTABLE column path"
                    ));
                }
                let Datum::Xml(fragment) = selected[0] else {
                    unreachable!()
                };
                Datum::Text(crate::sql::xml::string_value(fragment, arena)?)
            };
            if !value.is_null() {
                value = crate::sql::eval::funcs::json::sql_json_cast(
                    value,
                    type_name,
                    *type_mod as i32,
                    arena,
                    hooks,
                )?;
            }
            if *not_null && value.is_null() {
                return Err(sql_err!(
                    sqlstate::NOT_NULL_VIOLATION,
                    "null value in column \"{}\" violates not-null constraint",
                    column_name
                ));
            }
            values[column] = value;
        }
        encoded[count] =
            crate::sql::exec::encode_projected_pub(&values[..specifications.len()], arena)?;
        count += 1;
    }
    arena
        .alloc_slice_copy(&encoded[..count])
        .map(|rows| &*rows)
        .map_err(|_| arena_full())
}

struct XmlTableConcat<'a>(&'a [Datum<'a>]);
impl core::fmt::Display for XmlTableConcat<'_> {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for value in self.0 {
            if let Datum::Xml(text) = value {
                output.write_str(text)?;
            }
        }
        Ok(())
    }
}

fn json_table_rows<'a, C: ColumnLookup<'a>>(
    args: &[&'a Expr<'a>],
    arena: &'a Arena,
    params: &[Datum<'a>],
    columns: &C,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<&'a [&'a [u8]], SqlError> {
    if args.len() != 6 {
        return Err(srf_signature_error("json_table"));
    }
    let context = eval_full(args[0], arena, params, columns, hooks)?;
    if context.is_null() {
        return Ok(&[]);
    }
    // Context conversion errors are outside JSON_TABLE's ON ERROR boundary.
    let context = match context {
        Datum::Json { text, .. } => Datum::Json { text, jsonb: true },
        Datum::Text(text) | Datum::Bpchar(text) => {
            crate::sql::eval::cast_to(Datum::Text(text), ColType::Jsonb, arena)?
        }
        other => crate::sql::eval::cast_to(other, ColType::Jsonb, arena)?,
    };
    let path = eval_full(args[1], arena, params, columns, hooks)?;
    let Expr::Call {
        name: "__json_table_passing",
        args: passing,
        ..
    } = args[3]
    else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "invalid JSON_TABLE passing specification"
        ));
    };
    let variables = crate::sql::eval::funcs::json::sql_json_passing(
        "json_table",
        passing,
        0,
        arena,
        params,
        columns,
        hooks,
    )?
    .map(|text| Datum::Json { text, jsonb: true });
    let Expr::Str(on_error) = args[5] else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "invalid JSON_TABLE error behavior"
        ));
    };
    let roots = match crate::sql::eval::funcs::json::path_query_values(
        context, path, variables, None, false, arena,
    ) {
        Ok(Some(values)) => values,
        Ok(None) => return Ok(&[]),
        Err(_) if *on_error == "empty" => return Ok(&[]),
        Err(error) => return Err(error),
    };
    let Expr::Call {
        name: "__json_table_columns",
        args: specifications,
        ..
    } = args[4]
    else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "invalid JSON_TABLE column specification"
        ));
    };
    const EMPTY: &[u8] = &[];
    let mut encoded = [EMPTY; crate::sql::parser::MAX_ROWS];
    let mut encoded_count = 0usize;
    let output_columns = json_table_specifications_width(specifications)?;
    for (index, root) in roots.iter().enumerate() {
        let values = [Datum::Null; MAX_COLUMNS];
        json_table_emit_level(
            *root,
            index + 1,
            specifications,
            0,
            output_columns,
            values,
            variables,
            arena,
            params,
            columns,
            hooks,
            &mut encoded,
            &mut encoded_count,
        )?;
    }
    arena
        .alloc_slice_copy(&encoded[..encoded_count])
        .map(|rows| &*rows)
        .map_err(|_| arena_full())
}

#[allow(clippy::too_many_arguments)]
fn json_table_emit_level<'a, C: ColumnLookup<'a>>(
    current: crate::sql::json::Json<'a>,
    ordinal: usize,
    specifications: &[&'a Expr<'a>],
    start_column: usize,
    output_columns: usize,
    mut values: [Datum<'a>; MAX_COLUMNS],
    variables: Option<Datum<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    columns: &C,
    hooks: &EvalHooks<'_, 'a>,
    encoded: &mut [&'a [u8]; crate::sql::parser::MAX_ROWS],
    encoded_count: &mut usize,
) -> Result<usize, SqlError> {
    let mut column = start_column;
    let mut nested = [None; crate::sql::parser::MAX_LIST];
    let mut nested_count = 0usize;
    for specification in specifications {
        let Expr::Call { name, args, .. } = specification else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid JSON_TABLE column specification"
            ));
        };
        match *name {
            "__json_table_ordinality" => {
                values[column] = Datum::Int8(i64::try_from(ordinal).map_err(|_| {
                    sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "ordinality exceeds bigint")
                })?);
                column += 1;
            }
            "__json_table_value" | "__json_table_exists" => {
                values[column] = json_table_column_value(
                    current, name, args, variables, arena, params, columns, hooks,
                )?;
                column += 1;
            }
            "__json_table_nested" => {
                let width = json_table_spec_width(specification)?;
                nested[nested_count] = Some((*specification, column));
                nested_count += 1;
                column += width;
            }
            _ => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "invalid JSON_TABLE column kind"
                ));
            }
        }
    }
    if nested_count == 0 {
        if *encoded_count == encoded.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "JSON_TABLE produces more than {} rows",
                encoded.len()
            ));
        }
        encoded[*encoded_count] =
            crate::sql::exec::encode_projected_pub(&values[..output_columns], arena)?;
        *encoded_count += 1;
        return Ok(column - start_column);
    }

    for (nested_specification, nested_start) in nested[..nested_count].iter().flatten() {
        let Expr::Call { args, .. } = nested_specification else {
            unreachable!("nested specification is a call")
        };
        let path = eval_full(args[0], arena, params, columns, hooks)?;
        let current_text = crate::sql::eval::json_to_text_pub(&current, arena)?;
        let children = crate::sql::eval::funcs::json::path_query_values(
            Datum::Json {
                text: current_text,
                jsonb: true,
            },
            path,
            variables,
            None,
            false,
            arena,
        )?
        .unwrap_or(&[]);
        let Expr::Call {
            name: "__json_table_columns",
            args: child_specifications,
            ..
        } = args[2]
        else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid JSON_TABLE nested columns"
            ));
        };
        if children.is_empty() {
            if *encoded_count == encoded.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "JSON_TABLE produces more than {} rows",
                    encoded.len()
                ));
            }
            encoded[*encoded_count] =
                crate::sql::exec::encode_projected_pub(&values[..output_columns], arena)?;
            *encoded_count += 1;
        } else {
            for (child_index, child) in children.iter().enumerate() {
                json_table_emit_level(
                    *child,
                    child_index + 1,
                    child_specifications,
                    *nested_start,
                    output_columns,
                    values,
                    variables,
                    arena,
                    params,
                    columns,
                    hooks,
                    encoded,
                    encoded_count,
                )?;
            }
        }
    }
    Ok(column - start_column)
}

fn json_table_spec_width(specification: &Expr<'_>) -> Result<usize, SqlError> {
    let Expr::Call { name, args, .. } = specification else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "invalid JSON_TABLE column"
        ));
    };
    if *name != "__json_table_nested" {
        return Ok(1);
    }
    let Some(Expr::Call {
        name: "__json_table_columns",
        args: children,
        ..
    }) = args.get(2).copied()
    else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "invalid JSON_TABLE nested columns"
        ));
    };
    let mut width = 0usize;
    for child in *children {
        width = width
            .checked_add(json_table_spec_width(child)?)
            .ok_or_else(|| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "JSON_TABLE is too wide"))?;
    }
    Ok(width)
}

fn json_table_specifications_width(specifications: &[&Expr<'_>]) -> Result<usize, SqlError> {
    let mut width = 0usize;
    for specification in specifications {
        width = width
            .checked_add(json_table_spec_width(specification)?)
            .ok_or_else(|| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "JSON_TABLE is too wide"))?;
    }
    if width > MAX_COLUMNS {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "JSON_TABLE output exceeds configured column capacity"
        ));
    }
    Ok(width)
}

#[allow(clippy::too_many_arguments)]
fn json_table_column_value<'a, C: ColumnLookup<'a>>(
    current: crate::sql::json::Json<'a>,
    kind: &str,
    args: &[&'a Expr<'a>],
    variables: Option<Datum<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    columns: &C,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    let (Some(Expr::Str(type_name)), Some(Expr::Int(type_mod))) =
        (args.get(1).copied(), args.get(2).copied())
    else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "invalid JSON_TABLE column type"
        ));
    };
    let type_mod = i32::try_from(*type_mod)
        .map_err(|_| sql_err!(sqlstate::INTERNAL_ERROR, "invalid JSON_TABLE type modifier"))?;
    let path = eval_full(args[3], arena, params, columns, hooks)?;
    let current_text = crate::sql::eval::json_to_text_pub(&current, arena)?;
    let queried = crate::sql::eval::funcs::json::path_query_values(
        Datum::Json {
            text: current_text,
            jsonb: true,
        },
        path,
        variables,
        None,
        false,
        arena,
    );
    if kind == "__json_table_exists" {
        let Expr::Str(behavior) = args[4] else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid JSON_TABLE EXISTS behavior"
            ));
        };
        let value = match queried {
            Ok(Some(values)) => Datum::Bool(!values.is_empty()),
            Ok(None) => Datum::Null,
            Err(error) => match *behavior {
                "true" => Datum::Bool(true),
                "false" => Datum::Bool(false),
                "unknown" => Datum::Null,
                _ => return Err(error),
            },
        };
        return crate::sql::eval::funcs::json::sql_json_cast(
            value, type_name, type_mod, arena, hooks,
        );
    }

    let [
        _,
        _,
        _,
        _,
        Expr::Str(format),
        Expr::Str(wrapper),
        Expr::Str(quotes),
        empty_default,
        error_default,
        Expr::Str(empty_code),
        Expr::Str(error_code),
    ] = args
    else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "invalid JSON_TABLE value specification"
        ));
    };
    let convert = |values: &'a [crate::sql::json::Json<'a>]| -> Result<Datum<'a>, SqlError> {
        if values.is_empty() {
            return json_table_behavior(
                empty_code,
                empty_default,
                sql_err!(sqlstate::DATA_EXCEPTION, "no SQL/JSON item"),
                type_name,
                type_mod,
                arena,
                params,
                columns,
                hooks,
            );
        }
        let query_semantics = *format == "json"
            || *wrapper != "without"
            || ColType::from_sql_name(type_name).is_some_and(|ctype| {
                matches!(ctype, ColType::Json | ColType::Jsonb | ColType::Array(_))
            })
            || hooks
                .catalog
                .is_some_and(|catalog| catalog.is_composite_type_name(type_name));
        let raw = if query_semantics {
            json_table_query_datum(values, wrapper, quotes, arena)?
        } else {
            if values.len() != 1 {
                return Err(sql_err!(
                    sqlstate::CARDINALITY_VIOLATION,
                    "JSON path expression yielded multiple values"
                ));
            }
            match values[0] {
                crate::sql::json::Json::Null => Datum::Null,
                crate::sql::json::Json::Str(value) => Datum::Text(value),
                crate::sql::json::Json::Bool(value) => Datum::Bool(value),
                crate::sql::json::Json::Number(value) => Datum::Text(value),
                _ => {
                    return Err(sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "JSON path expression yielded a non-scalar value"
                    ));
                }
            }
        };
        crate::sql::eval::funcs::json::sql_json_cast(raw, type_name, type_mod, arena, hooks)
    };
    match queried {
        Ok(Some(values)) => match convert(values) {
            Ok(value) => Ok(value),
            Err(error) => json_table_behavior(
                error_code,
                error_default,
                error,
                type_name,
                type_mod,
                arena,
                params,
                columns,
                hooks,
            ),
        },
        Ok(None) => Ok(Datum::Null),
        Err(error) => json_table_behavior(
            error_code,
            error_default,
            error,
            type_name,
            type_mod,
            arena,
            params,
            columns,
            hooks,
        ),
    }
}

fn json_table_query_datum<'a>(
    values: &'a [crate::sql::json::Json<'a>],
    wrapper: &str,
    quotes: &str,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    use core::fmt::Write as _;
    let wrap = wrapper == "unconditional"
        || (wrapper == "conditional"
            && (values.len() != 1
                || !matches!(
                    values[0],
                    crate::sql::json::Json::Array(_) | crate::sql::json::Json::Object(_)
                )));
    if !wrap && values.len() != 1 {
        return Err(sql_err!(
            sqlstate::CARDINALITY_VIOLATION,
            "JSON path expression yielded multiple values"
        ));
    }
    if !wrap
        && quotes == "omit"
        && let crate::sql::json::Json::Str(value) = values[0]
    {
        return Ok(Datum::Text(value));
    }
    let mut text = StackStr::<65536>::new();
    if wrap {
        text.write_char('[').map_err(|_| arena_full())?;
        for (index, value) in values.iter().enumerate() {
            if index > 0 {
                text.write_str(", ").map_err(|_| arena_full())?;
            }
            value.write(&mut text).map_err(|_| arena_full())?;
        }
        text.write_char(']').map_err(|_| arena_full())?;
    } else {
        values[0].write(&mut text).map_err(|_| arena_full())?;
    }
    if text.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "JSON_TABLE column value is too large"
        ));
    }
    Ok(Datum::Json {
        text: arena.alloc_str(text.as_str()).map_err(|_| arena_full())?,
        jsonb: true,
    })
}

#[allow(clippy::too_many_arguments)]
fn json_table_behavior<'a, C: ColumnLookup<'a>>(
    behavior: &str,
    default: &'a Expr<'a>,
    original: SqlError,
    type_name: &str,
    type_mod: i32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    columns: &C,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    let value = match behavior {
        "null" => return Ok(Datum::Null),
        "array" => Datum::Json {
            text: "[]",
            jsonb: true,
        },
        "object" => Datum::Json {
            text: "{}",
            jsonb: true,
        },
        "default" => eval_full(default, arena, params, columns, hooks)?,
        _ => return Err(original),
    };
    crate::sql::eval::funcs::json::sql_json_cast(value, type_name, type_mod, arena, hooks)
}

#[allow(clippy::too_many_arguments)]
fn rows_from_base_rows_outer<'a, C: ColumnLookup<'a>>(
    functions: &'a [TableRef<'a>],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    columns: &C,
    eval_hooks: Option<&EvalHooks<'_, 'a>>,
    invocations: Option<&super::RoutineInvocationState<'a>>,
    statement_arena: Option<&'a Arena>,
) -> Result<&'a [&'a [u8]], SqlError> {
    const EMPTY_ROWS: &[&[u8]] = &[];
    let mut rows_by_function: [&[&[u8]]; crate::sql::parser::MAX_LIST] =
        [EMPTY_ROWS; crate::sql::parser::MAX_LIST];
    let mut widths = [0usize; crate::sql::parser::MAX_LIST];
    let mut row_count = 0usize;
    let mut total_width = 0usize;
    for (index, function) in functions.iter().enumerate() {
        let definition = table_func_def_outer(function, storage, txid, arena, params, columns)?;
        widths[index] = definition.n_columns;
        total_width = total_width
            .checked_add(definition.n_columns)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "ROWS FROM output exceeds configured column capacity"
                )
            })?;
        if total_width > MAX_COLUMNS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "ROWS FROM output exceeds configured column capacity"
            ));
        }
        rows_by_function[index] = table_func_base_rows_outer(
            function,
            storage,
            txid,
            arena,
            params,
            columns,
            eval_hooks,
            invocations,
            statement_arena,
        )?;
        row_count = row_count.max(rows_by_function[index].len());
    }
    const EMPTY: &[u8] = &[];
    let rows = arena
        .alloc_slice_with(row_count, |_| EMPTY)
        .map_err(|_| arena_full())?;
    for (row_index, encoded) in rows.iter_mut().enumerate() {
        let mut values = [Datum::Null; MAX_COLUMNS];
        let mut output_column = 0usize;
        for function_index in 0..functions.len() {
            let width = widths[function_index];
            if let Some(row) = rows_by_function[function_index].get(row_index) {
                for column in 0..width {
                    values[output_column + column] =
                        crate::sql::exec::decode_projected_col_record(row, column, arena)?;
                }
            }
            output_column += width;
        }
        *encoded = crate::sql::exec::encode_projected_pub(&values[..total_width], arena)?;
    }
    Ok(&*rows)
}
