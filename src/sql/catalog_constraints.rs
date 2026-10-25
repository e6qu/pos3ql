//! Constraint reports join selected base rows and current permission inquiries.
//! Source: https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/information_schema.sql

use super::information::owner_is_enabled;
use super::view_rows::{CatalogRow, Index, NamespaceJoin, Output, boolean, integer};
use super::views::{Base, Read, columns};
use super::{Arena, Datum, SqlError, Storage, SynthTable, text};
use crate::sql::eval::{
    CatalogAccess, PrivilegeColumn, PrivilegeRelation, PrivilegeObject, sqlstate,
};
use crate::sql_err;

pub(super) fn tables<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let constraints = read.base(storage, Base::Constraint, transaction, arena)?;
    let classes = Index::oid(read.base(storage, Base::Class, transaction, arena)?, arena)?;
    let namespaces = Index::oid(
        read.base(storage, Base::Namespace, transaction, arena)?,
        arena,
    )?;
    let index_rows = read.base(storage, Base::Index, transaction, arena)?;
    let [index_oid, nulls_not_distinct] =
        columns(&index_rows, ["indexrelid", "indnullsnotdistinct"]);
    let indexes = Index::new(index_rows, arena, |values| {
        Ok(Some((integer(values[index_oid])? as u32, 0)))
    })?;
    let relations = NamespaceJoin::new(&classes, &namespaces, "relnamespace")?;
    let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
    let database = storage.current_database_name(transaction);
    let database = text(database.as_str(), arena)?;
    let mut output = Output::new(arena);
    for values in constraints.rows {
        let constraint = CatalogRow {
            definition: constraints.def,
            values,
        };
        let kind = constraint.character("contype")?;
        if matches!(kind, b't' | b'x') {
            continue;
        }
        let kind_name = match kind {
            b'c' | b'n' => Datum::Text("CHECK"),
            b'f' => Datum::Text("FOREIGN KEY"),
            b'p' => Datum::Text("PRIMARY KEY"),
            b'u' => Datum::Text("UNIQUE"),
            _ => Datum::Null,
        };
        for (class, namespace) in
            relations.joined(Some((constraint.integer("conrelid")? as u32, 0)))
        {
            if !matches!(class.character("relkind")?, b'r' | b'p')
                || catalog.is_other_temporary_namespace(namespace.integer("oid")?)
            {
                continue;
            }
            let relation = PrivilegeRelation::Oid(class.integer("oid")?);
            if !owner_is_enabled(storage, transaction, class.integer("relowner")?)
                && catalog.has_table_privilege(
                    None,
                    relation,
                    "INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER",
                )? != Some(true)
                && catalog.has_any_column_privilege(None, relation, "INSERT, UPDATE, REFERENCES")?
                    != Some(true)
            {
                continue;
            }
            let nulls_distinct = if kind == b'u' {
                let mut selected = indexes.matching((constraint.integer("conindid")? as u32, 0));
                let index = selected.next();
                if selected.next().is_some() {
                    return Err(sql_err!(
                        sqlstate::CARDINALITY_VIOLATION,
                        "more than one row returned by a subquery used as an expression"
                    ));
                }
                // CASE treats an absent index's scalar NULL as the ELSE branch.
                Datum::Text(
                    if index
                        .map(|values| boolean(values[nulls_not_distinct]))
                        .transpose()?
                        == Some(false)
                    {
                        "YES"
                    } else {
                        "NO"
                    },
                )
            } else {
                Datum::Null
            };
            for names in namespaces.matching((constraint.integer("connamespace")? as u32, 0)) {
                let names = CatalogRow {
                    definition: namespaces.table.def,
                    values: names,
                };
                output.push(&[
                    database,
                    names.get("nspname")?,
                    constraint.get("conname")?,
                    database,
                    namespace.get("nspname")?,
                    class.get("relname")?,
                    kind_name,
                    Datum::Text(if constraint.boolean("condeferrable")? {
                        "YES"
                    } else {
                        "NO"
                    }),
                    Datum::Text(if constraint.boolean("condeferred")? {
                        "YES"
                    } else {
                        "NO"
                    }),
                    Datum::Text(if constraint.boolean("conenforced")? {
                        "YES"
                    } else {
                        "NO"
                    }),
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
    let namespaces = Index::oid(
        read.base(storage, Base::Namespace, transaction, arena)?,
        arena,
    )?;
    let relations = NamespaceJoin::new(&classes, &namespaces, "relnamespace")?;
    let database = storage.current_database_name(transaction);
    let database = text(database.as_str(), arena)?;
    let mut output = Output::new(arena);
    for values in constraints.rows {
        let constraint = CatalogRow {
            definition: constraints.def,
            values,
        };
        let relation = match constraint.character("contype")? {
            b'f' => constraint.integer("confrelid")?,
            b'p' | b'u' => constraint.integer("conrelid")?,
            _ => continue,
        };
        for (class, namespace) in relations.joined(Some((relation as u32, 0))) {
            if !matches!(class.character("relkind")?, b'r' | b'p')
                || !owner_is_enabled(storage, transaction, class.integer("relowner")?)
            {
                continue;
            }
            for names in namespaces.matching((constraint.integer("connamespace")? as u32, 0)) {
                let names = CatalogRow {
                    definition: namespaces.table.def,
                    values: names,
                };
                output.push(&[
                    database,
                    namespace.get("nspname")?,
                    class.get("relname")?,
                    database,
                    names.get("nspname")?,
                    constraint.get("conname")?,
                ])?;
            }
        }
    }
    output.finish("constraint_table_usage", true)
}

pub(super) fn keys<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let constraints = read.base(storage, Base::Constraint, transaction, arena)?;
    let classes = Index::oid(read.base(storage, Base::Class, transaction, arena)?, arena)?;
    let namespaces = Index::oid(
        read.base(storage, Base::Namespace, transaction, arena)?,
        arena,
    )?;
    let attributes = read.base(storage, Base::Attribute, transaction, arena)?;
    let [relation, number] = columns(&attributes, ["attrelid", "attnum"]);
    let attributes = Index::new(attributes, arena, |values| {
        Ok(Some((
            integer(values[relation])? as u32,
            integer(values[number])?,
        )))
    })?;
    let indexes = read.base(storage, Base::Index, transaction, arena)?;
    let [index_oid] = columns(&indexes, ["indexrelid"]);
    let indexes = Index::new(indexes, arena, |values| {
        Ok(Some((integer(values[index_oid])? as u32, 0)))
    })?;
    let relations = NamespaceJoin::new(&classes, &namespaces, "relnamespace")?;
    let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
    let database = storage.current_database_name(transaction);
    let database = text(database.as_str(), arena)?;
    let mut output = Output::new(arena);
    for values in constraints.rows {
        let constraint = CatalogRow {
            definition: constraints.def,
            values,
        };
        let kind = constraint.character("contype")?;
        if !matches!(kind, b'p' | b'u' | b'f') {
            continue;
        }
        let key = constraint.integers("conkey", arena)?;
        let referenced = constraint.integers("confkey", arena)?;
        for (class, namespace) in
            relations.joined(Some((constraint.integer("conrelid")? as u32, 0)))
        {
            if !matches!(class.character("relkind")?, b'r' | b'p')
                || catalog.is_other_temporary_namespace(namespace.integer("oid")?)
            {
                continue;
            }
            let relation_oid = class.integer("oid")?;
            for (ordinal, &number) in key.iter().enumerate() {
                for values in attributes.matching((relation_oid as u32, number)) {
                    let attribute = CatalogRow {
                        definition: attributes.table.def,
                        values,
                    };
                    if attribute.boolean("attisdropped")? {
                        continue;
                    }
                    let column = i16::try_from(number).map_err(|_| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "catalog attribute number is out of range"
                        )
                    })?;
                    if !owner_is_enabled(storage, transaction, class.integer("relowner")?)
                        && catalog.has_column_privilege(
                            None,
                            PrivilegeRelation::Oid(relation_oid),
                            PrivilegeColumn::Number(column),
                            "SELECT, INSERT, UPDATE, REFERENCES",
                        )? != Some(true)
                    {
                        continue;
                    }
                    let mut parent_position = Datum::Null;
                    if kind == b'f'
                        && let Some(&parent_column) = referenced.get(ordinal)
                    {
                        for values in indexes.matching((constraint.integer("conindid")? as u32, 0))
                        {
                            let index = CatalogRow {
                                definition: indexes.table.def,
                                values,
                            };
                            for (position, &indexed_column) in
                                index.integers("indkey", arena)?.iter().enumerate()
                            {
                                if indexed_column == parent_column {
                                    if !parent_position.is_null() {
                                        return Err(sql_err!(
                                            sqlstate::CARDINALITY_VIOLATION,
                                            "more than one row returned by a subquery used as an expression"
                                        ));
                                    }
                                    parent_position = Datum::Int4(position as i32 + 1);
                                }
                            }
                        }
                    }
                    for names in
                        namespaces.matching((constraint.integer("connamespace")? as u32, 0))
                    {
                        let names = CatalogRow {
                            definition: namespaces.table.def,
                            values: names,
                        };
                        output.push(&[
                            database,
                            names.get("nspname")?,
                            constraint.get("conname")?,
                            database,
                            namespace.get("nspname")?,
                            class.get("relname")?,
                            attribute.get("attname")?,
                            Datum::Int4(ordinal as i32 + 1),
                            parent_position,
                        ])?;
                    }
                }
            }
        }
    }
    output.finish("key_column_usage", true)
}

pub(super) fn column_usage<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let constraints = read.base(storage, Base::Constraint, transaction, arena)?;
    let classes = Index::oid(read.base(storage, Base::Class, transaction, arena)?, arena)?;
    let namespaces = Index::oid(
        read.base(storage, Base::Namespace, transaction, arena)?,
        arena,
    )?;
    let attributes = read.base(storage, Base::Attribute, transaction, arena)?;
    let [relation] = columns(&attributes, ["attrelid"]);
    let attributes = Index::new(attributes, arena, |values| {
        Ok(Some((integer(values[relation])? as u32, 0)))
    })?;
    let dependencies = read.base(storage, Base::Dependencies, transaction, arena)?;
    let [class, object, referenced_class] =
        columns(&dependencies, ["classid", "objid", "refclassid"]);
    let dependencies = Index::new(dependencies, arena, |values| {
        (integer(values[class])? == super::PG_CONSTRAINT_OID
            && integer(values[referenced_class])? == super::PG_CLASS_OID)
            .then(|| integer(values[object]).map(|oid| (oid as u32, 0)))
            .transpose()
    })?;
    let relations = NamespaceJoin::new(&classes, &namespaces, "relnamespace")?;
    let database = storage.current_database_name(transaction);
    let database = text(database.as_str(), arena)?;
    let mut output = Output::new(arena);
    // DISTINCT belongs to each CHECK/NOT NULL branch. Equal foreign-key
    // names on different tables must survive the outer UNION ALL.
    let mut checks = Output::new(arena);
    let mut not_nulls = Output::new(arena);
    for values in constraints.rows {
        let constraint = CatalogRow {
            definition: constraints.def,
            values,
        };
        let kind = constraint.character("contype")?;
        if !matches!(kind, b'c' | b'n' | b'p' | b'u' | b'f') {
            continue;
        }
        let key = constraint.integers(if kind == b'f' { "confkey" } else { "conkey" }, arena)?;
        let relation = constraint.integer(if kind == b'f' {
            "confrelid"
        } else {
            "conrelid"
        })?;
        let mut append = |relation, number: Option<i32>| -> Result<(), SqlError> {
            for (class, namespace) in relations.joined(Some((relation as u32, 0))) {
                if !matches!(class.character("relkind")?, b'r' | b'p')
                    || !owner_is_enabled(storage, transaction, class.integer("relowner")?)
                {
                    continue;
                }
                for values in attributes.matching((relation as u32, 0)) {
                    let attribute = CatalogRow {
                        definition: attributes.table.def,
                        values,
                    };
                    let matches = match number {
                        Some(number) => attribute.integer("attnum")? == number,
                        None => key.contains(&attribute.integer("attnum")?),
                    };
                    if !matches || attribute.boolean("attisdropped")? {
                        continue;
                    }
                    for names in
                        namespaces.matching((constraint.integer("connamespace")? as u32, 0))
                    {
                        let names = CatalogRow {
                            definition: namespaces.table.def,
                            values: names,
                        };
                        let row = [
                            database,
                            namespace.get("nspname")?,
                            class.get("relname")?,
                            attribute.get("attname")?,
                            database,
                            names.get("nspname")?,
                            constraint.get("conname")?,
                            class.get("relowner")?,
                        ];
                        match kind {
                            b'c' => checks.push(&row)?,
                            b'n' => not_nulls.push(&row)?,
                            _ => output.push(&row[..7])?,
                        }
                    }
                }
            }
            Ok(())
        };
        match kind {
            b'c' => {
                for values in dependencies.matching((constraint.integer("oid")? as u32, 0)) {
                    let dependency = CatalogRow {
                        definition: dependencies.table.def,
                        values,
                    };
                    append(
                        dependency.integer("refobjid")?,
                        Some(dependency.integer("refobjsubid")?),
                    )?;
                }
            }
            b'n' => {
                if let Some(&number) = key.first() {
                    append(relation, Some(number))?;
                }
            }
            _ => append(relation, None)?,
        }
    }
    for branch in [checks, not_nulls] {
        let rows = branch.rows()?;
        for row in rows.iter() {
            for &value in &row[..7] {
                super::view_rows::string(value)?;
            }
            integer(row[7])?;
        }
        rows.sort_unstable_by(|left, right| {
            left[..7]
                .iter()
                .map(|&value| super::view_rows::string(value).expect("validated name"))
                .cmp(
                    right[..7]
                        .iter()
                        .map(|&value| super::view_rows::string(value).expect("validated name")),
                )
                .then_with(|| {
                    integer(left[7])
                        .expect("validated owner")
                        .cmp(&integer(right[7]).expect("validated owner"))
                })
        });
        for (position, row) in rows.iter().enumerate() {
            if position == 0 || *row != rows[position - 1] {
                output.push(&row[..7])?;
            }
        }
    }
    output.finish("constraint_column_usage", true)
}

