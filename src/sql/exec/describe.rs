//! Static type analysis: what a query's result columns are, before a row of it
//! exists.
//!
//! The extended-query protocol makes a client ask for a statement's shape at
//! Describe time, so every expression's type and every column's name have to be
//! derivable from the statement and the catalog alone. That is what this does —
//! the same rules PostgreSQL's parse analysis applies, including which operand
//! combinations have no operator at all, so a query that cannot work is refused
//! here rather than part-way through a scan.

use crate::sql::ast::{Expr, SelectItem};
use crate::sql::eval::{sqlstate, SqlError};
use crate::sql::types::{oid, ColDesc, ColType};
use crate::sql_err;
use crate::storage::{ColumnMeta, TableDef};

/// Result-column names and types, statically inferred. Names borrow the
/// statement (aliases) or the catalog (wildcard columns); `'q` is whichever
/// is shorter at the call site.
pub fn describe_items<'q>(
    items: &[SelectItem<'q>],
    def: Option<&'q TableDef>,
    out: &mut [ColDesc<'q>],
) -> Result<usize, SqlError> {
    let mut n = 0;
    for item in items {
        let mut push = |desc: ColDesc<'q>| -> Result<(), SqlError> {
            if n == out.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "select list expands past {} columns",
                    out.len()
                ));
            }
            out[n] = desc;
            n += 1;
            Ok(())
        };
        match item {
            SelectItem::Wildcard => {
                let Some(def) = def else {
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "SELECT * requires a FROM clause"
                    ));
                };
                for c in def.columns() {
                    push(ColDesc::of_type(c.name.as_str(), c.ctype))?;
                }
            }
            SelectItem::TableWildcard(q) => {
                let matches = def.is_some_and(|d| d.name.as_str() == *q);
                if !matches {
                    return Err(sql_err!(
                        "42P01",
                        "missing FROM-clause entry for table \"{}\"",
                        q
                    ));
                }
                for c in def.expect("matched").columns() {
                    push(ColDesc::of_type(c.name.as_str(), c.ctype))?;
                }
            }
            SelectItem::RecordStar(base) => {
                describe_record_star(base, def, &mut push)?;
            }
            SelectItem::Expr { expression, alias } => {
                let (mut type_oid, mut typlen) = infer_type_pub(expression, def)?;
                // A bare unknown (string literal / param) resolves to text
                // for output, as PostgreSQL does.
                if type_oid == oid::UNKNOWN {
                    type_oid = oid::TEXT;
                    typlen = -1;
                }
                let name = alias.unwrap_or(derived_name(expression));
                push(ColDesc::new(name, type_oid, typlen))?;
            }
        }
    }
    Ok(n)
}

/// Emits one `ColDesc` per field of a `(record).*` expansion, resolving field
/// names and types at the caller's `'q` lifetime (single-table describe path).
fn describe_record_star<'q>(
    base: &Expr<'q>,
    def: Option<&'q TableDef>,
    push: &mut impl FnMut(ColDesc<'q>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    match base {
        Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("row") => {
            let resolver: &dyn ColTypeResolver = match def {
                Some(d) => &DefCols(d),
                None => &NoCols,
            };
            check_row_field_types(base, resolver)?;
            for (i, arg) in args.iter().take(RECORD_FIELD_NAMES.len()).enumerate() {
                let (oid, typlen) = infer_type_pub(arg, def)?;
                push(ColDesc::new(RECORD_FIELD_NAMES[i], oid, typlen))?;
            }
            Ok(())
        }
        Expr::Call { name, .. } if json_each_value_type(name).is_some() => {
            push(ColDesc::of_type("key", ColType::Text))?;
            push(ColDesc::of_type("value", json_each_value_type(name).expect("checked")))?;
            Ok(())
        }
        Expr::WholeRow(table) | Expr::Column { qualifier: None, name: table }
            if def.is_some_and(|d| d.name.as_str() == *table) =>
        {
            for c in def.expect("matched").columns() {
                push(ColDesc::of_type(c.name.as_str(), c.ctype))?;
            }
            Ok(())
        }
        _ => Err(sql_err!(
            "42809",
            "row expansion is not supported on this expression"
        )),
    }
}

/// Maps a type oid back to a ColType (numeric tower + common types).
pub(crate) fn coltype_of_oid(o: i32) -> Option<ColType> {
    Some(match o {
        oid::BOOL => ColType::Bool,
        oid::INT2 => ColType::Int2,
        oid::INT4 => ColType::Int4,
        oid::INT8 => ColType::Int8,
        oid::NUMERIC => ColType::Numeric,
        oid::FLOAT4 => ColType::Float4,
        oid::FLOAT8 => ColType::Float8,
        oid::TEXT => ColType::Text,
        oid::VARCHAR => ColType::Varchar,
        oid::BPCHAR => ColType::Bpchar,
        oid::DATE => ColType::Date,
        oid::TIMESTAMP => ColType::Timestamp,
        oid::TIMESTAMPTZ => ColType::Timestamptz,
        oid::TIME => ColType::Time,
        oid::TIMETZ => ColType::Timetz,
        oid::INTERVAL => ColType::Interval,
        oid::JSON => ColType::Json,
        oid::JSONB => ColType::Jsonb,
        oid::UUID => ColType::Uuid,
        oid::BYTEA => ColType::Bytea,
        oid::INT4MULTIRANGE => ColType::Multirange(crate::sql::types::RangeKind::Int4),
        oid::INT8MULTIRANGE => ColType::Multirange(crate::sql::types::RangeKind::Int8),
        oid::NUMMULTIRANGE => ColType::Multirange(crate::sql::types::RangeKind::Num),
        oid::DATEMULTIRANGE => ColType::Multirange(crate::sql::types::RangeKind::Date),
        oid::TSMULTIRANGE => ColType::Multirange(crate::sql::types::RangeKind::Ts),
        oid::TSTZMULTIRANGE => ColType::Multirange(crate::sql::types::RangeKind::Tstz),
        oid::BIT => ColType::Bit { varying: false },
        oid::VARBIT => ColType::Bit { varying: true },
        // `"char"` (internal single-byte) and `name` appear in catalog columns;
        // treat them as text so catalog-derived tables describe.
        18 | 19 => ColType::Text,
        // Array OIDs (catalog columns like indkey/conkey/indoption are arrays).
        1000 => ColType::Array(crate::sql::types::ArrElem::Bool),
        1005 | 1007 => ColType::Array(crate::sql::types::ArrElem::Int4),
        1016 => ColType::Array(crate::sql::types::ArrElem::Int8),
        1021 | 1022 => ColType::Array(crate::sql::types::ArrElem::Float8),
        1009 | 1015 | 1002 | 1014 => ColType::Array(crate::sql::types::ArrElem::Text),
        1231 => ColType::Array(crate::sql::types::ArrElem::Numeric),
        3904 => ColType::Range(crate::sql::types::RangeKind::Int4),
        3926 => ColType::Range(crate::sql::types::RangeKind::Int8),
        3906 => ColType::Range(crate::sql::types::RangeKind::Num),
        3912 => ColType::Range(crate::sql::types::RangeKind::Date),
        3908 => ColType::Range(crate::sql::types::RangeKind::Ts),
        3910 => ColType::Range(crate::sql::types::RangeKind::Tstz),
        _ => return None,
    })
}

