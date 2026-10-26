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
    ColumnLookup, EvalHooks, SqlError, arena_full, eval_full, sqlstate, text_arg, type_mismatch,
};

/// A catalog OID accepted by identity predicates.  Keeping conversion at the
/// boundary prevents a wide integer from wrapping into an unrelated object.
#[derive(Clone, Copy)]
struct CatalogOid(i32);

impl CatalogOid {
    /// Keep all four OID bytes in the legacy signed catalog interface; wider
    /// integers are rejected before their bits can name a different object.
    fn parse(value: Datum<'_>) -> Result<Option<Self>, SqlError> {
        match value {
            Datum::Null => Ok(None),
            Datum::Int2(oid) => Ok(Some(Self(i32::from(oid)))),
            Datum::Int4(oid) => Ok(Some(Self(oid))),
            Datum::Oid(oid) => Ok(Some(Self(oid as i32))),
            Datum::Int8(oid) => u32::try_from(oid)
                .map(|oid| Self(oid as i32))
                .map(Some)
                .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "OID out of range")),
            Datum::Regtype { referenced_oid, .. } | Datum::RegObject { referenced_oid, .. } => {
                Ok(Some(Self(referenced_oid)))
            }
            // PostgreSQL resolves an untyped string literal against the OID
            // argument in the intrinsic signature before the function runs.
            // String literals are represented as text in the AST, so perform
            // that parse at this identity boundary and carry only CatalogOid
            // afterward.
            Datum::Text(raw) => super::super::parse_oid(raw).map(|oid| Some(Self(oid as i32))),
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
            Datum::Int2(oid) => u32::try_from(oid)
                .map(Self)
                .map(Some)
                .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "OID out of range")),
            Datum::Regtype { referenced_oid, .. } => Ok(Some(Self(referenced_oid as u32))),
            Datum::Int4(oid) => u32::try_from(oid)
                .map(Self)
                .map(Some)
                .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "OID out of range")),
            Datum::Int8(oid) => u32::try_from(oid)
                .map(Self)
                .map(Some)
                .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "OID out of range")),
            Datum::RegObject { referenced_oid, .. } => Ok(Some(Self(referenced_oid as u32))),
            Datum::Text(raw) => super::super::parse_oid(raw).map(|oid| Some(Self(oid))),
            _ => Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "object identity must be oid"
            )),
        }
    }
}

fn advisory_key<'a>(
    name: &str,
    args: &[&Expr<'a>],
    arena: &'a crate::mem::arena::Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Option<crate::sql::lock::AdvisoryKey>, SqlError> {
    let integer = |value: Datum<'_>| -> Result<Option<i64>, SqlError> {
        match value {
            Datum::Null => Ok(None),
            Datum::Int2(value) => Ok(Some(i64::from(value))),
            Datum::Int4(value) => Ok(Some(i64::from(value))),
            Datum::Int8(value) => Ok(Some(value)),
            other => Err(type_mismatch(name, &other)),
        }
    };
    match args {
        [argument] => integer(eval_full(argument, arena, params, row, hooks)?)
            .map(|value| value.map(crate::sql::lock::AdvisoryKey::from_bigint)),
        [class_id, object_id] => {
            let Some(class_id) = integer(eval_full(class_id, arena, params, row, hooks)?)? else {
                return Ok(None);
            };
            let Some(object_id) = integer(eval_full(object_id, arena, params, row, hooks)?)? else {
                return Ok(None);
            };
            let class_id = i32::try_from(class_id)
                .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "integer out of range"))?;
            let object_id = i32::try_from(object_id)
                .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "integer out of range"))?;
            Ok(Some(crate::sql::lock::AdvisoryKey::from_ints(
                class_id, object_id,
            )))
        }
        _ => Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "function {}(...) with {} arguments does not exist",
            name,
            args.len()
        )),
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

std::thread_local! {
    static SESSION_USER: core::cell::Cell<crate::storage::SessionIdentity> =
        core::cell::Cell::new(crate::storage::SessionIdentity::bootstrap());
    static CURRENT_USER: core::cell::Cell<crate::storage::SessionIdentity> =
        core::cell::Cell::new(crate::storage::SessionIdentity::bootstrap());
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
    pub names: [crate::util::StackStr<64>; crate::storage::MAX_PATH_ENTRIES],
    pub n: usize,
    pub catalog_pos: usize,
}