pub(super) fn references<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let constraints = read.base(storage, Base::Constraint, transaction, arena)?;
    let parent_constraints = Index::new(constraints, arena, |values| {
        let value = CatalogRow {
            definition: constraints.def,
            values,
        };
        matches!(value.character("contype")?, b'p' | b'u')
            .then(|| Ok((value.integer("oid")? as u32, value.integer("conrelid")?)))
            .transpose()
    })?;
    let classes = Index::oid(read.base(storage, Base::Class, transaction, arena)?, arena)?;
    let namespaces = Index::oid(
        read.base(storage, Base::Namespace, transaction, arena)?,
        arena,
    )?;
    let dependency_rows = read.base(storage, Base::Dependencies, transaction, arena)?;
    let index_dependencies = Index::new(dependency_rows, arena, |values| {
        let value = CatalogRow {
            definition: dependency_rows.def,
            values,
        };
        (value.integer("classid")? == super::PG_CONSTRAINT_OID
            && value.integer("refclassid")? == super::PG_CLASS_OID
            && value.integer("refobjsubid")? == 0)
            .then(|| value.integer("objid").map(|oid| (oid as u32, 0)))
            .transpose()
    })?;
    let constraint_dependencies = Index::new(dependency_rows, arena, |values| {
        let value = CatalogRow {
            definition: dependency_rows.def,
            values,
        };
        (value.integer("classid")? == super::PG_CLASS_OID
            && value.integer("refclassid")? == super::PG_CONSTRAINT_OID
            && value.integer("objsubid")? == 0
            && value.character("deptype")? == b'i')
            .then(|| value.integer("objid").map(|oid| (oid as u32, 0)))
            .transpose()
    })?;
    let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
    let database = storage.current_database_name(transaction);
    let database = text(database.as_str(), arena)?;
    let mut output = Output::new(arena);
    for values in constraints.rows {
        let constraint = CatalogRow {
            definition: constraints.def,
            values,
        };
        if constraint.character("contype")? != b'f' {
            continue;
        }
        for values in classes.matching((constraint.integer("conrelid")? as u32, 0)) {
            let class = CatalogRow {
                definition: classes.table.def,
                values,
            };
            let relation = PrivilegeRelation::Oid(class.integer("oid")?);
            if !owner_is_enabled(storage, transaction, class.integer("relowner")?)
                && catalog.has_table_privilege(
                    None,
                    relation,
                    "INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER",
                )? != Some(true)
                && catalog.has_any_column_privilege(None, relation, "INSERT, UPDATE, REFERENCES")?
                    != Some(true)
            {
                continue;
            }
            for names in namespaces.matching((constraint.integer("connamespace")? as u32, 0)) {
                let names = CatalogRow {
                    definition: namespaces.table.def,
                    values: names,
                };
                for index_link in
                    index_dependencies.left(Some((constraint.integer("oid")? as u32, 0)))
                {
                    let index_link = index_link.map(|values| CatalogRow {
                        definition: dependency_rows.def,
                        values,
                    });
                    let index_key = index_link
                        .map(|row| row.integer("refobjid").map(|oid| (oid as u32, 0)))
                        .transpose()?;
                    for parent_link in constraint_dependencies.left(index_key) {
                        let parent_link = parent_link.map(|values| CatalogRow {
                            definition: dependency_rows.def,
                            values,
                        });
                        let parent_key = parent_link
                            .map(|row| {
                                Ok((
                                    row.integer("refobjid")? as u32,
                                    constraint.integer("confrelid")?,
                                ))
                            })
                            .transpose()?;
                        for parent in parent_constraints.left(parent_key) {
                            let parent = parent.map(|values| CatalogRow {
                                definition: parent_constraints.table.def,
                                values,
                            });
                            let namespace_key = parent
                                .map(|row| row.integer("connamespace").map(|oid| (oid as u32, 0)))
                                .transpose()?;
                            for parent_names in namespaces.left(namespace_key) {
                                let parent_names = parent_names.map(|values| CatalogRow {
                                    definition: namespaces.table.def,
                                    values,
                                });
                                output.push(&[
                                    database,
                                    names.get("nspname")?,
                                    constraint.get("conname")?,
                                    if parent_names.is_some() {
                                        database
                                    } else {
                                        Datum::Null
                                    },
                                    parent_names
                                        .map(|row| row.get("nspname"))
                                        .transpose()?
                                        .unwrap_or(Datum::Null),
                                    parent
                                        .map(|row| row.get("conname"))
                                        .transpose()?
                                        .unwrap_or(Datum::Null),
                                    match constraint.character("confmatchtype")? {
                                        b'f' => Datum::Text("FULL"),
                                        b'p' => Datum::Text("PARTIAL"),
                                        b's' => Datum::Text("NONE"),
                                        _ => Datum::Null,
                                    },
                                    action(constraint.character("confupdtype")?),
                                    action(constraint.character("confdeltype")?),
                                ])?;
                            }
                        }
                    }
                }
            }
        }
    }
    output.finish("referential_constraints", true)
}

