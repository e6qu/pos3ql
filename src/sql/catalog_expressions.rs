//! Catalog expressions retain parser-bound column ordinals across renames.

use super::{Arena, Datum, SqlError, Storage, SynthTable, arena_full};
use crate::mem::arena::ArenaString;
use crate::sql::catalog_node::{CatalogNode, HEADER};
use crate::sql::{
    ast::Expr,
    eval::sqlstate,
    lexer::{Lexer, Tok},
    parser,
};
use crate::sql_err;
use crate::storage::TableDef;
use core::fmt::Write;

#[derive(Clone, Copy)]
struct Site {
    start: usize,
    end: usize,
    column: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum Kind<'t> {
    Expression,
    /// An expression whose value is assigned to the given type: a column
    /// default. PostgreSQL prints its literals in that type.
    Assignment(i32),
    /// Routine parameter defaults, each assigned to its parameter's type.
    AssignmentList(&'t [i32]),
    List,
    Partition,
    Query,
}

pub(super) fn current<'a>(
    storage: &Storage,
    transaction: u32,
    table: SynthTable<'a>,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    use crate::sql::types::ColType;
    if !table
        .def
        .columns()
        .iter()
        .any(|column| column.ctype == ColType::PgNodeTree)
        || !table.rows.iter().any(|values| {
            table
                .def
                .columns()
                .iter()
                .zip(*values)
                .any(|(column, value)| column.ctype == ColType::PgNodeTree && !value.is_null())
        })
    {
        return Ok(table);
    }
    let rows = arena
        .alloc_slice_with(table.rows.len(), |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    for (target, values) in rows.iter_mut().zip(table.rows) {
        if !table
            .def
            .columns()
            .iter()
            .zip(*values)
            .any(|(column, value)| column.ctype == ColType::PgNodeTree && !value.is_null())
        {
            *target = values;
            continue;
        }
        let row = arena.alloc_slice_copy(values).map_err(|_| arena_full())?;
        for (position, column) in table.def.columns().iter().enumerate() {
            if column.ctype != ColType::PgNodeTree || row[position].is_null() {
                continue;
            }
            let source = super::view_rows::string(row[position])?;
            let (relation, kind) = match (table.def.name.as_str(), column.name.as_str()) {
                ("pg_constraint", "conbin") => (Some("conrelid"), Kind::Expression),
                ("pg_attrdef", "adbin") => (
                    Some("adrelid"),
                    default_destination(storage, transaction, &table, row, arena)?,
                ),
                ("pg_index", "indpred") => (Some("indrelid"), Kind::Expression),
                ("pg_index", "indexprs") => (Some("indrelid"), Kind::List),
                ("pg_statistic_ext", "stxexprs") => (Some("stxrelid"), Kind::List),
                ("pg_policy", "polqual" | "polwithcheck") => (Some("polrelid"), Kind::Expression),
                ("pg_publication_rel", "prqual") => (Some("prrelid"), Kind::Expression),
                ("pg_trigger", "tgqual") => (Some("tgrelid"), Kind::Expression),
                ("pg_partitioned_table", "partexprs") => (Some("partrelid"), Kind::List),
                ("pg_class", "relpartbound") => (None, Kind::Partition),
                ("pg_type", "typdefaultbin") => (None, domain_default_destination(&table, row)?),
                ("pg_proc", "proargdefaults") => (
                    None,
                    parameter_default_destinations(storage, transaction, &table, row, arena)?,
                ),
                ("pg_proc", "prosqlbody") => (None, Kind::Query),
                _ => {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "catalog expression {}.{} is not supported",
                        table.def.name.as_str(),
                        column.name.as_str()
                    ));
                }
            };
            let relation = relation
                .map(|field| {
                    super::view_rows::integer(
                        row[table
                            .def
                            .column_index(field)
                            .expect("catalog expression relation field exists")],
                    )
                })
                .transpose()?
                .unwrap_or(0);
            let slot = (0..storage.table_count()).find(|&slot| {
                storage.table_slot_visible_to(slot, transaction)
                    && super::user_table_oid(storage, slot) == relation
            });
            row[position] = Datum::PgNodeTree(if let Some(slot) = slot {
                let definition = storage.table_def(slot, transaction);
                if table.def.name.as_str() == "pg_policy" {
                    let policy_oid = super::view_rows::integer(row[0])?;
                    let policy = storage
                        .policies_with_slots_visible_to(transaction)
                        .find(|(_, policy)| crate::storage::policy_oid(policy) == policy_oid)
                        .map(|(slot, _)| slot)
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::DATA_EXCEPTION,
                                "catalog policy identity is missing"
                            )
                        })?;
                    bind_policy(
                        storage,
                        transaction,
                        slot,
                        policy,
                        source,
                        &definition,
                        arena,
                    )?
                } else {
                    bind(source, kind, Some(&definition), arena)?
                }
            } else {
                bind(source, kind, None, arena)?
            });
        }
        *target = row;
    }
    Ok(SynthTable { rows, ..table })
}

