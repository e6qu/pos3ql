//! Converting a value to a target type.
//!
//! One `cast_to` per target, plus the parsers and formatters the harder targets
//! need: bit strings, uuid, bytea's two input forms, and the text rendering
//! every type falls back to. This is the SQL cast, so it is stricter than the
//! coercion an operator applies to an unknown literal — a failure here is the
//! user's error, not a reason to try something else.

use crate::mem::arena::Arena;
use crate::sql::numeric::Numeric;
use crate::sql::types::{ColType, Datum};
use crate::sql_err;

use super::{
    arena_full, bad_text, cast_unsupported, load_array, overflow, parse_bool, session_zone_at,
    sqlstate, SqlError,
};

pub fn cast<'a>(v: Datum<'a>, type_name: &str, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let Some(target) = ColType::from_sql_name(type_name) else {
        return Err(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "type \"{}\" does not exist",
            type_name
        ));
    };
    cast_to(v, target, arena)
}

pub fn cast_to<'a>(
    v: Datum<'a>,
    target: ColType,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if v.is_null() {
        return Ok(Datum::Null);
    }
    let out = match target {
        ColType::Bool => match v {
            Datum::Bool(_) => v,
            Datum::Int4(x) => Datum::Bool(x != 0),
            Datum::Text(s) => Datum::Bool(parse_bool(s)?),
            _ => return Err(cast_unsupported(&v, "boolean")),
        },
        ColType::Int4 => {
            if let Datum::Bit { bits, .. } = v {
                // bit -> integer: the bits are the low bits of the result
                // (two's complement), so a full 32-bit string round-trips.
                Datum::Int4(bits_to_uint(bits, 32, "integer")? as u32 as i32)
            } else {
                let x = to_i64_for_cast(&v, "integer")?;
                Datum::Int4(i32::try_from(x).map_err(|_| overflow("integer"))?)
            }
        }
        ColType::Int8 => {
            if let Datum::Bit { bits, .. } = v {
                Datum::Int8(bits_to_uint(bits, 64, "bigint")? as i64)
            } else {
                Datum::Int8(to_i64_for_cast(&v, "bigint")?)
            }
        }
        // real/float4 collapse to float8 storage: full precision is retained so
        // text output stays shortest round-trip (true 4-byte float4 rounding
        // would need a dedicated Datum to render correctly).
        ColType::Float8 | ColType::Float4 => match v {
            Datum::Int4(x) => Datum::Float8(f64::from(x)),
            Datum::Int8(x) => Datum::Float8(x as f64),
            Datum::Float8(_) => v,
            Datum::Numeric(n) => Datum::Float8(n.to_f64()),
            Datum::Text(s) => Datum::Float8(s.trim().parse().map_err(|_| bad_text(s, "double precision"))?),
            _ => return Err(cast_unsupported(&v, "double precision")),
        },
        ColType::Text | ColType::Varchar | ColType::Bpchar => Datum::Text(cast_to_text(v, arena)?),
        ColType::Int2 => {
            let x = to_i64_for_cast(&v, "smallint")?;
            if !(-32768..=32767).contains(&x) {
                return Err(overflow("smallint"));
            }
            Datum::Int4(x as i32)
        }
        ColType::Date => match v {
            Datum::Date(_) => v,
            Datum::Timestamp(t) | Datum::Timestamptz(t) => {
                Datum::Date(t.div_euclid(86_400_000_000) as i32)
            }
            Datum::Text(s) => Datum::Date(crate::sql::datetime::parse_date(s)?),
            _ => return Err(cast_unsupported(&v, "date")),
        },
        ColType::Timestamp => match v {
            Datum::Timestamp(_) => v,
            Datum::Timestamptz(t) => Datum::Timestamp(t),
            Datum::Date(d) => Datum::Timestamp(d as i64 * 86_400_000_000),
            Datum::Text(s) => Datum::Timestamp(crate::sql::datetime::parse_timestamp(s, false)?),
            _ => return Err(cast_unsupported(&v, "timestamp")),
        },
        ColType::Timestamptz => match v {
            Datum::Timestamptz(_) => v,
            Datum::Timestamp(t) => Datum::Timestamptz(t),
            Datum::Date(d) => Datum::Timestamptz(d as i64 * 86_400_000_000),
            Datum::Text(s) => Datum::Timestamptz(crate::sql::datetime::parse_timestamp(s, true)?),
            _ => return Err(cast_unsupported(&v, "timestamp with time zone")),
        },
        ColType::Time => match v {
            Datum::Time(_) => v,
            Datum::Timetz(t, _) => Datum::Time(t),
            // The time-of-day portion of a timestamp (microseconds past midnight).
            Datum::Timestamp(t) | Datum::Timestamptz(t) => {
                Datum::Time(t.rem_euclid(86_400_000_000))
            }
            Datum::Text(s) => Datum::Time(crate::sql::datetime::parse_time(s)?),
            _ => return Err(cast_unsupported(&v, "time without time zone")),
        },
        ColType::Timetz => match v {
            Datum::Timetz(..) => v,
            // A value with no zone of its own takes the session's, as
            // PostgreSQL does — for a timestamptz that means converting the
            // instant into that zone first.
            Datum::Time(t) => Datum::Timetz(t, session_zone_at(crate::sql::datetime::now_micros())),
            Datum::Timestamptz(t) => {
                let zone = session_zone_at(t);
                let local = t + zone as i64 * 1_000_000;
                Datum::Timetz(local.rem_euclid(86_400_000_000), zone)
            }
            Datum::Timestamp(t) => {
                Datum::Timetz(t.rem_euclid(86_400_000_000), session_zone_at(t))
            }
            Datum::Text(s) => {
                let (t, zone) = crate::sql::datetime::parse_timetz(s)?;
                Datum::Timetz(t, zone.unwrap_or_else(|| session_zone_at(crate::sql::datetime::now_micros())))
            }
            _ => return Err(cast_unsupported(&v, "time with time zone")),
        },
        ColType::Interval => match v {
            Datum::Interval(_) => v,
            Datum::Text(s) => Datum::Interval(crate::sql::datetime::parse_interval(s)?),
            _ => return Err(cast_unsupported(&v, "interval")),
        },
        ColType::Json => match v {
            Datum::Json { text, .. } => {
                crate::sql::json::validate(text, arena)?;
                Datum::Json { text, jsonb: false }
            }
            Datum::Text(s) => {
                crate::sql::json::validate(s, arena)?;
                Datum::Json { text: s, jsonb: false }
            }
            _ => return Err(cast_unsupported(&v, "json")),
        },
        ColType::Jsonb => match v {
            Datum::Json { jsonb: true, .. } => v,
            Datum::Json { text, jsonb: false } | Datum::Text(text) => {
                let tree = crate::sql::json::parse(text, arena)?;
                let mut buffer = crate::util::StackStr::<8192>::new();
                let _ = core::fmt::Write::write_fmt(&mut buffer, format_args!("{}", crate::sql::json::JsonWrite(&tree)));
                if buffer.is_truncated() {
                    return Err(sql_err!("54000", "jsonb value exceeds the supported size"));
                }
                Datum::Json { text: arena.alloc_str(buffer.as_str()).map_err(|_| arena_full())?, jsonb: true }
            }
            _ => return Err(cast_unsupported(&v, "jsonb")),
        },
        ColType::Array(element) => match v {
            Datum::Array { element: e, .. } if e == element => v,
            // A different element type: re-encode each element cast to it.
            Datum::Array { element: e, raw } => {
                let mut items = [Datum::Null; 1024];
                let n = load_array(raw, e, element, &mut items, 0, arena)?;
                Datum::Array { element, raw: crate::sql::array::build(&items[..n], arena)? }
            }
            Datum::Text(s) => Datum::Array { element, raw: crate::sql::array::parse_literal(s, element, arena)? },
            _ => return Err(cast_unsupported(&v, "array")),
        },
        ColType::Uuid => match v {
            Datum::Uuid(_) => v,
            Datum::Text(s) => Datum::Uuid(parse_uuid(s)?),
            _ => return Err(cast_unsupported(&v, "uuid")),
        },
        ColType::Bytea => match v {
            Datum::Bytea(_) => v,
            Datum::Text(s) => Datum::Bytea(parse_bytea(s, arena)?),
            _ => return Err(cast_unsupported(&v, "bytea")),
        },
        ColType::Numeric => match v {
            Datum::Numeric(_) => v,
            Datum::Int4(x) => Datum::Numeric(Numeric::from_i64(i64::from(x), arena)?),
            Datum::Int8(x) => Datum::Numeric(Numeric::from_i64(x, arena)?),
            Datum::Float8(x) => {
                // float8 -> numeric via the shortest round-trip decimal.
                let text = crate::stack_format!(64, "{}", x);
                Datum::Numeric(Numeric::parse(text.as_str(), arena)?)
            }
            Datum::Text(s) => Datum::Numeric(Numeric::parse(s, arena)?),
            _ => return Err(cast_unsupported(&v, "numeric")),
        },
        ColType::Range(kind) => match v {
            Datum::Range { kind: k, .. } if k == kind => v,
            Datum::Text(s) => {
                let p = crate::sql::range::parse(s, kind)?;
                Datum::Range { text: crate::sql::range::canonical(&p, kind, arena)?, kind }
            }
            _ => return Err(cast_unsupported(&v, kind.name())),
        },
        ColType::Bit { varying } => match v {
            Datum::Bit { bits, .. } => Datum::Bit { bits, varying },
            Datum::Text(s) => Datum::Bit { bits: validate_bits(s)?, varying },
            // int -> bit yields the two's-complement bits at the type's full
            // width; `apply_cast_typmod` then keeps the low N bits for bit(N).
            Datum::Int4(x) => Datum::Bit { bits: int_to_bits(x as u32 as u64, 32, arena)?, varying },
            Datum::Int8(x) => Datum::Bit { bits: int_to_bits(x as u64, 64, arena)?, varying },
            _ => return Err(cast_unsupported(&v, "bit")),
        },
        ColType::Multirange(kind) => match v {
            Datum::Multirange { kind: k, .. } if k == kind => v,
            // A range promotes to a one-element multirange (empty range → {}).
            Datum::Range { text, kind: k } if k == kind => {
                Datum::Multirange { text: crate::sql::range::multirange_from_range(text, kind, arena)?, kind }
            }
            Datum::Text(s) => {
                Datum::Multirange { text: crate::sql::range::parse_multirange(s, kind, arena)?, kind }
            }
            _ => return Err(cast_unsupported(&v, kind.multirange_name())),
        },
    };
    Ok(out)
}

