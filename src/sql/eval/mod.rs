//! Expression evaluation with PostgreSQL semantics: three-valued logic,
//! NULL propagation through operators, checked integer arithmetic
//! (overflow is an error, not a wrap), and division by zero as SQLSTATE
//! 22012 for integers and floats alike.

use crate::mem::arena::Arena;
use crate::stack_format;
use crate::util::StackStr;

use super::ast::{BinaryOp, Collation, Expr, UnaryOp};
use super::numeric::Numeric;
use super::types::{ArrElem, ColType, Datum};

mod cast;
pub mod funcs;
pub use cast::{cast, cast_to, fit_bits, int_to_bits};
pub(crate) use cast::{
    cast_to_text, parse_bytea, parse_int_bounded, parse_int_literal, parse_uuid, validate_bits,
};

mod args;
mod operators;
pub(crate) use args::*;

mod pattern;
pub(crate) use pattern::regex_split;
pub use pattern::{like_match, regex_split_pub, regexp_flags};
pub(crate) use pattern::{regex_substring, similar_to_posix, sql_regex_substring};

pub(crate) use operators::arithmetic;
pub(crate) use operators::coerce_unknown as coerce_unknown_pub;
pub(crate) use operators::{binary, coerce_unknown, membership_eq};
pub use operators::{
    compare_datums, compare_datums_collated, compare_datums_with_catalog, hash_key,
    hash_key_collated,
};
use operators::{compare_text_collated, logic, range_mismatch, unary};

/// DETAIL/HINT lines for the next emitted error or notice. `SqlError` is
/// constructed at ~60 sites and stays two fields; the rare errors that carry
/// PostgreSQL DETAIL/HINT (DROP ... CASCADE dependency reports) stash them
/// here, and the wire responder consumes them with the next diagnostic it
/// writes. The engine is single-threaded per process; the responder clears
/// the slot on every emission, so a stale detail cannot outlive its error.
pub const MAX_DIAGNOSTIC_DETAIL_BYTES: usize = 64 * 192;

#[derive(Clone, Copy)]
pub struct Diagnostic {
    pub detail: StackStr<MAX_DIAGNOSTIC_DETAIL_BYTES>,
    pub hint: Option<StackStr<128>>,
}

std::thread_local! {
    static PENDING_DIAGNOSTIC: core::cell::RefCell<Option<Diagnostic>> =
        const { core::cell::RefCell::new(None) };
}

pub fn stash_diagnostic(
    detail: StackStr<MAX_DIAGNOSTIC_DETAIL_BYTES>,
    hint: Option<StackStr<128>>,
) {
    PENDING_DIAGNOSTIC.with(|d| *d.borrow_mut() = Some(Diagnostic { detail, hint }));
}

pub fn take_diagnostic() -> Option<Diagnostic> {
    PENDING_DIAGNOSTIC.with(|d| d.borrow_mut().take())
}

/// A PostgreSQL SQLSTATE that has already passed the protocol's five-byte
/// grammar. Keeping this separate from arbitrary text prevents a dynamic
/// diagnostic from reaching a wire response or handler unvalidated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlState(StackStr<5>);

impl SqlState {
    /// Constructs a code used by the engine itself. Constants are audited at
    /// the call site, while client-provided values must use [`Self::parse`].
    pub(crate) fn known(code: &'static str) -> Self {
        assert!(
            Self::is_valid(code),
            "engine SQLSTATE constants must be valid"
        );
        Self(StackStr::from_str(code))
    }

    /// Parses the PostgreSQL SQLSTATE grammar: five upper-case ASCII letters
    /// or digits.
    pub fn parse(code: &str) -> Option<Self> {
        Self::is_valid(code).then(|| Self(StackStr::from_str(code)))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn starts_with(self, prefix: &str) -> bool {
        self.0.as_str().starts_with(prefix)
    }

    pub fn is_successful_completion(self) -> bool {
        self == sqlstate::SUCCESSFUL_COMPLETION
    }

    fn is_valid(code: &str) -> bool {
        code.len() == 5
            && code
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    }
}

impl AsRef<str> for SqlState {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl core::fmt::Display for SqlState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl PartialEq<&str> for SqlState {
    fn eq(&self, other: &&str) -> bool {
        self.0.as_str() == *other
    }
}

#[derive(Debug)]
pub struct SqlError {
    /// Five-character SQLSTATE per PostgreSQL's errcodes table.
    pub sqlstate: SqlState,
    pub message: StackStr<192>,
}

#[macro_export]
macro_rules! sql_err {
    ($state:expr, $($arg:tt)*) => {
        $crate::sql::eval::SqlError {
            sqlstate: $crate::sql::eval::SqlState::known($state),
            message: $crate::stack_format!(192, $($arg)*),
        }
    };
}

pub mod sqlstate {
    pub const SYNTAX_ERROR: &str = "42601";
    pub const INVALID_NAME: &str = "42602";
    pub const UNDEFINED_COLUMN: &str = "42703";
    pub const UNDEFINED_TABLE: &str = "42P01";
    pub const DUPLICATE_TABLE: &str = "42P07";
    pub const DUPLICATE_SCHEMA: &str = "42P06";
    pub const INSUFFICIENT_PRIVILEGE: &str = "42501";
    pub const INVALID_GRANT_OPERATION: &str = "0LP01";
    pub const OBJECT_IN_USE: &str = "55006";
    pub const WARNING_PRIVILEGE_NOT_GRANTED: &str = "01007";
    pub const RESERVED_NAME: &str = "42939";
    pub const INVALID_SCHEMA_DEFINITION: &str = "42P15";
    pub const DUPLICATE_CURSOR: &str = "42P03";
    pub const DUPLICATE_FUNCTION: &str = "42723";
    pub const UNDEFINED_CURSOR: &str = "34000";
    pub const OBJECT_NOT_IN_PREREQUISITE_STATE: &str = "55000";
    pub const AMBIGUOUS_ALIAS: &str = "42P09";
    pub const INVALID_SCHEMA_NAME: &str = "3F000";
    pub const DEPENDENT_OBJECTS_STILL_EXIST: &str = "2BP01";
    pub const UNDEFINED_OBJECT: &str = "42704";
    pub const DATATYPE_MISMATCH: &str = "42804";
    pub const CANNOT_COERCE: &str = "42846";
    pub const DIVISION_BY_ZERO: &str = "22012";
    pub const NUMERIC_OUT_OF_RANGE: &str = "22003";
    pub const INVALID_TEXT_REPRESENTATION: &str = "22P02";
    pub const INVALID_BINARY_REPRESENTATION: &str = "22P03";
    pub const NOT_NULL_VIOLATION: &str = "23502";
    pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
    pub const PROGRAM_LIMIT_EXCEEDED: &str = "54000";
    pub const SEQUENCE_GENERATOR_LIMIT_EXCEEDED: &str = "2200H";
    pub const PROTOCOL_VIOLATION: &str = "08P01";
    pub const TOO_MANY_CONNECTIONS: &str = "53300";
    pub const INVALID_PARAMETER_VALUE: &str = "22023";
    pub const UNDEFINED_FUNCTION: &str = "42883";
    pub const DATA_EXCEPTION: &str = "22000";
    pub const STRING_DATA_RIGHT_TRUNCATION: &str = "22001";
    pub const NULL_VALUE_NOT_ALLOWED: &str = "22004";
    pub const INVALID_DATETIME_FORMAT: &str = "22007";
    pub const DATETIME_FIELD_OVERFLOW: &str = "22008";
    pub const SUBSTRING_ERROR: &str = "22011";
    pub const INTERVAL_FIELD_OVERFLOW: &str = "22015";
    pub const INVALID_REGULAR_EXPRESSION: &str = "2201B";
    pub const INVALID_ARGUMENT_FOR_LOG: &str = "2201E";
    pub const INVALID_ARGUMENT_FOR_POWER_FUNCTION: &str = "2201F";
    pub const INVALID_ARGUMENT_FOR_WIDTH_BUCKET: &str = "2201G";
    pub const INVALID_ROW_COUNT_IN_RESULT_OFFSET: &str = "2201X";
    pub const CHARACTER_NOT_IN_REPERTOIRE: &str = "22021";
    pub const BAD_COPY_FILE_FORMAT: &str = "22P04";
    pub const INVALID_ESCAPE_SEQUENCE: &str = "22025";
    pub const STRING_DATA_LENGTH_MISMATCH: &str = "22026";
    pub const ARRAY_SUBSCRIPT_ERROR: &str = "2202E";
    pub const IN_FAILED_SQL_TRANSACTION: &str = "25P02";
    pub const INVALID_SQL_STATEMENT_NAME: &str = "26000";
    pub const DUPLICATE_COLUMN: &str = "42701";
    pub const DUPLICATE_ALIAS: &str = "42712";
    pub const DUPLICATE_OBJECT: &str = "42710";
    pub const GROUPING_ERROR: &str = "42803";
    pub const WRONG_OBJECT_TYPE: &str = "42809";
    pub const INVALID_COLUMN_REFERENCE: &str = "42P10";
    pub const COLLATION_MISMATCH: &str = "42P21";
    pub const INDETERMINATE_COLLATION: &str = "42P22";
    pub const INVALID_FUNCTION_DEFINITION: &str = "42P13";
    pub const WINDOWING_ERROR: &str = "42P20";
    pub const OUT_OF_MEMORY: &str = "53200";
    pub const STATEMENT_TOO_COMPLEX: &str = "54001";
    pub const TOO_MANY_COLUMNS: &str = "54011";
    pub const TOO_MANY_ARGUMENTS: &str = "54023";
    pub const QUERY_CANCELED: &str = "57014";
    pub const IO_ERROR: &str = "58030";
    pub const INTERNAL_ERROR: &str = "XX000";
    pub const ACTIVE_SQL_TRANSACTION: &str = "25001";
    pub const READ_ONLY_SQL_TRANSACTION: &str = "25006";
    pub const AMBIGUOUS_COLUMN: &str = "42702";
    pub const AMBIGUOUS_FUNCTION: &str = "42725";
    pub const CANT_CHANGE_RUNTIME_PARAM: &str = "55P02";
    pub const CARDINALITY_VIOLATION: &str = "21000";
    pub const CHECK_VIOLATION: &str = "23514";
    pub const EXCLUSION_VIOLATION: &str = "23P01";
    pub const DUPLICATE_PREPARED_STATEMENT: &str = "42P05";
    pub const FOREIGN_KEY_VIOLATION: &str = "23503";
    pub const INVALID_FOREIGN_KEY: &str = "42830";
    pub const INVALID_PRECEDING_OR_FOLLOWING_SIZE: &str = "22013";
    pub const INVALID_RECURSION: &str = "42P19";
    pub const INVALID_ROW_COUNT_IN_LIMIT_CLAUSE: &str = "2201W";
    pub const INVALID_SAVEPOINT_SPECIFICATION: &str = "3B001";
    pub const INVALID_TABLE_DEFINITION: &str = "42P16";
    pub const INVALID_OBJECT_DEFINITION: &str = "42P17";
    pub const GENERATED_ALWAYS: &str = "428C9";
    pub const INVALID_USE_OF_ESCAPE_CHARACTER: &str = "2200C";
    pub const LOCK_NOT_AVAILABLE: &str = "55P03";
    pub const DEADLOCK_DETECTED: &str = "40P01";
    /// Private control-flow sentinel. The wire layer must park the connection
    /// and must never serialize this as an ErrorResponse.
    pub(crate) const INTERNAL_LOCK_WAIT: &str = "PZ001";
    /// A non-blocking block fetch is in progress; the statement must park and
    /// retry when the reactor completes the fetch.
    pub(crate) const INTERNAL_IO_WAIT: &str = "PZ002";
    /// Evaluation yielded a scalar routine which must run after the caller
    /// releases its immutable storage borrow.
    pub(crate) const INTERNAL_ROUTINE_INVOCATION: &str = "PZ003";
    pub const NAME_TOO_LONG: &str = "42622";
    pub const NO_ACTIVE_SQL_TRANSACTION: &str = "25P01";
    pub const SERIALIZATION_FAILURE: &str = "40001";
    pub const SUCCESSFUL_COMPLETION: &str = "00000";
    pub const WARNING: &str = "01000";
    pub const UNIQUE_VIOLATION: &str = "23505";
    pub const RAISE_EXCEPTION: &str = "P0001";
    pub const NO_DATA_FOUND: &str = "P0002";
    pub const TOO_MANY_ROWS: &str = "P0003";
    pub const ASSERT_FAILURE: &str = "P0004";
    pub const STACKED_DIAGNOSTICS_ACCESSED_WITHOUT_ACTIVE_HANDLER: &str = "0Z002";
}

/// Resolves column references during evaluation. Statements without a FROM
/// clause use [`NoColumns`].
/// Whether two column references resolve to the same scope column — the
/// semantic equality PostgreSQL uses for grouping keys, where `a` and `t.a`
/// are one key. False whenever either side is not a column or the lookup
/// cannot resolve identities.
fn same_resolved_column<'a>(row: &impl ColumnLookup<'a>, a: &Expr, b: &Expr) -> bool {
    let (
        Expr::Column {
            qualifier: qa,
            name: na,
        },
        Expr::Column {
            qualifier: qb,
            name: nb,
        },
    ) = (a, b)
    else {
        return false;
    };
    match (row.column_identity(*qa, na), row.column_identity(*qb, nb)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

pub trait ColumnLookup<'a> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError>;

    /// The scope-resolved identity of a column reference, when this lookup can
    /// name one — used to match a grouping key spelled `a` against a select
    /// item spelled `t.a` (PostgreSQL matches grouping keys semantically, not
    /// by spelling). The pair is (table index, column index), with a
    /// USING/NATURAL-merged column encoded as (u32::MAX, merge index).
    fn column_identity(&self, _qualifier: Option<&str>, _name: &str) -> Option<(u32, u32)> {
        None
    }

    /// The named table's row as record fields (name + type + value), or None
    /// for an outer-join null row. Used to build a `Datum::Record` for a
    /// whole-row reference; contexts without join rows reject it.
    fn whole_row_fields(
        &self,
        table: &str,
        _arena: &'a Arena,
    ) -> Result<Option<&'a [super::types::RecordField<'a>]>, SqlError> {
        Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "whole-row reference to \"{}\" is not supported in this context",
            table
        ))
    }

    /// A whole-row reference (`t.*` as a value): Ok(true) when the row is
    /// present, Ok(false) when it is an outer-join null row. Contexts without
    /// join rows reject it.
    fn whole_row_present(&self, table: &str) -> Result<bool, SqlError> {
        Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "whole-row reference to \"{}\" is not supported in this context",
            table
        ))
    }

    /// Static column type, if known — used to unify CASE branch types so a
    /// column reference contributes its declared type. Defaults to unknown.
    fn col_type(&self, _qualifier: Option<&str>, _name: &str) -> Option<ColType> {
        None
    }

    /// Declared collation of a resolved column.  The default makes synthetic
    /// scalar contexts explicitly non-collatable instead of guessing from the
    /// datum after expression identity has already been erased.
    fn collation(&self, _qualifier: Option<&str>, _name: &str) -> crate::sql::ast::Collation {
        crate::sql::ast::Collation::None
    }

    fn record_field_collation(&self, _base: &Expr<'a>, _field: &str) -> crate::sql::ast::Collation {
        crate::sql::ast::Collation::None
    }

    /// The stable user-defined type identity of a bare column, if any.
    fn column_user_type(
        &self,
        _qualifier: Option<&str>,
        _name: &str,
    ) -> Option<crate::storage::UserTypeName> {
        None
    }

    /// Whether a whole-row reference to `table` is a scalar (a
    /// set-returning-function scan's single output column) rather than a record.
    /// Defaults to false.
    fn whole_row_is_scalar(&self, _table: &str) -> bool {
        false
    }
}

/// Static type identity retained until a context decides how PostgreSQL
/// resolves an unconstrained expression.
#[derive(Clone, Copy)]
pub(crate) enum ExpressionTypeIdentity {
    Known(i32),
    Unresolved,
}

impl ExpressionTypeIdentity {
    pub(crate) const fn record_field_oid(self) -> i32 {
        match self {
            Self::Known(oid) => oid,
            Self::Unresolved => super::types::oid::UNKNOWN,
        }
    }

    pub(crate) fn routine_argument_oid(self, value: &Datum<'_>) -> i32 {
        match self {
            Self::Known(oid) => oid,
            // An unconstrained literal or parameter acquires its call-site
            // type from the value produced for overload resolution.
            Self::Unresolved => value.type_oid(),
        }
    }
}

fn catalog_user_type_oid<'a>(
    identity: crate::storage::UserTypeName,
    is_array: bool,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<i32, SqlError> {
    hooks
        .catalog
        .and_then(|catalog| catalog.user_type_identity_oid(identity, is_array))
        .ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "evaluated user-defined type has no catalog identity"
            )
        })
}

pub(crate) fn expression_type_identity<'a>(
    expression: &Expr<'a>,
    row: &dyn ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<ExpressionTypeIdentity, SqlError> {
    if let Expr::Collate { operand, .. } = expression {
        return expression_type_identity(operand, row, hooks);
    }
    if let Expr::Column { qualifier, name } = expression
        && let Some(identity) = row.column_user_type(*qualifier, name)
    {
        return catalog_user_type_oid(
            identity,
            matches!(
                row.col_type(*qualifier, name),
                Some(super::types::ColType::Array(_))
            ),
            hooks,
        )
        .map(ExpressionTypeIdentity::Known);
    }
    if let Expr::Cast { type_name, .. } = expression {
        if let Some(ctype) = super::types::ColType::from_sql_name(type_name) {
            return Ok(ExpressionTypeIdentity::Known(ctype.oid()));
        }
        return hooks
            .catalog
            .and_then(|catalog| catalog.user_type_oid(type_name))
            .map(ExpressionTypeIdentity::Known)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "evaluated user-type cast has no catalog identity"
                )
            });
    }
    if let Expr::Array(elements) = expression
        && let Some(first) = elements.first()
    {
        if let Expr::Cast { type_name, .. } = first
            && super::types::ColType::from_sql_name(type_name).is_none()
        {
            let array_name = stack_format!(128, "{}[]", type_name);
            return hooks
                .catalog
                .and_then(|catalog| catalog.user_type_oid(array_name.as_str()))
                .map(ExpressionTypeIdentity::Known)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "evaluated user-type array has no catalog identity"
                    )
                });
        }
        if let Expr::Column { qualifier, name } = first
            && let Some(identity) = row.column_user_type(*qualifier, name)
        {
            return catalog_user_type_oid(identity, true, hooks).map(ExpressionTypeIdentity::Known);
        }
    }
    if let Expr::Call { name, args, .. } = expression
        && args.len() <= crate::storage::MAX_ROUTINE_ARGUMENTS
        && let Some(catalog) = hooks.catalog
    {
        let mut argument_oids = [super::types::oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut resolved = true;
        for (index, argument) in args.iter().enumerate() {
            match expression_type_identity(argument, row, hooks)? {
                ExpressionTypeIdentity::Known(oid) => argument_oids[index] = oid,
                ExpressionTypeIdentity::Unresolved => resolved = false,
            }
        }
        if resolved
            && let Some(result_oid) = catalog.routine_result_oid(name, &argument_oids[..args.len()])
        {
            return Ok(ExpressionTypeIdentity::Known(result_oid));
        }
    }
    match crate::sql::exec::infer_type_res(expression, &RuntimeColumnTypes(row))?.0 {
        super::types::oid::UNKNOWN => Ok(ExpressionTypeIdentity::Unresolved),
        oid => Ok(ExpressionTypeIdentity::Known(oid)),
    }
}

struct RuntimeColumnTypes<'row, 'datum>(&'row dyn ColumnLookup<'datum>);

impl crate::sql::exec::ColTypeResolver for RuntimeColumnTypes<'_, '_> {
    fn resolve(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<super::types::ColType, SqlError> {
        self.0.col_type(qualifier, name).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_COLUMN,
                "column \"{}\" does not exist",
                name
            )
        })
    }
}

/// A reference to a lookup is itself a lookup, so `&dyn ColumnLookup` can be
/// passed to the generic `eval`/`where_passes` helpers.
impl<'a, T: ColumnLookup<'a> + ?Sized> ColumnLookup<'a> for &T {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        (**self).lookup(qualifier, name)
    }

    fn whole_row_present(&self, table: &str) -> Result<bool, SqlError> {
        (**self).whole_row_present(table)
    }

    fn whole_row_fields(
        &self,
        table: &str,
        arena: &'a Arena,
    ) -> Result<Option<&'a [super::types::RecordField<'a>]>, SqlError> {
        (**self).whole_row_fields(table, arena)
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        (**self).col_type(qualifier, name)
    }

    fn collation(&self, qualifier: Option<&str>, name: &str) -> crate::sql::ast::Collation {
        (**self).collation(qualifier, name)
    }

    fn whole_row_is_scalar(&self, table: &str) -> bool {
        (**self).whole_row_is_scalar(table)
    }

    fn column_identity(&self, qualifier: Option<&str>, name: &str) -> Option<(u32, u32)> {
        (**self).column_identity(qualifier, name)
    }

    fn column_user_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Option<crate::storage::UserTypeName> {
        (**self).column_user_type(qualifier, name)
    }
}

/// Whether a qualifier answers to one concrete table: its bare name, or the
/// composed `schema.table` a three-part reference resolves through.
pub fn qualifier_answers_single(def: &crate::storage::TableDef, q: &str) -> bool {
    match q.split_once('.') {
        None => q == def.name.as_str(),
        Some((schema, table)) => schema == def.schema.as_str() && table == def.name.as_str(),
    }
}

/// A DML correlation name replaces its target relation name everywhere that
/// statement can name the target.
pub fn qualifier_answers_target(
    def: &crate::storage::TableDef,
    alias: Option<&str>,
    q: &str,
) -> bool {
    match alias {
        Some(alias) => alias.eq_ignore_ascii_case(q),
        None => qualifier_answers_single(def, q),
    }
}

pub struct NoColumns;

impl<'a> ColumnLookup<'a> for NoColumns {
    fn lookup(&self, _qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        Err(sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "column \"{}\" does not exist",
            name
        ))
    }
}

/// No bound parameters (simple queries).
pub const NO_PARAMS: &[Datum<'static>] = &[];

/// Values injected into evaluation by the grouping/aggregation machinery
/// and by pre-evaluated subqueries, matched by AST equality (group keys)
/// or node identity (aggregates, subqueries).
#[derive(Clone, Copy)]
pub struct EvalHooks<'h, 'a> {
    /// (group-by expressions, this group's key values, active-column bitmask).
    /// The bitmask selects which `group_by` columns participate in the current
    /// grouping set (all bits set for a plain `GROUP BY`); it drives `GROUPING()`.
    pub group: Option<(&'h [&'h Expr<'h>], &'h [Datum<'a>], u64)>,
    /// (aggregate-call nodes by address, this group's results).
    pub aggs: Option<(&'h [*const Expr<'h>], &'h [Datum<'a>])>,
    /// (subquery nodes by address, their pre-evaluated results).
    pub subs: Option<&'h SubqueryValues<'h, 'a>>,
    /// (window-function call nodes by address, the current row's values).
    pub windows: Option<(&'h [*const Expr<'h>], &'h [Datum<'a>])>,
    /// Resolves catalog OIDs to reconstructed definition text for
    /// `pg_get_indexdef` (psql `\d`). A trait object so evaluation stays
    /// decoupled from `Storage`; `None` outside catalog-backed queries. Its
    /// generic method keeps `EvalHooks` variance unchanged.
    pub catalog: Option<&'h dyn CatalogAccess>,
    /// The current 1-based expansion index of a set-returning function
    /// (`_pg_expandarray`) in the projection; `None` outside such expansion.
    pub srf_index: Option<usize>,
    /// Materialized catalog SRFs keyed by their expression node. Built-in SRFs
    /// compute by index; SQL routines retain their one execution here.
    pub project_sets: Option<&'h [ProjectSetValue<'a>]>,
    /// Sequence side-effects for `nextval`/`currval`/`lastval`/`setval`. `None`
    /// in contexts where a sequence function cannot appear (catalog synthesis,
    /// constraint checks); the volatile functions error `0A000`-style if called
    /// without it. A trait object with interior mutability so evaluation stays
    /// `&`-only while advancing the generator.
    pub sequences: Option<&'h dyn SequenceAccess>,
}

