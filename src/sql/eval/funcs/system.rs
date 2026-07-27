//! System / introspection built-ins.
//!
//! Covers server identity/state (`version`, `pg_is_in_recovery`), session identity
//! (`current_database`/`current_catalog`,
//! `current_schema`/`current_schemas`, `current_user`/`session_user`/`user`,
//! `pg_get_userbyid`), the always-true visibility/privilege predicates, the
//! catalog-definition reconstructors (`pg_get_indexdef`/`pg_get_constraintdef`
//! and the not-reconstructed `pg_get_expr`/`pg_get_viewdef`/… → NULL),
//! partitioning identity (`pg_partition_ancestors`/`_root`/`_tree`),
//! `format_type`, `pg_encoding_to_char`, and `pg_typeof`.

use crate::sql::array;
use crate::sql::ast::Expr;
use crate::sql::exec;
use crate::sql::types::{ArrElem, ColType, Datum, TypeMod};
use crate::sql_err;
use crate::stack_format;

use super::super::{arena_full, eval_full, sqlstate, ColumnLookup, EvalHooks, SqlError};

std::thread_local! {
    /// The session's startup user, published per statement (like the session
    /// time zone) so `current_user` and friends reflect the connection.
    static SESSION_USER: core::cell::RefCell<crate::util::StackStr<64>> =
        core::cell::RefCell::new({
            let mut s = crate::util::StackStr::new();
            let _ = core::fmt::Write::write_str(&mut s, "postgres");
            s
        });
}

/// The effective search path's schema names, published per statement for
/// `current_schema`/`current_schemas`. `catalog_pos` is where the implicit or
/// explicit `pg_catalog` sits among them.
#[derive(Clone, Copy)]
pub struct SessionSchemas {
    pub names: [crate::util::StackStr<64>; 17],
    pub n: usize,
    pub catalog_pos: usize,
}

std::thread_local! {
    static SESSION_SCHEMAS: core::cell::RefCell<SessionSchemas> =
        const {
            core::cell::RefCell::new(SessionSchemas {
                names: [crate::util::StackStr::new(); 17],
                n: 0,
                catalog_pos: 0,
            })
        };
}

pub fn set_session_schemas(schemas: SessionSchemas) {
    SESSION_SCHEMAS.with(|s| *s.borrow_mut() = schemas);
}

fn session_schemas() -> SessionSchemas {
    SESSION_SCHEMAS.with(|s| *s.borrow())
}

/// Maximum number of readable settings published per statement for
/// `current_setting`. Comfortably covers the `SHOW ALL` set plus a margin.
pub const MAX_SESSION_SETTINGS: usize = 32;

/// A per-statement snapshot of the session's readable settings (name → value),
/// published like the session user so `current_setting` reads exactly what
/// `SHOW` would. Names are static; values are copied from the session GUC store.
#[derive(Clone, Copy)]
pub struct SessionSettings {
    pub names: [&'static str; MAX_SESSION_SETTINGS],
    pub values: [crate::util::StackStr<256>; MAX_SESSION_SETTINGS],
    pub n: usize,
}

std::thread_local! {
    static SESSION_SETTINGS: core::cell::RefCell<SessionSettings> = const {
        core::cell::RefCell::new(SessionSettings {
            names: [""; MAX_SESSION_SETTINGS],
            values: [crate::util::StackStr::new(); MAX_SESSION_SETTINGS],
            n: 0,
        })
    };
}

/// Publishes the readable settings for the statement about to evaluate. Each
/// pair is `(static name, current value)`; more than `MAX_SESSION_SETTINGS` is a
/// loud error, never silent truncation.
pub fn set_session_settings(
    names: &[&'static str],
    values: &[crate::util::StackStr<256>],
) -> Result<(), SqlError> {
    if names.len() != values.len() {
        return Err(sql_err!(sqlstate::INTERNAL_ERROR, "session setting snapshot is misaligned"));
    }
    if names.len() > MAX_SESSION_SETTINGS {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "too many session settings to publish ({} > {})",
            names.len(),
            MAX_SESSION_SETTINGS
        ));
    }
    SESSION_SETTINGS.with(|s| {
        let mut s = s.borrow_mut();
        s.names[..names.len()].copy_from_slice(names);
        s.values[..values.len()].copy_from_slice(values);
        s.n = names.len();
    });
    Ok(())
}