fn reference_start(
    source: &str,
    site: parser::ColumnSource<'_>,
    arena: &Arena,
) -> Result<usize, SqlError> {
    if site.qualifier_parts == 0 {
        return Ok(site.start);
    }
    let mut lexer = Lexer::new(source, arena);
    let mut starts = [0; 5];
    let mut count = 0;
    loop {
        let token = lexer.next_token().map_err(|error| {
            sql_err!(
                error.kind.sqlstate(),
                "invalid catalog expression: {}",
                error.message
            )
        })?;
        if token == Tok::Eof {
            break;
        }
        starts[count % starts.len()] = lexer.token_start();
        count += 1;
        if lexer.token_start() == site.start {
            return count
                .checked_sub(1 + 2 * site.qualifier_parts)
                .map(|position| starts[position % starts.len()])
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "catalog column source qualifier is incomplete"
                    )
                });
        }
    }
    Err(sql_err!(
        sqlstate::INTERNAL_ERROR,
        "catalog column source is missing"
    ))
}

pub(crate) fn bind<'a>(
    source: &str,
    kind: Kind,
    table: Option<&TableDef>,
    arena: &'a Arena,
) -> Result<CatalogNode<'a>, SqlError> {
    let mut output = ArenaString::new(arena);
    output.write_str(HEADER).map_err(|_| arena_full())?;
    if matches!(kind, Kind::Partition | Kind::Query) {
        write!(
            output,
            "{}\n0\n\n{source}",
            if matches!(kind, Kind::Partition) {
                "partition"
            } else {
                "query"
            }
        )
        .map_err(|_| arena_full())?;
        return CatalogNode::from_encoded(output.as_str()).ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid constructed catalog expression node"
            )
        });
    }
    let source = arena.alloc_str(source).map_err(|_| arena_full())?;
    let (parsed_source, columns, prefix) = match kind {
        Kind::Expression | Kind::Assignment(_) => {
            let (_, columns) = parser::parse_expr_with_column_sources(source, arena)?;
            (source, columns, 0)
        }
        Kind::List | Kind::AssignmentList(_) => {
            let query = arena
                .alloc_str_display(format_args!("SELECT {source}"))
                .map_err(|_| arena_full())?;
            let (_, columns) = parser::parse_query_with_column_sources(query, arena)?;
            (query, columns.columns, 7)
        }
        Kind::Partition | Kind::Query => {
            unreachable!("non-expression nodes have no scalar column references")
        }
    };
    let sites = arena
        .alloc_slice_with(if table.is_some() { columns.len() } else { 0 }, |_| Site {
            start: 0,
            end: 0,
            column: 0,
        })
        .map_err(|_| arena_full())?;
    if let Some(table) = table {
        for (target, column) in sites.iter_mut().zip(columns) {
            let number = table.column_index(column.name).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    column.name
                )
            })?;
            *target = Site {
                start: reference_start(parsed_source, *column, arena)? - prefix,
                end: column.end - prefix,
                column: usize::from(table.attribute_number(number)),
            };
        }
    }
    encode(source, kind, sites, output)
}

