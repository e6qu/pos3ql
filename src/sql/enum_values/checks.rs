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
    let bound = bind_expression::<N>(source, base, storage, txid, legacy, arena, true)?;
    if enum_slot(base).is_none() {
        return Ok(bound);
    }
    let mut executable = StackStr::<N>::new();
    executable.write_str(HEADER).map_err(|_| full::<N>())?;
    executable
        .write_str(
            bound
                .as_str()
                .strip_prefix(HEADER)
                .unwrap_or(bound.as_str()),
        )
        .map_err(|_| full::<N>())?;
    let rendered = resolve_expression(executable.as_str(), base, storage, txid, arena, true)?;
    let display = canonical_display(rendered, arena)?;
    let mut stored = StackStr::<N>::new();
    stored.write_str(HEADER).map_err(|_| full::<N>())?;
    stored.write_str("/*p3o:").map_err(|_| full::<N>())?;
    for character in display.chars() {
        if matches!(character, '/' | '%') {
            write!(stored, "%{:02x}", character as u8).map_err(|_| full::<N>())?;
        } else {
            stored.write_char(character).map_err(|_| full::<N>())?;
        }
    }
    stored.write_str("*/\n").map_err(|_| full::<N>())?;
    stored
        .write_str(
            executable
                .as_str()
                .strip_prefix(HEADER)
                .expect("writer prefix"),
        )
        .map_err(|_| full::<N>())?;
    Ok(stored)
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
    let target = crate::storage::TypeDefinitionTarget::by_creation(
        storage, crate::storage::NamedTypeKind::Enum, slot,
    )?;
    let definition = storage.read_enum_definition(target, txid)?.ok_or_else(corrupt)?;
    let tokens = query_tokens(source, arena)?;
    let count = tokens
        .iter()
        .filter(|token| matches!(token.token, Tok::Str(_)))
        .count();
    let mut name = StackStr::<260>::new();
    write_name(&mut name, definition.schema.as_str());
    let _ = name.write_char('.');
    write_name(&mut name, definition.name.as_str());
    let context_base = if default_target {
        match base {
            ColType::Array(_) => ColType::Array(crate::sql::types::ArrElem::Enum(Some(slot))),
            _ => ColType::Enum(Some(slot)),
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
    let prefix = if default_target {
        "SELECT (\n"
    } else {
        "SELECT \n"
    };
    let query = if default_target {
        arena.alloc_str_display(format_args!("{prefix}{source}\n)::{context_type}"))
    } else {
        arena.alloc_str_display(format_args!(
            "{prefix}{source}\n FROM (VALUES (NULL::{context_type})) AS __pos3ql_domain(value)"
        ))
    }
    .map_err(|_| crate::sql::query::arena_full_pub())?;
    let positions = super::string_parameter_positions(tokens, prefix.len(), arena)?;
    let select = crate::sql::parser::parse_query_with_string_parameters(query, arena, positions)?;
    let oids = arena
        .alloc_slice_with(count, |_| oid::TEXT)
        .map_err(|_| crate::sql::query::arena_full_pub())?;
    let statement = Stmt::Select(*select);
    crate::sql::param_types::infer(storage, &statement, txid, arena, oids);
    let mut output = StackStr::<N>::new();
    let _ = output.write_str(HEADER);
    let mut bound = false;
    let mut parameter = 0;
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        // Mark explicit casts to the base enum, including CAST(... AS type).
        // Their spelling must follow the selected type through name moves.
        let after_cast =
            index > 0 && matches!(tokens[index - 1].token, Tok::Op("::") | Tok::Ident("as"));
        if after_cast && let Tok::Ident(first) | Tok::QuotedIdent(first) = token.token {
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
            if selected.is_some_and(|selected| storage.enum_for(selected, txid).creation() == slot) {
                let _ = write!(output, "/*p3t:{width}*/");
                for selected in &tokens[index..index + width] {
                    let _ = write!(output, "{} ", &source[selected.start..selected.end]);
                }
                bound = true;
                index += width;
                continue;
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
    resolve_default_expression(source, base, storage, txid, arena, false)
}

pub(crate) fn describe_default<'a>(
    source: &str,
    base: ColType,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    canonical_display(
        resolve_default_expression(source, base, storage, txid, arena, true)?,
        arena,
    )
}

fn resolve_default_expression<'a>(
    source: &str,
    base: ColType,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    display_names: bool,
) -> Result<&'a str, SqlError> {
    if source.starts_with("pos3ql-enum-check-") {
        return resolve_expression(source, base, storage, txid, arena, display_names);
    }
    let legacy = bind_default::<{ crate::storage::DEFAULT_EXPR_MAX }>(
        source, base, storage, txid, true, arena,
    )?;
    if is_bound(legacy.as_str()) {
        resolve_expression(legacy.as_str(), base, storage, txid, arena, display_names)
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
    resolve_expression(source, base, storage, txid, arena, false)
}

fn resolve_expression<'a>(
    source: &str,
    base: ColType,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    display_names: bool,
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
    let target = crate::storage::TypeDefinitionTarget::by_creation(
        storage, crate::storage::NamedTypeKind::Enum, slot,
    )?;
    let definition = storage.read_enum_definition(target, txid)?.ok_or_else(corrupt)?;
    let mut name = StackStr::<260>::new();
    if display_names {
        let display_name =
            crate::sql::catalog::user_type_name_text(storage, txid, oid::enum_oid(slot), arena)?
                .ok_or_else(corrupt)?;
        name.write_str(display_name).map_err(|_| corrupt())?;
    } else {
        write_name(&mut name, definition.schema.as_str());
        let _ = name.write_char('.');
        write_name(&mut name, definition.name.as_str());
    }
    let (_, body) = split_display(body)?;
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
            let explicit_cast = tokens
                .get(index + 1)
                .is_some_and(|next| matches!(next.token, Tok::Op("::") | Tok::Ident("as")));
            write!(output, "{}", SqlString(member.label.as_str()))
                .map_err(|_| crate::sql::query::arena_full_pub())?;
            if !explicit_cast {
                write!(output, "::{}", name.as_str())
                    .map_err(|_| crate::sql::query::arena_full_pub())?;
            }
            output
                .write_char(' ')
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
            let explicit_cast = tokens
                .get(index + 1)
                .is_some_and(|next| matches!(next.token, Tok::Op("::") | Tok::Ident("as")));
            write!(output, "{}", SqlString(text))
                .map_err(|_| crate::sql::query::arena_full_pub())?;
            if !explicit_cast {
                write!(output, "::{}[]", name.as_str())
                    .map_err(|_| crate::sql::query::arena_full_pub())?;
            }
            output
                .write_char(' ')
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

pub(crate) fn canonical_display<'a>(source: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let expression = crate::sql::parser::parse_expr(source, arena)?;
    let parenthesize = matches!(expression, crate::sql::ast::Expr::Binary { .. });
    let mut tokens = query_tokens(source, arena)?;
    // PostgreSQL's non-pretty expression output encloses root operators once.
    // Lexer boundaries keep parentheses inside quoted text out of this test.
    while matches!(tokens.first().map(|token| token.token), Some(Tok::Op("(")))
        && matches!(tokens.last().map(|token| token.token), Some(Tok::Op(")")))
    {
        let mut depth = 0;
        let mut enclosed = true;
        for (position, token) in tokens.iter().enumerate() {
            match token.token {
                Tok::Op("(") => depth += 1,
                Tok::Op(")") => depth -= 1,
                _ => {}
            }
            if depth == 0 && position + 1 != tokens.len() {
                enclosed = false;
                break;
            }
        }
        if !enclosed {
            break;
        }
        tokens = &tokens[1..tokens.len() - 1];
    }
    let mut output = crate::mem::arena::ArenaString::new(arena);
    if parenthesize {
        output
            .write_char('(')
            .map_err(|_| crate::sql::query::arena_full_pub())?;
    }
    let mut previous = None;
    for token in tokens {
        let no_space_before = matches!(token.token, Tok::Op("::" | "." | ")" | "[" | "]" | ","))
            || (matches!(token.token, Tok::Op("("))
                && match previous {
                    Some(Tok::Ident(word)) => !matches!(
                        word,
                        "and" | "or" | "not" | "in" | "between" | "case" | "when" | "then" | "else"
                    ),
                    Some(Tok::QuotedIdent(_)) => true,
                    _ => false,
                });
        let no_space_after_previous =
            matches!(previous, None | Some(Tok::Op("::" | "." | "(" | "[")));
        if !no_space_before && !no_space_after_previous {
            output
                .write_char(' ')
                .map_err(|_| crate::sql::query::arena_full_pub())?;
        }
        if let Tok::Str(value) = token.token {
            output
                .write_str(crate::sql::eval::quote_literal_str(value, arena)?)
                .map_err(|_| crate::sql::query::arena_full_pub())?;
        } else {
            output
                .write_str(&source[token.start..token.end])
                .map_err(|_| crate::sql::query::arena_full_pub())?;
        }
        previous = Some(token.token);
    }
    if parenthesize {
        output
            .write_char(')')
            .map_err(|_| crate::sql::query::arena_full_pub())?;
    }
    Ok(output.as_str())
}