std::thread_local! {
    static SESSION_SCHEMAS: core::cell::RefCell<SessionSchemas> =
        const {
            core::cell::RefCell::new(SessionSchemas {
                names: [crate::util::StackStr::new(); crate::storage::MAX_PATH_ENTRIES],
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

/// A background callback restores the frontend's published session state.
pub(crate) struct SessionEvaluationScope {
    settings: SessionSettings,
    schemas: SessionSchemas,
    session_user: crate::storage::SessionIdentity,
    current_user: crate::storage::SessionIdentity,
    database: crate::util::StackStr<64>,
    timezone: crate::sql::timezone::Timezone,
}

pub(crate) fn retain_session_evaluation() -> SessionEvaluationScope {
    SessionEvaluationScope {
        settings: SESSION_SETTINGS.with(|value| *value.borrow()),
        schemas: session_schemas(),
        session_user: session_identity(),
        current_user: current_identity(),
        database: CURRENT_DATABASE.with(|value| *value.borrow()),
        timezone: crate::sql::timezone::session(),
    }
}

impl Drop for SessionEvaluationScope {
    fn drop(&mut self) {
        SESSION_SETTINGS.with(|value| *value.borrow_mut() = self.settings);
        set_session_schemas(self.schemas);
        set_session_user(self.session_user);
        set_current_user(self.current_user);
        set_current_database(self.database.as_str());
        crate::sql::timezone::set_session(self.timezone);
    }
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

pub(crate) fn set_session_user(user: crate::storage::SessionIdentity) {
    SESSION_USER.with(|active| active.set(user));
    set_current_user(user);
}

pub(crate) fn set_current_user(user: crate::storage::SessionIdentity) {
    CURRENT_USER.with(|active| active.set(user));
}

pub(crate) struct CurrentUserScope {
    prior: crate::storage::SessionIdentity,
}

impl Drop for CurrentUserScope {
    fn drop(&mut self) {
        set_current_user(self.prior);
    }
}

pub(crate) fn enter_current_user(user: crate::storage::SessionIdentity) -> CurrentUserScope {
    let prior = current_identity();
    set_current_user(user);
    CurrentUserScope { prior }
}

pub(crate) fn session_identity() -> crate::storage::SessionIdentity {
    SESSION_USER.with(core::cell::Cell::get)
}

pub(crate) fn current_identity() -> crate::storage::SessionIdentity {
    CURRENT_USER.with(core::cell::Cell::get)
}

pub fn session_user_owned() -> crate::util::StackStr<64> {
    session_identity().name()
}

pub fn current_user_owned() -> crate::util::StackStr<64> {
    current_identity().name()
}

fn session_user_str(arena: &crate::mem::arena::Arena) -> Result<&str, SqlError> {
    let user = session_identity().require_name()?;
    arena.alloc_str(user.as_str()).map_err(|_| arena_full())
}

fn current_user_str(arena: &crate::mem::arena::Arena) -> Result<&str, SqlError> {
    let user = current_identity().require_name()?;
    arena.alloc_str(user.as_str()).map_err(|_| arena_full())
}

fn object_address_parts<'a, const N: usize>(
    parts: &[crate::util::StackStr<N>],
    arena: &'a crate::mem::arena::Arena,
) -> Result<Datum<'a>, SqlError> {
    let mut values = [Datum::Null; crate::sql::event_trigger::MAX_ADDRESS_PARTS];
    for (value, part) in values.iter_mut().zip(parts) {
        *value = Datum::Text(part.as_str());
    }
    Ok(Datum::Array {
        element: ArrElem::Text,
        raw: array::build(&values[..parts.len()], arena)?,
    })
}

fn text_array_parts<'a>(
    value: Datum<'a>,
) -> Result<
    Option<(
        [&'a str; crate::sql::event_trigger::MAX_ADDRESS_PARTS],
        usize,
    )>,
    SqlError,
> {
    let Datum::Array {
        element: ArrElem::Text,
        raw,
    } = value
    else {
        return if value.is_null() {
            Ok(None)
        } else {
            Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "object address components must be text arrays"
            ))
        };
    };
    let shape = array::shape(raw).ok_or_else(|| {
        sql_err!(
            sqlstate::INVALID_BINARY_REPRESENTATION,
            "invalid array representation"
        )
    })?;
    if shape.dimension_count() > 1 {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "object address components must be one-dimensional arrays"
        ));
    }
    let count = array::len(raw);
    if count > crate::sql::event_trigger::MAX_ADDRESS_PARTS {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "object address has too many components"
        ));
    }
    let mut parts = [""; crate::sql::event_trigger::MAX_ADDRESS_PARTS];
    for (index, part) in parts[..count].iter_mut().enumerate() {
        *part = match array::get(raw, ArrElem::Text, index) {
            Some(Datum::Text(value)) => value,
            Some(Datum::Null) => {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "name or argument lists may not contain nulls"
                ));
            }
            Some(_) => unreachable!("text arrays decode text elements"),
            None => unreachable!("array length bounds each element"),
        };
    }
    Ok(Some((parts, count)))
}

fn object_address_record<'a>(
    class_id: i32,
    object_id: i32,
    sub_id: i32,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Datum<'a>, SqlError> {
    let fields = arena
        .alloc_slice_copy(&[
            crate::sql::types::RecordField {
                name: "classid",
                type_oid: crate::sql::types::oid::OID,
                value: Datum::Oid(class_id as u32),
            },
            crate::sql::types::RecordField {
                name: "objid",
                type_oid: crate::sql::types::oid::OID,
                value: Datum::Oid(object_id as u32),
            },
            crate::sql::types::RecordField {
                name: "objsubid",
                type_oid: crate::sql::types::oid::INT4,
                value: Datum::Int4(sub_id),
            },
        ])
        .map_err(|_| arena_full())?;
    Ok(Datum::Record(fields))
}

fn identify_object_record<'a>(
    object: crate::sql::event_trigger::EventObject,
    address: bool,
    arena: &'a crate::mem::arena::Arena,
) -> Result<Datum<'a>, SqlError> {
    let object_type = arena
        .alloc_str(object.object_type.as_str())
        .map_err(|_| arena_full())?;
    let object_name = object
        .object_name
        .as_ref()
        .map(|name| name.as_str())
        .or_else(|| {
            (object.object_type.as_str().ends_with(" column") && object.address_name_count == 3)
                .then(|| object.address_names[1].as_str())
        });
    let fields = if address {
        arena
            .alloc_slice_copy(&[
                crate::sql::types::RecordField {
                    name: "type",
                    type_oid: crate::sql::types::oid::TEXT,
                    value: Datum::Text(object_type),
                },
                crate::sql::types::RecordField {
                    name: "object_names",
                    type_oid: crate::sql::types::oid::TEXT_ARRAY,
                    value: if object.identity.is_empty() {
                        Datum::Null
                    } else {
                        object_address_parts(
                            &object.address_names[..object.address_name_count],
                            arena,
                        )?
                    },
                },
                crate::sql::types::RecordField {
                    name: "object_args",
                    type_oid: crate::sql::types::oid::TEXT_ARRAY,
                    value: if object.identity.is_empty() {
                        Datum::Null
                    } else {
                        object_address_parts(
                            &object.address_args[..object.address_arg_count],
                            arena,
                        )?
                    },
                },
            ])
            .map_err(|_| arena_full())?
    } else {
        arena
            .alloc_slice_copy(&[
                crate::sql::types::RecordField {
                    name: "type",
                    type_oid: crate::sql::types::oid::TEXT,
                    value: Datum::Text(object_type),
                },
                crate::sql::types::RecordField {
                    name: "schema",
                    type_oid: crate::sql::types::oid::TEXT,
                    value: match object.schema_name.as_ref() {
                        Some(value) => {
                            Datum::Text(arena.alloc_str(value.as_str()).map_err(|_| arena_full())?)
                        }
                        None => Datum::Null,
                    },
                },
                crate::sql::types::RecordField {
                    name: "name",
                    type_oid: crate::sql::types::oid::TEXT,
                    value: match object_name {
                        Some(value) => {
                            Datum::Text(arena.alloc_str(value).map_err(|_| arena_full())?)
                        }
                        None => Datum::Null,
                    },
                },
                crate::sql::types::RecordField {
                    name: "identity",
                    type_oid: crate::sql::types::oid::TEXT,
                    value: if object.identity.is_empty() {
                        Datum::Null
                    } else {
                        Datum::Text(
                            arena
                                .alloc_str(object.identity.as_str())
                                .map_err(|_| arena_full())?,
                        )
                    },
                },
            ])
            .map_err(|_| arena_full())?
    };
    Ok(Datum::Record(fields))
}