/// Validates that every character of a bit-string literal is `0` or `1`,
/// returning it unchanged.
pub(crate) fn validate_bits(s: &str) -> Result<&str, SqlError> {
    for c in s.bytes() {
        if c != b'0' && c != b'1' {
            return Err(sql_err!(
                "22P02",
                "\"{}\" is not a valid binary digit",
                (c as char)
            ));
        }
    }
    Ok(s)
}

/// Interprets a `'0'`/`'1'` bit string as an unsigned integer (most significant
/// bit first). Bit strings wider than `max_bits` overflow the target loudly.
fn bits_to_uint(bits: &str, max_bits: usize, target: &'static str) -> Result<u64, SqlError> {
    if bits.len() > max_bits {
        return Err(overflow(target));
    }
    let mut value = 0u64;
    for c in bits.bytes() {
        value = (value << 1) | u64::from(c == b'1');
    }
    Ok(value)
}

/// Renders `value` as a `width`-bit `'0'`/`'1'` string, most significant bit
/// first (right-aligned: the low bits occupy the rightmost positions, higher
/// positions zero-fill). Supports widths beyond 64 bits.
pub fn int_to_bits(value: u64, width: usize, arena: &Arena) -> Result<&str, SqlError> {
    let out = arena
        .alloc_slice_with(width, |i| {
            let shift = width - 1 - i;
            if shift < 64 && (value >> shift) & 1 != 0 { b'1' } else { b'0' }
        })
        .map_err(|_| arena_full())?;
    Ok(unsafe { core::str::from_utf8_unchecked(out) })
}

