//! Routine identities written with argument types (`DROP FUNCTION f(integer)`,
//! `ALTER PROCEDURE p`, `GRANT ... ON ROUTINE r(text)`), resolved as
//! PostgreSQL's `LookupFuncWithArgs` does, with its diagnostics.

use super::resolve_routine_type;
use crate::pg::respond::Responder;
use crate::sql::ast::{QualName, RoutineArgumentType, RoutineIdentity, RoutineTargetKind};
use crate::sql::catalog::BuiltinCallableSignature;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::storage::{
    MAX_ROUTINE_ARGUMENTS, PathEntry, RoutineArgumentDef, RoutineKind, RoutineResult, Storage,
};
use crate::util::StackStr;
use core::fmt::Write as _;

/// Why a routine identity names no routine. PostgreSQL's error reports a
/// missing argument type before a missing routine schema, while its skip
/// notice names the routine schema first; the miss keeps both facts.
#[derive(Clone, Copy)]
pub(crate) enum RoutineLookupMiss<'a> {
    Type {
        argument: RoutineArgumentType<'a>,
        routine_schema_missing: bool,
    },
    Schema,
    Routine,
}

/// The routine an identity names: a stored routine or a PostgreSQL built-in.
#[derive(Clone, Copy)]
pub(crate) enum RoutineTarget {
    Stored(usize),
    Builtin(&'static BuiltinCallableSignature),
}

impl RoutineTarget {
    pub(crate) fn kind(self, storage: &Storage, txid: u32) -> RoutineTargetKind {
        match self {
            Self::Stored(slot) => match storage.routine_for(slot, txid).kind {
                RoutineKind::Procedure => RoutineTargetKind::Procedure,
                RoutineKind::Aggregate(_) => RoutineTargetKind::Aggregate,
                _ => RoutineTargetKind::Function,
            },
            Self::Builtin(builtin) => match builtin.kind {
                b'p' => RoutineTargetKind::Procedure,
                b'a' => RoutineTargetKind::Aggregate,
                _ => RoutineTargetKind::Function,
            },
        }
    }