/// Unifies two types by PostgreSQL's numeric preference (int4<int8<numeric<
/// float8); non-numeric or equal types keep the first.
/// The result type (oid, typlen) of an array function that promotes an array's
/// element type to also hold a new scalar element (`array_append`/`prepend`/
/// `replace`). Falls back to the array's own type when either is unknown.
fn array_promoted(array_oid: Option<i32>, elem_oid: Option<i32>) -> (i32, i16) {
    let fallback = (array_oid.unwrap_or(oid::TEXT), -1i16);
    let (Some(ao), Some(eo)) = (array_oid, elem_oid) else {
        return fallback;
    };
    let (Some(ColType::Array(ae)), Some(et)) = (coltype_of_oid(ao), coltype_of_oid(eo)) else {
        return fallback;
    };
    let unified = unify_numeric_tower(ae.to_coltype(), et);
    match crate::sql::types::ArrElem::from_coltype(unified) {
        Some(e) => (ColType::Array(e).oid(), -1),
        None => fallback,
    }
}

pub(crate) fn unify_numeric_tower(a: ColType, b: ColType) -> ColType {
    use ColType::*;
    let rank = |t: ColType| match t {
        Int4 => 1, Int8 => 2, Numeric => 3, Float8 => 4, _ => 0,
    };
    let (ra, rb) = (rank(a), rank(b));
    if ra > 0 && rb > 0 {
        if ra >= rb { a } else { b }
    } else {
        a
    }
}

/// PostgreSQL's error when an aggregate has no signature for the argument
/// type (e.g. sum(text), max(boolean)).
fn agg_undefined(name: &str, arg_oid: i32) -> SqlError {
    let table_name = coltype_of_oid(arg_oid).map(|t| t.name()).unwrap_or("unknown");
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "function {}({}) does not exist",
        name,
        table_name
    )
}

/// A specific output name for an expression, if it has one (parse_target.c
/// FigureColnameInternal): a column ref, a function call, a cast (the type
/// name), or a CASE whose ELSE yields a name. `None` for anything unnamed.
fn name_of<'a>(expression: &Expr<'a>) -> Option<&'a str> {
    match expression {
        Expr::Column { name, .. } => Some(name),
        // The desugarings of syntax-only constructs must not be labelled with
        // the internal name they carry: `SIMILAR TO` is an operator, so its
        // column is anonymous, while PostgreSQL does label OVERLAPS.
        Expr::Call { name: crate::sql::parser::SIMILAR_TO, .. } => None,
        Expr::Call { name: crate::sql::parser::OVERLAPS_PERIODS, .. } => Some("overlaps"),
        Expr::Call { name, .. } => Some(name),
        // A cast keeps its operand's name when the operand is a column or
        // function call (`count(*)::int` → `count`); otherwise it takes the
        // target type's name (`'x'::int` → `int4`), matching PostgreSQL.
        // A cast keeps the name of what it casts when that names itself — a
        // column, a function call, an array constructor. It does not chain
        // through another cast: `'x'::int4range::int4multirange` is named for
        // the outer type, so forwarding indiscriminately gets it wrong.
        Expr::Cast { operand, type_name, .. } => match operand {
            Expr::Column { .. } | Expr::Call { .. } | Expr::Array(_) | Expr::ArraySubquery(_) => {
                name_of(operand)
            }
            _ => ColType::from_sql_name(type_name).map(ColType::internal_name),
        },
        // A desugared CASE (`IS TRUE`, `IS DISTINCT FROM`) is anonymous, as
        // PostgreSQL labels those `?column?`; a real CASE forwards to its ELSE.
        Expr::Case { synthetic: true, .. } => None,
        Expr::Case { otherwise: Some(e), .. } => name_of(e),
        Expr::Array(_) | Expr::ArraySubquery(_) => Some("array"),
        // An array subscript keeps the base column's name (`m[1]` → `m`).
        Expr::Subscript { base, .. } => name_of(base),
        // `(record).field` is named after the field.
        Expr::Field { field, .. } => Some(field),
        _ => None,
    }
}

/// PostgreSQL's output-column name for a SELECT-list expression: `name_of`
/// with the per-node fallback ("case" for a CASE, else "?column?").
pub fn derived_name<'a>(expression: &Expr<'a>) -> &'a str {
    if let Some(n) = name_of(expression) {
        return n;
    }
    match expression {
        Expr::Case { synthetic: false, .. } => "case",
        Expr::WholeRow(t) => t,
        Expr::Exists(_) => "exists",
        Expr::ArraySubquery(_) | Expr::Array(_) => "array",
        // A scalar subquery is named by its single output column.
        Expr::Subquery(s) => match s.items.first() {
            Some(SelectItem::Expr { alias: Some(a), .. }) => a,
            Some(SelectItem::Expr { expression, alias: None }) => derived_name(expression),
            _ => "?column?",
        },
        _ => "?column?",
    }
}

/// Resolves a column reference's type during static analysis. Returns an
/// error for an unknown column (or absent FROM clause).
pub trait ColTypeResolver {
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError>;

    /// Whether an unqualified `name` names a FROM item (so a bare reference to
    /// it is a whole-row/record value). Defaults to false.
    fn is_whole_row(&self, _name: &str) -> bool {
        false
    }

    /// If a whole-row reference to `name` is actually a scalar (a
    /// set-returning-function scan's single output column), that column's type.
    /// Defaults to None, meaning the whole-row reference is an anonymous record.
    fn whole_row_scalar_type(&self, _name: &str) -> Option<ColType> {
        None
    }

    /// The columns of the FROM item exposed as `name`, for resolving a
    /// whole-row record's field shape (`(t).c`, `(t).*`). Defaults to None.
    fn table_columns(&self, _name: &str) -> Option<&[ColumnMeta]> {
        None
    }
}

/// Static field names PostgreSQL assigns an anonymous record (`ROW(...)`):
/// `f1`, `f2`, … Indexed 1-based by the caller.
pub const RECORD_FIELD_NAMES: [&str; 64] = [
    "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13", "f14",
    "f15", "f16", "f17", "f18", "f19", "f20", "f21", "f22", "f23", "f24", "f25", "f26", "f27",
    "f28", "f29", "f30", "f31", "f32", "f33", "f34", "f35", "f36", "f37", "f38", "f39", "f40",
    "f41", "f42", "f43", "f44", "f45", "f46", "f47", "f48", "f49", "f50", "f51", "f52", "f53",
    "f54", "f55", "f56", "f57", "f58", "f59", "f60", "f61", "f62", "f63", "f64",
];

