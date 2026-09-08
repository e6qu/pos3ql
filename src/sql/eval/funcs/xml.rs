//! PostgreSQL SQL/XML scalar constructors, validators and query functions.

use core::fmt::Write as _;

use crate::sql::ast::Expr;
use crate::sql::types::{ColType, Datum};
use crate::sql::xml::{self, Mode};
use crate::sql_err;

use super::super::{
    ColumnLookup, EvalHooks, SqlError, arena_full, arity_err, eval_full, sqlstate, type_mismatch,
};

const XML_RESULT_MAX: usize = 65_536;

fn value_text<'a>(
    value: Datum<'a>,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Option<(&'a str, bool)>, SqlError> {
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(match value {
        Datum::Xml(text) => (text, true),
        other => (crate::sql::eval::cast::cast_to_text(other, arena)?, false),
    }))
}

fn push_escaped(out: &mut crate::util::StackStr<XML_RESULT_MAX>, text: &str, attribute: bool) {
    xml::escape_text(text, |part| {
        if attribute {
            for character in part.chars() {
                match character {
                    '"' => {
                        let _ = out.write_str("&quot;");
                    }
                    '\r' => {
                        let _ = out.write_str("&#xD;");
                    }
                    '\n' => {
                        let _ = out.write_str("&#xA;");
                    }
                    '\t' => {
                        let _ = out.write_str("&#x9;");
                    }
                    _ => {
                        let _ = out.write_char(character);
                    }
                }
            }
        } else {
            let _ = out.write_str(part);
        }
    });
}

fn xml_name(out: &mut crate::util::StackStr<XML_RESULT_MAX>, name: &str) {
    for (index, character) in name.chars().enumerate() {
        let valid = if index == 0 {
            character == ':'
                || character == '_'
                || character.is_alphabetic()
                || character as u32 >= 0x80
        } else {
            character == ':'
                || character == '_'
                || character == '-'
                || character == '.'
                || character.is_alphanumeric()
                || character as u32 >= 0x80
        };
        if valid {
            let _ = out.write_char(character);
        } else {
            let _ = write!(out, "_x{:04X}_", character as u32);
        }
    }
}

fn finish<'a>(
    out: &crate::util::StackStr<XML_RESULT_MAX>,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Datum<'a>, SqlError> {
    if out.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "SQL/XML result exceeds {} bytes",
            XML_RESULT_MAX
        ));
    }
    Ok(Datum::Xml(
        arena.alloc_str(out.as_str()).map_err(|_| arena_full())?,
    ))
}