    /// The input argument type OIDs, which identify a routine within its
    /// schema.
    fn argument_oids(self, storage: &Storage, txid: u32) -> ([i32; MAX_ROUTINE_ARGUMENTS], usize) {
        let mut oids = [0; MAX_ROUTINE_ARGUMENTS];
        match self {
            Self::Stored(slot) => {
                let routine = storage.routine_for(slot, txid);
                for (oid, argument) in oids.iter_mut().zip(routine.arguments()) {
                    *oid = argument_oid(storage, txid, argument);
                }
                (oids, routine.argument_count)
            }
            Self::Builtin(builtin) => {
                oids[..builtin.arguments.len()].copy_from_slice(builtin.arguments);
                (oids, builtin.arguments.len())
            }
        }
    }
}

/// An unresolvable stored type matches no built-in argument.
fn argument_oid(storage: &Storage, txid: u32, argument: &RoutineArgumentDef) -> i32 {
    storage
        .routine_type_oid(argument.ctype, argument.user_type, txid)
        .unwrap_or(0)
}

fn schema_missing(storage: &Storage, schema: Option<&str>, txid: u32) -> bool {
    schema.is_some_and(|schema| !storage.namespace_exists(schema, txid))
}

/// Visits the schemas searched for `name` in order: its written schema, or
/// the path's schemas, including the implicit `pg_catalog`. `visit` returns
/// `true` to stop.
fn visit_schemas(
    storage: &Storage,
    name: QualName<'_>,
    txid: u32,
    mut visit: impl FnMut(usize, &str) -> Result<bool, SqlError>,
) -> Result<(), SqlError> {
    if let Some(schema) = name.schema {
        visit(0, schema)?;
        return Ok(());
    }
    let path = storage.path();
    let mut rank = 0;
    for entry in path.entries() {
        let user_schema;
        let schema = match entry {
            PathEntry::Schema(slot) => {
                user_schema = storage.schema_def_for(usize::from(*slot), txid).name;
                user_schema.as_str()
            }
            PathEntry::Catalog => "pg_catalog",
            PathEntry::InformationSchema => "information_schema",
            PathEntry::Toast => continue,
        };
        if visit(rank, schema)? {
            break;
        }
        rank += 1;
    }
    Ok(())
}

/// Every routine named `name` in `schema`.
fn visit_named(
    storage: &Storage,
    txid: u32,
    schema: &str,
    name: &str,
    mut visit: impl FnMut(RoutineTarget) -> Result<bool, SqlError>,
) -> Result<bool, SqlError> {
    for slot in storage.routine_slots_named(schema, name, txid) {
        if visit(RoutineTarget::Stored(slot))? {
            return Ok(true);
        }
    }
    for builtin in crate::sql::catalog::builtin_callable_signatures(name) {
        if builtin.schema == schema && visit(RoutineTarget::Builtin(builtin))? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The routine `schema.name` with exactly these input argument types.
fn routine_with_arguments(
    storage: &Storage,
    txid: u32,
    schema: &str,
    name: &str,
    oids: &[i32],
) -> Result<Option<RoutineTarget>, SqlError> {
    let mut found = None;
    visit_named(storage, txid, schema, name, |target| {
        let (candidate, count) = target.argument_oids(storage, txid);
        if candidate[..count] == *oids {
            found = Some(target);
        }
        Ok(found.is_some())
    })?;
    Ok(found)
}

/// Resolves `identity` as a `kind` target. `Ok(Err(miss))` means nothing by
/// that identity exists; ambiguity and a routine of the wrong kind are errors.
pub(crate) fn lookup_routine<'a>(
    storage: &Storage,
    txid: u32,
    kind: RoutineTargetKind,
    identity: &RoutineIdentity<'a>,
) -> Result<Result<RoutineTarget, RoutineLookupMiss<'a>>, SqlError> {
    let routine_schema_missing = schema_missing(storage, identity.name.schema, txid);
    let arguments = match resolve_arguments(storage, txid, identity.argument_types)? {
        Ok(arguments) => arguments,
        Err(argument) => {
            return Ok(Err(RoutineLookupMiss::Type {
                argument,
                routine_schema_missing,
            }));
        }
    };
    if routine_schema_missing {
        return Ok(Err(RoutineLookupMiss::Schema));
    }
    let mut oids = [0; MAX_ROUTINE_ARGUMENTS];
    for (oid, argument) in oids
        .iter_mut()
        .zip(&arguments[..identity.argument_types.len()])
    {
        *oid = argument_oid(storage, txid, argument);
    }
    let oids = &oids[..identity.argument_types.len()];
    let name = identity.name.name;
    let mut found = None;
    if identity.signature_is_explicit {
        visit_schemas(storage, identity.name, txid, |_, schema| {
            found = routine_with_arguments(storage, txid, schema, name, oids)?;
            Ok(found.is_some())
        })?;
    } else {
        // PostgreSQL's omitted-argument form: every routine of this name on
        // the path is a candidate unless an earlier schema has one with the
        // same arguments. FUNCTION and AGGREGATE skip procedures; PROCEDURE
        // takes only procedures.
        visit_schemas(storage, identity.name, txid, |rank, schema| {
            visit_named(storage, txid, schema, name, |candidate| {
                let (candidate_oids, count) = candidate.argument_oids(storage, txid);
                let mut shadowed = false;
                visit_schemas(storage, identity.name, txid, |earlier, schema| {
                    if earlier >= rank {
                        return Ok(true);
                    }
                    shadowed = routine_with_arguments(
                        storage,
                        txid,
                        schema,
                        name,
                        &candidate_oids[..count],
                    )?
                    .is_some();
                    Ok(shadowed)
                })?;
                let candidate_kind = candidate.kind(storage, txid);
                let accepted = match kind {
                    RoutineTargetKind::Function | RoutineTargetKind::Aggregate => {
                        candidate_kind != RoutineTargetKind::Procedure
                    }
                    RoutineTargetKind::Procedure => candidate_kind == RoutineTargetKind::Procedure,
                    RoutineTargetKind::Either => true,
                };
                if shadowed || !accepted {
                    return Ok(false);
                }
                if found.replace(candidate).is_some() {
                    crate::sql::eval::stash_diagnostic(
                        None,
                        Some(crate::stack_format!(
                            128,
                            "Specify the argument list to select the {} unambiguously.",
                            kind.noun()
                        )),
                    );
                    return Err(sql_err!(
                        sqlstate::AMBIGUOUS_FUNCTION,
                        "{} name \"{}\" is not unique",
                        kind.noun(),
                        QualifiedName(identity.name)
                    ));
                }
                Ok(false)
            })?;
            Ok(false)
        })?;
    }
    let Some(target) = found else {
        return Ok(Err(RoutineLookupMiss::Routine));
    };
    let actual = target.kind(storage, txid);
    let wrong_kind = match kind {
        RoutineTargetKind::Function => actual == RoutineTargetKind::Procedure,
        RoutineTargetKind::Procedure | RoutineTargetKind::Aggregate => actual != kind,
        RoutineTargetKind::Either => false,
    };
    if wrong_kind {
        let (prefix, suffix) = match kind {
            RoutineTargetKind::Procedure => ("", " is not a procedure"),
            RoutineTargetKind::Aggregate => ("function ", " is not an aggregate"),
            _ => ("", " is not a function"),
        };
        return Err(signature_error(
            storage,
            txid,
            sqlstate::WRONG_OBJECT_TYPE,
            prefix,
            identity,
            suffix,
        ));
    }
    Ok(Ok(target))
}

/// PostgreSQL's error for dropping a built-in, which names it as
/// `format_procedure` does: `function name(type,type)`.
pub(crate) fn builtin_drop_error(builtin: &BuiltinCallableSignature) -> SqlError {
    crate::sql::error_position::full_message_error(
        sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
        |text| {
            let _ = write!(text, "cannot drop function {}(", builtin.name);
            for (index, oid) in builtin.arguments.iter().enumerate() {
                if index != 0 {
                    let _ = text.write_char(',');
                }
                let Some(name) = crate::sql::catalog::intrinsic_type_name(*oid) else {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "built-in argument type {} has no name",
                        oid
                    ));
                };
                let _ = text.write_str(name);
            }
            let _ = text.write_str(") because it is required by the database system");
            Ok(())
        },
    )
}

