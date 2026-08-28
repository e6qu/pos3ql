//! Binary-string, encoding, and hashing built-ins.
//!
//! Covers `bytea` bit/byte access (`get_byte`/`set_byte`/`get_bit`/`set_bit`/
//! `bit_count`), the `encode`/`decode`/`convert_to`/`convert_from` codecs, the
//! cryptographic digests (`md5`, `sha224`..`sha512`), and integer `to_hex`.
//! These share the `bytea_arg`/`text_arg`/`int_arg` argument helpers and the
//! `md5`/`sha512`/`encoding` support modules.

use crate::sql::ast::Expr;
use crate::sql::types::Datum;
use crate::{sql_err, stack_format};

use super::super::{
    ColumnLookup, EvalHooks, SqlError, arena_full, bytea_arg, eval_full, int_arg, sqlstate,
    text_arg, type_mismatch,
};

/// Handles the binary-string/encoding/hashing family. Returns `None` if `name`
/// is not one of these functions, leaving the router to keep matching.
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
        "to_hex"
            | "md5"
            | "sha224"
            | "sha256"
            | "sha384"
            | "sha512"
            | "encode"
            | "decode"
            | "convert_to"
            | "convert_from"
            | "convert"
            | "get_byte"
            | "set_byte"
            | "get_bit"
            | "set_bit"
            | "bit_count"
    ) {
        return None;
    }
    let arity = |n: usize| -> Result<(), SqlError> {
        if args.len() != n || star {
            Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {}(...) with {} arguments does not exist",
                name,
                if star { 1 } else { args.len() }
            ))
        } else {
            Ok(())
        }
    };
    Some((|| -> Result<Datum<'a>, SqlError> {
        match name {
            "to_hex" => {
                arity(1)?;
                let s = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => return Ok(Datum::Null),
                    // to_hex has int4 and int8 forms only; int2 is ambiguous.
                    Datum::Int2(_) => {
                        return Err(sql_err!(
                            sqlstate::AMBIGUOUS_FUNCTION,
                            "function to_hex(smallint) is not unique"
                        ));
                    }
                    Datum::Int4(v) => stack_format!(16, "{:x}", v as u32),
                    Datum::Int8(v) => stack_format!(16, "{:x}", v as u64),
                    other => return Err(type_mismatch(name, &other)),
                };
                Ok(Datum::Text(
                    arena.alloc_str(s.as_str()).map_err(|_| arena_full())?,
                ))
            }
            "md5" => {
                arity(1)?;
                let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let d = crate::sql::md5::digest(s.as_bytes());
                let mut hexbuf = [0u8; 32];
                crate::sql::md5::hex(&d, &mut hexbuf);
                let out = arena
                    .alloc_slice_with(32, |i| hexbuf[i])
                    .map_err(|_| arena_full())?;
                Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(out) }))
            }
            // Cryptographic hashes of a bytea, each returning bytea.
            "sha224" | "sha256" | "sha384" | "sha512" => {
                arity(1)?;
                let Some(bytes) = bytea_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let digest: &[u8] = match name {
                    "sha224" => arena
                        .alloc_slice_copy(&crate::crypto::sha256::sha224(bytes))
                        .map_err(|_| arena_full())?,
                    "sha256" => arena
                        .alloc_slice_copy(&crate::crypto::sha256::sha256(bytes))
                        .map_err(|_| arena_full())?,
                    "sha384" => arena
                        .alloc_slice_copy(&crate::sql::sha512::sha384(bytes))
                        .map_err(|_| arena_full())?,
                    _ => arena
                        .alloc_slice_copy(&crate::sql::sha512::sha512(bytes))
                        .map_err(|_| arena_full())?,
                };
                Ok(Datum::Bytea(digest))
            }
            // `encode(bytea, format)` → text; `decode(text, format)` → bytea.
            "encode" | "decode" => {
                arity(2)?;
                let Some(format) = text_arg(name, args, 1, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                if name == "encode" {
                    let Some(bytes) = bytea_arg(name, args, 0, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    let text = match format {
                        "base64" => crate::sql::encoding::base64_encode(bytes, arena)?,
                        "hex" => crate::sql::encoding::hex_encode(bytes, arena)?,
                        "escape" => crate::sql::encoding::escape_encode(bytes, arena)?,
                        _ => {
                            return Err(sql_err!(
                                sqlstate::INVALID_PARAMETER_VALUE,
                                "unrecognized encoding: \"{}\"",
                                format
                            ));
                        }
                    };
                    Ok(Datum::Text(text))
                } else {
                    let Some(text) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    let bytes = match format {
                        "base64" => crate::sql::encoding::base64_decode(text, arena)?,
                        "hex" => crate::sql::encoding::hex_decode(text, arena)?,
                        "escape" => crate::sql::encoding::escape_decode(text, arena)?,
                        _ => {
                            return Err(sql_err!(
                                sqlstate::INVALID_PARAMETER_VALUE,
                                "unrecognized encoding: \"{}\"",
                                format
                            ));
                        }
                    };
                    Ok(Datum::Bytea(bytes))
                }
            }
            "convert_to" | "convert_from" | "convert" => {
                arity(if name == "convert" { 3 } else { 2 })?;
                let destination_index = if name == "convert" { 2 } else { 1 };
                let Some(destination_name) =
                    text_arg(name, args, destination_index, arena, params, row, hooks)?
                else {
                    return Ok(Datum::Null);
                };
                let destination =
                    crate::storage::PgEncoding::parse(destination_name).ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "encoding \"{}\" does not exist",
                            destination_name
                        )
                    })?;
                let catalog = hooks.catalog.ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "encoding conversion catalog is unavailable"
                    )
                })?;
                if name == "convert_to" {
                    let Some(text) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    return Ok(Datum::Bytea(catalog.convert_encoding(
                        crate::storage::PgEncoding::UTF8,
                        destination,
                        text.as_bytes(),
                        arena,
                    )?));
                }
                let Some(bytes) = bytea_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let source = if name == "convert" {
                    let Some(source_name) = text_arg(name, args, 1, arena, params, row, hooks)?
                    else {
                        return Ok(Datum::Null);
                    };
                    crate::storage::PgEncoding::parse(source_name).ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "encoding \"{}\" does not exist",
                            source_name
                        )
                    })?
                } else {
                    destination
                };
                let target = if name == "convert" {
                    destination
                } else {
                    crate::storage::PgEncoding::UTF8
                };
                let converted = catalog.convert_encoding(source, target, bytes, arena)?;
                if name == "convert" {
                    Ok(Datum::Bytea(converted))
                } else {
                    Ok(Datum::Text(core::str::from_utf8(converted).map_err(
                        |_| {
                            sql_err!(
                                sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
                                "invalid byte sequence for encoding UTF8"
                            )
                        },
                    )?))
                }
            }
            // `get_byte(bytea, n)` / `set_byte(bytea, n, v)`: 0-based byte access.
            "get_byte" | "set_byte" => {
                arity(if name == "get_byte" { 2 } else { 3 })?;
                let Some(bytes) = bytea_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let Some(index) = int_arg(name, args, 1, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                if index < 0 || index as usize >= bytes.len() {
                    return Err(sql_err!(
                        sqlstate::ARRAY_SUBSCRIPT_ERROR,
                        "index {} out of valid range, 0..{}",
                        index,
                        bytes.len()
                    ));
                }
                if name == "get_byte" {
                    return Ok(Datum::Int4(bytes[index as usize] as i32));
                }
                let Some(value) = int_arg(name, args, 2, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let out = arena.alloc_slice_copy(bytes).map_err(|_| arena_full())?;
                out[index as usize] = value as u8;
                Ok(Datum::Bytea(out))
            }
            // `get_bit(bytea, n)` / `set_bit(bytea, n, v)`: 0-based bit access, with
            // PostgreSQL's per-byte bit numbering (bit 0 is the LSB of byte 0).
            "get_bit" | "set_bit" => {
                arity(if name == "get_bit" { 2 } else { 3 })?;
                let Some(bytes) = bytea_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let Some(bit) = int_arg(name, args, 1, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                if bit < 0 || (bit as usize) >= bytes.len() * 8 {
                    return Err(sql_err!(
                        sqlstate::ARRAY_SUBSCRIPT_ERROR,
                        "index {} out of valid range, 0..{}",
                        bit,
                        bytes.len() * 8 - 1
                    ));
                }
                let byte_index = bit as usize / 8;
                let bit_index = bit as usize % 8;
                if name == "get_bit" {
                    return Ok(Datum::Int4(((bytes[byte_index] >> bit_index) & 1) as i32));
                }
                let Some(value) = int_arg(name, args, 2, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let out = arena.alloc_slice_copy(bytes).map_err(|_| arena_full())?;
                if value & 1 == 1 {
                    out[byte_index] |= 1 << bit_index;
                } else {
                    out[byte_index] &= !(1 << bit_index);
                }
                Ok(Datum::Bytea(out))
            }
            // `bit_count`: the number of set bits in a bytea or bit string.
            "bit_count" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Bytea(b) => Ok(Datum::Int8(
                        b.iter().map(|byte| byte.count_ones() as i64).sum(),
                    )),
                    Datum::Bit { bits, .. } => Ok(Datum::Int8(
                        bits.bytes().filter(|c| *c == b'1').count() as i64,
                    )),
                    other => Err(type_mismatch("bit_count requires bytea or bit", &other)),
                }
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}

pub(crate) fn convert_encoding<'a>(
    source: crate::storage::PgEncoding,
    destination: crate::storage::PgEncoding,
    procedure: Option<i32>,
    input: &[u8],
    arena: &'a crate::mem::arena::Arena,
) -> Result<&'a [u8], SqlError> {
    if source == destination {
        if source == crate::storage::PgEncoding::UTF8 {
            core::str::from_utf8(input).map_err(|_| {
                sql_err!(
                    sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
                    "invalid byte sequence for encoding UTF8"
                )
            })?;
        }
        return arena
            .alloc_slice_copy(input)
            .map(|output| &*output)
            .map_err(|_| arena_full());
    }
    match procedure {
        Some(4374)
            if source == crate::storage::PgEncoding::LATIN1
                && destination == crate::storage::PgEncoding::UTF8 =>
        {
            let output = arena
                .alloc_slice_with(input.len().saturating_mul(2), |_| 0u8)
                .map_err(|_| arena_full())?;
            let mut written = 0;
            for &byte in input {
                if byte < 0x80 {
                    output[written] = byte;
                    written += 1;
                } else {
                    output[written] = 0xc0 | (byte >> 6);
                    output[written + 1] = 0x80 | (byte & 0x3f);
                    written += 2;
                }
            }
            Ok(&output[..written])
        }
        Some(4375)
            if source == crate::storage::PgEncoding::UTF8
                && destination == crate::storage::PgEncoding::LATIN1 =>
        {
            let text = core::str::from_utf8(input).map_err(|_| {
                sql_err!(
                    sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
                    "invalid byte sequence for encoding UTF8"
                )
            })?;
            let output = arena
                .alloc_slice_with(text.chars().count(), |_| 0u8)
                .map_err(|_| arena_full())?;
            for (index, character) in text.chars().enumerate() {
                output[index] = u8::try_from(character as u32).map_err(|_| {
                    sql_err!(
                        sqlstate::UNTRANSLATABLE_CHARACTER,
                        "character cannot be converted from encoding UTF8 to LATIN1"
                    )
                })?;
            }
            Ok(output)
        }
        _ => Err(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "default conversion from {} to {} does not exist",
            source.name(),
            destination.name()
        )),
    }
}
