//! Bind durable enum member identities to the selected catalog image.

pub(crate) mod checks;

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
        label,
        ..
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
    let definition = storage
        .read_enum_definition(
            crate::storage::TypeDefinitionTarget::Slot(crate::storage::NamedTypeSlot::Enum(
                usize::from(slot),
            )),
            txid,
        )?
        .ok_or_else(corrupt)?;
    if !definition.visible_to(txid) {
        return Err(corrupt());
    }
    let index = match member {
        Some(index) => index.ordinal() as usize,
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
    Ok(Datum::Enum {
        slot,
        member: Some(
            crate::sql::types::EnumMemberId::from_ordinal(index as u32)
                .expect("validated enum member ordinal"),
        ),
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
    let definition = storage
        .read_enum_definition(
            crate::storage::TypeDefinitionTarget::Slot(crate::storage::NamedTypeSlot::Enum(
                usize::from(slot),
            )),
            txid,
        )?
        .ok_or_else(corrupt)?;
    let member = match member {
        Some(index) => definition.members().nth(index.ordinal() as usize),
        None => definition.members().find(|member| {
            member
                .legacy_label
                .is_some_and(|legacy| legacy.as_str() == label)
        }),
    };
    member.map(|member| member.sort).ok_or_else(corrupt)
}

// The prefix is not SQL. Only catalog writers create this durable grammar;
// client comments cannot forge a binding.
const QUERY_HEADER: &str = "pos3ql-bound-query-v1\n";

pub(crate) fn is_bound_query(source: &str) -> bool {
    source.starts_with(QUERY_HEADER)
}

#[derive(Clone, Copy)]
struct QueryToken<'a> {
    token: super::lexer::Tok<'a>,
    start: usize,
    end: usize,
}

fn query_tokens<'a>(source: &'a str, arena: &'a Arena) -> Result<&'a [QueryToken<'a>], SqlError> {
    use super::lexer::{Lexer, Tok};
    let lexical = |error: super::lexer::LexError| {
        sql_err!(
            error.kind.sqlstate(),
            "invalid stored SQL at byte {}: {}",
            error.at,
            error.message
        )
    };
    let mut lexer = Lexer::new(source, arena);
    let mut count = 0;
    while lexer.next_token().map_err(lexical)? != Tok::Eof {
        count += 1;
    }
    let tokens = arena
        .alloc_slice_with(count, |_| QueryToken {
            token: Tok::Eof,
            start: 0,
            end: 0,
        })
        .map_err(|_| super::query::arena_full_pub())?;
    let mut lexer = Lexer::new(source, arena);
    for token in tokens.iter_mut() {
        token.token = lexer.next_token().map_err(lexical)?;
        token.start = lexer.token_start();
        token.end = lexer.token_end();
    }
    Ok(tokens)
}

fn string_parameter_positions<'a>(
    tokens: &[QueryToken<'_>],
    offset: usize,
    arena: &'a Arena,
) -> Result<&'a [usize], SqlError> {
    let count = tokens
        .iter()
        .filter(|token| matches!(token.token, super::lexer::Tok::Str(_)))
        .count();
    let positions = arena
        .alloc_slice_with(count, |_| 0)
        .map_err(|_| super::query::arena_full_pub())?;
    let mut index = 0;
    for token in tokens {
        if matches!(token.token, super::lexer::Tok::Str(_)) {
            positions[index] = token.start.checked_add(offset).ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "stored literal position is too large"
                )
            })?;
            index += 1;
        }
    }
    Ok(positions)
}

fn write_name<const N: usize>(output: &mut crate::util::StackStr<N>, name: &str) {
    use core::fmt::Write;
    let _ = output.write_char('"');
    for character in name.chars() {
        if character == '"' {
            let _ = output.write_char('"');
        }
        let _ = output.write_char(character);
    }
    let _ = output.write_char('"');
}

