//! System / introspection built-ins.
//!
//! Covers server identity/state (`version`, `pg_is_in_recovery`), session identity
//! (`current_database`/`current_catalog`,
//! `current_schema`/`current_schemas`, `current_user`/`session_user`/`user`,
//! `pg_get_userbyid`), visibility and privilege predicates, the
//! catalog-definition reconstructors (`pg_get_indexdef`/`pg_get_constraintdef`,
//! `pg_get_expr`, and `pg_get_viewdef`),
//! partitioning identity (`pg_partition_ancestors`/`_root`/`_tree`),
//! `format_type`, `pg_encoding_to_char`, and `pg_typeof`.

use crate::sql::array;
use crate::sql::ast::Expr;
use crate::sql::exec;
use crate::sql::types::{ArrElem, ColType, Datum, TypeMod};
use crate::sql_err;
use crate::stack_format;

use super::super::{
    ColumnLookup, EvalHooks, SqlError, arena_full, eval_full, sqlstate, type_mismatch,
};

/// A catalog OID accepted by identity predicates.  Keeping conversion at the
/// boundary prevents a wide integer from wrapping into an unrelated object.
#[derive(Clone, Copy)]
struct CatalogOid(i32);

impl CatalogOid {
    /// Parse the SQL identity boundary once: wide SQL integers and unsigned
    /// wire OIDs must not wrap into a different internal catalog object.
    fn parse(value: Datum<'_>) -> Result<Option<Self>, SqlError> {
        match value {
            Datum::Null => Ok(None),
            Datum::Int4(oid) => Ok(Some(Self(oid))),
            Datum::Oid(oid) => i32::try_from(oid)
                .map(Self)
                .map(Some)
                .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "OID out of range")),
            Datum::Int8(oid) => i32::try_from(oid)
                .map(Self)
                .map(Some)
                .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "OID out of range")),
            Datum::RegObject { referenced_oid, .. } => Ok(Some(Self(referenced_oid))),
            // PostgreSQL resolves an untyped string literal against the OID
            // argument in the intrinsic signature before the function runs.
            // String literals are represented as text in the AST, so perform
            // that parse at this identity boundary and carry only CatalogOid
            // afterward.
            Datum::Text(raw) => raw.parse::<i32>().map(Self).map(Some).map_err(|_| {
                sql_err!(
                    sqlstate::INVALID_TEXT_REPRESENTATION,
                    "invalid input syntax for type oid: \"{}\"",
                    raw
                )
            }),
            _ => Ok(None),
        }
    }
}

#[derive(Clone, Copy)]
struct ObjectOid(u32);

impl ObjectOid {
    fn parse(value: Datum<'_>) -> Result<Option<Self>, SqlError> {
        match value {
            Datum::Null => Ok(None),
            Datum::Oid(oid) => Ok(Some(Self(oid))),
            Datum::Regtype { referenced_oid, .. } => u32::try_from(referenced_oid)
                .map(Self)
                .map(Some)
                .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "OID must be nonnegative")),
            Datum::Int4(oid) => u32::try_from(oid)
                .map(Self)
                .map(Some)
                .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "OID out of range")),
            Datum::Int8(oid) => u32::try_from(oid)
                .map(Self)
                .map(Some)
                .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "OID out of range")),
            Datum::RegObject { referenced_oid, .. } => u32::try_from(referenced_oid)
                .map(Self)
                .map(Some)
                .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "OID out of range")),
            Datum::Text(raw) => raw.parse::<u32>().map(Self).map(Some).map_err(|_| {
                sql_err!(
                    sqlstate::INVALID_TEXT_REPRESENTATION,
                    "invalid input syntax for type oid: \"{}\"",
                    raw
                )
            }),
            _ => Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "object identity must be oid"
            )),
        }
    }
}

/// PostgreSQL's closed one-byte object-class protocol for `acldefault`.
#[derive(Clone, Copy)]
enum AclDefaultObject {
    Column,
    Database,
    ForeignDataWrapper,
    ForeignServer,
    Function,
    Language,
    LargeObject,
    Parameter,
    Relation,
    Schema,
    Sequence,
    Tablespace,
    Type,
}

impl AclDefaultObject {
    fn parse(value: &str) -> Option<Self> {
        Some(match value.as_bytes() {
            b"c" => Self::Column,
            b"d" => Self::Database,
            b"F" => Self::ForeignDataWrapper,
            b"S" => Self::ForeignServer,
            b"f" => Self::Function,
            b"l" => Self::Language,
            b"L" => Self::LargeObject,
            b"p" => Self::Parameter,
            b"r" => Self::Relation,
            b"n" => Self::Schema,
            b"s" => Self::Sequence,
            b"t" => Self::Tablespace,
            b"T" => Self::Type,
            _ => return None,
        })
    }

    /// Owner privileges followed by PUBLIC privileges. PostgreSQL orders the
    /// PUBLIC aclitem first when both exist.
    fn privileges(self) -> (Option<&'static str>, Option<&'static str>) {
        match self {
            Self::Column => (None, None),
            Self::Database => (Some("CTc"), Some("Tc")),
            Self::ForeignDataWrapper | Self::ForeignServer => (Some("U"), None),
            Self::Function => (Some("X"), Some("X")),
            Self::Language | Self::Type => (Some("U"), Some("U")),
            Self::LargeObject => (Some("rw"), None),
            Self::Parameter => (Some("sA"), None),
            Self::Relation => (Some("arwdDxtm"), None),
            Self::Schema => (Some("UC"), None),
            Self::Sequence => (Some("rwU"), None),
            Self::Tablespace => (Some("C"), None),
        }
    }
}