#[derive(Clone, Copy)]
pub struct ProjectSetValue<'a> {
    pub node: *const (),
    pub values: &'a [Datum<'a>],
    /// Nested project-set levels select their input independently of the
    /// outer level currently being expanded.
    pub fixed_index: Option<usize>,
}

/// The side-effecting sequence functions, abstracted so `eval` need not depend
/// on `Storage`. The implementor advances/reads generators (through `Cell`
/// interior mutability) and the session's `currval`/`lastval` state.
pub trait SequenceAccess {
    fn nextval(&self, name: &str) -> Result<i64, SqlError>;
    fn currval(&self, name: &str) -> Result<i64, SqlError>;
    fn lastval(&self) -> Result<i64, SqlError>;
    fn setval(&self, name: &str, value: i64, is_called: bool) -> Result<i64, SqlError>;
    fn dry_nextval(&self, name: &str) -> Result<i64, SqlError>;
    fn dry_currval(&self, name: &str) -> Result<i64, SqlError>;
    fn dry_lastval(&self) -> Result<i64, SqlError>;
    fn dry_setval(&self, name: &str, value: i64, is_called: bool) -> Result<i64, SqlError>;
    /// Captures and restores the logical position when a bounded physical
    /// executor pass repeats the same expression stream.
    fn statement_cursor(&self) -> Option<usize> {
        None
    }
    fn restore_statement_cursor(&self, _cursor: usize) {}
}

/// Reconstructs catalog definition text (index / constraint DDL) that psql's
/// `\d` obtains through functions like `pg_get_indexdef`. Implemented over
/// `Storage`; abstract here so `eval` need not depend on the catalog.
pub trait CatalogAccess {
    /// Materializes a durable named-composite row value into its catalog field
    /// layout. The default is a loud capability error: raw composite text is
    /// never treated as an anonymous record.
    fn materialize_composite<'a>(
        &self,
        _slot: u16,
        _physical_fields: u8,
        _text: &'a str,
        _arena: &'a Arena,
    ) -> Result<Datum<'a>, SqlError> {
        Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "named composite catalog access is unavailable"
        ))
    }
    /// Compares text with a resolved database collation. A query executor must
    /// supply this for the database default; an evaluator without a catalog
    /// cannot silently substitute a process-global locale.
    fn compare_text(
        &self,
        _collation: Collation,
        _left: &str,
        _right: &str,
    ) -> Result<core::cmp::Ordering, SqlError> {
        Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "database collation comparator is unavailable"
        ))
    }
    /// Executes a catalog-resolved scalar SQL routine. `None` means this
    /// catalog has no matching overload, preserving the normal undefined-
    /// function diagnostic at the shared call choke point.
    fn call_routine<'a>(
        &self,
        _name: &str,
        _arguments: &[Datum<'a>],
        _argument_type_oids: &[i32],
        _arena: &'a Arena,
    ) -> Result<Option<Datum<'a>>, SqlError> {
        Ok(None)
    }
    /// The declared result identity of a catalog routine. This is separate
    /// from execution because a domain result shares its base datum layout.
    fn routine_result_oid(&self, _name: &str, _argument_type_oids: &[i32]) -> Option<i32> {
        None
    }
    fn sequence_state_by_oid(&self, _oid: i32) -> Option<(i64, bool)> {
        None
    }
    fn routine_invocation_cursor(&self) -> Option<usize> {
        None
    }
    fn restore_routine_invocation_cursor(&self, _cursor: usize) {}
    /// Whether this OID names a relation visible to the current query.
    fn relation_is_visible(&self, oid: i32) -> Option<bool>;
    /// Whether this OID names a type visible to the current query.
    fn type_is_visible(&self, oid: i32) -> Option<bool>;
    /// Whether this OID names a function visible to the current query.
    fn function_is_visible(&self, oid: i32) -> Option<bool>;
    /// The canonical SQL definition for a function OID, if this catalog owns it.
    fn function_def<'a>(&self, _oid: i32, _arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        Ok(None)
    }
    fn function_arguments<'a>(
        &self,
        _oid: i32,
        _identity: bool,
        _arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        Ok(None)
    }
    fn function_result<'a>(
        &self,
        _oid: i32,
        _arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        Ok(None)
    }
    /// Whether this OID names a collation visible to the current query.
    fn collation_is_visible(&self, oid: i32) -> Option<bool>;
    fn tablespace_location<'a>(
        &self,
        _oid: i32,
        _arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        Ok(None)
    }
    /// Whether this OID names a relation eligible for a publication.
    fn relation_is_publishable(&self, oid: i32) -> Option<bool>;
    /// The index definition for this OID: `col == 0` gives the whole
    /// `btree (col, ...)` form; `col > 0` gives the name of that 1-based indexed
    /// column. `None` if no such index is known.
    fn index_def<'a>(
        &self,
        oid: i32,
        col: usize,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError>;
    /// The `FOREIGN KEY (...) REFERENCES ...` definition of the constraint with
    /// this OID, or `None` if no such foreign-key constraint is known.
    fn constraint_def<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError>;
    /// The executable `RANGE|LIST|HASH (...)` clause for a partitioned table.
    fn partition_key_def<'a>(
        &self,
        oid: i32,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError>;
    /// The relation name for an OID, for rendering `oid::regclass`.
    fn relname<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError>;
    /// The OID of the relation named `name`, for `'relname'::regclass`.
    fn reloid(&self, name: &str) -> Option<i32>;
    /// Resolve a role OID to its catalog name.
    fn role_name<'a>(&self, _oid: i32, _arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        Ok(None)
    }
    /// Resolve a role name to its catalog OID.
    fn role_oid(&self, _name: &str) -> Option<i32> {
        None
    }
    /// Resolve a namespace OID to its catalog name.
    fn schema_name<'a>(&self, _oid: i32, _arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        Ok(None)
    }
    /// Resolve a namespace name to its catalog OID.
    fn schema_oid(&self, _name: &str) -> Option<i32> {
        None
    }
    /// Resolve a routine OID to its catalog spelling. `signature` selects the
    /// regprocedure spelling with argument types rather than regproc's name.
    fn routine_name<'a>(
        &self,
        _oid: i32,
        _signature: bool,
        _arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        Ok(None)
    }
    /// Resolve a routine catalog spelling to its OID.
    fn routine_oid(&self, _name: &str, _signature: bool) -> Result<Option<i32>, SqlError> {
        Ok(None)
    }
    /// Resolve an operator OID to its catalog spelling. `signature` selects
    /// regoperator's argument-bearing spelling rather than regoper's name.
    fn operator_name<'a>(
        &self,
        _oid: i32,
        _signature: bool,
        _arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        Ok(None)
    }
    /// Resolve an operator catalog spelling to its OID.
    fn operator_oid(&self, _name: &str, _signature: bool) -> Result<Option<i32>, SqlError> {
        Ok(None)
    }
    /// PostgreSQL privilege inquiry functions. `None` represents a missing
    /// object or role, for which PostgreSQL returns NULL in the OID forms.
    fn has_table_privilege(
        &self,
        _role: Option<&str>,
        _relation: &str,
        _privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        Ok(None)
    }
    fn has_sequence_privilege(
        &self,
        _role: Option<&str>,
        _sequence: &str,
        _privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        Ok(None)
    }
    fn has_schema_privilege(
        &self,
        _role: Option<&str>,
        _schema: &str,
        _privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        Ok(None)
    }
    fn has_type_privilege(
        &self,
        _role: Option<&str>,
        _type_name: &str,
        _privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        Ok(None)
    }
    fn has_function_privilege(
        &self,
        _role: Option<&str>,
        _function: &str,
        _privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        Ok(None)
    }
    fn has_database_privilege(
        &self,
        _role: Option<&str>,
        _privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        Ok(None)
    }
    /// The comment text on the object with this OID and column `subid` (0 for
    /// the object itself), or `None`. `catalog_name` selects the owning
    /// catalog (`pg_class`, `pg_namespace`, or `pg_type`). Backs
    /// `obj_description`/`col_description`.
    fn comment<'a>(
        &self,
        catalog_name: &str,
        oid: i32,
        subid: i32,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError>;
    /// SQL spelling of a user-defined type OID for `format_type`.
    fn type_name<'a>(&self, _oid: i32, _arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        Ok(None)
    }
    /// Stored SELECT text of a view or materialized view OID.
    fn view_def<'a>(&self, _oid: i32, _arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        Ok(None)
    }
    /// Bytes occupied by the relation's stored row images. Relations without
    /// physical row storage (plain views and catalog-only indexes) report zero.
    fn relation_size(&self, _oid: i32) -> Result<Option<i64>, SqlError> {
        Ok(None)
    }
    /// Total bytes occupied by all stored row images in the current database.
    fn database_size(&self) -> Result<i64, SqlError> {
        Ok(0)
    }
    /// The name of the enum type in catalog `slot`, for `pg_typeof` on an enum
    /// value. `None` if the slot holds no live enum.
    fn enum_name<'a>(&self, _slot: u16, _arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        Ok(None)
    }
    /// The sort key of `label` in the enum type at `slot`, resolving a text
    /// literal against an enum in a comparison. `None` if the slot holds no
    /// live enum or the label is not a member.
    fn enum_label_sort(&self, _slot: u16, _label: &str) -> Option<f64> {
        None
    }
    /// The catalog slot of the (possibly schema-qualified) enum type named
    /// `type_name`, or `None` if no such live enum is visible — used to
    /// resolve `value::enumtype` casts, which base-type name lookup cannot.
    fn enum_slot_of_name(&self, _type_name: &str) -> Option<u16> {
        None
    }
    /// Casts to a catalog-defined domain, enum, or its automatically-created
    /// array type. `Ok(None)` means the name is not a visible user-defined
    /// type; `Some` contains the fully coerced and validated value.
    fn cast_user_type<'a>(
        &self,
        _type_name: &str,
        _value: Datum<'a>,
        _arena: &'a Arena,
    ) -> Result<Option<Datum<'a>>, SqlError> {
        Ok(None)
    }
    /// An array element identity for a domain whose base is itself an array.
    /// This is distinct from PostgreSQL's ordinary multidimensional constructor:
    /// `ARRAY[array_domain_value]` is an array of domain values, not one
    /// flattened base array.
    fn array_domain_element(&self, _type_name: &str) -> Option<crate::sql::types::ArrElem> {
        None
    }
    /// The SQL name of a user-defined array element identity, for
    /// `pg_typeof`. Built-in arrays do not consult the catalog.
    fn user_array_name<'a>(
        &self,
        _element: crate::sql::types::ArrElem,
        _arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        Ok(None)
    }
    /// Canonical visible name of a user-defined scalar or array type spelling.
    fn user_type_name<'a>(
        &self,
        _type_name: &str,
        _arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        Ok(None)
    }
    /// Resolves the exact OID of a visible user-defined scalar or array type.
    fn user_type_oid(&self, _type_name: &str) -> Option<i32> {
        None
    }
    /// Resolves a durable schema-qualified identity without reparsing it as
    /// SQL text.
    fn user_type_identity_oid(
        &self,
        _identity: crate::storage::UserTypeName,
        _array: bool,
    ) -> Option<i32> {
        None
    }
}

/// A bounded membership source for an `IN (subquery)` result.
///
/// Small/local results use the inline `values` slice on [`SubqueryList`].
/// Durable execution may instead keep the encoded result in an immutable
/// object-backed run and implement this probe by streaming that run. Keeping
/// the seam here prevents the expression evaluator from knowing about block
/// stores, cache tiers, or provider adapters.
pub trait SubqueryListProbe: 'static {
    fn is_empty(&self) -> bool;

    /// Returns `(matched, saw_unknown)`. `value` has already been coerced to
    /// the subquery column's type.
    fn probe<'a>(
        &self,
        value: Datum<'a>,
        collations: &[Collation],
        catalog: Option<&dyn CatalogAccess>,
        arena: &'a Arena,
    ) -> Result<(bool, bool), SqlError>;

    fn quantify<'a>(
        &self,
        value: Datum<'a>,
        operator: BinaryOp,
        all: bool,
        collations: &[Collation],
        catalog: Option<&dyn CatalogAccess>,
        arena: &'a Arena,
    ) -> Result<Datum<'a>, SqlError>;
}

/// One pre-evaluated `IN (subquery)` result.
#[derive(Clone, Copy)]
pub struct SubqueryList<'a> {
    pub node: *const (),
    pub values: &'a [Datum<'a>],
    /// Statement-arena pointer to a probe whose concrete type is `'static`.
    /// The pointed allocation is valid for the same lifetime as `values`.
    pub probe: Option<core::ptr::NonNull<dyn SubqueryListProbe>>,
    pub saw_null: bool,
    pub witness: Datum<'a>,
    pub collations: &'a [Collation],
}

/// Pre-evaluated (uncorrelated) subquery results.
pub struct SubqueryValues<'h, 'a> {
    /// Scalar subqueries: (node address, value, type-witness datum — the
    /// result column's type even when the value is NULL, for describes).
    pub scalars: &'h [(*const Expr<'h>, Datum<'a>, Datum<'a>)],
    /// IN-subqueries: (node address, member list, saw a NULL member, a
    /// type-witness datum of the subquery's result column). The witness lets
    /// the operand be coerced to the column type even when the set is empty or
    /// all-NULL, matching PostgreSQL (which type-checks `x IN (...)` regardless
    /// of contents).
    pub lists: &'h [SubqueryList<'a>],
}

pub const NO_HOOKS: EvalHooks<'static, 'static> = EvalHooks {
    group: None,
    aggs: None,
    subs: None,
    windows: None,
    catalog: None,
    srf_index: None,
    project_sets: None,
    sequences: None,
};

pub fn eval<'a>(
    expression: &Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
) -> Result<Datum<'a>, SqlError> {
    eval_full(expression, arena, params, row, &NO_HOOKS)
}

/// Surfaces errors from every maximal constant subexpression, as
/// PostgreSQL's plan-time constant folding does: `SELECT 1/0` and
/// `... OR 0.0/0.0 > 1` error even when no row would reach them. Constant
/// subtrees are evaluated once here; per-row evaluation (with short-circuit)
/// handles the rest.
pub fn check_constant_errors<'a>(expression: &Expr<'a>, arena: &'a Arena) -> Result<(), SqlError> {
    fold_check(expression, arena).map(|_| ())
}

/// The simplification-aware core of [`check_constant_errors`], mirroring
/// PostgreSQL's `eval_const_expressions`: it folds constant subexpressions
/// (surfacing their errors) but simplifies `A AND FALSE`→`FALSE`,
/// `A OR TRUE`→`TRUE`, and constant `CASE` arms — so a constant error inside a
/// branch that simplification *drops* is not surfaced (PostgreSQL evaluates
/// `... WHERE FALSE AND (id > (-1 % 0))` to no rows, never folding `-1 % 0`).
/// Returns the folded boolean value when the expression provably reduces to
/// one, else `None`.
fn fold_check<'a>(expression: &Expr<'a>, arena: &'a Arena) -> Result<Option<bool>, SqlError> {
    use super::ast::BinaryOp;
    if expression.is_constant() {
        // A fully-constant subtree folds eagerly; its error surfaces here.
        return Ok(match eval(expression, arena, NO_PARAMS, &NoColumns) {
            Ok(Datum::Bool(b)) => Some(b),
            Ok(_) => None,
            // Catalog-resolved values cannot fold without the catalog this
            // plan-time check intentionally does not carry. Runtime resolves
            // and validates them with the query catalog.
            Err(e)
                if e.sqlstate == sqlstate::UNDEFINED_OBJECT
                    || e.sqlstate == sqlstate::UNDEFINED_FUNCTION
                    || e.sqlstate == sqlstate::FEATURE_NOT_SUPPORTED =>
            {
                None
            }
            Err(e) => return Err(e),
        });
    }
    match expression {
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::NumericLit(_)
        | Expr::Str(_)
        | Expr::BitLit(_)
        | Expr::Column { .. }
        | Expr::RoutineParam { .. }
        | Expr::WholeRow(_)
        | Expr::SchemaColumn { .. }
        | Expr::Param(_)
        | Expr::DefaultMarker => Ok(None),
        // Boolean connectives short-circuit like PostgreSQL's folding: a FALSE
        // (AND) / TRUE (OR) operand settles the result and drops the sibling,
        // so the sibling's constant errors are never surfaced.
        Expr::Binary {
            operator: BinaryOp::And,
            left,
            right,
        } => {
            // FALSE settles AND; otherwise the result is known only when both
            // sides fold to TRUE (`TRUE AND TRUE` = TRUE).
            let l = fold_check(left, arena)?;
            if l == Some(false) {
                return Ok(Some(false));
            }
            let r = fold_check(right, arena)?;
            if r == Some(false) {
                return Ok(Some(false));
            }
            Ok(match (l, r) {
                (Some(true), Some(true)) => Some(true),
                _ => None,
            })
        }
        Expr::Binary {
            operator: BinaryOp::Or,
            left,
            right,
        } => {
            // TRUE settles OR; otherwise the result is known only when both
            // sides fold to FALSE (`FALSE OR FALSE` = FALSE) — so a constant
            // OR of dead predicates lets a CASE arm drop.
            let l = fold_check(left, arena)?;
            if l == Some(true) {
                return Ok(Some(true));
            }
            let r = fold_check(right, arena)?;
            if r == Some(true) {
                return Ok(Some(true));
            }
            Ok(match (l, r) {
                (Some(false), Some(false)) => Some(false),
                _ => None,
            })
        }
        // NOT propagates a folded boolean, so `NOT (x AND FALSE)` simplifies to
        // TRUE — which lets a CASE truncate exactly as PostgreSQL's plan-time
        // simplification does.
        Expr::Unary {
            operator: super::ast::UnaryOp::Not,
            operand,
        } => Ok(fold_check(operand, arena)?.map(|b| !b)),
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::Collate { operand, .. }
        | Expr::IsNull { operand, .. } => {
            fold_check(operand, arena)?;
            Ok(None)
        }
        Expr::Binary { left, right, .. } => {
            fold_check(left, arena)?;
            fold_check(right, arena)?;
            Ok(None)
        }
        Expr::InList { operand, list, .. } => {
            fold_check(operand, arena)?;
            for e in *list {
                fold_check(e, arena)?;
            }
            Ok(None)
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            fold_check(operand, arena)?;
            fold_check(low, arena)?;
            fold_check(high, arena)?;
            Ok(None)
        }
        Expr::Like {
            operand, pattern, ..
        }
        | Expr::Match {
            operand, pattern, ..
        } => {
            fold_check(operand, arena)?;
            fold_check(pattern, arena)?;
            Ok(None)
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
            ..
        } => {
            if let Some(o) = operand {
                // Operand form (`CASE x WHEN v ...`): the WHENs are compared to
                // x, not boolean conditions, so no arm is dropped by folding.
                fold_check(o, arena)?;
                for (c, r) in *whens {
                    fold_check(c, arena)?;
                    fold_check(r, arena)?;
                }
            } else {
                // Searched form: a constant-FALSE WHEN drops its THEN; a
                // constant-TRUE WHEN makes the CASE that THEN and drops the
                // rest — matching PostgreSQL, so a division in a dead arm
                // (`WHEN 'a' LIKE 'b' THEN 2/0`) is never folded.
                for (c, r) in *whens {
                    match fold_check(c, arena)? {
                        Some(false) => continue,
                        Some(true) => {
                            fold_check(r, arena)?;
                            return Ok(None);
                        }
                        None => {
                            fold_check(r, arena)?;
                        }
                    }
                }
            }
            if let Some(e) = otherwise {
                fold_check(e, arena)?;
            }
            Ok(None)
        }
        Expr::Call { args, .. } => {
            for a in *args {
                fold_check(a, arena)?;
            }
            Ok(None)
        }
        Expr::Subquery(_)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. }
        | Expr::Exists(_)
        | Expr::ArraySubquery(_) => Ok(None),
        Expr::Array(items) => {
            for e in *items {
                fold_check(e, arena)?;
            }
            Ok(None)
        }
        Expr::Subscript { base, index } => {
            fold_check(base, arena)?;
            fold_check(index, arena)?;
            Ok(None)
        }
        Expr::Slice { base, lower, upper } => {
            fold_check(base, arena)?;
            if let Some(e) = lower {
                fold_check(e, arena)?;
            }
            if let Some(e) = upper {
                fold_check(e, arena)?;
            }
            Ok(None)
        }
        Expr::Field { base, .. } => {
            fold_check(base, arena)?;
            Ok(None)
        }
        Expr::AnyAll { operand, array, .. } => {
            fold_check(operand, arena)?;
            fold_check(array, arena)?;
            Ok(None)
        }
    }
}

/// The `ESCAPE` operand of a LIKE or SIMILAR TO pattern. PostgreSQL takes one
/// character, or the empty string to mean no escaping at all, and refuses
/// anything longer.
pub(crate) fn escape_char(d: Datum<'_>) -> Result<Option<char>, SqlError> {
    let d = text_view(d);
    let Datum::Text(s) = d else {
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "ESCAPE requires a text operand, not {}",
            type_name_of(&d)
        ));
    };
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (None, _) => Ok(None),
        (Some(c), None) => Ok(Some(c)),
        _ => Err(sql_err!(
            sqlstate::INVALID_ESCAPE_SEQUENCE,
            "invalid escape string"
        )),
    }
}