fn bind_policy<'a>(
    storage: &Storage,
    transaction: u32,
    slot: usize,
    policy: usize,
    source: &str,
    definition: &TableDef,
    arena: &'a Arena,
) -> Result<CatalogNode<'a>, SqlError> {
    let source = arena.alloc_str(source).map_err(|_| arena_full())?;
    let mut relation = crate::util::StackStr::<260>::new();
    super::write_identifier(&mut relation, definition.schema.as_str());
    relation.write_char('.').map_err(|_| arena_full())?;
    super::write_identifier(&mut relation, definition.name.as_str());
    if relation.is_truncated() {
        return Err(arena_full());
    }
    let query = arena
        .alloc_str_display(format_args!("SELECT {source} FROM {}", relation.as_str()))
        .map_err(|_| arena_full())?;
    let (select, sources) = parser::parse_query_with_column_sources(query, arena)?;
    let dependencies = storage.snapshot_policy_dependencies(policy, transaction, arena)?;
    let bindings = super::super::query::catalog_expression_bindings(
        select,
        sources.columns,
        storage,
        transaction,
        slot,
        dependencies.view(),
        arena,
    )?;
    let mut sites = crate::mem::arena::ArenaList::new(arena);
    for (column, binding) in sources.columns.iter().zip(bindings) {
        if let Some(position) = binding.get() {
            sites
                .push(Site {
                    start: column.start - 7,
                    end: column.end - 7,
                    column: usize::from(definition.attribute_number(position)),
                })
                .map_err(|_| arena_full())?;
        }
    }
    let mut output = ArenaString::new(arena);
    output.write_str(HEADER).map_err(|_| arena_full())?;
    encode(
        source,
        Kind::Expression,
        sites.into_mut_slice().map_err(|_| arena_full())?,
        output,
    )
}

fn encode<'a>(
    source: &'a str,
    kind: Kind,
    sites: &mut [Site],
    mut output: ArenaString<'a>,
) -> Result<CatalogNode<'a>, SqlError> {
    sites.sort_unstable_by_key(|site| (site.start, site.end, site.column));
    let mut count = 0;
    for at in 0..sites.len() {
        if count == 0
            || (sites[at].start, sites[at].end) != (sites[count - 1].start, sites[count - 1].end)
        {
            sites[count] = sites[at];
            count += 1;
        } else if sites[at].column != sites[count - 1].column {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "catalog column source has conflicting bindings"
            ));
        }
    }
    match kind {
        Kind::List => writeln!(output, "list\n{count}"),
        Kind::AssignmentList(types) => {
            let mut result = write!(output, "list");
            for (index, type_oid) in types.iter().enumerate() {
                result = result.and_then(|()| {
                    write!(output, "{}{type_oid}", if index == 0 { " " } else { "," })
                });
            }
            result.and_then(|()| writeln!(output, "\n{count}"))
        }
        Kind::Assignment(type_oid) => writeln!(output, "assignment {type_oid}\n{count}"),
        Kind::Expression | Kind::Partition | Kind::Query => {
            writeln!(output, "expression\n{count}")
        }
    }
    .map_err(|_| arena_full())?;
    for site in &sites[..count] {
        writeln!(output, "{},{},{}", site.start, site.end, site.column)
            .map_err(|_| arena_full())?;
    }
    write!(output, "\n{source}").map_err(|_| arena_full())?;
    CatalogNode::from_encoded(output.as_str()).ok_or_else(|| {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "invalid constructed catalog expression node"
        )
    })
}