fn describe_catalog_object(
    object: crate::sql::event_trigger::EventObject,
    arena: &crate::mem::arena::Arena,
) -> Result<&str, SqlError> {
    let description = if object.object_type.as_str() == "domain constraint" {
        stack_format!(1024, "constraint {}", object.address_args[0].as_str())
    } else if object.object_type.as_str().ends_with(" column") && object.address_name_count == 3 {
        let column = crate::sql::types::acl_identifier(object.address_names[2].as_str());
        let schema = crate::sql::types::acl_identifier(object.address_names[0].as_str());
        let relation = crate::sql::types::acl_identifier(object.address_names[1].as_str());
        let relation_kind = object
            .object_type
            .as_str()
            .strip_suffix(" column")
            .unwrap_or("table");
        stack_format!(
            1024,
            "column {} of {} {}.{}",
            column.as_str(),
            relation_kind,
            schema.as_str(),
            relation.as_str()
        )
    } else {
        stack_format!(
            1024,
            "{} {}",
            object.object_type.as_str(),
            object.identity.as_str()
        )
    };
    if description.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "catalog object description exceeds static capacity"
        ));
    }
    arena
        .alloc_str(description.as_str())
        .map_err(|_| arena_full())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogPredicateKind {
    Privilege,
    Object,
}

