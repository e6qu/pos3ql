//! PostgreSQL full-text scalar functions.

use crate::sql::ast::Expr;
use crate::sql::full_text::{self, QueryInput, TextSearchConfig};
use crate::sql::types::{ArrElem, Datum};
use crate::sql_err;

use super::super::{ColumnLookup, EvalHooks, SqlError, eval_full, sqlstate};

fn arity(
    name: &str,
    count: usize,
    expected: core::ops::RangeInclusive<usize>,
) -> Result<(), SqlError> {
    if expected.contains(&count) {
        Ok(())
    } else {
        Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "function {}(...) with {} arguments does not exist",
            name,
            count
        ))
    }
}

fn text<'a>(value: Datum<'a>, name: &str) -> Result<Option<&'a str>, SqlError> {
    match value {
        Datum::Null => Ok(None),
        Datum::Text(value) | Datum::Bpchar(value) => Ok(Some(value)),
        Datum::RegObject { name, .. } => Ok(Some(name)),
        other => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "function {} does not accept type OID {}",
            name,
            other.type_oid()
        )),
    }
}

fn config(
    value: Datum<'_>,
    function: &str,
    hooks: &EvalHooks<'_, '_>,
) -> Result<Option<i32>, SqlError> {
    let name = match value {
        Datum::Null => return Ok(None),
        Datum::RegObject {
            type_oid,
            referenced_oid,
            ..
        } if type_oid == crate::sql::types::oid::REGCONFIG => return Ok(Some(referenced_oid)),
        Datum::Text(name) | Datum::Bpchar(name) => name,
        other => {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "function {} does not accept type OID {} as a text-search configuration",
                function,
                other.type_oid()
            ));
        }
    };
    let (schema, unqualified) = name
        .rsplit_once('.')
        .map_or((None, name), |(schema, name)| {
            (Some(schema.trim_matches('"')), name.trim_matches('"'))
        });
    let resolved = if let Some(catalog) = hooks.catalog {
        catalog.resolve_text_search_configuration(schema, unqualified)
    } else {
        match (schema, TextSearchConfig::parse(name)) {
            (None | Some("pg_catalog"), Some(TextSearchConfig::Simple)) => Some(3_748),
            (None | Some("pg_catalog"), Some(TextSearchConfig::English)) => Some(13_248),
            _ => None,
        }
    };
    resolved.map(Some).ok_or_else(|| {
        sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "text search configuration \"{}\" does not exist",
            name
        )
    })
}

fn dictionary(
    value: Datum<'_>,
    function: &str,
    hooks: &EvalHooks<'_, '_>,
) -> Result<Option<i32>, SqlError> {
    let name = match value {
        Datum::Null => return Ok(None),
        Datum::RegObject {
            type_oid,
            referenced_oid,
            ..
        } if type_oid == crate::sql::types::oid::REGDICTIONARY => return Ok(Some(referenced_oid)),
        Datum::Text(name) | Datum::Bpchar(name) => name,
        other => {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "function {} does not accept type OID {} as a text-search dictionary",
                function,
                other.type_oid()
            ));
        }
    };
    let (schema, unqualified) = name
        .rsplit_once('.')
        .map_or((None, name), |(schema, name)| {
            (Some(schema.trim_matches('"')), name.trim_matches('"'))
        });
    let resolved = if let Some(catalog) = hooks.catalog {
        catalog.resolve_text_search_dictionary(schema, unqualified)
    } else {
        match (schema, unqualified) {
            (None | Some("pg_catalog"), "simple") => Some(3_765),
            (None | Some("pg_catalog"), "english_stem") => Some(13_247),
            _ => None,
        }
    };
    resolved.map(Some).ok_or_else(|| {
        sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "text search dictionary \"{}\" does not exist",
            name
        )
    })
}