pub(crate) fn bind_query<const N: usize>(
    source: &str,
    storage: &Storage,
    txid: u32,
    path: crate::storage::PathContext,
    dependencies: &mut crate::storage::StoredQueryDependencies,
    legacy: bool,
    arena: &Arena,
) -> Result<crate::util::StackStr<N>, SqlError> {
    use super::lexer::Tok;
    use crate::storage::{DependencyClass, StoredQueryDependency};
    use core::fmt::Write;
    let tokens = query_tokens(source, arena)?;
    let count = tokens
        .iter()
        .filter(|token| matches!(token.token, Tok::Str(_)))
        .count();
    if count == 0 || !storage.has_visible_enums(txid) {
        return Ok(crate::util::StackStr::from_str(source));
    }
    let positions = string_parameter_positions(tokens, 0, arena)?;
    let query_source = arena
        .alloc_str(source)
        .map_err(|_| super::query::arena_full_pub())?;
    let select = super::parser::parse_query_with_string_parameters(query_source, arena, positions)?;
    let oids = arena
        .alloc_slice_with(count, |_| super::types::oid::TEXT)
        .map_err(|_| super::query::arena_full_pub())?;
    let captured_types = |name: &str| super::query::captured_type_oid(name, dependencies.view());
    super::param_types::infer_declared(
        storage,
        &super::ast::Stmt::Select(*select),
        txid,
        oids,
        Some(&captured_types),
    );
    let select =
        super::query::expand_stored_query(select, storage, txid, path, dependencies.view(), arena)?;
    super::param_types::infer(
        storage,
        &super::ast::Stmt::Select(*select),
        txid,
        arena,
        oids,
    );
    let mut output = crate::util::StackStr::<N>::new();
    let _ = output.write_str(QUERY_HEADER);
    let mut parameter = 0;
    let mut any_binding = false;
    for token in tokens {
        let mut binding = None;
        if let Tok::Str(label) = token.token {
            let oid = oids[parameter];
            parameter += 1;
            if (super::types::oid::FIRST_ENUM
                ..super::types::oid::FIRST_ENUM + storage.enum_count() as i32)
                .contains(&oid)
            {
                let slot = (oid - super::types::oid::FIRST_ENUM) as usize;
                let definition = storage.enum_for(slot, txid);
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
                    super::exec::ensure_enum_value_is_safe(storage, slot as u16, label, txid)?;
                }
                let dependency = dependencies.view().entries().iter().position(|dependency| {
                    dependency.class == DependencyClass::Enum
                        && usize::from(dependency.slot) == slot
                });
                let dependency = match dependency {
                    Some(index) => index,
                    None => {
                        let index = dependencies.view().entries().len();
                        dependencies.push(StoredQueryDependency {
                            class: DependencyClass::Enum,
                            slot: slot as u16,
                            identity: storage.capture_type_dependency(
                                DependencyClass::Enum,
                                slot,
                                txid,
                            )?,
                            referenced_columns: crate::storage::ColumnSet::EMPTY,
                            schema: definition.schema,
                            name: definition.name,
                            referenced_schema: definition.schema,
                            referenced_name: definition.name,
                        })?;
                        index
                    }
                };
                any_binding = true;
                let referenced = dependencies.view().entries()[dependency];
                binding = Some((
                    dependency,
                    member,
                    referenced.referenced_schema,
                    referenced.referenced_name,
                ));
            }
        }
        if let Some((dependency, member, _, _)) = binding {
            let _ = write!(output, "/*p3e:{dependency}:{member}*/");
        }
        let _ = output.write_str(&source[token.start..token.end]);
        if let Some((_, _, schema, name)) = binding {
            let _ = output.write_str("::");
            if !schema.as_str().is_empty() {
                write_name(&mut output, schema.as_str());
                let _ = output.write_char('.');
            }
            write_name(&mut output, name.as_str());
        }
        let _ = output.write_char(' ');
    }
    if !any_binding {
        return Ok(crate::util::StackStr::from_str(source));
    }
    if output.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "stored enum binding exceeds {} bytes",
            N
        ));
    }
    Ok(output)
}