fn strict_catalog_arguments<'a>(
    args: &[&Expr<'a>],
    mut evaluate: impl FnMut(usize, &Expr<'a>) -> Result<Datum<'a>, SqlError>,
) -> Result<Option<[Datum<'a>; 3]>, SqlError> {
    assert!(args.len() <= 3, "catalog call arity was validated");
    let mut values = [Datum::Null; 3];
    let mut constant_null = false;
    for (position, expression) in args.iter().enumerate() {
        if expression.is_constant() {
            values[position] = evaluate(position, expression)?;
            constant_null |= values[position].is_null();
        }
    }
    if constant_null {
        return Ok(None);
    }
    for (position, expression) in args.iter().enumerate() {
        if !expression.is_constant() {
            values[position] = evaluate(position, expression)?;
        }
    }
    Ok((!values[..args.len()].iter().any(Datum::is_null)).then_some(values))
}

/// Execution and result descriptions share these accepted Boolean families.
pub(crate) fn catalog_predicate_kind(name: &str) -> Option<CatalogPredicateKind> {
    let name = name.strip_prefix("pg_catalog.").unwrap_or(name);
    match name {
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
        | "has_foreign_data_wrapper_privilege"
        | "has_server_privilege"
        | "has_largeobject_privilege"
        | "has_parameter_privilege" => Some(CatalogPredicateKind::Privilege),
        "pg_table_is_visible"
        | "pg_type_is_visible"
        | "pg_function_is_visible"
        | "pg_collation_is_visible"
        | "pg_operator_is_visible"
        | "pg_opclass_is_visible"
        | "pg_opfamily_is_visible"
        | "pg_conversion_is_visible"
        | "pg_statistics_obj_is_visible"
        | "pg_ts_dict_is_visible"
        | "pg_ts_config_is_visible"
        | "pg_relation_is_publishable" => Some(CatalogPredicateKind::Object),
        _ => None,
    }
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
    let predicate = catalog_predicate_kind(name);
    if predicate.is_none()
        && !matches!(
            name,
            "version"
                | "pg_backend_pid"
                | "pg_blocking_pids"
                | "pg_cancel_backend"
                | "pg_terminate_backend"
                | "pg_listening_channels"
                | "pg_notification_queue_usage"
                | "pg_advisory_lock"
                | "pg_advisory_lock_shared"
                | "pg_try_advisory_lock"
                | "pg_try_advisory_lock_shared"
                | "pg_advisory_unlock"
                | "pg_advisory_unlock_shared"
                | "pg_advisory_unlock_all"
                | "pg_advisory_xact_lock"
                | "pg_advisory_xact_lock_shared"
                | "pg_try_advisory_xact_lock"
                | "pg_try_advisory_xact_lock_shared"
                | "pg_is_in_recovery"
                | "pg_reload_conf"
                | "pg_event_trigger_table_rewrite_oid"
                | "pg_event_trigger_table_rewrite_reason"
                | "pg_extension_config_dump"
                | "pg_input_is_valid"
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
                | "pg_identify_object"
                | "pg_identify_object_as_address"
                | "pg_get_object_address"
                | "pg_describe_object"
                | "pg_get_serial_sequence"
                | "pg_get_replica_identity_index"
                | "pg_my_temp_schema"
                | "pg_is_other_temp_schema"
                | "pg_get_indexdef"
                | "brin_summarize_new_values"
                | "brin_summarize_range"
                | "brin_desummarize_range"
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
                | "to_regclass"
                | "to_regproc"
                | "to_regprocedure"
                | "to_regoper"
                | "to_regoperator"
                | "to_regtype"
                | "to_regnamespace"
                | "to_regrole"
                | "to_regcollation"
                | "to_regtypemod"
                | "pg_encoding_to_char"
                | "pg_char_to_encoding"
                | "getdatabaseencoding"
                | "pg_current_xact_id"
                | "pg_current_xact_id_if_assigned"
                | "txid_current"
                | "txid_current_if_assigned"
                | "pg_current_snapshot"
                | "txid_current_snapshot"
                | "pg_snapshot_xmin"
                | "pg_snapshot_xmax"
                | "txid_snapshot_xmin"
                | "txid_snapshot_xmax"
                | "pg_visible_in_snapshot"
                | "txid_visible_in_snapshot"
                | "pg_xact_status"
                | "txid_status"
                | "pg_typeof"
                | "current_setting"
                | "set_config"
                | "acldefault"
                | "pg_get_acl"
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
            "brin_summarize_new_values" => {
                arity(1)?;
                let Some(oid) = CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)?
                else {
                    return Ok(Datum::Null);
                };
                let catalog = hooks.catalog.ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "BRIN maintenance access is unavailable"
                    )
                })?;
                Ok(Datum::Int4(catalog.brin_summarize_new_values(oid.0)?))
            }
            "brin_summarize_range" | "brin_desummarize_range" => {
                arity(2)?;
                let Some(oid) = CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)?
                else {
                    return Ok(Datum::Null);
                };
                let block = match eval_full(args[1], arena, params, row, hooks)? {
                    Datum::Int2(value) => i64::from(value),
                    Datum::Int4(value) => i64::from(value),
                    Datum::Int8(value) => value,
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch(name, &other)),
                };
                let block = u64::try_from(block).map_err(|_| {
                    sql_err!(
                        sqlstate::NUMERIC_OUT_OF_RANGE,
                        "block number out of range: {}",
                        block
                    )
                })?;
                let catalog = hooks.catalog.ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "BRIN maintenance access is unavailable"
                    )
                })?;
                if name == "brin_summarize_range" {
                    Ok(Datum::Int4(catalog.brin_summarize_range(oid.0, block)?))
                } else {
                    catalog.brin_desummarize_range(oid.0, block)?;
                    Ok(Datum::Text(""))
                }
            }
            "pg_backend_pid" => {
                arity(0)?;
                Ok(Datum::Int4(
                    hooks
                        .catalog
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::FEATURE_NOT_SUPPORTED,
                                "backend identity access is unavailable"
                            )
                        })?
                        .current_backend_pid()?,
                ))
            }
            "pg_blocking_pids" => {
                arity(1)?;
                let backend_pid = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Int2(value) => i32::from(value),
                    Datum::Int4(value) => value,
                    Datum::Int8(value) => i32::try_from(value).map_err(|_| {
                        sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "integer out of range")
                    })?,
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch(name, &other)),
                };
                let catalog = hooks.catalog.ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "backend lock visibility is unavailable"
                    )
                })?;
                let count = catalog.blocking_backend_pid_count(backend_pid);
                if count > array::MAX_ELEMENTS {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "blocking backend list exceeds {} entries",
                        array::MAX_ELEMENTS
                    ));
                }
                let pids = arena
                    .alloc_slice_with(count, |_| 0_i32)
                    .map_err(|_| arena_full())?;
                let written = catalog.blocking_backend_pids(backend_pid, pids);
                let values = arena
                    .alloc_slice_with(written, |index| Datum::Int4(pids[index]))
                    .map_err(|_| arena_full())?;
                Ok(Datum::Array {
                    element: ArrElem::Int4,
                    raw: array::build(values, arena)?,
                })
            }
            "pg_cancel_backend" | "pg_terminate_backend" => {
                let terminate = name.eq_ignore_ascii_case("pg_terminate_backend");
                if args.is_empty() || args.len() > if terminate { 2 } else { 1 } || star {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function {}(...) with {} arguments does not exist",
                        name,
                        if star { 1 } else { args.len() }
                    ));
                }
                let pid = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Int2(value) => i32::from(value),
                    Datum::Int4(value) => value,
                    Datum::Int8(value) => i32::try_from(value).map_err(|_| {
                        sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "integer out of range")
                    })?,
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch(name, &other)),
                };
                if args.len() == 2 {
                    let timeout = match eval_full(args[1], arena, params, row, hooks)? {
                        Datum::Int2(value) => i64::from(value),
                        Datum::Int4(value) => i64::from(value),
                        Datum::Int8(value) => value,
                        Datum::Null => return Ok(Datum::Null),
                        other => return Err(type_mismatch(name, &other)),
                    };
                    if timeout < 0 {
                        return Err(sql_err!(
                            sqlstate::NUMERIC_OUT_OF_RANGE,
                            "timeout must not be negative"
                        ));
                    }
                    if timeout != 0 {
                        return Err(sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "waiting for backend termination is not supported; use a zero timeout"
                        ));
                    }
                }
                let catalog = hooks.catalog.ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "backend signaling is unavailable"
                    )
                })?;
                Ok(Datum::Bool(catalog.signal_backend(pid, terminate)?))
            }
            "pg_listening_channels" => {
                arity(0)?;
                let Some(index) = hooks.srf_index.and_then(|index| index.checked_sub(1)) else {
                    return Ok(Datum::Null);
                };
                let channel = hooks
                    .catalog
                    .and_then(|catalog| catalog.listening_channel_at(index));
                match channel {
                    Some(channel) => Ok(Datum::Text(
                        arena
                            .alloc_str(channel.as_str())
                            .map_err(|_| crate::sql::eval::arena_full())?,
                    )),
                    None => Ok(Datum::Null),
                }
            }
            "pg_notification_queue_usage" => {
                arity(0)?;
                // Notifications are delivered after every reactor dispatch;
                // no committed queue is observable from a SQL statement.
                Ok(Datum::Float8(0.0))
            }
            "pg_advisory_lock"
            | "pg_advisory_lock_shared"
            | "pg_try_advisory_lock"
            | "pg_try_advisory_lock_shared"
            | "pg_advisory_xact_lock"
            | "pg_advisory_xact_lock_shared"
            | "pg_try_advisory_xact_lock"
            | "pg_try_advisory_xact_lock_shared" => {
                let Some(key) = advisory_key(name, args, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let shared = name.ends_with("_shared");
                let transaction_scope = name.contains("_xact_");
                let try_only = name.starts_with("pg_try_");
                let acquired = hooks
                    .catalog
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "advisory locks are unavailable"
                        )
                    })?
                    .acquire_advisory_lock(
                        key.class_id,
                        key.object_id,
                        key.object_sub_id,
                        shared,
                        transaction_scope,
                        try_only,
                    )?;
                if try_only {
                    Ok(Datum::Bool(acquired))
                } else {
                    Ok(Datum::Text(""))
                }
            }
            "pg_advisory_unlock" | "pg_advisory_unlock_shared" => {
                let Some(key) = advisory_key(name, args, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                let unlocked = hooks
                    .catalog
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "advisory locks are unavailable"
                        )
                    })?
                    .unlock_advisory_lock(
                        key.class_id,
                        key.object_id,
                        key.object_sub_id,
                        name.ends_with("_shared"),
                    )?;
                Ok(Datum::Bool(unlocked))
            }
            "pg_advisory_unlock_all" => {
                arity(0)?;
                hooks
                    .catalog
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "advisory locks are unavailable"
                        )
                    })?
                    .unlock_all_advisory_locks()?;
                Ok(Datum::Text(""))
            }
            "pg_current_xact_id"
            | "pg_current_xact_id_if_assigned"
            | "txid_current"
            | "txid_current_if_assigned" => {
                arity(0)?;
                let assign = matches!(name, "pg_current_xact_id" | "txid_current");
                let transaction_id = hooks
                    .catalog
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "transaction identity access is unavailable"
                        )
                    })?
                    .current_transaction_id(assign)?;
                let Some(transaction_id) = transaction_id else {
                    return Ok(Datum::Null);
                };
                if name.starts_with("txid_") {
                    Ok(Datum::Int8(i64::try_from(transaction_id).map_err(
                        |_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "bigint out of range"),
                    )?))
                } else {
                    Ok(Datum::Xid8(transaction_id))
                }
            }
            "pg_current_snapshot" | "txid_current_snapshot" => {
                arity(0)?;
                let snapshot = hooks
                    .catalog
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "transaction snapshot access is unavailable"
                        )
                    })?
                    .current_transaction_snapshot(arena)?;
                Ok(Datum::Snapshot {
                    value: snapshot,
                    legacy: name == "txid_current_snapshot",
                })
            }
            "pg_snapshot_xmin" | "pg_snapshot_xmax" | "txid_snapshot_xmin"
            | "txid_snapshot_xmax" => {
                arity(1)?;
                let expected_legacy = name.starts_with("txid_");
                let snapshot = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Snapshot { value, legacy } if legacy == expected_legacy => value,
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch(name, &other)),
                };
                let value = if name.ends_with("xmin") {
                    snapshot.xmin()
                } else {
                    snapshot.xmax()
                };
                if expected_legacy {
                    Ok(Datum::Int8(i64::try_from(value).map_err(|_| {
                        sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "bigint out of range")
                    })?))
                } else {
                    Ok(Datum::Xid8(value))
                }
            }
            "pg_visible_in_snapshot" | "txid_visible_in_snapshot" => {
                arity(2)?;
                let expected_legacy = name.starts_with("txid_");
                let transaction_id = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Xid8(value) if !expected_legacy => value,
                    Datum::Int8(value) if expected_legacy => {
                        u64::try_from(value).map_err(|_| {
                            sql_err!(
                                sqlstate::NUMERIC_OUT_OF_RANGE,
                                "transaction ID must be nonnegative"
                            )
                        })?
                    }
                    Datum::Int4(value) if expected_legacy => {
                        u64::try_from(value).map_err(|_| {
                            sql_err!(
                                sqlstate::NUMERIC_OUT_OF_RANGE,
                                "transaction ID must be nonnegative"
                            )
                        })?
                    }
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch(name, &other)),
                };
                let snapshot = match eval_full(args[1], arena, params, row, hooks)? {
                    Datum::Snapshot { value, legacy } if legacy == expected_legacy => value,
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch(name, &other)),
                };
                Ok(Datum::Bool(snapshot.visible(transaction_id)))
            }
            "pg_xact_status" | "txid_status" => {
                arity(1)?;
                let legacy = name == "txid_status";
                let transaction_id = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Xid8(value) if !legacy => value,
                    Datum::Int8(value) if legacy => u64::try_from(value).map_err(|_| {
                        sql_err!(
                            sqlstate::NUMERIC_OUT_OF_RANGE,
                            "transaction ID must be nonnegative"
                        )
                    })?,
                    Datum::Int4(value) if legacy => u64::try_from(value).map_err(|_| {
                        sql_err!(
                            sqlstate::NUMERIC_OUT_OF_RANGE,
                            "transaction ID must be nonnegative"
                        )
                    })?,
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch(name, &other)),
                };
                Ok(hooks
                    .catalog
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "transaction status access is unavailable"
                        )
                    })?
                    .transaction_status(transaction_id)?
                    .map_or(Datum::Null, Datum::Text))
            }
            "pg_get_replica_identity_index" => {
                arity(1)?;
                let relation_oid = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Oid(oid) => i32::try_from(oid).ok(),
                    Datum::Int4(oid) => Some(oid),
                    Datum::RegObject { referenced_oid, .. } => Some(referenced_oid),
                    Datum::Null => return Ok(Datum::Null),
                    value => return Err(type_mismatch(name, &value)),
                };
                let Some(catalog) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let Some(index_oid) =
                    relation_oid.and_then(|oid| catalog.replica_identity_index_oid(oid))
                else {
                    return Ok(Datum::Null);
                };
                let name = catalog
                    .relname(index_oid, arena)?
                    .unwrap_or("pg_get_replica_identity_index");
                Ok(Datum::RegObject {
                    type_oid: crate::sql::types::oid::REGCLASS,
                    referenced_oid: index_oid,
                    name,
                })
            }
            "version" => {
                arity(0)?;
                Ok(Datum::Text(concat!(
                    "PostgreSQL 18.4 (pos3ql ",
                    env!("CARGO_PKG_VERSION"),
                    ") on ",
                    env!("POS3QL_BUILD_TARGET")
                )))
            }
            "pg_input_is_valid" => {
                arity(2)?;
                let (Some(input), Some(type_name)) = (
                    text_arg(name, args, 0, arena, params, row, hooks)?,
                    text_arg(name, args, 1, arena, params, row, hooks)?,
                ) else {
                    return Ok(Datum::Null);
                };
                Ok(Datum::Bool(
                    super::super::input_error(input, type_name, arena, hooks)?.is_none(),
                ))
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
            "pg_get_acl" => {
                arity(3)?;
                let oid = |value: Datum<'_>| -> Result<Option<u32>, SqlError> {
                    match value {
                        Datum::Oid(value) => Ok(Some(value)),
                        Datum::Int4(value) => Ok(Some(value as u32)),
                        Datum::RegObject { referenced_oid, .. } => Ok(Some(referenced_oid as u32)),
                        Datum::Null => Ok(None),
                        other => Err(type_mismatch("pg_get_acl", &other)),
                    }
                };
                let Some(classid) = oid(eval_full(args[0], arena, params, row, hooks)?)? else {
                    return Ok(Datum::Null);
                };
                let Some(objid) = oid(eval_full(args[1], arena, params, row, hooks)?)? else {
                    return Ok(Datum::Null);
                };
                let objsubid = match eval_full(args[2], arena, params, row, hooks)? {
                    Datum::Int4(value) => value,
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch("pg_get_acl", &other)),
                };
                let catalog = hooks.catalog.ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "object ACL catalog access is unavailable"
                    )
                })?;
                Ok(catalog
                    .object_acl(classid, objid, objsubid, arena)?
                    .unwrap_or(Datum::Null))
            }
            "acldefault" => {
                arity(2)?;
                let object_type = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Text(value) => value,
                    Datum::Char(value) => match value {
                        b'c' => "c",
                        b'd' => "d",
                        b'F' => "F",
                        b'S' => "S",
                        b'f' => "f",
                        b'l' => "l",
                        b'L' => "L",
                        b'p' => "p",
                        b'r' => "r",
                        b'n' => "n",
                        b's' => "s",
                        b't' => "t",
                        b'T' => "T",
                        _ => "",
                    },
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
                let object =
                    crate::sql::acl::DefaultObject::parse(object_type).ok_or_else(|| {
                        sql_err!(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "unrecognized object type abbreviation: {}",
                            object_type
                        )
                    })?;
                crate::sql::acl::default_acl(object, owner_oid as u32, catalog, arena)
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
                let mut elems = [Datum::Null; crate::storage::MAX_PATH_ENTRIES];
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
            "pg_get_userbyid" => {
                arity(1)?;
                let oid = match ObjectOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0 as i32,
                    None => return Ok(Datum::Null),
                };
                let cat = hooks.catalog.ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "pg_get_userbyid requires catalog access"
                    )
                })?;
                Ok(Datum::Text(crate::sql::catalog::user_name_by_oid(
                    cat, oid, arena,
                )?))
            }
            "pg_partition_ancestors" | "pg_partition_root" | "pg_partition_tree" => {
                arity(1)?;
                eval_full(args[0], arena, params, row, hooks)
            }
            _ if predicate == Some(CatalogPredicateKind::Privilege) => {
                let name = name.strip_prefix("pg_catalog.").unwrap_or(name);
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
                if matches!(object, Datum::Null) {
                    return Ok(Datum::Null);
                }
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
                if matches!(
                    name,
                    "has_table_privilege"
                        | "has_column_privilege"
                        | "has_any_column_privilege"
                        | "has_sequence_privilege"
                ) {
                    let relation = match object {
                        Datum::Text(name) => super::super::PrivilegeRelation::Name(name),
                        value @ (Datum::Int4(_)
                        | Datum::Oid(_)
                        | Datum::Int8(_)
                        | Datum::RegObject { .. }
                        | Datum::Regtype { .. }) => super::super::PrivilegeRelation::Oid(
                            CatalogOid::parse(value)?
                                .expect("non-null relation identity")
                                .0,
                        ),
                        _ => {
                            return Err(sql_err!(
                                sqlstate::DATATYPE_MISMATCH,
                                "object must be specified by name or oid"
                            ));
                        }
                    };
                    let answer = match name {
                        "has_table_privilege" => {
                            cat.has_table_privilege(role, relation, privilege)?
                        }
                        "has_column_privilege" => cat.has_column_privilege(
                            role,
                            relation,
                            privilege_column.expect("column function parsed a column"),
                            privilege,
                        )?,
                        "has_any_column_privilege" => {
                            cat.has_any_column_privilege(role, relation, privilege)?
                        }
                        "has_sequence_privilege" => {
                            cat.has_sequence_privilege(role, relation, privilege)?
                        }
                        _ => unreachable!("relation privilege function was classified"),
                    };
                    return Ok(answer.map_or(Datum::Null, Datum::Bool));
                }
                if matches!(
                    name,
                    "has_type_privilege"
                        | "has_schema_privilege"
                        | "has_function_privilege"
                        | "has_language_privilege"
                        | "has_database_privilege"
                        | "has_tablespace_privilege"
                        | "has_foreign_data_wrapper_privilege"
                        | "has_server_privilege"
                        | "has_largeobject_privilege"
                ) {
                    let target = match object {
                        Datum::Text(name) => super::super::PrivilegeObject::Name(name),
                        value => super::super::PrivilegeObject::Oid(
                            CatalogOid::parse(value)?
                                .expect("non-null object identity")
                                .0,
                        ),
                    };
                    let answer = match name {
                        "has_type_privilege" => cat.has_type_privilege(role, target, privilege)?,
                        "has_schema_privilege" => {
                            cat.has_schema_privilege(role, target, privilege)?
                        }
                        "has_function_privilege" => {
                            cat.has_function_privilege(role, target, privilege)?
                        }
                        "has_language_privilege" => {
                            cat.has_language_privilege(role, target, privilege)?
                        }
                        "has_database_privilege" => {
                            cat.has_database_privilege(role, target, privilege)?
                        }
                        "has_tablespace_privilege" => {
                            cat.has_tablespace_privilege(role, target, privilege)?
                        }
                        "has_foreign_data_wrapper_privilege" => {
                            cat.has_foreign_data_wrapper_privilege(role, target, privilege)?
                        }
                        "has_server_privilege" => {
                            cat.has_server_privilege(role, target, privilege)?
                        }
                        "has_largeobject_privilege" => {
                            cat.has_largeobject_privilege(role, target, privilege)?
                        }
                        _ => unreachable!("typed object privilege function was classified"),
                    };
                    return Ok(answer.map_or(Datum::Null, Datum::Bool));
                }
                let Datum::Text(parameter) = object else {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "has_parameter_privilege() requires a text parameter name"
                    ));
                };
                let answer = cat.has_parameter_privilege(role, parameter, privilege)?;
                Ok(answer.map_or(Datum::Null, Datum::Bool))
            }
            _ if predicate == Some(CatalogPredicateKind::Object) => {
                let name = name.strip_prefix("pg_catalog.").unwrap_or(name);
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
                    "pg_operator_is_visible" => cat.operator_is_visible(oid.0),
                    "pg_opclass_is_visible" => cat.operator_class_is_visible(oid.0),
                    "pg_opfamily_is_visible" => cat.operator_family_is_visible(oid.0),
                    "pg_conversion_is_visible" => cat.conversion_is_visible(oid.0),
                    "pg_statistics_obj_is_visible" => cat.statistics_object_is_visible(oid.0),
                    "pg_ts_dict_is_visible" => cat.text_search_dictionary_is_visible(oid.0),
                    "pg_ts_config_is_visible" => cat.text_search_configuration_is_visible(oid.0),
                    "pg_relation_is_publishable" => cat.relation_is_publishable(oid.0),
                    _ => unreachable!(),
                }
                .map(Datum::Bool)
                .unwrap_or(Datum::Null))
            }
            "pg_identify_object" | "pg_identify_object_as_address" | "pg_describe_object" => {
                arity(3)?;
                let Some(catalog) = hooks.catalog else {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "catalog object introspection is unavailable"
                    ));
                };
                let class_id =
                    match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                        Some(oid) => oid.0,
                        None => return Ok(Datum::Null),
                    };
                let object_id =
                    match CatalogOid::parse(eval_full(args[1], arena, params, row, hooks)?)? {
                        Some(oid) => oid.0,
                        None => return Ok(Datum::Null),
                    };
                let sub_id = match eval_full(args[2], arena, params, row, hooks)? {
                    Datum::Int2(value) => i32::from(value),
                    Datum::Int4(value) => value,
                    Datum::Int8(value) => i32::try_from(value).map_err(|_| {
                        sql_err!(
                            sqlstate::NUMERIC_OUT_OF_RANGE,
                            "object sub-ID is out of range"
                        )
                    })?,
                    Datum::Null => return Ok(Datum::Null),
                    _ => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "object sub-ID must be an integer"
                        ));
                    }
                };
                let object = catalog.catalog_object(class_id, object_id, sub_id)?;
                if name == "pg_describe_object" {
                    if object.identity.is_empty() {
                        Ok(Datum::Null)
                    } else {
                        Ok(Datum::Text(describe_catalog_object(object, arena)?))
                    }
                } else {
                    identify_object_record(object, name == "pg_identify_object_as_address", arena)
                }
            }
            "pg_get_object_address" => {
                arity(3)?;
                let object_type = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Text(value) => value,
                    Datum::Null => return Ok(Datum::Null),
                    _ => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "object type must be text"
                        ));
                    }
                };
                let Some((names, name_count)) =
                    text_array_parts(eval_full(args[1], arena, params, row, hooks)?)?
                else {
                    return Ok(Datum::Null);
                };
                let Some((arguments, argument_count)) =
                    text_array_parts(eval_full(args[2], arena, params, row, hooks)?)?
                else {
                    return Ok(Datum::Null);
                };
                let catalog = hooks.catalog.ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "catalog object address resolution is unavailable"
                    )
                })?;
                let (class_id, object_id, sub_id) = catalog.catalog_object_address(
                    object_type,
                    &names[..name_count],
                    &arguments[..argument_count],
                )?;
                object_address_record(class_id, object_id, sub_id, arena)
            }
            "pg_get_serial_sequence" => {
                arity(2)?;
                let table = match eval_full(args[0], arena, params, row, hooks)? {
                    Datum::Text(value) => value,
                    Datum::Null => return Ok(Datum::Null),
                    _ => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "pg_get_serial_sequence() requires a table name"
                        ));
                    }
                };
                let column = match eval_full(args[1], arena, params, row, hooks)? {
                    Datum::Text(value) => value,
                    Datum::Null => return Ok(Datum::Null),
                    _ => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "pg_get_serial_sequence() requires a column name"
                        ));
                    }
                };
                let Some(catalog) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                Ok(catalog
                    .serial_sequence_name(table, column, arena)?
                    .map(Datum::Text)
                    .unwrap_or(Datum::Null))
            }
            "pg_my_temp_schema" => {
                arity(0)?;
                Ok(Datum::Oid(
                    hooks
                        .catalog
                        .map_or(0, |catalog| catalog.current_temporary_namespace_oid())
                        as u32,
                ))
            }
            "pg_is_other_temp_schema" => {
                arity(1)?;
                let Some(catalog) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match CatalogOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
                };
                Ok(Datum::Bool(catalog.is_other_temporary_namespace(oid)))
            }
            "pg_get_indexdef" => {
                if !matches!(args.len(), 1 | 3) {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function pg_get_indexdef(...) does not exist"
                    ));
                }
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let Some(values) =
                    strict_catalog_arguments(args, |position, expression| {
                        let value = eval_full(expression, arena, params, row, hooks)?;
                        match position {
                            0 => Ok(CatalogOid::parse(value)?
                                .map_or(Datum::Null, |oid| Datum::Int4(oid.0))),
                            1 => super::super::cast_to(value, ColType::Int4, arena),
                            2 => super::super::cast_to(value, ColType::Bool, arena),
                            _ => unreachable!("index definition call arity was validated"),
                        }
                    })?
                else {
                    return Ok(Datum::Null);
                };
                let oid = match CatalogOid::parse(values[0])? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
                };
                let col = if args.len() >= 2 {
                    match values[1] {
                        Datum::Null => return Ok(Datum::Null),
                        value => match super::super::cast_to(value, ColType::Int4, arena)? {
                            Datum::Int4(value) => value,
                            _ => unreachable!("integer argument was coerced"),
                        },
                    }
                } else {
                    0
                };
                let pretty = if args.len() == 3 {
                    let pretty = values[2];
                    match super::super::cast_to(pretty, ColType::Bool, arena)? {
                        Datum::Null => return Ok(Datum::Null),
                        Datum::Bool(pretty) => pretty,
                        _ => unreachable!("boolean argument was coerced"),
                    }
                } else {
                    false
                };
                Ok(cat
                    .index_def(oid, col, pretty, arena)?
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
                if !(1..=2).contains(&args.len()) || star {
                    arity(1)?;
                }
                // obj_description(objoid [, catalog]): the object's comment.
                // The one-argument form searches local descriptions across catalogs.
                let Some(cat) = hooks.catalog else {
                    return Ok(Datum::Null);
                };
                let oid = match ObjectOid::parse(eval_full(args[0], arena, params, row, hooks)?)? {
                    Some(oid) => oid.0,
                    None => return Ok(Datum::Null),
                };
                let catalog_name = if args.len() >= 2 {
                    match eval_full(args[1], arena, params, row, hooks)? {
                        Datum::Text(name) => Some(name),
                        _ => return Ok(Datum::Null),
                    }
                } else {
                    None
                };
                Ok(cat
                    .comment(
                        crate::sql::eval::DescriptionTarget::Local {
                            catalog: catalog_name,
                            column: 0,
                        },
                        oid,
                        arena,
                    )?
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
                    .comment(
                        crate::sql::eval::DescriptionTarget::Local {
                            catalog: Some("pg_class"),
                            column: col,
                        },
                        oid,
                        arena,
                    )?
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
                    .comment(
                        crate::sql::eval::DescriptionTarget::Shared {
                            catalog: catalog_name,
                        },
                        oid,
                        arena,
                    )?
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
                if let Some(source_type) = super::super::static_type(args[0], row)
                    && source_type != ColType::PgNodeTree
                {
                    if matches!(args[0], Expr::Str(_)) {
                        return Err(sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "cannot accept a value of type pg_node_tree"
                        ));
                    }
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function pg_get_expr(...) does not exist"
                    ));
                }
                let Some(values) =
                    strict_catalog_arguments(args, |position, expression| {
                        let value = eval_full(expression, arena, params, row, hooks)?;
                        match position {
                            0 => Ok(value),
                            1 => Ok(CatalogOid::parse(value)?
                                .map_or(Datum::Null, |oid| Datum::Int4(oid.0))),
                            2 => super::super::cast_to(value, ColType::Bool, arena),
                            _ => unreachable!("expression call arity was validated"),
                        }
                    })?
                else {
                    return Ok(Datum::Null);
                };
                let Datum::PgNodeTree(node) = values[0] else {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "pg_get_expr requires a pg_node_tree value"
                    ));
                };
                let Datum::Int4(relation) = values[1] else {
                    unreachable!("relation argument was coerced");
                };
                let pretty = args.len() == 3 && values[2] == Datum::Bool(true);
                let catalog = hooks.catalog.ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "catalog expression deparsing requires catalog access"
                    )
                })?;
                Ok(catalog
                    .expression_def(node.as_str(), relation, pretty, arena)?
                    .map_or(Datum::Null, Datum::Text))
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
                    TypeMod::Length(n) | TypeMod::BitLength(n) => {
                        stack_format!(80, "{}({})", name, n)
                    }
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
            "to_regclass" | "to_regproc" | "to_regprocedure" | "to_regoper" | "to_regoperator"
            | "to_regtype" | "to_regnamespace" | "to_regrole" | "to_regcollation"
            | "to_regtypemod" => {
                arity(1)?;
                let value = eval_full(args[0], arena, params, row, hooks)?;
                let written = match value {
                    Datum::Text(written) => written,
                    Datum::Bpchar(written) => written.trim_end_matches(' '),
                    Datum::Null => return Ok(Datum::Null),
                    other => return Err(type_mismatch(name, &other)),
                };
                if matches!(name, "to_regtype" | "to_regtypemod")
                    && let Some(identity) =
                        super::super::cross_database_catalog_reference(written, false)
                {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "cross-database references are not implemented: {}",
                        identity
                    ));
                }
                if name == "to_regtypemod" {
                    let (type_name, type_mod) =
                        crate::sql::parser::parse_type_name(written, arena)?;
                    let exists = ColType::from_sql_name(type_name).is_some()
                        || hooks
                            .catalog
                            .is_some_and(|catalog| catalog.user_type_oid(type_name).is_some());
                    return Ok(if exists {
                        Datum::Int4(type_mod)
                    } else {
                        Datum::Null
                    });
                }
                if name == "to_regtype" {
                    if let Ok(oid) = written.parse::<u32>() {
                        return super::super::regtype_of_oid(i64::from(oid), arena);
                    }
                    let (type_name, _) = crate::sql::parser::parse_type_name(written, arena)?;
                    if let Some(catalog) = hooks.catalog
                        && let Some(referenced_oid) = catalog.user_type_oid(type_name)
                    {
                        let rendered = catalog
                            .user_type_name(type_name, arena)?
                            .unwrap_or(type_name);
                        return Ok(Datum::Regtype {
                            referenced_oid,
                            name: rendered,
                        });
                    }
                    return match super::super::regtype_of_name(type_name) {
                        Ok(value) => Ok(value),
                        Err(error) if error.sqlstate == sqlstate::UNDEFINED_OBJECT => {
                            Ok(Datum::Null)
                        }
                        Err(error) => Err(error),
                    };
                }
                let target = match name {
                    "to_regclass" => ColType::Regclass,
                    "to_regproc" => ColType::Regproc,
                    "to_regprocedure" => ColType::Regprocedure,
                    "to_regoper" => ColType::Regoper,
                    "to_regoperator" => ColType::Regoperator,
                    "to_regnamespace" => ColType::Regnamespace,
                    "to_regrole" => ColType::Regrole,
                    "to_regcollation" => ColType::Regcollation,
                    _ => unreachable!(),
                };
                match super::super::regobject_cast(
                    Datum::Text(written),
                    target,
                    hooks.catalog,
                    arena,
                ) {
                    Ok(value) => Ok(value),
                    Err(error)
                        if matches!(
                            error.sqlstate.as_str(),
                            sqlstate::UNDEFINED_OBJECT
                                | sqlstate::UNDEFINED_FUNCTION
                                | sqlstate::UNDEFINED_TABLE
                                | sqlstate::INVALID_SCHEMA_NAME
                                | sqlstate::AMBIGUOUS_FUNCTION
                                | sqlstate::INVALID_NAME
                                | sqlstate::INVALID_TEXT_REPRESENTATION
                                | sqlstate::NUMERIC_OUT_OF_RANGE
                        ) =>
                    {
                        Ok(Datum::Null)
                    }
                    Err(error) => Err(error),
                }
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
                if let crate::sql::ast::Expr::Call { name, .. } = args[0]
                    && matches!(
                        *name,
                        "json_populate_record"
                            | "jsonb_populate_record"
                            | "json_populate_recordset"
                            | "jsonb_populate_recordset"
                    )
                    && let Some(cat) = hooks.catalog
                    && let super::super::ExpressionTypeIdentity::Known(referenced_oid) =
                        super::super::expression_type_identity(args[0], row, hooks)?
                    && let Some(type_name) = cat.type_name(referenced_oid, arena)?
                {
                    return Ok(regtype(referenced_oid, type_name));
                }
                if let crate::sql::ast::Expr::Call {
                    name,
                    args,
                    argument_names,
                    star: false,
                    ..
                } = args[0]
                    && let Some(cat) = hooks.catalog
                {
                    let argument_oids = arena
                        .alloc_slice_with(args.len(), |_| crate::sql::types::oid::UNKNOWN)
                        .map_err(|_| arena_full())?;
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
                    if let Some(referenced_oid) =
                        cat.routine_result_oid(name, argument_names, false, argument_oids)
                        && let Some(type_name) = cat.type_name(referenced_oid, arena)?
                    {
                        return Ok(regtype(referenced_oid, type_name));
                    }
                }
                // A domain declaration can contain an array without being
                // an array of the declared domain.
                if let crate::sql::ast::Expr::Column { qualifier, name } = args[0]
                    && let Some(identity) = row.column_user_type(*qualifier, name)
                    && let Some(ctype) = row.col_type(*qualifier, name)
                    && let Some(cat) = hooks.catalog
                    && let Some(referenced_oid) = cat.declared_user_type_oid(ctype, identity)
                    && let Some(type_name) = cat.type_name(referenced_oid, arena)?
                {
                    return Ok(regtype(referenced_oid, type_name));
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
                                    || matches!((ct, v), (ColType::Void, Datum::Text("")))
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
                    Datum::Xid8(_) => "xid8",
                    Datum::Snapshot { legacy: false, .. } => "pg_snapshot",
                    Datum::Snapshot { legacy: true, .. } => "txid_snapshot",
                    Datum::PgLsn(_) => "pg_lsn",
                    Datum::Money(_) => "money",
                    Datum::Tid(_) => "tid",
                    Datum::Cid(_) => "cid",
                    Datum::Int8(_) => "bigint",
                    Datum::Float4(_) => "real",
                    Datum::Float8(_) => "double precision",
                    Datum::Char(_) => "\"char\"",
                    Datum::Text(_) => "text",
                    Datum::AclItem(_) => "aclitem",
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
                    Datum::Xml(_) => "xml",
                    Datum::JsonPath(_) => "jsonpath",
                    Datum::TsVector(_) => "tsvector",
                    Datum::TsQuery(_) => "tsquery",
                    Datum::Array { element, .. } => element.typeof_name(),
                    Datum::Int2Vector { .. } => "int2vector",
                    Datum::OidVector { .. } => "oidvector",
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
                    Datum::PgNodeTree(_) => "pg_node_tree",
                };
                Ok(regtype(referenced_oid, name))
            }
            _ => unreachable!("dispatch guard admitted an unhandled name"),
        }
    })())
}