pub(super) fn deparse<'a>(
    storage: &Storage,
    transaction: u32,
    node: &str,
    relation: i32,
    pretty: bool,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let corrupt = || {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "invalid stored catalog expression node"
        )
    };
    CatalogNode::from_encoded(node).ok_or_else(corrupt)?;
    let body = node.strip_prefix(HEADER).ok_or_else(|| {
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "cannot accept a value of type pg_node_tree"
        )
    })?;
    let (kind, rest) = body.split_once('\n').ok_or_else(corrupt)?;
    let destination = match kind.strip_prefix("assignment ") {
        Some(type_oid) => Some(type_oid.parse::<i32>().map_err(|_| corrupt())?),
        None if matches!(kind, "expression" | "list" | "partition" | "query")
            || kind.starts_with("list ") =>
        {
            None
        }
        None => return Err(corrupt()),
    };
    let list_destinations = kind.strip_prefix("list ");
    let kind = if destination.is_some() {
        "expression"
    } else if list_destinations.is_some() {
        "list"
    } else {
        kind
    };
    let mut list_destinations = list_destinations.map(|types| types.split(','));
    let (count, mut rest) = rest.split_once('\n').ok_or_else(corrupt)?;
    let count = count.parse::<usize>().map_err(|_| corrupt())?;
    if count > rest.len() {
        return Err(corrupt());
    }
    if matches!(kind, "partition" | "query") && count != 0 {
        return Err(corrupt());
    }
    let sites = arena
        .alloc_slice_with(count, |_| Site {
            start: 0,
            end: 0,
            column: 0,
        })
        .map_err(|_| arena_full())?;
    for site in sites.iter_mut() {
        let (line, next) = rest.split_once('\n').ok_or_else(corrupt)?;
        let mut fields = line.split(',');
        site.start = fields
            .next()
            .ok_or_else(corrupt)?
            .parse()
            .map_err(|_| corrupt())?;
        site.end = fields
            .next()
            .ok_or_else(corrupt)?
            .parse()
            .map_err(|_| corrupt())?;
        site.column = fields
            .next()
            .ok_or_else(corrupt)?
            .parse()
            .map_err(|_| corrupt())?;
        if fields.next().is_some() {
            return Err(corrupt());
        }
        rest = next;
    }
    let source = rest.strip_prefix('\n').ok_or_else(corrupt)?;
    if kind == "query" {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "input is a query, not an expression"
        ));
    }
    if count != 0 && relation == 0 {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "expression contains variables"
        ));
    }
    if relation != 0 && !super::relation_oid_is_visible(storage, transaction, relation) {
        return Ok(None);
    }
    let slot = (0..storage.table_count()).find(|&slot| {
        storage.table_slot_visible_to(slot, transaction)
            && super::user_table_oid(storage, slot) == relation
    });
    let mut output = ArenaString::new(arena);
    let mut copied = 0;
    for site in sites {
        if site.start < copied
            || site.end <= site.start
            || !source.is_char_boundary(site.start)
            || !source.is_char_boundary(site.end)
            || site.column == 0
        {
            return Err(corrupt());
        }
        let slot = slot.ok_or_else(|| {
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "catalog expression requires a table relation"
            )
        })?;
        let table = storage.table_def(slot, transaction);
        let position = table
            .attribute_column(u16::try_from(site.column).map_err(|_| corrupt())?)
            .ok_or_else(corrupt)?;
        let column = &table.columns()[position];
        output
            .write_str(&source[copied..site.start])
            .map_err(|_| arena_full())?;
        let mut name = crate::util::StackStr::<130>::new();
        super::write_identifier(&mut name, column.name.as_str());
        if name.is_truncated() {
            return Err(corrupt());
        }
        output.write_str(name.as_str()).map_err(|_| arena_full())?;
        copied = site.end;
    }
    output
        .write_str(&source[copied..])
        .map_err(|_| arena_full())?;
    let source = output.as_str();
    if kind == "partition" {
        return Ok(Some(source));
    }
    if kind == "list" {
        // The query parser has already established the list grammar. Split
        // only top-level commas, keeping strings and nested calls intact.
        let mut lexer = Lexer::new(source, arena);
        let mut depth = 0;
        let mut start = 0;
        let mut display = ArenaString::new(arena);
        loop {
            let token = lexer.next_token().map_err(|_| corrupt())?;
            if matches!(token, Tok::Op("(" | "[")) {
                depth += 1;
            }
            if matches!(token, Tok::Op(")" | "]")) {
                depth -= 1;
            }
            if token == Tok::Eof || (token == Tok::Op(",") && depth == 0) {
                if start != 0 {
                    display.write_str(", ").map_err(|_| arena_full())?;
                }
                let element = &source[start..lexer.token_start()];
                let destination = match list_destinations.as_mut() {
                    Some(types) => Some(
                        types
                            .next()
                            .ok_or_else(corrupt)?
                            .parse::<i32>()
                            .map_err(|_| corrupt())?,
                    ),
                    None => None,
                };
                let deparsed = match destination {
                    Some(type_oid) if !pretty => super::deparse::deparse(
                        &super::deparse::Context {
                            storage,
                            txid: transaction,
                            table: None,
                            parameters: &[],
                            joined: &[],
                            pretty: false,
                        },
                        element,
                        super::deparse::Destination::Assignment(type_oid),
                        arena,
                    )?,
                    _ => None,
                };
                display
                    .write_str(match deparsed {
                        Some(deparsed) => deparsed,
                        None => {
                            super::super::enum_values::checks::canonical_display(element, arena)?
                        }
                    })
                    .map_err(|_| arena_full())?;
                start = lexer.token_end();
            }
            if token == Tok::Eof {
                break;
            }
        }
        return Ok(Some(display.as_str()));
    }
    {
        let table = slot.map(|slot| storage.table_def(slot, transaction));
        let context = super::deparse::Context {
            storage,
            txid: transaction,
            table: table.as_deref(),
            parameters: &[],
            joined: &[],
            pretty,
        };
        let destination = match destination {
            Some(type_oid) => super::deparse::Destination::Assignment(type_oid),
            None => super::deparse::Destination::Condition,
        };
        if let Some(deparsed) = super::deparse::deparse(&context, source, destination, arena)? {
            return Ok(Some(deparsed));
        }
    }
    let display = super::super::enum_values::checks::canonical_display(source, arena)?;
    let expression = parser::parse_expr(source, arena)?;
    Ok(Some(
        if pretty && matches!(expression, Expr::Binary { .. }) {
            &display[1..display.len() - 1]
        } else {
            display
        },
    ))
}

