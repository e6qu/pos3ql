//! Privilege reports use selected ACLs and role rows, with current role membership.
//! Source: https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/information_schema.sql

use super::view_rows::{CatalogRow, Index, Output};
use super::views::{Base, Read, columns};
use super::{Arena, Datum, SqlError, Storage, SynthTable, text};
use crate::sql::acl::{DefaultObject, Exploded};
use crate::sql::eval::sqlstate;
use crate::sql_err;

#[derive(Clone, Copy)]
pub(super) enum Visibility {
    Accessible,
    EnabledRoles,
}

struct Roles<'s, 'a> {
    storage: &'s Storage,
    transaction: u32,
    selected: Index<'a>,
    name: usize,
    enabled_names: &'a [&'a str],
}

impl<'s, 'a> Roles<'s, 'a> {
    fn new(storage: &'s Storage, transaction: u32, read: Read, arena: &'a Arena) -> Result<Self, SqlError> {
        let selected = Index::oid(read.authid_for_view(storage, transaction, arena)?, arena)?;
        let [name, oid] = columns(&selected.table, ["rolname", "oid"]);
        let names = arena.alloc_slice_with(selected.table.rows.len(), |_| "").map_err(|_| super::arena_full())?;
        let mut count = 0;
        for values in selected.table.rows {
            if super::information::owner_is_enabled(storage, transaction, super::view_rows::integer(values[oid])?) {
                names[count] = super::view_rows::string(values[name])?;
                count += 1;
            }
        }
        let names = &mut names[..count];
        names.sort_unstable();
        Ok(Self { storage, transaction, selected, name, enabled_names: names })
    }

    fn enabled(&self, oid: u32) -> bool {
        oid != 0 && super::information::owner_is_enabled(self.storage, self.transaction, oid as i32)
    }

    fn visible(&self, grant: Exploded, visibility: Visibility) -> bool {
        match visibility {
            Visibility::Accessible => self.enabled(grant.grantor) || self.enabled(grant.grantee)
                || self.names(grant).any(|(_, grantee)| super::view_rows::string(grantee).expect("selected role name") == "PUBLIC"),
            Visibility::EnabledRoles => self.names(grant).any(|(grantor, grantee)| {
                [grantor, grantee].into_iter().any(|name| {
                    self.enabled_names.binary_search(&super::view_rows::string(name).expect("selected role name")).is_ok()
                })
            }),
        }
    }

    fn grantable(&self, grant: Exploded, owner: u32) -> bool {
        grant.grantable || (grant.grantee != 0
            && super::information::role_slot(self.storage, self.transaction, grant.grantee as i32)
                .zip(super::information::role_slot(self.storage, self.transaction, owner as i32))
                .is_some_and(|(member, owner)| self.storage.role_has_privileges_of(member, owner, self.transaction)))
    }

    fn names(&self, grant: Exploded) -> impl Iterator<Item = (Datum<'a>, Datum<'a>)> + '_ {
        self.selected.matching((grant.grantor, 0)).flat_map(move |grantor| {
            self.selected.matching((grant.grantee, 0)).map(|grantee| grantee[self.name])
                .chain((grant.grantee == 0).then_some(Datum::Text("PUBLIC")))
                .map(move |grantee| (grantor[self.name], grantee))
        })
    }
}

fn acl<'a>(value: Datum<'a>, default: DefaultObject, owner: u32, storage: &Storage, transaction: u32, arena: &'a Arena) -> Result<&'a [u8], SqlError> {
    let value = if value.is_null() {
        let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
        crate::sql::acl::default_acl(default, owner, &catalog, arena)?
    } else { value };
    match value {
        Datum::Array { element: crate::sql::types::ArrElem::AclItem, raw } => Ok(raw),
        _ => Err(sql_err!(sqlstate::INTERNAL_ERROR, "catalog privilege field is not an ACL array")),
    }
}