fn normalize<'a>(
    configuration_oid: i32,
    token_type: u8,
    token: &str,
    arena: &'a crate::mem::arena::Arena,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<full_text::TextSearchLexeme<'a>, SqlError> {
    if let Some(catalog) = hooks.catalog {
        return catalog.normalize_text_search_token(configuration_oid, token_type, token, arena);
    }
    match configuration_oid {
        3_748 | 13_248 => {
            if !matches!(token_type, 1..=11 | 15..=22) {
                return Ok(full_text::TextSearchLexeme::Unmapped);
            }
            let config = if configuration_oid == 3_748 {
                TextSearchConfig::Simple
            } else {
                TextSearchConfig::English
            };
            Ok(match full_text::normalize_token(token, config, arena)? {
                Some(lexeme) => full_text::TextSearchLexeme::Lexeme(lexeme),
                None => full_text::TextSearchLexeme::StopWord,
            })
        }
        _ => Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "text-search catalog access is unavailable"
        )),
    }
}

fn current_config(hooks: &EvalHooks<'_, '_>) -> Result<i32, SqlError> {
    let setting = super::system::session_setting("default_text_search_config")
        .unwrap_or_else(|| crate::util::StackStr::from_str("pg_catalog.english"));
    config(
        Datum::Text(setting.as_str()),
        "default_text_search_config",
        hooks,
    )?
    .ok_or_else(|| {
        sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "default text search configuration is null"
        )
    })
}

fn text_array<'a>(value: Datum<'a>, out: &mut [&'a str; 512]) -> Result<Option<usize>, SqlError> {
    match value {
        Datum::Null => Ok(None),
        Datum::Array {
            element: ArrElem::Text,
            raw,
        } => {
            let count = crate::sql::array::len(raw);
            if count > out.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "text array is too large"
                ));
            }
            for (index, slot) in out[..count].iter_mut().enumerate() {
                *slot = match crate::sql::array::get(raw, ArrElem::Text, index) {
                    Some(Datum::Text(text)) => text,
                    Some(Datum::Null) | None => continue,
                    _ => unreachable!("text array invariant"),
                };
            }
            Ok(Some(count))
        }
        other => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "expected text[], not type OID {}",
            other.type_oid()
        )),
    }
}

fn rank_weights(value: Datum<'_>) -> Result<Option<[f32; 4]>, SqlError> {
    let Datum::Array {
        element: ArrElem::Float4,
        raw,
    } = value
    else {
        if value.is_null() {
            return Ok(None);
        }
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "ts_rank weights must be real[]"
        ));
    };
    if crate::sql::array::len(raw) < 4 {
        return Err(sql_err!(
            sqlstate::ARRAY_SUBSCRIPT_ERROR,
            "array of weight is too short"
        ));
    }
    let defaults = [0.1, 0.2, 0.4, 1.0];
    let mut weights = defaults;
    for index in 0..4 {
        weights[index] = match crate::sql::array::get(raw, ArrElem::Float4, index) {
            Some(Datum::Float4(value)) if value < 0.0 => defaults[index],
            Some(Datum::Float4(value)) if value <= 1.0 => value,
            Some(Datum::Float4(_)) => {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "weight out of range"
                ));
            }
            Some(Datum::Null) | None => {
                return Err(sql_err!(
                    sqlstate::NULL_VALUE_NOT_ALLOWED,
                    "array of weight must not contain nulls"
                ));
            }
            _ => unreachable!("real array invariant"),
        };
    }
    Ok(Some(weights))
}