/// A column default is assigned to its column's declared type: a table
/// column's stored type, or a view column's described output type.
fn default_destination(
    storage: &Storage,
    transaction: u32,
    table: &SynthTable<'_>,
    row: &[Datum<'_>],
    arena: &Arena,
) -> Result<Kind<'static>, SqlError> {
    let field = |name: &str| {
        table
            .def
            .column_index(name)
            .map(|index| super::view_rows::integer(row[index]))
            .transpose()
    };
    let (Some(relation), Some(number)) = (field("adrelid")?, field("adnum")?) else {
        return Err(unresolved_default());
    };
    let attribute = usize::try_from(number)
        .ok()
        .and_then(|number| number.checked_sub(1))
        .ok_or_else(unresolved_default)?;
    if let Some(slot) = (0..storage.table_count()).find(|&slot| {
        storage.table_slot_visible_to(slot, transaction)
            && super::user_table_oid(storage, slot) == relation
    }) {
        let definition = storage.table_def(slot, transaction);
        let column = u16::try_from(number)
            .ok()
            .and_then(|number| definition.attribute_column(number))
            .ok_or_else(unresolved_default)?;
        return Ok(Kind::Assignment(super::catalog_column_type_oid(
            storage,
            &definition.columns()[column],
            transaction,
        )?));
    }
    let view = storage
        .views_visible_to(transaction)
        .find_map(|(slot, view)| (super::view_oid(storage, slot) == relation).then_some(view))
        .ok_or_else(unresolved_default)?;
    let mut columns = [crate::sql::types::ColDesc::new("", 0, 0); crate::sql::exec::MAX_PROJ];
    let count = super::describe_view(&view, transaction, arena, &mut columns)?;
    columns[..count]
        .get(attribute)
        .map(|column| Kind::Assignment(column.type_oid))
        .ok_or_else(unresolved_default)
}

/// A domain default is assigned to the domain's base type.
fn domain_default_destination(
    table: &SynthTable<'_>,
    row: &[Datum<'_>],
) -> Result<Kind<'static>, SqlError> {
    let base = table
        .def
        .column_index("typbasetype")
        .map(|index| super::view_rows::integer(row[index]))
        .transpose()?
        .filter(|base| *base != 0)
        .ok_or_else(unresolved_default)?;
    Ok(Kind::Assignment(base))
}

fn unresolved_default() -> SqlError {
    sql_err!(
        sqlstate::INTERNAL_ERROR,
        "catalog default does not name a visible column"
    )
}

/// The types of a stored routine's defaulted parameters, in order.
fn parameter_default_destinations<'a>(
    storage: &Storage,
    transaction: u32,
    table: &SynthTable<'_>,
    row: &[Datum<'_>],
    arena: &'a Arena,
) -> Result<Kind<'a>, SqlError> {
    let Some(index) = table.def.column_index("oid") else {
        return Ok(Kind::List);
    };
    let oid = super::view_rows::integer(row[index])?;
    let Some(slot) = storage.routine_slot_by_oid(oid, transaction) else {
        return Ok(Kind::List);
    };
    let routine = storage.routine_for(slot, transaction);
    let mut types = crate::mem::arena::ArenaList::new(arena);
    for parameter in routine.parameters() {
        if parameter.mode.default().is_none() {
            continue;
        }
        let Some(type_oid) =
            storage.routine_type_oid(parameter.ctype, parameter.user_type, transaction)
        else {
            return Ok(Kind::List);
        };
        types.push(type_oid).map_err(|_| arena_full())?;
    }
    Ok(Kind::AssignmentList(types.as_slice()))
}