pub(super) fn domains<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let constraints = read.base(storage, Base::Constraint, transaction, arena)?;
    let types = Index::oid(read.base(storage, Base::Type, transaction, arena)?, arena)?;
    let namespaces = Index::oid(
        read.base(storage, Base::Namespace, transaction, arena)?,
        arena,
    )?;
    let types = NamespaceJoin::new(&types, &namespaces, "typnamespace")?;
    let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
    let database = storage.current_database_name(transaction);
    let database = text(database.as_str(), arena)?;
    let mut output = Output::new(arena);
    for values in constraints.rows {
        let constraint = CatalogRow {
            definition: constraints.def,
            values,
        };
        for (ty, namespace) in types.joined(Some((constraint.integer("contypid")? as u32, 0))) {
            if !owner_is_enabled(storage, transaction, ty.integer("typowner")?)
                && catalog.has_type_privilege(
                    None,
                    PrivilegeObject::Oid(ty.integer("oid")?),
                    "USAGE",
                )? != Some(true)
            {
                continue;
            }
            for names in namespaces.matching((constraint.integer("connamespace")? as u32, 0)) {
                let names = CatalogRow {
                    definition: namespaces.table.def,
                    values: names,
                };
                output.push(&[
                    database,
                    names.get("nspname")?,
                    constraint.get("conname")?,
                    database,
                    namespace.get("nspname")?,
                    ty.get("typname")?,
                    Datum::Text(if constraint.boolean("condeferrable")? {
                        "YES"
                    } else {
                        "NO"
                    }),
                    Datum::Text(if constraint.boolean("condeferred")? {
                        "YES"
                    } else {
                        "NO"
                    }),
                ])?;
            }
        }
    }
    output.finish("domain_constraints", true)
}

