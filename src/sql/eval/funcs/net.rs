//! PostgreSQL network-address scalar and support functions.

use core::cmp::Ordering;

use crate::sql::ast::{BinaryOp, Expr, UnaryOp};
use crate::sql::net::{self, NetAddr};
use crate::sql::types::{ColType, Datum};
use crate::sql_err;
use crate::util::StackStr;

use super::super::{
    ColumnLookup, EvalHooks, SqlError, arena_full, arity_err, bad_text, cast_to, eval_full,
    int_arg, sqlstate, type_mismatch,
};

/// Handles the network-address family. Returns `None` if `name` is not one of
/// these functions, leaving the router to keep matching.
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
        "family"
            | "host"
            | "masklen"
            | "set_masklen"
            | "broadcast"
            | "netmask"
            | "hostmask"
            | "network"
            | "abbrev"
            | "text"
            | "inet_same_family"
            | "inet_merge"
            | "macaddr8_set7bit"
            | "inet_in"
            | "inet_out"
            | "inet_send"
            | "cidr_in"
            | "cidr_out"
            | "cidr_send"
            | "macaddr_in"
            | "macaddr_out"
            | "macaddr_send"
            | "macaddr8_in"
            | "macaddr8_out"
            | "macaddr8_send"
            | "network_eq"
            | "network_ne"
            | "network_lt"
            | "network_le"
            | "network_gt"
            | "network_ge"
            | "network_cmp"
            | "network_sub"
            | "network_subeq"
            | "network_sup"
            | "network_supeq"
            | "network_overlap"
            | "network_larger"
            | "network_smaller"
            | "hashinet"
            | "hashinetextended"
            | "inetnot"
            | "inetand"
            | "inetor"
            | "inetpl"
            | "int8pl_inet"
            | "inetmi_int8"
            | "inetmi"
            | "macaddr_eq"
            | "macaddr_ne"
            | "macaddr_lt"
            | "macaddr_le"
            | "macaddr_gt"
            | "macaddr_ge"
            | "macaddr_cmp"
            | "hashmacaddr"
            | "hashmacaddrextended"
            | "macaddr_not"
            | "macaddr_and"
            | "macaddr_or"
            | "macaddr8_eq"
            | "macaddr8_ne"
            | "macaddr8_lt"
            | "macaddr8_le"
            | "macaddr8_gt"
            | "macaddr8_ge"
            | "macaddr8_cmp"
            | "hashmacaddr8"
            | "hashmacaddr8extended"
            | "macaddr8_not"
            | "macaddr8_and"
            | "macaddr8_or"
            | "macaddr"
            | "macaddr8"
            | "cidr"
    ) {
        return None;
    }
    Some((|| -> Result<Datum<'a>, SqlError> {
        // The argument evaluator; `text()` and `trunc()` overload names shared
        // with other families, so a non-network argument must fall through to
        // the generic error rather than be claimed here.
        let arg = |i: usize| eval_full(args[i], arena, params, row, hooks);
        let want = |n: usize| -> Result<(), SqlError> {
            if args.len() != n || star {
                Err(arity_err(name, if star { 1 } else { args.len() }))
            } else {
                Ok(())
            }
        };
        // Reads an inet/cidr argument, or a type error naming the function.
        let net_arg = |d: Datum<'a>| -> Result<NetAddr, SqlError> {
            match d {
                Datum::Inet(n) | Datum::Cidr(n) => Ok(n),
                Datum::Text(text) => net::parse_inet(text).ok_or_else(|| bad_text(text, "inet")),
                _ => Err(net_type_error(name)),
            }
        };
        let text = |s: &str| -> Result<Datum<'a>, SqlError> {
            Ok(Datum::Text(arena.alloc_str(s).map_err(|_| arena_full())?))
        };

        let net_pair = || -> Result<Option<(NetAddr, NetAddr)>, SqlError> {
            want(2)?;
            let (left, right) = (arg(0)?, arg(1)?);
            if left.is_null() || right.is_null() {
                return Ok(None);
            }
            Ok(Some((net_arg(left)?, net_arg(right)?)))
        };

        match name {
            "inet_in" | "cidr_in" | "macaddr_in" | "macaddr8_in" => {
                want(1)?;
                let value = arg(0)?;
                let target = match name {
                    "inet_in" => ColType::Inet,
                    "cidr_in" => ColType::Cidr,
                    "macaddr_in" => ColType::Macaddr,
                    _ => ColType::Macaddr8,
                };
                cast_to(value, target, arena)
            }
            "inet_out" | "cidr_out" | "text" => {
                want(1)?;
                let value = arg(0)?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                let mut buf = StackStr::<64>::new();
                let network = net_arg(value)?;
                let _ = net::format_addr(&network, name == "cidr_out", &mut buf);
                text(buf.as_str())
            }
            "macaddr_out" | "macaddr8_out" => {
                want(1)?;
                let value = cast_to(
                    arg(0)?,
                    if name == "macaddr_out" {
                        ColType::Macaddr
                    } else {
                        ColType::Macaddr8
                    },
                    arena,
                )?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                let mut buf = StackStr::<24>::new();
                match value {
                    Datum::Macaddr(bytes) => {
                        let _ = net::format_mac(&bytes, &mut buf);
                    }
                    Datum::Macaddr8(bytes) => {
                        let _ = net::format_mac(&bytes, &mut buf);
                    }
                    other => return Err(type_mismatch(name, &other)),
                }
                text(buf.as_str())
            }
            "inet_send" | "cidr_send" => {
                want(1)?;
                let value = arg(0)?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                let network = net_arg(value)?;
                let len = network.addr_len();
                let out = arena
                    .alloc_slice_with(4 + len, |_| 0_u8)
                    .map_err(|_| arena_full())?;
                out[0] = if network.family() == 4 { 2 } else { 3 };
                out[1] = network.bits();
                out[2] = u8::from(name == "cidr_send");
                out[3] = len as u8;
                out[4..].copy_from_slice(&network.addr()[..len]);
                Ok(Datum::Bytea(out))
            }
            "macaddr_send" | "macaddr8_send" => {
                want(1)?;
                let value = cast_to(
                    arg(0)?,
                    if name == "macaddr_send" {
                        ColType::Macaddr
                    } else {
                        ColType::Macaddr8
                    },
                    arena,
                )?;
                let bytes: &[u8] = match &value {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Macaddr(bytes) => bytes,
                    Datum::Macaddr8(bytes) => bytes,
                    _ => return Err(type_mismatch(name, &value)),
                };
                Ok(Datum::Bytea(
                    arena.alloc_slice_copy(bytes).map_err(|_| arena_full())?,
                ))
            }
            "network_eq" | "network_ne" | "network_lt" | "network_le" | "network_gt"
            | "network_ge" | "network_cmp" | "network_larger" | "network_smaller" => {
                let Some((left, right)) = net_pair()? else {
                    return Ok(Datum::Null);
                };
                let comparison = net::network_cmp(&left, &right);
                Ok(match name {
                    "network_eq" => Datum::Bool(comparison == 0),
                    "network_ne" => Datum::Bool(comparison != 0),
                    "network_lt" => Datum::Bool(comparison < 0),
                    "network_le" => Datum::Bool(comparison <= 0),
                    "network_gt" => Datum::Bool(comparison > 0),
                    "network_ge" => Datum::Bool(comparison >= 0),
                    "network_cmp" => Datum::Int4(comparison),
                    "network_larger" => Datum::Inet(if comparison >= 0 { left } else { right }),
                    _ => Datum::Inet(if comparison <= 0 { left } else { right }),
                })
            }
            "network_sub" | "network_subeq" | "network_sup" | "network_supeq"
            | "network_overlap" => {
                let Some((left, right)) = net_pair()? else {
                    return Ok(Datum::Null);
                };
                let operator = match name {
                    "network_sub" => BinaryOp::Shl,
                    "network_subeq" => BinaryOp::NetContainedEq,
                    "network_sup" => BinaryOp::Shr,
                    "network_supeq" => BinaryOp::NetContainsEq,
                    _ => BinaryOp::Overlaps,
                };
                Ok(Datum::Bool(super::super::operators::network_relop(
                    operator, &left, &right,
                )))
            }
            "hashinet" | "hashinetextended" => {
                want(if name == "hashinet" { 1 } else { 2 })?;
                let value = arg(0)?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                let network = net_arg(value)?;
                let mut bytes = [0_u8; 18];
                bytes[0] = if network.family() == 4 { 2 } else { 3 };
                bytes[1] = network.bits();
                let len = network.addr_len();
                bytes[2..2 + len].copy_from_slice(&network.addr()[..len]);
                let bytes = &bytes[..2 + len];
                if name == "hashinet" {
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
            "inetnot" => {
                want(1)?;
                let value = arg(0)?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                Ok(Datum::Inet(super::super::operators::network_not(&net_arg(
                    value,
                )?)))
            }
            "inetand" | "inetor" => {
                let Some((left, right)) = net_pair()? else {
                    return Ok(Datum::Null);
                };
                super::super::operators::network_bitwise(
                    if name == "inetand" {
                        BinaryOp::BitAnd
                    } else {
                        BinaryOp::BitOr
                    },
                    &left,
                    &right,
                )
                .map(Datum::Inet)
            }
            "inetpl" | "inetmi_int8" | "int8pl_inet" => {
                want(2)?;
                let (network_index, integer_index) = if name == "int8pl_inet" {
                    (1, 0)
                } else {
                    (0, 1)
                };
                let network = arg(network_index)?;
                let Some(delta) = int_arg(name, args, integer_index, arena, params, row, hooks)?
                else {
                    return Ok(Datum::Null);
                };
                if network.is_null() {
                    return Ok(Datum::Null);
                }
                let delta = if name == "inetmi_int8" {
                    delta.checked_neg().ok_or_else(|| {
                        sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "result is out of range")
                    })?
                } else {
                    delta
                };
                super::super::operators::addr_offset(&net_arg(network)?, delta)
                    .map(Datum::Inet)
                    .ok_or_else(|| {
                        sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "result is out of range")
                    })
            }
            "inetmi" => {
                let Some((left, right)) = net_pair()? else {
                    return Ok(Datum::Null);
                };
                super::super::operators::network_distance(&left, &right).map(Datum::Int8)
            }
            "macaddr_eq" | "macaddr_ne" | "macaddr_lt" | "macaddr_le" | "macaddr_gt"
            | "macaddr_ge" | "macaddr_cmp" | "macaddr8_eq" | "macaddr8_ne" | "macaddr8_lt"
            | "macaddr8_le" | "macaddr8_gt" | "macaddr8_ge" | "macaddr8_cmp" => {
                want(2)?;
                let target = if name.starts_with("macaddr8_") {
                    ColType::Macaddr8
                } else {
                    ColType::Macaddr
                };
                let (left, right) = (
                    cast_to(arg(0)?, target, arena)?,
                    cast_to(arg(1)?, target, arena)?,
                );
                if left.is_null() || right.is_null() {
                    return Ok(Datum::Null);
                }
                let comparison = match (&left, &right) {
                    (Datum::Macaddr(left), Datum::Macaddr(right)) => left.cmp(right),
                    (Datum::Macaddr8(left), Datum::Macaddr8(right)) => left.cmp(right),
                    _ => return Err(net_type_error(name)),
                };
                let stem = name.strip_prefix("macaddr8_").unwrap_or_else(|| {
                    name.strip_prefix("macaddr_").expect("matched MAC function")
                });
                Ok(comparison_result(stem, comparison))
            }
            "hashmacaddr" | "hashmacaddrextended" | "hashmacaddr8" | "hashmacaddr8extended" => {
                let extended = name.ends_with("extended");
                want(if extended { 2 } else { 1 })?;
                let target = if name.contains("macaddr8") {
                    ColType::Macaddr8
                } else {
                    ColType::Macaddr
                };
                let value = cast_to(arg(0)?, target, arena)?;
                let bytes: &[u8] = match &value {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Macaddr(bytes) if !name.contains("macaddr8") => bytes,
                    Datum::Macaddr8(bytes) if name.contains("macaddr8") => bytes,
                    _ => return Err(net_type_error(name)),
                };
                if extended {
                    let Some(seed) = int_arg(name, args, 1, arena, params, row, hooks)? else {
                        return Ok(Datum::Null);
                    };
                    Ok(Datum::Int8(crate::sql::identity::hash_bytea_extended(
                        bytes, seed,
                    )))
                } else {
                    Ok(Datum::Int4(crate::sql::identity::hash_bytea(bytes)))
                }
            }
            "macaddr_not" | "macaddr8_not" => {
                want(1)?;
                let target = if name == "macaddr_not" {
                    ColType::Macaddr
                } else {
                    ColType::Macaddr8
                };
                super::super::operators::unary(
                    UnaryOp::BitNot,
                    cast_to(arg(0)?, target, arena)?,
                    arena,
                )
            }
            "macaddr_and" | "macaddr_or" | "macaddr8_and" | "macaddr8_or" => {
                want(2)?;
                let target = if name.starts_with("macaddr8_") {
                    ColType::Macaddr8
                } else {
                    ColType::Macaddr
                };
                super::super::operators::binary(
                    if name.ends_with("and") {
                        BinaryOp::BitAnd
                    } else {
                        BinaryOp::BitOr
                    },
                    cast_to(arg(0)?, target, arena)?,
                    cast_to(arg(1)?, target, arena)?,
                    false,
                    false,
                    arena,
                )
            }
            "macaddr" | "macaddr8" | "cidr" => {
                want(1)?;
                cast_to(
                    arg(0)?,
                    match name {
                        "macaddr" => ColType::Macaddr,
                        "macaddr8" => ColType::Macaddr8,
                        _ => ColType::Cidr,
                    },
                    arena,
                )
            }
            "family" => {
                want(1)?;
                let d = arg(0)?;
                if d.is_null() {
                    return Ok(Datum::Null);
                }
                Ok(Datum::Int4(i32::from(net_arg(d)?.family())))
            }
            "masklen" => {
                want(1)?;
                let d = arg(0)?;
                if d.is_null() {
                    return Ok(Datum::Null);
                }
                Ok(Datum::Int4(i32::from(net_arg(d)?.bits())))
            }
            "host" => {
                want(1)?;
                let d = arg(0)?;
                if d.is_null() {
                    return Ok(Datum::Null);
                }
                let mut buf = StackStr::<64>::new();
                let _ = net::format_addr(&net_arg(d)?.host_only(), false, &mut buf);
                text(buf.as_str())
            }
            "abbrev" => {
                want(1)?;
                let d = arg(0)?;
                let mut buf = StackStr::<64>::new();
                match d {
                    Datum::Null => return Ok(Datum::Null),
                    // cidr abbreviates (drops trailing zero octets); inet shows
                    // the full address with its mask.
                    Datum::Cidr(n) => {
                        let _ = net::format_cidr_abbrev(&n, &mut buf);
                    }
                    Datum::Inet(n) => {
                        let _ = net::format_addr(&n, true, &mut buf);
                    }
                    _ => return Err(net_type_error(name)),
                }
                text(buf.as_str())
            }
            "broadcast" => {
                want(1)?;
                let d = arg(0)?;
                if d.is_null() {
                    return Ok(Datum::Null);
                }
                Ok(Datum::Inet(net_arg(d)?.broadcast()))
            }
            "netmask" => {
                want(1)?;
                let d = arg(0)?;
                if d.is_null() {
                    return Ok(Datum::Null);
                }
                Ok(Datum::Inet(net_arg(d)?.netmask()))
            }
            "hostmask" => {
                want(1)?;
                let d = arg(0)?;
                if d.is_null() {
                    return Ok(Datum::Null);
                }
                Ok(Datum::Inet(net_arg(d)?.hostmask()))
            }
            "network" => {
                want(1)?;
                let d = arg(0)?;
                if d.is_null() {
                    return Ok(Datum::Null);
                }
                Ok(Datum::Cidr(net_arg(d)?.to_network()))
            }
            "set_masklen" => {
                want(2)?;
                let d = arg(0)?;
                let m = arg(1)?;
                if d.is_null() || m.is_null() {
                    return Ok(Datum::Null);
                }
                let Datum::Int4(bits) = m else {
                    return Err(net_type_error(name));
                };
                // cidr keeps its host bits clear; inet preserves them.
                match d {
                    Datum::Cidr(n) => {
                        check_masklen(bits, n.max_bits())?;
                        Ok(Datum::Cidr(
                            n.with_masklen(bits as u8, true)
                                .expect("checked mask length"),
                        ))
                    }
                    Datum::Inet(n) => {
                        check_masklen(bits, n.max_bits())?;
                        Ok(Datum::Inet(
                            n.with_masklen(bits as u8, false)
                                .expect("checked mask length"),
                        ))
                    }
                    _ => Err(net_type_error(name)),
                }
            }
            "inet_same_family" => {
                want(2)?;
                let (a, b) = (arg(0)?, arg(1)?);
                if a.is_null() || b.is_null() {
                    return Ok(Datum::Null);
                }
                Ok(Datum::Bool(net_arg(a)?.family() == net_arg(b)?.family()))
            }
            "inet_merge" => {
                want(2)?;
                let (a, b) = (arg(0)?, arg(1)?);
                if a.is_null() || b.is_null() {
                    return Ok(Datum::Null);
                }
                net::inet_merge(&net_arg(a)?, &net_arg(b)?)
                    .map(Datum::Cidr)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_FUNCTION,
                            "cannot merge addresses from different families"
                        )
                    })
            }
            "macaddr8_set7bit" => {
                want(1)?;
                let d = cast_to(arg(0)?, ColType::Macaddr8, arena)?;
                match d {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Macaddr8(mut b) => {
                        b[0] |= 0x02;
                        Ok(Datum::Macaddr8(b))
                    }
                    _ => Err(net_type_error(name)),
                }
            }
            _ => unreachable!("dispatch admitted an unhandled network function"),
        }
    })())
}

fn net_type_error(name: &str) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "function {}(...) does not accept this argument type",
        name
    )
}

fn check_masklen(bits: i32, max: u8) -> Result<(), SqlError> {
    if bits < 0 || bits > i32::from(max) {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "invalid mask length: {}",
            bits
        ));
    }
    Ok(())
}

fn comparison_result<'a>(operation: &str, ordering: Ordering) -> Datum<'a> {
    match operation {
        "eq" => Datum::Bool(ordering.is_eq()),
        "ne" => Datum::Bool(!ordering.is_eq()),
        "lt" => Datum::Bool(ordering.is_lt()),
        "le" => Datum::Bool(!ordering.is_gt()),
        "gt" => Datum::Bool(ordering.is_gt()),
        "ge" => Datum::Bool(!ordering.is_lt()),
        "cmp" => Datum::Int4(match ordering {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        }),
        _ => unreachable!("comparison support-function name"),
    }
}