fn decode_display<'a>(
    encoded: &str,
    bytes: &'a mut [u8; crate::storage::DEFAULT_EXPR_MAX],
) -> Result<&'a str, SqlError> {
    if encoded.is_empty() {
        return Err(corrupt());
    }
    let encoded = encoded.as_bytes();
    let mut input = 0;
    let mut length = 0;
    while input < encoded.len() {
        let byte = if encoded[input] == b'%' {
            let pair = encoded.get(input + 1..input + 3).ok_or_else(corrupt)?;
            let high = (pair[0] as char).to_digit(16).ok_or_else(corrupt)?;
            let low = (pair[1] as char).to_digit(16).ok_or_else(corrupt)?;
            input += 3;
            (high * 16 + low) as u8
        } else {
            let byte = encoded[input];
            input += 1;
            byte
        };
        *bytes.get_mut(length).ok_or_else(corrupt)? = byte;
        length += 1;
    }
    let text = core::str::from_utf8(&bytes[..length]).map_err(|_| corrupt())?;
    if text.contains('\0') {
        return Err(corrupt());
    }
    Ok(text)
}

fn split_display(body: &str) -> Result<(Option<&str>, &str), SqlError> {
    let Some(display) = body.strip_prefix("/*p3o:") else {
        return Ok((None, body));
    };
    let (encoded, body) = display.split_once("*/\n").ok_or_else(corrupt)?;
    let mut bytes = [0; crate::storage::DEFAULT_EXPR_MAX];
    decode_display(encoded, &mut bytes)?;
    Ok((Some(encoded), body))
}

/// PostgreSQL keeps typdefault's original display text independently from
/// the executable expression, which resolves current enum labels and names.
pub(crate) fn default_display<'a>(source: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    if let Some(body) = source.strip_prefix(HEADER) {
        let (display, _) = split_display(body)?;
        let mut bytes = [0; crate::storage::DEFAULT_EXPR_MAX];
        let display = decode_display(display.ok_or_else(corrupt)?, &mut bytes)?;
        arena
            .alloc_str(display)
            .map_err(|_| crate::sql::query::arena_full_pub())
    } else if source.starts_with("pos3ql-enum-check-") {
        Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "unsupported enum expression encoding"
        ))
    } else {
        arena
            .alloc_str(source)
            .map_err(|_| crate::sql::query::arena_full_pub())
    }
}

fn corrupt() -> SqlError {
    sql_err!(
        sqlstate::INTERNAL_ERROR,
        "invalid stored enum CHECK binding"
    )
}

fn enum_slot(base: ColType) -> Option<crate::storage::TypeCreation> {
    match base {
        ColType::Enum(slot) => slot,
        ColType::Array(element) => match element.to_coltype() {
            ColType::Enum(slot) => slot,
            _ => None,
        },
        _ => None,
    }
}
