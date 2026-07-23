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
    arena_full, bytea_arg, eval_full, int_arg, sqlstate, text_arg, type_mismatch, ColumnLookup,
    EvalHooks, SqlError,
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
                        ))
                    }
                    Datum::Int4(v) => stack_format!(16, "{:x}", v as u32),
                    Datum::Int8(v) => stack_format!(16, "{:x}", v as u64),
                    other => return Err(type_mismatch(name, &other)),
                };
                Ok(Datum::Text(arena.alloc_str(s.as_str()).map_err(|_| arena_full())?))
            }
            "md5" => {
                arity(1)?;
                let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let d = crate::sql::md5::digest(s.as_bytes());
                let mut hexbuf = [0u8; 32];
                crate::sql::md5::hex(&d, &mut hexbuf);
                let out = arena.alloc_slice_with(32, |i| hexbuf[i]).map_err(|_| arena_full())?;
                Ok(Datum::Text(unsafe { core::str::from_utf8_unchecked(out) }))
            }
            // Cryptographic hashes of a bytea, each returning bytea.
            "sha224" | "sha256" | "sha384" | "sha512" => {
                arity(1)?;
                let Some(bytes) = bytea_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let digest: &[u8] = match name {
                    "sha224" => {
                        arena.alloc_slice_copy(&crate::s3::sha256::sha224(bytes)).map_err(|_| arena_full())?
                    }
                    "sha256" => {
                        arena.alloc_slice_copy(&crate::s3::sha256::sha256(bytes)).map_err(|_| arena_full())?
                    }
                    "sha384" => {
                        arena.alloc_slice_copy(&crate::sql::sha512::sha384(bytes)).map_err(|_| arena_full())?
                    }
                    _ => arena.alloc_slice_copy(&crate::sql::sha512::sha512(bytes)).map_err(|_| arena_full())?,
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
                        _ => return Err(sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "unrecognized encoding: \"{}\"", format)),
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
                        _ => return Err(sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "unrecognized encoding: \"{}\"", format)),
                    };
                    Ok(Datum::Bytea(bytes))
                }
            }
            // `convert_to(text, enc)` → bytea; `convert_from(bytea, enc)` → text.
            // Only UTF8/UTF-8 (an identity mapping over our text storage) is
            // supported; other encodings error loudly.
            "convert_to" | "convert_from" => {
                arity(2)?;
                let Some(encoding) = text_arg(name, args, 1, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                if !encoding.eq_ignore_ascii_case("UTF8") && !encoding.eq_ignore_ascii_case("UTF-8") {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "encoding \"{}\" is not supported (only UTF8)",
                        encoding
                    ));
                }
                if name == "convert_to" {
                    let Some(s) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    Ok(Datum::Bytea(s.as_bytes()))
                } else {
                    let Some(bytes) = bytea_arg(name, args, 0, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    let s = core::str::from_utf8(bytes)
                        .map_err(|_| sql_err!(sqlstate::CHARACTER_NOT_IN_REPERTOIRE, "invalid byte sequence for encoding UTF8"))?;
                    Ok(Datum::Text(s))
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
                    return Err(sql_err!(sqlstate::ARRAY_SUBSCRIPT_ERROR, "index {} out of valid range, 0..{}", index, bytes.len()));
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
                    return Err(sql_err!(sqlstate::ARRAY_SUBSCRIPT_ERROR, "index {} out of valid range, 0..{}", bit, bytes.len() * 8 - 1));
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
                    Datum::Bytea(b) => {
                        Ok(Datum::Int8(b.iter().map(|byte| byte.count_ones() as i64).sum()))
                    }
                    Datum::Bit { bits, .. } => {
                        Ok(Datum::Int8(bits.bytes().filter(|c| *c == b'1').count() as i64))
                    }
                    other => Err(type_mismatch("bit_count requires bytea or bit", &other)),
                }
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