/// pos3ql cannot change a built-in's catalog entry.
pub(crate) fn builtin_change_error(
    storage: &Storage,
    txid: u32,
    identity: &RoutineIdentity<'_>,
) -> SqlError {
    signature_error(
        storage,
        txid,
        sqlstate::FEATURE_NOT_SUPPORTED,
        "built-in function ",
        identity,
        " cannot be changed",
    )
}

/// Resolves the argument types, or names the first one that does not exist.
#[allow(clippy::type_complexity)]
fn resolve_arguments<'a>(
    storage: &Storage,
    txid: u32,
    argument_types: &[RoutineArgumentType<'a>],
) -> Result<Result<[RoutineArgumentDef; MAX_ROUTINE_ARGUMENTS], RoutineArgumentType<'a>>, SqlError>
{
    if argument_types.len() > MAX_ROUTINE_ARGUMENTS {
        return Err(sql_err!(
            sqlstate::TOO_MANY_ARGUMENTS,
            "functions cannot have more than {} arguments",
            MAX_ROUTINE_ARGUMENTS
        ));
    }
    let mut arguments = [RoutineArgumentDef::EMPTY; MAX_ROUTINE_ARGUMENTS];
    for (slot, argument) in argument_types.iter().enumerate() {
        match resolve_routine_type(storage, txid, argument.name) {
            Ok(resolved) => {
                arguments[slot].ctype = resolved.ctype;
                arguments[slot].user_type = resolved.user_type;
            }
            Err(error) if error.sqlstate == sqlstate::UNDEFINED_OBJECT => {
                return Ok(Err(*argument));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(Ok(arguments))
}

impl RoutineLookupMiss<'_> {
    /// The error PostgreSQL raises for this miss without `IF EXISTS`.
    pub(crate) fn error(
        self,
        storage: &Storage,
        txid: u32,
        kind: RoutineTargetKind,
        identity: &RoutineIdentity<'_>,
    ) -> SqlError {
        match self {
            Self::Type { argument, .. } => match argument.schema {
                Some(schema) if schema_missing(storage, Some(schema), txid) => sql_err!(
                    sqlstate::INVALID_SCHEMA_NAME,
                    "schema \"{}\" does not exist",
                    schema
                ),
                _ => sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "type \"{}\" does not exist",
                    argument.written
                ),
            },
            Self::Schema => sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "schema \"{}\" does not exist",
                identity.name.schema.unwrap_or_default()
            ),
            Self::Routine if !identity.signature_is_explicit => sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "could not find {} named \"{}\"",
                match kind {
                    RoutineTargetKind::Procedure => "a procedure",
                    RoutineTargetKind::Aggregate => "an aggregate",
                    _ => "a function",
                },
                QualifiedName(identity.name)
            ),
            Self::Routine
                if kind == RoutineTargetKind::Aggregate && identity.argument_types.is_empty() =>
            {
                sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "aggregate {}(*) does not exist",
                    QualifiedName(identity.name)
                )
            }
            Self::Routine => signature_error(
                storage,
                txid,
                sqlstate::UNDEFINED_FUNCTION,
                match kind {
                    RoutineTargetKind::Procedure => "procedure ",
                    RoutineTargetKind::Aggregate => "aggregate ",
                    _ => "function ",
                },
                identity,
                " does not exist",
            ),
        }
    }

    /// Sends PostgreSQL's `IF EXISTS` skip notice for this miss.
    pub(crate) fn notice(
        self,
        storage: &Storage,
        txid: u32,
        kind: RoutineTargetKind,
        identity: &RoutineIdentity<'_>,
        responder: &mut Responder,
    ) -> Result<(), crate::pg::wire::WireFull> {
        const NOTICE_BYTES: usize = 160 + MAX_ROUTINE_ARGUMENTS * 266;
        let mut notice = StackStr::<NOTICE_BYTES>::new();
        let _ = match self {
            Self::Schema
            | Self::Type {
                routine_schema_missing: true,
                ..
            } => write!(
                notice,
                "schema \"{}\" does not exist, skipping",
                identity.name.schema.unwrap_or_default()
            ),
            Self::Type { argument, .. } => match argument.schema {
                Some(schema) if schema_missing(storage, Some(schema), txid) => {
                    write!(notice, "schema \"{}\" does not exist, skipping", schema)
                }
                _ => write!(
                    notice,
                    "type \"{}\" does not exist, skipping",
                    argument.written
                ),
            },
            Self::Routine => {
                let _ = write!(notice, "{} {}(", kind.noun(), QualifiedName(identity.name));
                for (index, argument) in identity.argument_types.iter().enumerate() {
                    if index != 0 {
                        let _ = notice.write_char(',');
                    }
                    let _ = notice.write_str(argument.written);
                }
                notice.write_str(") does not exist, skipping")
            }
        };
        debug_assert!(!notice.is_truncated());
        responder.notice("00000", notice.as_str())
    }
}

