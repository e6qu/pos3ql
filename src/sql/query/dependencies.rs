//! Name-resolution pass for durable stored-query dependencies.
//!
//! Views keep their SQL text for execution, but dependency decisions must not
//! reparse that text after catalog objects have moved. This pass runs once at
//! CREATE time and records the exact relation, user type, and sequence slots
//! selected under the creator's search path.

use crate::mem::arena::Arena;
use crate::sql::ast::{Expr, FrameBound, Select, SelectItem, SetTree, TableRef};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::ColType;
use crate::sql_err;
use crate::storage::{
    DependencyClass, MAX_STORED_QUERY_DEPENDENCIES, PathContext, PathEntry, ResolvedRelation,
    SqlName, Storage, StoredQueryDependencies, StoredQueryDependency,
};

#[derive(Clone, Copy)]
struct CteNames<'a> {
    names: [&'a str; MAX_STORED_QUERY_DEPENDENCIES],
    len: usize,
}

impl<'a> CteNames<'a> {
    const EMPTY: Self = Self {
        names: [""; MAX_STORED_QUERY_DEPENDENCIES],
        len: 0,
    };

    fn contains(&self, name: &str) -> bool {
        self.names[..self.len].contains(&name)
    }

    fn push(&mut self, name: &'a str) -> Result<(), SqlError> {
        if self.len == self.names.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "stored query nests more than {} CTE names",
                self.names.len()
            ));
        }
        self.names[self.len] = name;
        self.len += 1;
        Ok(())
    }
}

pub(super) fn collect(
    sql: &str,
    storage: &Storage,
    txid: u32,
    path: PathContext,
    arena: &Arena,
) -> Result<StoredQueryDependencies, SqlError> {
    let select = crate::sql::parser::parse_query(sql, arena)?;
    let mut dependencies = StoredQueryDependencies::EMPTY;
    collect_select(
        select,
        storage,
        txid,
        &path,
        CteNames::EMPTY,
        &mut dependencies,
    )?;
    Ok(dependencies)
}

fn collect_select<'a>(
    select: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    inherited_ctes: CteNames<'a>,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    let mut visible_ctes = inherited_ctes;
    for cte in select.with {
        // A CTE can reference earlier entries, not itself unless recursive.
        let definition_scope = if cte.recursive {
            let mut scope = visible_ctes;
            scope.push(cte.name)?;
            scope
        } else {
            visible_ctes
        };
        collect_select(
            cte.query,
            storage,
            txid,
            path,
            definition_scope,
            dependencies,
        )?;
        visible_ctes.push(cte.name)?;
    }

    for item in select.items {
        match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                collect_expression(expression, storage, txid, path, visible_ctes, dependencies)?;
            }
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => {}
        }
    }
    for expression in select.distinct_on {
        collect_expression(expression, storage, txid, path, visible_ctes, dependencies)?;
    }
    for expression in select
        .where_clause
        .into_iter()
        .chain(select.group_by.iter().copied())
        .chain(select.having)
        .chain(select.order_by.iter().map(|order| order.expression))
        .chain(select.limit)
        .chain(select.offset)
    {
        collect_expression(expression, storage, txid, path, visible_ctes, dependencies)?;
    }
    if let Some(tree) = select.set_body {
        collect_set_tree(tree, storage, txid, path, visible_ctes, dependencies)?;
    }
    if let Some(from) = select.from {
        collect_table_ref(&from.base, storage, txid, path, visible_ctes, dependencies)?;
        for join in from.joins {
            collect_table_ref(&join.table, storage, txid, path, visible_ctes, dependencies)?;
            if let Some(on) = join.on {
                collect_expression(on, storage, txid, path, visible_ctes, dependencies)?;
            }
        }
    }
    Ok(())
}

fn collect_set_tree<'a>(
    tree: &'a SetTree<'a>,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    ctes: CteNames<'a>,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    match tree {
        SetTree::Select(select) => collect_select(select, storage, txid, path, ctes, dependencies),
        SetTree::Op { left, right, .. } => {
            collect_set_tree(left, storage, txid, path, ctes, dependencies)?;
            collect_set_tree(right, storage, txid, path, ctes, dependencies)
        }
    }
}

