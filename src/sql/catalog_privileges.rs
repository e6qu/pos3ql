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
}

impl<'s, 'a> Roles<'s, 'a> {
    fn new(storage: &'s Storage, transaction: u32, read: Read, arena: &'a Arena) -> Result<Self, SqlError> {
        let selected = Index::oid(read.authid_for_view(storage, transaction, arena)?, arena)?;
        let [name] = columns(&selected.table, ["rolname"]);
        Ok(Self { storage, transaction, selected, name })
    }

    fn enabled(&self, oid: u32) -> bool {
        oid != 0 && super::information::owner_is_enabled(self.storage, self.transaction, oid as i32)
    }

    fn visible(&self, grant: Exploded, visibility: Visibility) -> bool {
        self.enabled(grant.grantor) || self.enabled(grant.grantee)
            || (matches!(visibility, Visibility::Accessible) && grant.grantee == 0)
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