/// PostgreSQL's `NameListToString`: the written name, dotted, never quoted.
pub(super) struct QualifiedName<'a>(pub(super) QualName<'a>);

impl core::fmt::Display for QualifiedName<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(schema) = self.0.schema {
            write!(f, "{}.", schema)?;
        }
        f.write_str(self.0.name)
    }
}

/// `prefix name(type, ...) suffix` with PostgreSQL's `func_signature_string`
/// spelling of each argument type (`format_type_be`).
fn signature_error(
    storage: &Storage,
    txid: u32,
    state: &'static str,
    prefix: &str,
    identity: &RoutineIdentity<'_>,
    suffix: &str,
) -> SqlError {
    crate::sql::error_position::full_message_error(state, |message| {
        let _ = write!(message, "{}{}(", prefix, QualifiedName(identity.name));
        for (index, argument) in identity.argument_types.iter().enumerate() {
            if index != 0 {
                let _ = message.write_str(", ");
            }
            let resolved = resolve_routine_type(storage, txid, argument.name)?;
            let _ = message.write_str(format_routine_type(storage, txid, resolved)?.as_str());
        }
        let _ = write!(message, "){}", suffix);
        Ok(())
    })
}

fn format_routine_type(
    storage: &Storage,
    txid: u32,
    resolved: RoutineResult,
) -> Result<StackStr<260>, SqlError> {
    let unavailable = || {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "routine argument type name is unavailable"
        )
    };
    let oid = storage
        .routine_type_oid(resolved.ctype, resolved.user_type, txid)
        .ok_or_else(unavailable)?;
    if let Some(name) = crate::sql::catalog::user_type_name_fixed(storage, txid, oid)? {
        return Ok(name);
    }
    crate::sql::catalog::intrinsic_type_name(oid)
        .map(StackStr::from_str)
        .ok_or_else(unavailable)
}
