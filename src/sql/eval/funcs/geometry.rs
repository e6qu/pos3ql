//! PostgreSQL geometric constructors and scalar inspectors.

use core::fmt::Write as _;

use crate::sql::ast::Expr;
use crate::sql::geometry;
use crate::sql::types::{Datum, GeometryKind, PgFloat8};
use crate::{sql_err, util::StackStr};

use super::super::{ColumnLookup, EvalHooks, SqlError, arity_err, datum_f64, eval_full, sqlstate};

// PostgreSQL deliberately uses fuzzy comparisons for most geometric
// primitives. Keep this value in sync with `utils/geo_decls.h`.
const EPSILON: f64 = 1e-6;

fn fp_eq(left: f64, right: f64) -> bool {
    left == right || (left - right).abs() <= EPSILON
}

fn fp_compare(operator: &str, left: f64, right: f64) -> bool {
    match operator {
        "=" => fp_eq(left, right),
        "<>" => left != right && (left - right).abs() > EPSILON,
        "<" => left + EPSILON < right,
        "<=" => left <= right + EPSILON,
        ">" => left > right + EPSILON,
        ">=" => left + EPSILON >= right,
        _ => unreachable!("comparison operator was validated"),
    }
}

fn pg_gt(left: f64, right: f64) -> bool {
    match (left.is_nan(), right.is_nan()) {
        (true, false) => true,
        (false, true) | (true, true) => false,
        (false, false) => left > right,
    }
}

fn pg_max(left: f64, right: f64) -> f64 {
    if pg_gt(left, right) { left } else { right }
}

fn pg_min(left: f64, right: f64) -> f64 {
    if pg_gt(left, right) { right } else { left }
}

#[derive(Clone, Copy, Default)]
struct Point {
    x: f64,
    y: f64,
}

impl Point {
    fn distance(self, other: Self) -> f64 {
        (self.x - other.x).hypot(self.y - other.y)
    }

    fn close(self, other: Self) -> bool {
        fp_eq(self.x, other.x) && fp_eq(self.y, other.y)
    }
}

#[derive(Clone, Copy)]
struct Geo {
    kind: GeometryKind,
    points: [Point; 128],
    count: usize,
    closed: bool,
    extra: f64,
    extra2: f64,
}

fn decoded(kind: GeometryKind, text: &str) -> Result<Geo, SqlError> {
    let (values, count, closed) = points_parts(kind, text)?;
    let mut points = [Point::default(); 128];
    let point_count = match kind {
        GeometryKind::Circle | GeometryKind::Line => 1,
        _ => count / 2,
    };
    for (output, input) in points
        .iter_mut()
        .zip(values[..point_count * 2].as_chunks::<2>().0)
    {
        *output = Point {
            x: input[0],
            y: input[1],
        };
    }
    Ok(Geo {
        kind,
        points,
        count: point_count,
        closed,
        extra: match kind {
            GeometryKind::Circle => values[2],
            GeometryKind::Line => values[1],
            _ => 0.0,
        },
        extra2: if kind == GeometryKind::Line {
            values[2]
        } else {
            0.0
        },
    })
}

fn line_coefficients(kind: GeometryKind, text: &str) -> Result<(f64, f64, f64), SqlError> {
    let (values, _, _) = points_parts(kind, text)?;
    match kind {
        GeometryKind::Line => Ok((values[0], values[1], values[2])),
        GeometryKind::Lseg => {
            let (x1, y1, x2, y2) = (values[0], values[1], values[2], values[3]);
            Ok((y2 - y1, x1 - x2, x2 * y1 - x1 * y2))
        }
        _ => Err(type_error("line")),
    }
}

fn geo_from_datum(value: Datum<'_>) -> Result<(Geo, &str), SqlError> {
    match value {
        Datum::Geometry { kind, text } => Ok((decoded(kind, text)?, text)),
        other => Err(type_error(type_name_of_geometry(&other))),
    }
}