pub fn eval_full<'a>(
    expression: &Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    if let Some(set) = hooks.project_sets.and_then(|sets| {
        sets.iter()
            .find(|set| set.node == expression as *const Expr<'a> as *const ())
    }) && let Some(index) = set.fixed_index.or(hooks.srf_index)
    {
        return Ok(set.values.get(index - 1).copied().unwrap_or(Datum::Null));
    }
    // GROUPING(arg, ...): each argument contributes one bit (1 if that column
    // is NOT part of the current grouping set), most significant first.
    if let Expr::Call { name, args, .. } = expression
        && name.eq_ignore_ascii_case("grouping")
    {
        let Some((exprs, _, mask)) = hooks.group else {
            return Err(sql_err!(
                sqlstate::GROUPING_ERROR,
                "GROUPING must be used with grouping sets or GROUP BY"
            ));
        };
        let mut result = 0i32;
        for arg in args.iter() {
            let idx = exprs
                .iter()
                .position(|g| **g == **arg || same_resolved_column(row, g, arg))
                .ok_or_else(|| {
                sql_err!(sqlstate::GROUPING_ERROR, "arguments to GROUPING must be grouping expressions of the associated query level")
            })?;
            let grouped = mask & (1u64 << idx) != 0;
            result = (result << 1) | i32::from(!grouped);
        }
        return Ok(Datum::Int4(result));
    }
    // Group-key substitution: any expression equal to a GROUP BY key
    // evaluates to the group's value. Column references match by resolved
    // identity too, so `t.a` finds the key spelled `a`.
    if let Some((exprs, values, _mask)) = hooks.group {
        for (g, v) in exprs.iter().zip(values) {
            if **g == *expression || same_resolved_column(row, g, expression) {
                return Ok(*v);
            }
        }
    }
    match *expression {
        Expr::Null => Ok(Datum::Null),
        // A whole-row value: NULL for an outer-join null row, else a non-null
        // Preserve the table field layout for composite evaluation and output.
        Expr::WholeRow(table) => match row.whole_row_fields(table, arena)? {
            // A function scan's whole row is its single scalar column.
            Some(fields) if row.whole_row_is_scalar(table) => {
                Ok(fields.first().map(|f| f.value).unwrap_or(Datum::Null))
            }
            Some(fields) => Ok(Datum::Record(fields)),
            None => Ok(Datum::Null), // outer-join null row
        },
        Expr::Bool(b) => Ok(Datum::Bool(b)),
        Expr::Int(v) => Ok(if let Ok(small) = i32::try_from(v) {
            Datum::Int4(small)
        } else {
            Datum::Int8(v)
        }),
        Expr::Float(v) => Ok(Datum::Float8(v)),
        Expr::NumericLit(s) => Ok(Datum::Numeric(Numeric::parse(s, arena)?)),
        Expr::Str(s) => Ok(Datum::Text(s)),
        Expr::BitLit(s) => Ok(Datum::Bit {
            bits: s,
            varying: false,
        }),
        Expr::Column { qualifier, name } => match row.lookup(qualifier, name) {
            Ok(v) => materialize_named_composite(v, hooks, arena),
            // A bare name that is not a column but names a FROM item is a
            // whole-row reference (`SELECT t FROM t`, `row_to_json(r)`).
            Err(e) if qualifier.is_none() && e.sqlstate == sqlstate::UNDEFINED_COLUMN => {
                match row.whole_row_fields(name, arena) {
                    Ok(Some(fields)) if row.whole_row_is_scalar(name) => {
                        Ok(fields.first().map(|f| f.value).unwrap_or(Datum::Null))
                    }
                    Ok(Some(fields)) => Ok(Datum::Record(fields)),
                    Ok(None) => Ok(Datum::Null),
                    Err(_) => Err(e),
                }
            }
            Err(e) => Err(e),
        },
        Expr::RoutineParam {
            qualifier,
            name,
            index,
        } => match row.lookup(qualifier, name) {
            Ok(value) => materialize_named_composite(value, hooks, arena),
            Err(error)
                if error.sqlstate == sqlstate::UNDEFINED_COLUMN
                    || error.sqlstate == sqlstate::UNDEFINED_TABLE =>
            {
                params.get(index as usize - 1).copied().ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "there is no parameter ${}",
                        index
                    )
                })
            }
            Err(error) => Err(error),
        },
        Expr::SchemaColumn {
            schema,
            table,
            name,
        } => {
            // A three-part reference resolves through a composed
            // `schema.table` qualifier: only an unaliased FROM entry whose
            // base table really lives in that schema answers to it, exactly
            // as PostgreSQL binds these — and it disambiguates two
            // same-named tables from different schemas.
            let composed = arena
                .alloc_str(crate::stack_format!(130, "{}.{}", schema, table).as_str())
                .map_err(|_| arena_full())?;
            materialize_named_composite(row.lookup(Some(composed), name)?, hooks, arena)
        }
        Expr::Param(n) => params.get(n as usize - 1).copied().ok_or_else(|| {
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "there is no parameter ${}",
                n
            )
        }),
        Expr::Unary { operator, operand } => {
            // The prefix arithmetic operators compute exactly what their
            // functions do, so they run the same code rather than a second copy.
            if let Some(function) = operator.arithmetic_function() {
                return call(function, &[operand], false, arena, params, row, hooks);
            }
            let v = eval_full(operand, arena, params, row, hooks)?;
            unary(operator, v, arena)
        }
        Expr::Binary {
            operator: BinaryOp::And,
            left,
            right,
        } => {
            // PostgreSQL simplifies `x AND FALSE` to FALSE and short-circuits a
            // scan qual in a cost order that is not fixed, so a FALSE operand
            // determines the result even when the *other* operand would error at
            // runtime. Match that: a definite FALSE on either side yields FALSE
            // and absorbs the sibling's runtime error. A constant erroring
            // operand still errors — `check_constant_errors` surfaces it before
            // we get here, so anything that reaches this point is per-row.
            eval_logic_short_circuit(BinaryOp::And, left, right, arena, params, row, hooks)
        }
        Expr::Binary {
            operator: BinaryOp::Or,
            left,
            right,
        } => {
            // Dual of AND: a definite TRUE on either side yields TRUE and
            // absorbs the sibling's runtime error (PostgreSQL's `x OR TRUE`).
            eval_logic_short_circuit(BinaryOp::Or, left, right, arena, params, row, hooks)
        }
        Expr::Binary {
            operator,
            left,
            right,
        } => {
            let comparison_collation = if matches!(
                operator,
                BinaryOp::Eq
                    | BinaryOp::NotEq
                    | BinaryOp::Lt
                    | BinaryOp::LtEq
                    | BinaryOp::Gt
                    | BinaryOp::GtEq
            ) {
                Some(resolve_comparison_collation(left, right, row)?)
            } else {
                None
            };
            // A column reference materializes its stored composite layout, but
            // a cast or binary Bind parameter can still carry the same value
            // as `CompositeText`. Normalize both operands at this comparison
            // choke point so historical physical layouts never acquire a
            // second equality or index-key semantics.
            let l = materialize_named_composite(
                eval_full(left, arena, params, row, hooks)?,
                hooks,
                arena,
            )?;
            let r = materialize_named_composite(
                eval_full(right, arena, params, row, hooks)?,
                hooks,
                arena,
            )?;
            // `array || NULL` resolution depends on the NULL operand's static
            // type, which the datum has lost — resolve it here where the
            // expression is still available.
            if operator == BinaryOp::Concat
                && let Some(d) = array_null_concat(l, r, left, right, row, arena)?
            {
                return Ok(d);
            }
            // Track which side is an "unknown" literal (a string literal or a
            // parameter): only those coerce to the other operand's type, as
            // PostgreSQL does. A real text value never coerces to a number.
            let l_unknown = is_unknown_literal(left);
            let r_unknown = is_unknown_literal(right);
            // An enum operand meeting an unknown text literal resolves the
            // literal to a member of the enum's type (the generic coercion has
            // no catalog to look up labels). A non-member is 22P02.
            let (l, r) = coerce_enum_literal(l, r, l_unknown, r_unknown, hooks, arena)?;
            if let Some(collation) = comparison_collation
                && matches!(
                    (&l, &r),
                    (
                        Datum::Text(_) | Datum::Bpchar(_),
                        Datum::Text(_) | Datum::Bpchar(_)
                    )
                )
            {
                return compare_text_collated(
                    operator,
                    l,
                    r,
                    l_unknown,
                    r_unknown,
                    collation,
                    hooks.catalog,
                );
            }
            binary(operator, l, r, l_unknown, r_unknown, arena)
        }
        Expr::Cast {
            operand,
            type_name,
            type_mod,
        } => {
            let mut v = eval_full(operand, arena, params, row, hooks)?;
            let catalog_text = matches!(v, Datum::CompositeText { .. })
                || matches!(
                    v,
                    Datum::Array { element, .. }
                        if matches!(element.to_coltype(), ColType::Composite(_))
                );
            if matches!(
                ColType::from_sql_name(type_name),
                Some(ColType::Text | ColType::Varchar | ColType::Bpchar | ColType::Name)
            ) && catalog_text
            {
                let catalog = hooks.catalog.ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "named composite catalog access is unavailable"
                    )
                })?;
                v = materialize_composite_text_output(v, catalog, arena)?;
            }
            if let Some(target) = ColType::from_sql_name(type_name)
                && target.is_reg_object()
            {
                return regobject_cast(v, target, hooks.catalog, arena);
            }
            if let Some(ColType::Array(element)) = ColType::from_sql_name(type_name)
                && element.is_catalog_reference()
            {
                return reg_array_cast(v, element, hooks.catalog, arena);
            }
            // `::regtype` resolves a type name to the type and renders its
            // canonical SQL name (`'varchar(5)'::regtype` is `character
            // varying`); an OID renders the type it names, an unknown OID
            // renders as the number and OID 0 as `-`, as PostgreSQL has it.
            if type_name.eq_ignore_ascii_case("regtype") {
                match text_view(v) {
                    Datum::Text(name) => {
                        // A known enum type name renders as itself (like a base
                        // type's regtype), rather than erroring as "unknown".
                        if let Some(cat) = hooks.catalog
                            && cat.enum_slot_of_name(name.trim()).is_some()
                        {
                            return Ok(Datum::Text(
                                arena.alloc_str(name.trim()).map_err(|_| arena_full())?,
                            ));
                        }
                        return regtype_of_name(name);
                    }
                    Datum::Int4(x) => return regtype_of_oid(x as i64, arena),
                    Datum::Oid(x) => return regtype_of_oid(i64::from(x), arena),
                    Datum::Int8(x) => return regtype_of_oid(x, arena),
                    _ => {}
                }
            }
            // integer -> bit(n): the low n bits, right-aligned. This is
            // PostgreSQL's int-to-bit conversion, distinct from bit-string
            // length coercion (which left-aligns), so it is handled here where
            // the source type is known.
            if let Some(ct @ ColType::Bit { varying }) = ColType::from_sql_name(type_name)
                && matches!(v, Datum::Int4(_) | Datum::Oid(_) | Datum::Int8(_))
            {
                let n = match crate::sql::types::TypeMod::decode(ct, type_mod) {
                    crate::sql::types::TypeMod::Length(n) => n,
                    _ => 1,
                };
                let value = match v {
                    Datum::Int2(x) => x as u16 as u64,
                    Datum::Int4(x) => x as u32 as u64,
                    Datum::Oid(x) => u64::from(x),
                    Datum::Int8(x) => x as u64,
                    _ => unreachable!(),
                };
                return Ok(Datum::Bit {
                    bits: int_to_bits(value, n, arena)?,
                    varying,
                });
            }
            // Base-type lookup deliberately has no catalog dependency. Route
            // any other type spelling through the one catalog-aware cast hook:
            // domains, enums, and their automatically-created array types all
            // resolve and validate there.
            if ColType::from_sql_name(type_name).is_none()
                && let Some(cat) = hooks.catalog
                && let Some(value) = cat.cast_user_type(type_name, v, arena)?
            {
                return Ok(value);
            }
            let v = cast(v, type_name, arena)?;
            // `::numeric(p,s)` / `::varchar(n)`: enforce the modifier on the
            // cast result exactly as a column of that type would.
            if type_mod != -1
                && let Some(ct) = ColType::from_sql_name(type_name)
            {
                return super::exec::apply_cast_typmod(v, ct, type_mod, arena);
            }
            Ok(v)
        }
        Expr::Collate { operand, .. } => eval_full(operand, arena, params, row, hooks),
        Expr::IsNull { operand, negated } => {
            let v = eval_full(operand, arena, params, row, hooks)?;
            // A row `IS NULL` is true only when *every* field is null, and
            // `IS NOT NULL` only when every field is non-null — so a mixed row
            // is false for both (PostgreSQL's row null-test, not a plain
            // negation).
            if let Datum::Record(fields) = v {
                let result = if negated {
                    fields.iter().all(|f| !f.value.is_null())
                } else {
                    fields.iter().all(|f| f.value.is_null())
                };
                return Ok(Datum::Bool(result));
            }
            Ok(Datum::Bool(v.is_null() != negated))
        }
        Expr::Call {
            name,
            args,
            star,
            distinct,
            over,
            ..
        } => {
            // A window-function call resolves to this row's precomputed value.
            if over.is_some()
                && let Some((nodes, values)) = hooks.windows
            {
                for (node, v) in nodes.iter().zip(values) {
                    if core::ptr::eq(*node, expression as *const _) {
                        return Ok(*v);
                    }
                }
            }
            if let Some((nodes, values)) = hooks.aggs {
                for (node, v) in nodes.iter().zip(values) {
                    if core::ptr::eq(*node, expression as *const _) {
                        return Ok(*v);
                    }
                }
            }
            if distinct {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "DISTINCT is only supported inside aggregate functions"
                ));
            }
            call(name, args, star, arena, params, row, hooks)
        }
        Expr::InList {
            operand,
            list,
            negated,
        } => {
            let v = eval_full(operand, arena, params, row, hooks)?;
            if v.is_null() {
                return Ok(Datum::Null);
            }
            // SQL semantics: x IN (..) with no match but a NULL member is
            // NULL, not false.
            let mut saw_null = false;
            for item in list {
                let member = eval_full(item, arena, params, row, hooks)?;
                if member.is_null() {
                    saw_null = true;
                    continue;
                }
                let l = coerce_unknown(v, &member)?;
                let r = coerce_unknown(member, &l)?;
                match (&l, &r) {
                    (Datum::Text(_) | Datum::Bpchar(_), Datum::Text(_) | Datum::Bpchar(_)) => {
                        let collation = resolve_comparison_collation(operand, item, row)?;
                        if compare_datums_with_catalog(collation, hooks.catalog, &l, &r)?.is_eq() {
                            return Ok(Datum::Bool(!negated));
                        }
                    }
                    _ => match membership_eq(&l, &r)? {
                        Some(true) => return Ok(Datum::Bool(!negated)),
                        Some(false) => {}
                        None => saw_null = true,
                    },
                }
            }
            Ok(if saw_null {
                Datum::Null
            } else {
                Datum::Bool(negated)
            })
        }
        Expr::Between {
            operand,
            low,
            high,
            negated,
        } => {
            let v = eval_full(operand, arena, params, row, hooks)?;
            let lo = eval_full(low, arena, params, row, hooks)?;
            let hi = eval_full(high, arena, params, row, hooks)?;
            if v.is_null() || lo.is_null() || hi.is_null() {
                return Ok(Datum::Null);
            }
            let a = coerce_unknown(v, &lo)?;
            let lo = coerce_unknown(lo, &a)?;
            let hi = coerce_unknown(hi, &a)?;
            let collation = resolve_comparison_collation(operand, low, row)?;
            let high_collation = resolve_comparison_collation(operand, high, row)?;
            if collation != high_collation {
                return Err(sql_err!(
                    sqlstate::COLLATION_MISMATCH,
                    "collation mismatch between \"{}\" and \"{}\"",
                    collation.name(),
                    high_collation.name()
                ));
            }
            let inside = compare_datums_with_catalog(collation, hooks.catalog, &a, &lo)?.is_ge()
                && compare_datums_with_catalog(collation, hooks.catalog, &a, &hi)?.is_le();
            Ok(Datum::Bool(inside != negated))
        }
        Expr::Like {
            operand,
            pattern,
            negated,
            case_insensitive,
            escape,
        } => {
            let v = eval_full(operand, arena, params, row, hooks)?;
            let p = eval_full(pattern, arena, params, row, hooks)?;
            let escape = match escape {
                Some(e) => match eval_full(e, arena, params, row, hooks)? {
                    Datum::Null => return Ok(Datum::Null),
                    d => Some(escape_char(d)?),
                },
                None => None,
            };
            match (v, p) {
                (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
                (Datum::Text(s) | Datum::Bpchar(s), Datum::Text(pat) | Datum::Bpchar(pat)) => {
                    let matched =
                        like_match(s, pat, case_insensitive, escape.unwrap_or(Some('\\')));
                    Ok(Datum::Bool(matched != negated))
                }
                (l, r) => Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "LIKE requires text operands, got {:?} and {:?}",
                    l,
                    r
                )),
            }
        }
        Expr::Match {
            operand,
            pattern,
            negated,
            case_insensitive,
        } => {
            let v = eval_full(operand, arena, params, row, hooks)?;
            let p = eval_full(pattern, arena, params, row, hooks)?;
            match (v, p) {
                (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
                (Datum::Text(s) | Datum::Bpchar(s), Datum::Text(pat) | Datum::Bpchar(pat)) => {
                    let matched = super::regex::regex_search(pat, s, case_insensitive)?;
                    Ok(Datum::Bool(matched != negated))
                }
                (l, r) => Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "regex match requires text operands, got {:?} and {:?}",
                    l,
                    r
                )),
            }
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
            ..
        } => {
            let scrutinee = match operand {
                Some(operator) => Some(eval_full(operator, arena, params, row, hooks)?),
                None => None,
            };
            // PostgreSQL unifies all branch result types to one common type;
            // compute it so every row's value has the same type as the
            // column PostgreSQL would report.
            let unified = case_result_type(whens, &otherwise, row);
            let chosen = 'chosen: {
                for (cond, result) in whens {
                    let hit = match &scrutinee {
                        Some(s) => {
                            let c = eval_full(cond, arena, params, row, hooks)?;
                            if s.is_null() || c.is_null() {
                                false
                            } else {
                                let l = coerce_unknown(*s, &c)?;
                                let r = coerce_unknown(c, &l)?;
                                let collation = resolve_comparison_collation(
                                    operand.expect("simple CASE has an operand"),
                                    cond,
                                    row,
                                )?;
                                compare_datums_with_catalog(collation, hooks.catalog, &l, &r)?
                                    .is_eq()
                            }
                        }
                        None => matches!(
                            boolean_argument(
                                eval_full(cond, arena, params, row, hooks)?,
                                "CASE/WHEN"
                            )?,
                            Datum::Bool(true)
                        ),
                    };
                    if hit {
                        break 'chosen eval_full(result, arena, params, row, hooks)?;
                    }
                }
                match otherwise {
                    Some(e) => eval_full(e, arena, params, row, hooks)?,
                    None => Datum::Null,
                }
            };
            match unified {
                Some(t) if !chosen.is_null() => cast_to(chosen, t, arena),
                _ => Ok(chosen),
            }
        }
        Expr::DefaultMarker => Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "DEFAULT is only allowed as a DML assignment value"
        )),
        Expr::Subquery(_) | Expr::ArraySubquery(_) => {
            if let Some(subs) = hooks.subs {
                for (node, v, _) in subs.scalars {
                    if core::ptr::eq(*node, expression as *const _) {
                        return Ok(*v);
                    }
                }
            }
            Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "subqueries are not allowed in this context (or are correlated)"
            ))
        }
        Expr::InSubquery {
            operand, negated, ..
        } => {
            let Some(subs) = hooks.subs else {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "subqueries are not allowed in this context"
                ));
            };
            let mut found: Option<SubqueryList<'_>> = None;
            for list in subs.lists {
                if core::ptr::eq(list.node, (expression as *const Expr).cast()) {
                    found = Some(*list);
                    break;
                }
            }
            let Some(list) = found else {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "subqueries are not allowed in this context (or are correlated)"
                ));
            };
            let witness = list.witness;
            let collations = effective_quantified_collations(operand, list.collations, row, arena)?;
            // Coerce the operand to the subquery's column type first: PostgreSQL
            // type-checks `x IN (...)` regardless of the set's contents, so a
            // string literal that cannot become the column type errors even
            // against an empty or all-NULL set.
            let v = eval_full(operand, arena, params, row, hooks)?;
            let v = coerce_unknown(v, &witness)?;
            // A bit string is comparable only to another bit string; reject a
            // bit-vs-other membership test up front (PostgreSQL type-checks the
            // operand against the column type even over an empty set).
            if matches!(v, Datum::Bit { .. }) && !matches!(witness, Datum::Bit { .. } | Datum::Null)
            {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "operator does not exist: bit = {}",
                    type_name_of(&witness)
                ));
            }
            // `x IN (subquery)` is `x = ANY (subquery)`. Over an empty set the
            // result is a constant FALSE (TRUE for NOT IN) regardless of x —
            // even a NULL x — so the empty case precedes the null short-circuit.
            if list.values.is_empty()
                && list.probe.is_none_or(|probe| {
                    // SAFETY: probe contexts are statement-arena allocations
                    // and the hook containing this pointer cannot outlive that
                    // arena.
                    unsafe { probe.as_ref() }.is_empty()
                })
            {
                return Ok(Datum::Bool(negated));
            }
            if v.is_null() {
                return Ok(Datum::Null);
            }
            let mut saw_null = list.saw_null;
            for member in list.values {
                if member.is_null() {
                    continue;
                }
                let l = coerce_unknown(v, member)?;
                let r = coerce_unknown(*member, &l)?;
                match quantified_comparison(BinaryOp::Eq, l, r, collations, hooks.catalog, arena)? {
                    Datum::Bool(true) => return Ok(Datum::Bool(!negated)),
                    Datum::Bool(false) => {}
                    Datum::Null => saw_null = true,
                    _ => unreachable!("equality returns boolean or NULL"),
                }
            }
            if let Some(probe) = list.probe {
                // SAFETY: see the lifetime invariant on `SubqueryList::probe`.
                let (matched, unknown) =
                    unsafe { probe.as_ref() }.probe(v, collations, hooks.catalog, arena)?;
                if matched {
                    return Ok(Datum::Bool(!negated));
                }
                saw_null |= unknown;
            }
            Ok(if saw_null {
                Datum::Null
            } else {
                Datum::Bool(negated)
            })
        }
        Expr::QuantifiedSubquery {
            operand,
            operator,
            all,
            ..
        } => {
            let Some(subs) = hooks.subs else {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "subqueries are not allowed in this context"
                ));
            };
            let list = subs
                .lists
                .iter()
                .find(|list| core::ptr::eq(list.node, (expression as *const Expr).cast()))
                .copied()
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "subqueries are not allowed in this context (or are correlated)"
                    )
                })?;
            let value = materialize_named_composite(
                eval_full(operand, arena, params, row, hooks)?,
                hooks,
                arena,
            )?;
            let value = coerce_unknown(value, &list.witness)?;
            let collations = effective_quantified_collations(operand, list.collations, row, arena)?;
            // Resolve the operator against the subquery's declared output
            // type even when it is empty or yields only NULLs.
            let _ = quantified_comparison(
                operator,
                value,
                list.witness,
                collations,
                hooks.catalog,
                arena,
            )?;
            let mut saw_unknown = false;
            for member in list.values {
                let left = coerce_unknown(value, member)?;
                let right = coerce_unknown(*member, &left)?;
                match quantified_comparison(
                    operator,
                    left,
                    right,
                    collations,
                    hooks.catalog,
                    arena,
                )? {
                    Datum::Bool(true) if !all => return Ok(Datum::Bool(true)),
                    Datum::Bool(false) if all => return Ok(Datum::Bool(false)),
                    Datum::Null => saw_unknown = true,
                    Datum::Bool(_) => {}
                    other => return Err(type_mismatch("quantified comparison", &other)),
                }
            }
            if let Some(probe) = list.probe {
                // SAFETY: see the lifetime invariant on `SubqueryList::probe`.
                let result = unsafe { probe.as_ref() }.quantify(
                    value,
                    operator,
                    all,
                    collations,
                    hooks.catalog,
                    arena,
                )?;
                match result {
                    Datum::Bool(true) if !all => return Ok(result),
                    Datum::Bool(false) if all => return Ok(result),
                    Datum::Null => saw_unknown = true,
                    Datum::Bool(_) => {}
                    _ => unreachable!("subquery quantifier returns boolean or NULL"),
                }
            }
            Ok(if saw_unknown {
                Datum::Null
            } else {
                Datum::Bool(all)
            })
        }
        Expr::Exists(_) => {
            // EXISTS results are pre-evaluated (uncorrelated) or evaluated per
            // outer row (correlated) and stored as a boolean scalar keyed by
            // node identity, alongside scalar subqueries.
            if let Some(subs) = hooks.subs {
                for (node, v, _) in subs.scalars {
                    if core::ptr::eq(*node, expression as *const _) {
                        return Ok(*v);
                    }
                }
            }
            Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXISTS is not allowed in this context"
            ))
        }
        Expr::Array(items) => {
            // A nested constructor adds one rectangular dimension; scalar members
            // form the final dimension.
            let mut vals = [Datum::Null; 256];
            if items.len() > vals.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "array constructor too large"
                ));
            }
            let mut element: Option<super::types::ArrElem> = None;
            for (i, e) in items.iter().enumerate() {
                let v = eval_full(e, arena, params, row, hooks)?;
                if let Some(el) = super::types::ArrElem::from_datum(&v) {
                    element = Some(element.map_or(el, |acc| unify_arr_elem(acc, el)));
                }
                vals[i] = v;
            }
            let array_domain_type = items.first().and_then(|expression| {
                let Expr::Cast { type_name, .. } = expression else {
                    return None;
                };
                hooks.catalog.and_then(|catalog| {
                    catalog
                        .array_domain_element(type_name)
                        .map(|element| (type_name, element))
                })
            });
            if let Some((type_name, element)) = array_domain_type {
                if vals[..items.len()]
                    .iter()
                    .any(|value| !value.is_null() && !matches!(value, Datum::Array { .. }))
                {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "ARRAY could not convert type to {}",
                        element.array_name()
                    ));
                }
                let catalog = hooks
                    .catalog
                    .expect("domain array identity requires catalog");
                for value in vals.iter_mut().take(items.len()) {
                    *value = catalog
                        .cast_user_type(type_name, *value, arena)?
                        .expect("domain array identity resolves a live domain");
                }
                return Ok(Datum::Array {
                    element,
                    raw: super::array::build(&vals[..items.len()], arena)?,
                });
            }
            if let Some(Datum::Array { element, raw }) = vals.first().copied() {
                let child = super::array::shape(raw).expect("array datum invariant");
                if child.dimension_count() == super::array::MAX_DIMENSIONS {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "array has too many dimensions"
                    ));
                }
                let mut flattened = [Datum::Null; super::array::MAX_ELEMENTS];
                let mut count = 0usize;
                for value in vals.iter().take(items.len()) {
                    let Datum::Array {
                        element: member_element,
                        raw: member_raw,
                    } = *value
                    else {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "ARRAY could not convert type {} to {}[]",
                            type_name_of(value),
                            element.array_name()
                        ));
                    };
                    let member_shape =
                        super::array::shape(member_raw).expect("array datum invariant");
                    if member_element != element || member_shape != child {
                        return Err(sql_err!(
                            sqlstate::ARRAY_SUBSCRIPT_ERROR,
                            "multidimensional arrays must have array expressions with matching dimensions"
                        ));
                    }
                    count = load_array(
                        member_raw,
                        member_element,
                        element,
                        &mut flattened,
                        count,
                        arena,
                    )?;
                }
                return Ok(Datum::Array {
                    element,
                    raw: super::array::build_shaped(
                        &flattened[..count],
                        child.with_first(items.len(), 1)?,
                        arena,
                    )?,
                });
            }
            let element = element.unwrap_or(super::types::ArrElem::Int4);
            // Coerce each element to the unified type.
            let ct = element.to_coltype();
            for v in vals.iter_mut().take(items.len()) {
                if !v.is_null() {
                    *v = match *v {
                        // A bpchar element keeps its padding in the array
                        // value, as PostgreSQL's array_out shows it.
                        Datum::Bpchar(s) => Datum::Text(s),
                        Datum::Composite { slot, .. } if matches!(element, super::types::ArrElem::Composite(expected) if expected == slot) => {
                            Datum::CompositeText {
                                slot,
                                physical_fields: 0,
                                text: arena.alloc_str_display(*v).map_err(|_| arena_full())?,
                            }
                        }
                        value @ Datum::CompositeText { slot, .. } if matches!(element, super::types::ArrElem::Composite(expected) if expected == slot) => {
                            value
                        }
                        other => cast_to(other, ct, arena)?,
                    };
                }
            }
            Ok(Datum::Array {
                element,
                raw: super::array::build(&vals[..items.len()], arena)?,
            })
        }
        Expr::Subscript { base, index } => {
            let b = eval_full(base, arena, params, row, hooks)?;
            let i = eval_full(index, arena, params, row, hooks)?;
            let index = match i {
                Datum::Int2(x) => x as i64,
                Datum::Int4(x) => x as i64,
                Datum::Int8(x) => x,
                Datum::Null => return Ok(Datum::Null),
                _ => return Err(type_mismatch("array subscript must be integer", &i)),
            };
            match b {
                Datum::Array { element, raw } => {
                    let shape = super::array::shape(raw).expect("array datum invariant");
                    if shape.dimension_count() == 0 {
                        return Ok(Datum::Null);
                    }
                    let lower = i64::from(shape.lower_bound(0).unwrap());
                    let upper = i64::from(shape.upper_bound(0).unwrap());
                    if index < lower || index > upper {
                        return Ok(Datum::Null);
                    }
                    let block = shape.element_count() / shape.dimension(0).unwrap();
                    let start = usize::try_from(index - lower).unwrap() * block;
                    if shape.dimension_count() == 1 {
                        return Ok(super::array::get(raw, element, start).unwrap());
                    }
                    let items = arena
                        .alloc_slice_with(block, |offset| {
                            super::array::get(raw, element, start + offset).unwrap()
                        })
                        .map_err(|_| arena_full())?;
                    Ok(Datum::Array {
                        element,
                        raw: super::array::build_shaped(items, shape.without_first()?, arena)?,
                    })
                }
                Datum::Text(value)
                    if matches!(
                        base,
                        Expr::Column { qualifier, name }
                            if row.col_type(*qualifier, name) == Some(ColType::Name)
                    ) =>
                {
                    // PostgreSQL's fixed-width `name` type exposes its
                    // underlying character array with zero-based subscripts.
                    // SQL text remains non-subscriptable.
                    if index < 0 {
                        return Ok(Datum::Null);
                    }
                    match value.as_bytes().get(index as usize) {
                        Some(byte) if byte.is_ascii() => {
                            let character = arena.alloc_slice_copy(&[*byte]).map_err(|_| {
                                sql_err!(sqlstate::OUT_OF_MEMORY, "statement arena exhausted")
                            })?;
                            Ok(Datum::Text(unsafe {
                                core::str::from_utf8_unchecked(character)
                            }))
                        }
                        Some(_) => Ok(Datum::Null),
                        None => Ok(Datum::Null),
                    }
                }
                Datum::Null => Ok(Datum::Null),
                _ => Err(type_mismatch("cannot subscript a non-array", &b)),
            }
        }
        Expr::Slice { base, lower, upper } => {
            let b = eval_full(base, arena, params, row, hooks)?;
            let (element, raw) = match b {
                Datum::Array { element, raw } => (element, raw),
                Datum::Null => return Ok(Datum::Null),
                _ => return Err(type_mismatch("cannot slice a non-array", &b)),
            };
            // Resolve the leading-dimension bounds; chained slices address the
            // remaining dimensions.
            let bound = |e: &Expr<'a>| -> Result<Option<i64>, SqlError> {
                match eval_full(e, arena, params, row, hooks)? {
                    Datum::Int2(x) => Ok(Some(x as i64)),
                    Datum::Int4(x) => Ok(Some(x as i64)),
                    Datum::Int8(x) => Ok(Some(x)),
                    Datum::Null => Ok(None),
                    other => Err(type_mismatch("array slice bound must be integer", &other)),
                }
            };
            let shape = super::array::shape(raw).expect("array datum invariant");
            if shape.dimension_count() == 0 {
                return Ok(Datum::Array { element, raw });
            }
            let first_lower = i64::from(shape.lower_bound(0).unwrap());
            let first_upper = i64::from(shape.upper_bound(0).unwrap());
            let lo = match lower {
                Some(e) => match bound(e)? {
                    Some(v) => v,
                    None => return Ok(Datum::Null),
                },
                None => first_lower,
            };
            let hi = match upper {
                Some(e) => match bound(e)? {
                    Some(v) => v,
                    None => return Ok(Datum::Null),
                },
                None => first_upper,
            };
            let lo = lo.max(first_lower);
            let hi = hi.min(first_upper);
            let block = shape.element_count() / shape.dimension(0).unwrap();
            let items: &[Datum] = if lo > hi {
                &[]
            } else {
                arena
                    .alloc_slice_with((hi - lo + 1) as usize * block, |i| {
                        let start = (lo - first_lower) as usize * block;
                        super::array::get(raw, element, start + i).unwrap()
                    })
                    .map_err(|_| arena_full())?
            };
            Ok(Datum::Array {
                element,
                raw: if items.is_empty() {
                    super::array::build(items, arena)?
                } else {
                    super::array::build_shaped(
                        items,
                        shape.sliced_first((hi - lo + 1) as usize)?,
                        arena,
                    )?
                },
            })
        }
        Expr::Field { base, field } => {
            // A `.*` that survived to a value position was not a top-level
            // select item — PostgreSQL refuses it there.
            if *field == *"*" {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "row expansion via \"*\" is not supported here"
                ));
            }
            // A direct ROW(...) resolves its f1..fn — unless the selected
            // field is a bare unknown literal, which PostgreSQL cannot
            // coerce (XX000). A typed sibling of an unknown field is fine.
            if let Expr::Call { name, args, .. } = base
                && name.eq_ignore_ascii_case("row")
                && let Some(position) = crate::sql::exec::RECORD_FIELD_NAMES
                    .iter()
                    .position(|n| n.eq_ignore_ascii_case(field))
                && matches!(args.get(position), Some(Expr::Str(_)))
            {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "failed to find conversion function from unknown to text"
                ));
            }
            let b = eval_full(base, arena, params, row, hooks)?;
            // A record arriving indirectly — CASE, a scalar subquery, an
            // aggregate — is anonymous to PostgreSQL: no field name resolves
            // in it. A chain of field accesses is judged by its root: rooted
            // in a column or whole row (whose record types are known) it
            // resolves; rooted in an anonymous constructor it does not, even
            // through a field that exists.
            fn chain_root<'e, 'x>(e: &'e Expr<'x>) -> &'e Expr<'x> {
                match e {
                    Expr::Field { base, .. } => chain_root(base),
                    other => other,
                }
            }
            let anonymous_source = match base {
                Expr::Call { name, .. } => {
                    !(name.eq_ignore_ascii_case("row")
                        || name.eq_ignore_ascii_case("json_each")
                        || name.eq_ignore_ascii_case("jsonb_each")
                        || name.eq_ignore_ascii_case("json_each_text")
                        || name.eq_ignore_ascii_case("jsonb_each_text"))
                }
                Expr::Case { .. } | Expr::Subquery(_) => true,
                Expr::Field { .. } => matches!(
                    chain_root(base),
                    Expr::Call { .. } | Expr::Case { .. } | Expr::Subquery(_)
                ),
                _ => false,
            };
            match b {
                Datum::Record(_) if anonymous_source => {
                    Err(crate::sql::exec::could_not_identify(field))
                }
                Datum::Null => Ok(Datum::Null),
                // A record: select the field by name (records carry lowercase
                // field names — `f1,f2,…` for ROW(), column names for a row).
                Datum::Record(fields) | Datum::Composite { fields, .. } => {
                    match fields.iter().find(|f| f.name.eq_ignore_ascii_case(field)) {
                        Some(f) => Ok(f.value),
                        None => Err(match base {
                            Expr::WholeRow(table)
                            | Expr::Column {
                                qualifier: None,
                                name: table,
                            } => sql_err!(
                                sqlstate::UNDEFINED_COLUMN,
                                "column {}.{} does not exist",
                                table,
                                field
                            ),
                            _ => crate::sql::exec::could_not_identify(field),
                        }),
                    }
                }
                Datum::CompositeText {
                    slot,
                    physical_fields,
                    text,
                } => {
                    let catalog = hooks.catalog.ok_or_else(|| {
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "named composite catalog access is unavailable"
                        )
                    })?;
                    let Datum::Composite { fields, .. } =
                        catalog.materialize_composite(slot, physical_fields, text, arena)?
                    else {
                        unreachable!("catalog materializes named composites")
                    };
                    fields
                        .iter()
                        .find(|f| f.name.eq_ignore_ascii_case(field))
                        .map(|f| f.value)
                        .ok_or_else(|| crate::sql::exec::could_not_identify(field))
                }
                // The `_pg_expandarray` result is encoded as the 2-element array
                // `[x, n]`; `.x`/`.f1` is the element and `.n`/`.f2` the ordinal.
                Datum::Array { element, raw } => {
                    let index = if field.eq_ignore_ascii_case("x")
                        || field.eq_ignore_ascii_case("f1")
                    {
                        0
                    } else if field.eq_ignore_ascii_case("n") || field.eq_ignore_ascii_case("f2") {
                        1
                    } else {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_COLUMN,
                            "field \"{}\" not found",
                            field
                        ));
                    };
                    Ok(super::array::get(raw, element, index).unwrap_or(Datum::Null))
                }
                _ => Err(crate::sql::exec::not_composite(field, type_name_of(&b))),
            }
        }
        Expr::AnyAll {
            operand,
            operator,
            array,
            all,
        } => {
            let lhs = eval_full(operand, arena, params, row, hooks)?;
            let array = eval_full(array, arena, params, row, hooks)?;
            let (element, raw) = match array {
                Datum::Array { element, raw } => (element, raw),
                Datum::Null => return Ok(Datum::Null),
                // An unknown literal on the array side (`= ANY('{1,2}')`) is cast
                // to an array of the left operand's element type, as PostgreSQL
                // resolves it.
                Datum::Text(s) => {
                    let element = super::types::ArrElem::from_datum(&lhs)
                        .unwrap_or(super::types::ArrElem::Text);
                    let raw = super::array::parse_literal(s, element, arena)?;
                    (element, raw)
                }
                _ => return Err(type_mismatch("ANY/ALL requires an array", &array)),
            };
            let n = super::array::len(raw);
            let mut saw_null = false;
            for i in 0..n {
                let el = super::array::get(raw, element, i).unwrap_or(Datum::Null);
                match binary(operator, lhs, el, false, false, arena)? {
                    Datum::Bool(true) if !all => return Ok(Datum::Bool(true)),
                    Datum::Bool(false) if all => return Ok(Datum::Bool(false)),
                    Datum::Null => saw_null = true,
                    _ => {}
                }
            }
            if saw_null {
                Ok(Datum::Null)
            } else {
                // ANY with no match is false; ALL with no counterexample is true.
                Ok(Datum::Bool(all))
            }
        }
    }
}

