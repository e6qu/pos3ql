//! PostgreSQL access-control-item scalar functions.

use crate::sql::ast::Expr;
use crate::sql::types::{ArrElem, ColType, Datum};
use crate::sql_err;

use super::super::{ColumnLookup, EvalHooks, SqlError, eval_full, sqlstate, type_mismatch};

#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch<'a>(
    name: &str,
    args: &[&Expr<'a>],
    star: bool,
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Option<Result<Datum<'a>, SqlError>> {
    if !matches!(
        name,
        "aclitemin"
            | "aclitemout"
            | "aclitemeq"
            | "hash_aclitem"
            | "hash_aclitem_extended"
            | "aclcontains"
            | "aclinsert"
            | "aclremove"
            | "makeaclitem"
    ) {
        return None;
    }
    Some((|| {
        let expected = match name {
            "aclitemin" | "aclitemout" | "hash_aclitem" => 1,
            "makeaclitem" => 4,
            _ => 2,
        };
        if star || args.len() != expected {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {}(...) with {} arguments does not exist",
                name,
                if star { 1 } else { args.len() }
            ));
        }
        if matches!(name, "aclinsert" | "aclremove") {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "{} is no longer supported",
                name
            ));
        }
        if name == "makeaclitem" {
            return make_aclitem(args, arena, params, row, hooks);
        }
        let left = eval_full(args[0], arena, params, row, hooks)?;
        if left.is_null() {
            return Ok(Datum::Null);
        }
        match name {
            "aclitemin" => match left {
                Datum::Text(value) => {
                    let catalog = hooks.catalog.ok_or_else(|| {
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "aclitem role catalog access is unavailable"
                        )
                    })?;
                    crate::sql::acl::from_text(value, catalog, arena).map(Datum::AclItem)
                }
                other => Err(type_mismatch(name, &other)),
            },
            "aclitemout" => match left {
                Datum::Text(value) => Ok(Datum::Text(value)),
                Datum::AclItem(item) => Ok(Datum::Text(item.text())),
                other => Err(type_mismatch(name, &other)),
            },
            "hash_aclitem" | "hash_aclitem_extended" => {
                let hash = match left {
                    Datum::AclItem(item) => item
                        .grantee()
                        .wrapping_add(item.grantor())
                        .wrapping_add(u32::from(item.privileges())),
                    Datum::Text(value) => {
                        let catalog = hooks.catalog.ok_or_else(|| {
                            sql_err!(
                                sqlstate::FEATURE_NOT_SUPPORTED,
                                "aclitem role catalog access is unavailable"
                            )
                        })?;
                        crate::sql::acl::hash_value(crate::sql::acl::parse(value)?, catalog, arena)?
                    }
                    _ => return Err(type_mismatch(name, &left)),
                };
                if name == "hash_aclitem" {
                    Ok(Datum::Int4(hash as i32))
                } else {
                    let seed = eval_full(args[1], arena, params, row, hooks)?;
                    let seed = match seed {
                        Datum::Int2(seed) => i64::from(seed),
                        Datum::Int4(seed) => i64::from(seed),
                        Datum::Oid(seed) => i64::from(seed),
                        Datum::Int8(seed) => seed,
                        Datum::Null => return Ok(Datum::Null),
                        other => return Err(type_mismatch(name, &other)),
                    };
                    match seed {
                        0 => Ok(Datum::Int8(i64::from(hash))),
                        seed => Ok(Datum::Int8(crate::sql::identity::hash_uint32_extended(
                            hash, seed,
                        ) as i64)),
                    }
                }
            }
            "aclitemeq" => {
                let right = eval_full(args[1], arena, params, row, hooks)?;
                if right.is_null() {
                    return Ok(Datum::Null);
                }
                match (left, right) {
                    (Datum::AclItem(left), Datum::AclItem(right)) => Ok(Datum::Bool(
                        left.grantee() == right.grantee()
                            && left.grantor() == right.grantor()
                            && left.privileges() == right.privileges()
                            && left.grant_options() == right.grant_options(),
                    )),
                    (Datum::Text(left), Datum::Text(right)) => Ok(Datum::Bool(
                        crate::sql::acl::parse(left)?.equivalent(crate::sql::acl::parse(right)?),
                    )),
                    (left, _) => Err(type_mismatch(name, &left)),
                }
            }
            "aclcontains" => {
                let Datum::Array { element, raw } = left else {
                    return Err(type_mismatch(name, &left));
                };
                if element != ArrElem::AclItem {
                    return Err(type_mismatch(name, &left));
                }
                let right = eval_full(args[1], arena, params, row, hooks)?;
                let right = match right {
                    Datum::AclItem(item) => item,
                    Datum::Text(value) => {
                        let catalog = hooks.catalog.ok_or_else(|| {
                            sql_err!(
                                sqlstate::FEATURE_NOT_SUPPORTED,
                                "aclitem role catalog access is unavailable"
                            )
                        })?;
                        crate::sql::acl::from_text(value, catalog, arena)?
                    }
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch(name, &other)),
                };
                for index in 0..crate::sql::array::len(raw) {
                    if !matches!(
                        crate::sql::array::get(raw, element, index),
                        Some(Datum::Text(_) | Datum::AclItem(_))
                    ) {
                        return Err(sql_err!(
                            sqlstate::NULL_VALUE_NOT_ALLOWED,
                            "ACL arrays must not contain null values"
                        ));
                    }
                }
                for index in 0..crate::sql::array::len(raw) {
                    let value = match crate::sql::array::get(raw, element, index) {
                        Some(Datum::AclItem(item)) => item,
                        Some(Datum::Text(value)) => {
                            let catalog = hooks.catalog.expect("ACL catalog was required above");
                            crate::sql::acl::from_text(value, catalog, arena)?
                        }
                        _ => unreachable!("ACL array was validated before containment"),
                    };
                    if value.grantee() == right.grantee()
                        && value.grantor() == right.grantor()
                        && value.privileges() & right.privileges() == right.privileges()
                        && value.grant_options() & right.grant_options() == right.grant_options()
                    {
                        return Ok(Datum::Bool(true));
                    }
                }
                Ok(Datum::Bool(false))
            }
            _ => unreachable!("guard admitted an unhandled aclitem function"),
        }
    })())
}

