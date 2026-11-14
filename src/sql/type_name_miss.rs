//! The one error for a type name no lookup resolved, in PostgreSQL's
//! `LookupTypeName` order: an explicit schema must exist (`3F000`) before the
//! type is missing from it (`42704`).
//!
//! Callers name the type as written: either the parser's composed spelling
//! (`schema.name`, `schema.name[]`, with `pg_catalog` kept only for a name
//! that is not a built-in type) or a separate schema and name.

use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::util::StackStr;

/// Why a written type name resolved to no type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TypeNameMiss<'a> {
    /// The written schema does not exist.
    Schema(&'a str),
    /// The schema exists (or none was written) but holds no such type.
    Type {
        schema: Option<&'a str>,
        name: &'a str,
    },
}

/// The bytes a skip notice needs: a quoted qualified name and its wording.
pub(crate) const NOTICE_BYTES: usize = 2 * crate::sql::lexer::MAX_IDENTIFIER_BYTES + 64;

impl<'a> TypeNameMiss<'a> {
    /// A miss for the parser's composed spelling of a type name. The schema
    /// is the text before its first `.`, as the type catalog splits it.
    pub(crate) fn of_written(written: &'a str, schema_exists: impl FnOnce(&str) -> bool) -> Self {
        match written.split_once('.') {
            Some((schema, _)) if !schema_exists(schema) => Self::Schema(schema),
            _ => Self::Type {
                schema: None,
                name: written,
            },
        }
    }

    /// A miss for a type named by a separate optional schema and name.
    pub(crate) fn of_qualified(
        schema: Option<&'a str>,
        name: &'a str,
        schema_exists: impl FnOnce(&str) -> bool,
    ) -> Self {
        match schema {
            Some(schema) if !schema_exists(schema) => Self::Schema(schema),
            _ => Self::Type { schema, name },
        }
    }

    /// [`Self::of_written`] against the catalog `storage` shows `txid`.
    pub(crate) fn in_storage(
        storage: &crate::storage::Storage,
        txid: u32,
        written: &'a str,
    ) -> Self {
        Self::of_written(written, |schema| type_schema_exists(storage, txid, schema))
    }

    /// [`Self::of_qualified`] against the catalog `storage` shows `txid`.
    pub(crate) fn qualified_in_storage(
        storage: &crate::storage::Storage,
        txid: u32,
        schema: Option<&'a str>,
        name: &'a str,
    ) -> Self {
        Self::of_qualified(schema, name, |schema| {
            type_schema_exists(storage, txid, schema)
        })
    }

    /// The error PostgreSQL raises for this miss.
    pub(crate) fn error(self) -> SqlError {
        match self {
            Self::Schema(schema) => sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "schema \"{}\" does not exist",
                schema
            ),
            Self::Type {
                schema: Some(schema),
                name,
            } => sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "type \"{}.{}\" does not exist",
                schema,
                name
            ),
            Self::Type { schema: None, name } => sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "type \"{}\" does not exist",
                name
            ),
        }
    }

    /// The error for a routine parameter's type, which PostgreSQL's
    /// `interpret_function_parameter_list` names without quotes.
    pub(crate) fn parameter_error(self) -> SqlError {
        match self {
            Self::Schema(_) => self.error(),
            Self::Type {
                schema: Some(schema),
                name,
            } => sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "type {}.{} does not exist",
                schema,
                name
            ),
            Self::Type { schema: None, name } => {
                sql_err!(sqlstate::UNDEFINED_OBJECT, "type {} does not exist", name)
            }
        }
    }

    /// PostgreSQL's `IF EXISTS` notice for this miss.
    pub(crate) fn skip_notice(self) -> StackStr<NOTICE_BYTES> {
        match self {
            Self::Schema(schema) => crate::stack_format!(
                NOTICE_BYTES,
                "schema \"{}\" does not exist, skipping",
                schema
            ),
            Self::Type {
                schema: Some(schema),
                name,
            } => crate::stack_format!(
                NOTICE_BYTES,
                "type \"{}.{}\" does not exist, skipping",
                schema,
                name
            ),
            Self::Type { schema: None, name } => {
                crate::stack_format!(NOTICE_BYTES, "type \"{}\" does not exist, skipping", name)
            }
        }
    }
}

/// Whether the schema written before a type name exists, as PostgreSQL's
/// `LookupExplicitNamespace` decides: `pg_temp` names the session's
/// temporary schema only once it has one.
pub(crate) fn type_schema_exists(
    storage: &crate::storage::Storage,
    txid: u32,
    schema: &str,
) -> bool {
    if schema == "pg_temp" {
        return crate::sql::catalog::current_temporary_namespace_oid(storage, txid) != 0;
    }
    crate::sql::catalog::schema_oid_by_name(storage, txid, schema).is_some()
}

/// Whether `error` reports a type name that resolved to no type, so the
/// caller positions it at that name.
pub(crate) fn is_type_name_miss(error: &SqlError) -> bool {
    let message = error.message.as_str();
    match error.sqlstate.as_str() {
        sqlstate::UNDEFINED_OBJECT => {
            message.starts_with("type ") && message.ends_with(" does not exist")
        }
        sqlstate::INVALID_SCHEMA_NAME => message.starts_with("schema \""),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::TypeNameMiss;

    fn exists(schema: &str) -> bool {
        matches!(schema, "public" | "pg_catalog")
    }

    #[test]
    fn unknown_schema_precedes_unknown_type() {
        assert_eq!(
            TypeNameMiss::of_written("nope.int4", exists),
            TypeNameMiss::Schema("nope")
        );
        assert_eq!(
            TypeNameMiss::of_written("nope.int4[]", exists),
            TypeNameMiss::Schema("nope")
        );
        let miss = TypeNameMiss::of_written("public.nope[]", exists);
        assert_eq!(
            miss,
            TypeNameMiss::Type {
                schema: None,
                name: "public.nope[]"
            }
        );
        let error = TypeNameMiss::of_written("nope.int4", exists).error();
        assert_eq!(
            (error.sqlstate.as_str(), error.message.as_str()),
            ("3F000", "schema \"nope\" does not exist")
        );
        let error = miss.error();
        assert_eq!(
            (error.sqlstate.as_str(), error.message.as_str()),
            ("42704", "type \"public.nope[]\" does not exist")
        );
        assert!(super::is_type_name_miss(&error));
    }

    #[test]
    fn qualified_miss_names_schema_and_type() {
        let miss = TypeNameMiss::of_qualified(Some("public"), "t", exists);
        assert_eq!(
            miss.skip_notice().as_str(),
            "type \"public.t\" does not exist, skipping"
        );
        assert_eq!(
            TypeNameMiss::of_qualified(Some("nope"), "t", exists)
                .skip_notice()
                .as_str(),
            "schema \"nope\" does not exist, skipping"
        );
        assert_eq!(
            TypeNameMiss::of_qualified(None, "t", exists)
                .error()
                .message
                .as_str(),
            "type \"t\" does not exist"
        );
    }
}