fn collect_table_ref<'a>(
    table: &'a TableRef<'a>,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    ctes: CteNames<'a>,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    if let Some(select) = table.subquery {
        collect_select(select, storage, txid, path, ctes, dependencies)?;
    } else if table.func_args.is_none()
        && table.cte.is_none()
        && !(table.schema.is_none() && ctes.contains(table.table))
    {
        match storage.resolve_relation_under(path, table.schema, table.table, txid) {
            Some(ResolvedRelation::Table(slot)) => {
                let definition = storage.table_def(slot, txid);
                dependencies.push(StoredQueryDependency {
                    class: DependencyClass::Table,
                    slot: slot as u16,
                    schema: definition.schema,
                    name: definition.name,
                    referenced_schema: SqlName::parse(table.schema.unwrap_or(""))?,
                    referenced_name: SqlName::parse(table.table)?,
                })?;
            }
            Some(ResolvedRelation::View(slot)) => {
                let view = storage.view(slot);
                dependencies.push(StoredQueryDependency {
                    class: DependencyClass::View,
                    slot: slot as u16,
                    schema: view.schema,
                    name: view.name,
                    referenced_schema: SqlName::parse(table.schema.unwrap_or(""))?,
                    referenced_name: SqlName::parse(table.table)?,
                })?;
            }
            Some(ResolvedRelation::Catalog) | None => {}
        }
    }
    if let Some(arguments) = table.func_args {
        for argument in arguments {
            collect_expression(argument, storage, txid, path, ctes, dependencies)?;
        }
    }
    Ok(())
}

