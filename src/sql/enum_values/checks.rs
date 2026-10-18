//! Stored enum expressions bind constants to members, not their labels.

use super::{SqlString, query_tokens, write_name};
use crate::mem::arena::Arena;
use crate::sql::ast::Stmt;
use crate::sql::eval::{EvalHooks, SqlError, sqlstate};
use crate::sql::lexer::Tok;
use crate::sql::types::{ColType, oid};
use crate::sql_err;
use crate::storage::Storage;
use crate::util::StackStr;
use core::fmt::Write;

const HEADER: &str = "pos3ql-enum-check-v1\n";

pub(crate) fn is_bound(source: &str) -> bool {
    source.starts_with(HEADER)
}

pub(crate) fn bind<const N: usize>(
    source: &str,
    base: ColType,
    storage: &Storage,
    txid: u32,
    legacy: bool,
    arena: &Arena,
) -> Result<StackStr<N>, SqlError> {
    bind_expression(source, base, storage, txid, legacy, arena, false)
}

pub(crate) fn bind_default<const N: usize>(
    source: &str,
    base: ColType,
    storage: &Storage,
    txid: u32,
    legacy: bool,
    arena: &Arena,
) -> Result<StackStr<N>, SqlError> {
    bind_expression(source, base, storage, txid, legacy, arena, true)
}

fn bind_expression<const N: usize>(
    source: &str,
    base: ColType,
    storage: &Storage,
    txid: u32,
    legacy: bool,
    arena: &Arena,
    default_target: bool,
) -> Result<StackStr<N>, SqlError> {
    let Some(slot) = enum_slot(base) else {
        return Ok(StackStr::from_str(source));
    };
    let definition = storage.enum_for(usize::from(slot), txid);
    let tokens = query_tokens(source, arena)?;
    let count = tokens
        .iter()
        .filter(|token| matches!(token.token, Tok::Str(_)))
        .count();
    let mut parameters = StackStr::<N>::new();
    let mut parameter = 0;
    for token in tokens {
        if matches!(token.token, Tok::Str(_)) {
            parameter += 1;
            let _ = write!(parameters, "${parameter} ");
        } else {
            let _ = write!(parameters, "{} ", &source[token.start..token.end]);
        }
    }
    if parameters.is_truncated() {
        return Err(full::<N>());
    }
    let mut name = StackStr::<256>::new();
    write_name(&mut name, definition.schema.as_str());
    let _ = name.write_char('.');
    write_name(&mut name, definition.name.as_str());
    let context_base = if default_target {
        match base {
            ColType::Array(_) => ColType::Array(crate::sql::types::ArrElem::Enum(slot)),
            _ => ColType::Enum(slot),
        }
    } else {
        base
    };
    let context_type =
        crate::sql::catalog::user_type_name_text(storage, txid, context_base.oid(), arena)?
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "enum expression has no base type name"
                )
            })?;
    let query = if default_target {
        arena.alloc_str_display(format_args!(
            "SELECT ({})::{}",
            parameters.as_str(),
            context_type
        ))
    } else {
        arena.alloc_str_display(format_args!(
            "SELECT {} FROM (VALUES (NULL::{})) AS __pos3ql_domain(value)",
            parameters.as_str(),
            context_type
        ))
    }
    .map_err(|_| crate::sql::query::arena_full_pub())?;
    let select = crate::sql::parser::parse_query(query, arena)?;
    let oids = arena
        .alloc_slice_with(count, |_| oid::TEXT)
        .map_err(|_| crate::sql::query::arena_full_pub())?;
    let statement = Stmt::Select(*select);
    crate::sql::param_types::infer(storage, &statement, txid, arena, oids);
    let mut output = StackStr::<N>::new();
    let _ = output.write_str(HEADER);
    let mut bound = false;
    parameter = 0;
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        // Mark explicit casts to the base enum, including CAST(... AS type).
        // Their spelling must follow the selected type through name moves.
        let after_cast =
            index > 0 && matches!(tokens[index - 1].token, Tok::Op("::") | Tok::Ident("as"));
        if after_cast {
            if let Tok::Ident(first) | Tok::QuotedIdent(first) = token.token {
                let (schema, bare, width) =
                    if index + 2 < tokens.len() && tokens[index + 1].token == Tok::Op(".") {
                        match tokens[index + 2].token {
                            Tok::Ident(bare) | Tok::QuotedIdent(bare) => (Some(first), bare, 3),
                            _ => (None, first, 1),
                        }
                    } else {
                        (None, first, 1)
                    };
                let selected = match schema {
                    Some(schema) => storage.enum_slot(schema, bare, txid),
                    None => storage.resolve_enum_slot(bare, txid),
                };
                if selected == Some(usize::from(slot)) {
                    let _ = write!(output, "/*p3t:{width}*/");
                    for selected in &tokens[index..index + width] {
                        let _ = write!(output, "{} ", &source[selected.start..selected.end]);
                    }
                    bound = true;
                    index += width;
                    continue;
                }
            }
        }
        if let Tok::Str(label) = token.token {
            let type_oid = oids[parameter];
            parameter += 1;
            if type_oid == oid::enum_oid(slot) {
                let member = definition
                    .members()
                    .position(|member| {
                        if legacy {
                            member.legacy_label.is_some_and(|old| old.as_str() == label)
                        } else {
                            member.label.as_str() == label
                        }
                    })
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::INVALID_TEXT_REPRESENTATION,
                            "invalid input value for enum {}: \"{}\"",
                            definition.name.as_str(),
                            label
                        )
                    })?;
                if !legacy {
                    crate::sql::exec::ensure_enum_value_is_safe(storage, slot, label, txid)?;
                }
                let _ = write!(
                    output,
                    "/*p3c:{member}*/{} ",
                    &source[token.start..token.end]
                );
                bound = true;
                index += 1;
                continue;
            }
            if type_oid == oid::enum_array_oid(slot) {
                let expression = arena
                    .alloc_str_display(format_args!("{}::{}[]", SqlString(label), name.as_str()))
                    .map_err(|_| crate::sql::query::arena_full_pub())?;
                let expression = crate::sql::parser::parse_expr(expression, arena)?;
                let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
                let hooks = EvalHooks {
                    catalog: Some(&catalog),
                    ..crate::sql::eval::NO_HOOKS
                };
                let value = if legacy {
                    crate::sql::exec::enum_array_from_legacy_labels(
                        label, slot, storage, txid, arena,
                    )?
                } else {
                    crate::sql::eval::eval_full(
                        expression,
                        arena,
                        crate::sql::eval::NO_PARAMS,
                        &crate::sql::eval::NoColumns,
                        &hooks,
                    )?
                };
                let text = crate::sql::exec::enum_array_member_text(value, storage, txid, arena)?;
                let _ = write!(output, "/*p3a*/{} ", SqlString(text));
                bound = true;
                index += 1;
                continue;
            }
        }
        let _ = write!(output, "{} ", &source[token.start..token.end]);
        index += 1;
    }
    if output.is_truncated() {
        return Err(full::<N>());
    }
    if bound {
        Ok(output)
    } else {
        Ok(StackStr::from_str(source))
    }
}

