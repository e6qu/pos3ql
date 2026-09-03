//! PostgreSQL geometric constructors and scalar inspectors.

use core::fmt::Write as _;

use crate::sql::ast::Expr;
use crate::sql::geometry;
use crate::sql::types::{Datum, GeometryKind, PgFloat8};
use crate::{sql_err, util::StackStr};

use super::super::{ColumnLookup, EvalHooks, SqlError, arity_err, datum_f64, eval_full, sqlstate};

fn type_error(name: &str) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "function {} has incompatible argument types",
        name
    )
}

fn geometry<'a>(value: Datum<'a>, kind: GeometryKind, name: &str) -> Result<&'a str, SqlError> {
    match value {
        Datum::Geometry { kind: actual, text } if actual == kind => Ok(text),
        _ => Err(type_error(name)),
    }
}

fn point<'a>(value: Datum<'a>, name: &str) -> Result<&'a str, SqlError> {
    geometry(value, GeometryKind::Point, name)
}

fn geometry_value<'a>(
    kind: GeometryKind,
    text: &str,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Datum<'a>, SqlError> {
    Ok(Datum::Geometry {
        kind,
        text: geometry::parse(kind, text, arena)?,
    })
}

fn point_parts(text: &str) -> Result<(f64, f64), SqlError> {
    let mut values = [0.0; 256];
    let (count, _) = geometry::components(GeometryKind::Point, text, &mut values)?;
    debug_assert_eq!(count, 2);
    Ok((values[0], values[1]))
}

fn points_parts(kind: GeometryKind, text: &str) -> Result<([f64; 256], usize, bool), SqlError> {
    let mut values = [0.0; 256];
    let (count, closed) = geometry::components(kind, text, &mut values)?;
    Ok((values, count, closed))
}