fn collect_expression<'a>(
    expression: &'a Expr<'a>,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    ctes: CteNames<'a>,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    if let Expr::Cast { type_name, .. } = expression {
        collect_type(type_name, storage, txid, path, dependencies)?;
    }
    if let Expr::Call { name, args, .. } = expression
        && matches!(*name, "nextval" | "currval" | "setval")
        && let Some(sequence_name) = args.first().and_then(|argument| regclass_literal(argument))
    {
        collect_sequence(sequence_name, storage, txid, path, dependencies)?;
    }
    let mut child =
        |expression| collect_expression(expression, storage, txid, path, ctes, dependencies);
    match expression {
        Expr::Cast { operand, .. } => child(operand),
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Field { base: operand, .. } => child(operand),
        Expr::Binary { left, right, .. }
        | Expr::Subscript {
            base: left,
            index: right,
        }
        | Expr::AnyAll {
            operand: left,
            array: right,
            ..
        } => {
            child(left)?;
            child(right)
        }
        Expr::Call {
            name,
            args,
            order_by,
            over,
            filter,
            ..
        } => {
            let _ = name;
            for argument in *args {
                child(argument)?;
            }
            for order in *order_by {
                child(order.expression)?;
            }
            if let Some(filter) = filter {
                child(filter)?;
            }
            if let Some(window) = over {
                for expression in window.partition_by {
                    child(expression)?;
                }
                for order in window.order_by {
                    child(order.expression)?;
                }
                if let Some(frame) = window.frame {
                    for bound in [frame.start, frame.end] {
                        if let FrameBound::Preceding(expression)
                        | FrameBound::Following(expression) = bound
                        {
                            child(expression)?;
                        }
                    }
                }
            }
            Ok(())
        }
        Expr::InList { operand, list, .. } => {
            child(operand)?;
            for expression in *list {
                child(expression)?;
            }
            Ok(())
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            child(operand)?;
            child(low)?;
            child(high)
        }
        Expr::Like {
            operand,
            pattern,
            escape,
            ..
        } => {
            child(operand)?;
            child(pattern)?;
            if let Some(escape) = escape {
                child(escape)?;
            }
            Ok(())
        }
        Expr::Match {
            operand, pattern, ..
        } => {
            child(operand)?;
            child(pattern)
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
            ..
        } => {
            if let Some(operand) = operand {
                child(operand)?;
            }
            for (condition, result) in *whens {
                child(condition)?;
                child(result)?;
            }
            if let Some(otherwise) = otherwise {
                child(otherwise)?;
            }
            Ok(())
        }
        Expr::Subquery(select) | Expr::Exists(select) | Expr::ArraySubquery(select) => {
            collect_select(select, storage, txid, path, ctes, dependencies)
        }
        Expr::InSubquery {
            operand, select, ..
        } => {
            child(operand)?;
            collect_select(select, storage, txid, path, ctes, dependencies)
        }
        Expr::Array(items) => {
            for expression in *items {
                child(expression)?;
            }
            Ok(())
        }
        Expr::Slice { base, lower, upper } => {
            child(base)?;
            if let Some(lower) = lower {
                child(lower)?;
            }
            if let Some(upper) = upper {
                child(upper)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn collect_type(
    type_name: &str,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    let bare = type_name.strip_suffix("[]").unwrap_or(type_name);
    let (referenced_schema, referenced_name) = bare
        .split_once('.')
        .map_or(("", bare), |(schema, name)| (schema, name));
    let referenced_schema = SqlName::parse(referenced_schema)?;
    let referenced_name = SqlName::parse(referenced_name)?;
    let record = |class, slot: usize, schema, name, dependencies: &mut StoredQueryDependencies| {
        dependencies.push(StoredQueryDependency {
            class,
            slot: slot as u16,
            schema,
            name,
            referenced_schema,
            referenced_name,
        })
    };
    if let Some((schema, name)) = bare.split_once('.') {
        if let Some(slot) = storage.domain_slot(schema, name, txid) {
            let definition = storage.domain(slot);
            return record(
                DependencyClass::Domain,
                slot,
                definition.schema,
                definition.name,
                dependencies,
            );
        }
        if let Some(slot) = storage.enum_slot(schema, name, txid) {
            let definition = storage.enum_def(slot);
            return record(
                DependencyClass::Enum,
                slot,
                definition.schema,
                definition.name,
                dependencies,
            );
        }
        return Ok(());
    }
    for entry in path.entries() {
        match entry {
            PathEntry::Catalog if ColType::from_sql_name(bare).is_some() => return Ok(()),
            PathEntry::Catalog => {}
            PathEntry::Schema(schema_slot) => {
                let schema = storage.schema_def(*schema_slot as usize).name;
                if let Some(slot) = storage.domain_slot(schema.as_str(), bare, txid) {
                    let definition = storage.domain(slot);
                    return record(
                        DependencyClass::Domain,
                        slot,
                        definition.schema,
                        definition.name,
                        dependencies,
                    );
                }
                if let Some(slot) = storage.enum_slot(schema.as_str(), bare, txid) {
                    let definition = storage.enum_def(slot);
                    return record(
                        DependencyClass::Enum,
                        slot,
                        definition.schema,
                        definition.name,
                        dependencies,
                    );
                }
            }
        }
    }
    Ok(())
}

fn regclass_literal<'a>(expression: &'a Expr<'a>) -> Option<&'a str> {
    match expression {
        Expr::Str(value) => Some(value),
        Expr::Cast { operand, .. } => regclass_literal(operand),
        _ => None,
    }
}

fn collect_sequence(
    sequence_name: &str,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    let (schema, name) = sequence_name
        .rsplit_once('.')
        .map_or((None, sequence_name), |(schema, name)| (Some(schema), name));
    let slot = if let Some(schema) = schema {
        storage.sequence_slot(schema, name, txid)
    } else {
        path.entries().iter().find_map(|entry| match entry {
            PathEntry::Schema(schema_slot) => {
                let schema = storage.schema_def(*schema_slot as usize).name;
                storage.sequence_slot(schema.as_str(), name, txid)
            }
            PathEntry::Catalog => None,
        })
    };
    if let Some(slot) = slot {
        let sequence = storage.sequence(slot);
        dependencies.push(StoredQueryDependency {
            class: DependencyClass::Sequence,
            slot: slot as u16,
            schema: sequence.schema,
            name: sequence.name,
            referenced_schema: SqlName::parse(schema.unwrap_or(""))?,
            referenced_name: SqlName::parse(name)?,
        })?;
    }
    Ok(())
}