pub(super) fn routines<'a>(storage: &Storage, transaction: u32, read: Read, visibility: Visibility, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let procedures = read.base(storage, Base::Procedure, transaction, arena)?;
    let namespaces = Index::oid(read.base(storage, Base::Namespace, transaction, arena)?, arena)?;
    let roles = Roles::new(storage, transaction, read, arena)?;
    let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
    let database = storage.current_database_name(transaction);
    let database = text(database.as_str(), arena)?;
    let mut output = Output::new(arena);
    for values in procedures.rows {
        let procedure = CatalogRow { definition: procedures.def, values };
        let owner = procedure.integer("proowner")? as u32;
        let grants = acl(procedure.get("proacl")?, DefaultObject::Function, owner, storage, transaction, arena)?;
        let count = crate::sql::acl::explode_count(grants)?;
        let specific = super::routine_specific_name(procedure.string("proname")?, procedure.integer("oid")? as u32);
        let specific = text(specific.as_str(), arena)?;
        for index in 0..count {
            let grant = crate::sql::acl::explode_at(grants, index, &catalog, arena)?.expect("counted ACL privilege");
            if grant.privilege != "EXECUTE" || !roles.visible(grant, visibility) { continue; }
            let grantable = text(if roles.grantable(grant, owner) { "YES" } else { "NO" }, arena)?;
            for values in namespaces.matching((procedure.integer("pronamespace")? as u32, 0)) {
                let namespace = CatalogRow { definition: namespaces.table.def, values };
                for (grantor, grantee) in roles.names(grant) {
                    output.push(&[grantor,grantee,database,namespace.get("nspname")?,specific,
                        database,namespace.get("nspname")?,procedure.get("proname")?,text("EXECUTE", arena)?,grantable])?;
                }
            }
        }
    }
    output.finish(match visibility { Visibility::Accessible => "routine_privileges", Visibility::EnabledRoles => "role_routine_grants" }, true)
}

pub(super) fn tables<'a>(storage: &Storage, transaction: u32, read: Read, visibility: Visibility, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let classes = read.base(storage, Base::Class, transaction, arena)?;
    let namespaces = Index::oid(read.base(storage, Base::Namespace, transaction, arena)?, arena)?;
    let roles = Roles::new(storage, transaction, read, arena)?;
    let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
    let database = storage.current_database_name(transaction);
    let database = text(database.as_str(), arena)?;
    let mut output = Output::new(arena);
    for values in classes.rows {
        let class = CatalogRow { definition: classes.def, values };
        if !matches!(class.character("relkind")?, b'r' | b'v' | b'f' | b'p') { continue; }
        let owner = class.integer("relowner")? as u32;
        let grants = acl(class.get("relacl")?, DefaultObject::Relation, owner, storage, transaction, arena)?;
        for index in 0..crate::sql::acl::explode_count(grants)? {
            let grant = crate::sql::acl::explode_at(grants, index, &catalog, arena)?.expect("counted ACL privilege");
            if !matches!(grant.privilege, "INSERT" | "SELECT" | "UPDATE" | "DELETE" | "TRUNCATE" | "REFERENCES" | "TRIGGER") || !roles.visible(grant, visibility) { continue; }
            let grantable = text(if roles.grantable(grant, owner) { "YES" } else { "NO" }, arena)?;
            for values in namespaces.matching((class.integer("relnamespace")? as u32, 0)) {
                let namespace = CatalogRow { definition: namespaces.table.def, values };
                for (grantor, grantee) in roles.names(grant) {
                    output.push(&[grantor,grantee,database,namespace.get("nspname")?,class.get("relname")?,
                        text(grant.privilege, arena)?,grantable,text(if grant.privilege == "SELECT" { "YES" } else { "NO" }, arena)?])?;
                }
            }
        }
    }
    output.finish(match visibility { Visibility::Accessible => "table_privileges", Visibility::EnabledRoles => "role_table_grants" }, true)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ColumnGrant<'a> {
    grantor: u32,
    grantee: u32,
    column: &'a str,
    relation: &'a str,
    namespace: u32,
    privilege: &'static str,
    grantable: bool,
    owner: u32,
}