fn xpath_namespace_pairs<'a>(
    value: Datum<'a>,
    output: &mut [(&'a str, &'a str)],
) -> Result<Option<usize>, SqlError> {
    if value.is_null() {
        return Ok(None);
    }
    let Datum::Array { element, raw } = value else {
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "XPath namespace mappings must be a two-dimensional text array"
        ));
    };
    if element != crate::sql::types::ArrElem::Text {
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "XPath namespace mappings must be a two-dimensional text array"
        ));
    }
    let shape = crate::sql::array::shape(raw).ok_or_else(|| {
        sql_err!(
            sqlstate::DATA_EXCEPTION,
            "invalid XPath namespace mapping array"
        )
    })?;
    if shape.dimension_count() != 2 || shape.dimension(1) != Some(2) {
        return Err(sql_err!(
            sqlstate::ARRAY_SUBSCRIPT_ERROR,
            "XPath namespace mappings must have two columns"
        ));
    }
    let rows = shape.dimension(0).unwrap_or(0);
    if rows > output.len() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "too many XPath namespace mappings"
        ));
    }
    let mut count = 0usize;
    for row in 0..rows {
        let prefix = crate::sql::array::get(raw, element, row * 2)
            .ok_or_else(|| sql_err!(sqlstate::DATA_EXCEPTION, "invalid XPath namespace mapping"))?;
        let uri = crate::sql::array::get(raw, element, row * 2 + 1)
            .ok_or_else(|| sql_err!(sqlstate::DATA_EXCEPTION, "invalid XPath namespace mapping"))?;
        let (Datum::Text(prefix), Datum::Text(uri)) = (prefix, uri) else {
            return Err(sql_err!(
                sqlstate::NULL_VALUE_NOT_ALLOWED,
                "null XPath namespace mapping"
            ));
        };
        if prefix.is_empty() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "could not register XML namespace with an empty name"
            ));
        }
        if let Some(existing) = output[..count]
            .iter_mut()
            .find(|(prior, _)| *prior == prefix)
        {
            *existing = (prefix, uri);
        } else {
            output[count] = (prefix, uri);
            count += 1;
        }
    }
    Ok(Some(count))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch<'a>(
    name: &str,
    args: &[&'a Expr<'a>],
    star: bool,
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Option<Result<Datum<'a>, SqlError>> {
    if !name.starts_with("__xml")
        && !matches!(
            name,
            "xmlconcat"
                | "xmlconcat2"
                | "xmlcomment"
                | "xml_is_well_formed"
                | "xml_is_well_formed_document"
                | "xml_is_well_formed_content"
                | "xpath"
                | "xpath_exists"
        )
    {
        return None;
    }
    let arity = |n: usize| -> Result<(), SqlError> {
        if args.len() != n || star {
            Err(arity_err(name, if star { 1 } else { args.len() }))
        } else {
            Ok(())
        }
    };
    Some((|| match name {
        "__xmlparse_content" | "__xmlparse_document" => {
            arity(1)?;
            let value = eval_full(args[0], arena, params, row, hooks)?;
            if value.is_null() {
                return Ok(Datum::Null);
            }
            let text = match value {
                Datum::Text(text) | Datum::Xml(text) => text,
                other => return Err(type_mismatch("XMLPARSE input must be text", &other)),
            };
            xml::validate(
                text,
                if name.ends_with("document") {
                    Mode::Document
                } else {
                    Mode::Content
                },
            )?;
            Ok(Datum::Xml(text))
        }
        "__xmlserialize" => {
            arity(4)?;
            let value = eval_full(args[0], arena, params, row, hooks)?;
            if value.is_null() {
                return Ok(Datum::Null);
            }
            let text = match value {
                Datum::Xml(text) => text,
                other => return Err(type_mismatch("XMLSERIALIZE input must be xml", &other)),
            };
            let target = match *args[1] {
                Expr::Str(target) => ColType::from_sql_name(target).unwrap(),
                _ => unreachable!(),
            };
            let type_mod = match *args[2] {
                Expr::Int(value) => value as i32,
                _ => unreachable!(),
            };
            if matches!(*args[3], Expr::Bool(true)) {
                xml::validate(text, Mode::Document)?;
            }
            crate::sql::exec::apply_cast_typmod(Datum::Text(text), target, type_mod, arena)
        }
        "__xmlelement" => {
            if star || args.len() < 2 {
                return Err(arity_err(name, args.len()));
            }
            let element_name = match *args[0] {
                Expr::Str(value) => value,
                _ => unreachable!(),
            };
            let attribute_count = match *args[1] {
                Expr::Int(value) => value as usize,
                _ => unreachable!(),
            };
            let content_start = 2 + attribute_count * 2;
            if content_start > args.len() {
                return Err(arity_err(name, args.len()));
            }
            let mut out = crate::util::StackStr::<XML_RESULT_MAX>::new();
            out.write_char('<').map_err(|_| arena_full())?;
            xml_name(&mut out, element_name);
            for pair in args[2..content_start].as_chunks::<2>().0 {
                let attr_name = match *pair[0] {
                    Expr::Str(value) => value,
                    _ => unreachable!(),
                };
                let value = eval_full(pair[1], arena, params, row, hooks)?;
                let Some((text, _)) = value_text(value, arena)? else {
                    continue;
                };
                out.write_char(' ').map_err(|_| arena_full())?;
                xml_name(&mut out, attr_name);
                out.write_str("=\"").map_err(|_| arena_full())?;
                push_escaped(&mut out, text, true);
                out.write_char('"').map_err(|_| arena_full())?;
            }
            if content_start == args.len() {
                out.write_str("/>").map_err(|_| arena_full())?;
                return finish(&out, arena);
            }
            out.write_char('>').map_err(|_| arena_full())?;
            for expression in &args[content_start..] {
                if let Some((text, raw)) =
                    value_text(eval_full(expression, arena, params, row, hooks)?, arena)?
                {
                    if raw {
                        out.write_str(text).map_err(|_| arena_full())?;
                    } else {
                        push_escaped(&mut out, text, false);
                    }
                }
            }
            out.write_str("</").map_err(|_| arena_full())?;
            xml_name(&mut out, element_name);
            out.write_char('>').map_err(|_| arena_full())?;
            finish(&out, arena)
        }
        "__xmlforest" => {
            if star || !args.len().is_multiple_of(2) {
                return Err(arity_err(name, args.len()));
            }
            let mut out = crate::util::StackStr::<XML_RESULT_MAX>::new();
            for pair in args.as_chunks::<2>().0 {
                let tag = match *pair[0] {
                    Expr::Str(value) => value,
                    _ => unreachable!(),
                };
                let Some((text, raw)) =
                    value_text(eval_full(pair[1], arena, params, row, hooks)?, arena)?
                else {
                    continue;
                };
                out.write_char('<').map_err(|_| arena_full())?;
                xml_name(&mut out, tag);
                out.write_char('>').map_err(|_| arena_full())?;
                if raw {
                    out.write_str(text).map_err(|_| arena_full())?;
                } else {
                    push_escaped(&mut out, text, false);
                }
                out.write_str("</").map_err(|_| arena_full())?;
                xml_name(&mut out, tag);
                out.write_char('>').map_err(|_| arena_full())?;
            }
            finish(&out, arena)
        }
        "xmlconcat" | "xmlconcat2" => {
            if star || args.is_empty() {
                return Err(arity_err(name, args.len()));
            }
            let mut out = crate::util::StackStr::<XML_RESULT_MAX>::new();
            for expression in args {
                match eval_full(expression, arena, params, row, hooks)? {
                    Datum::Null => {}
                    Datum::Xml(text) => {
                        let text = xml::without_declaration(text);
                        out.write_str(text).map_err(|_| arena_full())?;
                    }
                    other => return Err(type_mismatch("XMLCONCAT arguments must be xml", &other)),
                }
            }
            finish(&out, arena)
        }
        "xmlcomment" => {
            arity(1)?;
            let value = eval_full(args[0], arena, params, row, hooks)?;
            if value.is_null() {
                return Ok(Datum::Null);
            }
            let text = crate::sql::eval::cast::cast_to_text(value, arena)?;
            if text.contains("--") || text.ends_with('-') {
                return Err(sql_err!(
                    sqlstate::INVALID_XML_CONTENT,
                    "invalid XML comment"
                ));
            }
            let mut out = crate::util::StackStr::<XML_RESULT_MAX>::new();
            write!(out, "<!--{}-->", text).map_err(|_| arena_full())?;
            finish(&out, arena)
        }
        "__xmlpi" => {
            if star || !(1..=2).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let target = match *args[0] {
                Expr::Str(value) => value,
                _ => unreachable!(),
            };
            if target.eq_ignore_ascii_case("xml") {
                return Err(sql_err!(
                    sqlstate::INVALID_XML_CONTENT,
                    "invalid XML processing instruction"
                ));
            }
            let content = if args.len() == 2 {
                let value = eval_full(args[1], arena, params, row, hooks)?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                Some(crate::sql::eval::cast::cast_to_text(value, arena)?.trim_start())
            } else {
                None
            };
            if content.is_some_and(|value| value.contains("?>")) {
                return Err(sql_err!(
                    sqlstate::INVALID_XML_CONTENT,
                    "invalid XML processing instruction"
                ));
            }
            let mut out = crate::util::StackStr::<XML_RESULT_MAX>::new();
            out.write_str("<?").map_err(|_| arena_full())?;
            xml_name(&mut out, target);
            if let Some(content) = content {
                out.write_char(' ').map_err(|_| arena_full())?;
                out.write_str(content).map_err(|_| arena_full())?;
            }
            out.write_str("?>").map_err(|_| arena_full())?;
            finish(&out, arena)
        }
        "__xmlroot" => {
            arity(3)?;
            let value = eval_full(args[0], arena, params, row, hooks)?;
            if value.is_null() {
                return Ok(Datum::Null);
            }
            let text = match value {
                Datum::Xml(text) => text,
                other => return Err(type_mismatch("XMLROOT input must be xml", &other)),
            };
            let version_value = eval_full(args[1], arena, params, row, hooks)?;
            let version = if version_value.is_null() {
                None
            } else {
                Some(crate::sql::eval::cast::cast_to_text(version_value, arena)?)
            };
            let standalone = match *args[2] {
                Expr::Int(value) => value,
                _ => unreachable!(),
            };
            let mut out = crate::util::StackStr::<XML_RESULT_MAX>::new();
            if version.is_some() || standalone >= 0 {
                out.write_str("<?xml version=\"")
                    .map_err(|_| arena_full())?;
                out.write_str(version.unwrap_or("1.0"))
                    .map_err(|_| arena_full())?;
                out.write_char('"').map_err(|_| arena_full())?;
                if standalone >= 0 {
                    out.write_str(if standalone == 1 {
                        " standalone=\"yes\""
                    } else {
                        " standalone=\"no\""
                    })
                    .map_err(|_| arena_full())?;
                }
                out.write_str("?>").map_err(|_| arena_full())?;
            }
            out.write_str(xml::without_declaration(text))
                .map_err(|_| arena_full())?;
            finish(&out, arena)
        }
        "xml_is_well_formed" | "xml_is_well_formed_content" | "xml_is_well_formed_document" => {
            arity(1)?;
            let value = eval_full(args[0], arena, params, row, hooks)?;
            if value.is_null() {
                return Ok(Datum::Null);
            }
            let text = crate::sql::eval::cast::cast_to_text(value, arena)?;
            Ok(Datum::Bool(xml::is_well_formed(
                text,
                if name.ends_with("document") {
                    Mode::Document
                } else {
                    Mode::Content
                },
            )))
        }
        "__xmlexists" | "xpath_exists" => {
            if name == "__xmlexists" {
                arity(2)?;
            } else if star || !(2..=3).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let path = eval_full(args[0], arena, params, row, hooks)?;
            let document = eval_full(args[1], arena, params, row, hooks)?;
            if path.is_null() || document.is_null() {
                return Ok(Datum::Null);
            }
            let path = crate::sql::eval::cast::cast_to_text(path, arena)?;
            let document = match document {
                Datum::Xml(text) => text,
                other => return Err(type_mismatch("XPath document must be xml", &other)),
            };
            let mut namespace_pairs = [("", ""); 128];
            let namespace_count = if args.len() == 3 {
                match xpath_namespace_pairs(
                    eval_full(args[2], arena, params, row, hooks)?,
                    &mut namespace_pairs,
                )? {
                    Some(count) => count,
                    None => return Ok(Datum::Null),
                }
            } else {
                0
            };
            let path = xml::rewrite_namespaces(
                path,
                document,
                &namespace_pairs[..namespace_count],
                arena,
            )?;
            Ok(Datum::Bool(xml::xpath_exists(path, document, arena)?))
        }
        "xpath" => {
            if star || !(2..=3).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let path = eval_full(args[0], arena, params, row, hooks)?;
            let document = eval_full(args[1], arena, params, row, hooks)?;
            if path.is_null() || document.is_null() {
                return Ok(Datum::Null);
            }
            let path = crate::sql::eval::cast::cast_to_text(path, arena)?;
            let document = match document {
                Datum::Xml(text) => text,
                other => return Err(type_mismatch("XPath document must be xml", &other)),
            };
            let mut namespace_pairs = [("", ""); 128];
            let namespace_count = if args.len() == 3 {
                match xpath_namespace_pairs(
                    eval_full(args[2], arena, params, row, hooks)?,
                    &mut namespace_pairs,
                )? {
                    Some(count) => count,
                    None => return Ok(Datum::Null),
                }
            } else {
                0
            };
            let path = xml::rewrite_namespaces(
                path,
                document,
                &namespace_pairs[..namespace_count],
                arena,
            )?;
            let values = xml::xpath(path, document, arena)?;
            Ok(Datum::Array {
                element: crate::sql::types::ArrElem::Xml,
                raw: crate::sql::array::build(values, arena)?,
            })
        }
        _ => unreachable!(),
    })())
}