/// Fits a bit string to a declared length `n`: fixed `bit(n)` zero-pads or
/// truncates on the right to exactly `n`; `varbit(n)` only truncates when
/// longer. (PostgreSQL adjusts bit-string length on the right.)
pub fn fit_bits<'a>(
    bits: &'a str,
    n: usize,
    varying: bool,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let len = bits.len();
    if len == n || (varying && len < n) {
        return Ok(Datum::Bit { bits, varying });
    }
    if len > n {
        let out = arena.alloc_str(&bits[..n]).map_err(|_| arena_full())?;
        return Ok(Datum::Bit { bits: out, varying });
    }
    // Fixed bit(n) shorter than n: zero-pad on the right.
    let out = arena
        .alloc_slice_with(n, |i| if i < len { bits.as_bytes()[i] } else { b'0' })
        .map_err(|_| arena_full())?;
    Ok(Datum::Bit { bits: unsafe { core::str::from_utf8_unchecked(out) }, varying })
}

pub(crate) fn parse_uuid(s: &str) -> Result<[u8; 16], SqlError> {
    let bad = || {
        sql_err!(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            "invalid input syntax for type uuid: \"{}\"",
            s
        )
    };
    let mut out = [0u8; 16];
    let mut nibbles = 0usize;
    for c in s.trim().chars() {
        if c == '-' {
            continue;
        }
        let d = c.to_digit(16).ok_or_else(bad)? as u8;
        if nibbles >= 32 {
            return Err(bad());
        }
        if nibbles.is_multiple_of(2) {
            out[nibbles / 2] = d << 4;
        } else {
            out[nibbles / 2] |= d;
        }
        nibbles += 1;
    }
    if nibbles != 32 {
        return Err(bad());
    }
    Ok(out)
}