fn rank_normalization(value: Datum<'_>) -> Result<Option<i32>, SqlError> {
    match value {
        Datum::Null => Ok(None),
        Datum::Int2(value) => Ok(Some(i32::from(value))),
        Datum::Int4(value) => Ok(Some(value)),
        Datum::Int8(value) => i32::try_from(value)
            .map(Some)
            .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "integer out of range")),
        other => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "ts_rank normalization must be integer, not type OID {}",
            other.type_oid()
        )),
    }
}

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
        "to_tsvector"
            | "json_to_tsvector"
            | "jsonb_to_tsvector"
            | "to_tsquery"
            | "plainto_tsquery"
            | "phraseto_tsquery"
            | "websearch_to_tsquery"
            | "strip"
            | "setweight"
            | "ts_delete"
            | "ts_filter"
            | "numnode"
            | "querytree"
            | "ts_rank"
            | "ts_rank_cd"
            | "ts_headline"
            | "get_current_ts_config"
            | "array_to_tsvector"
            | "tsvector_to_array"
            | "tsquery_phrase"
            | "ts_rewrite"
            | "ts_lexize"
    ) {
        return None;
    }
    Some((|| {
        if star {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {}(*) does not exist",
                name
            ));
        }
        match name {
            "to_tsvector" => {
                arity(name, args.len(), 1..=2)?;
                let (config, document_index) = if args.len() == 2 {
                    let value = eval_full(args[0], arena, params, row, hooks)?;
                    let Some(config) = config(value, name, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    (config, 1)
                } else {
                    (current_config(hooks)?, 0)
                };
                let value = eval_full(args[document_index], arena, params, row, hooks)?;
                let rendered = match value {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Text(document) | Datum::Bpchar(document) => {
                        full_text::to_tsvector_with(document, arena, |token_type, token, arena| {
                            normalize(config, token_type, token, arena, hooks)
                        })?
                    }
                    Datum::Json { text, jsonb } => {
                        let value = if jsonb {
                            crate::sql::json::parse(text, arena)?
                        } else {
                            crate::sql::json::parse_source_order(text, arena)?
                        };
                        full_text::json_to_tsvector_with(
                            value,
                            full_text::JsonTextSearchFilter::STRINGS,
                            arena,
                            |token_type, token, arena| {
                                normalize(config, token_type, token, arena, hooks)
                            },
                        )?
                    }
                    other => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "to_tsvector does not accept type OID {}",
                            other.type_oid()
                        ));
                    }
                };
                Ok(Datum::TsVector(full_text::restore_vector(rendered)))
            }
            "json_to_tsvector" | "jsonb_to_tsvector" => {
                arity(name, args.len(), 2..=3)?;
                let (config, value_index) = if args.len() == 3 {
                    let value = eval_full(args[0], arena, params, row, hooks)?;
                    let Some(config) = config(value, name, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    (config, 1)
                } else {
                    (current_config(hooks)?, 0)
                };
                let value = eval_full(args[value_index], arena, params, row, hooks)?;
                let filter = eval_full(args[value_index + 1], arena, params, row, hooks)?;
                let (text, jsonb) = match value {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Json { text, jsonb } => (text, jsonb),
                    other => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "{} does not accept type OID {}",
                            name,
                            other.type_oid()
                        ));
                    }
                };
                if (name == "jsonb_to_tsvector") != jsonb {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "{} requires {}",
                        name,
                        if name.starts_with("jsonb") {
                            "jsonb"
                        } else {
                            "json"
                        }
                    ));
                }
                let filter = match filter {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Json { text, jsonb: true } => full_text::JsonTextSearchFilter::parse(
                        crate::sql::json::parse(text, arena)?,
                    )?,
                    other => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "{} filter requires jsonb, not type OID {}",
                            name,
                            other.type_oid()
                        ));
                    }
                };
                let value = if jsonb {
                    crate::sql::json::parse(text, arena)?
                } else {
                    crate::sql::json::parse_source_order(text, arena)?
                };
                Ok(Datum::TsVector(full_text::restore_vector(
                    full_text::json_to_tsvector_with(
                        value,
                        filter,
                        arena,
                        |token_type, token, arena| {
                            normalize(config, token_type, token, arena, hooks)
                        },
                    )?,
                )))
            }
            "to_tsquery" | "plainto_tsquery" | "phraseto_tsquery" | "websearch_to_tsquery" => {
                arity(name, args.len(), 1..=2)?;
                let (config, query_index) = if args.len() == 2 {
                    let value = eval_full(args[0], arena, params, row, hooks)?;
                    let Some(config) = config(value, name, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    (config, 1)
                } else {
                    (current_config(hooks)?, 0)
                };
                let value = eval_full(args[query_index], arena, params, row, hooks)?;
                let Some(source) = text(value, name)? else {
                    return Ok(Datum::Null);
                };
                let query = if name == "to_tsquery" {
                    full_text::explicit_text_to_query_with(
                        source,
                        arena,
                        |token_type, token, arena| {
                            normalize(config, token_type, token, arena, hooks)
                        },
                    )?
                } else {
                    let mode = match name {
                        "phraseto_tsquery" => QueryInput::Phrase,
                        "websearch_to_tsquery" => QueryInput::Websearch,
                        _ => QueryInput::Plain,
                    };
                    full_text::text_to_query_with(
                        source,
                        mode,
                        arena,
                        |token_type, token, arena| {
                            normalize(config, token_type, token, arena, hooks)
                        },
                    )?
                };
                Ok(Datum::TsQuery(full_text::restore_query(query)))
            }
            "strip" => {
                arity(name, args.len(), 1..=1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::TsVector(vector) => Ok(Datum::TsVector(full_text::restore_vector(
                        full_text::strip_vector(vector.as_str(), arena)?,
                    ))),
                    other => Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "strip requires tsvector, not type OID {}",
                        other.type_oid()
                    )),
                }
            }
            "setweight" => {
                arity(name, args.len(), 2..=3)?;
                let Datum::TsVector(vector) = eval_full(args[0], arena, params, row, hooks)? else {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "setweight requires tsvector"
                    ));
                };
                let Some(weight) = text(eval_full(args[1], arena, params, row, hooks)?, name)?
                else {
                    return Ok(Datum::Null);
                };
                let weight = match weight {
                    "A" => 3,
                    "B" => 2,
                    "C" => 1,
                    "D" => 0,
                    _ => {
                        return Err(sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "unrecognized weight: \"{}\"",
                            weight
                        ));
                    }
                };
                let mut selected = [""; 512];
                let selected = if args.len() == 3 {
                    let count = text_array(
                        eval_full(args[2], arena, params, row, hooks)?,
                        &mut selected,
                    )?;
                    count.map(|count| &selected[..count])
                } else {
                    None
                };
                Ok(Datum::TsVector(full_text::restore_vector(
                    full_text::set_weight(vector.as_str(), weight, selected, arena)?,
                )))
            }
            "ts_delete" => {
                arity(name, args.len(), 2..=2)?;
                let Datum::TsVector(vector) = eval_full(args[0], arena, params, row, hooks)? else {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "ts_delete requires tsvector"
                    ));
                };
                let value = eval_full(args[1], arena, params, row, hooks)?;
                let mut deleted = [""; 512];
                let count = match value {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Text(text) => {
                        deleted[0] = text;
                        1
                    }
                    array => text_array(array, &mut deleted)?.unwrap_or(0),
                };
                Ok(Datum::TsVector(full_text::restore_vector(
                    full_text::delete_lexemes(vector.as_str(), &deleted[..count], arena)?,
                )))
            }
            "ts_filter" => {
                arity(name, args.len(), 2..=2)?;
                let Datum::TsVector(vector) = eval_full(args[0], arena, params, row, hooks)? else {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "ts_filter requires tsvector"
                    ));
                };
                let value = eval_full(args[1], arena, params, row, hooks)?;
                let Datum::Array {
                    element: ArrElem::Char,
                    raw,
                } = value
                else {
                    if value.is_null() {
                        return Ok(Datum::Null);
                    }
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "ts_filter weights must be \"char\"[]"
                    ));
                };
                let count = crate::sql::array::len(raw);
                let mut weights = 0u8;
                for index in 0..count {
                    let weight = match crate::sql::array::get(raw, ArrElem::Char, index) {
                        Some(Datum::Char(weight)) => match weight {
                            b'A' => "A",
                            b'B' => "B",
                            b'C' => "C",
                            b'D' => "D",
                            _ => "",
                        },
                        Some(Datum::Null) | None => {
                            return Err(sql_err!(
                                sqlstate::NULL_VALUE_NOT_ALLOWED,
                                "weight array may not contain nulls"
                            ));
                        }
                        _ => unreachable!("char array invariant"),
                    };
                    weights |= match weight {
                        "A" => 1 << 3,
                        "B" => 1 << 2,
                        "C" => 1 << 1,
                        "D" => 1,
                        _ => {
                            return Err(sql_err!(
                                sqlstate::INVALID_PARAMETER_VALUE,
                                "unrecognized weight: \"{}\"",
                                weight
                            ));
                        }
                    };
                }
                Ok(Datum::TsVector(full_text::restore_vector(
                    full_text::filter_weights(vector.as_str(), weights, arena)?,
                )))
            }
            "numnode" => {
                arity(name, args.len(), 1..=1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::TsQuery(query) => Ok(Datum::Int4(full_text::query_node_count(
                        query.as_str(),
                        arena,
                    )?)),
                    other => Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "numnode requires tsquery, not type OID {}",
                        other.type_oid()
                    )),
                }
            }
            "querytree" => {
                arity(name, args.len(), 1..=1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::TsQuery(query) => {
                        Ok(Datum::Text(full_text::query_tree(query.as_str(), arena)?))
                    }
                    other => Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "querytree requires tsquery, not type OID {}",
                        other.type_oid()
                    )),
                }
            }
            "ts_rank" | "ts_rank_cd" => {
                arity(name, args.len(), 2..=4)?;
                let first = eval_full(args[0], arena, params, row, hooks)?;
                let (weights, vector, query, normalization) = match args.len() {
                    2 => (
                        [0.1, 0.2, 0.4, 1.0],
                        first,
                        eval_full(args[1], arena, params, row, hooks)?,
                        0,
                    ),
                    3 if matches!(first, Datum::Array { .. } | Datum::Null) => {
                        let Some(weights) = rank_weights(first)? else {
                            return Ok(Datum::Null);
                        };
                        (
                            weights,
                            eval_full(args[1], arena, params, row, hooks)?,
                            eval_full(args[2], arena, params, row, hooks)?,
                            0,
                        )
                    }
                    3 => {
                        let query = eval_full(args[1], arena, params, row, hooks)?;
                        let Some(normalization) =
                            rank_normalization(eval_full(args[2], arena, params, row, hooks)?)?
                        else {
                            return Ok(Datum::Null);
                        };
                        ([0.1, 0.2, 0.4, 1.0], first, query, normalization)
                    }
                    4 => {
                        let Some(weights) = rank_weights(first)? else {
                            return Ok(Datum::Null);
                        };
                        let vector = eval_full(args[1], arena, params, row, hooks)?;
                        let query = eval_full(args[2], arena, params, row, hooks)?;
                        let Some(normalization) =
                            rank_normalization(eval_full(args[3], arena, params, row, hooks)?)?
                        else {
                            return Ok(Datum::Null);
                        };
                        (weights, vector, query, normalization)
                    }
                    _ => unreachable!("rank arity guard"),
                };
                let (Datum::TsVector(vector), Datum::TsQuery(query)) = (vector, query) else {
                    if vector.is_null() || query.is_null() {
                        return Ok(Datum::Null);
                    }
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "{} requires tsvector and tsquery",
                        name
                    ));
                };
                Ok(Datum::Float4(full_text::rank_with_options(
                    vector.as_str(),
                    query.as_str(),
                    name == "ts_rank_cd",
                    weights,
                    normalization,
                    arena,
                )?))
            }
            "ts_headline" => {
                arity(name, args.len(), 2..=4)?;
                let first = eval_full(args[0], arena, params, row, hooks)?;
                let second = eval_full(args[1], arena, params, row, hooks)?;
                let (configuration, document_value, query_value, options) =
                    if args.len() == 2 || matches!(second, Datum::TsQuery(_)) {
                        let options = if args.len() == 3 {
                            text(eval_full(args[2], arena, params, row, hooks)?, name)?
                        } else {
                            None
                        };
                        (current_config(hooks)?, first, second, options)
                    } else {
                        let Some(configuration) = config(first, name, hooks)? else {
                            return Ok(Datum::Null);
                        };
                        let query = eval_full(args[2], arena, params, row, hooks)?;
                        let options = if args.len() == 4 {
                            text(eval_full(args[3], arena, params, row, hooks)?, name)?
                        } else {
                            None
                        };
                        (configuration, second, query, options)
                    };
                let document = match document_value {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Text(document) | Datum::Bpchar(document) => (document, None),
                    Datum::Json { text, jsonb } => (text, Some(jsonb)),
                    other => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "ts_headline does not accept type OID {}",
                            other.type_oid()
                        ));
                    }
                };
                let Datum::TsQuery(query) = query_value else {
                    if query_value.is_null() {
                        return Ok(Datum::Null);
                    }
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "ts_headline requires tsquery"
                    ));
                };
                let mut start = "<b>";
                let mut stop = "</b>";
                let mut fragment_delimiter = " ... ";
                let mut min_words = 15i32;
                let mut max_words = 35i32;
                let mut short_word = 3i32;
                let mut max_fragments = 0i32;
                let mut highlight_all = false;
                if let Some(option_text) = options {
                    for option in option_text.split(',') {
                        let (key, value) = option.split_once('=').ok_or_else(|| {
                            sql_err!(
                                sqlstate::INVALID_PARAMETER_VALUE,
                                "invalid ts_headline option: {}",
                                option.trim()
                            )
                        })?;
                        let key = key.trim();
                        let value = value.trim();
                        if key.eq_ignore_ascii_case("StartSel") {
                            start = value;
                        } else if key.eq_ignore_ascii_case("StopSel") {
                            stop = value;
                        } else if key.eq_ignore_ascii_case("FragmentDelimiter") {
                            fragment_delimiter = value;
                        } else if key.eq_ignore_ascii_case("HighlightAll") {
                            highlight_all = value == "1"
                                || value.eq_ignore_ascii_case("on")
                                || value.eq_ignore_ascii_case("true")
                                || value.eq_ignore_ascii_case("t")
                                || value.eq_ignore_ascii_case("y")
                                || value.eq_ignore_ascii_case("yes");
                        } else {
                            let parsed = value.parse::<i32>().map_err(|_| {
                                sql_err!(
                                    sqlstate::INVALID_TEXT_REPRESENTATION,
                                    "invalid input syntax for type integer: \"{}\"",
                                    value
                                )
                            })?;
                            if key.eq_ignore_ascii_case("MinWords") {
                                min_words = parsed;
                            } else if key.eq_ignore_ascii_case("MaxWords") {
                                max_words = parsed;
                            } else if key.eq_ignore_ascii_case("ShortWord") {
                                short_word = parsed;
                            } else if key.eq_ignore_ascii_case("MaxFragments") {
                                max_fragments = parsed;
                            } else {
                                return Err(sql_err!(
                                    sqlstate::INVALID_PARAMETER_VALUE,
                                    "unrecognized headline parameter: \"{}\"",
                                    key
                                ));
                            }
                        }
                    }
                }
                if start.len() > i16::MAX as usize
                    || stop.len() > i16::MAX as usize
                    || fragment_delimiter.len() > i16::MAX as usize
                {
                    return Err(sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "headline selection marker is too long"
                    ));
                }
                if !highlight_all {
                    if min_words >= max_words {
                        return Err(sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "MinWords must be less than MaxWords"
                        ));
                    }
                    if min_words <= 0 {
                        return Err(sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "MinWords must be positive"
                        ));
                    }
                    if short_word < 0 {
                        return Err(sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "ShortWord must be >= 0"
                        ));
                    }
                    if max_fragments < 0 {
                        return Err(sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "MaxFragments must be >= 0"
                        ));
                    }
                }
                let headline_options = full_text::HeadlineOptions {
                    start,
                    stop,
                    fragment_delimiter,
                    min_words: min_words.max(1) as usize,
                    max_words: max_words.max(1) as usize,
                    short_word: short_word.max(0) as usize,
                    max_fragments: max_fragments.max(0) as usize,
                    highlight_all,
                };
                let mut headline = |document, arena| {
                    full_text::headline_with(
                        document,
                        query.as_str(),
                        headline_options,
                        arena,
                        |token_type, token, arena| {
                            normalize(configuration, token_type, token, arena, hooks)
                        },
                    )
                };
                let Some(jsonb) = document.1 else {
                    return Ok(Datum::Text(headline(document.0, arena)?));
                };
                let value = if jsonb {
                    crate::sql::json::parse(document.0, arena)?
                } else {
                    crate::sql::json::parse_source_order(document.0, arena)?
                };
                let value = crate::sql::json::map_string_values(value, arena, &mut headline)?;
                let rendered = if jsonb {
                    super::super::json_to_text_pub(&value, arena)?
                } else {
                    super::super::json_to_text_compact(&value, arena)?
                };
                Ok(Datum::Json {
                    text: rendered,
                    jsonb,
                })
            }
            "get_current_ts_config" => {
                arity(name, args.len(), 0..=0)?;
                let setting = super::system::session_setting("default_text_search_config")
                    .unwrap_or_else(|| crate::util::StackStr::from_str("pg_catalog.english"));
                let oid = config(Datum::Text(setting.as_str()), name, hooks)?.ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "default text search configuration is null"
                    )
                })?;
                let display = if let Some(catalog) = hooks.catalog {
                    catalog.text_search_configuration_name(oid)
                } else {
                    match oid {
                        3_748 => Some(crate::util::StackStr::from_str("simple")),
                        13_248 => Some(crate::util::StackStr::from_str("english")),
                        _ => None,
                    }
                }
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "text search configuration with OID {} does not exist",
                        oid
                    )
                })?;
                let stored = arena
                    .alloc_slice_copy(display.as_str().as_bytes())
                    .map_err(|_| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "text-search configuration name exceeds the statement arena"
                        )
                    })?;
                let stored = core::str::from_utf8(stored).expect("configuration setting is UTF-8");
                Ok(Datum::RegObject {
                    type_oid: crate::sql::types::oid::REGCONFIG,
                    referenced_oid: oid,
                    name: stored,
                })
            }
            "array_to_tsvector" => {
                arity(name, args.len(), 1..=1)?;
                let value = eval_full(args[0], arena, params, row, hooks)?;
                let Datum::Array {
                    element: ArrElem::Text,
                    raw,
                } = value
                else {
                    if value.is_null() {
                        return Ok(Datum::Null);
                    }
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "array_to_tsvector requires text[]"
                    ));
                };
                let count = crate::sql::array::len(raw);
                let mut lexemes = [""; 512];
                if count > lexemes.len() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "lexeme array is too large"
                    ));
                }
                for (index, lexeme) in lexemes[..count].iter_mut().enumerate() {
                    *lexeme = match crate::sql::array::get(raw, ArrElem::Text, index) {
                        Some(Datum::Text(text)) => text,
                        Some(Datum::Null) | None => {
                            return Err(sql_err!(
                                sqlstate::NULL_VALUE_NOT_ALLOWED,
                                "lexeme array may not contain nulls"
                            ));
                        }
                        _ => unreachable!("text array invariant"),
                    };
                }
                Ok(Datum::TsVector(full_text::restore_vector(
                    full_text::array_to_vector(&lexemes[..count], arena)?,
                )))
            }
            "tsvector_to_array" => {
                arity(name, args.len(), 1..=1)?;
                let value = eval_full(args[0], arena, params, row, hooks)?;
                let Datum::TsVector(source) = value else {
                    if value.is_null() {
                        return Ok(Datum::Null);
                    }
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "tsvector_to_array requires tsvector"
                    ));
                };
                let vector = full_text::parse_vector(source.as_str(), arena)?;
                let mut values = [Datum::Null; 512];
                let mut count = 0usize;
                let mut previous = None;
                for index in 0..vector.lexeme_count() {
                    let (lexeme, _) = vector.lexeme(index).expect("vector index");
                    if previous == Some(lexeme) {
                        continue;
                    }
                    values[count] = Datum::Text(lexeme);
                    count += 1;
                    previous = Some(lexeme);
                }
                Ok(Datum::Array {
                    element: ArrElem::Text,
                    raw: crate::sql::array::build(&values[..count], arena)?,
                })
            }
            "tsquery_phrase" => {
                arity(name, args.len(), 2..=3)?;
                let left = eval_full(args[0], arena, params, row, hooks)?;
                let right = eval_full(args[1], arena, params, row, hooks)?;
                let (Datum::TsQuery(left), Datum::TsQuery(right)) = (left, right) else {
                    if left.is_null() || right.is_null() {
                        return Ok(Datum::Null);
                    }
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "tsquery_phrase requires tsquery arguments"
                    ));
                };
                let distance = if args.len() == 3 {
                    match eval_full(args[2], arena, params, row, hooks)? {
                        Datum::Null => return Ok(Datum::Null),
                        Datum::Int2(value) if value >= 0 => value as u16,
                        Datum::Int4(value) if (0..=16_384).contains(&value) => value as u16,
                        Datum::Int8(value) if (0..=16_384).contains(&value) => value as u16,
                        Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_) => {
                            return Err(sql_err!(
                                sqlstate::INVALID_PARAMETER_VALUE,
                                "distance in tsquery_phrase must be an integer value between zero and 16384 inclusive"
                            ));
                        }
                        _ => {
                            return Err(sql_err!(
                                sqlstate::DATATYPE_MISMATCH,
                                "tsquery_phrase distance must be integer"
                            ));
                        }
                    }
                } else {
                    1
                };
                Ok(Datum::TsQuery(full_text::restore_query(
                    full_text::phrase_queries_distance(
                        left.as_str(),
                        right.as_str(),
                        distance,
                        arena,
                    )?,
                )))
            }
            "ts_rewrite" => {
                arity(name, args.len(), 2..=3)?;
                let source = eval_full(args[0], arena, params, row, hooks)?;
                if args.len() == 2 {
                    let Datum::TsQuery(source) = source else {
                        if source.is_null() {
                            return Ok(Datum::Null);
                        }
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "ts_rewrite requires a tsquery source"
                        ));
                    };
                    let Some(query) = text(eval_full(args[1], arena, params, row, hooks)?, name)?
                    else {
                        return Ok(Datum::Null);
                    };
                    let catalog = hooks.catalog.ok_or_else(|| {
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "ts_rewrite query execution requires catalog access"
                        )
                    })?;
                    return Ok(Datum::TsQuery(full_text::restore_query(
                        catalog.rewrite_text_search_query(source.as_str(), query, arena)?,
                    )));
                }
                let target = eval_full(args[1], arena, params, row, hooks)?;
                let replacement = eval_full(args[2], arena, params, row, hooks)?;
                let (Datum::TsQuery(source), Datum::TsQuery(target), Datum::TsQuery(replacement)) =
                    (source, target, replacement)
                else {
                    if source.is_null() || target.is_null() || replacement.is_null() {
                        return Ok(Datum::Null);
                    }
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "ts_rewrite requires tsquery arguments"
                    ));
                };
                Ok(Datum::TsQuery(full_text::restore_query(
                    full_text::rewrite_query(
                        source.as_str(),
                        target.as_str(),
                        replacement.as_str(),
                        arena,
                    )?,
                )))
            }
            "ts_lexize" => {
                arity(name, args.len(), 2..=2)?;
                let Some(dictionary_oid) =
                    dictionary(eval_full(args[0], arena, params, row, hooks)?, name, hooks)?
                else {
                    return Ok(Datum::Null);
                };
                let Some(token) = text(eval_full(args[1], arena, params, row, hooks)?, name)?
                else {
                    return Ok(Datum::Null);
                };
                let lexeme = if let Some(catalog) = hooks.catalog {
                    catalog.lexize_text_search_dictionary(dictionary_oid, token, arena)?
                } else {
                    match dictionary_oid {
                        3_765 => match full_text::normalize_token(
                            token,
                            TextSearchConfig::Simple,
                            arena,
                        )? {
                            Some(value) => full_text::TextSearchLexeme::Lexeme(value),
                            None => full_text::TextSearchLexeme::StopWord,
                        },
                        13_247 => match full_text::normalize_token(
                            token,
                            TextSearchConfig::English,
                            arena,
                        )? {
                            Some(value) => full_text::TextSearchLexeme::Lexeme(value),
                            None => full_text::TextSearchLexeme::StopWord,
                        },
                        _ => {
                            return Err(sql_err!(
                                sqlstate::UNDEFINED_OBJECT,
                                "text search dictionary with OID {} does not exist",
                                dictionary_oid
                            ));
                        }
                    }
                };
                match lexeme {
                    full_text::TextSearchLexeme::Unmapped => Ok(Datum::Null),
                    full_text::TextSearchLexeme::StopWord => Ok(Datum::Array {
                        element: ArrElem::Text,
                        raw: crate::sql::array::build(&[], arena)?,
                    }),
                    full_text::TextSearchLexeme::Lexeme(value) => Ok(Datum::Array {
                        element: ArrElem::Text,
                        raw: crate::sql::array::build(&[Datum::Text(value)], arena)?,
                    }),
                }
            }
            _ => unreachable!("full-text function dispatch guard"),
        }
    })())
}