fn type_name_of_geometry(value: &Datum<'_>) -> &'static str {
    match value {
        Datum::Geometry { kind, .. } => kind.name(),
        _ => "geometric operator",
    }
}

fn bounds(geo: &Geo) -> (f64, f64, f64, f64) {
    if geo.kind == GeometryKind::Circle {
        let center = geo.points[0];
        return (
            center.x + geo.extra,
            center.y + geo.extra,
            center.x - geo.extra,
            center.y - geo.extra,
        );
    }
    let mut high_x = geo.points[0].x;
    let mut high_y = geo.points[0].y;
    let mut low_x = geo.points[0].x;
    let mut low_y = geo.points[0].y;
    for point in &geo.points[1..geo.count] {
        high_x = pg_max(high_x, point.x);
        high_y = pg_max(high_y, point.y);
        low_x = pg_min(low_x, point.x);
        low_y = pg_min(low_y, point.y);
    }
    (high_x, high_y, low_x, low_y)
}

fn cross(a: Point, b: Point, c: Point) -> f64 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

fn point_on_segment(point: Point, start: Point, end: Point) -> bool {
    cross(start, end, point).abs() <= EPSILON
        && point.x >= start.x.min(end.x) - EPSILON
        && point.x <= start.x.max(end.x) + EPSILON
        && point.y >= start.y.min(end.y) - EPSILON
        && point.y <= start.y.max(end.y) + EPSILON
}

fn segments_intersect(a: Point, b: Point, c: Point, d: Point) -> bool {
    let (ab_c, ab_d, cd_a, cd_b) = (
        cross(a, b, c),
        cross(a, b, d),
        cross(c, d, a),
        cross(c, d, b),
    );
    ((ab_c > EPSILON && ab_d < -EPSILON) || (ab_c < -EPSILON && ab_d > EPSILON))
        && ((cd_a > EPSILON && cd_b < -EPSILON) || (cd_a < -EPSILON && cd_b > EPSILON))
        || point_on_segment(c, a, b)
        || point_on_segment(d, a, b)
        || point_on_segment(a, c, d)
        || point_on_segment(b, c, d)
}

fn closest_on_segment(point: Point, start: Point, end: Point) -> Point {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let square = dx * dx + dy * dy;
    if square <= EPSILON {
        return start;
    }
    let t = (((point.x - start.x) * dx + (point.y - start.y) * dy) / square).clamp(0.0, 1.0);
    Point {
        x: start.x + t * dx,
        y: start.y + t * dy,
    }
}

fn point_in_polygon(point: Point, polygon: &Geo) -> bool {
    if polygon.count == 1 {
        return point.close(polygon.points[0]);
    }
    if polygon.count == 2 {
        return point_on_segment(point, polygon.points[0], polygon.points[1]);
    }
    let mut inside = false;
    for index in 0..polygon.count {
        let a = polygon.points[index];
        let b = polygon.points[(index + 1) % polygon.count];
        if point_on_segment(point, a, b) {
            return true;
        }
        if (a.y > point.y) != (b.y > point.y)
            && point.x < (b.x - a.x) * (point.y - a.y) / (b.y - a.y) + a.x
        {
            inside = !inside;
        }
    }
    inside
}

fn segment_count(geo: &Geo) -> usize {
    match geo.kind {
        GeometryKind::Lseg => 1,
        GeometryKind::Box | GeometryKind::Polygon => geo.count,
        GeometryKind::Path if geo.closed => geo.count,
        GeometryKind::Path => geo.count.saturating_sub(1),
        _ => 0,
    }
}

fn segment(geo: &Geo, index: usize) -> (Point, Point) {
    match geo.kind {
        GeometryKind::Lseg => (geo.points[0], geo.points[1]),
        GeometryKind::Box => {
            let (high_x, high_y, low_x, low_y) = bounds(geo);
            let corners = [
                Point { x: low_x, y: low_y },
                Point {
                    x: low_x,
                    y: high_y,
                },
                Point {
                    x: high_x,
                    y: high_y,
                },
                Point {
                    x: high_x,
                    y: low_y,
                },
            ];
            (corners[index], corners[(index + 1) % 4])
        }
        _ => (geo.points[index], geo.points[(index + 1) % geo.count]),
    }
}

fn point_on_geo(point: Point, geo: &Geo) -> bool {
    match geo.kind {
        GeometryKind::Point => point.close(geo.points[0]),
        GeometryKind::Line => {
            let (a, b, c) = (geo.points[0].x, geo.extra, geo.extra2);
            (a * point.x + b * point.y + c).abs() <= EPSILON
        }
        GeometryKind::Lseg | GeometryKind::Path => (0..segment_count(geo)).any(|index| {
            let (start, end) = segment(geo, index);
            point_on_segment(point, start, end)
        }),
        GeometryKind::Box => {
            let (high_x, high_y, low_x, low_y) = bounds(geo);
            point.x >= low_x - EPSILON
                && point.x <= high_x + EPSILON
                && point.y >= low_y - EPSILON
                && point.y <= high_y + EPSILON
        }
        GeometryKind::Polygon => point_in_polygon(point, geo),
        GeometryKind::Circle => point.distance(geo.points[0]) <= geo.extra + EPSILON,
    }
}

fn point_distance(point: Point, geo: &Geo) -> f64 {
    match geo.kind {
        GeometryKind::Point => point.distance(geo.points[0]),
        GeometryKind::Line => {
            let (a, b, c) = (geo.points[0].x, geo.extra, geo.extra2);
            (a * point.x + b * point.y + c).abs() / a.hypot(b)
        }
        GeometryKind::Lseg | GeometryKind::Path => {
            let segments = segment_count(geo);
            if segments == 0 {
                0.0
            } else {
                (0..segments)
                    .map(|index| {
                        let (start, end) = segment(geo, index);
                        point.distance(closest_on_segment(point, start, end))
                    })
                    .fold(f64::INFINITY, f64::min)
            }
        }
        GeometryKind::Box => {
            let (high_x, high_y, low_x, low_y) = bounds(geo);
            point.distance(Point {
                x: point.x.clamp(low_x, high_x),
                y: point.y.clamp(low_y, high_y),
            })
        }
        GeometryKind::Polygon => {
            if point_in_polygon(point, geo) {
                0.0
            } else {
                (0..segment_count(geo))
                    .map(|index| {
                        let (start, end) = segment(geo, index);
                        point.distance(closest_on_segment(point, start, end))
                    })
                    .fold(f64::INFINITY, f64::min)
            }
        }
        GeometryKind::Circle => (point.distance(geo.points[0]) - geo.extra).abs(),
    }
}

fn geo_line(geo: &Geo) -> (f64, f64, f64) {
    match geo.kind {
        GeometryKind::Line => (geo.points[0].x, geo.extra, geo.extra2),
        GeometryKind::Lseg => {
            let (start, end) = (geo.points[0], geo.points[1]);
            (
                end.y - start.y,
                start.x - end.x,
                end.x * start.y - start.x * end.y,
            )
        }
        _ => unreachable!("line coefficients require a line or segment"),
    }
}

fn point_on_line(geo: &Geo) -> Point {
    let (a, b, c) = geo_line(geo);
    if b.abs() > EPSILON {
        Point { x: 0.0, y: -c / b }
    } else {
        Point { x: -c / a, y: 0.0 }
    }
}

fn closest_between_segments(
    left_start: Point,
    left_end: Point,
    right_start: Point,
    right_end: Point,
) -> (f64, Point) {
    if segments_intersect(left_start, left_end, right_start, right_end) {
        let left = (
            left_end.y - left_start.y,
            left_start.x - left_end.x,
            left_end.x * left_start.y - left_start.x * left_end.y,
        );
        let right = (
            right_end.y - right_start.y,
            right_start.x - right_end.x,
            right_end.x * right_start.y - right_start.x * right_end.y,
        );
        return (0.0, line_intersection(left, right).unwrap_or(right_start));
    }
    let candidates = [
        (
            left_start.distance(closest_on_segment(left_start, right_start, right_end)),
            closest_on_segment(left_start, right_start, right_end),
        ),
        (
            left_end.distance(closest_on_segment(left_end, right_start, right_end)),
            closest_on_segment(left_end, right_start, right_end),
        ),
        (
            right_start.distance(closest_on_segment(right_start, left_start, left_end)),
            right_start,
        ),
        (
            right_end.distance(closest_on_segment(right_end, left_start, left_end)),
            right_end,
        ),
    ];
    candidates
        .into_iter()
        .min_by(|left, right| left.0.total_cmp(&right.0))
        .unwrap()
}

fn line_intersects_geo(line: &Geo, other: &Geo) -> bool {
    let coefficients = geo_line(line);
    if other.kind == GeometryKind::Line {
        let rhs = geo_line(other);
        return line_intersection(coefficients, rhs).is_some();
    }
    if other.kind == GeometryKind::Lseg {
        return line_intersection(coefficients, geo_line(other))
            .is_some_and(|point| point_on_segment(point, other.points[0], other.points[1]));
    }
    (0..segment_count(other)).any(|index| {
        let (start, end) = segment(other, index);
        let first = coefficients.0 * start.x + coefficients.1 * start.y + coefficients.2;
        let second = coefficients.0 * end.x + coefficients.1 * end.y + coefficients.2;
        first.abs() <= EPSILON
            || second.abs() <= EPSILON
            || first.is_sign_positive() != second.is_sign_positive()
    })
}

fn geo_intersects(left: &Geo, right: &Geo) -> bool {
    if left.kind == GeometryKind::Line {
        return line_intersects_geo(left, right);
    }
    if right.kind == GeometryKind::Line {
        return line_intersects_geo(right, left);
    }
    if left.kind == GeometryKind::Circle && right.kind == GeometryKind::Circle {
        return left.points[0].distance(right.points[0]) <= left.extra + right.extra + EPSILON;
    }
    if left.kind == GeometryKind::Circle {
        return point_distance(left.points[0], right) <= left.extra + EPSILON
            || right.points[..right.count]
                .iter()
                .any(|point| point_on_geo(*point, left));
    }
    if right.kind == GeometryKind::Circle {
        return geo_intersects(right, left);
    }
    if left.points[..left.count]
        .iter()
        .any(|point| point_on_geo(*point, right))
        || right.points[..right.count]
            .iter()
            .any(|point| point_on_geo(*point, left))
    {
        return true;
    }
    (0..segment_count(left)).any(|li| {
        let (la, lb) = segment(left, li);
        (0..segment_count(right)).any(|ri| {
            let (ra, rb) = segment(right, ri);
            segments_intersect(la, lb, ra, rb)
        })
    })
}

fn area_of(geo: &Geo) -> f64 {
    match geo.kind {
        GeometryKind::Box => {
            let (high_x, high_y, low_x, low_y) = bounds(geo);
            (high_x - low_x) * (high_y - low_y)
        }
        GeometryKind::Circle => core::f64::consts::PI * geo.extra * geo.extra,
        GeometryKind::Path | GeometryKind::Polygon => {
            let mut area = 0.0;
            for index in 0..geo.count {
                let next = (index + 1) % geo.count;
                area += geo.points[index].x * geo.points[next].y
                    - geo.points[next].x * geo.points[index].y;
            }
            area.abs() / 2.0
        }
        GeometryKind::Lseg => geo.points[0].distance(geo.points[1]),
        GeometryKind::Point | GeometryKind::Line => 0.0,
    }
}

fn render_geo<'a>(geo: &Geo, arena: &'a crate::mem::arena::Arena) -> Result<Datum<'a>, SqlError> {
    let mut out = StackStr::<2048>::new();
    match geo.kind {
        GeometryKind::Point => write_point(&mut out, geo.points[0].x, geo.points[0].y),
        GeometryKind::Lseg => {
            let _ = out.write_str("[");
            write_point(&mut out, geo.points[0].x, geo.points[0].y);
            let _ = out.write_str(",");
            write_point(&mut out, geo.points[1].x, geo.points[1].y);
            let _ = out.write_str("]");
        }
        GeometryKind::Box => {
            let (high_x, high_y, low_x, low_y) = bounds(geo);
            write_point(&mut out, high_x, high_y);
            let _ = out.write_str(",");
            write_point(&mut out, low_x, low_y);
        }
        GeometryKind::Path => {
            let mut values = [0.0; 256];
            for (index, point) in geo.points[..geo.count].iter().enumerate() {
                values[index * 2] = point.x;
                values[index * 2 + 1] = point.y;
            }
            write_points(&mut out, &values[..geo.count * 2], geo.closed);
        }
        GeometryKind::Polygon => {
            let _ = out.write_str("(");
            for (index, point) in geo.points[..geo.count].iter().enumerate() {
                if index != 0 {
                    let _ = out.write_str(",");
                }
                write_point(&mut out, point.x, point.y);
            }
            let _ = out.write_str(")");
        }
        GeometryKind::Line => {
            let _ = write!(
                out,
                "{{{},{},{}}}",
                PgFloat8(geo.points[0].x),
                PgFloat8(geo.extra),
                PgFloat8(geo.extra2)
            );
        }
        GeometryKind::Circle => {
            let _ = out.write_str("<");
            write_point(&mut out, geo.points[0].x, geo.points[0].y);
            let _ = write!(out, ",{}>", PgFloat8(geo.extra));
        }
    }
    if matches!(geo.kind, GeometryKind::Line | GeometryKind::Circle) {
        return arena
            .alloc_str(out.as_str())
            .map(|text| Datum::Geometry {
                kind: geo.kind,
                text,
            })
            .map_err(|_| super::super::arena_full());
    }
    geometry_value(geo.kind, out.as_str(), arena)
}

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

