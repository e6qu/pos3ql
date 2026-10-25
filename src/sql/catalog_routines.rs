//! Routine reports join selected catalog rows and use current permission checks.
//! Source: https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/information_schema.sql

use super::view_rows::{CatalogRow, Index, NamespaceJoin, Output};
use super::views::{Base, Read};
use super::{Arena, Datum, SqlError, Storage, SynthTable, arena_full, text};
use crate::sql::eval::{CatalogAccess, PrivilegeObject};
use crate::sql::eval::sqlstate;
use crate::sql_err;
use core::fmt::Write;

fn accessible(storage: &Storage, transaction: u32, procedure: CatalogRow<'_>, arena: &Arena) -> Result<bool, SqlError> {
    Ok(super::information::owner_is_enabled(storage, transaction, procedure.integer("proowner")?)
        || crate::sql::query::storage_catalog(storage, arena, transaction)
            .has_function_privilege(None, PrivilegeObject::Oid(procedure.integer("oid")?), "EXECUTE")? == Some(true))
}

fn specific_name<'a>(procedure: CatalogRow<'_>, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let name = super::routine_specific_name(procedure.string("proname")?, procedure.integer("oid")? as u32);
    text(name.as_str(), arena)
}

fn data_type<'a>(ty: CatalogRow<'a>, namespace: CatalogRow<'a>, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    if ty.integer("typelem")? != 0 && ty.integer("typlen")? == -1 {
        return text("ARRAY", arena);
    }
    if namespace.string("nspname")? != "pg_catalog" {
        return text("USER-DEFINED", arena);
    }
    let oid = ty.integer("oid")?;
    let name = if oid == crate::sql::types::oid::CHAR {
        "\"char\""
    } else if let Some(ty) = crate::sql::types::ColType::from_oid(oid) {
        ty.name()
    } else {
        ty.string("typname")?
    };
    text(name, arena)
}

pub(super) fn routines<'a>(storage: &Storage, transaction: u32, read: Read, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let procedures = read.base(storage, Base::Procedure, transaction, arena)?;
    let namespaces = Index::oid(read.base(storage, Base::Namespace, transaction, arena)?, arena)?;
    let languages = Index::oid(read.base(storage, Base::Language, transaction, arena)?, arena)?;
    let types = Index::oid(read.base(storage, Base::Type, transaction, arena)?, arena)?;
    let types = NamespaceJoin::new(&types, &namespaces, "typnamespace")?;
    let database = storage.current_database_name(transaction);
    let database = text(database.as_str(), arena)?;
    let mut output = Output::new(arena);
    for values in procedures.rows {
        let procedure = CatalogRow { definition: procedures.def, values };
        if !accessible(storage, transaction, procedure, arena)? { continue; }
        let kind = procedure.character("prokind")?;
        let owned = super::information::owner_is_enabled(storage, transaction, procedure.integer("proowner")?);
        let specific = specific_name(procedure, arena)?;
        for values in namespaces.matching((procedure.integer("pronamespace")? as u32, 0)) {
            let namespace = CatalogRow { definition: namespaces.table.def, values };
            for values in languages.matching((procedure.integer("prolang")? as u32, 0)) {
                let language = CatalogRow { definition: languages.table.def, values };
                let language_name = language.string("lanname")?;
                let mut external_language = crate::util::StackStr::<63>::new();
                for character in language_name.chars() {
                    external_language.write_char(character.to_ascii_uppercase()).map_err(|_| arena_full())?;
                }
                let external_language = text(external_language.as_str(), arena)?;
                let type_key = (kind != b'p').then_some((procedure.integer("prorettype")? as u32, 0));
                for result in types.left(type_key, |_, _| true) {
                    let mut fields = [Datum::Null; 82];
                    fields[0] = database;
                    fields[1] = namespace.get("nspname")?;
                    fields[2] = specific;
                    fields[3] = database;
                    fields[4] = namespace.get("nspname")?;
                    fields[5] = procedure.get("proname")?;
                    fields[6] = match kind { b'f' => text("FUNCTION", arena)?, b'p' => text("PROCEDURE", arena)?, _ => Datum::Null };
                    if let Some((ty, namespace)) = result {
                        fields[13] = data_type(ty, namespace, arena)?;
                        fields[28] = database;
                        fields[29] = namespace.get("nspname")?;
                        fields[30] = ty.get("typname")?;
                    }
                    if kind != b'p' { fields[35] = text("0", arena)?; }
                    fields[36] = text(if language_name == "sql" { "SQL" } else { "EXTERNAL" }, arena)?;
                    if owned { fields[37] = procedure.get("prosrc")?; }
                    if language_name == "c" { fields[38] = procedure.get("prosrc")?; }
                    fields[39] = external_language;
                    fields[40] = text("GENERAL", arena)?;
                    fields[41] = text(if procedure.character("provolatile")? == b'i' { "YES" } else { "NO" }, arena)?;
                    fields[42] = text("MODIFIES", arena)?;
                    if kind != b'p' { fields[43] = text(if procedure.boolean("proisstrict")? { "YES" } else { "NO" }, arena)?; }
                    fields[45] = text("YES", arena)?;
                    fields[46] = Datum::Int4(0);
                    fields[49] = text(if procedure.boolean("prosecdef")? { "DEFINER" } else { "INVOKER" }, arena)?;
                    fields[53] = text("NO", arena)?;
                    fields[57] = text("NO", arena)?;
                    output.push(&fields)?;
                }
            }
        }
    }
    output.finish("routines", true)
}

