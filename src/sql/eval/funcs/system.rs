//! System / introspection built-ins.
//!
//! Covers session identity (`version`, `current_database`/`current_catalog`,
//! `current_schema`/`current_schemas`, `current_user`/`session_user`/`user`,
//! `pg_get_userbyid`), the always-true visibility/privilege predicates, the
//! catalog-definition reconstructors (`pg_get_indexdef`/`pg_get_constraintdef`
//! and the not-reconstructed `pg_get_expr`/`pg_get_viewdef`/… → NULL),
//! partitioning identity (`pg_partition_ancestors`/`_root`/`_tree`),
//! `format_type`, `pg_encoding_to_char`, and `pg_typeof`.

use crate::sql::array;
use crate::sql::ast::Expr;
use crate::sql::exec;
use crate::sql::types::{ArrElem, Datum, TypeMod};
use crate::sql_err;
use crate::stack_format;

use super::super::{arena_full, eval_full, sqlstate, ColumnLookup, EvalHooks, SqlError};

/// Handles the system/introspection family. Returns `None` if `name` is not one
/// of these functions, leaving the router to keep matching.
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
        "version"
            | "current_database"
            | "current_catalog"
            | "current_schema"
            | "current_schemas"
            | "current_user"
            | "session_user"
            | "user"
            | "pg_get_userbyid"
            | "pg_partition_ancestors"
            | "pg_partition_root"
            | "pg_partition_tree"
            | "pg_table_is_visible"
            | "pg_type_is_visible"
            | "pg_function_is_visible"
            | "has_table_privilege"
            | "has_column_privilege"
            | "has_schema_privilege"
            | "pg_relation_is_publishable"
            | "pg_get_indexdef"
            | "pg_get_constraintdef"
            | "pg_get_expr"
            | "pg_get_viewdef"
            | "pg_get_functiondef"
            | "col_description"
            | "obj_description"
            | "shobj_description"
            | "pg_get_statisticsobjdef_columns"
            | "format_type"
            | "pg_encoding_to_char"
            | "pg_typeof"
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
            "version" => {
                arity(0)?;
                Ok(Datum::Text(concat!(
                    "PostgreSQL 18.4 (pos3ql ",
                    env!("CARGO_PKG_VERSION"),
                    ") on aarch64-apple-darwin"
                )))
            }
            "current_database" | "current_catalog" => {
                arity(0)?;
                Ok(Datum::Text("postgres"))
            }
            "current_schema" => {
                arity(0)?;
                Ok(Datum::Text("public"))
            }
            // `current_schemas(bool)` returns the search-path schemas as a text[];
            // with `true` it prepends the implicit pg_catalog.
            "current_schemas" => {
                arity(1)?;
                let include_implicit =
                    matches!(eval_full(args[0], arena, params, row, hooks)?, Datum::Bool(true));
                let elems: &[Datum] = if include_implicit {
                    &[Datum::Text("pg_catalog"), Datum::Text("public")]
                } else {
                    &[Datum::Text("public")]
                };
                Ok(Datum::Array {
                    element: ArrElem::Text,
                    raw: array::build(elems, arena)?,
                })
            }
            "current_user" | "session_user" | "user" => {
                arity(0)?;
                Ok(Datum::Text("pos3ql"))
            }
            // Catalog helpers for psql introspection. Every user object lives in the
            // single visible schema owned by the connection role.
            "pg_get_userbyid" => {
                arity(1)?;
                Ok(Datum::Text("pos3ql"))
            }
            // A non-partitioned table is its own only ancestor/root; we have no
            // partitioning, so these return the argument unchanged.
            "pg_partition_ancestors" | "pg_partition_root" | "pg_partition_tree" => {
                arity(1)?;
                eval_full(args[0], arena, params, row, hooks)
            }
            "pg_table_is_visible" | "pg_type_is_visible" | "pg_function_is_visible"
            | "has_table_privilege" | "has_column_privilege" | "has_schema_privilege"
            | "pg_relation_is_publishable" => {
                Ok(Datum::Bool(true))
            }
            "pg_get_indexdef" => {
                // `pg_get_indexdef(oid)` / `(oid, 0, _)` reconstruct the whole
                // `btree (columns)` definition; `(oid, n, _)` with n>0 returns the name
                // of the n-th (1-based) indexed column (used by JDBC getIndexInfo).
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Int4(v) => v,
                    Datum::Int8(v) => v as i32,
                    _ => return Ok(Datum::Null),
                };
                let col = if args.len() >= 2 {
                    match eval_full(args[1], arena, params, row, hooks)? {
                        Datum::Int4(v) => v.max(0) as usize,
                        Datum::Int8(v) => v.max(0) as usize,
                        _ => 0,
                    }
                } else {
                    0
                };
                Ok(cat.index_def(oid, col, arena)?.map(Datum::Text).unwrap_or(Datum::Null))
            }
            "pg_get_constraintdef" => {
                // psql `\d` calls this with a constraint OID; reconstruct a
                // foreign-key definition via the catalog resolver when present.
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Int4(v) => v,
                    Datum::Int8(v) => v as i32,
                    _ => return Ok(Datum::Null),
                };
                Ok(cat.constraint_def(oid, arena)?.map(Datum::Text).unwrap_or(Datum::Null))
            }
            "pg_get_expr" | "pg_get_viewdef"
            | "pg_get_functiondef" | "col_description" | "obj_description"
            | "shobj_description" | "pg_get_statisticsobjdef_columns" => {
                // Definitions/comments we do not reconstruct render as empty/NULL,
                // as PostgreSQL does for an absent comment.
                Ok(Datum::Null)
            }
            "format_type" => {
                arity(2)?;
                // format_type(typoid, typmod): map the common base-type oids back to
                // their SQL spelling; unknown oids render as "???".
                let o = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Int4(v) => v,
                    Datum::Int8(v) => v as i32,
                    Datum::Null => return Ok(Datum::Null),
                    _ => -1,
                };
                let type_mod = match eval_full(args[1], arena, params, row, hooks)? {
                    Datum::Int4(v) => v,
                    Datum::Int8(v) => v as i32,
                    // A NULL modifier means "no modifier", not a NULL result.
                    _ => -1,
                };
                let Some(coltype) = exec::coltype_of_oid(o) else {
                    return Ok(Datum::Text("???"));
                };
                let name = coltype.name();
                // The modifier is decoded once under the type's own encoding;
                // the arms render meanings, not integer arithmetic.
                let text = match TypeMod::decode(coltype, type_mod) {
                    TypeMod::None => return Ok(Datum::Text(name)),
                    TypeMod::Length(n) => stack_format!(64, "{}({})", name, n),
                    TypeMod::NumericPS { precision, scale } => {
                        stack_format!(64, "{}({},{})", name, precision, scale)
                    }
                    TypeMod::TemporalPrecision(p) => {
                        // The precision sits inside the name, before the
                        // time-zone tail — `timestamp(3) without time zone`. The
                        // split finds the tail for both spellings, since
                        // "without" begins "with".
                        match name.split_once(" with") {
                            Some((head, tail)) => {
                                stack_format!(64, "{}({}) with{}", head, p, tail)
                            }
                            None => stack_format!(64, "{}({})", name, p),
                        }
                    }
                    TypeMod::IntervalMod { precision: Some(p), .. } => {
                        stack_format!(64, "interval({})", p)
                    }
                    // A range form with no precision renders bare; naming the
                    // field range (`interval hour to minute`) is not built yet.
                    TypeMod::IntervalMod { precision: None, .. } => {
                        return Ok(Datum::Text(name))
                    }
                };
                Ok(Datum::Text(arena.alloc_str(text.as_str()).map_err(|_| arena_full())?))
            }
            "pg_encoding_to_char" => {
                arity(1)?;
                Ok(Datum::Text("UTF8"))
            }
            "pg_typeof" => {
                arity(1)?;
                let v = eval_full(args[0], arena, params, row, hooks)?;
                // PostgreSQL's pg_typeof reports the argument's *static* type —
                // `current_user` is `name` though the value is plain text. The
                // static answer is used whenever it is consistent with the
                // runtime value (same storage type, or NULL); an inconsistent
                // one — a mis-inferred set-returning function, say — falls
                // back to the type the value itself carries.
                if let Some(name) = exec::typeof_static(args[0], row) {
                    let consistent = v.is_null()
                        || exec::typeof_static_coltype(args[0], row)
                            .is_some_and(|ct| ct.storage().oid() == v.type_oid());
                    if consistent {
                        return Ok(Datum::Text(name));
                    }
                }
                Ok(Datum::Text(match v {
                    Datum::Null => "unknown",
                    Datum::Bool(_) => "boolean",
                    Datum::Int2(_) => "smallint",
                    Datum::Int4(_) => "integer",
                    Datum::Int8(_) => "bigint",
                    Datum::Float8(_) => "double precision",
                    Datum::Text(_) => "text",
                    Datum::Bpchar(_) => "character",
                    Datum::Date(_) => "date",
                    Datum::Timestamp(_) => "timestamp without time zone",
                    Datum::Timestamptz(_) => "timestamp with time zone",
                    Datum::Time(_) => "time without time zone",
                    Datum::Timetz(..) => "time with time zone",
                    Datum::Interval(_) => "interval",
                    Datum::Json { jsonb: false, .. } => "json",
                    Datum::Json { jsonb: true, .. } => "jsonb",
                    Datum::Array { element, .. } => element.typeof_name(),
                    Datum::Uuid(_) => "uuid",
                    Datum::Bytea(_) => "bytea",
                    Datum::Numeric(_) => "numeric",
                    Datum::Range { kind, .. } => kind.name(),
                    Datum::Bit { varying: false, .. } => "bit",
                    Datum::Bit { varying: true, .. } => "bit varying",
                    Datum::Multirange { kind, .. } => kind.multirange_name(),
                    Datum::Record(_) => "record",
                }))
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