pub(super) fn checks<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let constraints = read.base(storage, Base::Constraint, transaction, arena)?;
    let classes = Index::oid(read.base(storage, Base::Class, transaction, arena)?, arena)?;
    let types = Index::oid(read.base(storage, Base::Type, transaction, arena)?, arena)?;
    let namespaces = Index::oid(
        read.base(storage, Base::Namespace, transaction, arena)?,
        arena,
    )?;
    let attributes = read.base(storage, Base::Attribute, transaction, arena)?;
    let [relation, number] = columns(&attributes, ["attrelid", "attnum"]);
    let attributes = Index::new(attributes, arena, |values| {
        Ok(Some((
            integer(values[relation])? as u32,
            integer(values[number])?,
        )))
    })?;
    let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
    let database = storage.current_database_name(transaction);
    let database = text(database.as_str(), arena)?;
    let mut output = Output::new(arena);
    for values in constraints.rows {
        let constraint = CatalogRow {
            definition: constraints.def,
            values,
        };
        let kind = constraint.character("contype")?;
        if !matches!(kind, b'c' | b'n') {
            continue;
        }
        let key = constraint.integers("conkey", arena)?;
        for class in classes.left(Some((constraint.integer("conrelid")? as u32, 0))) {
            let class = class.map(|values| CatalogRow {
                definition: classes.table.def,
                values,
            });
            for ty in types.left(Some((constraint.integer("contypid")? as u32, 0))) {
                let ty = ty.map(|values| CatalogRow {
                    definition: types.table.def,
                    values,
                });
                let owner = class
                    .map(|row| row.integer("relowner"))
                    .transpose()?
                    .or(ty.map(|row| row.integer("typowner")).transpose()?);
                if !owner.is_some_and(|owner| owner_is_enabled(storage, transaction, owner)) {
                    continue;
                }
                let expression = if kind == b'c' {
                    match constraint.get("conbin")? {
                        Datum::Null => Datum::Null,
                        node => catalog
                            .expression_def(
                                super::view_rows::string(node)?,
                                class
                                    .map(|row| row.integer("oid"))
                                    .transpose()?
                                    .unwrap_or(0),
                                false,
                                arena,
                            )?
                            .map_or(Datum::Null, Datum::Text),
                    }
                } else {
                    Datum::Null
                };
                for names in namespaces.left(Some((constraint.integer("connamespace")? as u32, 0)))
                {
                    let names = names.map(|values| CatalogRow {
                        definition: namespaces.table.def,
                        values,
                    });
                    let namespace = names
                        .map(|row| row.get("nspname"))
                        .transpose()?
                        .unwrap_or(Datum::Null);
                    if kind == b'c' {
                        output.push(&[
                            database,
                            namespace,
                            constraint.get("conname")?,
                            expression,
                        ])?;
                        continue;
                    }
                    let attribute_key = key
                        .first()
                        .map(|&number| {
                            constraint
                                .integer("conrelid")
                                .map(|relation| (relation as u32, number))
                        })
                        .transpose()?;
                    for attribute in attributes.left(attribute_key) {
                        let attribute = attribute.map(|values| CatalogRow {
                            definition: attributes.table.def,
                            values,
                        });
                        let name = attribute
                            .map(|row| row.string("attname"))
                            .transpose()?
                            .unwrap_or("VALUE");
                        let clause = arena
                            .alloc_str_display(format_args!("{name} IS NOT NULL"))
                            .map_err(|_| super::arena_full())?;
                        output.push(&[
                            database,
                            namespace,
                            constraint.get("conname")?,
                            Datum::Text(clause),
                        ])?;
                    }
                }
            }
        }
    }
    output.finish("check_constraints", true)
}