/// The published value of setting `name` (case-insensitive), if any.
fn session_setting(name: &str) -> Option<crate::util::StackStr<256>> {
    SESSION_SETTINGS.with(|s| {
        let s = s.borrow();
        (0..s.n)
            .find(|&i| s.names[i].eq_ignore_ascii_case(name))
            .map(|i| s.values[i])
    })
}

fn update_session_setting(name: &str, value: crate::util::StackStr<256>) {
    SESSION_SETTINGS.with(|settings| {
        let mut settings = settings.borrow_mut();
        if let Some(index) =
            (0..settings.n).find(|&index| settings.names[index].eq_ignore_ascii_case(name))
        {
            settings.values[index] = value;
        }
    });
}

pub fn set_session_user(user: &str) {
    SESSION_USER.with(|u| {
        let mut u = u.borrow_mut();
        *u = crate::util::StackStr::new();
        let _ = core::fmt::Write::write_str(&mut *u, user);
    });
}

pub fn session_user_owned() -> crate::util::StackStr<64> {
    SESSION_USER.with(|u| *u.borrow())
}

fn session_user_str(arena: &crate::mem::arena::Arena) -> Result<&str, SqlError> {
    let user = session_user_owned();
    arena.alloc_str(user.as_str()).map_err(|_| arena_full())
}

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
            | "pg_is_in_recovery"
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
            | "pg_collation_is_visible"
            | "has_table_privilege"
            | "has_column_privilege"
            | "has_schema_privilege"
            | "has_database_privilege"
            | "pg_relation_is_publishable"
            | "pg_get_indexdef"
            | "pg_get_constraintdef"
            | "pg_get_expr"
            | "pg_get_viewdef"
            | "pg_table_size"
            | "pg_database_size"
            | "pg_tablespace_location"
            | "pg_tablespace_size"
            | "pg_get_functiondef"
            | "col_description"
            | "obj_description"
            | "shobj_description"
            | "pg_get_statisticsobjdef_columns"
            | "format_type"
            | "pg_encoding_to_char"
            | "pg_char_to_encoding"
            | "getdatabaseencoding"
            | "pg_typeof"
            | "current_setting"
            | "set_config"
            | "acldefault"
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
            // pos3ql is a single-primary server and has no recovery/standby mode.
            "pg_is_in_recovery" => {
                arity(0)?;
                Ok(Datum::Bool(false))
            }
            // There are no grantable ACLs yet. Catalog rows therefore have the
            // same empty default ACL for every supported object kind.
            "acldefault" => {
                arity(2)?;
                Ok(Datum::Text("{}"))
            }
            "current_database" | "current_catalog" => {
                arity(0)?;
                Ok(Datum::Text("postgres"))
            }
            // `current_setting(name [, missing_ok])` returns the setting's value
            // as text — the same value `SHOW name` reports. An unknown setting
            // errors `42704`, unless `missing_ok` is true, when it returns NULL.
            "current_setting" => {
                if args.len() != 1 && args.len() != 2 || star {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function current_setting(...) with {} arguments does not exist",
                        if star { 1 } else { args.len() }
                    ));
                }
                let name_value = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Text(t) => t,
                    Datum::Null => return Ok(Datum::Null),
                    _ => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "current_setting() requires a text setting name"
                        ))
                    }
                };
                let missing_ok = args.len() == 2
                    && matches!(eval_full(args[1], arena, params, row, hooks)?, Datum::Bool(true));
                match session_setting(name_value) {
                    Some(v) => Ok(Datum::Text(
                        arena.alloc_str(v.as_str()).map_err(|_| arena_full())?,
                    )),
                    None if missing_ok => Ok(Datum::Null),
                    None => Err(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "unrecognized configuration parameter \"{}\"",
                        name_value
                    )),
                }
            }
            "set_config" => {
                if args.len() != 3 || star {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function set_config(...) with {} arguments does not exist",
                        if star { 1 } else { args.len() }
                    ));
                }
                let name = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Text(value) => value,
                    Datum::Null => {
                        return Err(sql_err!(
                            sqlstate::NULL_VALUE_NOT_ALLOWED,
                            "SET requires parameter name"
                        ))
                    }
                    _ => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "set_config() requires a text setting name"
                        ))
                    }
                };
                let value = match eval_full(args[1], arena, params, row, hooks)? {
                    Datum::Text(value) => Some(value),
                    Datum::Null => None,
                    _ => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "set_config() requires a text setting value"
                        ))
                    }
                };
                let local = match eval_full(args[2], arena, params, row, hooks)? {
                    Datum::Bool(value) => value,
                    Datum::Null => false,
                    _ => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "set_config() requires a boolean is_local argument"
                        ))
                    }
                };
                let configured = crate::sql::guc::set_active_config(name, value, local)?;
                update_session_setting(name, configured);
                Ok(Datum::Text(
                    arena.alloc_str(configured.as_str()).map_err(|_| arena_full())?,
                ))
            }
            "current_schema" => {
                arity(0)?;
                let schemas = session_schemas();
                if schemas.n == 0 {
                    return Ok(Datum::Null);
                }
                Ok(Datum::Text(
                    arena.alloc_str(schemas.names[0].as_str()).map_err(|_| arena_full())?,
                ))
            }
            // `current_schemas(bool)` returns the search-path schemas as a text[];
            // with `true` it includes pg_catalog at its (implicit or explicit)
            // position.
            "current_schemas" => {
                arity(1)?;
                let include_implicit =
                    matches!(eval_full(args[0], arena, params, row, hooks)?, Datum::Bool(true));
                let schemas = session_schemas();
                let mut elems = [Datum::Null; 18];
                let mut n = 0;
                for (i, name) in schemas.names[..schemas.n].iter().enumerate() {
                    if include_implicit && i == schemas.catalog_pos {
                        elems[n] = Datum::Text("pg_catalog");
                        n += 1;
                    }
                    elems[n] =
                        Datum::Text(arena.alloc_str(name.as_str()).map_err(|_| arena_full())?);
                    n += 1;
                }
                if include_implicit
                    && schemas.catalog_pos != usize::MAX
                    && schemas.catalog_pos >= schemas.n
                {
                    elems[n] = Datum::Text("pg_catalog");
                    n += 1;
                }
                Ok(Datum::Array {
                    element: ArrElem::Text,
                    raw: array::build(&elems[..n], arena)?,
                })
            }
            "current_user" | "session_user" | "user" => {
                arity(0)?;
                Ok(Datum::Text(session_user_str(arena)?))
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
            | "pg_collation_is_visible"
            | "has_table_privilege" | "has_column_privilege" | "has_schema_privilege"
            | "has_database_privilege" | "pg_relation_is_publishable" => {
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
            "obj_description" => {
                // obj_description(objoid [, catalog]): the object's comment.
                // The optional catalog selects the object's catalog class. The
                // deprecated one-argument form keeps the historical pg_class
                // default used by clients.
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Int4(v) => v,
                    Datum::Int8(v) => v as i32,
                    _ => return Ok(Datum::Null),
                };
                let catalog_name = if args.len() >= 2 {
                    match eval_full(args[1], arena, params, row, hooks)? {
                        Datum::Text(name) => name,
                        _ => return Ok(Datum::Null),
                    }
                } else {
                    "pg_class"
                };
                Ok(cat
                    .comment(catalog_name, oid, 0, arena)?
                    .map(Datum::Text)
                    .unwrap_or(Datum::Null))
            }
            "col_description" => {
                // col_description(objoid, objsubid): a column's comment.
                arity(2)?;
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Int4(v) => v,
                    Datum::Int8(v) => v as i32,
                    _ => return Ok(Datum::Null),
                };
                let col = match eval_full(args[1], arena, params, row, hooks)? {
                    Datum::Int4(v) => v,
                    Datum::Int8(v) => v as i32,
                    _ => return Ok(Datum::Null),
                };
                Ok(cat
                    .comment("pg_class", oid, col, arena)?
                    .map(Datum::Text)
                    .unwrap_or(Datum::Null))
            }
            "pg_get_expr" => {
                if !(2..=3).contains(&args.len()) {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function pg_get_expr(...) does not exist"
                    ));
                }
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Text(expression) => Ok(Datum::Text(expression)),
                    Datum::Null => Ok(Datum::Null),
                    _ => Ok(Datum::Null),
                }
            }
            "pg_get_viewdef" => {
                if !(1..=2).contains(&args.len()) {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function pg_get_viewdef(...) does not exist"
                    ));
                }
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Int4(oid) => oid,
                    Datum::Int8(oid) => oid as i32,
                    Datum::Null => return Ok(Datum::Null),
                    _ => return Ok(Datum::Null),
                };
                Ok(cat
                    .view_def(oid, arena)?
                    .map(Datum::Text)
                    .unwrap_or(Datum::Null))
            }
            "pg_table_size" => {
                arity(1)?;
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Int4(oid) => oid,
                    Datum::Int8(oid) => oid as i32,
                    Datum::Null => return Ok(Datum::Null),
                    _ => return Ok(Datum::Null),
                };
                Ok(cat
                    .relation_size(oid)?
                    .map(Datum::Int8)
                    .unwrap_or(Datum::Null))
            }
            "pg_database_size" => {
                arity(1)?;
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Text(_) | Datum::Int4(_) | Datum::Int8(_) => {
                        Ok(Datum::Int8(cat.database_size()?))
                    }
                    Datum::Null => Ok(Datum::Null),
                    _ => Ok(Datum::Null),
                }
            }
            "pg_tablespace_location" => {
                arity(1)?;
                let _ = eval_full(args[0], arena, params, row, hooks)?;
                Ok(Datum::Text(""))
            }
            "pg_tablespace_size" => {
                arity(1)?;
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Int4(_) | Datum::Int8(_) | Datum::Text(_) => {
                        Ok(Datum::Int8(cat.database_size()?))
                    }
                    Datum::Null => Ok(Datum::Null),
                    _ => Ok(Datum::Null),
                }
            }
            "pg_get_functiondef"
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
                if let Some(cat) = hooks.catalog
                    && let Some(name) = cat.type_name(o, arena)?
                {
                    return Ok(Datum::Text(name));
                }
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
            "pg_char_to_encoding" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Text(name) if name.eq_ignore_ascii_case("UTF8") => Ok(Datum::Int4(6)),
                    Datum::Text(_) => Ok(Datum::Int4(-1)),
                    Datum::Null => Ok(Datum::Null),
                    _ => Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "pg_char_to_encoding() requires a text encoding name"
                    )),
                }
            }
            "getdatabaseencoding" => {
                arity(0)?;
                Ok(Datum::Text("UTF8"))
            }
            "pg_typeof" => {
                arity(1)?;
                // A bare column of a domain type reports the domain name (an
                // expression over it un-domains to the base, handled below).
                if let crate::sql::ast::Expr::Column { qualifier, name } = args[0]
                    && let Some(dname) = row.column_domain(*qualifier, name)
                {
                    if let Some(ColType::Array(element)) = row.col_type(*qualifier, name)
                        && element.user_type_slot().is_some()
                        && let Some(cat) = hooks.catalog
                        && let Some(name) = cat.user_array_name(element, arena)?
                    {
                        return Ok(Datum::Text(name));
                    }
                    return Ok(Datum::Text(
                        arena.alloc_str(dname.as_str()).map_err(|_| arena_full())?,
                    ));
                }
                let v = eval_full(args[0], arena, params, row, hooks)?;
                if let crate::sql::ast::Expr::Cast { type_name, .. } = args[0]
                    && let Some(cat) = hooks.catalog
                    && let Some(name) = cat.user_type_name(type_name, arena)?
                {
                    return Ok(Datum::Text(name));
                }
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
                // An enum value reports its type name, resolved from the slot.
                if let Datum::Enum { slot, .. } = v
                    && let Some(cat) = hooks.catalog
                    && let Some(name) = cat.enum_name(slot, arena)?
                {
                    return Ok(Datum::Text(name));
                }
                if let Datum::Array { element, .. } = v
                    && element.user_type_slot().is_some()
                    && let Some(cat) = hooks.catalog
                    && let Some(name) = cat.user_array_name(element, arena)?
                {
                    return Ok(Datum::Text(name));
                }
                Ok(Datum::Text(match v {
                    Datum::Null => "unknown",
                    Datum::Bool(_) => "boolean",
                    Datum::Int2(_) => "smallint",
                    Datum::Int4(_) => "integer",
                    Datum::Int8(_) => "bigint",
                    Datum::Float4(_) => "real",
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
                    Datum::Inet(_) => "inet",
                    Datum::Cidr(_) => "cidr",
                    Datum::Macaddr(_) => "macaddr",
                    Datum::Macaddr8(_) => "macaddr8",
                    Datum::Record(_) => "record",
                    Datum::Enum { .. } => "enum",
                }))
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