fn write_point(out: &mut StackStr<2048>, x: f64, y: f64) {
    let _ = write!(out, "({},{})", PgFloat8(x), PgFloat8(y));
}

fn write_points(out: &mut StackStr<2048>, values: &[f64], closed: bool) {
    let _ = out.write_str(if closed { "(" } else { "[" });
    for (index, point) in values.as_chunks::<2>().0.iter().enumerate() {
        if index != 0 {
            let _ = out.write_str(",");
        }
        write_point(out, point[0], point[1]);
    }
    let _ = out.write_str(if closed { ")" } else { "]" });
}

fn center_of(kind: GeometryKind, text: &str) -> Result<(f64, f64), SqlError> {
    let (values, count, _) = points_parts(kind, text)?;
    match kind {
        GeometryKind::Point | GeometryKind::Circle => Ok((values[0], values[1])),
        GeometryKind::Box | GeometryKind::Lseg => {
            Ok(((values[0] + values[2]) / 2.0, (values[1] + values[3]) / 2.0))
        }
        GeometryKind::Polygon => {
            let points = count / 2;
            let mut x = 0.0;
            let mut y = 0.0;
            for point in values[..count].as_chunks::<2>().0 {
                x += point[0];
                y += point[1];
            }
            Ok((x / points as f64, y / points as f64))
        }
        _ => Err(type_error("point")),
    }
}