pub(super) fn columns_view<'a>(storage: &Storage, transaction: u32, read: Read, visibility: Visibility, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let classes = read.base(storage, Base::Class, transaction, arena)?;
    let attributes = read.base(storage, Base::Attribute, transaction, arena)?;
    let [relation, number, dropped] = columns(&attributes, ["attrelid", "attnum", "attisdropped"]);
    let attributes = Index::new(attributes, arena, |values| {
        if super::view_rows::integer(values[number])? > 0 && !super::view_rows::boolean(values[dropped])? {
            Ok(Some((super::view_rows::integer(values[relation])? as u32, 0)))
        } else { Ok(None) }
    })?;
    let namespaces = Index::oid(read.base(storage, Base::Namespace, transaction, arena)?, arena)?;
    let roles = Roles::new(storage, transaction, read, arena)?;
    let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
    let database = storage.current_database_name(transaction);
    let database = text(database.as_str(), arena)?;
    let mut union = crate::mem::arena::ArenaList::new(arena);
    for values in classes.rows {
        let class = CatalogRow { definition: classes.def, values };
        if !matches!(class.character("relkind")?, b'r' | b'v' | b'f' | b'p') { continue; }
        let owner = class.integer("relowner")? as u32;
        let table_grants = acl(class.get("relacl")?, DefaultObject::Relation, owner, storage, transaction, arena)?;
        for values in attributes.matching((class.integer("oid")? as u32, 0)) {
            let attribute = CatalogRow { definition: attributes.table.def, values };
            // acldefault('c', owner) is empty: a missing column ACL adds no rows.
            let column_grants = if attribute.get("attacl")?.is_null() { None } else {
                Some(acl(attribute.get("attacl")?, DefaultObject::Column, owner, storage, transaction, arena)?)
            };
            for grants in [Some(table_grants), column_grants].into_iter().flatten() {
                for index in 0..crate::sql::acl::explode_count(grants)? {
                    let grant = crate::sql::acl::explode_at(grants, index, &catalog, arena)?.expect("counted ACL privilege");
                    if !matches!(grant.privilege, "INSERT" | "SELECT" | "UPDATE" | "REFERENCES") || !roles.visible(grant, visibility) { continue; }
                    union.push(ColumnGrant {
                        grantor: grant.grantor,
                        grantee: grant.grantee,
                        column: attribute.string("attname")?,
                        relation: class.string("relname")?,
                        namespace: class.integer("relnamespace")? as u32,
                        privilege: grant.privilege,
                        grantable: grant.grantable,
                        owner,
                    }).map_err(|_| super::arena_full())?;
                }
            }
        }
    }
    let union = union.into_mut_slice().map_err(|_| super::arena_full())?;
    union.sort_unstable();
    let mut prior = None;
    let mut output = Output::new(arena);
    for &item in union.iter() {
        if prior == Some(item) { continue; }
        prior = Some(item);
        let grant = Exploded { grantor: item.grantor, grantee: item.grantee, privilege: item.privilege, grantable: item.grantable };
        let grantable = text(if roles.grantable(grant, item.owner) { "YES" } else { "NO" }, arena)?;
        for values in namespaces.matching((item.namespace, 0)) {
            let namespace = CatalogRow { definition: namespaces.table.def, values };
            for (grantor, grantee) in roles.names(grant) {
                output.push(&[grantor,grantee,database,namespace.get("nspname")?,text(item.relation, arena)?,
                    text(item.column, arena)?,text(item.privilege, arena)?,grantable])?;
            }
        }
    }
    output.finish(match visibility { Visibility::Accessible => "column_privileges", Visibility::EnabledRoles => "role_column_grants" }, true)
}

#[derive(Clone, Copy)]
enum UsageSource {
    Collation,
    Domain,
    ForeignDataWrapper,
    ForeignServer,
    Sequence,
}