fn effective_quantified_collations<'a>(
    operand: &Expr<'a>,
    right: &[Collation],
    row: &impl ColumnLookup<'a>,
    arena: &'a Arena,
) -> Result<&'a [Collation], SqlError> {
    let mut output = [Collation::None; super::parser::MAX_LIST];
    if right.len() > output.len() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "too many fields in row comparison"
        ));
    }
    let row_args = match operand {
        Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("row") => Some(*args),
        _ => None,
    };
    let whole_fields = match operand {
        Expr::WholeRow(table) => row.whole_row_fields(table, arena)?,
        _ => None,
    };
    for (index, right) in right.iter().copied().enumerate() {
        let left = if let Some(args) = row_args {
            args.get(index)
                .map(|expression| expression_collation(expression, row))
                .transpose()?
                .flatten()
        } else if let Expr::WholeRow(table) = operand {
            whole_fields
                .and_then(|fields| fields.get(index))
                .map(|field| DerivedCollation {
                    value: row.collation(Some(table), field.name),
                    explicit: false,
                    indeterminate: false,
                })
        } else if index == 0 {
            expression_collation(operand, row)?
        } else {
            None
        };
        let right = (right != Collation::None).then_some(DerivedCollation {
            value: right,
            explicit: false,
            indeterminate: false,
        });
        output[index] = required_comparison_collation(merge_derived_collations(left, right)?)?;
    }
    arena
        .alloc_slice_copy(&output[..right.len()])
        .map(|slice| &*slice)
        .map_err(|_| arena_full())
}

fn typed_field_value<'a>(field: &super::types::RecordField<'a>) -> Datum<'a> {
    if field.value.is_null() {
        ColType::from_oid(field.type_oid)
            .map(super::query::type_witness)
            .unwrap_or(Datum::Null)
    } else {
        field.value
    }
}

fn quantified_scalar<'a>(
    operator: BinaryOp,
    left: Datum<'a>,
    right: Datum<'a>,
    collation: Collation,
    catalog: Option<&dyn CatalogAccess>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if matches!(
        (&left, &right),
        (
            Datum::Text(_) | Datum::Bpchar(_),
            Datum::Text(_) | Datum::Bpchar(_)
        )
    ) {
        compare_text_collated(operator, left, right, false, false, collation, catalog)
    } else {
        binary(operator, left, right, false, false, arena)
    }
}

pub(crate) fn quantified_comparison<'a>(
    operator: BinaryOp,
    left: Datum<'a>,
    right: Datum<'a>,
    collations: &[Collation],
    catalog: Option<&dyn CatalogAccess>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let materialize = |value| match value {
        Datum::CompositeText {
            slot,
            physical_fields,
            text,
        } => catalog
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "named composite catalog access is unavailable"
                )
            })?
            .materialize_composite(slot, physical_fields, text, arena),
        value => Ok(value),
    };
    let left = materialize(left)?;
    let right = materialize(right)?;
    match (left, right) {
        (
            Datum::Record(left) | Datum::Composite { fields: left, .. },
            Datum::Record(right) | Datum::Composite { fields: right, .. },
        ) => {
            if left.len() != right.len() {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "unequal number of entries in row expressions"
                ));
            }
            if collations.len() != left.len() {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "unequal number of entries in row expressions"
                ));
            }
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                let _ = quantified_scalar(
                    operator,
                    typed_field_value(left),
                    typed_field_value(right),
                    collations[index],
                    catalog,
                    arena,
                )?;
            }
            if matches!(operator, BinaryOp::Eq | BinaryOp::NotEq) {
                let mut saw_null = false;
                for (index, (left, right)) in left.iter().zip(right).enumerate() {
                    match quantified_scalar(
                        BinaryOp::Eq,
                        left.value,
                        right.value,
                        collations[index],
                        catalog,
                        arena,
                    )? {
                        Datum::Bool(false) => return Ok(Datum::Bool(operator == BinaryOp::NotEq)),
                        Datum::Null => saw_null = true,
                        Datum::Bool(true) => {}
                        _ => unreachable!("equality returns boolean or NULL"),
                    }
                }
                return Ok(if saw_null {
                    Datum::Null
                } else {
                    Datum::Bool(operator == BinaryOp::Eq)
                });
            }
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                match quantified_scalar(
                    BinaryOp::Eq,
                    left.value,
                    right.value,
                    collations[index],
                    catalog,
                    arena,
                )? {
                    Datum::Bool(true) => continue,
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Bool(false) => {
                        return quantified_scalar(
                            operator,
                            left.value,
                            right.value,
                            collations[index],
                            catalog,
                            arena,
                        );
                    }
                    _ => unreachable!("equality returns boolean or NULL"),
                }
            }
            Ok(Datum::Bool(matches!(
                operator,
                BinaryOp::LtEq | BinaryOp::GtEq
            )))
        }
        (left, right) => quantified_scalar(
            operator,
            left,
            right,
            collations.first().copied().unwrap_or(Collation::None),
            catalog,
            arena,
        ),
    }
}

pub(crate) fn resolve_comparison_collation<'a>(
    left: &Expr<'a>,
    right: &Expr<'a>,
    row: &impl ColumnLookup<'a>,
) -> Result<crate::sql::ast::Collation, SqlError> {
    let left = expression_collation(left, row)?;
    let right = expression_collation(right, row)?;
    required_comparison_collation(merge_derived_collations(left, right)?)
}

fn required_comparison_collation(
    collation: Option<DerivedCollation>,
) -> Result<crate::sql::ast::Collation, SqlError> {
    match collation {
        Some(collation) if collation.indeterminate => Err(sql_err!(
            sqlstate::INDETERMINATE_COLLATION,
            "could not determine which collation to use for string comparison"
        )),
        Some(collation) => Ok(collation.value),
        None => Ok(crate::sql::ast::Collation::None),
    }
}

/// PostgreSQL's collation-combination rule, shared by every expression that
/// produces a collatable result.
fn merge_derived_collations(
    left: Option<DerivedCollation>,
    right: Option<DerivedCollation>,
) -> Result<Option<DerivedCollation>, SqlError> {
    match (left, right) {
        (Some(left), Some(right))
            if left.explicit && right.explicit && left.value != right.value =>
        {
            Err(sql_err!(
                sqlstate::COLLATION_MISMATCH,
                "collation mismatch between \"{}\" and \"{}\"",
                left.value.name(),
                right.value.name()
            ))
        }
        (Some(left), Some(_right)) if left.explicit => Ok(Some(left)),
        (Some(_left), Some(right)) if right.explicit => Ok(Some(right)),
        (Some(left), Some(right)) if left.indeterminate && right.indeterminate => {
            Ok(Some(DerivedCollation {
                value: crate::sql::ast::Collation::None,
                explicit: false,
                indeterminate: true,
            }))
        }
        (Some(left), Some(right)) if left.indeterminate => Ok(Some(right)),
        (Some(left), Some(right)) if right.indeterminate => Ok(Some(left)),
        (Some(left), Some(right))
            if left.value != crate::sql::ast::Collation::Default
                && right.value != crate::sql::ast::Collation::Default
                && left.value != right.value =>
        {
            Ok(Some(DerivedCollation {
                value: crate::sql::ast::Collation::None,
                explicit: false,
                indeterminate: true,
            }))
        }
        (Some(left), Some(right)) => {
            Ok(Some(if left.value == crate::sql::ast::Collation::Default {
                right
            } else {
                left
            }))
        }
        (Some(collation), _) | (_, Some(collation)) => Ok(Some(collation)),
        (None, None) => Ok(None),
    }
}

#[derive(Clone, Copy)]
struct DerivedCollation {
    value: crate::sql::ast::Collation,
    explicit: bool,
    indeterminate: bool,
}

fn expression_collation<'a>(
    expression: &Expr<'a>,
    row: &impl ColumnLookup<'a>,
) -> Result<Option<DerivedCollation>, SqlError> {
    match expression {
        Expr::Collate { operand, collation } => {
            let collatable = static_type(operand, row).is_none_or(ColType::is_collatable);
            if !collatable {
                return Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "collations are not supported by type {}",
                    static_type(operand, row)
                        .expect("known noncollatable type")
                        .name()
                ));
            }
            Ok(Some(DerivedCollation {
                value: *collation,
                explicit: true,
                indeterminate: false,
            }))
        }
        Expr::Cast {
            operand, type_name, ..
        } => match ColType::from_sql_name(type_name) {
            Some(ctype) if ctype.is_collatable() => expression_collation(operand, row),
            Some(_) => Ok(None),
            None => expression_collation(operand, row),
        },
        Expr::Column { qualifier, name } => {
            let value = row.collation(*qualifier, name);
            Ok(Some(DerivedCollation {
                value,
                explicit: false,
                indeterminate: value == crate::sql::ast::Collation::None
                    && row
                        .col_type(*qualifier, name)
                        .is_some_and(ColType::is_collatable),
            }))
        }
        Expr::SchemaColumn { table, name, .. } => {
            let value = row.collation(Some(table), name);
            Ok(Some(DerivedCollation {
                value,
                explicit: false,
                indeterminate: value == crate::sql::ast::Collation::None
                    && row
                        .col_type(Some(table), name)
                        .is_some_and(ColType::is_collatable),
            }))
        }
        Expr::Field { base, field } => match &**base {
            Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("row") => {
                let position = crate::sql::exec::RECORD_FIELD_NAMES
                    .iter()
                    .position(|name| name.eq_ignore_ascii_case(field));
                match position.and_then(|position| args.get(position)) {
                    Some(argument) => expression_collation(argument, row),
                    None => Ok(None),
                }
            }
            Expr::WholeRow(table) => {
                let value = row.collation(Some(table), field);
                Ok(Some(DerivedCollation {
                    value,
                    explicit: false,
                    indeterminate: value == crate::sql::ast::Collation::None
                        && static_type(expression, row).is_some_and(ColType::is_collatable),
                }))
            }
            _ => {
                let value = row.record_field_collation(base, field);
                Ok(Some(DerivedCollation {
                    value,
                    explicit: false,
                    indeterminate: value == crate::sql::ast::Collation::None
                        && static_type(expression, row).is_some_and(ColType::is_collatable),
                }))
            }
        },
        Expr::Binary {
            operator: BinaryOp::Concat,
            left,
            right,
        } => merge_derived_collations(
            expression_collation(left, row)?,
            expression_collation(right, row)?,
        ),
        Expr::Case {
            whens, otherwise, ..
        } => {
            let mut derived = None;
            for (_, result) in *whens {
                derived = merge_derived_collations(derived, expression_collation(result, row)?)?;
            }
            if let Some(result) = otherwise {
                derived = merge_derived_collations(derived, expression_collation(result, row)?)?;
            }
            Ok(derived)
        }
        Expr::Call { name, args, .. }
            if name.eq_ignore_ascii_case("coalesce")
                || name.eq_ignore_ascii_case("greatest")
                || name.eq_ignore_ascii_case("least")
                || collation_preserving_call(name) =>
        {
            let mut derived = None;
            for argument in *args {
                derived = merge_derived_collations(derived, expression_collation(argument, row)?)?;
            }
            Ok(derived)
        }
        _ => Ok(None),
    }
}