fn action(code: u8) -> Datum<'static> {
    match code {
        b'c' => Datum::Text("CASCADE"),
        b'n' => Datum::Text("SET NULL"),
        b'd' => Datum::Text("SET DEFAULT"),
        b'r' => Datum::Text("RESTRICT"),
        b'a' => Datum::Text("NO ACTION"),
        _ => Datum::Null,
    }
}

pub(super) fn dependencies<'a>(
    storage: &Storage,
    transaction: u32,
    arena: &'a Arena,
) -> Result<&'a [&'a [Datum<'a>]], SqlError> {
    let constraints = super::pg_constraint_values(storage, transaction, arena)?;
    let definition = super::schema::require("pg_constraint", false);
    let [kind, oid, domain, table, key, index, parent, referenced] = [
        "contype",
        "oid",
        "contypid",
        "conrelid",
        "conkey",
        "conindid",
        "confrelid",
        "confkey",
    ]
    .map(|name| {
        definition
            .columns
            .iter()
            .position(|(field, _)| *field == name)
            .expect("constraint source field exists")
    });
    let indexes = super::collect_indexes(storage, transaction, arena)?;
    let mut output = Output::new(arena);
    let mut push = |class, object, referenced_class, referenced_object, column, kind| {
        output.push(&[
            Datum::Oid(class as u32),
            Datum::Oid(object as u32),
            Datum::Int4(0),
            Datum::Oid(referenced_class as u32),
            Datum::Oid(referenced_object as u32),
            Datum::Int4(column),
            Datum::Char(kind),
        ])
    };
    for values in constraints {
        let constraint_kind = super::view_rows::character(values[kind])?;
        if !matches!(constraint_kind, b'c' | b'n' | b'p' | b'u' | b'f') {
            continue;
        }
        let oid = integer(values[oid])?;
        let domain = integer(values[domain])?;
        if domain != 0 {
            push(
                super::PG_CONSTRAINT_OID,
                oid,
                super::PG_TYPE_OID,
                domain,
                0,
                b'a',
            )?;
            continue;
        }
        let table = integer(values[table])?;
        let key = super::view_rows::integers(values[key], arena)?;
        if key.is_empty() {
            push(
                super::PG_CONSTRAINT_OID,
                oid,
                super::PG_CLASS_OID,
                table,
                0,
                b'a',
            )?;
        }
        for &column in key {
            push(
                super::PG_CONSTRAINT_OID,
                oid,
                super::PG_CLASS_OID,
                table,
                column,
                b'a',
            )?;
            if constraint_kind == b'c' {
                push(
                    super::PG_CONSTRAINT_OID,
                    oid,
                    super::PG_CLASS_OID,
                    table,
                    column,
                    b'n',
                )?;
            }
        }
        let index = integer(values[index])?;
        if matches!(constraint_kind, b'p' | b'u') {
            push(
                super::PG_CLASS_OID,
                index,
                super::PG_CONSTRAINT_OID,
                oid,
                0,
                b'i',
            )?;
        } else if constraint_kind == b'f' {
            push(
                super::PG_CONSTRAINT_OID,
                oid,
                super::PG_CLASS_OID,
                index,
                0,
                b'n',
            )?;
            for &column in super::view_rows::integers(values[referenced], arena)? {
                push(
                    super::PG_CONSTRAINT_OID,
                    oid,
                    super::PG_CLASS_OID,
                    integer(values[parent])?,
                    column,
                    b'n',
                )?;
            }
        }
    }
    for index in indexes {
        if index.is_constraint {
            continue;
        }
        let table = storage.table_def(index.table_slot, transaction);
        let mut referenced = crate::storage::ColumnSet::EMPTY;
        for (position, &column) in index.columns[..index.n_cols].iter().enumerate() {
            if !index.expression_keys[position] {
                referenced.insert(usize::from(column));
            }
        }
        for &column in &index.include_columns[..index.n_include_cols] {
            referenced.insert(usize::from(column));
        }
        for position in 0..index.n_cols {
            if index.expression_keys[position] {
                let source = super::index_expression_source(storage, index, position, transaction)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "index expression source is absent"
                        )
                    })?;
                let expression = crate::sql::parser::parse_expr(source.as_str(), arena)?;
                referenced |= super::super::exec::check_referenced_columns(expression, &table)?;
            }
        }
        if let Some(source) = index.predicate {
            let expression = crate::sql::parser::parse_expr(source.as_str(), arena)?;
            referenced |= super::super::exec::check_referenced_columns(expression, &table)?;
        }
        if referenced.is_empty() {
            push(
                super::PG_CLASS_OID,
                index.oid,
                super::PG_CLASS_OID,
                index.table_oid,
                0,
                b'a',
            )?;
        }
        for column in 0..table.n_columns {
            if referenced.contains(column) {
                push(
                    super::PG_CLASS_OID,
                    index.oid,
                    super::PG_CLASS_OID,
                    index.table_oid,
                    i32::from(table.attribute_number(column)),
                    b'a',
                )?;
            }
        }
    }
    output.rows().map(|rows| &*rows)
}