/// The value type of `json_each`-family output's `value` column, for callers
/// outside this module (scope-based record-star expansion).
pub(crate) fn json_each_value_type_pub(name: &str) -> Option<ColType> {
    json_each_value_type(name)
}

/// The value type of `json_each`-family output's `value` column.
fn json_each_value_type(name: &str) -> Option<ColType> {
    if name.eq_ignore_ascii_case("json_each") {
        Some(ColType::Json)
    } else if name.eq_ignore_ascii_case("jsonb_each") {
        Some(ColType::Jsonb)
    } else if name.eq_ignore_ascii_case("json_each_text") || name.eq_ignore_ascii_case("jsonb_each_text") {
        Some(ColType::Text)
    } else {
        None
    }
}

/// Visits each `(field_name, type)` of a record-valued expression's shape,
/// returning the field count, or None when `base` is not a record whose shape
/// is statically known. Handles `ROW(...)`, a whole-row reference to a FROM
/// table, and the `json_each` family. The visited names borrow only for the
/// call, so callers copy them (into the arena, or into a `ColDesc`).
pub fn record_shape(
    base: &Expr,
    columns: &dyn ColTypeResolver,
    mut visit: impl FnMut(&str, ColType),
) -> Option<usize> {
    match base {
        Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("row") => {
            let n = args.len().min(RECORD_FIELD_NAMES.len());
            for (i, arg) in args[..n].iter().enumerate() {
                let oid = infer_type_res(arg, columns).ok()?.0;
                visit(RECORD_FIELD_NAMES[i], coltype_of_oid(oid).unwrap_or(ColType::Text));
            }
            Some(n)
        }
        Expr::Call { name, .. } if json_each_value_type(name).is_some() => {
            visit("key", ColType::Text);
            visit("value", json_each_value_type(name)?);
            Some(2)
        }
        Expr::WholeRow(table) => shape_from_columns(columns.table_columns(table)?, visit),
        Expr::Column { qualifier: None, name } if columns.is_whole_row(name) => {
            shape_from_columns(columns.table_columns(name)?, visit)
        }
        _ => None,
    }
}

fn shape_from_columns(cols: &[ColumnMeta], mut visit: impl FnMut(&str, ColType)) -> Option<usize> {
    for col in cols {
        visit(col.name.as_str(), col.ctype);
    }
    Some(cols.len())
}

/// PostgreSQL cannot form the composite type of a `ROW(...)` that contains a
/// bare unknown literal, so selecting a field of (or expanding) such a record
/// fails — even for a well-typed sibling field. Mirror that so `(ROW(1,'x')).f1`
/// errors exactly as PostgreSQL does.
pub fn check_row_field_types(base: &Expr, columns: &dyn ColTypeResolver) -> Result<(), SqlError> {
    if let Expr::Call { name, args, .. } = base
        && name.eq_ignore_ascii_case("row")
    {
        for arg in *args {
            if infer_type_res(arg, columns)?.0 == oid::UNKNOWN {
                return Err(sql_err!(
                    "XX000",
                    "failed to find conversion function from unknown to text"
                ));
            }
        }
    }
    Ok(())
}

/// The type of a record's field `field` (for `(base).field`), or an error if
/// `base` is not a record whose shape is known or the field does not exist.
pub fn record_field_type(
    base: &Expr,
    field: &str,
    columns: &dyn ColTypeResolver,
) -> Result<ColType, SqlError> {
    check_row_field_types(base, columns)?;
    let mut found = None;
    let shape = record_shape(base, columns, |name, ctype| {
        if found.is_none() && name.eq_ignore_ascii_case(field) {
            found = Some(ctype);
        }
    });
    if shape.is_none() {
        return Err(sql_err!(
            "42809",
            "field selection is not supported on this expression"
        ));
    }
    found.ok_or_else(|| {
        sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "could not identify column \"{}\" in record data type",
            field
        )
    })
}

/// No FROM clause: any column reference is an error.
pub struct NoCols;
impl ColTypeResolver for NoCols {
    fn resolve(&self, _q: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        Err(sql_err!(sqlstate::UNDEFINED_COLUMN, "column \"{}\" does not exist", name))
    }
}

/// A single table's columns.
pub struct DefCols<'d>(pub &'d TableDef);
impl ColTypeResolver for DefCols<'_> {
    fn resolve(&self, q: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        if let Some(q) = q
            && q != self.0.name.as_str() {
                return Err(sql_err!("42P01", "missing FROM-clause entry for table \"{}\"", q));
            }
        match self.0.column_index(name) {
            Some(i) => Ok(self.0.columns()[i].ctype),
            None => Err(sql_err!(sqlstate::UNDEFINED_COLUMN, "column \"{}\" does not exist", name)),
        }
    }

    fn is_whole_row(&self, name: &str) -> bool {
        name == self.0.name.as_str()
    }

    fn table_columns(&self, name: &str) -> Option<&[ColumnMeta]> {
        (name == self.0.name.as_str()).then(|| self.0.columns())
    }
}

/// Adapts a runtime row (`ColumnLookup`) to the static `ColTypeResolver` that
/// `infer_type_res` needs, so an expression's declared type can be recovered
/// during evaluation even when its value is NULL.
struct RowCols<'r, 'a>(&'r dyn crate::sql::eval::ColumnLookup<'a>);
impl<'a> ColTypeResolver for RowCols<'_, 'a> {
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        self.0.col_type(qualifier, name).ok_or_else(|| {
            sql_err!(sqlstate::UNDEFINED_COLUMN, "column \"{}\" does not exist", name)
        })
    }
}

/// The PostgreSQL type name `pg_typeof` reports for `expression` evaluated
/// against `row`, resolved statically (so a NULL value still names its declared
/// type, matching PostgreSQL). `None` when the static type can't be pinned down
/// (the caller then falls back to the runtime datum's type).
pub fn typeof_static<'a>(
    expression: &Expr,
    row: &dyn crate::sql::eval::ColumnLookup<'a>,
) -> Option<&'static str> {
    use crate::sql::types::ArrElem;
    let (type_oid, _) = infer_type_res(expression, &RowCols(row)).ok()?;
    Some(match coltype_of_oid(type_oid)? {
        ColType::Array(elem) => match elem {
            ArrElem::Bool => "boolean[]",
            ArrElem::Int4 => "integer[]",
            ArrElem::Int8 => "bigint[]",
            ArrElem::Float8 => "double precision[]",
            ArrElem::Text => "text[]",
            ArrElem::Numeric => "numeric[]",
            ArrElem::Date => "date[]",
            ArrElem::Timestamp => "timestamp without time zone[]",
            ArrElem::Timestamptz => "timestamp with time zone[]",
        },
        other => other.name(),
    })
}