/// Returns the collation required by an operation that compares or orders an
/// expression. An indeterminate collatable expression is rejected here rather
/// than being mistaken for a non-collatable value.
pub(crate) fn resolved_expression_collation<'a>(
    expression: &Expr<'a>,
    row: &impl ColumnLookup<'a>,
) -> Result<crate::sql::ast::Collation, SqlError> {
    required_comparison_collation(expression_collation(expression, row)?)
}

/// Returns result metadata without requiring a usable comparison collation.
/// PostgreSQL permits this state through `UNION ALL` and diagnoses it only
/// when a later operation needs collation semantics.
pub(crate) fn described_expression_collation<'a>(
    expression: &Expr<'a>,
    row: &impl ColumnLookup<'a>,
) -> Result<
    (
        crate::sql::ast::Collation,
        crate::sql::types::CollationDerivation,
    ),
    SqlError,
> {
    use crate::sql::types::CollationDerivation;
    Ok(match expression_collation(expression, row)? {
        Some(value) if value.indeterminate => (
            crate::sql::ast::Collation::None,
            CollationDerivation::Indeterminate,
        ),
        Some(value) if value.explicit => (value.value, CollationDerivation::Explicit),
        Some(value) => (value.value, CollationDerivation::Implicit),
        None => (crate::sql::ast::Collation::None, CollationDerivation::None),
    })
}

/// Text functions whose result retains the input collation.  Non-text calls
/// deliberately stay absent: a catalog collation OID of zero is meaningful.
fn collation_preserving_call(name: &str) -> bool {
    [
        "lower",
        "upper",
        "initcap",
        "trim",
        "btrim",
        "ltrim",
        "rtrim",
        "replace",
        "translate",
        "regexp_replace",
        "overlay",
        "substring",
        "substr",
        "left",
        "right",
        "lpad",
        "rpad",
        "repeat",
        "concat",
        "concat_ws",
    ]
    .iter()
    .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

/// The wider of two array element types (for `ARRAY[...]` type unification).
fn unify_arr_elem(a: super::types::ArrElem, b: super::types::ArrElem) -> super::types::ArrElem {
    use super::types::ArrElem::*;
    match (a, b) {
        (x, y) if x == y => x,
        (Float8, _) | (_, Float8) => Float8,
        (Numeric, _) | (_, Numeric) => Numeric,
        (Int8, _) | (_, Int8) => Int8,
        (Text, _) | (_, Text) => Text,
        _ => a,
    }
}

/// PostgreSQL names the argument types a call was made with, so that the
/// message says which function was looked for rather than only that one was:
/// `nosuchfunc(integer)`, not `nosuchfunc()`. The types are the static ones —
/// an argument is never evaluated to build an error about a function that will
/// not run — so an untyped literal is `unknown`, exactly as PostgreSQL has it.
fn undefined_function<'a>(name: &str, args: &[&Expr<'a>], row: &impl ColumnLookup<'a>) -> SqlError {
    use core::fmt::Write as _;
    let mut list = StackStr::<256>::new();
    for (i, argument) in args.iter().enumerate() {
        if i > 0 {
            let _ = list.write_str(", ");
        }
        // An untyped literal is `unknown` to PostgreSQL however it would later
        // coerce, and an array constructor names its element type.
        let named = if is_unknown_literal(argument) {
            None
        } else if let Expr::Array(items) = argument {
            items
                .first()
                .and_then(|first| static_type(first, row))
                .and_then(crate::sql::types::ArrElem::from_coltype)
                .map(|element| element.array_name())
        } else {
            static_type(argument, row).map(ColType::name)
        };
        let _ = list.write_str(named.unwrap_or("unknown"));
    }
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "function {}({}) does not exist",
        name,
        list.as_str()
    )
}

fn call<'a>(
    name: &str,
    args: &[&Expr<'a>],
    star: bool,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
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
    if let Some(result) = funcs::bytea::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::math::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::string::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::datetime::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::json::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::array::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::net::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::range::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::regex::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::system::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if let Some(result) = funcs::conditional::dispatch(name, args, star, arena, params, row, hooks)
    {
        return result;
    }
    if let Some(result) = funcs::misc::dispatch(name, args, star, arena, params, row, hooks) {
        return result;
    }
    if !star && let Some(catalog) = hooks.catalog {
        let mut arguments = [Datum::Null; crate::sql::parser::MAX_LIST];
        let mut argument_type_oids =
            [crate::sql::types::oid::UNKNOWN; crate::sql::parser::MAX_LIST];
        if args.len() <= arguments.len() {
            for (slot, argument) in args.iter().enumerate() {
                arguments[slot] = eval_full(argument, arena, params, row, hooks)?;
                argument_type_oids[slot] = expression_type_identity(argument, row, hooks)?
                    .routine_argument_oid(&arguments[slot]);
            }
            if let Some(result) = catalog.call_routine(
                name,
                &arguments[..args.len()],
                &argument_type_oids[..args.len()],
                arena,
            )? {
                return Ok(result);
            }
        }
    }
    match name {
        "count" | "sum" | "avg" | "min" | "max" | "bool_and" | "bool_or" | "every"
        | "string_agg" => Err(sql_err!(
            sqlstate::GROUPING_ERROR,
            "aggregate functions are not allowed here"
        )),
        // Sequence functions: side-effecting, so they run through the
        // `hooks.sequences` bridge (interior-mutable) rather than as pure eval.
        "nextval" | "currval" | "setval" => {
            let engine = hooks.sequences.ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "{}() cannot be used in this context",
                    name
                )
            })?;
            let seq_name = match args::text_arg(name, args, 0, arena, params, row, hooks)? {
                Some(s) => s,
                None => return Ok(Datum::Null),
            };
            let result = match name {
                "nextval" => {
                    arity(1)?;
                    engine.nextval(seq_name)?
                }
                "currval" => {
                    arity(1)?;
                    engine.currval(seq_name)?
                }
                _ => {
                    if args.len() != 2 && args.len() != 3 || star {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_FUNCTION,
                            "function setval(...) with {} arguments does not exist",
                            args.len()
                        ));
                    }
                    let value = match args::int_arg(name, args, 1, arena, params, row, hooks)? {
                        Some(v) => v,
                        None => return Ok(Datum::Null),
                    };
                    let is_called = if args.len() == 3 {
                        match args::bool_arg(name, args, 2, arena, params, row, hooks)? {
                            Some(b) => b,
                            None => return Ok(Datum::Null),
                        }
                    } else {
                        true
                    };
                    engine.setval(seq_name, value, is_called)?
                }
            };
            Ok(Datum::Int8(result))
        }
        "lastval" => {
            arity(0)?;
            let engine = hooks.sequences.ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "lastval() cannot be used in this context"
                )
            })?;
            Ok(Datum::Int8(engine.lastval()?))
        }
        // Set-returning functions: during expansion `hooks.srf_index` (1-based)
        // selects which element/value this output row carries.
        "unnest" => {
            arity(1)?;
            let a = eval_full(args[0], arena, params, row, hooks)?;
            let (element, raw) = match a {
                Datum::Array { element, raw } => (element, raw),
                Datum::Null => return Ok(Datum::Null),
                _ => return Err(type_mismatch("unnest requires an array", &a)),
            };
            let k = hooks.srf_index.ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "set-returning function called where not allowed"
                )
            })?;
            Ok(super::array::get(raw, element, k - 1).unwrap_or(Datum::Null))
        }
        "generate_series" => {
            if !(2..=3).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let k = hooks.srf_index.ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "set-returning function called where not allowed"
                )
            })? as i64;
            let start = eval_full(args[0], arena, params, row, hooks)?;
            let stop = eval_full(args[1], arena, params, row, hooks)?;
            let step = if args.len() == 3 {
                eval_full(args[2], arena, params, row, hooks)?
            } else {
                Datum::Int4(1)
            };
            if let (Some(s), Some(e), Some(st)) = (as_i64(&start), as_i64(&stop), as_i64(&step)) {
                let v = s + (k - 1) * st;
                // Past the end of this series (a shorter SRF paired with a longer
                // one runs out): NULL, matching PostgreSQL's lockstep expansion.
                if st == 0 || (st > 0 && v > e) || (st < 0 && v < e) {
                    return Ok(Datum::Null);
                }
                // int4 unless an argument is int8 or the value overflows int4.
                let wide = matches!(start, Datum::Int8(_))
                    || matches!(stop, Datum::Int8(_))
                    || matches!(step, Datum::Int8(_));
                return Ok(if !wide && i32::try_from(v).is_ok() {
                    Datum::Int4(v as i32)
                } else {
                    Datum::Int8(v)
                });
            }
            if let Some((base, kind)) = timestamp_series_start(&start) {
                if args.len() != 3 {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "generate_series over timestamps requires a step"
                    ));
                }
                let stop_micros =
                    timestamp_series_start(&cast_to(stop, kind.coltype(), arena)?).map(|(m, _)| m);
                // The step is an interval — coerce a bare string literal, as
                // PostgreSQL's function resolution does.
                let Datum::Interval(step_iv) = cast_to(step, ColType::Interval, arena)? else {
                    return Ok(Datum::Null);
                };
                // Iterative addition — calendar month/day arithmetic does not
                // distribute over multiplication, so the k-th value is `start`
                // stepped k-1 times (matching PostgreSQL).
                let mut v = base;
                for _ in 1..k {
                    v = super::datetime::add_interval(v, step_iv);
                }
                // Past the end of this series (lockstep with a longer SRF): NULL.
                let positive = interval_is_positive(step_iv);
                return match stop_micros {
                    Some(stop) if (positive && v > stop) || (!positive && v < stop) => {
                        Ok(Datum::Null)
                    }
                    Some(_) => Ok(kind.datum(v)),
                    None => Ok(Datum::Null),
                };
            }
            if start.is_null() || stop.is_null() || step.is_null() {
                return Ok(Datum::Null);
            }
            let (Datum::Numeric(start), Datum::Numeric(stop), Datum::Numeric(step)) = (
                cast_to(start, ColType::Numeric, arena)?,
                cast_to(stop, ColType::Numeric, arena)?,
                cast_to(step, ColType::Numeric, arena)?,
            ) else {
                return Ok(Datum::Null);
            };
            Ok(numeric_series_at(start, stop, step, k as usize, arena)?
                .map_or(Datum::Null, Datum::Numeric))
        }
        // Set-returning `regexp_matches(string, pattern [, flags])`: for the
        // current expansion index k, the capture groups of the k-th match as a
        // text[] (or the whole match when the pattern has no groups). NULLs
        // (arguments or non-participating groups) follow PostgreSQL.
        "regexp_matches" => {
            if !(2..=3).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let k = hooks.srf_index.ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "set-returning function called where not allowed"
                )
            })?;
            let (Some(string), Some(pattern)) = (
                text_arg(name, args, 0, arena, params, row, hooks)?,
                text_arg(name, args, 1, arena, params, row, hooks)?,
            ) else {
                return Ok(Datum::Null);
            };
            let flags = if args.len() == 3 {
                text_arg(name, args, 2, arena, params, row, hooks)?.unwrap_or("")
            } else {
                ""
            };
            let (global, ci) = regexp_flags(flags)?;
            let mut spans = [(-1i64, -1i64); super::regex::MAX_GROUPS];
            let mut from = 0usize;
            let mut count = 0usize;
            loop {
                let Some(((mstart, mend), ng)) =
                    super::regex::find_captures(pattern, string, from, ci, &mut spans)?
                else {
                    return Ok(Datum::Null);
                };
                count += 1;
                if count == k {
                    // No capture groups: the whole match is the single element.
                    let mut elems = [Datum::Null; super::regex::MAX_GROUPS];
                    let n = if ng == 0 {
                        elems[0] = Datum::Text(&string[mstart..mend]);
                        1
                    } else {
                        for (i, span) in spans[..ng].iter().enumerate() {
                            elems[i] = if span.0 < 0 {
                                Datum::Null
                            } else {
                                Datum::Text(&string[span.0 as usize..span.1 as usize])
                            };
                        }
                        ng
                    };
                    return Ok(Datum::Array {
                        element: super::types::ArrElem::Text,
                        raw: super::array::build(&elems[..n], arena)?,
                    });
                }
                if !global {
                    return Ok(Datum::Null);
                }
                from = if mend > mstart { mend } else { mend + 1 };
                if from > string.len() {
                    return Ok(Datum::Null);
                }
            }
        }
        // Set-returning `_pg_expandarray(array)` yields, for the current expansion
        // index k, the composite `(x, n)` = (array[k], k), encoded as `[x, n]`.
        "_pg_expandarray" => {
            arity(1)?;
            let a = eval_full(args[0], arena, params, row, hooks)?;
            let k = hooks.srf_index.unwrap_or(1);
            let x = match a {
                Datum::Array { element, raw } => {
                    super::array::get(raw, element, k - 1).unwrap_or(Datum::Null)
                }
                Datum::Int2Vector(raw) => raw
                    .chunks_exact(2)
                    .nth(k - 1)
                    .map(|bytes| Datum::Int4(i16::from_le_bytes([bytes[0], bytes[1]]) as i32))
                    .unwrap_or(Datum::Null),
                Datum::OidVector(raw) => raw
                    .chunks_exact(4)
                    .nth(k - 1)
                    .map(|bytes| Datum::Oid(u32::from_le_bytes(bytes.try_into().unwrap())))
                    .unwrap_or(Datum::Null),
                Datum::Null => return Ok(Datum::Null),
                _ => return Err(type_mismatch("_pg_expandarray requires an array", &a)),
            };
            let comp = [x, Datum::Int4(k as i32)];
            Ok(Datum::Array {
                element: super::types::ArrElem::Int4,
                raw: super::array::build(&comp, arena)?,
            })
        }
        // Set-returning `jsonb_object_keys(obj)` / `json_object_keys(obj)`
        // yield each key of the object as one text row.
        // Set-returning `regexp_split_to_table(source, pattern [, flags])`:
        // the k-th split piece for the current expansion index.
        "regexp_split_to_table" => {
            if !(2..=3).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let (Some(src), Some(pat)) = (
                text_arg(name, args, 0, arena, params, row, hooks)?,
                text_arg(name, args, 1, arena, params, row, hooks)?,
            ) else {
                return Ok(Datum::Null);
            };
            let case_insensitive = if args.len() == 3 {
                let Some(flags) = text_arg(name, args, 2, arena, params, row, hooks)? else {
                    return Ok(Datum::Null);
                };
                regexp_flags(flags)?.1
            } else {
                false
            };
            let pieces = regex_split_pub(src, pat, case_insensitive, arena)?;
            let k = hooks.srf_index.ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "set-returning function called where not allowed"
                )
            })?;
            Ok(pieces.get(k - 1).copied().unwrap_or(Datum::Null))
        }
        // Set-returning `string_to_table(string, delimiter [, null_string])`:
        // the k-th piece for the current expansion index. The split rule is
        // shared with `string_to_array`, so the two cannot disagree.
        "string_to_table" => {
            if !(2..=3).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let Some(source) = text_arg(name, args, 0, arena, params, row, hooks)? else {
                return Ok(Datum::Null);
            };
            // A NULL delimiter splits into characters rather than yielding NULL.
            let delimiter = text_arg(name, args, 1, arena, params, row, hooks)?;
            let null_string = if args.len() == 3 {
                text_arg(name, args, 2, arena, params, row, hooks)?
            } else {
                None
            };
            let mut pieces = [""; crate::sql::parser::MAX_LIST * 16];
            let n = split_pieces(source, delimiter, &mut pieces)?;
            let k = hooks.srf_index.ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "set-returning function called where not allowed"
                )
            })?;
            Ok(match pieces[..n].get(k - 1) {
                Some(piece) if null_string == Some(*piece) => Datum::Null,
                Some(piece) => Datum::Text(arena.alloc_str(piece).map_err(|_| arena_full())?),
                None => Datum::Null,
            })
        }
        // Set-returning `generate_subscripts(array, dim [, reverse])`.
        "generate_subscripts" => {
            if !(2..=3).contains(&args.len()) {
                return Err(arity_err(name, args.len()));
            }
            let raw = match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Array { raw, .. } => raw,
                Datum::Null => return Ok(Datum::Null),
                other => {
                    return Err(type_mismatch(
                        "generate_subscripts requires an array",
                        &other,
                    ));
                }
            };
            let dim = match eval_full(args[1], arena, params, row, hooks)? {
                Datum::Int2(v) => v as i64,
                Datum::Int4(v) => v as i64,
                Datum::Int8(v) => v,
                Datum::Null => return Ok(Datum::Null),
                other => {
                    return Err(type_mismatch(
                        "generate_subscripts dim must be an integer",
                        &other,
                    ));
                }
            };
            let k = hooks.srf_index.ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "set-returning function called where not allowed"
                )
            })?;
            let reverse = if args.len() == 3 {
                match eval_full(args[2], arena, params, row, hooks)? {
                    Datum::Bool(reverse) => reverse,
                    Datum::Null => return Ok(Datum::Null),
                    other => {
                        return Err(type_mismatch(
                            "generate_subscripts reverse must be boolean",
                            &other,
                        ));
                    }
                }
            } else {
                false
            };
            let dimension = usize::try_from(dim).ok().and_then(|dim| dim.checked_sub(1));
            let shape = super::array::shape(raw).expect("array datum invariant");
            let Some(dimension) = dimension else {
                return Ok(Datum::Null);
            };
            let Some(length) = shape.dimension(dimension) else {
                return Ok(Datum::Null);
            };
            if k > length {
                return Ok(Datum::Null);
            }
            let offset = i32::try_from(k - 1).expect("array dimension fits i32");
            let subscript = if reverse {
                shape.upper_bound(dimension).expect("known dimension") - offset
            } else {
                shape.lower_bound(dimension).expect("known dimension") + offset
            };
            Ok(Datum::Int4(subscript))
        }
        "jsonb_object_keys" | "json_object_keys" => {
            arity(1)?;
            let jsonb = name.starts_with("jsonb");
            let text = match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Json { text, .. } => text,
                Datum::Text(s) => s,
                Datum::Null => return Ok(Datum::Null),
                other => return Err(type_mismatch("object_keys requires an object", &other)),
            };
            let k = hooks.srf_index.ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "set-returning function called where not allowed"
                )
            })?;
            let kind = super::json::kind_of(text);
            if kind != super::json::Kind::Object {
                return Err(super::json::object_keys_error(name, kind));
            }
            if jsonb {
                // jsonb keys: sorted, deduplicated (the normalized parse order).
                let super::json::Json::Object(members) = super::json::parse(text, arena)? else {
                    return Err(super::json::object_keys_error(name, kind));
                };
                return Ok(members
                    .get(k - 1)
                    .map(|(key, _)| Datum::Text(key))
                    .unwrap_or(Datum::Null));
            }
            // json keys: original source order, duplicates kept.
            let members = super::json::object_members_source(text, arena)?;
            Ok(members
                .get(k - 1)
                .map(|(key, _)| Datum::Text(key))
                .unwrap_or(Datum::Null))
        }
        // Set-returning `jsonb_array_elements` / `json_array_elements` yield each
        // array element as a json/jsonb row; the `_text` variants yield text.
        "jsonb_array_elements"
        | "json_array_elements"
        | "jsonb_array_elements_text"
        | "json_array_elements_text" => {
            arity(1)?;
            let jsonb = name.starts_with("jsonb");
            let as_text = name.ends_with("_text");
            let text = match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Json { text, .. } => text,
                Datum::Text(s) => s,
                Datum::Null => return Ok(Datum::Null),
                other => return Err(type_mismatch("array_elements requires an array", &other)),
            };
            let k = hooks.srf_index.ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "set-returning function called where not allowed"
                )
            })?;
            let kind = super::json::kind_of(text);
            if kind != super::json::Kind::Array {
                return Err(super::json::array_elements_error(name, jsonb, kind));
            }
            if jsonb {
                // jsonb elements: normalized (re-rendered) json values.
                let super::json::Json::Array(items) = super::json::parse(text, arena)? else {
                    return Err(super::json::array_elements_error(name, jsonb, kind));
                };
                let Some(element) = items.get(k - 1) else {
                    return Ok(Datum::Null);
                };
                if as_text {
                    return Ok(match *element {
                        super::json::Json::Str(s) => {
                            Datum::Text(super::json::decode_string(s, arena)?)
                        }
                        super::json::Json::Null => Datum::Null,
                        _ => Datum::Text(json_to_text(element, arena)?),
                    });
                }
                return Ok(Datum::Json {
                    text: json_to_text(element, arena)?,
                    jsonb,
                });
            }
            // json elements: verbatim source text (interior whitespace kept).
            let items = super::json::array_elements_source(text, arena)?;
            let Some(element) = items.get(k - 1) else {
                return Ok(Datum::Null);
            };
            if as_text {
                // The text form of a json element: a string's decoded value,
                // anything else its verbatim json (NULL for a json null).
                let parsed = super::json::parse(element, arena)?;
                return Ok(match parsed {
                    super::json::Json::Str(s) => Datum::Text(super::json::decode_string(s, arena)?),
                    super::json::Json::Null => Datum::Null,
                    _ => Datum::Text(element),
                });
            }
            Ok(Datum::Json {
                text: element,
                jsonb,
            })
        }
        // Set-returning `json_each` / `jsonb_each[_text]` yield, for the current
        // expansion index k, the composite `(key, value)` of the k-th object
        // member as a record (`SELECT * FROM json_each(...)` expands it to two
        // columns; a bare `SELECT json_each(...)` shows the record).
        "json_each" | "jsonb_each" | "json_each_text" | "jsonb_each_text" => {
            arity(1)?;
            let jsonb = name.starts_with("jsonb");
            let as_text = name.ends_with("_text");
            let value_oid = if as_text {
                super::types::oid::TEXT
            } else if jsonb {
                super::types::oid::JSONB
            } else {
                super::types::oid::JSON
            };
            let text = match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Json { text, .. } => text,
                Datum::Text(s) => s,
                Datum::Null => return Ok(Datum::Null),
                _ => {
                    return Err(sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "cannot deconstruct a scalar"
                    ));
                }
            };
            let pairs = json_each_pairs(text, jsonb, as_text, arena)?;
            let k = hooks.srf_index.ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "set-returning function called where not allowed"
                )
            })?;
            let Some((key, value)) = pairs.get(k - 1) else {
                return Ok(Datum::Null);
            };
            let fields = arena
                .alloc_slice_copy(&[
                    super::types::RecordField {
                        name: "key",
                        type_oid: super::types::oid::TEXT,
                        value: Datum::Text(key),
                    },
                    super::types::RecordField {
                        name: "value",
                        type_oid: value_oid,
                        value: *value,
                    },
                ])
                .map_err(|_| arena_full())?;
            Ok(Datum::Record(fields))
        }
        "pg_options_to_table" => {
            arity(1)?;
            let raw = match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Array {
                    element: super::types::ArrElem::Text,
                    raw,
                } => raw,
                Datum::Null => return Ok(Datum::Null),
                other => return Err(type_mismatch("pg_options_to_table", &other)),
            };
            let index = hooks.srf_index.ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "set-returning function called where not allowed"
                )
            })?;
            let option = match super::array::get(raw, super::types::ArrElem::Text, index - 1) {
                Some(Datum::Text(option)) => option,
                Some(Datum::Null) => {
                    return Err(sql_err!(
                        sqlstate::NULL_VALUE_NOT_ALLOWED,
                        "null value not allowed"
                    ));
                }
                None => return Ok(Datum::Null),
                Some(other) => return Err(type_mismatch("pg_options_to_table", &other)),
            };
            let (option_name, option_value) = match option.split_once('=') {
                Some((name, value)) => (Datum::Text(name), Datum::Text(value)),
                None => (Datum::Text(option), Datum::Null),
            };
            let fields = arena
                .alloc_slice_copy(&[
                    super::types::RecordField {
                        name: "option_name",
                        type_oid: super::types::oid::TEXT,
                        value: option_name,
                    },
                    super::types::RecordField {
                        name: "option_value",
                        type_oid: super::types::oid::TEXT,
                        value: option_value,
                    },
                ])
                .map_err(|_| arena_full())?;
            Ok(Datum::Record(fields))
        }
        "pg_get_sequence_data" => {
            arity(1)?;
            let oid = match eval_full(args[0], arena, params, row, hooks)? {
                Datum::Int4(oid) => oid,
                Datum::Null => return Ok(Datum::Null),
                other => return Err(type_mismatch("pg_get_sequence_data", &other)),
            };
            let (last_value, is_called) = hooks
                .catalog
                .and_then(|catalog| catalog.sequence_state_by_oid(oid))
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "sequence with OID {} does not exist",
                        oid
                    )
                })?;
            if hooks.srf_index != Some(1) {
                return Ok(Datum::Null);
            }
            let fields = arena
                .alloc_slice_copy(&[
                    super::types::RecordField {
                        name: "last_value",
                        type_oid: super::types::oid::INT8,
                        value: Datum::Int8(last_value),
                    },
                    super::types::RecordField {
                        name: "is_called",
                        type_oid: super::types::oid::BOOL,
                        value: Datum::Bool(is_called),
                    },
                ])
                .map_err(|_| arena_full())?;
            Ok(Datum::Record(fields))
        }
        _ => Err(undefined_function(name, args, row)),
    }
}