pub(super) fn usage<'a>(storage: &Storage, transaction: u32, read: Read, visibility: Visibility, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let namespaces = Index::oid(read.base(storage, Base::Namespace, transaction, arena)?, arena)?;
    let roles = Roles::new(storage, transaction, read, arena)?;
    let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
    let database = storage.current_database_name(transaction);
    let databases = read.base(storage, Base::Database, transaction, arena)?;
    let [database_name, encoding] = columns(&databases, ["datname", "encoding"]);
    let mut database_encoding = None;
    for values in databases.rows {
        if super::view_rows::string(values[database_name])? == database.as_str() {
            if database_encoding.is_some() {
                return Err(sql_err!(sqlstate::CARDINALITY_VIOLATION, "more than one row returned by a subquery used as an expression"));
            }
            database_encoding = Some(super::view_rows::integer(values[encoding])?);
        }
    }
    let database = text(database.as_str(), arena)?;
    let mut output = Output::new(arena);
    for source in [UsageSource::Collation, UsageSource::Domain, UsageSource::ForeignDataWrapper, UsageSource::ForeignServer, UsageSource::Sequence] {
        let (base, name, namespace, owner, privilege_field, default, kind) = match source {
            UsageSource::Collation => (Base::Collation, "collname", Some("collnamespace"), "collowner", None, DefaultObject::Type, "COLLATION"),
            UsageSource::Domain => (Base::Type, "typname", Some("typnamespace"), "typowner", Some("typacl"), DefaultObject::Type, "DOMAIN"),
            UsageSource::ForeignDataWrapper => (Base::ForeignDataWrapper, "fdwname", None, "fdwowner", Some("fdwacl"), DefaultObject::ForeignDataWrapper, "FOREIGN DATA WRAPPER"),
            UsageSource::ForeignServer => (Base::ForeignServer, "srvname", None, "srvowner", Some("srvacl"), DefaultObject::ForeignServer, "FOREIGN SERVER"),
            UsageSource::Sequence => (Base::Class, "relname", Some("relnamespace"), "relowner", Some("relacl"), DefaultObject::Relation, "SEQUENCE"),
        };
        let objects = read.base(storage, base, transaction, arena)?;
        for values in objects.rows {
            let object = CatalogRow { definition: objects.def, values };
            match source {
                UsageSource::Collation => {
                    let encoding = object.integer("collencoding")?;
                    if encoding != -1 && Some(encoding) != database_encoding { continue; }
                }
                UsageSource::Domain if object.character("typtype")? != b'd' => continue,
                UsageSource::Sequence if object.character("relkind")? != b'S' => continue,
                _ => {}
            }
            let owner = object.integer(owner)? as u32;
            let grants = privilege_field.map(|field| object.get(field)).transpose()?
                .map(|value| acl(value, default, owner, storage, transaction, arena)).transpose()?;
            let count = grants.map(crate::sql::acl::explode_count).transpose()?.unwrap_or(1);
            for index in 0..count {
                let grant = match grants {
                    Some(grants) => crate::sql::acl::explode_at(grants, index, &catalog, arena)?.expect("counted ACL privilege"),
                    None => Exploded { grantor: owner, grantee: 0, privilege: "USAGE", grantable: false },
                };
                if grant.privilege != "USAGE" || !roles.visible(grant, visibility) { continue; }
                let grantable = text(if privilege_field.is_some() && roles.grantable(grant, owner) { "YES" } else { "NO" }, arena)?;
                let namespace = namespace.map(|field| object.integer(field)).transpose()?;
                for values in namespaces.left(namespace.map(|oid| (oid as u32, 0))) {
                    let schema = match (namespace, values) {
                        (None, _) => text("", arena)?,
                        (Some(_), Some(values)) => CatalogRow { definition: namespaces.table.def, values }.get("nspname")?,
                        (Some(_), None) => continue,
                    };
                    for (grantor, grantee) in roles.names(grant) {
                        output.push(&[grantor,grantee,database,schema,object.get(name)?,text(kind, arena)?,text("USAGE", arena)?,grantable])?;
                    }
                }
            }
        }
    }
    output.finish(match visibility { Visibility::Accessible => "usage_privileges", Visibility::EnabledRoles => "role_usage_grants" }, true)
}