/// Whether two concrete types have a comparison operator, per PostgreSQL:
/// same type, both numeric-tower, or both in the date/time family.
/// Whether an OID names a range type (so range operators apply).
fn is_range_oid(oid: i32) -> bool {
    matches!(coltype_of_oid(oid), Some(ColType::Range(_)))
}

fn is_multirange_oid(oid: i32) -> bool {
    matches!(coltype_of_oid(oid), Some(ColType::Multirange(_)))
}

fn comparable(a: ColType, b: ColType) -> bool {
    use ColType::*;
    // `json` has no equality operator in PostgreSQL — two documents that differ
    // only in whitespace or key order are the same value but not the same text,
    // so it declines to say. `jsonb`, which is canonicalized, does compare.
    if matches!(a, Json) || matches!(b, Json) {
        return false;
    }
    if a == b {
        return true;
    }
    let numeric = |t: ColType| matches!(t, Int4 | Int8 | Numeric | Float8);
    let datetime = |t: ColType| matches!(t, Date | Timestamp | Timestamptz);
    let timeofday = |t: ColType| matches!(t, Time | Timetz);
    let bit = |t: ColType| matches!(t, Bit { .. });
    (numeric(a) && numeric(b))
        || (datetime(a) && datetime(b))
        || (timeofday(a) && timeofday(b))
        || (bit(a) && bit(b))
}

fn operator_undefined(l: ColType, operator: &str, r: ColType) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "operator does not exist: {} {} {}",
        l.name(),
        operator,
        r.name()
    )
}

pub fn infer_type_pub(expression: &Expr, def: Option<&TableDef>) -> Result<(i32, i16), SqlError> {
    match def {
        Some(d) => infer_type_res(expression, &DefCols(d)),
        None => infer_type_res(expression, &NoCols),
    }
}

