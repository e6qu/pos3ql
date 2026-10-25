//! Catalog expressions retain parser-bound column ordinals across renames.

use super::{Arena, Datum, SqlError, Storage, SynthTable, arena_full};
use crate::mem::arena::ArenaString;
use crate::sql::{ast::Expr, eval::sqlstate, lexer::{Lexer, Tok}, parser};
use crate::storage::TableDef;
use crate::sql_err;
use core::fmt::Write;

const HEADER: &str = "pos3ql-catalog-expression-v1\n";

#[derive(Clone, Copy)]
struct Site { start: usize, end: usize, column: usize }

#[derive(Clone, Copy)]
enum Kind { Expression, List, Partition, Query }

pub(super) fn current<'a>(storage: &Storage, transaction: u32, table: SynthTable<'a>, arena: &'a Arena)
    -> Result<SynthTable<'a>, SqlError> {
    use crate::sql::types::ColType;
    if !table.def.columns().iter().any(|column| column.ctype == ColType::PgNodeTree)
        || !table.rows.iter().any(|values| table.def.columns().iter().zip(*values)
            .any(|(column, value)| column.ctype == ColType::PgNodeTree && !value.is_null())) {
        return Ok(table);
    }
    let rows = arena.alloc_slice_with(table.rows.len(), |_| &[] as &[Datum]).map_err(|_| arena_full())?;
    for (target, values) in rows.iter_mut().zip(table.rows) {
        let row = arena.alloc_slice_copy(values).map_err(|_| arena_full())?;
        for (position, column) in table.def.columns().iter().enumerate() {
            if column.ctype != ColType::PgNodeTree || row[position].is_null() { continue; }
            let source = super::view_rows::string(row[position])?;
            let (relation, kind) = match (table.def.name.as_str(), column.name.as_str()) {
                ("pg_constraint", "conbin") => (Some("conrelid"), Kind::Expression),
                ("pg_attrdef", "adbin") => (Some("adrelid"), Kind::Expression),
                ("pg_index", "indpred") => (Some("indrelid"), Kind::Expression),
                ("pg_index", "indexprs") => (Some("indrelid"), Kind::List),
                ("pg_statistic_ext", "stxexprs") => (Some("stxrelid"), Kind::List),
                ("pg_policy", "polqual" | "polwithcheck") => (Some("polrelid"), Kind::Expression),
                ("pg_publication_rel", "prqual") => (Some("prrelid"), Kind::Expression),
                ("pg_trigger", "tgqual") => (Some("tgrelid"), Kind::Expression),
                ("pg_partitioned_table", "partexprs") => (Some("partrelid"), Kind::List),
                ("pg_class", "relpartbound") => (None, Kind::Partition),
                ("pg_type", "typdefaultbin") => (None, Kind::Expression),
                ("pg_proc", "proargdefaults") => (None, Kind::List),
                ("pg_proc", "prosqlbody") => (None, Kind::Query),
                _ => return Err(sql_err!(sqlstate::FEATURE_NOT_SUPPORTED, "catalog expression {}.{} is not supported",
                    table.def.name.as_str(), column.name.as_str())),
            };
            let relation = relation.map(|field| super::view_rows::integer(row[table.def.column_index(field)
                .expect("catalog expression relation field exists")])).transpose()?.unwrap_or(0);
            let slot = (0..storage.table_count()).find(|&slot| storage.table_slot_visible_to(slot, transaction)
                && super::table_oid(storage, slot) == relation);
            row[position] = Datum::Text(if let Some(slot) = slot {
                let definition = storage.table_def(slot, transaction);
                bind(source, kind, Some(&definition), arena)?
            } else { bind(source, kind, None, arena)? });
        }
        *target = row;
    }
    Ok(SynthTable { rows, ..table })
}