fn privilege_role_name<'a>(
    value: Datum<'a>,
    catalog: &dyn super::super::CatalogAccess,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Option<&'a str>, SqlError> {
    match value {
        Datum::Text(role) => Ok(Some(role)),
        value @ (Datum::Int4(_) | Datum::Oid(_) | Datum::Int8(_) | Datum::RegObject { .. }) => {
            let oid = CatalogOid::parse(value)?.expect("identity datum is non-null");
            catalog.role_name(oid.0, arena)
        }
        Datum::Null => Ok(None),
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "role must be specified by name or oid"
        )),
    }
}

fn privilege_object_name<'a>(
    value: Datum<'a>,
    function: &str,
    catalog: &dyn super::super::CatalogAccess,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Option<&'a str>, SqlError> {
    match value {
        Datum::Text(object) => Ok(Some(object)),
        value @ (Datum::Int4(_) | Datum::Oid(_) | Datum::Int8(_) | Datum::RegObject { .. }) => {
            let oid = CatalogOid::parse(value)?
                .expect("identity datum is non-null")
                .0;
            if function == "has_type_privilege" {
                catalog.type_name(oid, arena)
            } else if function == "has_language_privilege" {
                catalog.language_name(oid, arena)
            } else if function == "has_schema_privilege" {
                catalog.schema_name(oid, arena)
            } else if function == "has_database_privilege" {
                catalog.database_name(oid, arena)
            } else if function == "has_tablespace_privilege" {
                catalog.tablespace_name(oid, arena)
            } else {
                catalog.relname(oid, arena)
            }
        }
        Datum::Null => Ok(None),
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "object must be specified by name or oid"
        )),
    }
}

std::thread_local! {
    /// The session's startup user, published per statement (like the session
    /// time zone) so `current_user` and friends reflect the connection.
    static SESSION_USER: core::cell::RefCell<crate::util::StackStr<64>> =
        core::cell::RefCell::new({
            let mut s = crate::util::StackStr::new();
            let _ = core::fmt::Write::write_str(&mut s, "postgres");
            s
        });
    static CURRENT_USER: core::cell::RefCell<crate::util::StackStr<64>> =
        core::cell::RefCell::new({
            let mut s = crate::util::StackStr::new();
            let _ = core::fmt::Write::write_str(&mut s, "postgres");
            s
        });
    static CURRENT_DATABASE: core::cell::RefCell<crate::util::StackStr<64>> =
        core::cell::RefCell::new({
            let mut s = crate::util::StackStr::new();
            let _ = core::fmt::Write::write_str(&mut s, "postgres");
            s
        });
    static CONFIGURATION_RELOAD_REQUESTED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    static TABLE_REWRITE_CONTEXT: core::cell::Cell<Option<TableRewriteContext>> = const { core::cell::Cell::new(None) };
}

#[derive(Clone, Copy)]
pub(crate) struct TableRewriteContext {
    pub relation_oid: i32,
    pub reason: i32,
}

pub(crate) struct TableRewriteScope(Option<TableRewriteContext>);

impl Drop for TableRewriteScope {
    fn drop(&mut self) {
        TABLE_REWRITE_CONTEXT.with(|context| context.set(self.0));
    }
}

pub(crate) fn enter_table_rewrite_context(context: TableRewriteContext) -> TableRewriteScope {
    TableRewriteScope(TABLE_REWRITE_CONTEXT.with(|active| active.replace(Some(context))))
}

fn table_rewrite_context(function: &str) -> Result<TableRewriteContext, SqlError> {
    TABLE_REWRITE_CONTEXT.with(|context| {
        context.get().ok_or_else(|| {
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "{}() can only be called in a table_rewrite event trigger function",
                function
            )
        })
    })
}

pub(crate) fn clear_configuration_reload_request() {
    CONFIGURATION_RELOAD_REQUESTED.with(|requested| requested.set(false));
}

pub(crate) fn take_configuration_reload_request() -> bool {
    CONFIGURATION_RELOAD_REQUESTED.with(core::cell::Cell::take)
}

pub fn set_current_database(database: &str) {
    CURRENT_DATABASE.with(|current| {
        *current.borrow_mut() = crate::util::StackStr::from_str(database);
    });
}