pub(crate) fn resolve_query<const N: usize>(
    source: &str,
    storage: &Storage,
    txid: u32,
    path: crate::storage::PathContext,
    dependencies: &mut crate::storage::StoredQueryDependencies,
    arena: &Arena,
) -> Result<crate::util::StackStr<N>, SqlError> {
    use super::lexer::Tok;
    use core::fmt::Write;
    let legacy;
    let source = if let Some(source) = source.strip_prefix(QUERY_HEADER) {
        source
    } else {
        if source.starts_with("pos3ql-bound-query-") {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "unsupported stored query encoding"
            ));
        }
        legacy = bind_query::<N>(source, storage, txid, path, dependencies, true, arena)?;
        let Some(source) = legacy.as_str().strip_prefix(QUERY_HEADER) else {
            return Ok(legacy);
        };
        source
    };
    let tokens = query_tokens(source, arena)?;
    let mut output = crate::util::StackStr::<N>::new();
    let mut previous = 0;
    for token in tokens {
        let gap = source[previous..token.start].trim();
        let binding = gap
            .strip_prefix("/*p3e:")
            .and_then(|gap| gap.strip_suffix("*/"));
        if let Some(binding) = binding {
            if !matches!(token.token, Tok::Str(_)) {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "stored enum binding does not precede a literal"
                ));
            }
            let (dependency, member) = binding
                .split_once(':')
                .and_then(|(dependency, member)| {
                    Some((
                        dependency.parse::<usize>().ok()?,
                        member.parse::<usize>().ok()?,
                    ))
                })
                .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "invalid stored enum binding"))?;
            let dependency = dependencies
                .view()
                .entries()
                .get(dependency)
                .copied()
                .filter(|dependency| dependency.class == crate::storage::DependencyClass::Enum)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "stored enum binding has no type dependency"
                    )
                })?;
            let definition = storage.enum_for(usize::from(dependency.slot), txid);
            let member = definition.members().nth(member).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "stored enum binding has no member"
                )
            })?;
            let _ = output.write_str("E'");
            for character in member.label.as_str().chars() {
                if matches!(character, '\'' | '\\') {
                    let _ = output.write_char(character);
                }
                let _ = output.write_char(character);
            }
            let _ = output.write_char('\'');
        } else {
            let _ = output.write_str(&source[token.start..token.end]);
        }
        let _ = output.write_char(' ');
        previous = token.end;
    }
    if output.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "resolved stored enum binding exceeds {} bytes",
            N
        ));
    }
    Ok(output)
}

pub(crate) fn legacy_identity(slot: u16, label: &str, storage: &Storage) -> Result<u32, SqlError> {
    let corrupt = || {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "stored enum member has no catalog identity"
        )
    };
    if usize::from(slot) >= storage.enum_count() {
        return Err(corrupt());
    }
    storage
        .enum_for(usize::from(slot), 0)
        .members()
        .position(|member| {
            member
                .legacy_label
                .is_some_and(|legacy| legacy.as_str() == label)
        })
        .map(|index| index as u32)
        .ok_or_else(corrupt)
}

struct SqlString<'a>(&'a str);

impl core::fmt::Display for SqlString<'_> {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        use core::fmt::Write;
        output.write_str("E'")?;
        for character in self.0.chars() {
            if matches!(character, '\'' | '\\') {
                output.write_char(character)?;
            }
            output.write_char(character)?;
        }
        output.write_char('\'')
    }
}

/// Catalog SQL must follow a frozen value's identity, just like execution.
/// Re-render only changed enum constants; preserve other expression sources.
pub(crate) fn default_expression<'a>(
    default: &crate::storage::ColumnDefault,
    base: Option<crate::sql::types::ColType>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let Some(expression) = default.expression() else {
        return Ok(None);
    };
    if default.constant().is_none() {
        if let Some(base) = base {
            return checks::resolve_default(expression.as_str(), base, storage, txid, arena)
                .map(Some);
        }
        if checks::is_bound(expression.as_str()) {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "bound enum default has no declared base type"
            ));
        }
    }
    let copy_original = || {
        arena
            .alloc_str(expression.as_str())
            .map(Some)
            .map_err(|_| super::query::arena_full_pub())
    };
    let Some(constant) = default.constant() else {
        return copy_original();
    };
    let cached = constant.as_datum();
    if !matches!(cached, Datum::Enum { .. })
        && !matches!(cached, Datum::Array { element, .. } if matches!(element.to_coltype(), super::types::ColType::Enum(_)))
    {
        return copy_original();
    }
    let catalog = super::query::storage_catalog(storage, arena, txid);
    let current = super::eval::materialize_enum_values(cached, Some(&catalog), arena)?;
    if current == cached {
        return copy_original();
    }
    let type_name = super::catalog::user_type_name_text(storage, txid, current.type_oid(), arena)?
        .ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "enum default has no declared type"
            )
        })?;
    let value = arena
        .alloc_str_display(current)
        .map_err(|_| super::query::arena_full_pub())?;
    arena
        .alloc_str_display(format_args!("{}::{}", SqlString(value), type_name))
        .map(Some)
        .map_err(|_| super::query::arena_full_pub())
}

pub(crate) fn materialize<'a>(
    value: Datum<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let catalog = super::query::storage_catalog(storage, arena, txid);
    super::eval::materialize_enum_values(value, Some(&catalog), arena)
}