fn reference_start(source: &str, site: parser::ColumnSource<'_>, arena: &Arena) -> Result<usize, SqlError> {
    if site.qualifier_parts == 0 { return Ok(site.start); }
    let mut lexer = Lexer::new(source, arena);
    let mut starts = [0; 5];
    let mut count = 0;
    loop {
        let token = lexer.next_token().map_err(|error| sql_err!(error.kind.sqlstate(), "invalid catalog expression: {}", error.message))?;
        if token == Tok::Eof { break; }
        starts[count % starts.len()] = lexer.token_start();
        count += 1;
        if lexer.token_start() == site.start {
            return count.checked_sub(1 + 2 * site.qualifier_parts).map(|position| starts[position % starts.len()])
                .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "catalog column source qualifier is incomplete"));
        }
    }
    Err(sql_err!(sqlstate::INTERNAL_ERROR, "catalog column source is missing"))
}

fn bind<'a>(source: &str, kind: Kind, table: Option<&TableDef>, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let mut output = ArenaString::new(arena);
    output.write_str(HEADER).map_err(|_| arena_full())?;
    if matches!(kind, Kind::Partition | Kind::Query) {
        write!(output, "{}\n0\n\n{source}", if matches!(kind, Kind::Partition) { "partition" } else { "query" }).map_err(|_| arena_full())?;
        return Ok(output.as_str());
    }
    let source = arena.alloc_str(source).map_err(|_| arena_full())?;
    let (parsed_source, columns, prefix) = match kind {
        Kind::Expression => {
            let (_, columns) = parser::parse_expr_with_column_sources(source, arena)?;
            (source, columns, 0)
        }
        Kind::List => {
            let query = arena.alloc_str_display(format_args!("SELECT {source}")).map_err(|_| arena_full())?;
            let (_, columns) = parser::parse_query_with_column_sources(query, arena)?;
            (query, columns.columns, 7)
        }
        Kind::Partition | Kind::Query => unreachable!("non-expression nodes have no scalar column references"),
    };
    let sites = arena.alloc_slice_with(if table.is_some() { columns.len() } else { 0 }, |_| Site { start: 0, end: 0, column: 0 })
        .map_err(|_| arena_full())?;
    if let Some(table) = table {
        for (target, column) in sites.iter_mut().zip(columns) {
            let number = table.column_index(column.name).ok_or_else(|| sql_err!(sqlstate::UNDEFINED_COLUMN,
                "column \"{}\" does not exist", column.name))?;
            *target = Site { start: reference_start(parsed_source, *column, arena)? - prefix,
                end: column.end - prefix, column: number + 1 };
        }
    }
    sites.sort_unstable_by_key(|site| (site.start, site.end, site.column));
    let mut count = 0;
    for at in 0..sites.len() {
        if count == 0 || (sites[at].start, sites[at].end) != (sites[count-1].start, sites[count-1].end) {
            sites[count] = sites[at]; count += 1;
        } else if sites[at].column != sites[count-1].column {
            return Err(sql_err!(sqlstate::INTERNAL_ERROR, "catalog column source has conflicting bindings"));
        }
    }
    writeln!(output, "{}\n{count}", if matches!(kind, Kind::List) { "list" } else { "expression" }).map_err(|_| arena_full())?;
    for site in &sites[..count] { writeln!(output, "{},{},{}", site.start, site.end, site.column).map_err(|_| arena_full())?; }
    write!(output, "\n{source}").map_err(|_| arena_full())?;
    Ok(output.as_str())
}