/// The common type of all CASE branch results (+ ELSE), by PostgreSQL's
/// numeric-tower preference. Returns None when the branches are all
/// unknown or of a single non-unifiable class (leave values as-is).
fn case_result_type<'a>(
    whens: &[(&Expr<'a>, &Expr<'a>)],
    otherwise: &Option<&Expr<'a>>,
    row: &impl ColumnLookup<'a>,
) -> Option<ColType> {
    let mut acc: Option<ColType> = None;
    let mut mixed = false;
    let mut consider = |e: &Expr<'a>| {
        if let Some(t) = static_type(e, row) {
            acc = Some(match acc {
                None => t,
                Some(prev) => match unify_types(prev, t) {
                    Some(u) => u,
                    None => {
                        mixed = true;
                        prev
                    }
                },
            });
        }
    };
    for (_, result) in whens {
        consider(result);
    }
    if let Some(e) = otherwise {
        consider(e);
    }
    if mixed { None } else { acc }
}

/// Numeric-tower unification (int4 < int8 < numeric < float8); same type
/// unifies to itself; text unifies with text. Otherwise None.
fn unify_types(a: ColType, b: ColType) -> Option<ColType> {
    use ColType::*;
    if a == b {
        return Some(a);
    }
    let rank = |t: ColType| match t {
        Int4 => Some(1),
        Int8 => Some(2),
        Numeric => Some(3),
        Float8 => Some(4),
        _ => None,
    };
    match (rank(a), rank(b)) {
        (Some(ra), Some(rb)) => Some(if ra >= rb { a } else { b }),
        _ => None,
    }
}

/// Best-effort static type of an expression for CASE unification.
pub(crate) fn static_type_pub<'a>(e: &Expr<'a>, row: &impl ColumnLookup<'a>) -> Option<ColType> {
    static_type(e, row)
}

fn static_type<'a>(e: &Expr<'a>, row: &impl ColumnLookup<'a>) -> Option<ColType> {
    match e {
        Expr::Null | Expr::Param(_) => None,
        Expr::Bool(_) => Some(ColType::Bool),
        Expr::Int(v) => Some(if i32::try_from(*v).is_ok() {
            ColType::Int4
        } else {
            ColType::Int8
        }),
        Expr::Float(_) => Some(ColType::Float8),
        Expr::NumericLit(_) => Some(ColType::Numeric),
        Expr::Str(_) => Some(ColType::Text),
        Expr::Column { qualifier, name } => row.col_type(*qualifier, name),
        Expr::SchemaColumn { table, name, .. } => row.col_type(Some(table), name),
        Expr::Cast { type_name, .. } => ColType::from_sql_name(type_name),
        Expr::Collate { operand, .. } => static_type(operand, row),
        Expr::Array(items) => items
            .first()
            .and_then(|item| static_type(item, row))
            .and_then(|ctype| match ctype {
                ColType::Array(element) => Some(ColType::Array(element)),
                scalar => super::types::ArrElem::from_coltype(scalar).map(ColType::Array),
            }),
        Expr::Unary {
            operator: UnaryOp::Neg,
            operand,
        } => static_type(operand, row),
        Expr::Unary {
            operator: UnaryOp::Not,
            ..
        }
        | Expr::IsNull { .. }
        | Expr::InList { .. }
        | Expr::Between { .. }
        | Expr::Like { .. }
        | Expr::Match { .. } => Some(ColType::Bool),
        Expr::Binary {
            operator,
            left,
            right,
        } => match operator {
            BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
            | BinaryOp::And
            | BinaryOp::Or => Some(ColType::Bool),
            BinaryOp::Concat => Some(ColType::Text),
            _ => {
                let l = static_type(left, row)?;
                let r = static_type(right, row)?;
                unify_types(l, r)
            }
        },
        Expr::Case {
            whens, otherwise, ..
        } => case_result_type(whens, otherwise, row),
        Expr::Subscript { base, .. } => match static_type(base, row) {
            Some(ColType::Array(element)) => Some(element.to_coltype()),
            Some(ctype) if matches!(base, Expr::Subscript { .. }) => Some(ctype),
            _ => None,
        },
        _ => None,
    }
}

/// A string literal or a parameter is PostgreSQL's "unknown" type, which
/// coerces to whatever it is compared/combined with. A real typed value
/// (column, function result, cast) does not.
fn is_unknown_literal(expression: &Expr) -> bool {
    matches!(expression, Expr::Str(_) | Expr::Param(_))
}

#[allow(clippy::too_many_arguments)]
/// Bitwise combine of two `bit_and`/`bit_or`/`bit_xor` aggregate inputs, over
/// integers or bit strings, reusing the operator machinery (bit strings of
/// differing lengths error, as in PostgreSQL).
pub fn bit_aggregate<'a>(
    operator: BinaryOp,
    a: Datum<'a>,
    b: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    binary(operator, a, b, false, false, arena)
}

pub(crate) fn exclusion_operator<'a>(
    operator: BinaryOp,
    left: Datum<'a>,
    right: Datum<'a>,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    Ok(matches!(
        binary(operator, left, right, false, false, arena)?,
        Datum::Bool(true)
    ))
}

/// The result kind of a temporal `generate_series` / `date_bin`: a plain
/// timestamp, or a timestamptz (which a `date` argument resolves to, matching
/// PostgreSQL's preference for the timestamptz overload).
#[derive(Clone, Copy)]
pub enum SeriesKind {
    Timestamp,
    Timestamptz,
}

/// PostgreSQL resolves `generate_series` from its first argument's concrete
/// overload family. Keep the type decision in one place so select-list,
/// FROM, Describe, and Result encoding cannot disagree about a temporal
/// series' wire type.
pub(crate) fn generate_series_result_type(
    start: Option<ColType>,
    has_numeric: bool,
    has_int8: bool,
) -> ColType {
    if has_numeric {
        return ColType::Numeric;
    }
    if has_int8 {
        return ColType::Int8;
    }
    match start {
        Some(ColType::Timestamp) => ColType::Timestamp,
        Some(ColType::Timestamptz | ColType::Date) => ColType::Timestamptz,
        Some(ColType::Numeric) => ColType::Numeric,
        _ => ColType::Int4,
    }
}

/// Counts a numeric `generate_series` without allowing an unsupported value
/// family to enter the executor. The bounded arena is the ordinary result
/// limit; this counter additionally prevents an unbounded loop before a row
/// can be materialized.
pub(crate) fn numeric_series_count(
    start: Numeric<'_>,
    stop: Numeric<'_>,
    step: Numeric<'_>,
    arena: &Arena,
) -> Result<usize, SqlError> {
    use core::cmp::Ordering;
    if step.is_zero() || step.is_nan() {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "step size cannot equal zero"
        ));
    }
    let positive = crate::sql::numeric::compare(&step, &Numeric::ZERO) == Ordering::Greater;
    let mut value = start;
    let mut count = 0usize;
    while if positive {
        crate::sql::numeric::compare(&value, &stop) != Ordering::Greater
    } else {
        crate::sql::numeric::compare(&value, &stop) != Ordering::Less
    } {
        count += 1;
        if count > 100_000_000 {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "generate_series produces too many rows"
            ));
        }
        value = crate::sql::numeric::add(&value, &step, arena)?;
    }
    Ok(count)
}

/// The one-based numeric series value, or None after its stop bound. This is
/// shared by SELECT-list SRF expansion and FROM materialization so their row
/// count and values cannot drift.
pub(crate) fn numeric_series_at<'a>(
    start: Numeric<'a>,
    stop: Numeric<'a>,
    step: Numeric<'a>,
    index: usize,
    arena: &'a Arena,
) -> Result<Option<Numeric<'a>>, SqlError> {
    use core::cmp::Ordering;
    if index == 0 {
        return Ok(None);
    }
    if step.is_zero() || step.is_nan() {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "step size cannot equal zero"
        ));
    }
    let value = if index == 1 {
        start
    } else {
        let offset = Numeric::from_i64((index - 1) as i64, arena)?;
        let increment = crate::sql::numeric::mul(&step, &offset, arena)?;
        crate::sql::numeric::add(&start, &increment, arena)?
    };
    let positive = crate::sql::numeric::compare(&step, &Numeric::ZERO) == Ordering::Greater;
    if (positive && crate::sql::numeric::compare(&value, &stop) == Ordering::Greater)
        || (!positive && crate::sql::numeric::compare(&value, &stop) == Ordering::Less)
    {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

impl SeriesKind {
    pub fn datum<'a>(self, micros: i64) -> Datum<'a> {
        match self {
            SeriesKind::Timestamp => Datum::Timestamp(micros),
            SeriesKind::Timestamptz => Datum::Timestamptz(micros),
        }
    }

    pub fn coltype(self) -> ColType {
        match self {
            SeriesKind::Timestamp => ColType::Timestamp,
            SeriesKind::Timestamptz => ColType::Timestamptz,
        }
    }
}

/// Whether a `generate_series` interval step advances toward larger timestamps.
/// Uses PostgreSQL's canonical interval ordering (30-day months, 24-hour days).
fn interval_is_positive(step: super::types::Interval) -> bool {
    let canonical = step.months as i128 * 2_592_000_000_000
        + step.days as i128 * 86_400_000_000
        + step.micros as i128;
    canonical > 0
}

/// The number of values a temporal `generate_series(base, stop, step)` yields,
/// iterating by calendar addition. A zero step errors; a runaway series is a
/// loud error rather than an unbounded loop.
pub fn timestamp_series_count(
    base: i64,
    stop: i64,
    step: super::types::Interval,
) -> Result<usize, SqlError> {
    if step.months == 0 && step.days == 0 && step.micros == 0 {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "step size cannot equal zero"
        ));
    }
    let positive = interval_is_positive(step);
    let mut v = base;
    let mut n = 0usize;
    while if positive { v <= stop } else { v >= stop } {
        n += 1;
        // A generous backstop against a pathologically large series; real limits
        // come from the row arena when the values are materialized.
        if n > 100_000_000 {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "generate_series produces too many rows"
            ));
        }
        v = super::datetime::add_interval(v, step);
    }
    Ok(n)
}

/// The base micros and result kind of a temporal `generate_series` start value,
/// or None when it is not a date/timestamp. A `date` becomes UTC-midnight
/// timestamptz.
pub fn timestamp_series_start(d: &Datum) -> Option<(i64, SeriesKind)> {
    match d {
        Datum::Timestamp(v) => Some((*v, SeriesKind::Timestamp)),
        Datum::Timestamptz(v) => Some((*v, SeriesKind::Timestamptz)),
        Datum::Date(days) => Some((*days as i64 * 86_400_000_000, SeriesKind::Timestamptz)),
        _ => None,
    }
}

/// `json -> key/index` and `json ->> key/index`. A missing member yields NULL;
/// `->>` unwraps a JSON string to plain text.
fn json_get<'a>(
    l: Datum<'a>,
    r: Datum<'a>,
    as_text: bool,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let (text, jsonb) = match l {
        Datum::Json { text, jsonb } => (text, jsonb),
        Datum::Null => return Ok(Datum::Null),
        other => return Err(type_mismatch("-> requires json/jsonb", &other)),
    };
    if r.is_null() {
        return Ok(Datum::Null);
    }
    let tree = super::json::parse(text, arena)?;
    let child = match r {
        Datum::Text(k) => tree.get_field(k),
        Datum::Int2(i) => tree.get_index(i as i64),
        Datum::Int4(i) => tree.get_index(i as i64),
        Datum::Int8(i) => tree.get_index(i),
        other => return Err(type_mismatch("-> key must be text or integer", &other)),
    };
    let Some(child) = child else {
        return Ok(Datum::Null);
    };
    if as_text {
        // ->> renders a JSON string as its unescaped text; other values as
        // their canonical JSON.
        if let super::json::Json::Str(s) = child {
            return Ok(Datum::Text(super::json::decode_string(s, arena)?));
        }
        let mut buffer = crate::util::StackStr::<8192>::new();
        let _ = core::fmt::Write::write_fmt(
            &mut buffer,
            format_args!("{}", super::json::JsonWrite(&child)),
        );
        return Ok(Datum::Text(
            arena.alloc_str(buffer.as_str()).map_err(|_| arena_full())?,
        ));
    }
    let mut buffer = crate::util::StackStr::<8192>::new();
    let _ = core::fmt::Write::write_fmt(
        &mut buffer,
        format_args!("{}", super::json::JsonWrite(&child)),
    );
    Ok(Datum::Json {
        text: arena.alloc_str(buffer.as_str()).map_err(|_| arena_full())?,
        jsonb,
    })
}

/// Renders a `Json` value to canonical jsonb text in the arena.
/// Renders a parsed JSON node back to its canonical text, for callers outside
/// this module (set-returning-function materialization in the query layer).
pub fn json_to_text_pub<'a>(
    v: &super::json::Json<'a>,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    json_to_text(v, arena)
}

/// Decodes every element of an array blob into `items` starting at `start`,
/// coercing each to `to` (PostgreSQL promotes the element type when array
/// functions mix numeric widths). Returns the new count; errors on overflow.
fn load_array<'a>(
    raw: &'a [u8],
    from: super::types::ArrElem,
    to: super::types::ArrElem,
    items: &mut [Datum<'a>],
    start: usize,
    arena: &'a Arena,
) -> Result<usize, SqlError> {
    let mut n = start;
    let to_coltype = to.to_coltype();
    for i in 0..super::array::len(raw) {
        if n == items.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "array value too large"
            ));
        }
        let el = super::array::get(raw, from, i).unwrap_or(Datum::Null);
        items[n] = if el.is_null() || from == to {
            el
        } else {
            cast_to(el, to_coltype, arena)?
        };
        n += 1;
    }
    Ok(n)
}

fn json_to_text<'a>(v: &super::json::Json<'a>, arena: &'a Arena) -> Result<&'a str, SqlError> {
    // Render straight into the arena at exact length — a jsonb value can be
    // larger than any fixed scratch buffer, and truncating it would corrupt it.
    arena
        .alloc_str_display(super::json::JsonWrite(v))
        .map_err(|_| arena_full())
}

/// [`json_to_text`] in the compact form a `json`-typed result carries.
pub(crate) fn json_to_text_compact<'a>(
    v: &super::json::Json<'a>,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    arena
        .alloc_str_display(super::json::JsonWriteCompact(v))
        .map_err(|_| arena_full())
}

/// Expands a `(record).*` base to its fields for a projection. The runtime
/// field count matches the static shape (`exec::record_shape`) for every
/// supported record source, so describe and data-row column counts agree.
/// A null or non-composite value is rejected loudly (a `(t).*` over an
/// outer-join null row is the one shape whose width is not carried at runtime).
pub fn record_star_expand<'a>(
    base: &Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<&'a [super::types::RecordField<'a>], SqlError> {
    match eval_full(base, arena, params, row, hooks)? {
        Datum::Record(fields) | Datum::Composite { fields, .. } => Ok(fields),
        Datum::CompositeText {
            slot,
            physical_fields,
            text,
        } => {
            let catalog = hooks.catalog.ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "named composite catalog access is unavailable"
                )
            })?;
            let Datum::Composite { fields, .. } =
                catalog.materialize_composite(slot, physical_fields, text, arena)?
            else {
                unreachable!("catalog materializes named composites")
            };
            Ok(fields)
        }
        other => Err(type_mismatch(
            "record expansion of a non-composite value",
            &other,
        )),
    }
}

/// Storage rows carry named composites as canonical text. Evaluation exposes
/// the catalog-defined structural value before operators, grouping, joins, or
/// set operations can observe it, so a text representation never becomes a
/// second comparison semantics.
fn materialize_named_composite<'a>(
    value: Datum<'a>,
    hooks: &EvalHooks<'_, 'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let Datum::CompositeText {
        slot,
        physical_fields,
        text,
    } = value
    else {
        return Ok(value);
    };
    let catalog = hooks.catalog.ok_or_else(|| {
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "named composite catalog access is unavailable"
        )
    })?;
    catalog.materialize_composite(slot, physical_fields, text, arena)
}

/// Converts durable composite payloads to their current catalog layout before
/// a text output function can observe them. Arrays retain dimensions, lower
/// bounds, and domain element identity while every historical element gains
/// newly-added NULL attributes.
pub(crate) fn materialize_composite_text_output<'a>(
    value: Datum<'a>,
    catalog: &dyn CatalogAccess,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    match value {
        Datum::CompositeText {
            slot,
            physical_fields,
            text,
        } => catalog.materialize_composite(slot, physical_fields, text, arena),
        Datum::Array { element, raw } if matches!(element.to_coltype(), ColType::Composite(_)) => {
            let shape = super::array::shape(raw).expect("array datum invariant");
            let mut items = [Datum::Null; super::array::MAX_ELEMENTS];
            for (index, item) in items.iter_mut().take(shape.element_count()).enumerate() {
                *item = match super::array::get(raw, element, index).unwrap_or(Datum::Null) {
                    Datum::CompositeText {
                        slot,
                        physical_fields,
                        text,
                    } => catalog.materialize_composite(slot, physical_fields, text, arena)?,
                    value => value,
                };
            }
            Ok(Datum::Text(super::array::format_shaped(
                &items[..shape.element_count()],
                shape,
                arena,
            )?))
        }
        value => Ok(value),
    }
}

/// The `(key, value)` members a `json_each` / `jsonb_each` family call yields
/// for the object `text`. `jsonb` selects normalized (sorted, deduplicated,
/// re-rendered) members over the `json` variants' source-order/verbatim members;
/// `as_text` makes each value the `_text` form (a decoded string, else the
/// value's json text). Errors match PostgreSQL's `cannot deconstruct ...`.
pub fn json_each_pairs<'a>(
    text: &'a str,
    jsonb: bool,
    as_text: bool,
    arena: &'a Arena,
) -> Result<&'a [(&'a str, Datum<'a>)], SqlError> {
    match super::json::kind_of(text) {
        super::json::Kind::Object => {}
        super::json::Kind::Array => {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "cannot deconstruct an array as an object"
            ));
        }
        super::json::Kind::Scalar => {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "cannot deconstruct a scalar"
            ));
        }
    }
    if jsonb {
        let super::json::Json::Object(members) = super::json::parse(text, arena)? else {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "cannot deconstruct an array as an object"
            ));
        };
        let out = arena
            .alloc_slice_with(members.len(), |_| ("", Datum::Null))
            .map_err(|_| arena_full())?;
        for (slot, (key, value)) in out.iter_mut().zip(members.iter()) {
            let datum = if as_text {
                match *value {
                    super::json::Json::Str(s) => Datum::Text(super::json::decode_string(s, arena)?),
                    super::json::Json::Null => Datum::Null,
                    _ => Datum::Text(json_to_text(value, arena)?),
                }
            } else {
                Datum::Json {
                    text: json_to_text(value, arena)?,
                    jsonb: true,
                }
            };
            *slot = (*key, datum);
        }
        return Ok(&*out);
    }
    // json: source order, duplicates kept, values verbatim.
    let members = super::json::object_members_source(text, arena)?;
    let out = arena
        .alloc_slice_with(members.len(), |_| ("", Datum::Null))
        .map_err(|_| arena_full())?;
    for (slot, (key, value)) in out.iter_mut().zip(members.iter()) {
        let datum = if as_text {
            match super::json::parse(value, arena)? {
                super::json::Json::Str(s) => Datum::Text(super::json::decode_string(s, arena)?),
                super::json::Json::Null => Datum::Null,
                _ => Datum::Text(value),
            }
        } else {
            Datum::Json {
                text: value,
                jsonb: false,
            }
        };
        *slot = (*key, datum);
    }
    Ok(&*out)
}

/// `jsonb || jsonb`: merge two objects (right key wins), concatenate two
/// arrays, else concatenate as arrays wrapping any non-array operand.
fn jsonb_concat<'a>(l: Datum<'a>, r: Datum<'a>, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    use super::json::Json;
    let text_of = |d: Datum<'a>| -> Result<Option<&'a str>, SqlError> {
        match d {
            Datum::Json { text, .. } => Ok(Some(text)),
            // An unknown text literal (`'{"b":2}'`) coerces to jsonb.
            Datum::Text(s) => Ok(Some(s)),
            Datum::Null => Ok(None),
            other => Err(type_mismatch("|| requires jsonb", &other)),
        }
    };
    let (Some(lt), Some(rt)) = (text_of(l)?, text_of(r)?) else {
        return Ok(Datum::Null);
    };
    let lj = super::json::parse(lt, arena)?;
    let rj = super::json::parse(rt, arena)?;
    let merged = match (&lj, &rj) {
        (Json::Object(a), Json::Object(b)) => {
            // Concatenate then re-sort/dedup (last wins) by re-serializing an
            // object literal through the parser.
            let mut buffer = crate::util::StackStr::<32768>::new();
            let _ = core::fmt::Write::write_str(&mut buffer, "{");
            let mut first = true;
            for (k, v) in a.iter().chain(b.iter()) {
                if !first {
                    let _ = core::fmt::Write::write_str(&mut buffer, ",");
                }
                first = false;
                let _ = super::json::write_json_raw_string(k, &mut buffer);
                let _ = core::fmt::Write::write_str(&mut buffer, ":");
                let _ = core::fmt::Write::write_fmt(
                    &mut buffer,
                    format_args!("{}", super::json::JsonWrite(v)),
                );
            }
            let _ = core::fmt::Write::write_str(&mut buffer, "}");
            let owned = arena.alloc_str(buffer.as_str()).map_err(|_| arena_full())?;
            return Ok(Datum::Json {
                text: json_to_text(&super::json::parse(owned, arena)?, arena)?,
                jsonb: true,
            });
        }
        (Json::Array(a), Json::Array(b)) => {
            let items = arena
                .alloc_slice_with(a.len() + b.len(), |_| Json::Null)
                .map_err(|_| arena_full())?;
            items[..a.len()].copy_from_slice(a);
            items[a.len()..].copy_from_slice(b);
            Json::Array(items)
        }
        // Non-array || anything (or vice-versa): each non-array becomes a
        // one-element array, then concatenate.
        _ => {
            let as_items = |j: &Json<'a>| -> &'a [Json<'a>] {
                match j {
                    Json::Array(items) => items,
                    _ => core::slice::from_ref(arena.alloc(*j).expect("arena")),
                }
            };
            let (ai, bi) = (as_items(&lj), as_items(&rj));
            let items = arena
                .alloc_slice_with(ai.len() + bi.len(), |_| Json::Null)
                .map_err(|_| arena_full())?;
            items[..ai.len()].copy_from_slice(ai);
            items[ai.len()..].copy_from_slice(bi);
            Json::Array(items)
        }
    };
    Ok(Datum::Json {
        text: json_to_text(&merged, arena)?,
        jsonb: true,
    })
}