/// Static type inference with operator/aggregate validation, matching
/// PostgreSQL's plan-time analysis: comparisons and arithmetic over
/// incompatible types raise 42883 here, before any row is scanned. String
/// literals and parameters are UNKNOWN and coerce to the other operand.
pub fn infer_type_res(expression: &Expr, columns: &dyn ColTypeResolver) -> Result<(i32, i16), SqlError> {
    let of = |t: ColType| (t.oid(), t.typlen());
    Ok(match expression {
        Expr::Null | Expr::Str(_) | Expr::Param(_) => (oid::UNKNOWN, -2),
        // A whole-row reference is an anonymous record — unless it is a function
        // scan's whole row, which is its single scalar column.
        Expr::WholeRow(t) => match columns.whole_row_scalar_type(t) {
            Some(ty) => of(ty),
            None => (oid::RECORD, -1),
        },
        Expr::BitLit(_) => (oid::BIT, -1),
        Expr::Bool(_) => of(ColType::Bool),
        Expr::Int(v) => {
            if i32::try_from(*v).is_ok() { of(ColType::Int4) } else { of(ColType::Int8) }
        }
        Expr::Float(_) => of(ColType::Float8),
        Expr::NumericLit(_) => of(ColType::Numeric),
        Expr::Column { qualifier, name } => match columns.resolve(*qualifier, name) {
            Ok(t) => of(t),
            // A bare name that is not a column but names a FROM item is a
            // whole-row/record value — except a function scan's whole row,
            // which is its single scalar column.
            Err(e) if qualifier.is_none() && columns.is_whole_row(name) => {
                let _ = e;
                match columns.whole_row_scalar_type(name) {
                    Some(t) => of(t),
                    None => (oid::RECORD, -1),
                }
            }
            Err(e) => return Err(e),
        },
        Expr::Unary { operator, operand } => match operator {
            crate::sql::ast::UnaryOp::Not => of(ColType::Bool),
            crate::sql::ast::UnaryOp::Neg | crate::sql::ast::UnaryOp::BitNot => infer_type_res(operand, columns)?,
            crate::sql::ast::UnaryOp::SquareRoot | crate::sql::ast::UnaryOp::CubeRoot => {
                of(ColType::Float8)
            }
            crate::sql::ast::UnaryOp::AbsoluteValue => infer_type_res(operand, columns)?,
        },
        Expr::Binary { operator, left, right } => {
            use crate::sql::ast::BinaryOp::*;
            let lo = infer_type_res(left, columns)?.0;
            let ro = infer_type_res(right, columns)?.0;
            let is_bit = |o: i32| matches!(o, oid::BIT | oid::VARBIT);
            match operator {
                Eq | NotEq | Lt | LtEq | Gt | GtEq => {
                    // Unknown coerces; two concrete types must be comparable.
                    if lo != oid::UNKNOWN && ro != oid::UNKNOWN
                        && let (Some(a), Some(b)) = (coltype_of_oid(lo), coltype_of_oid(ro))
                            && !comparable(a, b) {
                                let sym = match operator {
                                    Eq => "=", NotEq => "<>", Lt => "<",
                                    LtEq => "<=", Gt => ">", _ => ">=",
                                };
                                return Err(operator_undefined(a, sym, b));
                            }
                    of(ColType::Bool)
                }
                And | Or | Like | ILike => of(ColType::Bool),
                Contains | ContainedBy | Overlaps | NotRightOf | NotLeftOf | Adjacent => {
                    of(ColType::Bool)
                }
                // Multirange set operators (`+`/`-`/`*`) return a multirange of
                // the same subtype.
                Add | Sub | Mul if is_multirange_oid(lo) || is_multirange_oid(ro) => {
                    (if is_multirange_oid(lo) { lo } else { ro }, -1)
                }
                // Range set operators (`+`/`-`/`*` on ranges) return a range of
                // the same type; shifts on ranges (`<<`/`>>`) return boolean.
                Add | Sub | Mul if is_range_oid(lo) || is_range_oid(ro) => {
                    (if is_range_oid(lo) { lo } else { ro }, -1)
                }
                Shl | Shr if is_range_oid(lo) || is_range_oid(ro) => of(ColType::Bool),
                // `jsonb - key/keys/index` deletes and returns jsonb.
                Sub if lo == oid::JSONB => (oid::JSONB, -1),
                // `||` concatenates arrays when either side is an array (the
                // array type is preserved), otherwise it is text concatenation.
                Concat if coltype_of_oid(lo).is_some_and(|t| matches!(t, ColType::Array(_))) => {
                    (lo, -1)
                }
                Concat if coltype_of_oid(ro).is_some_and(|t| matches!(t, ColType::Array(_))) => {
                    (ro, -1)
                }
                // `^` stays numeric when an operand is numeric (and none is a
                // float); otherwise it is double precision.
                Pow => {
                    if (lo == oid::NUMERIC || ro == oid::NUMERIC)
                        && lo != oid::FLOAT8
                        && ro != oid::FLOAT8
                        && lo != oid::FLOAT4
                        && ro != oid::FLOAT4
                    {
                        of(ColType::Numeric)
                    } else {
                        of(ColType::Float8)
                    }
                }
                // Bit-string concatenation yields varbit; otherwise text.
                Concat => {
                    if lo == oid::JSONB || ro == oid::JSONB {
                        (oid::JSONB, -1)
                    } else if is_bit(lo) || is_bit(ro) {
                        (oid::VARBIT, -1)
                    } else {
                        (oid::TEXT, -1)
                    }
                }
                // `json -> k` keeps the json/jsonb type; `->>` yields text.
                JsonGet | JsonPath => (if lo == oid::JSONB { oid::JSONB } else { oid::JSON }, -1),
                JsonGetText | JsonPathText => (oid::TEXT, -1),
                JsonDeletePath => (oid::JSONB, -1),
                JsonExists | JsonExistsAny | JsonExistsAll => of(ColType::Bool),
                // On bit strings the bitwise/shift operators return a bit
                // string; on integers they keep the wider integer width.
                BitAnd | BitOr | BitXor | Shl | Shr => {
                    if is_bit(lo) || is_bit(ro) {
                        (if lo == oid::VARBIT || ro == oid::VARBIT { oid::VARBIT } else { oid::BIT }, -1)
                    } else if lo == oid::INT8 || ro == oid::INT8 {
                        of(ColType::Int8)
                    } else {
                        of(ColType::Int4)
                    }
                }
                Add | Sub | Mul | Div | Mod => {
                    let numeric = |o: i32| {
                        matches!(o, oid::INT4 | oid::INT8 | oid::NUMERIC | oid::FLOAT8)
                    };
                    let int_like = |o: i32| matches!(o, oid::INT4 | oid::INT8 | oid::UNKNOWN);
                    // Date arithmetic: date - date -> int4; date +/- int -> date;
                    // int + date -> date.
                    if lo == oid::DATE && ro == oid::DATE && matches!(operator, Sub) {
                        return Ok(of(ColType::Int4));
                    }
                    // timestamp - timestamp -> interval.
                    if matches!(operator, Sub)
                        && (lo == oid::TIMESTAMP && ro == oid::TIMESTAMP
                            || lo == oid::TIMESTAMPTZ && ro == oid::TIMESTAMPTZ)
                    {
                        return Ok(of(ColType::Interval));
                    }
                    if lo == oid::DATE && matches!(operator, Add | Sub) && int_like(ro) {
                        return Ok(of(ColType::Date));
                    }
                    if ro == oid::DATE && matches!(operator, Add) && int_like(lo) {
                        return Ok(of(ColType::Date));
                    }
                    // Interval arithmetic: date/timestamp ± interval -> the
                    // timestamp type; interval ± interval -> interval.
                    let is_dt = |o: i32| matches!(o, oid::DATE | oid::TIMESTAMP | oid::TIMESTAMPTZ);
                    if matches!(operator, Add | Sub) {
                        if lo == oid::INTERVAL && ro == oid::INTERVAL {
                            return Ok(of(ColType::Interval));
                        }
                        if is_dt(lo) && ro == oid::INTERVAL {
                            return Ok(of(if lo == oid::TIMESTAMPTZ { ColType::Timestamptz } else { ColType::Timestamp }));
                        }
                        if matches!(operator, Add) && lo == oid::INTERVAL && is_dt(ro) {
                            return Ok(of(if ro == oid::TIMESTAMPTZ { ColType::Timestamptz } else { ColType::Timestamp }));
                        }
                        // A time of day keeps its own type, and its zone; the
                        // result wraps within the day.
                        let time_of_day = |o: i32| matches!(o, oid::TIME | oid::TIMETZ);
                        if time_of_day(lo) && ro == oid::INTERVAL {
                            return Ok(of(if lo == oid::TIMETZ { ColType::Timetz } else { ColType::Time }));
                        }
                        if matches!(operator, Add) && lo == oid::INTERVAL && time_of_day(ro) {
                            return Ok(of(if ro == oid::TIMETZ { ColType::Timetz } else { ColType::Time }));
                        }
                    }
                    // interval * number / number * interval / interval / number.
                    if (matches!(operator, Mul) && lo == oid::INTERVAL && numeric(ro))
                        || (matches!(operator, Mul) && numeric(lo) && ro == oid::INTERVAL)
                        || (matches!(operator, Div) && lo == oid::INTERVAL && numeric(ro))
                    {
                        return Ok(of(ColType::Interval));
                    }
                    let l_ok = lo == oid::UNKNOWN || numeric(lo);
                    let r_ok = ro == oid::UNKNOWN || numeric(ro);
                    if (!l_ok || !r_ok)
                        && let (Some(a), Some(b)) = (coltype_of_oid(lo), coltype_of_oid(ro)) {
                            let sym = match operator {
                                Add => "+", Sub => "-", Mul => "*", Div => "/", _ => "%",
                            };
                            return Err(operator_undefined(a, sym, b));
                        }
                    // Promotion: float8 > numeric > int8 > int4; unknown is
                    // absorbed by the concrete side.
                    if lo == oid::FLOAT8 || ro == oid::FLOAT8 {
                        of(ColType::Float8)
                    } else if lo == oid::NUMERIC || ro == oid::NUMERIC {
                        of(ColType::Numeric)
                    } else if lo == oid::INT8 || ro == oid::INT8 {
                        of(ColType::Int8)
                    } else if lo == oid::UNKNOWN && ro == oid::UNKNOWN {
                        of(ColType::Numeric)
                    } else if lo == oid::UNKNOWN {
                        (ro, coltype_of_oid(ro).map(|t| t.typlen()).unwrap_or(-1))
                    } else if ro == oid::UNKNOWN {
                        (lo, coltype_of_oid(lo).map(|t| t.typlen()).unwrap_or(-1))
                    } else {
                        of(ColType::Int4)
                    }
                }
            }
        }
        Expr::Cast { operand, type_name, .. } => {
            // `regclass` is oid-based: `'relname'::regclass` yields the relation
            // OID (so `attrelid = 'tbl'::regclass` compares OIDs, as pgx and
            // most tools introspect), while `oid::regclass` renders as the name.
            if type_name.eq_ignore_ascii_case("regclass") {
                let src = infer_type_res(operand, columns)?.0;
                return Ok(if src == oid::TEXT || src == oid::UNKNOWN {
                    of(ColType::Int4)
                } else {
                    of(ColType::Text)
                });
            }
            match ColType::from_sql_name(type_name) {
                Some(t) => of(t),
                None => return Err(sql_err!(sqlstate::UNDEFINED_OBJECT, "type \"{}\" does not exist", type_name)),
            }
        }
        Expr::IsNull { .. } => of(ColType::Bool),
        Expr::InList { .. } | Expr::Between { .. } | Expr::Like { .. } | Expr::Match { .. } => of(ColType::Bool),
        Expr::Case { whens, otherwise, .. } => {
            let mut acc: Option<ColType> = None;
            let mut consider = |e: &Expr| -> Result<(), SqlError> {
                let (o, _) = infer_type_res(e, columns)?;
                if let Some(t) = coltype_of_oid(o) {
                    acc = Some(match acc {
                        None => t,
                        Some(prev) => unify_numeric_tower(prev, t),
                    });
                }
                Ok(())
            };
            for (_, result) in whens.iter() {
                consider(result)?;
            }
            if let Some(e) = otherwise {
                consider(e)?;
            }
            match acc {
                Some(t) => of(t),
                None => (oid::UNKNOWN, -2),
            }
        }
        Expr::DefaultMarker => (oid::UNKNOWN, -2),
        // A scalar subquery's type is not known at static-inference time (its
        // body is resolved against storage only at execution); an array-from-
        // subquery is likewise unknown here. Both carry their real type in the
        // pre-evaluated datum.
        Expr::Subquery(_) | Expr::ArraySubquery(_) => (oid::UNKNOWN, -2),
        // `x IN (subquery)` and EXISTS are predicates: their result is boolean.
        Expr::InSubquery { .. } | Expr::Exists(_) => of(ColType::Bool),
        Expr::AnyAll { .. } => of(ColType::Bool),
        Expr::Array(items) => {
            // An unknown-typed element (a bare string literal) makes the array
            // text[], as PostgreSQL coerces it; only a concrete element type
            // narrows it further.
            let element = items
                .first()
                .and_then(|e| infer_type_res(e, columns).ok())
                .and_then(|(o, _)| coltype_of_oid(o))
                .and_then(crate::sql::types::ArrElem::from_coltype)
                .unwrap_or(crate::sql::types::ArrElem::Text);
            of(ColType::Array(element))
        }
        Expr::Subscript { base, .. } => {
            match coltype_of_oid(infer_type_res(base, columns)?.0) {
                Some(ColType::Array(e)) => of(e.to_coltype()),
                _ => (oid::UNKNOWN, -2),
            }
        }
        // `(record).field`: the field's type from the record's shape. When the
        // shape is not a statically known record (a `_pg_expandarray` result,
        // reached directly or through a derived-table column — the shape driver
        // introspection relies on), fall back to int4, matching its `.x`/`.n`
        // ordinal fields; a *known* record with a missing field still errors.
        Expr::Field { base, field } => match record_field_type(base, field, columns) {
            Ok(t) => of(t),
            Err(e) if e.sqlstate == "42809" => of(ColType::Int4),
            Err(e) => return Err(e),
        },
        Expr::Call { name, args, order_by, .. } => match *name {
            // Catalog-introspection helpers (for psql \d).
            "pg_get_userbyid" | "format_type" | "pg_get_expr" | "pg_get_indexdef"
            | "pg_get_constraintdef" | "pg_get_viewdef" | "pg_get_functiondef"
            | "col_description" | "obj_description" | "shobj_description"
            | "pg_encoding_to_char" | "array_to_string"
            | "pg_get_statisticsobjdef_columns" => (oid::TEXT, -1),
            "pg_table_is_visible" | "pg_type_is_visible" | "pg_function_is_visible"
            | "has_table_privilege" | "has_column_privilege" | "has_schema_privilege"
            | "pg_relation_is_publishable" => {
                of(ColType::Bool)
            }
            "array_length" | "cardinality" | "array_upper" | "array_lower" | "array_ndims" => {
                of(ColType::Int4)
            }
            "array_dims" => of(ColType::Text),
            "array_to_json" => of(ColType::Json),
            // Array-manipulation functions keep the array argument's type, but
            // promote its element type to hold a wider new/replacement element
            // (PostgreSQL's polymorphic anyarray/anyelement resolution).
            "array_append" => {
                let array_oid = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                let elem_oid = args.get(1).map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                array_promoted(array_oid, elem_oid)
            }
            "array_prepend" => {
                let elem_oid = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                let array_oid = args.get(1).map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                array_promoted(array_oid, elem_oid)
            }
            "array_replace" => {
                let array_oid = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                let to_oid = args.get(2).map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                array_promoted(array_oid, to_oid)
            }
            "array_cat" => {
                let a_oid = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                let b_oid = args.get(1).map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                // Element-type promotion across the two arrays.
                match (a_oid.and_then(coltype_of_oid), b_oid.and_then(coltype_of_oid)) {
                    (Some(ColType::Array(ae)), Some(ColType::Array(be))) => {
                        let e = unify_numeric_tower(ae.to_coltype(), be.to_coltype());
                        of(ColType::Array(crate::sql::types::ArrElem::from_coltype(e).unwrap_or(ae)))
                    }
                    _ => (a_oid.unwrap_or(oid::TEXT), -1),
                }
            }
            "array_remove" | "trim_array" => {
                args.first().map(|a| infer_type_res(a, columns)).transpose()?.unwrap_or((oid::TEXT, -1))
            }
            "pg_partition_ancestors" | "pg_partition_root" | "pg_partition_tree" => {
                args.first().map(|a| infer_type_res(a, columns)).transpose()?.unwrap_or((oid::INT4, 4))
            }
            // Window-only functions.
            "row_number" | "rank" | "dense_rank" | "ntile" => of(ColType::Int8),
            "percent_rank" | "cume_dist" => of(ColType::Float8),
            "lag" | "lead" | "first_value" | "last_value" | "nth_value" => args
                .first()
                .map(|a| infer_type_res(a, columns))
                .transpose()?
                .unwrap_or_else(|| of(ColType::Int8)),
            "count" => of(ColType::Int8),
            "row_to_json" | "to_json" | "json_build_object" | "json_build_array" => {
                of(ColType::Json)
            }
            "to_jsonb" | "jsonb_build_object" | "jsonb_build_array" => of(ColType::Jsonb),
            "row" => (oid::RECORD, -1),
            "sum" | "avg" => {
                let a = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                match a {
                    Some(oid::INT4) if *name == "sum" => of(ColType::Int8),
                    Some(oid::INT4) | Some(oid::INT8) | Some(oid::NUMERIC) => of(ColType::Numeric),
                    Some(oid::FLOAT8) => of(ColType::Float8),
                    Some(oid::UNKNOWN) | None => of(ColType::Numeric),
                    Some(other) => return Err(agg_undefined(name, other)),
                }
            }
            "min" | "max" => {
                // PostgreSQL defines min/max only where a total order is part
                // of the type's contract: the numeric tower, strings, the
                // temporal types, bytea and arrays. It has none for boolean,
                // uuid, json or jsonb, bit strings, ranges or multiranges —
                // this engine can order most of those internally, but ordering
                // them is not the same as PostgreSQL offering the aggregate.
                let t = args.first().map(|a| infer_type_res(a, columns)).transpose()?;
                if let Some((o, _)) = t {
                    let unordered = o == oid::BOOL
                        || o == oid::UUID
                        || matches!(
                            coltype_of_oid(o),
                            Some(
                                ColType::Json
                                    | ColType::Jsonb
                                    | ColType::Bit { .. }
                                    | ColType::Range(_)
                                    | ColType::Multirange(_)
                            )
                        );
                    if unordered {
                        return Err(agg_undefined(name, o));
                    }
                }
                t.unwrap_or_else(|| of(ColType::Int8))
            }
            // Functions returning the common type of their arguments (numeric
            // tower: float8 > numeric > int8 > int4), so a NULL of a wider type
            // still widens the result — matching PostgreSQL and the runtime
            // promotion in `greatest`/`least`.
            "greatest" | "least" => {
                let rank = |o: i32| {
                    if o == oid::FLOAT8 || o == oid::FLOAT4 {
                        4
                    } else if o == oid::NUMERIC {
                        3
                    } else if o == oid::INT8 {
                        2
                    } else if o == oid::INT4 {
                        1
                    } else {
                        0
                    }
                };
                let mut best: Option<(i32, i16)> = None;
                for a in args.iter() {
                    let t = infer_type_res(a, columns)?;
                    best = Some(match best {
                        None => t,
                        Some(p) => {
                            if rank(t.0) > rank(p.0) {
                                t
                            } else {
                                p
                            }
                        }
                    });
                }
                best.unwrap_or(of(ColType::Int8))
            }
            // `abs`/`nullif` take their first argument's type. `coalesce`
            // unifies across all of them, so an untyped NULL in front must not
            // decide the result: `coalesce(NULL, 1)` is integer, not text.
            "coalesce" | "abs" | "nullif" => {
                let mut chosen = None;
                for a in args.iter() {
                    let t = infer_type_res(a, columns)?;
                    if t.0 != oid::UNKNOWN {
                        chosen = Some(t);
                        break;
                    }
                    if !name.eq_ignore_ascii_case("coalesce") {
                        break;
                    }
                }
                match chosen {
                    Some(t) => t,
                    None if args.is_empty() => of(ColType::Int8),
                    // All arguments untyped: PostgreSQL resolves the unknown
                    // to text, exactly as it does for a bare literal.
                    None if name.eq_ignore_ascii_case("coalesce") => of(ColType::Text),
                    None => infer_type_res(args[0], columns)?,
                }
            }
            "length" | "char_length" | "character_length" | "octet_length" | "strpos"
            | "position" | "ascii" => of(ColType::Int4),
            // Math: sqrt/exp/ln/power stay numeric for a numeric argument (and
            // no float argument outranking it), else double; floor/ceil/trunc/
            // round/sign are numeric for a numeric argument and double
            // otherwise; mod returns the integer type of its arguments.
            "sqrt" | "exp" | "ln" | "power" | "pow" | "log" | "log10" => {
                let mut numeric = false;
                let mut float = false;
                for a in args.iter() {
                    match infer_type_res(a, columns)?.0 {
                        oid::NUMERIC => numeric = true,
                        oid::FLOAT8 | oid::FLOAT4 => float = true,
                        _ => {}
                    }
                }
                if numeric && !float { of(ColType::Numeric) } else { of(ColType::Float8) }
            }
            "div" | "trim_scale" | "to_number" => of(ColType::Numeric),
            "scale" | "min_scale" | "width_bucket" | "regexp_count" | "regexp_instr"
            | "array_position" | "jsonb_array_length" | "json_array_length"
            | "num_nonnulls" | "num_nulls" => of(ColType::Int4),
            "array_positions" => of(ColType::Array(crate::sql::types::ArrElem::Int4)),
            // array_fill returns an array of its value argument's element type.
            "array_fill" => {
                let elem = args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .and_then(|(oid, _)| coltype_of_oid(oid))
                    .and_then(crate::sql::types::ArrElem::from_coltype)
                    .unwrap_or(crate::sql::types::ArrElem::Int4);
                of(ColType::Array(elem))
            }
            "jsonb_typeof" | "json_typeof" | "json_extract_path_text"
            | "jsonb_extract_path_text" => of(ColType::Text),
            "json_extract_path" => of(ColType::Json),
            "jsonb_extract_path" => of(ColType::Jsonb),
            "regexp_substr" => of(ColType::Text),
            "regexp_like" => of(ColType::Bool),
            "regexp_split_to_array" | "string_to_array" => {
                of(ColType::Array(crate::sql::types::ArrElem::Text))
            }
            "format" | "overlay" | "regexp_replace" => of(ColType::Text),
            "floor" | "ceil" | "ceiling" | "sign" => {
                let a = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                if a == Some(oid::NUMERIC) { of(ColType::Numeric) } else { of(ColType::Float8) }
            }
            "round" | "trunc" => {
                if args.len() == 2 {
                    of(ColType::Numeric)
                } else {
                    let a = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                    if a == Some(oid::NUMERIC) { of(ColType::Numeric) } else { of(ColType::Float8) }
                }
            }
            "mod" | "gcd" | "lcm" => {
                let a = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                let b = args.get(1).map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                // `mod` keeps a numeric operand's type; gcd/lcm are integer-only.
                if *name == "mod" && (a == Some(oid::NUMERIC) || b == Some(oid::NUMERIC)) {
                    of(ColType::Numeric)
                } else if a == Some(oid::INT8) || b == Some(oid::INT8) {
                    of(ColType::Int8)
                } else {
                    of(ColType::Int4)
                }
            }
            "to_hex" | "md5" | "to_char" | "pg_size_pretty" => of(ColType::Text),
            "factorial" => of(ColType::Numeric),
            "bit_length" => of(ColType::Int4),
            "starts_with" => of(ColType::Bool),
            "cbrt" | "sin" | "cos" | "tan" | "cot" | "asin" | "acos" | "atan" | "atan2" | "sinh"
            | "cosh" | "tanh" | "asinh" | "acosh" | "atanh" | "degrees" | "radians" | "pi" => {
                of(ColType::Float8)
            }
            "bool_and" | "bool_or" | "every" => of(ColType::Bool),
            // Bitwise aggregates preserve the argument's (integer or bit) type.
            "bit_and" | "bit_or" | "bit_xor" => {
                args.first().map(|a| infer_type_res(a, columns)).transpose()?.unwrap_or(of(ColType::Int4))
            }
            // Single-argument variance/stddev mirror the input class: numeric for
            // integer/numeric inputs, double precision for float8 (PostgreSQL's
            // aggregate signatures).
            "var_pop" | "var_samp" | "variance" | "stddev_pop" | "stddev_samp" | "stddev" => {
                let a = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                match a {
                    Some(oid::FLOAT8) | Some(oid::FLOAT4) => of(ColType::Float8),
                    _ => of(ColType::Numeric),
                }
            }
            // The two-argument regression/covariance/correlation aggregates take
            // and return double precision; regr_count returns bigint.
            "corr" | "covar_pop" | "covar_samp" | "regr_slope" | "regr_intercept" | "regr_r2"
            | "regr_avgx" | "regr_avgy" | "regr_sxx" | "regr_syy" | "regr_sxy" => {
                of(ColType::Float8)
            }
            "regr_count" => of(ColType::Int8),
            "string_agg" => of(ColType::Text),
            "array_agg" => {
                // Element type from the argument; the result is elem[].
                let elem = args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .and_then(|(oid, _)| coltype_of_oid(oid))
                    .and_then(crate::sql::types::ArrElem::from_coltype)
                    .unwrap_or(crate::sql::types::ArrElem::Int4);
                of(ColType::Array(elem))
            }
            // Ordered-set aggregates: percentile_cont yields double precision
            // (numeric for a numeric input); percentile_disc/mode yield the
            // WITHIN GROUP input type.
            "percentile_cont" | "percentile_disc" | "mode" => {
                let input = order_by
                    .first()
                    .map(|o| infer_type_res(o.expression, columns))
                    .transpose()?
                    .map(|t| t.0);
                match *name {
                    "percentile_cont" if input == Some(oid::NUMERIC) => of(ColType::Numeric),
                    "percentile_cont" => of(ColType::Float8),
                    _ => match input.and_then(coltype_of_oid) {
                        Some(t) => of(t),
                        None => (oid::UNKNOWN, -2),
                    },
                }
            }
            "extract" => of(ColType::Numeric),
            "date_part" => of(ColType::Float8),
            // Paren-less temporal functions carry a proper type so date/time
            // arithmetic (e.g. `current_date - 1`) type-checks correctly.
            "to_date" => of(ColType::Date),
            "to_timestamp" => of(ColType::Timestamptz),
            "generate_series" => {
                let a = args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                if a == Some(oid::INT8) { of(ColType::Int8) } else { of(ColType::Int4) }
            }
            "unnest" => {
                // The element type of the array argument.
                match args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0) {
                    Some(o) => match coltype_of_oid(o) {
                        Some(ColType::Array(element)) => of(element.to_coltype()),
                        _ => of(ColType::Text),
                    },
                    None => of(ColType::Text),
                }
            }
            // regexp_matches returns each match's capture groups as text[].
            "regexp_matches" => of(ColType::Array(crate::sql::types::ArrElem::Text)),
            "regexp_split_to_table" | "string_to_table" => of(ColType::Text),
            "generate_subscripts" => of(ColType::Int4),
            "jsonb_object_keys" | "json_object_keys" | "jsonb_array_elements_text"
            | "json_array_elements_text" => of(ColType::Text),
            "jsonb_array_elements" => of(ColType::Jsonb),
            "json_array_elements" => of(ColType::Json),
            // The `each` family yields a `(key, value)` composite per member.
            "json_each" | "jsonb_each" | "json_each_text" | "jsonb_each_text" => {
                (oid::RECORD, -1)
            }
            "grouping" => of(ColType::Int4),
            "make_date" => of(ColType::Date),
            "make_time" => of(ColType::Time),
            "make_timestamp" => of(ColType::Timestamp),
            "make_timestamptz" => of(ColType::Timestamptz),
            "isfinite" => of(ColType::Bool),
            // Encoding / hashing / bytea manipulation.
            "sha224" | "sha256" | "sha384" | "sha512" | "decode" | "set_byte" | "set_bit"
            | "convert_to" => of(ColType::Bytea),
            "encode" | "convert_from" | "quote_ident" | "quote_literal" | "quote_nullable" => {
                of(ColType::Text)
            }
            "get_byte" | "get_bit" => of(ColType::Int4),
            crate::sql::parser::OVERLAPS_PERIODS => of(ColType::Bool),
            "bit_count" => of(ColType::Int8),
            "parse_ident" => of(ColType::Array(crate::sql::types::ArrElem::Text)),
            // date_bin returns the type of its source timestamp (arg 1).
            "date_bin" => {
                let src = args.get(1).map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                if src == Some(oid::TIMESTAMPTZ) {
                    of(ColType::Timestamptz)
                } else {
                    of(ColType::Timestamp)
                }
            }
            "age" | "justify_hours" | "justify_days" | "justify_interval" | "make_interval" => {
                of(ColType::Interval)
            }
            // timezone(zone, ts) == ts AT TIME ZONE zone: timestamptz <-> timestamp.
            "timezone" => {
                let arg = args.get(1).map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                match arg {
                    Some(oid::TIMESTAMPTZ) => of(ColType::Timestamp),
                    _ => of(ColType::Timestamptz),
                }
            }
            "int4range" | "int8range" | "numrange" | "daterange" | "tsrange" | "tstzrange" => {
                of(ColType::Range(crate::sql::types::RangeKind::from_name(name).expect("range name")))
            }
            "int4multirange" | "int8multirange" | "nummultirange" | "datemultirange"
            | "tsmultirange" | "tstzmultirange" => of(ColType::Multirange(
                crate::sql::types::RangeKind::from_multirange_name(name).expect("multirange name"),
            )),
            "similar_to" | "isempty" | "lower_inc" | "upper_inc" | "lower_inf" | "upper_inf" => of(ColType::Bool),
            "range_merge" => {
                // Same range type as its arguments.
                match args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0) {
                    Some(o) if is_range_oid(o) => (o, -1),
                    _ => (oid::TEXT, -1),
                }
            }
            "lower" | "upper" => {
                // A range argument yields its element type; otherwise text.
                match args.first().map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0) {
                    Some(o) => match coltype_of_oid(o) {
                        Some(ColType::Range(kind)) | Some(ColType::Multirange(kind)) => {
                            of(kind.elem_type())
                        }
                        _ => (oid::TEXT, -1),
                    },
                    None => (oid::TEXT, -1),
                }
            }
            "current_date" => of(ColType::Date),
            "current_time" => of(ColType::Timetz),
            "localtime" => of(ColType::Time),
            "localtimestamp" => of(ColType::Timestamp),
            "now" | "current_timestamp" | "transaction_timestamp" | "statement_timestamp"
            | "clock_timestamp" => of(ColType::Timestamptz),
            "date_trunc" => {
                // Returns the timestamp type of its second argument.
                let a = args.get(1).map(|a| infer_type_res(a, columns)).transpose()?.map(|t| t.0);
                if a == Some(oid::TIMESTAMPTZ) {
                    of(ColType::Timestamptz)
                } else {
                    of(ColType::Timestamp)
                }
            }
            // The remaining implemented functions (trim family, substr, replace,
            // repeat, reverse, left, right, concat[_ws], initcap, chr, ...) and
            // any not-yet-modeled function default to text.
            _ => (oid::TEXT, -1),
        },
    })
}