/// `\x` hex form (PostgreSQL's default bytea output).
pub(crate) fn parse_bytea<'a>(s: &str, arena: &'a Arena) -> Result<&'a [u8], SqlError> {
    // The hex form `\xNN…`; otherwise PostgreSQL's escape form (printable bytes
    // verbatim, `\\` for backslash, `\ooo` octal for the rest).
    if let Some(hex) = s.strip_prefix("\\x") {
        let bad = || {
            sql_err!(
                sqlstate::INVALID_TEXT_REPRESENTATION,
                "invalid input syntax for type bytea"
            )
        };
        // Whitespace between hex digits is permitted.
        let out = arena.alloc_slice_with(hex.len() / 2 + 1, |_| 0u8).map_err(|_| arena_full())?;
        let mut n = 0usize;
        let mut high: Option<u8> = None;
        for &c in hex.as_bytes() {
            if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                continue;
            }
            let v = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return Err(bad()),
            };
            match high {
                None => high = Some(v),
                Some(h) => {
                    out[n] = (h << 4) | v;
                    n += 1;
                    high = None;
                }
            }
        }
        if high.is_some() {
            return Err(bad());
        }
        return Ok(&out[..n]);
    }
    crate::sql::encoding::escape_decode(s, arena)
}

/// Cast-to-text semantics (`true`/`false`), unlike wire output (`t`/`f`).
pub(crate) fn cast_to_text<'a>(v: Datum<'a>, arena: &'a Arena) -> Result<&'a str, SqlError> {
    match v {
        Datum::Text(s) => Ok(s),
        Datum::Bool(b) => Ok(if b { "true" } else { "false" }),
        Datum::Bytea(b) => {
            // 2 + 2 bytes per input byte, straight into the arena.
            let out = arena
                .alloc_slice_with(2 + b.len() * 2, |_| 0u8)
                .map_err(|_| arena_full())?;
            out[0] = b'\\';
            out[1] = b'x';
            const HEX: &[u8; 16] = b"0123456789abcdef";
            for (i, byte) in b.iter().enumerate() {
                out[2 + i * 2] = HEX[(byte >> 4) as usize];
                out[3 + i * 2] = HEX[(byte & 0xf) as usize];
            }
            Ok(unsafe { core::str::from_utf8_unchecked(out) })
        }
        other => arena.alloc_str_display(other).map_err(|_| arena_full()),
    }
}

fn to_i64_for_cast(v: &Datum, target: &'static str) -> Result<i64, SqlError> {
    if let Datum::Numeric(n) = v {
        return n.to_i64().map_err(|_| overflow(target));
    }
    match v {
        Datum::Int4(x) => Ok(i64::from(*x)),
        Datum::Int8(x) => Ok(*x),
        Datum::Bool(b) => Ok(i64::from(*b)),
        Datum::Float8(x) => {
            // PostgreSQL rounds half away from zero.
            let rounded = x.round();
            if rounded >= i64::MIN as f64 && rounded <= i64::MAX as f64 {
                Ok(rounded as i64)
            } else {
                Err(overflow(target))
            }
        }
        Datum::Text(s) => parse_int_literal(s).ok_or_else(|| bad_text(s, target)),
        Datum::Null => unreachable!("null handled by caller"),
        other => Err(cast_unsupported(other, target)),
    }
}

/// Parses an integer the way PostgreSQL's integer input does: optional sign, an
/// optional `0x`/`0o`/`0b` base prefix, and `_` digit separators (only between
/// digits). Returns None for anything malformed or out of `i64` range.
pub(crate) fn parse_int_literal(s: &str) -> Option<i64> {
    let t = s.trim();
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (radix, digits) = if let Some(r) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        (16, r)
    } else if let Some(r) = rest.strip_prefix("0o").or_else(|| rest.strip_prefix("0O")) {
        (8, r)
    } else if let Some(r) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
        (2, r)
    } else {
        (10, rest)
    };
    let db = digits.as_bytes();
    if db.is_empty() || db[0] == b'_' || db[db.len() - 1] == b'_' {
        return None;
    }
    let mut buffer = [0u8; 80];
    let mut n = 0;
    let mut prev_underscore = false;
    for &c in db {
        if c == b'_' {
            if prev_underscore {
                return None; // `__` is not allowed
            }
            prev_underscore = true;
            continue;
        }
        prev_underscore = false;
        if n >= buffer.len() {
            return None;
        }
        buffer[n] = c;
        n += 1;
    }
    let cleaned = core::str::from_utf8(&buffer[..n]).ok()?;
    let v = i64::from_str_radix(cleaned, radix).ok()?;
    Some(if neg { -v } else { v })
}
