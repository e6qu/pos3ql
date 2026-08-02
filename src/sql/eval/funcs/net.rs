//! Network address built-ins: the `inet`/`cidr` inspection and manipulation
//! functions (`family`, `host`, `masklen`, `set_masklen`, `broadcast`,
//! `netmask`, `hostmask`, `network`, `abbrev`, `text`, `inet_same_family`,
//! `inet_merge`) and the MAC helpers (`trunc`, `macaddr8_set7bit`).

use crate::sql::ast::Expr;
use crate::sql::net::{self, NetAddr};
use crate::sql::types::Datum;
use crate::sql_err;
use crate::util::StackStr;

use super::super::{ColumnLookup, EvalHooks, SqlError, arena_full, arity_err, eval_full, sqlstate};

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
            | "inet_same_family"
            | "inet_merge"
            | "macaddr8_set7bit"
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
                _ => Err(net_type_error(name)),
            }
        };
        let text = |s: &str| -> Result<Datum<'a>, SqlError> {
            Ok(Datum::Text(arena.alloc_str(s).map_err(|_| arena_full())?))
        };

        match name {
            "family" => {
                want(1)?;
                let d = arg(0)?;
                if d.is_null() {
                    return Ok(Datum::Null);
                }
                Ok(Datum::Int4(i32::from(net_arg(d)?.family)))
            }
            "masklen" => {
                want(1)?;
                let d = arg(0)?;
                if d.is_null() {
                    return Ok(Datum::Null);
                }
                Ok(Datum::Int4(i32::from(net_arg(d)?.bits)))
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
                        Ok(Datum::Cidr(n.with_masklen(bits as u8, true)))
                    }
                    Datum::Inet(n) => {
                        check_masklen(bits, n.max_bits())?;
                        Ok(Datum::Inet(n.with_masklen(bits as u8, false)))
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
                Ok(Datum::Bool(net_arg(a)?.family == net_arg(b)?.family))
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
                let d = arg(0)?;
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