fn array_item(value: Datum<'_>, index: usize) -> Result<Datum<'_>, SqlError> {
    match value {
        Datum::Null => Ok(Datum::Null),
        Datum::Array { raw, element } => Ok(crate::sql::array::get(raw, element, index).unwrap_or(Datum::Null)),
        _ => Err(sql_err!(sqlstate::INTERNAL_ERROR, "routine catalog field is not an array")),
    }
}

fn argument_default<'a>(storage: &Storage, transaction: u32, oid: i32, index: usize, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    if let Some(slot) = storage.routine_slot_by_oid(oid, transaction) {
        let routine = storage.routine_for(slot, transaction);
        return routine.parameters().get(index).and_then(|parameter| parameter.mode.default())
            .map(|source| crate::sql::enum_values::checks::canonical_display(source.as_str(), arena).map(Datum::Text))
            .transpose().map(|value| value.unwrap_or(Datum::Null));
    }
    let Some((count, source)) = super::intrinsic_routine_defaults(oid) else { return Ok(Datum::Null); };
    let routine = super::INTRINSIC_ROUTINES.iter().find(|routine| routine.oid == oid)
        .expect("intrinsic defaults name an intrinsic routine");
    let first = routine.argument_count as usize - count;
    if index < first || index >= routine.argument_count as usize { return Ok(Datum::Null); }
    let wanted = index - first;
    let mut lexer = crate::sql::lexer::Lexer::new(source, arena);
    let mut start = 0;
    let mut depth = 0usize;
    let mut position = 0usize;
    loop {
        use crate::sql::lexer::Tok;
        let token = lexer.next_token().map_err(|_| sql_err!(sqlstate::INTERNAL_ERROR, "invalid intrinsic routine defaults"))?;
        match token {
            Tok::Op("(" | "[") => depth += 1,
            Tok::Op(")" | "]") => depth = depth.checked_sub(1).ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "invalid intrinsic routine defaults"))?,
            _ => {}
        }
        if token == Tok::Eof || (token == Tok::Op(",") && depth == 0) {
            if position == wanted {
                return crate::sql::enum_values::checks::canonical_display(&source[start..lexer.token_start()], arena).map(Datum::Text);
            }
            position += 1;
            start = lexer.token_end();
        }
        if token == Tok::Eof { break; }
    }
    Err(sql_err!(sqlstate::INTERNAL_ERROR, "intrinsic routine default count does not match its expressions"))
}

pub(super) fn parameters<'a>(storage: &Storage, transaction: u32, read: Read, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let procedures = read.base(storage, Base::Procedure, transaction, arena)?;
    let namespaces = Index::oid(read.base(storage, Base::Namespace, transaction, arena)?, arena)?;
    let types = Index::oid(read.base(storage, Base::Type, transaction, arena)?, arena)?;
    let types = NamespaceJoin::new(&types, &namespaces, "typnamespace")?;
    let database = storage.current_database_name(transaction);
    let database = text(database.as_str(), arena)?;
    let mut output = Output::new(arena);
    for values in procedures.rows {
        let procedure = CatalogRow { definition: procedures.def, values };
        if !accessible(storage, transaction, procedure, arena)? { continue; }
        let arguments = procedure.integers(if procedure.get("proallargtypes")?.is_null() { "proargtypes" } else { "proallargtypes" }, arena)?;
        let owned = super::information::owner_is_enabled(storage, transaction, procedure.integer("proowner")?);
        let specific = specific_name(procedure, arena)?;
        for values in namespaces.matching((procedure.integer("pronamespace")? as u32, 0)) {
            let namespace = CatalogRow { definition: namespaces.table.def, values };
            for (index, oid) in arguments.iter().enumerate() {
                let modes = procedure.get("proargmodes")?;
                let mode = if modes.is_null() { Some("IN") } else {
                    match array_item(modes, index)? {
                        Datum::Char(b'i' | b'v') => Some("IN"),
                        Datum::Char(b'o' | b't') => Some("OUT"),
                        Datum::Char(b'b') => Some("INOUT"),
                        _ => None,
                    }
                };
                let name = array_item(procedure.get("proargnames")?, index)?;
                let name = if name.is_null() || super::view_rows::string(name)?.is_empty() { Datum::Null } else { name };
                let ordinal = arena.alloc_str_display(format_args!("{}", index + 1)).map_err(|_| arena_full())?;
                let default = if owned { argument_default(storage, transaction, procedure.integer("oid")?, index, arena)? } else { Datum::Null };
                for (ty, type_namespace) in types.joined(Some((*oid as u32, 0))) {
                    let mut fields = [Datum::Null; 32];
                    fields[0] = database;
                    fields[1] = namespace.get("nspname")?;
                    fields[2] = specific;
                    fields[3] = Datum::Int4((index + 1) as i32);
                    fields[4] = mode.map(|mode| text(mode, arena)).transpose()?.unwrap_or(Datum::Null);
                    fields[5] = text("NO", arena)?;
                    fields[6] = text("NO", arena)?;
                    fields[7] = name;
                    fields[8] = data_type(ty, type_namespace, arena)?;
                    fields[23] = database;
                    fields[24] = type_namespace.get("nspname")?;
                    fields[25] = ty.get("typname")?;
                    fields[30] = Datum::Text(ordinal);
                    fields[31] = default;
                    output.push(&fields)?;
                }
            }
        }
    }
    output.finish("parameters", true)
}