/// `json #> path` / `#>>`: extract the value at a `text[]` path.
/// Extracts a JSON path (`text[]`, or an unknown `'{a,b}'` literal) into its
/// string parts, for `jsonb_set` / `jsonb_insert` / `#-`.
fn json_path_parts<'a>(r: Datum<'a>, arena: &'a Arena) -> Result<&'a [&'a str], SqlError> {
    let (element, raw) = match r {
        Datum::Array { element, raw } => (element, raw),
        Datum::Text(lit) => (
            super::types::ArrElem::Text,
            super::array::parse_literal(lit, super::types::ArrElem::Text, arena)?,
        ),
        other => return Err(type_mismatch("path must be a text array", &other)),
    };
    let n = super::array::len(raw);
    if n > 64 {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "JSON path too long"
        ));
    }
    let mut buffer = [""; 64];
    for (i, slot) in buffer[..n].iter_mut().enumerate() {
        *slot = match super::array::get(raw, element, i) {
            Some(Datum::Text(s)) => s,
            _ => {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "path element is not text"
                ));
            }
        };
    }
    Ok(&*arena
        .alloc_slice_copy(&buffer[..n])
        .map_err(|_| arena_full())?)
}

/// `jsonb - text`/`text[]`/`integer`: delete a key, several keys, or an element.
fn jsonb_delete<'a>(l: Datum<'a>, r: Datum<'a>, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let Datum::Json { text, .. } = l else {
        return Err(type_mismatch("- requires jsonb", &l));
    };
    let root = super::json::parse(text, arena)?;
    let result = match r {
        Datum::Null => return Ok(Datum::Null),
        Datum::Text(key) => super::json::delete_key(root, key, arena)?,
        Datum::Int2(i) => super::json::delete_index(root, i as i64, arena)?,
        Datum::Int4(i) => super::json::delete_index(root, i as i64, arena)?,
        Datum::Int8(i) => super::json::delete_index(root, i, arena)?,
        Datum::Array { element, raw } => {
            // `jsonb - text[]`: delete each named key.
            let mut node = root;
            for i in 0..super::array::len(raw) {
                if let Some(Datum::Text(key)) = super::array::get(raw, element, i) {
                    node = super::json::delete_key(node, key, arena)?;
                }
            }
            node
        }
        other => return Err(type_mismatch("- requires text, text[], or integer", &other)),
    };
    Ok(Datum::Json {
        text: json_to_text(&result, arena)?,
        jsonb: true,
    })
}

/// `jsonb #- text[]`: delete the value at a path.
fn jsonb_delete_path<'a>(
    l: Datum<'a>,
    r: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let text = match l {
        Datum::Json { text, .. } => text,
        Datum::Null => return Ok(Datum::Null),
        other => return Err(type_mismatch("#- requires jsonb", &other)),
    };
    if r.is_null() {
        return Ok(Datum::Null);
    }
    let root = super::json::parse(text, arena)?;
    let path = json_path_parts(r, arena)?;
    let result = super::json::delete_path(root, path, arena)?;
    Ok(Datum::Json {
        text: json_to_text(&result, arena)?,
        jsonb: true,
    })
}

/// Parses a json/jsonb argument (or unknown text literal) into a tree.
fn json_tree_arg<'a>(d: Datum<'a>, arena: &'a Arena) -> Result<super::json::Json<'a>, SqlError> {
    match d {
        Datum::Json { text, .. } => super::json::parse(text, arena),
        Datum::Text(s) => super::json::parse(s, arena),
        other => Err(type_mismatch("argument is not jsonb", &other)),
    }
}

fn json_path<'a>(
    l: Datum<'a>,
    r: Datum<'a>,
    as_text: bool,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let (text, jsonb) = match l {
        Datum::Json { text, jsonb } => (text, jsonb),
        Datum::Null => return Ok(Datum::Null),
        other => return Err(type_mismatch("#> requires json/jsonb", &other)),
    };
    // The path is a `text[]`; an unknown literal (`'{a,b}'`) arrives as text
    // and is parsed as a text-array literal, as PostgreSQL coerces it.
    let (element, raw) = match r {
        Datum::Array { element, raw } => (element, raw),
        Datum::Text(lit) => (
            super::types::ArrElem::Text,
            super::array::parse_literal(lit, super::types::ArrElem::Text, arena)?,
        ),
        Datum::Null => return Ok(Datum::Null),
        other => return Err(type_mismatch("#> path must be a text array", &other)),
    };
    let mut node = super::json::parse(text, arena)?;
    for i in 0..super::array::len(raw) {
        let step = super::array::get(raw, element, i).unwrap_or(Datum::Null);
        let Datum::Text(key) = step else {
            return Ok(Datum::Null);
        };
        let next = match &node {
            super::json::Json::Object(_) => node.get_field(key),
            super::json::Json::Array(_) => key.parse::<i64>().ok().and_then(|n| node.get_index(n)),
            _ => None,
        };
        let Some(next) = next else {
            return Ok(Datum::Null);
        };
        node = next;
    }
    if as_text {
        if let super::json::Json::Str(str_value) = node {
            return Ok(Datum::Text(super::json::decode_string(str_value, arena)?));
        }
        if matches!(node, super::json::Json::Null) {
            return Ok(Datum::Null);
        }
        return Ok(Datum::Text(json_to_text(&node, arena)?));
    }
    Ok(Datum::Json {
        text: json_to_text(&node, arena)?,
        jsonb,
    })
}

/// `jsonb ? key` / `?|` / `?&`: key/element existence tests.
fn json_exists<'a>(
    operator: super::ast::BinaryOp,
    l: Datum<'a>,
    r: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    use super::ast::BinaryOp::{JsonExistsAll, JsonExistsAny};
    use super::json::Json;
    let text = match l {
        Datum::Json { text, .. } => text,
        Datum::Null => return Ok(Datum::Null),
        other => return Err(type_mismatch("? requires jsonb", &other)),
    };
    let node = super::json::parse(text, arena)?;
    // Does a single string key exist (object key, or array string element)?
    let has = |key: &str| -> bool {
        match &node {
            Json::Object(members) => members.iter().any(|(k, _)| *k == key),
            Json::Array(items) => items
                .iter()
                .any(|it| matches!(it, Json::Str(s) if *s == key)),
            _ => false,
        }
    };
    match operator {
        super::ast::BinaryOp::JsonExists => {
            let Datum::Text(key) = r else {
                if r.is_null() {
                    return Ok(Datum::Null);
                }
                return Err(type_mismatch("? key must be text", &r));
            };
            Ok(Datum::Bool(has(key)))
        }
        JsonExistsAny | JsonExistsAll => {
            let (element, raw) = match r {
                Datum::Array { element, raw } => (element, raw),
                Datum::Text(lit) => (
                    super::types::ArrElem::Text,
                    super::array::parse_literal(lit, super::types::ArrElem::Text, arena)?,
                ),
                Datum::Null => return Ok(Datum::Null),
                other => return Err(type_mismatch("?|/?& require a text array", &other)),
            };
            let n = super::array::len(raw);
            let all = operator == JsonExistsAll;
            let mut result = all;
            for i in 0..n {
                let key = super::array::get(raw, element, i).unwrap_or(Datum::Null);
                let present = matches!(key, Datum::Text(k) if has(k));
                if all {
                    result = result && present;
                } else if present {
                    result = true;
                    break;
                }
            }
            Ok(Datum::Bool(result))
        }
        _ => unreachable!("json_exists only handles ?, ?|, ?&"),
    }
}

/// Evaluates `left AND right` / `left OR right` with PostgreSQL's short-circuit
/// semantics. The *absorbing* value is FALSE for AND, TRUE for OR. PostgreSQL
/// simplifies `x AND FALSE` / `x OR TRUE` at plan time — dropping `x` even when
/// it would error, and even when the settling value is nested (`A AND (FALSE
/// AND c)` drops `A`) — but is otherwise strict left-to-right: `(1/a=1) AND
/// (b>0)` errors on the division, it does not swallow it because `b>0` is not
/// statically FALSE. `fold_check` decides statically (surfacing a constant
/// operand's own error left-first, exactly as plan-time folding does).
fn eval_logic_short_circuit<'a>(
    operator: BinaryOp,
    left: &Expr<'a>,
    right: &Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Datum<'a>, SqlError> {
    let absorbing = matches!(operator, BinaryOp::Or);
    // Left first: a statically-determined left settles the result (absorbing) or
    // hands offset to the right (non-absorbing), matching plan-time folding order.
    let context = if absorbing { "OR" } else { "AND" };
    check_boolean_operand(left, row, context)?;
    check_boolean_operand(right, row, context)?;
    match fold_check(left, arena)? {
        Some(b) if b == absorbing => return Ok(Datum::Bool(absorbing)),
        Some(_) => return boolean_argument(eval_full(right, arena, params, row, hooks)?, context),
        None => {}
    }
    // Left is runtime; if the right statically folds to the absorbing value it
    // settles the result and drops the (possibly-erroring) left.
    match fold_check(right, arena)? {
        Some(b) if b == absorbing => return Ok(Datum::Bool(absorbing)),
        Some(_) => return boolean_argument(eval_full(left, arena, params, row, hooks)?, context),
        None => {}
    }
    let l = boolean_argument(eval_full(left, arena, params, row, hooks)?, context)?;
    if matches!(l, Datum::Bool(b) if b == absorbing) {
        return Ok(Datum::Bool(absorbing));
    }
    let r = boolean_argument(eval_full(right, arena, params, row, hooks)?, context)?;
    logic(operator, l, r)
}

/// Resolves `array || NULL` / `NULL || array`, which PostgreSQL decides from
/// the NULL operand's static type: an untyped NULL or a NULL of the array type
/// is the identity (returns the array), a NULL of the element type appends a
/// NULL element, and any other type is an undefined operator. Returns `None`
/// when this is not an array-with-NULL concatenation (fall through to `concat`).
fn array_null_concat<'a>(
    l: Datum<'a>,
    r: Datum<'a>,
    left: &Expr<'a>,
    right: &Expr<'a>,
    row: &impl ColumnLookup<'a>,
    arena: &'a Arena,
) -> Result<Option<Datum<'a>>, SqlError> {
    let (array, element, null_expr) = match (l, r) {
        (Datum::Array { element, .. }, Datum::Null) => (l, element, right),
        (Datum::Null, Datum::Array { element, .. }) => (r, element, left),
        _ => return Ok(None),
    };
    match static_type(null_expr, row) {
        // Untyped NULL or a NULL of the array type: identity.
        None | Some(ColType::Array(_)) => Ok(Some(array)),
        // NULL of the element type: append/prepend a NULL element.
        Some(t) if super::types::ArrElem::from_coltype(t) == Some(element) => {
            Ok(Some(array_concat(l, r, arena)?))
        }
        Some(t) => Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "operator does not exist: {}[] || {}",
            element.to_coltype().name(),
            t.name()
        )),
    }
}

fn concat<'a>(
    l: Datum<'a>,
    r: Datum<'a>,
    l_unknown: bool,
    r_unknown: bool,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if l.is_null() || r.is_null() {
        return Ok(Datum::Null);
    }
    // `||` on arrays concatenates: array||array, and array||element or
    // element||array append/prepend the element. An *unknown literal* on the
    // scalar side is resolved as array||array (PostgreSQL casts it to the array
    // type), so it is parsed as an array literal and errors if malformed —
    // matching `ARRAY['a','b'] || 'c'` (error) vs `|| 'c'::text` (append).
    let arr_elem = match (&l, &r) {
        (Datum::Array { element, .. }, _) | (_, Datum::Array { element, .. }) => Some(*element),
        _ => None,
    };
    if let Some(element) = arr_elem {
        let coerce = |d: Datum<'a>, unknown: bool| -> Result<Datum<'a>, SqlError> {
            match d {
                Datum::Text(s) if unknown => Ok(Datum::Array {
                    element,
                    raw: super::array::parse_literal(s, element, arena)?,
                }),
                other => Ok(other),
            }
        };
        return array_concat(coerce(l, l_unknown)?, coerce(r, r_unknown)?, arena);
    }
    let left = cast_to_text(l, arena)?;
    let right = cast_to_text(r, arena)?;
    let bytes = arena
        .alloc_slice_with(left.len() + right.len(), |i| {
            if i < left.len() {
                left.as_bytes()[i]
            } else {
                right.as_bytes()[i - left.len()]
            }
        })
        .map_err(|_| arena_full())?;
    Ok(Datum::Text(unsafe {
        core::str::from_utf8_unchecked(bytes)
    }))
}

/// Concatenates two operands where at least one is an array, following
/// PostgreSQL's `array || array`, `array || element`, and `element || array`.
fn array_concat<'a>(l: Datum<'a>, r: Datum<'a>, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let element = match (&l, &r) {
        (Datum::Array { element: left, .. }, Datum::Array { element: right, .. }) => {
            unify_arr_elem(*left, *right)
        }
        (Datum::Array { element, .. }, scalar) | (scalar, Datum::Array { element, .. }) => {
            ArrElem::from_datum(scalar).map_or(*element, |scalar| unify_arr_elem(*element, scalar))
        }
        _ => unreachable!("caller ensures one side is an array"),
    };
    let left_shape = match l {
        Datum::Array { raw, .. } => Some(super::array::shape(raw).expect("array datum invariant")),
        _ => None,
    };
    let right_shape = match r {
        Datum::Array { raw, .. } => Some(super::array::shape(raw).expect("array datum invariant")),
        _ => None,
    };
    let result_shape = match (left_shape, right_shape) {
        (Some(left), Some(right)) if left.dimension_count() == 0 => right,
        (Some(left), Some(right)) if right.dimension_count() == 0 => left,
        (Some(left), Some(right)) => {
            if left.dimension_count() != right.dimension_count()
                || (1..left.dimension_count())
                    .any(|index| left.dimension(index) != right.dimension(index))
            {
                return Err(sql_err!(
                    sqlstate::ARRAY_SUBSCRIPT_ERROR,
                    "cannot concatenate incompatible multidimensional arrays"
                ));
            }
            left.without_first()?.with_first(
                left.dimension(0).unwrap() + right.dimension(0).unwrap(),
                left.lower_bound(0).unwrap(),
            )?
        }
        (Some(shape), None) => {
            if shape.dimension_count() > 1 {
                return Err(sql_err!(
                    sqlstate::ARRAY_SUBSCRIPT_ERROR,
                    "cannot append an element to a multidimensional array"
                ));
            }
            let lower = shape.lower_bound(0).unwrap_or(1);
            super::array::Shape::new(&[shape.element_count() + 1], &[lower])?
        }
        (None, Some(shape)) => {
            if shape.dimension_count() > 1 {
                return Err(sql_err!(
                    sqlstate::ARRAY_SUBSCRIPT_ERROR,
                    "cannot prepend an element to a multidimensional array"
                ));
            }
            let lower = shape.lower_bound(0).unwrap_or(1);
            super::array::Shape::new(&[shape.element_count() + 1], &[lower])?
        }
        (None, None) => unreachable!("caller ensures an array operand"),
    };
    let mut items = [Datum::Null; super::array::MAX_ELEMENTS];
    let mut n = 0usize;
    for side in [l, r] {
        match side {
            Datum::Array { raw, element: e } => {
                for i in 0..super::array::len(raw) {
                    if n >= items.len() {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "array size exceeds the maximum allowed"
                        ));
                    }
                    let value = super::array::get(raw, e, i).ok_or_else(|| {
                        sql_err!(sqlstate::INTERNAL_ERROR, "corrupt array element")
                    })?;
                    items[n] = if value.is_null() || e == element {
                        value
                    } else {
                        cast_to(value, element.to_coltype(), arena)?
                    };
                    n += 1;
                }
            }
            scalar => {
                if n >= items.len() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "array size exceeds the maximum allowed"
                    ));
                }
                items[n] = if scalar.is_null() {
                    scalar
                } else {
                    cast_to(scalar, element.to_coltype(), arena)?
                };
                n += 1;
            }
        }
    }
    Ok(Datum::Array {
        element,
        raw: super::array::build_shaped(&items[..n], result_shape, arena)?,
    })
}

/// Converts a temporal datum to microseconds from the PostgreSQL epoch, as the
/// symbolic-age functions need. A date is taken at midnight.
fn timestamp_micros(name: &str, d: Datum) -> Result<i64, SqlError> {
    match d {
        Datum::Timestamp(t) | Datum::Timestamptz(t) => Ok(t),
        Datum::Date(day) => Ok(i64::from(day) * 86_400_000_000),
        other => Err(type_mismatch(name, &other)),
    }
}

/// A numeric scaling factor for `interval * n` / `interval / n` (integer,
/// double, or numeric). Text and other types are not factors.
fn num_factor(d: &Datum) -> Option<f64> {
    match d {
        Datum::Int2(x) => Some(f64::from(*x)),
        Datum::Int4(x) => Some(f64::from(*x)),
        Datum::Oid(x) => Some(f64::from(*x)),
        Datum::Int8(x) => Some(*x as f64),
        Datum::Float4(x) => Some(f64::from(*x)),
        Datum::Float8(x) => Some(*x),
        Datum::Numeric(n) => Some(n.to_f64()),
        _ => None,
    }
}

/// The static counterpart of [`boolean_argument`], for an operand a
/// short-circuit is about to drop. PostgreSQL type-checks both arguments of
/// AND/OR during parse analysis, so `true OR 1` is refused even though nothing
/// would evaluate the `1`; only a *runtime* error is what short-circuiting
/// spares an operand from. An operand whose type is not statically known is
/// left to the runtime check.
fn check_boolean_operand<'a>(
    expression: &Expr<'a>,
    row: &impl ColumnLookup<'a>,
    context: &str,
) -> Result<(), SqlError> {
    match static_type(expression, row) {
        Some(ColType::Bool) | None => Ok(()),
        // An unknown-type literal is read as a boolean rather than refused for
        // its type — and reading it is what reports one that is not a boolean
        // at all, which PostgreSQL also does before any short-circuit.
        Some(_) if is_unknown_literal(expression) => match expression {
            Expr::Str(text) => parse_bool(text).map(|_| ()),
            _ => Ok(()),
        },
        Some(other) => Err(sql_err!(
            crate::sql::eval::sqlstate::DATATYPE_MISMATCH,
            "argument of {} must be type boolean, not type {}",
            context,
            other.name()
        )),
    }
}

/// A value used where SQL requires a boolean — an AND/OR operand, a NOT
/// operand, a `CASE WHEN` condition. PostgreSQL accepts a boolean, a NULL, and
/// an unknown-type literal it can read as one (`'yes'`), and refuses every
/// other type by name rather than treating it as truthy. `context` names the
/// construct, as PostgreSQL's message does.
pub(crate) fn boolean_argument<'a>(v: Datum<'a>, context: &str) -> Result<Datum<'a>, SqlError> {
    match v {
        Datum::Null | Datum::Bool(_) => Ok(v),
        Datum::Text(s) => Ok(Datum::Bool(parse_bool(s)?)),
        other => Err(sql_err!(
            crate::sql::eval::sqlstate::DATATYPE_MISMATCH,
            "argument of {} must be type boolean, not type {}",
            context,
            type_name_of(&other)
        )),
    }
}

fn parse_bool(s: &str) -> Result<bool, SqlError> {
    // Accepted spellings per PostgreSQL's boolean input, case-insensitive.
    let t = s.trim();
    if ["t", "true", "yes", "on", "1"]
        .iter()
        .any(|w| t.eq_ignore_ascii_case(w))
    {
        Ok(true)
    } else if ["f", "false", "no", "off", "0"]
        .iter()
        .any(|w| t.eq_ignore_ascii_case(w))
    {
        Ok(false)
    } else {
        Err(bad_text(s, "boolean"))
    }
}

/// Promotes an integer or numeric datum to Numeric (arena-allocated).
fn to_numeric<'a>(d: &Datum, arena: &'a Arena) -> Result<Numeric<'a>, SqlError> {
    match d {
        Datum::Numeric(n) => Ok(Numeric {
            sign: n.sign,
            weight: n.weight,
            dscale: n.dscale,
            // Re-alloc digit bytes into this arena scope.
            digits: arena
                .alloc_slice_copy(n.digits)
                .map_err(|_| overflow("numeric"))?,
        }),
        Datum::Int2(x) => Numeric::from_i64(*x as i64, arena),
        Datum::Int4(x) => Numeric::from_i64(*x as i64, arena),
        Datum::Oid(x) => Numeric::from_i64(i64::from(*x), arena),
        Datum::Int8(x) => Numeric::from_i64(*x, arena),
        other => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "cannot use {:?} as numeric",
            other
        )),
    }
}

/// PostgreSQL type name for a datum, for operator-error messages.
/// PostgreSQL's `interval_cmp_value`: the canonical microsecond magnitude used
/// to order intervals, counting a month as 30 days and a day as 24 hours. i128
/// keeps the full range exact.
fn interval_cmp_value(interval: super::types::Interval) -> i128 {
    i128::from(interval.months) * 30 * 86_400_000_000
        + i128::from(interval.days) * 86_400_000_000
        + i128::from(interval.micros)
}

