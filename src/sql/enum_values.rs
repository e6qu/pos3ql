//! Bind durable enum member identities to the selected catalog image.

use crate::mem::arena::Arena;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::Datum;
use crate::sql_err;
use crate::storage::Storage;

pub(crate) fn resolve<'a>(
    value: Datum<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let Datum::Enum {
        slot,
        member,
        sort,
        label,
    } = value
    else {
        return Ok(value);
    };
    let corrupt = || {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "stored enum member has no catalog identity"
        )
    };
    if usize::from(slot) >= storage.enum_count() {
        return Err(corrupt());
    }
    let definition = storage.enum_for(usize::from(slot), txid);
    if !definition.visible_to(txid) {
        return Err(corrupt());
    }
    let index = match member {
        Some(index) => index as usize,
        None => definition
            .members()
            .position(|candidate| {
                candidate
                    .legacy_label
                    .is_some_and(|legacy| legacy.as_str() == label)
            })
            .ok_or_else(corrupt)?,
    };
    let selected = definition.members().nth(index).ok_or_else(corrupt)?;
    let current_label = if selected.label.as_str() == label {
        label
    } else {
        arena.alloc_str(selected.label.as_str()).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "enum value exceeds the statement arena"
            )
        })?
    };
    let _ = sort;
    Ok(Datum::Enum {
        slot,
        member: Some(index as u32),
        sort: selected.sort,
        label: current_label,
    })
}

pub(crate) fn sort(value: Datum<'_>, storage: &Storage, txid: u32) -> Result<f64, SqlError> {
    let Datum::Enum {
        slot,
        member,
        label,
        ..
    } = value
    else {
        unreachable!()
    };
    let corrupt = || {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "stored enum member has no catalog identity"
        )
    };
    if usize::from(slot) >= storage.enum_count() {
        return Err(corrupt());
    }
    let definition = storage.enum_for(usize::from(slot), txid);
    let member = match member {
        Some(index) => definition.members().nth(index as usize),
        None => definition.members().find(|member| {
            member
                .legacy_label
                .is_some_and(|legacy| legacy.as_str() == label)
        }),
    };
    member.map(|member| member.sort).ok_or_else(corrupt)
}