fn box_parts(kind: GeometryKind, text: &str) -> Result<(f64, f64, f64, f64), SqlError> {
    let (values, count, _) = points_parts(kind, text)?;
    match kind {
        GeometryKind::Point => Ok((values[0], values[1], values[0], values[1])),
        GeometryKind::Box => Ok((values[0], values[1], values[2], values[3])),
        GeometryKind::Circle => {
            let side = values[2] / core::f64::consts::SQRT_2;
            Ok((
                values[0] + side,
                values[1] + side,
                values[0] - side,
                values[1] - side,
            ))
        }
        GeometryKind::Polygon => {
            let points = values[..count].as_chunks::<2>().0;
            let mut high_x = points[0][0];
            let mut high_y = points[0][1];
            let mut low_x = points[0][0];
            let mut low_y = points[0][1];
            for point in &points[1..] {
                high_x = pg_max(high_x, point[0]);
                high_y = pg_max(high_y, point[1]);
                low_x = pg_min(low_x, point[0]);
                low_y = pg_min(low_y, point[1]);
            }
            Ok((high_x, high_y, low_x, low_y))
        }
        _ => Err(type_error("box")),
    }
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
            | "diagonal"
            | "height"
            | "width"
            | "slope"
            | "bound_box"
            | "ishorizontal"
            | "isvertical"
            | "isparallel"
            | "isperp"
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
            "ishorizontal" | "isvertical" => {
                if star || !matches!(args.len(), 1 | 2) {
                    return Err(arity_err(name, args.len()));
                }
                let left = arg(0)?;
                if left.is_null() {
                    return Ok(Datum::Null);
                }
                if args.len() == 2 {
                    let right = arg(1)?;
                    if right.is_null() {
                        return Ok(Datum::Null);
                    }
                    let (left_x, left_y) = point_parts(point(left, name)?)?;
                    let (right_x, right_y) = point_parts(point(right, name)?)?;
                    return Ok(Datum::Bool(if name == "ishorizontal" {
                        fp_eq(left_y, right_y)
                    } else {
                        fp_eq(left_x, right_x)
                    }));
                }
                let Datum::Geometry { kind, text } = left else {
                    return Err(type_error(name));
                };
                if !matches!(kind, GeometryKind::Line | GeometryKind::Lseg) {
                    return Err(type_error(name));
                }
                let (a, b, _) = line_coefficients(kind, text)?;
                Ok(Datum::Bool(if name == "ishorizontal" {
                    fp_eq(a, 0.0)
                } else {
                    fp_eq(b, 0.0)
                }))
            }
            "isparallel" | "isperp" => {
                want(2)?;
                let (left, right) = (arg(0)?, arg(1)?);
                if left.is_null() || right.is_null() {
                    return Ok(Datum::Null);
                }
                let (
                    Datum::Geometry {
                        kind: left_kind,
                        text: left_text,
                    },
                    Datum::Geometry {
                        kind: right_kind,
                        text: right_text,
                    },
                ) = (left, right)
                else {
                    return Err(type_error(name));
                };
                if left_kind != right_kind
                    || !matches!(left_kind, GeometryKind::Line | GeometryKind::Lseg)
                {
                    return Err(type_error(name));
                }
                let left_line = line_coefficients(left_kind, left_text)?;
                let right_line = line_coefficients(right_kind, right_text)?;
                let left_geo = decoded(left_kind, left_text)?;
                let right_geo = decoded(right_kind, right_text)?;
                Ok(Datum::Bool(if left_kind == GeometryKind::Lseg {
                    if name == "isparallel" {
                        fp_eq(
                            segment_slope(&left_geo, false),
                            segment_slope(&right_geo, false),
                        )
                    } else {
                        fp_eq(
                            segment_slope(&left_geo, false),
                            segment_slope(&right_geo, true),
                        )
                    }
                } else if name == "isparallel" {
                    lines_parallel(left_line, right_line)
                } else {
                    lines_perpendicular(left_line, right_line)
                }))
            }
            "point" => {
                if args.len() == 1 && !star {
                    return match arg(0)? {
                        Datum::Null => Ok(Datum::Null),
                        Datum::Text(text) => built(GeometryKind::Point, text),
                        Datum::Geometry {
                            kind: GeometryKind::Point,
                            text,
                        } => built(GeometryKind::Point, text),
                        Datum::Geometry { kind, text }
                            if matches!(
                                kind,
                                GeometryKind::Box
                                    | GeometryKind::Circle
                                    | GeometryKind::Lseg
                                    | GeometryKind::Polygon
                            ) =>
                        {
                            let (x, y) = center_of(kind, text)?;
                            let mut out = StackStr::<2048>::new();
                            write_point(&mut out, x, y);
                            built(GeometryKind::Point, out.as_str())
                        }
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
                if args.len() == 1 && !star {
                    let value = arg(0)?;
                    if value.is_null() {
                        return Ok(Datum::Null);
                    }
                    return match (name, value) {
                        (
                            "lseg",
                            Datum::Geometry {
                                kind: GeometryKind::Box,
                                text,
                            },
                        ) => {
                            let (high_x, high_y, low_x, low_y) =
                                box_parts(GeometryKind::Box, text)?;
                            let mut out = StackStr::<2048>::new();
                            let _ = out.write_str("[");
                            write_point(&mut out, high_x, high_y);
                            let _ = out.write_str(",");
                            write_point(&mut out, low_x, low_y);
                            let _ = out.write_str("]");
                            built(GeometryKind::Lseg, out.as_str())
                        }
                        ("box", Datum::Geometry { kind, text })
                            if matches!(
                                kind,
                                GeometryKind::Point | GeometryKind::Circle | GeometryKind::Polygon
                            ) =>
                        {
                            let (high_x, high_y, low_x, low_y) = box_parts(kind, text)?;
                            let mut out = StackStr::<2048>::new();
                            write_point(&mut out, high_x, high_y);
                            let _ = out.write_str(",");
                            write_point(&mut out, low_x, low_y);
                            built(GeometryKind::Box, out.as_str())
                        }
                        _ => Err(type_error(name)),
                    };
                }
                want(2)?;
                let (left, right) = (arg(0)?, arg(1)?);
                if left.is_null() || right.is_null() {
                    return Ok(Datum::Null);
                }
                let (left, right) = (point(left, name)?, point(right, name)?);
                if name == "line" {
                    let (left_x, left_y) = point_parts(left)?;
                    let (right_x, right_y) = point_parts(right)?;
                    if fp_eq(left_x, right_x) && fp_eq(left_y, right_y) {
                        return Err(sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "invalid line specification: must be two distinct points"
                        ));
                    }
                }
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
                if args.len() == 1 && !star {
                    let value = arg(0)?;
                    if value.is_null() {
                        return Ok(Datum::Null);
                    }
                    return match value {
                        Datum::Geometry {
                            kind: GeometryKind::Box,
                            text,
                        } => {
                            let (high_x, high_y, low_x, low_y) =
                                box_parts(GeometryKind::Box, text)?;
                            let x = (high_x + low_x) / 2.0;
                            let y = (high_y + low_y) / 2.0;
                            let radius = (high_x - low_x).hypot(high_y - low_y) / 2.0;
                            let mut out = StackStr::<2048>::new();
                            let _ = out.write_str("<");
                            write_point(&mut out, x, y);
                            let _ = write!(out, ",{}>", PgFloat8(radius));
                            built(GeometryKind::Circle, out.as_str())
                        }
                        Datum::Geometry {
                            kind: GeometryKind::Polygon,
                            text,
                        } => {
                            let (values, count, _) = points_parts(GeometryKind::Polygon, text)?;
                            let (x, y) = center_of(GeometryKind::Polygon, text)?;
                            let mut radius = 0.0;
                            for point in values[..count].as_chunks::<2>().0 {
                                radius += (point[0] - x).hypot(point[1] - y);
                            }
                            radius /= (count / 2) as f64;
                            let mut out = StackStr::<2048>::new();
                            let _ = out.write_str("<");
                            write_point(&mut out, x, y);
                            let _ = write!(out, ",{}>", PgFloat8(radius));
                            built(GeometryKind::Circle, out.as_str())
                        }
                        _ => Err(type_error(name)),
                    };
                }
                want(2)?;
                let (center, radius) = (arg(0)?, arg(1)?);
                if center.is_null() || radius.is_null() {
                    return Ok(Datum::Null);
                }
                let center = point(center, name)?;
                let radius = datum_f64(name, radius)?;
                let mut text = StackStr::<256>::new();
                let _ = write!(text, "<{},{}>", center, PgFloat8(radius));
                arena
                    .alloc_str(text.as_str())
                    .map(|text| Datum::Geometry {
                        kind: GeometryKind::Circle,
                        text,
                    })
                    .map_err(|_| super::super::arena_full())
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
                if args.len() == 2 && !star {
                    let count = match arg(0)? {
                        Datum::Int2(value) => i64::from(value),
                        Datum::Int4(value) => i64::from(value),
                        Datum::Int8(value) => value,
                        Datum::Null => return Ok(Datum::Null),
                        _ => return Err(type_error(name)),
                    };
                    let circle = arg(1)?;
                    if circle.is_null() {
                        return Ok(Datum::Null);
                    }
                    if !(2..=128).contains(&count) {
                        return Err(sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "polygon must have between 2 and 128 points"
                        ));
                    }
                    let (values, _, _) = points_parts(
                        GeometryKind::Circle,
                        geometry(circle, GeometryKind::Circle, name)?,
                    )?;
                    if values[2].abs() <= EPSILON {
                        return Err(sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "cannot convert circle with radius zero to polygon"
                        ));
                    }
                    let mut out = StackStr::<2048>::new();
                    let _ = out.write_str("(");
                    let angle_step = core::f64::consts::TAU / count as f64;
                    for index in 0..count {
                        if index != 0 {
                            let _ = out.write_str(",");
                        }
                        let angle = angle_step * index as f64;
                        write_point(
                            &mut out,
                            values[0] - values[2] * angle.cos(),
                            values[1] + values[2] * angle.sin(),
                        );
                    }
                    let _ = out.write_str(")");
                    return built(GeometryKind::Polygon, out.as_str());
                }
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
                    Datum::Geometry {
                        kind: GeometryKind::Box,
                        text,
                    } => {
                        let (high_x, high_y, low_x, low_y) = box_parts(GeometryKind::Box, text)?;
                        let mut out = StackStr::<2048>::new();
                        let _ = out.write_str("(");
                        write_point(&mut out, low_x, low_y);
                        let _ = out.write_str(",");
                        write_point(&mut out, low_x, high_y);
                        let _ = out.write_str(",");
                        write_point(&mut out, high_x, high_y);
                        let _ = out.write_str(",");
                        write_point(&mut out, high_x, low_y);
                        let _ = out.write_str(")");
                        built(GeometryKind::Polygon, out.as_str())
                    }
                    circle @ Datum::Geometry {
                        kind: GeometryKind::Circle,
                        ..
                    } => {
                        let literal = match circle {
                            Datum::Geometry { text, .. } => text,
                            _ => unreachable!(),
                        };
                        let (values, _, _) = points_parts(GeometryKind::Circle, literal)?;
                        if values[2].abs() <= EPSILON {
                            return Err(sql_err!(
                                sqlstate::FEATURE_NOT_SUPPORTED,
                                "cannot convert circle with radius zero to polygon"
                            ));
                        }
                        let mut out = StackStr::<2048>::new();
                        let _ = out.write_str("(");
                        let angle_step = core::f64::consts::TAU / 12.0;
                        for index in 0..12 {
                            if index != 0 {
                                let _ = out.write_str(",");
                            }
                            let angle = angle_step * index as f64;
                            write_point(
                                &mut out,
                                values[0] - values[2] * angle.cos(),
                                values[1] + values[2] * angle.sin(),
                            );
                        }
                        let _ = out.write_str(")");
                        built(GeometryKind::Polygon, out.as_str())
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
            "diagonal" => {
                want(1)?;
                let value = arg(0)?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                let (high_x, high_y, low_x, low_y) =
                    box_parts(GeometryKind::Box, geometry(value, GeometryKind::Box, name)?)?;
                let mut out = StackStr::<2048>::new();
                let _ = out.write_str("[");
                write_point(&mut out, high_x, high_y);
                let _ = out.write_str(",");
                write_point(&mut out, low_x, low_y);
                let _ = out.write_str("]");
                built(GeometryKind::Lseg, out.as_str())
            }
            "height" | "width" => {
                want(1)?;
                let value = arg(0)?;
                if value.is_null() {
                    return Ok(Datum::Null);
                }
                let (high_x, high_y, low_x, low_y) =
                    box_parts(GeometryKind::Box, geometry(value, GeometryKind::Box, name)?)?;
                Ok(Datum::Float8(if name == "width" {
                    high_x - low_x
                } else {
                    high_y - low_y
                }))
            }
            "slope" => {
                want(2)?;
                let (left, right) = (arg(0)?, arg(1)?);
                if left.is_null() || right.is_null() {
                    return Ok(Datum::Null);
                }
                let (x1, y1) = point_parts(point(left, name)?)?;
                let (x2, y2) = point_parts(point(right, name)?)?;
                Ok(Datum::Float8(point_slope(
                    Point { x: x1, y: y1 },
                    Point { x: x2, y: y2 },
                )))
            }
            "bound_box" => {
                want(2)?;
                let (left, right) = (arg(0)?, arg(1)?);
                if left.is_null() || right.is_null() {
                    return Ok(Datum::Null);
                }
                let (ahx, ahy, alx, aly) =
                    box_parts(GeometryKind::Box, geometry(left, GeometryKind::Box, name)?)?;
                let (bhx, bhy, blx, bly) =
                    box_parts(GeometryKind::Box, geometry(right, GeometryKind::Box, name)?)?;
                let mut out = StackStr::<2048>::new();
                write_point(&mut out, ahx.max(bhx), ahy.max(bhy));
                let _ = out.write_str(",");
                write_point(&mut out, alx.min(blx), aly.min(bly));
                built(GeometryKind::Box, out.as_str())
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

fn operand_kind(oid: i32) -> Option<GeometryKind> {
    match crate::sql::types::ColType::from_oid(oid) {
        Some(crate::sql::types::ColType::Geometry(kind)) => Some(kind),
        _ => None,
    }
}

/// Static result contract for PostgreSQL's geometric operator overloads.
/// Keeping this beside execution prevents Describe from admitting a spelling
/// that runtime would later reinterpret as an integer or text-search operator.
pub(crate) fn operator_result(
    name: &str,
    argument_oids: &[i32],
) -> Option<crate::sql::types::ColType> {
    let mut kinds = [None; 2];
    for (output, oid) in kinds.iter_mut().zip(argument_oids) {
        *output = operand_kind(*oid);
    }
    if argument_oids.len() == 2 {
        match (kinds[0], kinds[1]) {
            (None, Some(known)) if argument_oids[0] == crate::sql::types::oid::UNKNOWN => {
                kinds[0] = unknown_operand_kind(name, known, true);
            }
            (Some(known), None) if argument_oids[1] == crate::sql::types::oid::UNKNOWN => {
                kinds[1] = unknown_operand_kind(name, known, false);
            }
            _ => {}
        }
    }
    exact_operator_result(name, &kinds[..argument_oids.len().min(kinds.len())])
}

fn exact_operator_result(
    name: &str,
    kinds: &[Option<GeometryKind>],
) -> Option<crate::sql::types::ColType> {
    use crate::sql::types::ColType;
    match (name, kinds) {
        ("@-@", [Some(GeometryKind::Lseg | GeometryKind::Path)]) => Some(ColType::Float8),
        (
            "@@",
            [
                Some(
                    GeometryKind::Box
                    | GeometryKind::Lseg
                    | GeometryKind::Polygon
                    | GeometryKind::Circle,
                ),
            ],
        ) => Some(ColType::Geometry(GeometryKind::Point)),
        ("#", [Some(GeometryKind::Path | GeometryKind::Polygon)]) => Some(ColType::Int4),
        ("?-" | "?|", [Some(GeometryKind::Line | GeometryKind::Lseg)]) => Some(ColType::Bool),
        (
            "+",
            [
                Some(
                    kind @ (GeometryKind::Point
                    | GeometryKind::Box
                    | GeometryKind::Path
                    | GeometryKind::Circle),
                ),
                Some(GeometryKind::Point),
            ],
        )
        | (
            "-" | "*" | "/",
            [
                Some(
                    kind @ (GeometryKind::Point
                    | GeometryKind::Box
                    | GeometryKind::Path
                    | GeometryKind::Circle),
                ),
                Some(GeometryKind::Point),
            ],
        ) => Some(ColType::Geometry(*kind)),
        ("+", [Some(GeometryKind::Path), Some(GeometryKind::Path)]) => {
            Some(ColType::Geometry(GeometryKind::Path))
        }
        ("#", [Some(GeometryKind::Box), Some(GeometryKind::Box)]) => {
            Some(ColType::Geometry(GeometryKind::Box))
        }
        ("#", [Some(GeometryKind::Lseg), Some(GeometryKind::Lseg)])
        | ("#", [Some(GeometryKind::Line), Some(GeometryKind::Line)])
        | (
            "##",
            [
                Some(GeometryKind::Point),
                Some(GeometryKind::Box | GeometryKind::Lseg | GeometryKind::Line),
            ],
        )
        | (
            "##",
            [
                Some(GeometryKind::Lseg),
                Some(GeometryKind::Box | GeometryKind::Lseg),
            ],
        )
        | ("##", [Some(GeometryKind::Line), Some(GeometryKind::Lseg)]) => {
            Some(ColType::Geometry(GeometryKind::Point))
        }
        ("<->", [Some(left), Some(right)]) if distance_pair(*left, *right) => Some(ColType::Float8),
        (operator, [Some(left), Some(right)]) if predicate_pair(operator, *left, *right) => {
            Some(ColType::Bool)
        }
        (operator, [Some(left), Some(right)]) if comparison_pair(operator, *left, *right) => {
            Some(ColType::Bool)
        }
        _ => None,
    }
}

/// Resolves PostgreSQL's one-known/one-unknown binary-operator shortcut for
/// geometric operands. An exact same-type overload wins; otherwise the only
/// viable geometric overload wins. Multiple viable types remain ambiguous.
pub(crate) fn unknown_operand_kind(
    name: &str,
    known: GeometryKind,
    unknown_on_left: bool,
) -> Option<GeometryKind> {
    let result_for = |candidate| {
        let kinds = if unknown_on_left {
            [Some(candidate), Some(known)]
        } else {
            [Some(known), Some(candidate)]
        };
        exact_operator_result(name, &kinds).is_some()
    };
    if result_for(known) {
        return Some(known);
    }
    let mut resolved = None;
    for candidate in [
        GeometryKind::Point,
        GeometryKind::Box,
        GeometryKind::Lseg,
        GeometryKind::Line,
        GeometryKind::Path,
        GeometryKind::Polygon,
        GeometryKind::Circle,
    ] {
        if result_for(candidate) {
            if resolved.is_some() {
                return None;
            }
            resolved = Some(candidate);
        }
    }
    resolved
}

pub(crate) fn unknown_operand_is_ambiguous(
    name: &str,
    known: GeometryKind,
    unknown_on_left: bool,
) -> bool {
    let result_for = |candidate| {
        let kinds = if unknown_on_left {
            [Some(candidate), Some(known)]
        } else {
            [Some(known), Some(candidate)]
        };
        exact_operator_result(name, &kinds).is_some()
    };
    !result_for(known)
        && [
            GeometryKind::Point,
            GeometryKind::Box,
            GeometryKind::Lseg,
            GeometryKind::Line,
            GeometryKind::Path,
            GeometryKind::Polygon,
            GeometryKind::Circle,
        ]
        .into_iter()
        .filter(|candidate| result_for(*candidate))
        .take(2)
        .count()
            == 2
}

fn comparison_pair(operator: &str, left: GeometryKind, right: GeometryKind) -> bool {
    if left != right {
        return false;
    }
    match left {
        GeometryKind::Point => operator == "<>",
        GeometryKind::Line => operator == "=",
        GeometryKind::Lseg | GeometryKind::Circle => {
            matches!(operator, "=" | "<>" | "<" | "<=" | ">" | ">=")
        }
        GeometryKind::Box | GeometryKind::Path => {
            matches!(operator, "=" | "<" | "<=" | ">" | ">=")
        }
        GeometryKind::Polygon => false,
    }
}

fn predicate_pair(operator: &str, left: GeometryKind, right: GeometryKind) -> bool {
    use GeometryKind::*;
    match operator {
        "@>" => matches!(
            (left, right),
            (Box, Point | Box)
                | (Path, Point)
                | (Polygon, Point | Polygon)
                | (Circle, Point | Circle)
        ),
        "<@" => matches!(
            (left, right),
            (Point, Box | Lseg | Line | Path | Polygon | Circle)
                | (Box, Box)
                | (Lseg, Box | Line)
                | (Polygon, Polygon)
                | (Circle, Circle)
        ),
        "&&" => left == right && matches!(left, Box | Polygon | Circle),
        "<<" | ">>" | "<<|" | "|>>" => {
            left == right && matches!(left, Point | Box | Polygon | Circle)
        }
        "&<" | "&>" | "&<|" | "|&>" => left == right && matches!(left, Box | Polygon | Circle),
        "<^" | ">^" => left == right && matches!(left, Point | Box),
        "?#" => matches!(
            (left, right),
            (Box, Box) | (Lseg, Box | Lseg | Line) | (Line, Box | Line) | (Path, Path)
        ),
        "?-" | "?|" => left == Point && right == Point,
        "?-|" | "?||" => left == right && matches!(left, Line | Lseg),
        "~=" => left == right && matches!(left, Point | Box | Polygon | Circle),
        _ => false,
    }
}

fn distance_pair(left: GeometryKind, right: GeometryKind) -> bool {
    left == right
        || left == GeometryKind::Point
        || right == GeometryKind::Point
        || matches!(
            (left, right),
            (GeometryKind::Box, GeometryKind::Lseg)
                | (GeometryKind::Lseg, GeometryKind::Box)
                | (GeometryKind::Lseg, GeometryKind::Line)
                | (GeometryKind::Line, GeometryKind::Lseg)
                | (GeometryKind::Polygon, GeometryKind::Circle)
                | (GeometryKind::Circle, GeometryKind::Polygon)
        )
}

fn line_intersection(left: (f64, f64, f64), right: (f64, f64, f64)) -> Option<Point> {
    if lines_parallel(left, right) {
        return None;
    }
    let determinant = left.0 * right.1 - right.0 * left.1;
    Some(Point {
        x: (left.1 * right.2 - right.1 * left.2) / determinant,
        y: (left.2 * right.0 - right.2 * left.0) / determinant,
    })
}

fn lines_parallel(left: (f64, f64, f64), right: (f64, f64, f64)) -> bool {
    if !fp_eq(left.1, 0.0) {
        fp_eq(right.0, left.0 * (right.1 / left.1))
    } else if !fp_eq(right.1, 0.0) {
        fp_eq(left.0, right.0 * (left.1 / right.1))
    } else {
        true
    }
}

fn lines_perpendicular(left: (f64, f64, f64), right: (f64, f64, f64)) -> bool {
    if fp_eq(left.0, 0.0) {
        return fp_eq(right.1, 0.0);
    }
    if fp_eq(right.0, 0.0) {
        return fp_eq(left.1, 0.0);
    }
    if fp_eq(left.1, 0.0) {
        return fp_eq(right.0, 0.0);
    }
    if fp_eq(right.1, 0.0) {
        return fp_eq(left.0, 0.0);
    }
    fp_eq((left.0 * right.0) / (left.1 * right.1), -1.0)
}

fn segment_slope(segment: &Geo, inverse: bool) -> f64 {
    let (first, second) = (segment.points[0], segment.points[1]);
    if fp_eq(first.x, second.x) {
        return if inverse { 0.0 } else { f64::INFINITY };
    }
    if fp_eq(first.y, second.y) {
        return if inverse { f64::INFINITY } else { 0.0 };
    }
    if inverse {
        (first.x - second.x) / (second.y - first.y)
    } else {
        (first.y - second.y) / (first.x - second.x)
    }
}

fn point_slope(first: Point, second: Point) -> f64 {
    if fp_eq(first.x, second.x) {
        f64::INFINITY
    } else if fp_eq(first.y, second.y) {
        0.0
    } else {
        (first.y - second.y) / (first.x - second.x)
    }
}

fn render_point<'a>(
    point: Point,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Datum<'a>, SqlError> {
    let mut out = StackStr::<2048>::new();
    write_point(&mut out, point.x, point.y);
    geometry_value(GeometryKind::Point, out.as_str(), arena)
}

fn distance(left: &Geo, right: &Geo) -> f64 {
    if left.kind == GeometryKind::Box && right.kind == GeometryKind::Box {
        let (lhx, lhy, llx, lly) = bounds(left);
        let (rhx, rhy, rlx, rly) = bounds(right);
        return Point {
            x: (lhx + llx) / 2.0,
            y: (lhy + lly) / 2.0,
        }
        .distance(Point {
            x: (rhx + rlx) / 2.0,
            y: (rhy + rly) / 2.0,
        });
    }
    if left.kind == GeometryKind::Circle && right.kind == GeometryKind::Circle {
        return (left.points[0].distance(right.points[0]) - left.extra - right.extra).max(0.0);
    }
    if left.kind == GeometryKind::Circle && right.kind == GeometryKind::Point {
        return (left.points[0].distance(right.points[0]) - left.extra).max(0.0);
    }
    if left.kind == GeometryKind::Point && right.kind == GeometryKind::Circle {
        return (left.points[0].distance(right.points[0]) - right.extra).max(0.0);
    }
    if left.kind == GeometryKind::Circle && right.kind == GeometryKind::Polygon {
        return (point_distance(left.points[0], right) - left.extra).max(0.0);
    }
    if left.kind == GeometryKind::Polygon && right.kind == GeometryKind::Circle {
        return (point_distance(right.points[0], left) - right.extra).max(0.0);
    }
    if geo_intersects(left, right) {
        return 0.0;
    }
    if left.kind == GeometryKind::Point {
        return point_distance(left.points[0], right);
    }
    if right.kind == GeometryKind::Point {
        return point_distance(right.points[0], left);
    }
    if left.kind == GeometryKind::Line && right.kind == GeometryKind::Line {
        return point_distance(point_on_line(right), left);
    }
    let mut minimum = f64::INFINITY;
    for index in 0..segment_count(left) {
        let (a, b) = segment(left, index);
        minimum = minimum.min(point_distance(a, right));
        minimum = minimum.min(point_distance(b, right));
    }
    for index in 0..segment_count(right) {
        let (a, b) = segment(right, index);
        minimum = minimum.min(point_distance(a, left));
        minimum = minimum.min(point_distance(b, left));
    }
    if left.kind == GeometryKind::Line {
        minimum = minimum.min(point_distance(right.points[0], left));
    }
    if right.kind == GeometryKind::Line {
        minimum = minimum.min(point_distance(left.points[0], right));
    }
    minimum
}

fn contains(left: &Geo, right: &Geo) -> bool {
    match (left.kind, right.kind) {
        (GeometryKind::Box, GeometryKind::Box) => {
            let (lhx, lhy, llx, lly) = bounds(left);
            let (rhx, rhy, rlx, rly) = bounds(right);
            rlx >= llx - EPSILON
                && rhx <= lhx + EPSILON
                && rly >= lly - EPSILON
                && rhy <= lhy + EPSILON
        }
        (GeometryKind::Polygon, GeometryKind::Polygon) => right.points[..right.count]
            .iter()
            .all(|point| point_in_polygon(*point, left)),
        (GeometryKind::Circle, GeometryKind::Circle) => {
            left.points[0].distance(right.points[0]) + right.extra <= left.extra + EPSILON
        }
        (GeometryKind::Circle, GeometryKind::Point) => {
            left.points[0].distance(right.points[0]) <= left.extra
        }
        (GeometryKind::Path, GeometryKind::Point) if left.closed => {
            point_in_polygon(right.points[0], left)
        }
        (GeometryKind::Box | GeometryKind::Line, GeometryKind::Lseg) => right.points[..right.count]
            .iter()
            .all(|point| point_on_geo(*point, left)),
        (_, GeometryKind::Point) => point_on_geo(right.points[0], left),
        _ => false,
    }
}

fn same(left: &Geo, right: &Geo) -> bool {
    if left.kind != right.kind {
        return false;
    }
    match left.kind {
        GeometryKind::Point => left.points[0].close(right.points[0]),
        GeometryKind::Box => {
            let a = bounds(left);
            let b = bounds(right);
            [a.0, a.1, a.2, a.3]
                .iter()
                .zip([b.0, b.1, b.2, b.3])
                .all(|(x, y)| (*x - y).abs() <= EPSILON)
        }
        GeometryKind::Circle => {
            left.points[0].close(right.points[0])
                && ((left.extra.is_nan() && right.extra.is_nan()) || fp_eq(left.extra, right.extra))
        }
        GeometryKind::Line => {
            let lhs = [left.points[0].x, left.extra, left.extra2];
            let rhs = [right.points[0].x, right.extra, right.extra2];
            if lhs.iter().chain(&rhs).any(|value| value.is_nan()) {
                return lhs
                    .iter()
                    .zip(rhs)
                    .all(|(a, b)| a == &b || (a.is_nan() && b.is_nan()));
            }
            let ratio = rhs
                .iter()
                .zip(lhs)
                .find_map(|(right, left)| (!fp_eq(*right, 0.0)).then_some(left / right))
                .unwrap_or(1.0);
            lhs.iter()
                .zip(rhs)
                .all(|(left, right)| fp_eq(*left, ratio * right))
        }
        GeometryKind::Polygon => {
            if left.count != right.count {
                return false;
            }
            (0..left.count).any(|offset| {
                (0..left.count).all(|index| {
                    left.points[index].close(right.points[(offset + index) % right.count])
                }) || (0..left.count).all(|index| {
                    left.points[index]
                        .close(right.points[(offset + right.count - index) % right.count])
                })
            })
        }
        GeometryKind::Lseg | GeometryKind::Path => {
            left.count == right.count
                && left.points[..left.count]
                    .iter()
                    .zip(&right.points[..right.count])
                    .all(|(a, b)| a.close(*b))
        }
    }
}

fn transform<'a>(
    name: &str,
    mut geo: Geo,
    point: Point,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Datum<'a>, SqlError> {
    let divisor = point.x * point.x + point.y * point.y;
    if name == "/" && divisor == 0.0 {
        return Err(sql_err!(sqlstate::DIVISION_BY_ZERO, "division by zero"));
    }
    for value in &mut geo.points[..geo.count] {
        *value = match name {
            "+" => Point {
                x: value.x + point.x,
                y: value.y + point.y,
            },
            "-" => Point {
                x: value.x - point.x,
                y: value.y - point.y,
            },
            "*" => Point {
                x: value.x * point.x - value.y * point.y,
                y: value.x * point.y + value.y * point.x,
            },
            "/" => Point {
                x: (value.x * point.x + value.y * point.y) / divisor,
                y: (value.y * point.x - value.x * point.y) / divisor,
            },
            _ => unreachable!(),
        };
    }
    if geo.kind == GeometryKind::Circle && matches!(name, "*" | "/") {
        let scale = point.x.hypot(point.y);
        geo.extra = if name == "*" {
            geo.extra * scale
        } else {
            geo.extra / scale
        };
    }
    render_geo(&geo, arena)
}

fn operator_undefined(name: &str) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "operator does not exist: {}",
        name
    )
}