fn make_aclitem<'a>(
    args: &[&Expr<'a>],
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    let oid = |value| match value {
        Datum::Oid(value) => Ok(value),
        Datum::RegObject { referenced_oid, .. } => u32::try_from(referenced_oid)
            .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "OID is out of range")),
        Datum::Int2(value) => u32::try_from(value)
            .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "OID is out of range")),
        Datum::Int4(value) => u32::try_from(value)
            .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "OID is out of range")),
        Datum::Int8(value) => u32::try_from(value)
            .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "OID is out of range")),
        _ => Err(type_mismatch("makeaclitem", &value)),
    };
    let grantee = eval_full(args[0], arena, params, row, hooks)?;
    let grantor = eval_full(args[1], arena, params, row, hooks)?;
    let privilege = eval_full(args[2], arena, params, row, hooks)?;
    let grantable = eval_full(args[3], arena, params, row, hooks)?;
    if grantee.is_null() || grantor.is_null() || privilege.is_null() || grantable.is_null() {
        return Ok(Datum::Null);
    }
    let grantee_oid = oid(grantee)?;
    let grantor_oid = oid(grantor)?;
    let Datum::Text(privilege) = privilege else {
        return Err(type_mismatch("makeaclitem", &privilege));
    };
    let Datum::Bool(grantable) = grantable else {
        return Err(type_mismatch("makeaclitem", &grantable));
    };
    let role = |oid: u32| -> Result<&'a str, SqlError> {
        let name = hooks
            .catalog
            .map(|catalog| catalog.role_name(oid as i32, arena))
            .transpose()?
            .flatten();
        match name {
            Some(name) => arena
                .alloc_str_display(crate::sql::types::acl_identifier(name))
                .map_err(|_| super::super::arena_full()),
            None => arena
                .alloc_str_display(oid)
                .map_err(|_| super::super::arena_full()),
        }
    };
    let value = crate::sql::acl::make(
        role(grantee_oid)?,
        role(grantor_oid)?,
        privilege,
        grantable,
        arena,
    )?;
    crate::sql::acl::with_identity(grantee_oid, grantor_oid, value, arena).map(Datum::AclItem)
}

pub(crate) fn result_type(name: &str) -> Option<ColType> {
    match name {
        "aclitemin" | "makeaclitem" => Some(ColType::AclItem),
        "aclitemout" => Some(ColType::Text),
        "aclitemeq" | "aclcontains" => Some(ColType::Bool),
        "hash_aclitem" => Some(ColType::Int4),
        "hash_aclitem_extended" => Some(ColType::Int8),
        "aclinsert" | "aclremove" => Some(ColType::Array(ArrElem::AclItem)),
        _ => None,
    }
}
