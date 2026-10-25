//! Constraint reports join selected base rows and current permission inquiries.
//! Source: https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/information_schema.sql

use super::information::owner_is_enabled;
use super::view_rows::{CatalogRow, Index, NamespaceJoin, Output, boolean, integer};
use super::views::{Base, Read, columns};
use super::{Arena, Datum, SqlError, Storage, SynthTable, text};
use crate::sql::eval::{CatalogAccess, PrivilegeRelation, sqlstate};
use crate::sql_err;

pub(super) fn tables<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let constraints = read.base(storage, Base::Constraint, transaction, arena)?;
    let classes = Index::oid(read.base(storage, Base::Class, transaction, arena)?, arena)?;
    let namespaces = Index::oid(read.base(storage, Base::Namespace, transaction, arena)?, arena)?;
    let index_rows = read.base(storage, Base::Index, transaction, arena)?;
    let [index_oid, nulls_not_distinct] = columns(&index_rows, ["indexrelid", "indnullsnotdistinct"]);
    let indexes = Index::new(index_rows, arena, |values| {
        Ok(Some((integer(values[index_oid])? as u32, 0)))
    })?;
    let relations = NamespaceJoin::new(&classes, &namespaces, "relnamespace")?;
    let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
    let database = storage.current_database_name(transaction);
    let database = text(database.as_str(), arena)?;
    let mut output = Output::new(arena);
    for values in constraints.rows {
        let constraint = CatalogRow { definition: constraints.def, values };
        let kind = constraint.character("contype")?;
        if matches!(kind, b't' | b'x') { continue; }
        let kind_name = match kind {
            b'c' | b'n' => Datum::Text("CHECK"),
            b'f' => Datum::Text("FOREIGN KEY"),
            b'p' => Datum::Text("PRIMARY KEY"),
            b'u' => Datum::Text("UNIQUE"),
            _ => Datum::Null,
        };
        for (class, namespace) in relations.joined(Some((constraint.integer("conrelid")? as u32, 0))) {
            if !matches!(class.character("relkind")?, b'r' | b'p')
                || catalog.is_other_temporary_namespace(namespace.integer("oid")?) {
                continue;
            }
            let relation = PrivilegeRelation::Oid(class.integer("oid")?);
            if !owner_is_enabled(storage, transaction, class.integer("relowner")?)
                && catalog.has_table_privilege(None, relation,
                    "INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER")? != Some(true)
                && catalog.has_any_column_privilege(None, relation,
                    "INSERT, UPDATE, REFERENCES")? != Some(true) {
                continue;
            }
            let nulls_distinct = if kind == b'u' {
                let mut selected = indexes.matching((constraint.integer("conindid")? as u32, 0));
                let index = selected.next();
                if selected.next().is_some() {
                    return Err(sql_err!(sqlstate::CARDINALITY_VIOLATION,
                        "more than one row returned by a subquery used as an expression"));
                }
                // CASE treats an absent index's scalar NULL as the ELSE branch.
                Datum::Text(if index.map(|values| boolean(values[nulls_not_distinct])).transpose()? == Some(false) { "YES" } else { "NO" })
            } else { Datum::Null };
            for names in namespaces.matching((constraint.integer("connamespace")? as u32, 0)) {
                let names = CatalogRow { definition: namespaces.table.def, values: names };
                output.push(&[
                    database, names.get("nspname")?, constraint.get("conname")?,
                    database, namespace.get("nspname")?, class.get("relname")?, kind_name,
                    Datum::Text(if constraint.boolean("condeferrable")? { "YES" } else { "NO" }),
                    Datum::Text(if constraint.boolean("condeferred")? { "YES" } else { "NO" }),
                    Datum::Text(if constraint.boolean("conenforced")? { "YES" } else { "NO" }),
                    nulls_distinct,
                ])?;
            }
        }
    }
    output.finish("table_constraints", true)
}

pub(super) fn table_usage<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let constraints = read.base(storage, Base::Constraint, transaction, arena)?;
    let classes = Index::oid(read.base(storage, Base::Class, transaction, arena)?, arena)?;
    let namespaces = Index::oid(read.base(storage, Base::Namespace, transaction, arena)?, arena)?;
    let relations = NamespaceJoin::new(&classes, &namespaces, "relnamespace")?;
    let database = storage.current_database_name(transaction);
    let database = text(database.as_str(), arena)?;
    let mut output = Output::new(arena);
    for values in constraints.rows {
        let constraint = CatalogRow { definition: constraints.def, values };
        let relation = match constraint.character("contype")? {
            b'f' => constraint.integer("confrelid")?,
            b'p' | b'u' => constraint.integer("conrelid")?,
            _ => continue,
        };
        for (class, namespace) in relations.joined(Some((relation as u32, 0))) {
            if !matches!(class.character("relkind")?, b'r' | b'p')
                || !owner_is_enabled(storage, transaction, class.integer("relowner")?) {
                continue;
            }
            for names in namespaces.matching((constraint.integer("connamespace")? as u32, 0)) {
                let names = CatalogRow { definition: namespaces.table.def, values: names };
                output.push(&[database, namespace.get("nspname")?, class.get("relname")?,
                    database, names.get("nspname")?, constraint.get("conname")?])?;
            }
        }
    }
    output.finish("constraint_table_usage", true)
}