/// Executes built-in geometric prefix and binary operators. `None` means the
/// operand signature is not a PostgreSQL geometric overload, allowing a user
/// catalog operator with the same spelling to resolve normally.
pub(crate) fn operator<'a>(
    name: &str,
    arguments: &[Datum<'a>],
    argument_oids: &[i32],
    arena: &'a crate::mem::arena::Arena,
) -> Option<Result<Datum<'a>, SqlError>> {
    operator_result(name, argument_oids)?;
    if arguments.iter().any(Datum::is_null) {
        return Some(Ok(Datum::Null));
    }
    Some((|| {
        let (left, left_text) = geo_from_datum(arguments[0])?;
        if arguments.len() == 1 {
            return match name {
                "@-@" => {
                    let length = (0..segment_count(&left))
                        .map(|index| {
                            let (a, b) = segment(&left, index);
                            a.distance(b)
                        })
                        .sum();
                    Ok(Datum::Float8(length))
                }
                "@@" => {
                    let (x, y) = center_of(left.kind, left_text)?;
                    render_point(Point { x, y }, arena)
                }
                "#" => Ok(Datum::Int4(left.count as i32)),
                "?-" | "?|" => {
                    let (a, b, _) = line_coefficients(left.kind, left_text)?;
                    Ok(Datum::Bool(if name == "?-" {
                        a.abs() <= EPSILON
                    } else {
                        b.abs() <= EPSILON
                    }))
                }
                _ => Err(operator_undefined(name)),
            };
        }
        let (right, right_text) = geo_from_datum(arguments[1])?;
        match name {
            "+" if left.kind == GeometryKind::Path && right.kind == GeometryKind::Path => {
                if left.closed || right.closed {
                    return Ok(Datum::Null);
                }
                if left.count + right.count > 128 {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "path has too many points"
                    ));
                }
                let mut joined = left;
                joined.points[left.count..left.count + right.count]
                    .copy_from_slice(&right.points[..right.count]);
                joined.count += right.count;
                render_geo(&joined, arena)
            }
            "+" | "-" | "*" | "/" if right.kind == GeometryKind::Point => {
                transform(name, left, right.points[0], arena)
            }
            "#" if left.kind == GeometryKind::Box && right.kind == GeometryKind::Box => {
                let (lhx, lhy, llx, lly) = bounds(&left);
                let (rhx, rhy, rlx, rly) = bounds(&right);
                let high_x = lhx.min(rhx);
                let high_y = lhy.min(rhy);
                let low_x = llx.max(rlx);
                let low_y = lly.max(rly);
                if high_x < low_x || high_y < low_y {
                    return Ok(Datum::Null);
                }
                let geo = Geo {
                    kind: GeometryKind::Box,
                    points: [Point::default(); 128],
                    count: 2,
                    closed: false,
                    extra: 0.0,
                    extra2: 0.0,
                };
                let mut geo = geo;
                geo.points[0] = Point {
                    x: high_x,
                    y: high_y,
                };
                geo.points[1] = Point { x: low_x, y: low_y };
                render_geo(&geo, arena)
            }
            "#" => {
                let left_line = line_coefficients(left.kind, left_text)?;
                let right_line = line_coefficients(right.kind, right_text)?;
                let Some(point) = line_intersection(left_line, right_line) else {
                    return Ok(Datum::Null);
                };
                if (left.kind == GeometryKind::Lseg
                    && !point_on_segment(point, left.points[0], left.points[1]))
                    || (right.kind == GeometryKind::Lseg
                        && !point_on_segment(point, right.points[0], right.points[1]))
                {
                    Ok(Datum::Null)
                } else {
                    render_point(point, arena)
                }
            }
            "##" => {
                if matches!(
                    (left.kind, right.kind),
                    (GeometryKind::Lseg, GeometryKind::Lseg)
                        | (GeometryKind::Line, GeometryKind::Lseg)
                ) && line_intersection(
                    line_coefficients(left.kind, left_text)?,
                    line_coefficients(right.kind, right_text)?,
                )
                .is_none()
                {
                    return Ok(Datum::Null);
                }
                let point = match (left.kind, right.kind) {
                    (GeometryKind::Point, GeometryKind::Box) => {
                        let (hx, hy, lx, ly) = bounds(&right);
                        Point {
                            x: left.points[0].x.clamp(lx, hx),
                            y: left.points[0].y.clamp(ly, hy),
                        }
                    }
                    (GeometryKind::Point, GeometryKind::Lseg) => {
                        closest_on_segment(left.points[0], right.points[0], right.points[1])
                    }
                    (GeometryKind::Point, GeometryKind::Line) => {
                        let (a, b, c) = line_coefficients(right.kind, right_text)?;
                        let d = (a * left.points[0].x + b * left.points[0].y + c) / (a * a + b * b);
                        Point {
                            x: left.points[0].x - a * d,
                            y: left.points[0].y - b * d,
                        }
                    }
                    (GeometryKind::Lseg, GeometryKind::Lseg) if geo_intersects(&left, &right) => {
                        line_intersection(
                            line_coefficients(left.kind, left_text)?,
                            line_coefficients(right.kind, right_text)?,
                        )
                        .unwrap_or(left.points[0])
                    }
                    (GeometryKind::Lseg, GeometryKind::Lseg) => {
                        closest_between_segments(
                            left.points[0],
                            left.points[1],
                            right.points[0],
                            right.points[1],
                        )
                        .1
                    }
                    (GeometryKind::Lseg, GeometryKind::Box) => {
                        if point_on_geo(left.points[0], &right) {
                            left.points[0]
                        } else if point_on_geo(left.points[1], &right) {
                            left.points[1]
                        } else {
                            (0..segment_count(&right))
                                .map(|index| {
                                    let (start, end) = segment(&right, index);
                                    closest_between_segments(
                                        left.points[0],
                                        left.points[1],
                                        start,
                                        end,
                                    )
                                })
                                .min_by(|left, right| left.0.total_cmp(&right.0))
                                .unwrap()
                                .1
                        }
                    }
                    (GeometryKind::Line, GeometryKind::Lseg) => {
                        if let Some(point) = line_intersection(
                            line_coefficients(left.kind, left_text)?,
                            line_coefficients(right.kind, right_text)?,
                        )
                        .filter(|point| point_on_segment(*point, right.points[0], right.points[1]))
                        {
                            point
                        } else if point_distance(right.points[0], &left)
                            <= point_distance(right.points[1], &left)
                        {
                            right.points[0]
                        } else {
                            right.points[1]
                        }
                    }
                    _ => return Err(operator_undefined(name)),
                };
                render_point(point, arena)
            }
            "<->"
                if left.kind == GeometryKind::Path
                    && right.kind == GeometryKind::Path
                    && (segment_count(&left) == 0 || segment_count(&right) == 0) =>
            {
                Ok(Datum::Null)
            }
            "<->" => Ok(Datum::Float8(distance(&left, &right))),
            "@>" => Ok(Datum::Bool(contains(&left, &right))),
            "<@" => Ok(Datum::Bool(contains(&right, &left))),
            "&&" => Ok(Datum::Bool(geo_intersects(&left, &right))),
            "<<" | ">>" | "&<" | "&>" | "<<|" | "|>>" | "&<|" | "|&>" | "<^" | ">^" => {
                let (lhx, lhy, llx, lly) = bounds(&left);
                let (rhx, rhy, rlx, rly) = bounds(&right);
                Ok(Datum::Bool(match name {
                    "<<" => lhx < rlx,
                    ">>" => llx > rhx,
                    "&<" => lhx <= rhx,
                    "&>" => llx >= rlx,
                    "<<|" => lhy < rly,
                    "|>>" => lly > rhy,
                    "&<|" => lhy <= rhy,
                    "|&>" => lly >= rly,
                    "<^" if left.kind == GeometryKind::Point => lhy < rly,
                    ">^" if left.kind == GeometryKind::Point => lly > rhy,
                    "<^" => lhy <= rly,
                    _ => lly >= rhy,
                }))
            }
            "?#" => Ok(Datum::Bool(geo_intersects(&left, &right))),
            "?-" => Ok(Datum::Bool(
                (left.points[0].y - right.points[0].y).abs() <= EPSILON,
            )),
            "?|" => Ok(Datum::Bool(
                (left.points[0].x - right.points[0].x).abs() <= EPSILON,
            )),
            "?-|" | "?||" => {
                let left_line = line_coefficients(left.kind, left_text)?;
                let right_line = line_coefficients(right.kind, right_text)?;
                Ok(Datum::Bool(if left.kind == GeometryKind::Lseg {
                    if name == "?||" {
                        fp_eq(segment_slope(&left, false), segment_slope(&right, false))
                    } else {
                        fp_eq(segment_slope(&left, false), segment_slope(&right, true))
                    }
                } else if name == "?||" {
                    lines_parallel(left_line, right_line)
                } else {
                    lines_perpendicular(left_line, right_line)
                }))
            }
            "~=" => Ok(Datum::Bool(same(&left, &right))),
            "=" | "<>" | "<" | "<=" | ">" | ">=" => {
                if matches!(left.kind, GeometryKind::Point | GeometryKind::Line)
                    || (left.kind == GeometryKind::Lseg && matches!(name, "=" | "<>"))
                {
                    let equal = same(&left, &right);
                    return Ok(Datum::Bool(if name == "=" { equal } else { !equal }));
                }
                if left.kind == GeometryKind::Path {
                    return Ok(Datum::Bool(match name {
                        "=" => left.count == right.count,
                        "<" => left.count < right.count,
                        "<=" => left.count <= right.count,
                        ">" => left.count > right.count,
                        _ => left.count >= right.count,
                    }));
                }
                Ok(Datum::Bool(fp_compare(
                    name,
                    area_of(&left),
                    area_of(&right),
                )))
            }
            _ => Err(operator_undefined(name)),
        }
    })())
}

