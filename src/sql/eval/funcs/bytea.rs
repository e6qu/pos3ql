//! Binary-string, encoding, and hashing built-ins.
//!
//! Covers binary- and bit-string slicing, overlay, search, trimming, reversal,
//! bit/byte access, encodings, cryptographic digests, checksums, and integer
//! hexadecimal rendering.
//! These share the `bytea_arg`/`text_arg`/`int_arg` argument helpers and the
//! `md5`/`sha512`/`encoding` support modules.

use crate::sql::ast::{BinaryOp, Expr};
use crate::sql::types::{ColType, Datum};
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
            | "to_bin"
            | "to_oct"
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
            | "crc32"
            | "crc32c"
            | "substr"
            | "substring"
            | "overlay"
            | "position"
            | "btrim"
            | "ltrim"
            | "rtrim"
            | "reverse"
            | "byteain"
            | "byteaout"
            | "byteasend"
            | "float8send"
            | "byteaeq"
            | "byteane"
            | "bytealt"
            | "byteale"
            | "byteagt"
            | "byteage"
            | "byteacmp"
            | "byteacat"
            | "bytealike"
            | "byteanlike"
            | "bytea_larger"
            | "bytea_smaller"
            | "hashbytea"
            | "hashbyteaextended"
            | "bytea"
            | "int2"
            | "int4"
            | "int8"
            | "bit_in"
            | "varbit_in"
            | "bit_out"
            | "varbit_out"
            | "bit_send"
            | "varbit_send"
            | "biteq"
            | "bitne"
            | "bitlt"
            | "bitle"
            | "bitgt"
            | "bitge"
            | "bitcmp"
            | "varbiteq"
            | "varbitne"
            | "varbitlt"
            | "varbitle"
            | "varbitgt"
            | "varbitge"
            | "varbitcmp"
            | "bitand"
            | "bitor"
            | "bitxor"
            | "bitnot"
            | "bitshiftleft"
            | "bitshiftright"
            | "bitcat"
    ) {
        return None;
    }
    // These names are overloaded with text functions. Route them here only
    // when the resolved input family is binary; the text dispatcher remains
    // responsible for ordinary strings and unknown literals.
    if matches!(
        name,
        "substr" | "substring" | "overlay" | "position" | "btrim" | "ltrim" | "rtrim" | "reverse"
    ) && !matches!(
        args.first()
            .and_then(|argument| super::super::static_type_pub(argument, row)),
        Some(ColType::Bytea | ColType::Bit { .. })
    ) {
        return None;
    }
    if matches!(name, "int2" | "int4" | "int8")
        && !matches!(
            args.first()
                .and_then(|argument| super::super::static_type_pub(argument, row)),
            Some(ColType::Bytea)
        )
    {
        return None;
    }
    if name == "bytea"
        && !matches!(
            args.first()
                .and_then(|argument| super::super::static_type_pub(argument, row)),
            Some(ColType::Int2 | ColType::Int4 | ColType::Int8)
        )
    {
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
            "byteain" => {
                arity(1)?;
                let Some(text) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                Ok(Datum::Bytea(crate::sql::eval::parse_bytea(text, arena)?))
            }
            "byteaout" => {
                arity(1)?;
                let Some(bytes) = bytea_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                super::super::cast_to_text(Datum::Bytea(bytes), arena).map(Datum::Text)
            }
            "byteasend" => {
                arity(1)?;
                bytea_arg(name, args, 0, arena, params, row, hooks)
                    .map(|value| value.map_or(Datum::Null, Datum::Bytea))
            }
            "float8send" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Float8(value) => arena
                        .alloc_slice_copy(&value.to_bits().to_be_bytes())
                        .map(|bytes| Datum::Bytea(&*bytes))
                        .map_err(|_| arena_full()),
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "byteaeq" | "byteane" | "bytealt" | "byteale" | "byteagt" | "byteage" | "byteacmp"
            | "byteacat" | "bytealike" | "byteanlike" | "bytea_larger" | "bytea_smaller" => {
                arity(2)?;
                let Some(left) = bytea_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let Some(right) = bytea_arg(name, args, 1, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                match name {
                    "byteacat" => {
                        let out = arena
                            .alloc_slice_with(left.len() + right.len(), |index| {
                                if index < left.len() {
                                    left[index]
                                } else {
                                    right[index - left.len()]
                                }
                            })
                            .map_err(|_| arena_full())?;
                        Ok(Datum::Bytea(out))
                    }
                    "bytealike" | "byteanlike" => {
                        let matched =
                            super::super::pattern::like_match_bytes(left, right, Some(b'\\'))?;
                        Ok(Datum::Bool(matched == (name == "bytealike")))
                    }
                    "bytea_larger" => Ok(Datum::Bytea(if left >= right { left } else { right })),
                    "bytea_smaller" => Ok(Datum::Bytea(if left <= right { left } else { right })),
                    _ => {
                        let comparison = binary_byte_comparison(left, right);
                        Ok(match name {
                            "byteaeq" => Datum::Bool(comparison == 0),
                            "byteane" => Datum::Bool(comparison != 0),
                            "bytealt" => Datum::Bool(comparison < 0),
                            "byteale" => Datum::Bool(comparison <= 0),
                            "byteagt" => Datum::Bool(comparison > 0),
                            "byteage" => Datum::Bool(comparison >= 0),
                            _ => Datum::Int4(comparison),
                        })
                    }
                }
            }
            "hashbytea" | "hashbyteaextended" => {
                arity(if name == "hashbytea" { 1 } else { 2 })?;
                let Some(bytes) = bytea_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                if name == "hashbytea" {
                    Ok(Datum::Int4(crate::sql::identity::hash_bytea(bytes)))
                } else {
                    let Some(seed) = int_arg(name, args, 1, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    Ok(Datum::Int8(crate::sql::identity::hash_bytea_extended(
                        bytes, seed,
                    )))
                }
            }
            "bytea" | "int2" | "int4" | "int8" => {
                arity(1)?;
                let value = eval_full(args[0], arena, params, row, hooks)?;
                let target = match name {
                    "bytea" => ColType::Bytea,
                    "int2" => ColType::Int2,
                    "int4" => ColType::Int4,
                    _ => ColType::Int8,
                };
                super::super::cast_to(value, target, arena)
            }
            "bit_in" | "varbit_in" => {
                arity(3)?;
                let Some(text) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                if eval_full(args[1], arena, params, row, hooks)?.is_null() {
                    return Ok(Datum::Null);
                }
                let Some(type_modifier) = int_arg(name, args, 2, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let bits = super::super::cast::parse_bits_text(text, arena)?;
                if type_modifier >= 0
                    && (bits.len() > type_modifier as usize
                        || name == "bit_in" && bits.len() != type_modifier as usize)
                {
                    return Err(sql_err!(
                        sqlstate::STRING_DATA_LENGTH_MISMATCH,
                        "{}",
                        if name == "bit_in" {
                            "bit string length does not match type bit"
                        } else {
                            "bit string too long for type bit varying"
                        }
                    ));
                }
                Ok(Datum::Bit {
                    bits,
                    varying: name == "varbit_in",
                })
            }
            "bit_out" | "varbit_out" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Bit { bits, .. } => Ok(Datum::Text(bits)),
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "bit_send" | "varbit_send" => {
                arity(1)?;
                let value = eval_full(args[0], arena, params, row, hooks)?;
                let Datum::Bit { bits, .. } = value else {
                    return if value.is_null() {
                        Ok(Datum::Null)
                    } else {
                        Err(type_mismatch(name, &value))
                    };
                };
                let out = arena
                    .alloc_slice_with(4 + bits.len().div_ceil(8), |_| 0_u8)
                    .map_err(|_| arena_full())?;
                out[..4].copy_from_slice(&(bits.len() as i32).to_be_bytes());
                for (index, bit) in bits.bytes().enumerate() {
                    if bit == b'1' {
                        out[4 + index / 8] |= 1 << (7 - index % 8);
                    }
                }
                Ok(Datum::Bytea(out))
            }
            "biteq" | "bitne" | "bitlt" | "bitle" | "bitgt" | "bitge" | "bitcmp" | "varbiteq"
            | "varbitne" | "varbitlt" | "varbitle" | "varbitgt" | "varbitge" | "varbitcmp"
            | "bitand" | "bitor" | "bitxor" | "bitcat" | "bitshiftleft" | "bitshiftright" => {
                arity(2)?;
                let left = eval_full(args[0], arena, params, row, hooks)?;
                let right = eval_full(args[1], arena, params, row, hooks)?;
                if left.is_null() || right.is_null() {
                    return Ok(Datum::Null);
                }
                match name {
                    "bitand" => fixed_bit_result(super::super::operators::bit_bitwise(
                        BinaryOp::BitAnd,
                        left,
                        right,
                        arena,
                    )),
                    "bitor" => fixed_bit_result(super::super::operators::bit_bitwise(
                        BinaryOp::BitOr,
                        left,
                        right,
                        arena,
                    )),
                    "bitxor" => fixed_bit_result(super::super::operators::bit_bitwise(
                        BinaryOp::BitXor,
                        left,
                        right,
                        arena,
                    )),
                    "bitcat" => super::super::operators::bit_concat(left, right, arena),
                    "bitshiftleft" => fixed_bit_result(super::super::operators::bit_shift(
                        BinaryOp::Shl,
                        left,
                        right,
                        arena,
                    )),
                    "bitshiftright" => fixed_bit_result(super::super::operators::bit_shift(
                        BinaryOp::Shr,
                        left,
                        right,
                        arena,
                    )),
                    _ => {
                        let comparison = packed_bit_comparison(left, right)?;
                        Ok(match name {
                            "biteq" | "varbiteq" => Datum::Bool(comparison == 0),
                            "bitne" | "varbitne" => Datum::Bool(comparison != 0),
                            "bitlt" | "varbitlt" => Datum::Bool(comparison < 0),
                            "bitle" | "varbitle" => Datum::Bool(comparison <= 0),
                            "bitgt" | "varbitgt" => Datum::Bool(comparison > 0),
                            "bitge" | "varbitge" => Datum::Bool(comparison >= 0),
                            _ => Datum::Int4(comparison),
                        })
                    }
                }
            }
            "bitnot" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Bit { bits, .. } => {
                        let out = arena
                            .alloc_slice_with(bits.len(), |index| {
                                if bits.as_bytes()[index] == b'0' {
                                    b'1'
                                } else {
                                    b'0'
                                }
                            })
                            .map_err(|_| arena_full())?;
                        Ok(Datum::Bit {
                            bits: unsafe { core::str::from_utf8_unchecked(out) },
                            varying: false,
                        })
                    }
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "to_hex" | "to_bin" | "to_oct" => {
                arity(1)?;
                let (value, width) = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => return Ok(Datum::Null),
                    // These functions have int4 and int8 forms only; int2 is
                    // ambiguous between the two implicit promotions.
                    Datum::Int2(_) => {
                        return Err(sql_err!(
                            sqlstate::AMBIGUOUS_FUNCTION,
                            "function {}(smallint) is not unique",
                            name
                        ));
                    }
                    Datum::Int4(value) => (u64::from(value as u32), 32),
                    Datum::Int8(value) => (value as u64, 64),
                    other => return Err(type_mismatch(name, &other)),
                };
                let s = match (name, width) {
                    ("to_bin", 32) => stack_format!(64, "{:b}", value as u32),
                    ("to_oct", 32) => stack_format!(64, "{:o}", value as u32),
                    ("to_hex", 32) => stack_format!(64, "{:x}", value as u32),
                    ("to_bin", _) => stack_format!(64, "{:b}", value),
                    ("to_oct", _) => stack_format!(64, "{:o}", value),
                    _ => stack_format!(64, "{:x}", value),
                };
                Ok(Datum::Text(
                    arena.alloc_str(s.as_str()).map_err(|_| arena_full())?,
                ))
            }
            "md5" => {
                arity(1)?;
                let value = eval_full(args[0], arena, params, row, hooks)?;
                let bytes = match value {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Text(s) => s.as_bytes(),
                    Datum::Bpchar(s) => s.trim_end_matches(' ').as_bytes(),
                    Datum::Bytea(bytes) => bytes,
                    other => return Err(type_mismatch(name, &other)),
                };
                let d = crate::sql::md5::digest(bytes);
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
                let input = eval_full(args[0], arena, params, row, hooks)?;
                let Some(bit) = int_arg(name, args, 1, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                if input.is_null() {
                    return Ok(Datum::Null);
                }
                if let Datum::Bit { bits, .. } = input {
                    if bit < 0 || bit as usize >= bits.len() {
                        return Err(sql_err!(
                            sqlstate::ARRAY_SUBSCRIPT_ERROR,
                            "bit index {} out of valid range (0..{})",
                            bit,
                            bits.len() as i64 - 1
                        ));
                    }
                    if name == "get_bit" {
                        return Ok(Datum::Int4(i32::from(
                            bits.as_bytes()[bit as usize] == b'1',
                        )));
                    }
                    let Some(value) = int_arg(name, args, 2, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    if !(0..=1).contains(&value) {
                        return Err(sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "new bit must be 0 or 1"
                        ));
                    }
                    let out = arena
                        .alloc_slice_copy(bits.as_bytes())
                        .map_err(|_| arena_full())?;
                    out[bit as usize] = if value == 0 { b'0' } else { b'1' };
                    return Ok(Datum::Bit {
                        bits: unsafe { core::str::from_utf8_unchecked(out) },
                        varying: false,
                    });
                }
                let bytes = match input {
                    Datum::Bytea(bytes) => bytes,
                    Datum::Text(text) => crate::sql::eval::parse_bytea(text, arena)?,
                    Datum::Bpchar(text) => {
                        crate::sql::eval::parse_bytea(text.trim_end_matches(' '), arena)?
                    }
                    other => return Err(type_mismatch(name, &other)),
                };
                let bit_len = bytes.len().saturating_mul(8);
                if bit < 0 || (bit as usize) >= bit_len {
                    return Err(sql_err!(
                        sqlstate::ARRAY_SUBSCRIPT_ERROR,
                        "index {} out of valid range, 0..{}",
                        bit,
                        bit_len as i64 - 1
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
                if !(0..=1).contains(&value) {
                    return Err(sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "new bit must be 0 or 1"
                    ));
                }
                let out = arena.alloc_slice_copy(bytes).map_err(|_| arena_full())?;
                if value == 1 {
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
            "crc32" | "crc32c" => {
                arity(1)?;
                let Some(bytes) = bytea_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let polynomial = if name == "crc32" {
                    0xedb8_8320
                } else {
                    0x82f6_3b78
                };
                let mut crc = u32::MAX;
                for &byte in bytes {
                    crc ^= u32::from(byte);
                    for _ in 0..8 {
                        crc = (crc >> 1) ^ (polynomial & (0_u32.wrapping_sub(crc & 1)));
                    }
                }
                Ok(Datum::Int8(i64::from(!crc)))
            }
            "substr" | "substring" => {
                if star || !(2..=3).contains(&args.len()) {
                    return Err(super::super::arity_err(name, args.len()));
                }
                let value = eval_full(args[0], arena, params, row, hooks)?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                let Some(start) = int_arg(name, args, 1, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let count = if args.len() == 3 {
                    let Some(count) = int_arg(name, args, 2, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    if count < 0 {
                        return Err(sql_err!(
                            sqlstate::SUBSTRING_ERROR,
                            "negative substring length not allowed"
                        ));
                    }
                    Some(count)
                } else {
                    None
                };
                let lo = start.saturating_sub(1).max(0) as usize;
                let slice = |len: usize| {
                    let lo = lo.min(len);
                    let hi = count.map_or(len, |count| {
                        start.saturating_sub(1).saturating_add(count).max(0) as usize
                    });
                    (lo, hi.min(len).max(lo))
                };
                match value {
                    Datum::Bytea(bytes) => {
                        let (lo, hi) = slice(bytes.len());
                        Ok(Datum::Bytea(&bytes[lo..hi]))
                    }
                    Datum::Bit { bits, .. } => {
                        let (lo, hi) = slice(bits.len());
                        Ok(Datum::Bit {
                            bits: &bits[lo..hi],
                            varying: false,
                        })
                    }
                    other => Err(type_mismatch(name, &other)),
                }
            }
            "overlay" => {
                if star || !(3..=4).contains(&args.len()) {
                    return Err(super::super::arity_err(name, args.len()));
                }
                let source = eval_full(args[0], arena, params, row, hooks)?;
                let mut replacement = eval_full(args[1], arena, params, row, hooks)?;
                if source.is_null() || replacement.is_null() {
                    return Ok(Datum::Null);
                }
                if matches!(args[1], Expr::Str(_)) {
                    replacement = match source {
                        Datum::Bytea(_) => {
                            super::super::cast_to(replacement, ColType::Bytea, arena)?
                        }
                        Datum::Bit { varying, .. } => Datum::Bit {
                            bits: match replacement {
                                Datum::Text(text) => {
                                    super::super::cast::parse_bits_text(text, arena)?
                                }
                                _ => unreachable!("unknown literal evaluates as text"),
                            },
                            varying,
                        },
                        _ => replacement,
                    };
                }
                let Some(start) = int_arg(name, args, 2, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let replacement_len = match replacement {
                    Datum::Bytea(bytes) => bytes.len(),
                    Datum::Bit { bits, .. } => bits.len(),
                    ref other => return Err(type_mismatch(name, other)),
                };
                let count = if args.len() == 4 {
                    let Some(count) = int_arg(name, args, 3, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    if count < 0 {
                        return Err(sql_err!(
                            sqlstate::SUBSTRING_ERROR,
                            "negative substring length not allowed"
                        ));
                    }
                    count as usize
                } else {
                    replacement_len
                };
                let prefix = start.saturating_sub(1).max(0) as usize;
                match (source, replacement) {
                    (Datum::Bytea(source), Datum::Bytea(replacement)) => {
                        let prefix = prefix.min(source.len());
                        let suffix = prefix.saturating_add(count).min(source.len());
                        let out_len = prefix + replacement.len() + source.len() - suffix;
                        let out = arena
                            .alloc_slice_with(out_len, |_| 0)
                            .map_err(|_| arena_full())?;
                        out[..prefix].copy_from_slice(&source[..prefix]);
                        out[prefix..prefix + replacement.len()].copy_from_slice(replacement);
                        out[prefix + replacement.len()..].copy_from_slice(&source[suffix..]);
                        Ok(Datum::Bytea(out))
                    }
                    (
                        Datum::Bit { bits: source, .. },
                        Datum::Bit {
                            bits: replacement, ..
                        },
                    ) => {
                        let prefix = prefix.min(source.len());
                        let suffix = prefix.saturating_add(count).min(source.len());
                        let out_len = prefix + replacement.len() + source.len() - suffix;
                        let out = arena
                            .alloc_slice_with(out_len, |_| b'0')
                            .map_err(|_| arena_full())?;
                        out[..prefix].copy_from_slice(&source.as_bytes()[..prefix]);
                        out[prefix..prefix + replacement.len()]
                            .copy_from_slice(replacement.as_bytes());
                        out[prefix + replacement.len()..]
                            .copy_from_slice(&source.as_bytes()[suffix..]);
                        Ok(Datum::Bit {
                            bits: unsafe { core::str::from_utf8_unchecked(out) },
                            varying: false,
                        })
                    }
                    (left, right) => Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "overlay requires matching binary-string types, got {:?} and {:?}",
                        left,
                        right
                    )),
                }
            }
            "position" => {
                arity(2)?;
                let haystack = eval_full(args[0], arena, params, row, hooks)?;
                let needle = eval_full(args[1], arena, params, row, hooks)?;
                if haystack.is_null() || needle.is_null() {
                    return Ok(Datum::Null);
                }
                let found = match (haystack, needle) {
                    (Datum::Bytea(haystack), Datum::Bytea(needle)) => find_bytes(haystack, needle),
                    (Datum::Bit { bits: haystack, .. }, Datum::Bit { bits: needle, .. }) => {
                        if needle.is_empty() && haystack.is_empty() {
                            None
                        } else {
                            find_bytes(haystack.as_bytes(), needle.as_bytes())
                        }
                    }
                    (left, right) => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "position requires matching binary-string types, got {:?} and {:?}",
                            left,
                            right
                        ));
                    }
                };
                Ok(Datum::Int4(found.map_or(0, |offset| offset as i32 + 1)))
            }
            "btrim" | "ltrim" | "rtrim" => {
                arity(2)?;
                let Some(source) = bytea_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let Some(set) = bytea_arg(name, args, 1, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let mut lo = 0;
                let mut hi = source.len();
                if name != "rtrim" {
                    while lo < hi && set.contains(&source[lo]) {
                        lo += 1;
                    }
                }
                if name != "ltrim" {
                    while hi > lo && set.contains(&source[hi - 1]) {
                        hi -= 1;
                    }
                }
                Ok(Datum::Bytea(&source[lo..hi]))
            }
            "reverse" => {
                arity(1)?;
                let Some(source) = bytea_arg(name, args, 0, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let out = arena
                    .alloc_slice_with(source.len(), |index| source[source.len() - index - 1])
                    .map_err(|_| arena_full())?;
                Ok(Datum::Bytea(out))
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn ordering_sign(ordering: core::cmp::Ordering) -> i32 {
    match ordering {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    }
}

fn fixed_bit_result(result: Result<Datum<'_>, SqlError>) -> Result<Datum<'_>, SqlError> {
    result.map(|value| match value {
        Datum::Bit { bits, .. } => Datum::Bit {
            bits,
            varying: false,
        },
        other => other,
    })
}

fn binary_byte_comparison(left: &[u8], right: &[u8]) -> i32 {
    for (&left, &right) in left.iter().zip(right) {
        if left != right {
            return i32::from(left) - i32::from(right);
        }
    }
    ordering_sign(left.len().cmp(&right.len()))
}

fn packed_bit_comparison(left: Datum<'_>, right: Datum<'_>) -> Result<i32, SqlError> {
    let Datum::Bit { bits: left, .. } = left else {
        return Err(type_mismatch("bit comparison", &left));
    };
    let Datum::Bit { bits: right, .. } = right else {
        return Err(type_mismatch("bit comparison", &right));
    };
    let common = left.len().min(right.len());
    for offset in (0..common).step_by(8) {
        let width = (common - offset).min(8);
        let pack = |bits: &str| {
            let mut byte = 0_u8;
            for index in 0..width {
                if bits.as_bytes()[offset + index] == b'1' {
                    byte |= 1 << (7 - index);
                }
            }
            byte
        };
        let left_byte = pack(left);
        let right_byte = pack(right);
        if left_byte != right_byte {
            return Ok(i32::from(left_byte) - i32::from(right_byte));
        }
    }
    Ok(ordering_sign(left.len().cmp(&right.len())))
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