/// `EXTRACT` / `date_part` on an interval, decomposing its `(months, days,
/// micros)` components exactly as PostgreSQL's `interval2tm` does (truncating
/// division toward zero, so negative intervals split the same way). Hours are
/// not rolled into days, and the year-scaled fields (decade/century/millennium)
/// use plain division, not the AD/BC-adjusted timestamp rule. `numeric_result`
/// selects `EXTRACT` (numeric) over `date_part` (double precision).
fn interval_extract<'a>(
    numeric_result: bool,
    field: &str,
    interval: super::types::Interval,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    use super::numeric::Numeric;
    let eq = |k: &str| field.eq_ignore_ascii_case(k);
    let months = i64::from(interval.months);
    let days = i64::from(interval.days);
    let micros = interval.micros;
    let year = months / 12;
    let hour = micros / 3_600_000_000;
    let after_hour = micros % 3_600_000_000;
    let minute = after_hour / 60_000_000;
    let sub_minute = after_hour % 60_000_000; // whole seconds + fractional micros
    let int_val: Option<i64> = if eq("year") || eq("years") {
        Some(year)
    } else if eq("month") || eq("months") {
        Some(months % 12)
    } else if eq("day") || eq("days") {
        Some(days)
    } else if eq("hour") || eq("hours") {
        Some(hour)
    } else if eq("minute") || eq("minutes") {
        Some(minute)
    } else if eq("microseconds") {
        Some(sub_minute)
    } else if eq("decade") || eq("decades") {
        Some(year / 10)
    } else if eq("century") || eq("centuries") {
        Some(year / 100)
    } else if eq("millennium") || eq("millennia") {
        Some(year / 1000)
    } else if eq("quarter") {
        Some((months % 12) / 3 + 1)
    } else {
        None
    };
    if let Some(v) = int_val {
        return Ok(if numeric_result {
            Datum::Numeric(Numeric::from_i64(v, arena)?)
        } else {
            Datum::Float8(v as f64)
        });
    }
    // Fractional fields carried in microseconds, with PostgreSQL's per-unit
    // display scale (seconds/epoch → 6 fractional digits, milliseconds → 3).
    // `epoch` scales whole years by 365.25 days and residual months by 30 days
    // (PostgreSQL's DAYS_PER_YEAR / DAYS_PER_MONTH); i128 keeps it exact.
    let (value_micros, divisor, decimals): (i128, i128, usize) = if eq("second") || eq("seconds") {
        (i128::from(sub_minute), 1_000_000, 6)
    } else if eq("milliseconds") {
        (i128::from(sub_minute), 1_000, 3)
    } else if eq("epoch") {
        let epoch = (i128::from(months) / 12) * 31_557_600_000_000
            + (i128::from(months) % 12) * 2_592_000_000_000
            + i128::from(days) * 86_400_000_000
            + i128::from(micros);
        (epoch, 1_000_000, 6)
    } else {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "unit \"{}\" not supported for type interval",
            field
        ));
    };
    if numeric_result {
        let neg = value_micros < 0;
        let magnitude = value_micros.unsigned_abs();
        let text = stack_format!(
            48,
            "{}{}.{:0width$}",
            if neg { "-" } else { "" },
            magnitude / divisor as u128,
            magnitude % divisor as u128,
            width = decimals
        );
        Ok(Datum::Numeric(Numeric::parse(text.as_str(), arena)?))
    } else {
        Ok(Datum::Float8(value_micros as f64 / divisor as f64))
    }
}

/// The session zone's offset (seconds east) in effect at an instant — DST means
/// the answer depends on when.
fn session_zone_at(utc_micros: i64) -> i32 {
    super::timezone::session().resolve(utc_micros).0
}

/// The text a bpchar value presents to any text-typed context: PostgreSQL's
/// bpchar-to-text cast strips the blank padding, and every function or
/// operator without a bpchar-specific form receives the value through that
/// cast. Other datums pass through untouched.
pub(crate) fn text_view(d: Datum<'_>) -> Datum<'_> {
    match d {
        Datum::Bpchar(s) => Datum::Text(s.trim_end_matches(' ')),
        Datum::Regtype { name, .. } => Datum::Text(name),
        other => other,
    }
}

/// `oid::regtype`: the canonical SQL name of the type an OID names. An OID no
/// type carries renders as the number itself, and 0 as `-`.
pub(crate) fn regtype_of_oid<'a>(o: i64, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let referenced_oid = i32::try_from(o).map_err(|_| overflow("regtype"))?;
    let name = if o == 0 {
        "-"
    } else if let Some(name) = regtype_builtin_name(referenced_oid) {
        name
    } else if let Some(ctype) = crate::sql::exec::coltype_of_oid_pub(referenced_oid) {
        ctype.name()
    } else {
        return arena
            .alloc_str_display(o)
            .map(|name| Datum::Regtype {
                referenced_oid,
                name,
            })
            .map_err(|_| arena_full());
    };
    Ok(Datum::Regtype {
        referenced_oid,
        name,
    })
}

pub(crate) fn regobject_cast<'a>(
    value: Datum<'a>,
    target: ColType,
    catalog: Option<&dyn CatalogAccess>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if value.is_null() {
        return Ok(Datum::Null);
    }
    let object_oid = match value {
        Datum::RegObject {
            type_oid,
            referenced_oid,
            ..
        } if type_oid == target.oid() => referenced_oid,
        Datum::RegObject { .. } => return Err(cast_unsupported(&value, target.name())),
        Datum::Int2(value) => i32::from(value),
        Datum::Int4(value) => value,
        Datum::Oid(value) => i32::try_from(value).map_err(|_| overflow(target.name()))?,
        Datum::Int8(value) => i32::try_from(value).map_err(|_| overflow(target.name()))?,
        Datum::Text(name) | Datum::Bpchar(name) => {
            let name = name.trim_end_matches(' ');
            if let Ok(value) = name.parse::<i32>() {
                value
            } else {
                let catalog = catalog.ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "{} input requires catalog access",
                        target.name()
                    )
                })?;
                match target {
                    ColType::Regclass => catalog.reloid(name).ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_TABLE,
                            "relation \"{}\" does not exist",
                            name
                        )
                    })?,
                    ColType::Regrole => catalog.role_oid(name).ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "role \"{}\" does not exist",
                            name
                        )
                    })?,
                    ColType::Regnamespace => catalog.schema_oid(name).ok_or_else(|| {
                        sql_err!(
                            sqlstate::INVALID_SCHEMA_NAME,
                            "schema \"{}\" does not exist",
                            name
                        )
                    })?,
                    ColType::Regproc | ColType::Regprocedure => catalog
                        .routine_oid(name, target == ColType::Regprocedure)?
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::UNDEFINED_FUNCTION,
                                "function \"{}\" does not exist",
                                name
                            )
                        })?,
                    ColType::Regoper | ColType::Regoperator => catalog
                        .operator_oid(name, target == ColType::Regoperator)?
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::UNDEFINED_FUNCTION,
                                "operator \"{}\" does not exist",
                                name
                            )
                        })?,
                    _ => {
                        return Err(sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "named {} input is not modeled",
                            target.name()
                        ));
                    }
                }
            }
        }
        _ => return Err(cast_unsupported(&value, target.name())),
    };
    let name = match target {
        ColType::Regclass => catalog
            .map(|catalog| catalog.relname(object_oid, arena))
            .transpose()?
            .flatten(),
        ColType::Regrole => catalog
            .map(|catalog| catalog.role_name(object_oid, arena))
            .transpose()?
            .flatten(),
        ColType::Regnamespace => catalog
            .map(|catalog| catalog.schema_name(object_oid, arena))
            .transpose()?
            .flatten(),
        ColType::Regproc | ColType::Regprocedure => catalog
            .map(|catalog| catalog.routine_name(object_oid, target == ColType::Regprocedure, arena))
            .transpose()?
            .flatten(),
        ColType::Regoper | ColType::Regoperator => catalog
            .map(|catalog| catalog.operator_name(object_oid, target == ColType::Regoperator, arena))
            .transpose()?
            .flatten(),
        _ => None,
    };
    let name = match name {
        Some(name) => name,
        None => arena
            .alloc_str_display(object_oid)
            .map_err(|_| arena_full())?,
    };
    Ok(Datum::RegObject {
        type_oid: target.oid(),
        referenced_oid: object_oid,
        name,
    })
}

/// Resolves each text or already-typed member of a `reg*` array at the same
/// catalog boundary as the scalar input function.  Keeping the array shape
/// while rebuilding values avoids accepting unresolved object names into a
/// durable array datum.
pub(crate) fn reg_array_cast<'a>(
    value: Datum<'a>,
    target: ArrElem,
    catalog: Option<&dyn CatalogAccess>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    debug_assert!(target.is_catalog_reference());
    let (source, raw) = match value {
        Datum::Array { element, raw } => (element, raw),
        Datum::Text(text) | Datum::Bpchar(text) => (
            ArrElem::Text,
            crate::sql::array::parse_literal(text.trim_end_matches(' '), ArrElem::Text, arena)?,
        ),
        Datum::Null => return Ok(Datum::Null),
        other => return Err(cast_unsupported(&other, target.array_name())),
    };
    let shape = crate::sql::array::shape(raw).expect("array datum carries a valid shape");
    let count = shape.element_count();
    let mut items = [Datum::Null; crate::sql::array::MAX_ELEMENTS];
    let element_type = target.to_coltype();
    for (index, output) in items.iter_mut().take(count).enumerate() {
        let input = crate::sql::array::get(raw, source, index).unwrap_or(Datum::Null);
        *output = if element_type == ColType::Regtype {
            cast_to(input, element_type, arena)?
        } else {
            regobject_cast(input, element_type, catalog, arena)?
        };
    }
    Ok(Datum::Array {
        element: target,
        raw: crate::sql::array::build_shaped(&items[..count], shape, arena)?,
    })
}

fn regtype_builtin_name(type_oid: i32) -> Option<&'static str> {
    use crate::sql::types::oid;
    Some(match type_oid {
        oid::REGPROC => "regproc",
        oid::REGPROCEDURE => "regprocedure",
        oid::REGOPER => "regoper",
        oid::REGOPERATOR => "regoperator",
        oid::REGCLASS => "regclass",
        oid::REGTYPE => "regtype",
        oid::REGNAMESPACE => "regnamespace",
        oid::REGROLE => "regrole",
        _ => return None,
    })
}

/// `'typename'::regtype`: resolves a spelled type name — with any `(...)`
/// modifier ignored, as PostgreSQL ignores it — to its canonical SQL name.
/// The serial pseudo-names are not types and are refused (42704), matching
/// PostgreSQL; `name`, `oid` and the `reg*` identifiers resolve to themselves.
pub(crate) fn regtype_of_name<'a>(spelled: &str) -> Result<Datum<'a>, SqlError> {
    let mut base = spelled.trim();
    if let Some(open) = base.find('(') {
        // `varchar(5)` names varchar; the modifier plays no part.
        let close = base.rfind(')').unwrap_or(base.len());
        let tail = base[close..].trim_start_matches(')').trim();
        if tail.is_empty() {
            base = base[..open].trim_end();
        }
    }
    let mut lowered = crate::util::StackStr::<128>::new();
    use core::fmt::Write as _;
    for c in base.chars() {
        let _ = lowered.write_char(c.to_ascii_lowercase());
    }
    let unknown = || {
        sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "type \"{}\" does not exist",
            spelled.trim()
        )
    };
    if lowered.is_truncated() {
        return Err(unknown());
    }
    // Collapse interior whitespace runs so `timestamp   with time zone` reads.
    let mut collapsed = crate::util::StackStr::<128>::new();
    let mut last_space = false;
    for c in lowered.as_str().chars() {
        if c.is_whitespace() {
            if !last_space {
                let _ = collapsed.write_char(' ');
            }
            last_space = true;
        } else {
            let _ = collapsed.write_char(c);
            last_space = false;
        }
    }
    let canonical = match collapsed.as_str() {
        // These spell types the engine models; render via the one name table.
        "serial" | "serial2" | "serial4" | "serial8" | "smallserial" | "bigserial" => {
            return Err(unknown());
        }
        "timestamp without time zone" | "timestamp" => "timestamp without time zone",
        "timestamp with time zone" | "timestamptz" => "timestamp with time zone",
        "time without time zone" | "time" => "time without time zone",
        "time with time zone" | "timetz" => "time with time zone",
        // Identifier/object types render as themselves.
        "oid" => "oid",
        s @ ("regtype" | "regclass" | "regproc" | "regprocedure" | "regrole" | "regnamespace"
        | "regoper" | "regoperator") => match s {
            "regtype" => "regtype",
            "regclass" => "regclass",
            "regproc" => "regproc",
            "regprocedure" => "regprocedure",
            "regrole" => "regrole",
            "regnamespace" => "regnamespace",
            "regoper" => "regoper",
            "regoperator" => "regoperator",
            _ => unreachable!(),
        },
        other => match ColType::from_sql_name(other) {
            Some(ct) => ct.name(),
            None => return Err(unknown()),
        },
    };
    let referenced_oid = match canonical {
        "timestamp without time zone" => crate::sql::types::oid::TIMESTAMP,
        "timestamp with time zone" => crate::sql::types::oid::TIMESTAMPTZ,
        "time without time zone" => crate::sql::types::oid::TIME,
        "time with time zone" => crate::sql::types::oid::TIMETZ,
        "regtype" => crate::sql::types::oid::REGTYPE,
        "regclass" => crate::sql::types::oid::REGCLASS,
        "regproc" => crate::sql::types::oid::REGPROC,
        "regprocedure" => crate::sql::types::oid::REGPROCEDURE,
        "regrole" => crate::sql::types::oid::REGROLE,
        "regnamespace" => crate::sql::types::oid::REGNAMESPACE,
        "regoper" => crate::sql::types::oid::REGOPER,
        "regoperator" => crate::sql::types::oid::REGOPERATOR,
        _ => ColType::from_sql_name(canonical)
            .expect("canonical type")
            .oid(),
    };
    Ok(Datum::Regtype {
        referenced_oid,
        name: canonical,
    })
}

pub(crate) fn type_name_of_pub(d: &Datum) -> &'static str {
    type_name_of(d)
}

/// When an enum value is compared with an unknown text literal, resolves the
/// literal to a member of the enum's type (catalog-aware). PostgreSQL reports a
/// non-member as 22P02. Non-enum pairs and already-typed operands pass through.
fn coerce_enum_literal<'a>(
    l: Datum<'a>,
    r: Datum<'a>,
    l_unknown: bool,
    r_unknown: bool,
    hooks: &EvalHooks<'_, 'a>,
    arena: &'a Arena,
) -> Result<(Datum<'a>, Datum<'a>), SqlError> {
    let resolve = |slot: u16, text: Datum<'a>| -> Result<Datum<'a>, SqlError> {
        let label = match text {
            Datum::Text(s) => s,
            Datum::Bpchar(s) => s.trim_end_matches(' '),
            _ => return Ok(text),
        };
        let cat = hooks.catalog.ok_or_else(|| {
            sql_err!(
                sqlstate::INVALID_TEXT_REPRESENTATION,
                "invalid input value for enum: \"{}\"",
                label
            )
        })?;
        let Some(sort) = cat.enum_label_sort(slot, label) else {
            return Err(sql_err!(
                sqlstate::INVALID_TEXT_REPRESENTATION,
                "invalid input value for enum: \"{}\"",
                label
            ));
        };
        Ok(Datum::Enum {
            slot,
            sort,
            label: arena.alloc_str(label).map_err(|_| arena_full())?,
        })
    };
    match (l, r) {
        (Datum::Enum { slot, .. }, _) if r_unknown => Ok((l, resolve(slot, r)?)),
        (_, Datum::Enum { slot, .. }) if l_unknown => Ok((resolve(slot, l)?, r)),
        _ => Ok((l, r)),
    }
}

fn type_name_of(d: &Datum) -> &'static str {
    match d {
        Datum::Array { element, .. } => element.array_name(),
        Datum::Int2Vector(_) => "int2vector",
        Datum::OidVector(_) => "oidvector",
        Datum::Null => "unknown",
        Datum::Bool(_) => "boolean",
        Datum::Int2(_) => "smallint",
        Datum::Int4(_) => "integer",
        Datum::Oid(_) => "oid",
        Datum::Int8(_) => "bigint",
        Datum::Float4(_) => "real",
        Datum::Float8(_) => "double precision",
        Datum::Numeric(_) => "numeric",
        Datum::Text(_) => "text",
        Datum::Bpchar(_) => "character",
        Datum::Regtype { .. } => "regtype",
        Datum::RegObject { type_oid, .. } => match *type_oid {
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
        Datum::Uuid(_) => "uuid",
        Datum::Bytea(_) => "bytea",
        Datum::Range { kind, .. } => kind.name(),
        Datum::Bit { varying: false, .. } => "bit",
        Datum::Bit { varying: true, .. } => "bit varying",
        Datum::Multirange { kind, .. } => kind.multirange_name(),
        Datum::Inet(_) => "inet",
        Datum::Cidr(_) => "cidr",
        Datum::Macaddr(_) => "macaddr",
        Datum::Macaddr8(_) => "macaddr8",
        Datum::Record(_) => "record",
        // The catalog-free diagnostic names the dynamic enum category.
        Datum::Enum { .. } => "enum",
        Datum::Composite { .. } | Datum::CompositeText { .. } => "record",
    }
}

fn as_i64(d: &Datum) -> Option<i64> {
    match d {
        Datum::Int2(x) => Some(i64::from(*x)),
        Datum::Int4(x) => Some(i64::from(*x)),
        Datum::Oid(x) => Some(i64::from(*x)),
        Datum::Int8(x) => Some(*x),
        _ => None,
    }
}

fn as_f64(d: &Datum) -> Option<f64> {
    if let Datum::Numeric(n) = d {
        return Some(n.to_f64());
    }
    match d {
        Datum::Int2(x) => Some(f64::from(*x)),
        Datum::Int4(x) => Some(f64::from(*x)),
        Datum::Int8(x) => Some(*x as f64),
        Datum::Float4(x) => Some(f64::from(*x)),
        Datum::Float8(x) => Some(*x),
        _ => None,
    }
}

fn overflow(what: &'static str) -> SqlError {
    sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "{} out of range", what)
}

/// PostgreSQL's out-of-range error for a *text* value that names the value and
/// the type — distinct from [`overflow`], which a value-to-value cast raises
/// without a value. `'3000000000'::int4` gets this; `(big::int8)::int4` gets
/// the other.
pub(crate) fn out_of_range(value: &str, target: &'static str) -> SqlError {
    sql_err!(
        sqlstate::NUMERIC_OUT_OF_RANGE,
        "value \"{}\" is out of range for type {}",
        value,
        target
    )
}

fn division_by_zero() -> SqlError {
    sql_err!(sqlstate::DIVISION_BY_ZERO, "division by zero")
}

/// [`type_mismatch`] for callers outside this module (table-function args).
pub fn type_mismatch_pub(operator: &str, d: &Datum) -> SqlError {
    type_mismatch(operator, d)
}

fn type_mismatch(operator: &str, d: &Datum) -> SqlError {
    sql_err!(
        sqlstate::DATATYPE_MISMATCH,
        "operator {} does not accept {}",
        operator,
        type_name_of(d)
    )
}

fn cast_unsupported(from: &Datum, to: &'static str) -> SqlError {
    sql_err!(
        sqlstate::DATATYPE_MISMATCH,
        "cannot cast {} to {}",
        type_name_of(from),
        to
    )
}

fn bad_text(s: &str, target: &'static str) -> SqlError {
    sql_err!(
        sqlstate::INVALID_TEXT_REPRESENTATION,
        "invalid input syntax for type {}: \"{}\"",
        target,
        s
    )
}

pub(crate) fn arena_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "statement too large for SQL arena"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Budget;
    use crate::sql::ast::{SelectItem, Stmt};
    use crate::sql::parser::Parser;

    fn eval_one<'a>(arena: &'a Arena, text: &'a str) -> Result<Datum<'a>, SqlError> {
        let mut p = Parser::new(text, arena).unwrap();
        let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
            panic!()
        };
        let SelectItem::Expr { expression, .. } = s.items[0] else {
            panic!()
        };
        eval(expression, arena, NO_PARAMS, &NoColumns)
    }

    fn with_arena(f: impl FnOnce(&Arena)) {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "test", 1 << 18).unwrap();
        f(&arena);
    }

    #[test]
    fn arithmetic_matches_postgres() {
        with_arena(|a| {
            assert_eq!(eval_one(a, "SELECT 1 + 2 * 3").unwrap(), Datum::Int4(7));
            assert_eq!(eval_one(a, "SELECT 7 / 2").unwrap(), Datum::Int4(3));
            assert_eq!(eval_one(a, "SELECT 7 % 2").unwrap(), Datum::Int4(1));
            // Decimal literals are NUMERIC (as in PostgreSQL), so 7.0/2 is
            // numeric 3.5000000000000000, not float8.
            assert_eq!(
                eval_one(a, "SELECT 7.0 / 2").unwrap().to_string(),
                "3.5000000000000000"
            );
            assert_eq!(
                eval_one(a, "SELECT 7.0::float8 / 2").unwrap(),
                Datum::Float8(3.5)
            );
            assert_eq!(eval_one(a, "SELECT -(-5)").unwrap(), Datum::Int4(5));
            // int4 + int4 overflows like PostgreSQL (no silent widening);
            // int8 arithmetic carries the value.
            assert_eq!(
                eval_one(a, "SELECT 2147483647 + 1").unwrap_err().sqlstate,
                "22003"
            );
            assert_eq!(
                eval_one(a, "SELECT 2147483647::bigint + 1").unwrap(),
                Datum::Int8(2147483648)
            );
        });
    }

    #[test]
    fn division_by_zero_is_22012() {
        with_arena(|a| {
            for q in ["SELECT 1/0", "SELECT 1.0/0", "SELECT 1%0"] {
                let err = eval_one(a, q).unwrap_err();
                assert_eq!(err.sqlstate, "22012", "{q}");
            }
        });
    }

    #[test]
    fn int8_overflow_is_22003() {
        with_arena(|a| {
            let err = eval_one(a, "SELECT 9223372036854775807 + 1").unwrap_err();
            assert_eq!(err.sqlstate, "22003");
        });
    }

    #[test]
    fn three_valued_logic() {
        with_arena(|a| {
            assert_eq!(
                eval_one(a, "SELECT NULL AND FALSE").unwrap(),
                Datum::Bool(false)
            );
            assert_eq!(eval_one(a, "SELECT NULL AND TRUE").unwrap(), Datum::Null);
            assert_eq!(
                eval_one(a, "SELECT NULL OR TRUE").unwrap(),
                Datum::Bool(true)
            );
            assert_eq!(eval_one(a, "SELECT NULL OR FALSE").unwrap(), Datum::Null);
            assert_eq!(eval_one(a, "SELECT NOT NULL::bool").unwrap(), Datum::Null);
            assert_eq!(eval_one(a, "SELECT 1 = NULL").unwrap(), Datum::Null);
            assert_eq!(
                eval_one(a, "SELECT NULL IS NULL").unwrap(),
                Datum::Bool(true)
            );
        });
    }

    #[test]
    fn comparisons_and_concat() {
        with_arena(|a| {
            assert_eq!(eval_one(a, "SELECT 1 < 2").unwrap(), Datum::Bool(true));
            assert_eq!(eval_one(a, "SELECT 2.5 >= 2").unwrap(), Datum::Bool(true));
            assert_eq!(
                eval_one(a, "SELECT 'abc' < 'abd'").unwrap(),
                Datum::Bool(true)
            );
            assert_eq!(
                eval_one(a, "SELECT 'a' || 'b' || 'c'").unwrap(),
                Datum::Text("abc")
            );
            assert_eq!(
                eval_one(a, "SELECT 'n=' || 42").unwrap(),
                Datum::Text("n=42")
            );
            assert_eq!(eval_one(a, "SELECT 'x' || NULL").unwrap(), Datum::Null);
        });
    }

    #[test]
    fn casts() {
        with_arena(|a| {
            assert_eq!(eval_one(a, "SELECT '42'::int").unwrap(), Datum::Int4(42));
            assert_eq!(eval_one(a, "SELECT 42::bigint").unwrap(), Datum::Int8(42));
            assert_eq!(eval_one(a, "SELECT 2.7::int").unwrap(), Datum::Int4(3));
            assert_eq!(
                eval_one(a, "SELECT true::text").unwrap(),
                Datum::Text("true")
            );
            assert_eq!(eval_one(a, "SELECT 'on'::bool").unwrap(), Datum::Bool(true));
            assert_eq!(
                eval_one(a, "SELECT '2.5'::float8").unwrap(),
                Datum::Float8(2.5)
            );
            let err = eval_one(a, "SELECT 'zap'::int").unwrap_err();
            assert_eq!(err.sqlstate, "22P02");
            let err = eval_one(a, "SELECT 1::geometry").unwrap_err();
            assert_eq!(err.sqlstate, "42704");
        });
    }

    #[test]
    fn sqlstate_is_a_validated_fixed_value() {
        assert_eq!(SqlState::parse("ZX123").unwrap().as_str(), "ZX123");
        assert!(SqlState::parse("00000").unwrap().is_successful_completion());
        for invalid in ["", "2201", "220123", "22p02", "22-02"] {
            assert!(SqlState::parse(invalid).is_none(), "{invalid}");
        }
    }
}