fn full<const N: usize>() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "bound enum expression exceeds {} bytes",
        N
    )
}

pub(crate) fn resolve_default<'a>(
    source: &str,
    base: ColType,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    if source.starts_with("pos3ql-enum-check-") {
        return resolve(source, base, storage, txid, arena);
    }
    let legacy = bind_default::<{ crate::storage::DEFAULT_EXPR_MAX }>(
        source, base, storage, txid, true, arena,
    )?;
    if is_bound(legacy.as_str()) {
        resolve(legacy.as_str(), base, storage, txid, arena)
    } else {
        arena
            .alloc_str(source)
            .map_err(|_| crate::sql::query::arena_full_pub())
    }
}

pub(crate) fn resolve<'a>(
    source: &str,
    base: ColType,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    if source.starts_with("pos3ql-enum-check-") && !is_bound(source) {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "unsupported enum expression encoding"
        ));
    }
    let Some(slot) = enum_slot(base) else {
        if is_bound(source) {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "bound enum expression has no enum base type"
            ));
        }

        return arena
            .alloc_str(source)
            .map_err(|_| crate::sql::query::arena_full_pub());
    };
    let legacy;
    let body = if let Some(body) = source.strip_prefix(HEADER) {
        body
    } else {
        if source.starts_with("pos3ql-enum-check-") {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "unsupported enum CHECK encoding"
            ));
        }
        legacy =
            bind::<{ crate::storage::CHECK_SQL_MAX }>(source, base, storage, txid, true, arena)?;
        let Some(body) = legacy.as_str().strip_prefix(HEADER) else {
            return arena
                .alloc_str(source)
                .map_err(|_| crate::sql::query::arena_full_pub());
        };
        body
    };
    let definition = storage.enum_for(usize::from(slot), txid);
    let mut name = StackStr::<256>::new();
    write_name(&mut name, definition.schema.as_str());
    let _ = name.write_char('.');
    write_name(&mut name, definition.name.as_str());
    let tokens = query_tokens(body, arena)?;
    let mut output = crate::mem::arena::ArenaString::new(arena);
    let mut previous = 0;
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        let gap = body[previous..token.start].trim();
        if let Some(member) = gap
            .strip_prefix("/*p3c:")
            .and_then(|text| text.strip_suffix("*/"))
        {
            if !matches!(token.token, Tok::Str(_)) {
                return Err(corrupt());
            }
            let member = member
                .parse::<usize>()
                .ok()
                .and_then(|member| definition.members().nth(member))
                .ok_or_else(corrupt)?;
            write!(output, "{} ", SqlString(member.label.as_str()))
                .map_err(|_| crate::sql::query::arena_full_pub())?;
        } else if gap == "/*p3a*/" {
            let Tok::Str(text) = token.token else {
                return Err(corrupt());
            };
            let value =
                crate::sql::exec::enum_array_from_members(text, slot, storage, txid, arena)?;
            let text = arena
                .alloc_str_display(value)
                .map_err(|_| crate::sql::query::arena_full_pub())?;
            write!(output, "{} ", SqlString(text))
                .map_err(|_| crate::sql::query::arena_full_pub())?;
        } else if let Some(width) = gap
            .strip_prefix("/*p3t:")
            .and_then(|text| text.strip_suffix("*/"))
        {
            let width = width
                .parse::<usize>()
                .ok()
                .filter(|width| matches!(width, 1 | 3))
                .ok_or_else(corrupt)?;
            if index + width > tokens.len() {
                return Err(corrupt());
            }
            write!(output, "{} ", name.as_str())
                .map_err(|_| crate::sql::query::arena_full_pub())?;
            previous = tokens[index + width - 1].end;
            index += width;
            continue;
        } else {
            write!(output, "{} ", &body[token.start..token.end])
                .map_err(|_| crate::sql::query::arena_full_pub())?;
        }
        previous = token.end;
        index += 1;
    }
    Ok(output.as_str())
}

fn corrupt() -> SqlError {
    sql_err!(
        sqlstate::INTERNAL_ERROR,
        "invalid stored enum CHECK binding"
    )
}

fn enum_slot(base: ColType) -> Option<u16> {
    match base {
        ColType::Enum(slot) => Some(slot),
        ColType::Array(element) => match element.to_coltype() {
            ColType::Enum(slot) => Some(slot),
            _ => None,
        },
        _ => None,
    }
}