/// Handles the constructor/accessor subset shared by normal SQL expression,
/// prepared-statement and routine evaluation paths.
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
        "point"
            | "lseg"
            | "path"
            | "box"
            | "polygon"
            | "line"
            | "circle"
            | "x"
            | "y"
            | "center"
            | "radius"
            | "diameter"
            | "area"
            | "npoints"
            | "isclosed"
            | "isopen"
            | "pclose"
            | "popen"
    ) {
        return None;
    }
    Some((|| -> Result<Datum<'a>, SqlError> {
        let want = |n: usize| {
            if star || args.len() != n {
                Err(arity_err(name, args.len()))
            } else {
                Ok(())
            }
        };
        let arg = |index| eval_full(args[index], arena, params, row, hooks);
        let built = |kind, text: &str| geometry_value(kind, text, arena);
        match name {
            "point" => {
                if args.len() == 1 && !star {
                    return match arg(0)? {
                        Datum::Null => Ok(Datum::Null),
                        Datum::Text(text) => built(GeometryKind::Point, text),
                        Datum::Geometry {
                            kind: GeometryKind::Point,
                            text,
                        } => built(GeometryKind::Point, text),
                        _ => Err(type_error(name)),
                    };
                }
                want(2)?;
                let (x, y) = (datum_f64(name, arg(0)?)?, datum_f64(name, arg(1)?)?);
                let mut text = StackStr::<128>::new();
                let _ = write!(text, "({},{})", PgFloat8(x), PgFloat8(y));
                built(GeometryKind::Point, text.as_str())
            }
            "lseg" | "box" | "line" => {
                want(2)?;
                let (left, right) = (arg(0)?, arg(1)?);
                if left.is_null() || right.is_null() {
                    return Ok(Datum::Null);
                }
                let (left, right) = (point(left, name)?, point(right, name)?);
                let mut text = StackStr::<512>::new();
                match name {
                    "lseg" => {
                        let _ = write!(text, "[{},{}]", left, right);
                        built(GeometryKind::Lseg, text.as_str())
                    }
                    "box" => {
                        let _ = write!(text, "{},{}", left, right);
                        built(GeometryKind::Box, text.as_str())
                    }
                    _ => {
                        let _ = write!(text, "({},{})", left, right);
                        built(GeometryKind::Line, text.as_str())
                    }
                }
            }
            "circle" => {
                want(2)?;
                let (center, radius) = (arg(0)?, arg(1)?);
                if center.is_null() || radius.is_null() {
                    return Ok(Datum::Null);
                }
                let center = point(center, name)?;
                let radius = datum_f64(name, radius)?;
                let mut text = StackStr::<256>::new();
                let _ = write!(text, "<{},{}>", center, PgFloat8(radius));
                built(GeometryKind::Circle, text.as_str())
            }
            "path" => {
                want(1)?;
                match arg(0)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Geometry {
                        kind: GeometryKind::Path,
                        text,
                    } => built(GeometryKind::Path, text),
                    Datum::Geometry {
                        kind: GeometryKind::Polygon,
                        text,
                    } => built(GeometryKind::Path, text),
                    _ => Err(type_error(name)),
                }
            }
            "polygon" => {
                want(1)?;
                match arg(0)? {
                    Datum::Null => Ok(Datum::Null),
                    Datum::Geometry {
                        kind: GeometryKind::Polygon,
                        text,
                    } => built(GeometryKind::Polygon, text),
                    Datum::Geometry {
                        kind: GeometryKind::Path,
                        text,
                    } => {
                        let mut polygon = StackStr::<2048>::new();
                        let _ = polygon.write_str("(");
                        let _ = polygon.write_str(&text[1..text.len() - 1]);
                        let _ = polygon.write_str(")");
                        built(GeometryKind::Polygon, polygon.as_str())
                    }
                    _ => Err(type_error(name)),
                }
            }
            "x" | "y" => {
                want(1)?;
                let value = arg(0)?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                let (x, y) = point_parts(point(value, name)?)?;
                Ok(Datum::Float8(if name == "x" { x } else { y }))
            }
            "radius" | "diameter" => {
                want(1)?;
                let value = arg(0)?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                let (values, _, _) = points_parts(
                    GeometryKind::Circle,
                    geometry(value, GeometryKind::Circle, name)?,
                )?;
                Ok(Datum::Float8(
                    values[2] * if name == "diameter" { 2.0 } else { 1.0 },
                ))
            }
            "center" => {
                want(1)?;
                let value = arg(0)?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                let (kind, text) = match value {
                    Datum::Geometry { kind, text }
                        if matches!(kind, GeometryKind::Box | GeometryKind::Circle) =>
                    {
                        (kind, text)
                    }
                    _ => return Err(type_error(name)),
                };
                let (values, _, _) = points_parts(kind, text)?;
                let (x, y) = if kind == GeometryKind::Circle {
                    (values[0], values[1])
                } else {
                    ((values[0] + values[2]) / 2.0, (values[1] + values[3]) / 2.0)
                };
                let mut text = StackStr::<128>::new();
                let _ = write!(text, "({},{})", PgFloat8(x), PgFloat8(y));
                built(GeometryKind::Point, text.as_str())
            }
            "npoints" => {
                want(1)?;
                let value = arg(0)?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                let (kind, text) = match value {
                    Datum::Geometry {
                        kind: kind @ (GeometryKind::Path | GeometryKind::Polygon),
                        text,
                    } => (kind, text),
                    _ => return Err(type_error(name)),
                };
                let (_, count, _) = points_parts(kind, text)?;
                Ok(Datum::Int4((count / 2) as i32))
            }
            "isclosed" | "isopen" | "pclose" | "popen" | "area" => {
                want(1)?;
                let value = arg(0)?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                let (kind, text) = match value {
                    Datum::Geometry { kind, text } => (kind, text),
                    _ => return Err(type_error(name)),
                };
                let (values, count, closed) = points_parts(kind, text)?;
                match name {
                    "isclosed" | "isopen" if kind == GeometryKind::Path => {
                        Ok(Datum::Bool(closed == (name == "isclosed")))
                    }
                    "pclose" | "popen" if kind == GeometryKind::Path => {
                        let mut canonical = StackStr::<2048>::new();
                        let _ = canonical.write_str(if name == "pclose" { "(" } else { "[" });
                        for index in (0..count).step_by(2) {
                            if index != 0 {
                                let _ = canonical.write_str(",");
                            }
                            let _ = write!(
                                canonical,
                                "({},{})",
                                PgFloat8(values[index]),
                                PgFloat8(values[index + 1])
                            );
                        }
                        let _ = canonical.write_str(if name == "pclose" { ")" } else { "]" });
                        built(GeometryKind::Path, canonical.as_str())
                    }
                    "area" if kind == GeometryKind::Circle => {
                        Ok(Datum::Float8(core::f64::consts::PI * values[2] * values[2]))
                    }
                    "area" if kind == GeometryKind::Box => Ok(Datum::Float8(
                        (values[0] - values[2]).abs() * (values[1] - values[3]).abs(),
                    )),
                    "area" if kind == GeometryKind::Path && !closed => Ok(Datum::Null),
                    "area" if kind == GeometryKind::Path => {
                        let mut twice_area = 0.0;
                        for index in (0..count).step_by(2) {
                            let next = (index + 2) % count;
                            twice_area +=
                                values[index] * values[next + 1] - values[next] * values[index + 1];
                        }
                        Ok(Datum::Float8(twice_area.abs() / 2.0))
                    }
                    _ => Err(type_error(name)),
                }
            }
            _ => unreachable!(),
        }
    })())
}