pub(super) fn deparse<'a>(storage: &Storage, transaction: u32, node: &str, relation: i32, pretty: bool, arena: &'a Arena)
    -> Result<Option<&'a str>, SqlError> {
    let corrupt = || sql_err!(sqlstate::INTERNAL_ERROR, "invalid stored catalog expression node");
    let body = node.strip_prefix(HEADER).ok_or_else(|| sql_err!(sqlstate::FEATURE_NOT_SUPPORTED,
        "cannot accept a value of type pg_node_tree"))?;
    let (kind, rest) = body.split_once('\n').ok_or_else(corrupt)?;
    if !matches!(kind, "expression" | "list" | "partition" | "query") { return Err(corrupt()); }
    let (count, mut rest) = rest.split_once('\n').ok_or_else(corrupt)?;
    let count = count.parse::<usize>().map_err(|_| corrupt())?;
    if count > rest.len() { return Err(corrupt()); }
    if matches!(kind, "partition" | "query") && count != 0 { return Err(corrupt()); }
    let sites = arena.alloc_slice_with(count, |_| Site { start: 0, end: 0, column: 0 }).map_err(|_| arena_full())?;
    for site in sites.iter_mut() {
        let (line, next) = rest.split_once('\n').ok_or_else(corrupt)?;
        let mut fields = line.split(',');
        site.start = fields.next().ok_or_else(corrupt)?.parse().map_err(|_| corrupt())?;
        site.end = fields.next().ok_or_else(corrupt)?.parse().map_err(|_| corrupt())?;
        site.column = fields.next().ok_or_else(corrupt)?.parse().map_err(|_| corrupt())?;
        if fields.next().is_some() { return Err(corrupt()); }
        rest = next;
    }
    let source = rest.strip_prefix('\n').ok_or_else(corrupt)?;
    if kind == "query" { return Err(sql_err!(sqlstate::FEATURE_NOT_SUPPORTED, "SQL routine body is not a scalar expression")); }
    if count != 0 && relation == 0 {
        return Err(sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "expression contains variables of more than one relation"));
    }
    if relation != 0 && !super::relation_oid_is_visible(storage, transaction, relation) { return Ok(None); }
    let slot = (0..storage.table_count()).find(|&slot| storage.table_slot_visible_to(slot, transaction)
        && super::table_oid(storage, slot) == relation);
    let mut output = ArenaString::new(arena);
    let mut copied = 0;
    for site in sites {
        if site.start < copied || site.end <= site.start || !source.is_char_boundary(site.start)
            || !source.is_char_boundary(site.end) || site.column == 0 { return Err(corrupt()); }
        let slot = slot.ok_or_else(|| sql_err!(sqlstate::FEATURE_NOT_SUPPORTED, "catalog expression requires a table relation"))?;
        let table = storage.table_def(slot, transaction);
        let column = table.columns().get(site.column - 1).ok_or_else(corrupt)?;
        output.write_str(&source[copied..site.start]).map_err(|_| arena_full())?;
        let mut name = crate::util::StackStr::<130>::new();
        super::write_identifier(&mut name, column.name.as_str());
        if name.is_truncated() { return Err(corrupt()); }
        output.write_str(name.as_str()).map_err(|_| arena_full())?;
        copied = site.end;
    }
    output.write_str(&source[copied..]).map_err(|_| arena_full())?;
    let source = output.as_str();
    if kind == "partition" { return Ok(Some(source)); }
    if kind == "list" {
        // The query parser has already established the list grammar. Split
        // only top-level commas, keeping strings and nested calls intact.
        let mut lexer = Lexer::new(source, arena);
        let mut depth = 0;
        let mut start = 0;
        let mut display = ArenaString::new(arena);
        loop {
            let token = lexer.next_token().map_err(|_| corrupt())?;
            if matches!(token, Tok::Op("(" | "[")) { depth += 1; }
            if matches!(token, Tok::Op(")" | "]")) { depth -= 1; }
            if token == Tok::Eof || (token == Tok::Op(",") && depth == 0) {
                if start != 0 { display.write_str(", ").map_err(|_| arena_full())?; }
                display.write_str(super::super::enum_values::checks::canonical_display(&source[start..lexer.token_start()], arena)?)
                    .map_err(|_| arena_full())?;
                start = lexer.token_end();
            }
            if token == Tok::Eof { break; }
        }
        return Ok(Some(display.as_str()));
    }
    let display = super::super::enum_values::checks::canonical_display(source, arena)?;
    let expression = parser::parse_expr(source, arena)?;
    Ok(Some(if pretty && matches!(expression, Expr::Binary { .. }) { &display[1..display.len()-1] } else { display }))
}