fn current_database_str(arena: &crate::mem::arena::Arena) -> Result<&str, SqlError> {
    CURRENT_DATABASE.with(|current| {
        let current = *current.borrow();
        arena.alloc_str(current.as_str()).map_err(|_| arena_full())
    })
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
pub const MAX_SESSION_SETTINGS: usize = 64;

/// A per-statement snapshot of the session's readable settings (name → value),
/// published like the session user so `current_setting` reads exactly what
/// `SHOW` would. Names are static; values are copied from the session GUC store.
#[derive(Clone, Copy)]
pub struct SessionSettings {
    pub names: [&'static str; MAX_SESSION_SETTINGS],
    pub values: [crate::util::StackStr<256>; MAX_SESSION_SETTINGS],
    pub reset_values: [crate::util::StackStr<256>; MAX_SESSION_SETTINGS],
    pub sources: [&'static str; MAX_SESSION_SETTINGS],
    pub n: usize,
}

std::thread_local! {
    static SESSION_SETTINGS: core::cell::RefCell<SessionSettings> = const {
        core::cell::RefCell::new(SessionSettings {
            names: [""; MAX_SESSION_SETTINGS],
            values: [crate::util::StackStr::new(); MAX_SESSION_SETTINGS],
            reset_values: [crate::util::StackStr::new(); MAX_SESSION_SETTINGS],
            sources: ["default"; MAX_SESSION_SETTINGS],
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
    reset_values: &[crate::util::StackStr<256>],
    sources: &[&'static str],
) -> Result<(), SqlError> {
    if names.len() != values.len()
        || names.len() != reset_values.len()
        || names.len() != sources.len()
    {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "session setting snapshot is misaligned"
        ));
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
        s.reset_values[..reset_values.len()].copy_from_slice(reset_values);
        s.sources[..sources.len()].copy_from_slice(sources);
        s.n = names.len();
    });
    Ok(())
}

pub(crate) fn session_setting_metadata(
    name: &str,
) -> Option<(crate::util::StackStr<256>, &'static str)> {
    SESSION_SETTINGS.with(|settings| {
        let settings = settings.borrow();
        (0..settings.n)
            .find(|&index| settings.names[index].eq_ignore_ascii_case(name))
            .map(|index| (settings.reset_values[index], settings.sources[index]))
    })
}

/// The published value of setting `name` (case-insensitive), if any.
pub(crate) fn session_setting(name: &str) -> Option<crate::util::StackStr<256>> {
    SESSION_SETTINGS.with(|s| {
        let s = s.borrow();
        (0..s.n)
            .find(|&i| s.names[i].eq_ignore_ascii_case(name))
            .map(|i| s.values[i])
    })
}

pub(crate) fn update_session_setting(
    name: &str,
    value: crate::util::StackStr<256>,
    reset_value: crate::util::StackStr<256>,
    source: &'static str,
) {
    SESSION_SETTINGS.with(|settings| {
        let mut settings = settings.borrow_mut();
        if let Some(index) =
            (0..settings.n).find(|&index| settings.names[index].eq_ignore_ascii_case(name))
        {
            settings.values[index] = value;
            settings.reset_values[index] = reset_value;
            settings.sources[index] = source;
        }
    });
}

pub fn set_session_user(user: &str) {
    SESSION_USER.with(|u| {
        let mut u = u.borrow_mut();
        *u = crate::util::StackStr::new();
        let _ = core::fmt::Write::write_str(&mut *u, user);
    });
    CURRENT_USER.with(|current| {
        let mut current = current.borrow_mut();
        *current = crate::util::StackStr::from_str(user);
    });
}

pub fn set_current_user(user: &str) {
    CURRENT_USER.with(|current| {
        *current.borrow_mut() = crate::util::StackStr::from_str(user);
    });
}

pub(crate) struct CurrentUserScope {
    prior: crate::util::StackStr<64>,
}

impl Drop for CurrentUserScope {
    fn drop(&mut self) {
        set_current_user(self.prior.as_str());
    }
}

pub(crate) fn enter_current_user(user: &str) -> CurrentUserScope {
    let prior = current_user_owned();
    set_current_user(user);
    CurrentUserScope { prior }
}

pub fn session_user_owned() -> crate::util::StackStr<64> {
    SESSION_USER.with(|u| *u.borrow())
}

pub fn current_user_owned() -> crate::util::StackStr<64> {
    CURRENT_USER.with(|user| *user.borrow())
}

fn session_user_str(arena: &crate::mem::arena::Arena) -> Result<&str, SqlError> {
    let user = session_user_owned();
    arena.alloc_str(user.as_str()).map_err(|_| arena_full())
}

fn current_user_str(arena: &crate::mem::arena::Arena) -> Result<&str, SqlError> {
    let user = current_user_owned();
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
            | "pg_reload_conf"
            | "pg_event_trigger_table_rewrite_oid"
            | "pg_event_trigger_table_rewrite_reason"
            | "pg_extension_config_dump"
            | "current_database"
            | "current_catalog"
            | "current_schema"
            | "current_schemas"
            | "current_user"
            | "current_role"
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
            | "has_any_column_privilege"
            | "has_sequence_privilege"
            | "has_schema_privilege"
            | "has_type_privilege"
            | "has_language_privilege"
            | "has_function_privilege"
            | "has_database_privilege"
            | "has_tablespace_privilege"
            | "has_parameter_privilege"
            | "pg_relation_is_publishable"
            | "pg_get_indexdef"
            | "pg_get_constraintdef"
            | "pg_get_partkeydef"
            | "pg_get_functiondef"
            | "pg_get_triggerdef"
            | "pg_get_function_arguments"
            | "pg_get_function_identity_arguments"
            | "pg_get_function_result"
            | "pg_get_function_sqlbody"
            | "pg_get_expr"
            | "pg_get_viewdef"
            | "pg_get_ruledef"
            | "pg_table_size"
            | "pg_database_size"
            | "pg_tablespace_location"
            | "pg_tablespace_size"
            | "col_description"
            | "obj_description"
            | "shobj_description"
            | "pg_get_statisticsobjdef"
            | "pg_get_statisticsobjdef_expressions"
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
            "pg_reload_conf" => {
                arity(0)?;
                CONFIGURATION_RELOAD_REQUESTED.with(|requested| requested.set(true));
                Ok(Datum::Bool(true))
            }
            "pg_event_trigger_table_rewrite_oid" => {
                arity(0)?;
                let context = table_rewrite_context(name)?;
                let oid = u32::try_from(context.relation_oid).map_err(|_| {
                    sql_err!(sqlstate::INTERNAL_ERROR, "table rewrite OID is invalid")
                })?;
                Ok(Datum::Oid(oid))
            }
            "pg_event_trigger_table_rewrite_reason" => {
                arity(0)?;
                Ok(Datum::Int4(table_rewrite_context(name)?.reason))
            }
            "pg_extension_config_dump" => {
                arity(2)?;
                Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "pg_extension_config_dump() can only be called from an SQL script executed by CREATE EXTENSION"
                ))
            }
            "acldefault" => {
                arity(2)?;
                let object_type = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Text(value) => value,
                    Datum::Null => return Ok(Datum::Null),
                    _ => {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_FUNCTION,
                            "function acldefault(...) with these argument types does not exist"
                        ));
                    }
                };
                let owner_oid = match eval_full(args[1], arena, params, row, hooks)? {
                    Datum::Oid(value) => i32::try_from(value).map_err(|_| {
                        sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "role OID is out of range")
                    })?,
                    Datum::Int4(value) => value,
                    Datum::Int8(value) => i32::try_from(value).map_err(|_| {
                        sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "role OID is out of range")
                    })?,
                    Datum::Null => return Ok(Datum::Null),
                    _ => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "acldefault() requires a role OID"
                        ));
                    }
                };
                let Some(catalog) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let Some(owner) = catalog.role_name(owner_oid, arena)? else {
                    return Ok(Datum::Null);
                };
                let object = AclDefaultObject::parse(object_type).ok_or_else(|| {
                    sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "unrecognized object type abbreviation: {}",
                        object_type
                    )
                })?;
                let owner = crate::sql::types::acl_identifier(owner);
                let (owner_privileges, public_privileges) = object.privileges();
                let mut values = [Datum::Null; 2];
                let mut count = 0;
                if let Some(privileges) = public_privileges {
                    let acl = stack_format!(256, "={}/{}", privileges, owner.as_str());
                    values[count] =
                        Datum::Text(arena.alloc_str(acl.as_str()).map_err(|_| arena_full())?);
                    count += 1;
                }
                if let Some(privileges) = owner_privileges {
                    let acl =
                        stack_format!(256, "{}={}/{}", owner.as_str(), privileges, owner.as_str());
                    values[count] =
                        Datum::Text(arena.alloc_str(acl.as_str()).map_err(|_| arena_full())?);
                    count += 1;
                }
                Ok(Datum::Array {
                    element: crate::sql::types::ArrElem::AclItem,
                    raw: crate::sql::array::build(&values[..count], arena)?,
                })
            }
            "current_database" | "current_catalog" => {
                arity(0)?;
                Ok(Datum::Text(current_database_str(arena)?))
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
                        ));
                    }
                };
                let missing_ok = args.len() == 2
                    && matches!(
                        eval_full(args[1], arena, params, row, hooks)?,
                        Datum::Bool(true)
                    );
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
                        ));
                    }
                    _ => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "set_config() requires a text setting name"
                        ));
                    }
                };
                let value = match eval_full(args[1], arena, params, row, hooks)? {
                    Datum::Text(value) => Some(value),
                    Datum::Null => None,
                    _ => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "set_config() requires a text setting value"
                        ));
                    }
                };
                let local = match eval_full(args[2], arena, params, row, hooks)? {
                    Datum::Bool(value) => value,
                    Datum::Null => false,
                    _ => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "set_config() requires a boolean is_local argument"
                        ));
                    }
                };
                let configured = crate::sql::guc::set_active_config(name, value, local)?;
                Ok(Datum::Text(
                    arena
                        .alloc_str(configured.as_str())
                        .map_err(|_| arena_full())?,
                ))
            }
            "current_schema" => {
                arity(0)?;
                let schemas = session_schemas();
                if schemas.n == 0 {
                    return Ok(Datum::Null);
                }
                Ok(Datum::Text(
                    arena
                        .alloc_str(schemas.names[0].as_str())
                        .map_err(|_| arena_full())?,
                ))
            }
            // `current_schemas(bool)` returns the search-path schemas as a text[];
            // with `true` it includes pg_catalog at its (implicit or explicit)
            // position.
            "current_schemas" => {
                arity(1)?;
                let include_implicit = matches!(
                    eval_full(args[0], arena, params, row, hooks)?,
                    Datum::Bool(true)
                );
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
            "session_user" => {
                arity(0)?;
                Ok(Datum::Text(session_user_str(arena)?))
            }
            "current_user" | "current_role" | "user" => {
                arity(0)?;
                Ok(Datum::Text(current_user_str(arena)?))
            }
            // Catalog helpers for psql introspection. Every user object lives in the
            // single visible schema owned by the connection role.
            "pg_get_userbyid" => {
                arity(1)?;
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "pg_get_userbyid() requires an oid"
                        ));
                    }
                };
                Ok(cat
                    .role_name(oid, arena)?
                    .map(Datum::Text)
                    .unwrap_or(Datum::Null))
            }
            // A non-partitioned table is its own only ancestor/root; we have no
            // partitioning, so these return the argument unchanged.
            "pg_partition_ancestors" | "pg_partition_root" | "pg_partition_tree" => {
                arity(1)?;
                eval_full(args[0], arena, params, row, hooks)
            }
            "has_table_privilege"
            | "has_column_privilege"
            | "has_any_column_privilege"
            | "has_sequence_privilege"
            | "has_schema_privilege"
            | "has_type_privilege"
            | "has_language_privilege"
            | "has_function_privilege"
            | "has_database_privilege"
            | "has_tablespace_privilege"
            | "has_parameter_privilege" => {
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let column = name == "has_column_privilege";
                let minimum = if column { 3 } else { 2 };
                let maximum = minimum + 1;
                if !(minimum..=maximum).contains(&args.len()) {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function {}(...) with {} arguments does not exist",
                        name,
                        args.len()
                    ));
                }
                let explicit_role = args.len() == maximum;
                let mut at = 0usize;
                let role = if explicit_role {
                    let value = eval_full(args[at], arena, params, row, hooks)?;
                    at += 1;
                    match privilege_role_name(value, cat, arena)? {
                        Some(role) => Some(role),
                        None => return Ok(Datum::Null),
                    }
                } else {
                    None
                };
                let object = eval_full(args[at], arena, params, row, hooks)?;
                at += 1;
                let object = if name == "has_parameter_privilege" {
                    match object {
                        Datum::Text(parameter) => parameter,
                        Datum::Null => return Ok(Datum::Null),
                        _ => {
                            return Err(sql_err!(
                                sqlstate::DATATYPE_MISMATCH,
                                "has_parameter_privilege() requires a text parameter name"
                            ));
                        }
                    }
                } else {
                    match privilege_object_name(object, name, cat, arena)? {
                        Some(object) => object,
                        None => return Ok(Datum::Null),
                    }
                };
                let privilege_column = if column {
                    let column = eval_full(args[at], arena, params, row, hooks)?;
                    at += 1;
                    Some(match column {
                        Datum::Text(name) => super::super::PrivilegeColumn::Name(name),
                        Datum::Int2(number) => super::super::PrivilegeColumn::Number(number),
                        Datum::Null => return Ok(Datum::Null),
                        _ => {
                            return Err(sql_err!(
                                sqlstate::UNDEFINED_FUNCTION,
                                "function has_column_privilege(...) does not exist"
                            ));
                        }
                    })
                } else {
                    None
                };
                let privilege = match eval_full(args[at], arena, params, row, hooks)? {
                    Datum::Text(privilege) => privilege,
                    Datum::Null => return Ok(Datum::Null),
                    _ => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "{}() requires a text privilege name",
                            name
                        ));
                    }
                };
                let answer = match name {
                    "has_table_privilege" => cat.has_table_privilege(role, object, privilege)?,
                    "has_column_privilege" => cat.has_column_privilege(
                        role,
                        object,
                        privilege_column.expect("column function parsed a column"),
                        privilege,
                    )?,
                    "has_any_column_privilege" => {
                        cat.has_any_column_privilege(role, object, privilege)?
                    }
                    "has_sequence_privilege" => {
                        cat.has_sequence_privilege(role, object, privilege)?
                    }
                    "has_schema_privilege" => cat.has_schema_privilege(role, object, privilege)?,
                    "has_type_privilege" => cat.has_type_privilege(role, object, privilege)?,
                    "has_language_privilege" => {
                        cat.has_language_privilege(role, object, privilege)?
                    }
                    "has_function_privilege" => {
                        cat.has_function_privilege(role, object, privilege)?
                    }
                    "has_database_privilege" => {
                        cat.has_database_privilege(role, object, privilege)?
                    }
                    "has_tablespace_privilege" => {
                        cat.has_tablespace_privilege(role, object, privilege)?
                    }
                    "has_parameter_privilege" => {
                        cat.has_parameter_privilege(role, object, privilege)?
                    }
                    _ => unreachable!(),
                };
                Ok(answer.map(Datum::Bool).unwrap_or(Datum::Null))
            }
            "pg_table_is_visible"
            | "pg_type_is_visible"
            | "pg_function_is_visible"
            | "pg_collation_is_visible"
            | "pg_relation_is_publishable" => {
                arity(1)?;
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid,
                    None => return Ok(Datum::Null),
                };
                Ok(match name {
                    "pg_table_is_visible" => cat.relation_is_visible(oid.0),
                    "pg_type_is_visible" => cat.type_is_visible(oid.0),
                    "pg_function_is_visible" => cat.function_is_visible(oid.0),
                    "pg_collation_is_visible" => cat.collation_is_visible(oid.0),
                    "pg_relation_is_publishable" => cat.relation_is_publishable(oid.0),
                    _ => unreachable!(),
                }
                .map(Datum::Bool)
                .unwrap_or(Datum::Null))
            }
            "pg_get_indexdef" => {
                // `(oid, n, _)` with n>0 returns the n-th indexed column.
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
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
                Ok(cat
                    .index_def(oid, col, arena)?
                    .map(Datum::Text)
                    .unwrap_or(Datum::Null))
            }
            "pg_get_constraintdef" => {
                // psql `\d` calls this with a constraint OID; reconstruct a
                // foreign-key definition via the catalog resolver when present.
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
                };
                Ok(cat
                    .constraint_def(oid, arena)?
                    .map(Datum::Text)
                    .unwrap_or(Datum::Null))
            }
            "pg_get_partkeydef" => {
                arity(1)?;
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
                };
                Ok(cat
                    .partition_key_def(oid, arena)?
                    .map(Datum::Text)
                    .unwrap_or(Datum::Null))
            }
            "pg_get_functiondef" => {
                arity(1)?;
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
                };
                match cat.function_def(oid, arena)? {
                    Some(definition) => Ok(Datum::Text(definition)),
                    None => Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function pg_get_functiondef(integer) does not exist"
                    )),
                }
            }
            "pg_get_triggerdef" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function pg_get_triggerdef(...) does not exist"
                    ));
                }
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
                };
                Ok(cat
                    .trigger_def(oid, arena)?
                    .map(Datum::Text)
                    .unwrap_or(Datum::Null))
            }
            "pg_get_function_arguments"
            | "pg_get_function_identity_arguments"
            | "pg_get_function_result" => {
                arity(1)?;
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
                };
                let value = if name == "pg_get_function_result" {
                    cat.function_result(oid, arena)?
                } else {
                    cat.function_arguments(
                        oid,
                        name == "pg_get_function_identity_arguments",
                        arena,
                    )?
                };
                Ok(value.map(Datum::Text).unwrap_or(Datum::Null))
            }
            "pg_get_function_sqlbody" => {
                arity(1)?;
                // Supported bodies are stored as prosrc; pg_dump selects it
                // whenever this SQL-standard parse-tree representation is NULL.
                Ok(Datum::Null)
            }
            "obj_description" => {
                // obj_description(objoid [, catalog]): the object's comment.
                // The optional catalog selects the object's catalog class. The
                // deprecated one-argument form keeps the historical pg_class
                // default used by clients.
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match ObjectOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
                };
                let catalog_name = if args.len() >= 2 {
                    match eval_full(args[1], arena, params, row, hooks)? {
                        Datum::Text(name) => name,
                        _ => return Ok(Datum::Null),
                    }
                } else {
                    "pg_class"
                };
                if matches!(catalog_name, "pg_database" | "pg_tablespace" | "pg_authid") {
                    return Ok(Datum::Null);
                }
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
                let oid = match ObjectOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
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
            "shobj_description" => {
                arity(2)?;
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match ObjectOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
                };
                let catalog_name = match eval_full(args[1], arena, params, row, hooks)? {
                    Datum::Text(name) => name,
                    _ => return Ok(Datum::Null),
                };
                Ok(cat
                    .comment(catalog_name, oid, 0, arena)?
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
                let oid = match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
                };
                Ok(cat
                    .view_def(oid, arena)?
                    .map(Datum::Text)
                    .unwrap_or(Datum::Null))
            }
            "pg_get_ruledef" => {
                if !(1..=2).contains(&args.len()) {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function pg_get_ruledef(...) does not exist"
                    ));
                }
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
                };
                Ok(cat
                    .rule_def(oid, arena)?
                    .map(Datum::Text)
                    .unwrap_or(Datum::Null))
            }
            "pg_table_size" => {
                arity(1)?;
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
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
                    Datum::Text(_) | Datum::Int4(_) | Datum::Int8(_) | Datum::RegObject { .. } => {
                        Ok(Datum::Int8(cat.database_size()?))
                    }
                    Datum::Null => Ok(Datum::Null),
                    _ => Ok(Datum::Null),
                }
            }
            "pg_tablespace_location" => {
                arity(1)?;
                let oid = match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
                };
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                Ok(cat
                    .tablespace_location(oid, arena)?
                    .map(Datum::Text)
                    .unwrap_or(Datum::Null))
            }
            "pg_tablespace_size" => {
                arity(1)?;
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Int4(_) | Datum::Oid(_) | Datum::Int8(_) | Datum::Text(_) => {
                        Ok(Datum::Int8(cat.database_size()?))
                    }
                    Datum::Null => Ok(Datum::Null),
                    _ => Ok(Datum::Null),
                }
            }
            "pg_get_statisticsobjdef_columns" => {
                arity(1)?;
                let oid = match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
                };
                let Some(catalog) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                Ok(catalog
                    .statistics_columns(oid, arena)?
                    .map(Datum::Text)
                    .unwrap_or(Datum::Null))
            }
            "pg_get_statisticsobjdef" => {
                arity(1)?;
                let oid = match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
                };
                let Some(catalog) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                Ok(catalog
                    .statistics_definition(oid, arena)?
                    .map(Datum::Text)
                    .unwrap_or(Datum::Null))
            }
            "pg_get_statisticsobjdef_expressions" => {
                arity(1)?;
                let oid = match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
                };
                let Some(catalog) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                Ok(catalog
                    .statistics_expressions(oid, arena)?
                    .unwrap_or(Datum::Null))
            }
            "format_type" => {
                arity(2)?;
                // format_type(typoid, typmod): map the common base-type oids back to
                // their SQL spelling; unknown oids render as "???".
                let o = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Null => return Ok(Datum::Null),
                    value => match CatalogOid::parse(value)? {
                        Some(oid) => oid.0,
                        None => -1,
                    },
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
                let (base, array) = match coltype {
                    ColType::Array(element) => (element.to_coltype(), true),
                    scalar => (scalar, false),
                };
                let name = base.name();
                // Array typmods describe their elements. Render the base under
                // that modifier, then attach [] as PostgreSQL does.
                let mut text = match TypeMod::decode(base, type_mod) {
                    TypeMod::None => stack_format!(80, "{}", name),
                    TypeMod::Length(n) => stack_format!(80, "{}({})", name, n),
                    TypeMod::NumericPS { precision, scale } => {
                        stack_format!(80, "{}({},{})", name, precision, scale)
                    }
                    TypeMod::TemporalPrecision(p) => {
                        // The precision sits inside the name, before the
                        // time-zone tail — `timestamp(3) without time zone`. The
                        // split finds the tail for both spellings, since
                        // "without" begins "with".
                        match name.split_once(" with") {
                            Some((head, tail)) => {
                                stack_format!(80, "{}({}) with{}", head, p, tail)
                            }
                            None => stack_format!(80, "{}({})", name, p),
                        }
                    }
                    TypeMod::IntervalMod { range, precision } => match range.name() {
                        Some(fields) => match precision {
                            Some(p) => stack_format!(80, "interval {}({})", fields, p),
                            None => stack_format!(80, "interval {}", fields),
                        },
                        None => match precision {
                            Some(p) => stack_format!(80, "interval({})", p),
                            None => stack_format!(80, "interval"),
                        },
                    },
                };
                if array {
                    use core::fmt::Write as _;
                    let _ = text.write_str("[]");
                }
                Ok(Datum::Text(
                    arena.alloc_str(text.as_str()).map_err(|_| arena_full())?,
                ))
            }
            "pg_encoding_to_char" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Int2(code) => Ok(Datum::Text(
                        crate::storage::PgEncoding::from_code(code as u8)
                            .filter(|_| code >= 0)
                            .map_or("", crate::storage::PgEncoding::name),
                    )),
                    Datum::Int4(code) => Ok(Datum::Text(
                        u8::try_from(code)
                            .ok()
                            .and_then(crate::storage::PgEncoding::from_code)
                            .map_or("", crate::storage::PgEncoding::name),
                    )),
                    Datum::Null => Ok(Datum::Null),
                    other => Err(type_mismatch("pg_encoding_to_char", &other)),
                }
            }
            "pg_char_to_encoding" => {
                arity(1)?;
                match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Text(name) => Ok(Datum::Int4(
                        crate::storage::PgEncoding::parse(name).map_or(-1, |value| value.code()),
                    )),
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
                let regtype = |referenced_oid, name| Datum::Regtype {
                    referenced_oid,
                    name,
                };
                if let crate::sql::ast::Expr::Call {
                    name,
                    args,
                    argument_names,
                    star: false,
                    ..
                } = args[0]
                    && let Some(cat) = hooks.catalog
                    && args.len() <= crate::sql::parser::MAX_LIST
                {
                    let mut argument_oids =
                        [crate::sql::types::oid::UNKNOWN; crate::sql::parser::MAX_LIST];
                    for (index, argument) in args.iter().enumerate() {
                        argument_oids[index] = match argument {
                            crate::sql::ast::Expr::Cast { type_name, .. } => cat
                                .user_type_oid(type_name)
                                .unwrap_or(crate::sql::types::oid::UNKNOWN),
                            _ => crate::sql::exec::infer_type_res(
                                argument,
                                &crate::sql::exec::NoCols,
                            )
                            .map(|(oid, _)| oid)
                            .unwrap_or(crate::sql::types::oid::UNKNOWN),
                        };
                    }
                    if let Some(referenced_oid) = cat.routine_result_oid(
                        name,
                        argument_names,
                        false,
                        &argument_oids[..args.len()],
                    ) && let Some(type_name) = cat.type_name(referenced_oid, arena)?
                    {
                        return Ok(regtype(referenced_oid, type_name));
                    }
                }
                // A bare user-typed column retains its declared identity; an
                // expression over it uses the representation type below.
                if let crate::sql::ast::Expr::Column { qualifier, name } = args[0]
                    && let Some(identity) = row.column_user_type(*qualifier, name)
                {
                    if let Some(ColType::Array(element)) = row.col_type(*qualifier, name)
                        && element.user_type_slot().is_some()
                        && let Some(cat) = hooks.catalog
                        && let Some(name) = cat.user_array_name(element, arena)?
                    {
                        return Ok(regtype(element.array_oid(), name));
                    }
                    if let Some(cat) = hooks.catalog
                        && let Some(referenced_oid) = cat.user_type_identity_oid(identity, false)
                        && let Some(type_name) = cat.type_name(referenced_oid, arena)?
                    {
                        return Ok(regtype(referenced_oid, type_name));
                    }
                }
                if let crate::sql::ast::Expr::Subscript { base, .. } = args[0]
                    && let crate::sql::ast::Expr::Column { qualifier, name } = &**base
                    && let Some(ColType::Array(element)) = row.col_type(*qualifier, name)
                    && let Some(cat) = hooks.catalog
                    && let Some(name) = cat.type_name(element.element_oid(), arena)?
                {
                    return Ok(regtype(element.element_oid(), name));
                }
                let v = eval_full(args[0], arena, params, row, hooks)?;
                if let crate::sql::ast::Expr::Cast { type_name, .. } = args[0]
                    && let Some(cat) = hooks.catalog
                    && let Some(name) = cat.user_type_name(type_name, arena)?
                    && let Some(referenced_oid) = cat.user_type_oid(type_name)
                {
                    return Ok(regtype(referenced_oid, name));
                }
                // A named composite is represented structurally at execution
                // time, while its PostgreSQL identity belongs to the catalog.
                // Resolve that identity before the generic static-type path,
                // whose structural representation is `record`.
                if matches!(v, Datum::Composite { .. } | Datum::CompositeText { .. })
                    && let Some(cat) = hooks.catalog
                    && let Some(name) = cat.type_name(v.type_oid(), arena)?
                {
                    return Ok(regtype(v.type_oid(), name));
                }
                if matches!(args[0], crate::sql::ast::Expr::Field { .. })
                    && let Ok(super::super::ExpressionTypeIdentity::Known(referenced_oid)) =
                        super::super::expression_type_identity(args[0], row, hooks)
                    && let Some(cat) = hooks.catalog
                    && let Some(name) = cat.type_name(referenced_oid, arena)?
                {
                    return Ok(regtype(referenced_oid, name));
                }
                // Array subscripting yields a structural record value, but
                // inference still retains the declared named-composite OID.
                // Prefer that catalog identity to the structural runtime tag.
                if let Some(referenced_oid) = exec::typeof_static_oid(args[0], row, hooks.catalog)
                    && let Some(cat) = hooks.catalog
                    && let Some(name) = cat.type_name(referenced_oid, arena)?
                {
                    return Ok(regtype(referenced_oid, name));
                }
                // PostgreSQL's pg_typeof reports the argument's *static* type —
                // `current_user` is `name` though the value is plain text. The
                // static answer is used whenever it is consistent with the
                // runtime value (same storage type, or NULL); an inconsistent
                // one — a mis-inferred set-returning function, say — falls
                // back to the type the value itself carries.
                if let Some(name) = exec::typeof_static(args[0], row, hooks.catalog)
                    && let Some(referenced_oid) =
                        exec::typeof_static_oid(args[0], row, hooks.catalog)
                {
                    let consistent = v.is_null()
                        || exec::typeof_static_coltype(args[0], row, hooks.catalog).is_some_and(
                            |ct| {
                                ct.storage().oid() == v.type_oid()
                                    || matches!((ct, v), (ColType::Xid, Datum::Oid(_)))
                            },
                        );
                    if consistent {
                        return Ok(regtype(referenced_oid, name));
                    }
                }
                // An enum value reports its type name, resolved from the slot.
                if let Datum::Enum { slot, .. } = v
                    && let Some(cat) = hooks.catalog
                    && let Some(name) = cat.enum_name(slot, arena)?
                {
                    return Ok(regtype(crate::sql::types::oid::enum_oid(slot), name));
                }
                if let Datum::Array { element, .. } = v
                    && element.user_type_slot().is_some()
                    && let Some(cat) = hooks.catalog
                    && let Some(name) = cat.user_array_name(element, arena)?
                {
                    return Ok(regtype(element.array_oid(), name));
                }
                let referenced_oid = if v.is_null() {
                    crate::sql::types::oid::UNKNOWN
                } else {
                    v.type_oid()
                };
                let name = match v {
                    Datum::Null => "unknown",
                    Datum::Bool(_) => "boolean",
                    Datum::Int2(_) => "smallint",
                    Datum::Int4(_) => "integer",
                    Datum::Oid(_) => "oid",
                    Datum::Int8(_) => "bigint",
                    Datum::Float4(_) => "real",
                    Datum::Float8(_) => "double precision",
                    Datum::Char(_) => "\"char\"",
                    Datum::Text(_) => "text",
                    Datum::Bpchar(_) => "character",
                    Datum::Regtype { .. } => "regtype",
                    Datum::RegObject { type_oid, .. } => match type_oid {
                        crate::sql::types::oid::REGPROC => "regproc",
                        crate::sql::types::oid::REGPROCEDURE => "regprocedure",
                        crate::sql::types::oid::REGOPER => "regoper",
                        crate::sql::types::oid::REGOPERATOR => "regoperator",
                        crate::sql::types::oid::REGCLASS => "regclass",
                        crate::sql::types::oid::REGNAMESPACE => "regnamespace",
                        crate::sql::types::oid::REGROLE => "regrole",
                        _ => "regobject",
                    },
                    Datum::Date(_) => "date",
                    Datum::Timestamp(_) => "timestamp without time zone",
                    Datum::Timestamptz(_) => "timestamp with time zone",
                    Datum::Time(_) => "time without time zone",
                    Datum::Timetz(..) => "time with time zone",
                    Datum::Interval(_) => "interval",
                    Datum::Json { jsonb: false, .. } => "json",
                    Datum::Json { jsonb: true, .. } => "jsonb",
                    Datum::TsVector(_) => "tsvector",
                    Datum::TsQuery(_) => "tsquery",
                    Datum::Array { element, .. } => element.typeof_name(),
                    Datum::Int2Vector(_) => "int2vector",
                    Datum::OidVector(_) => "oidvector",
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
                    Datum::Geometry { kind, .. } => kind.name(),
                    Datum::Record(_) => "record",
                    Datum::Enum { .. } => "enum",
                    Datum::Composite { .. } | Datum::CompositeText { .. } => "record",
                    Datum::PgDdlCommand => "pg_ddl_command",
                };
                Ok(regtype(referenced_oid, name))
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