pub(crate) fn subscript<'a>(
    kind: GeometryKind,
    text: &str,
    index: i64,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Datum<'a>, SqlError> {
    let geo = decoded(kind, text)?;
    match kind {
        GeometryKind::Point => match index {
            0 => Ok(Datum::Float8(geo.points[0].x)),
            1 => Ok(Datum::Float8(geo.points[0].y)),
            _ => Ok(Datum::Null),
        },
        GeometryKind::Line => match index {
            0 => Ok(Datum::Float8(geo.points[0].x)),
            1 => Ok(Datum::Float8(geo.extra)),
            2 => Ok(Datum::Float8(geo.extra2)),
            _ => Ok(Datum::Null),
        },
        GeometryKind::Box | GeometryKind::Lseg => match usize::try_from(index) {
            Ok(index @ 0..=1) => render_point(geo.points[index], arena),
            _ => Ok(Datum::Null),
        },
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "cannot subscript type {} because it does not support subscripting",
            kind.name()
        )),
    }
}

pub(crate) fn set_subscript<'a>(
    kind: GeometryKind,
    text: &str,
    index: i64,
    value: Datum<'a>,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Datum<'a>, SqlError> {
    let mut geo = decoded(kind, text)?;
    match kind {
        GeometryKind::Point | GeometryKind::Line => {
            let coordinate = datum_f64("point subscript assignment", value)?;
            match (kind, index) {
                (GeometryKind::Point | GeometryKind::Line, 0) => geo.points[0].x = coordinate,
                (GeometryKind::Point, 1) => geo.points[0].y = coordinate,
                (GeometryKind::Line, 1) => geo.extra = coordinate,
                (GeometryKind::Line, 2) => geo.extra2 = coordinate,
                _ => {
                    return Err(sql_err!(
                        sqlstate::ARRAY_SUBSCRIPT_ERROR,
                        "{} subscript {} is out of range",
                        kind.name(),
                        index
                    ));
                }
            }
        }
        GeometryKind::Box | GeometryKind::Lseg => {
            let Datum::Geometry {
                kind: GeometryKind::Point,
                text,
            } = value
            else {
                return Err(type_error("geometric subscript assignment"));
            };
            let replacement = decoded(GeometryKind::Point, text)?.points[0];
            match usize::try_from(index) {
                Ok(index @ 0..=1) => geo.points[index] = replacement,
                _ => {
                    return Err(sql_err!(
                        sqlstate::ARRAY_SUBSCRIPT_ERROR,
                        "{} subscript {} is out of range",
                        kind.name(),
                        index
                    ));
                }
            }
        }
        _ => {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "cannot subscript type {} because it does not support subscripting",
                kind.name()
            ));
        }
    }
    render_geo(&geo, arena)
}
