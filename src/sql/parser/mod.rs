//! Recursive-descent parser (Pratt for expressions) into the arena AST.
//!
//! Statement lists (select items, column lists, VALUES rows, …) are bounded
//! by the statement arena or PostgreSQL's client-visible limit. `LOCK TABLE`
//! name lists keep their own catalog-width limit.

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::eval::sqlstate;
use crate::stack_format;
use crate::util::StackStr;

use super::ast::*;
use super::lexer::{LexError, Lexer, Tok};
use super::types::{IntervalField, IntervalRange, TypeMod};

/// Names for the calls a desugaring produces, for syntax PostgreSQL does not
/// also expose as a function. A space cannot appear in an identifier, so a
/// query cannot reach these by writing them, and the function router will not
/// answer to `similar_to(...)` or `overlaps(...)` — which PostgreSQL refuses.
/// Any future desugaring of syntax-only constructs belongs here too.
pub(crate) const SIMILAR_TO: &str = "similar to";
pub(crate) const OVERLAPS_PERIODS: &str = "overlaps periods";
/// `ROW(...) IS NOT DISTINCT FROM ROW(...)`: field-by-field distinctness, where
/// two NULL fields are not distinct.
pub(crate) const ROWS_NOT_DISTINCT: &str = "rows not distinct";

/// PostgreSQL's maximum target-list width also bounds the flat set of distinct
/// grouping expressions, because grouping expressions absent from the visible
/// result are carried as resjunk target entries.
pub const MAX_GROUP_TERMS: usize = 1_664;
const MAX_GROUP_WORDS: usize = MAX_GROUP_TERMS.div_ceil(u64::BITS as usize);
/// A single `LOCK TABLE` may name every relation in the largest bounded
/// catalog operation. This is separate from ordinary SQL lists because
/// pg_dump locks its complete table set in one statement.
pub const MAX_LOCK_TABLES: usize = crate::storage::MAX_PUBLICATION_TABLES;
type OrderLimit<'a> = (
    &'a [OrderBy<'a>],
    Option<&'a Expr<'a>>,
    Option<&'a Expr<'a>>,
    bool,
);
/// PostgreSQL bounds the fully expanded grouping-set product at 4,096 and a
/// single CUBE at 12 elements.
pub const MAX_GROUPING_SETS: usize = 4_096;
const MAX_CUBE_TERMS: usize = 12;
/// PostgreSQL executes ALTER TABLE subcommands in a fixed pass order rather
/// than the written order: drops first, then column-type changes, then column
/// adds, then constraint adds, then column-attribute changes. This returns the
/// pass number the parser sorts by so that, e.g., an ADD CONSTRAINT can
/// reference a column ADDed later in the same statement. The standalone forms
/// (RENAME/SET SCHEMA) never share a list, so their pass is irrelevant.
fn alter_pass(action: &AlterAction) -> u8 {
    match action {
        AlterAction::DropColumn { .. } | AlterAction::DropConstraint { .. } => 0,
        AlterAction::AlterColumnType { .. } => 1,
        AlterAction::AddColumn { .. } => 2,
        AlterAction::AddConstraint(_)
        | AlterAction::AttachIndexConstraint { .. }
        | AlterAction::AlterConstraint { .. }
        | AlterAction::ValidateConstraint(_) => 3,
        AlterAction::SetDefault { .. }
        | AlterAction::DropDefault { .. }
        | AlterAction::SetNotNull { .. }
        | AlterAction::DropNotNull { .. }
        | AlterAction::SetStatistics { .. }
        | AlterAction::SetStatisticsOptions { .. }
        | AlterAction::ResetStatisticsOptions { .. }
        | AlterAction::SetStorage { .. }
        | AlterAction::ResetStorage { .. }
        | AlterAction::SetCompression { .. }
        | AlterAction::AddIdentity { .. }
        | AlterAction::DropIdentity { .. }
        | AlterAction::SetIdentityMode { .. }
        | AlterAction::SetGeneratedExpression { .. }
        | AlterAction::DropGeneratedExpression { .. }
        | AlterAction::AlterIdentitySequence { .. } => 4,
        AlterAction::SetForeignOptions(_)
        | AlterAction::SetColumnForeignOptions { .. }
        | AlterAction::SetStorageOptions(_)
        | AlterAction::SetViewOptions { .. }
        | AlterAction::ResetStorageOptions(_) => 4,
        // Standalone forms; never appear in a multi-action list.
        AlterAction::RenameTable(_)
        | AlterAction::RenameColumn { .. }
        | AlterAction::RenameConstraint { .. }
        | AlterAction::SetNotNullInheritance { .. }
        | AlterAction::SetTriggerEnabled { .. }
        | AlterAction::SetRuleEnabled { .. }
        | AlterAction::SetRowLevelSecurity(_)
        | AlterAction::SetPersistence(_)
        | AlterAction::SetInheritance { .. }
        | AlterAction::SetTablespace(_)
        | AlterAction::SetAccessMethod(_)
        | AlterAction::ResetAccessMethod
        | AlterAction::SetReplicaIdentity(_)
        | AlterAction::AttachPartition { .. }
        | AlterAction::DetachPartition { .. }
        | AlterAction::SetSchema(_)
        | AlterAction::SetCluster(_)
        | AlterAction::ClearCluster
        | AlterAction::SetWithoutOids
        | AlterAction::SetTypeMembership(_) => 5,
    }
}

pub const MAX_ROWS: usize = 256;

/// An allocation upper bound for the statements in one SQL source string.
/// Semicolons inside literals and comments deliberately over-count: reserving
/// a few empty arena slots is cheaper and safer than maintaining a second SQL
/// lexer solely to size parser output.
pub(crate) fn statement_capacity(source: &str) -> usize {
    source
        .as_bytes()
        .iter()
        .filter(|&&byte| byte == b';')
        .count()
        .saturating_add(1)
}

/// Words that cannot appear as a bare column reference; mirrors the
/// reserved entries of PostgreSQL's keyword table that this grammar uses.
/// Whether a numeric token carries a `0x`/`0o`/`0b` base prefix.
/// Whether `text` mentions the word `window` in any case — the cheap pre-filter
/// that keeps the WINDOW-clause lookahead off the path of ordinary queries.
fn mentions_window(text: &str) -> bool {
    text.as_bytes()
        .windows(6)
        .any(|w| w.eq_ignore_ascii_case(b"window"))
}

fn is_base_prefixed(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() > 2 && b[0] == b'0' && matches!(b[1], b'x' | b'X' | b'o' | b'O' | b'b' | b'B')
}

/// The PostgreSQL keyword categories that constrain where a word may be used
/// unquoted. The fourth category, plain `unreserved`, behaves exactly like a
/// non-keyword in both places this matters, so it is deliberately absent —
/// `None` covers it.
///
/// Provenance: `SELECT word, catcode FROM pg_get_keywords()` on PostgreSQL
/// 18.6.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Keyword {
    /// `unreserved (cannot be function or type name)` — legal as a column or
    /// table name, so only identifier quoting cares.
    ColumnName,
    /// `reserved (can be function or type name)`.
    TypeFuncName,
    /// `reserved`.
    Reserved,
}

/// Categorizes `word`, or `None` when it is unreserved or not a keyword at all.
pub(crate) fn keyword_category(word: &str) -> Option<Keyword> {
    Some(match word {
        "between" | "bigint" | "bit" | "boolean" | "char" | "character" | "coalesce" | "dec"
        | "decimal" | "exists" | "extract" | "float" | "greatest" | "grouping" | "inout"
        | "int" | "integer" | "interval" | "json" | "json_array" | "json_arrayagg"
        | "json_exists" | "json_object" | "json_objectagg" | "json_query" | "json_scalar"
        | "json_serialize" | "json_table" | "json_value" | "least" | "merge_action"
        | "national" | "nchar" | "none" | "normalize" | "nullif" | "numeric" | "out"
        | "overlay" | "position" | "precision" | "real" | "row" | "setof" | "smallint"
        | "substring" | "time" | "timestamp" | "treat" | "trim" | "values" | "varchar"
        | "xmlattributes" | "xmlconcat" | "xmlelement" | "xmlexists" | "xmlforest"
        | "xmlnamespaces" | "xmlparse" | "xmlpi" | "xmlroot" | "xmlserialize" | "xmltable" => {
            Keyword::ColumnName
        }
        "authorization" | "binary" | "collation" | "concurrently" | "cross" | "current_schema"
        | "freeze" | "full" | "ilike" | "inner" | "is" | "isnull" | "join" | "left" | "like"
        | "natural" | "notnull" | "outer" | "overlaps" | "right" | "similar" | "tablesample"
        | "verbose" => Keyword::TypeFuncName,
        "all" | "analyse" | "analyze" | "and" | "any" | "array" | "as" | "asc" | "asymmetric"
        | "both" | "case" | "cast" | "check" | "collate" | "column" | "constraint" | "create"
        | "current_catalog" | "current_date" | "current_role" | "current_time"
        | "current_timestamp" | "current_user" | "default" | "deferrable" | "desc" | "distinct"
        | "do" | "else" | "end" | "except" | "false" | "fetch" | "for" | "foreign" | "from"
        | "grant" | "group" | "having" | "in" | "initially" | "intersect" | "into" | "lateral"
        | "leading" | "limit" | "localtime" | "localtimestamp" | "not" | "null" | "offset"
        | "on" | "only" | "or" | "order" | "placing" | "primary" | "references" | "returning"
        | "select" | "session_user" | "some" | "symmetric" | "system_user" | "table" | "then"
        | "to" | "trailing" | "true" | "union" | "unique" | "user" | "using" | "variadic"
        | "when" | "where" | "window" | "with" => Keyword::Reserved,
        _ => return None,
    })
}

/// Whether `word` is a keyword PostgreSQL refuses in a `ColId` position — a
/// column name, a table name, or a FROM-item alias. Its two reserved categories
/// are rejected there; the unreserved ones are accepted — `begin`, `values` and
/// `set` are all perfectly legal column names.
pub(crate) fn is_column_name_keyword(word: &str) -> bool {
    matches!(
        keyword_category(word),
        Some(Keyword::TypeFuncName | Keyword::Reserved)
    )
}

/// Whether `word` is one of PostgreSQL's fully `reserved` keywords — the ones
/// that can never name anything. Distinct from [`is_column_name_keyword`],
/// which also rejects the `can be function or type name` category: those may
/// not name a column, but `left('abc', 2)` and `array[1]` are ordinary
/// expressions, so an expression position must let them through.
pub(crate) fn is_reserved_keyword(word: &str) -> bool {
    matches!(keyword_category(word), Some(Keyword::Reserved))
}

/// Whether PostgreSQL rejects `word` in its `type_function_name` production,
/// which routine parameter names use.
pub(crate) fn is_type_function_name_keyword(word: &str) -> bool {
    matches!(
        keyword_category(word),
        Some(Keyword::ColumnName | Keyword::Reserved)
    )
}

/// Whether an identifier must be quoted to survive a round trip, mirroring
/// PostgreSQL's `quote_ident`: any keyword outside the plain unreserved
/// category would otherwise be reinterpreted.
pub(crate) fn keyword_needs_quotes(word: &str) -> bool {
    keyword_category(word).is_some()
}

/// A NOTICE or WARNING raised while parsing. The engine reports these
/// before the statement's own output, as PostgreSQL does.
#[derive(Clone, Copy, Debug)]
pub struct ParseNotice<'a> {
    pub warning: bool,
    pub sqlstate: &'static str,
    pub message: &'a str,
    /// Byte offset in the parsed text PostgreSQL reports as the cursor.
    pub cursor: Option<usize>,
    /// Raised by PostgreSQL's raw parser rather than parse analysis, so it is
    /// reported even when a later part of the query text fails to parse.
    pub raw: bool,
}

/// Capacity of a parse error message: PostgreSQL's fixed wording plus three
/// names of up to `NAMEDATALEN - 1` bytes, the most any parser message names.
/// Echoed query text has no such bound; see [`ParseError::near`].
pub const PARSE_MESSAGE_BYTES: usize = 128 + 3 * crate::sql::lexer::MAX_IDENTIFIER_BYTES;

#[derive(Debug)]
pub struct ParseError {
    pub at: usize,
    pub message: StackStr<PARSE_MESSAGE_BYTES>,
    /// The range of query text a scanner or grammar error echoes as `at or
    /// near "..."`, which [`Self::render`] reproduces in full even when it
    /// does not fit `message`.
    pub near: Option<(usize, usize)>,
    /// SQLSTATE; almost always 42601 (syntax error), but some parse-analysis
    /// errors carry their own (e.g. 42P20 for window-frame shape).
    pub sqlstate: &'static str,
    /// Byte offset in the parsed text that PostgreSQL reports as the error
    /// cursor (the ErrorResponse `P` field), or `None` when it reports none.
    pub cursor: Option<usize>,
    /// PostgreSQL's HINT for this error, if it sends one.
    pub hint: Option<&'static str>,
    /// PostgreSQL's DETAIL for this error, if it sends one.
    pub detail: Option<&'static str>,
}

impl ParseError {
    fn new(at: usize, text: &str) -> Self {
        Self {
            at,
            near: None,
            detail: None,
            message: stack_format!(PARSE_MESSAGE_BYTES, "{}", text),
            sqlstate: sqlstate::SYNTAX_ERROR,
            cursor: None,
            hint: None,
        }
    }
}

impl ParseError {
    /// A scanner error on `text`, echoing the scanned text as PostgreSQL does.
    pub(crate) fn lexical(error: LexError, text: &str) -> Self {
        Self {
            at: error.at,
            message: error.render(text),
            near: error.echo(text),
            detail: None,
            sqlstate: error.kind.sqlstate(),
            cursor: error.cursor(),
            hint: error.hint,
        }
    }

    /// The complete message for an error raised parsing `text`.
    pub(crate) fn render<'t>(&'t self, text: &'t str) -> impl core::fmt::Display + 't {
        struct Rendered<'t>(&'t ParseError, &'t str);
        impl core::fmt::Display for Rendered<'_> {
            fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                let Rendered(error, text) = *self;
                match error.near.and_then(|(start, end)| text.get(start..end)) {
                    Some(near) if error.message.is_truncated() => {
                        let message = error.message.as_str();
                        let prefix = message
                            .find(" at or near \"")
                            .map_or(message, |index| &message[..index]);
                        write!(formatter, "{prefix} at or near \"{near}\"")
                    }
                    _ => formatter.write_str(error.message.as_str()),
                }
            }
        }
        Rendered(self, text)
    }
}

mod copy_options;
mod expr;
mod typmod;
mod window;

mod ddl;

struct TypeInputSources<'a> {
    strings: ArenaList<'a, (&'a Expr<'a>, NamespaceSource)>,
    candidates: ArenaList<'a, super::type_inputs::Candidate<'a>>,
    restored: ArenaList<
        'a,
        (
            super::type_inputs::Candidate<'a>,
            super::type_inputs::Binding<'a>,
        ),
    >,
}

pub struct Parser<'a> {
    text: &'a str,
    lexer: Lexer<'a>,
    peeked: Tok<'a>,
    peek_at: usize,
    previous_token: Option<(Tok<'a>, usize, usize)>,
    column_sources: Option<core::cell::RefCell<QuerySourceCollector<'a>>>,
    namespace_sources: Option<core::cell::RefCell<SchemaSqlSourceCollector<'a>>>,
    type_input_sources: Option<core::cell::RefCell<TypeInputSources<'a>>>,
    type_inputs: &'a [super::type_inputs::Binding<'a>],
    arena: &'a Arena,
    /// Highest `$n` seen — the statement's parameter count.
    max_param: u32,
    // Source positions of string tokens considered by stored enum inference.
    // Grammar literals keep their syntax and leave their parameter slot unused.
    string_parameters: &'a [usize],
    /// The `WINDOW name AS (...)` definitions of the SELECT being parsed, which
    /// `OVER name` resolves against. Scoped to that SELECT: the length is saved
    /// and truncated back around a nested one, since a subquery neither sees
    /// nor exports them.
    windows: ArenaList<'a, Option<(&'a str, &'a WindowSpec<'a>)>>,
    /// Warnings raised while parsing. PostgreSQL reports these before the
    /// statement's own output, so the engine drains them after each
    /// `next_stmt` and emits them ahead of executing it.
    warnings: ArenaList<'a, ParseNotice<'a>>,
    /// Tokens whose text does not start at their token (strings, quoted and
    /// copied identifiers), for positioning analysis errors. Recorded only
    /// when the caller keeps the parser's arena for the whole text.
    source_copies:
        Option<core::cell::RefCell<ArenaList<'a, crate::sql::error_position::SourceCopy>>>,
    /// End of the furthest identifier whose truncation NOTICE was recorded,
    /// so a token scanned again after lookahead is reported once.
    truncation_reported_to: usize,
    /// Report errors PostgreSQL raises during parse analysis when the
    /// statement ends instead of immediately, so the caller can still find
    /// syntax errors later in the query text, which PostgreSQL reports first.
    defer_analysis_errors: bool,
    /// The first analysis error in the statement being parsed.
    analysis_error: Option<ParseError>,
    /// The first `DEFAULT` of the statement not written as a whole
    /// INSERT value or assigned value, and the most recent `DEFAULT`.
    misplaced_default: Option<usize>,
    last_default: usize,
    /// The type a deferred "type modifier is not allowed" error names, which
    /// PostgreSQL reports as unknown instead when no such type exists.
    analysis_error_type: Option<&'a str>,
    /// The statement whose analysis error was last reported, so the caller
    /// can report the analysis errors PostgreSQL finds before it.
    rejected_statement: Option<Stmt<'a>>,
    /// The last `next_stmt` failure was a deferred analysis error, and the
    /// parser stands at the end of that statement.
    analysis_error_reported: bool,
    /// True while parsing a position where `SELECT ... INTO table` is legal (a
    /// top-level query). A subquery / CTE / set-op branch clears it, so an
    /// `INTO` there is rejected as PostgreSQL rejects it.
    allow_into: bool,
    /// The `INTO table` clause a top-level SELECT carried, with the byte range
    /// of the clause itself so the query can be reconstructed without it (for
    /// reuse of the CREATE TABLE AS machinery). `None` when there was none.
    into_clause: Option<(QualName<'a>, RelationPersistence, usize, usize)>,
    /// A column/domain DEFAULT ends before a following `NOT NULL`. Ordinarily
    /// `NOT` is an infix-expression prefix (`NOT IN`, `NOT LIKE`, ...), so the
    /// expression parser needs this narrow bit of grammar context to leave the
    /// column constraint for its caller.
    stop_default_at_not_null: bool,
    /// Where the latest parenthesized primary expression (or scalar
    /// subquery) began: one of the forms PostgreSQL lets take a subscript.
    parenthesized_primary: Option<usize>,
    /// SQL-language routine formals. Their names are resolved to the same
    /// positional parameter nodes as `$n` while the body is parsed, so a
    /// runtime value cannot lose its declared type or be mistaken for a table
    /// column merely because the caller used the named spelling.
    routine_parameters: [Option<&'a str>; crate::storage::MAX_ROUTINE_ARGUMENTS],
    routine_name: Option<&'a str>,
    /// Local variables and record fields captured by a procedural OPEN.
    named_parameters: &'a [NamedParameter<'a>],
    /// `$n.field` selections, kept so a parameter whose type stays undetermined
    /// reports PostgreSQL's `unknown` composite error.
    parameter_fields: core::cell::RefCell<ArenaList<'a, (u32, &'a str)>>,
}

/// One step of an assignment target after its column: a field of a
/// composite, or the subscripts of an array or other subscriptable value.
#[derive(Clone, Copy)]
enum Indirection<'a> {
    Field(&'a str),
    Subscripts(&'a [AssignmentSubscript<'a>]),
}

/// A parsed INSERT target list: the written column names, each target's
/// indirection by position, and where each target was written.
#[derive(Clone, Copy)]
struct InsertTargets<'a> {
    names: &'a [&'a str],
    steps: &'a [(usize, &'a [Indirection<'a>])],
    positions: &'a [usize],
}

impl<'a> InsertTargets<'a> {
    const EMPTY: Self = Self {
        names: &[],
        steps: &[],
        positions: &[],
    };

    fn path(&self, target: usize) -> Option<&'a [Indirection<'a>]> {
        self.steps
            .iter()
            .find(|(at, _)| *at == target)
            .map(|(_, path)| *path)
    }

    /// Whether several targets write `column`, all through indirection, so
    /// they merge into one value.
    fn merged(&self, column: &str) -> bool {
        let mut written = self
            .names
            .iter()
            .enumerate()
            .filter(|(_, name)| **name == column);
        written.clone().count() > 1 && written.all(|(index, _)| self.path(index).is_some())
    }

    /// Whether target `index` is a merged column's first.
    fn kept(&self, index: usize) -> bool {
        let name = self.names[index];
        !(self.merged(name) && self.names[..index].contains(&name))
    }
}

/// One subscript of an assignment target.
#[derive(Clone, Copy)]
enum AssignmentSubscript<'a> {
    Element(&'a Expr<'a>),
    Slice {
        lower: Option<&'a Expr<'a>>,
        upper: Option<&'a Expr<'a>>,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct NamedParameter<'a> {
    pub qualifier: Option<&'a str>,
    pub name: &'a str,
    pub index: u32,
    pub conflict: crate::sql::guc::VariableConflict,
}

/// The parts of PostgreSQL's `TypeNameToString` for a parsed type name.
#[derive(Clone, Copy)]
struct WrittenTypeName<'a> {
    schema: Option<&'a str>,
    name: &'a str,
    array: bool,
}

impl core::fmt::Display for WrittenTypeName<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(schema) = self.schema {
            write!(formatter, "{schema}.")?;
        }
        formatter.write_str(self.name)?;
        if self.array {
            formatter.write_str("[]")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NamespaceSource {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum CatalogLiteralKind {
    QualifiedName,
    SchemaName,
    TypeName,
    RoutineIdentity,
    OperatorName,
    OperatorIdentity,
}

impl CatalogLiteralKind {
    pub(super) fn for_cast(name: &str) -> Option<Self> {
        match name {
            "regclass" | "regproc" | "regcollation" | "regconfig" | "regdictionary" => {
                Some(Self::QualifiedName)
            }
            "regnamespace" => Some(Self::SchemaName),
            "regtype" => Some(Self::TypeName),
            "regprocedure" => Some(Self::RoutineIdentity),
            "regoper" => Some(Self::OperatorName),
            "regoperator" => Some(Self::OperatorIdentity),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CatalogLiteralSource<'a> {
    pub(crate) kind: CatalogLiteralKind,
    pub(crate) position: NamespaceSource,
    pub(crate) value: &'a str,
}

struct SchemaSqlSourceCollector<'a> {
    namespaces: ArenaList<'a, NamespaceSource>,
    catalog_literals: ArenaList<'a, CatalogLiteralSource<'a>>,
    string_literals: ArenaList<'a, (&'a Expr<'a>, NamespaceSource)>,
}

pub(crate) struct SchemaSqlSources<'a> {
    pub(crate) namespaces: &'a [NamespaceSource],
    pub(crate) catalog_literals: &'a [CatalogLiteralSource<'a>],
}

#[derive(Clone, Copy)]
pub(crate) enum StoredSqlKind {
    Expression,
    Statements,
    TypeName,
    RoutineIdentity,
    OperatorName,
    OperatorIdentity,
}

pub(crate) struct TypeInputSyntax<'a> {
    pub casts: &'a [super::type_inputs::Candidate<'a>],
    pub literals: &'a [(&'a Expr<'a>, NamespaceSource)],
    pub expression: Option<&'a Expr<'a>>,
}

pub(crate) fn type_input_sources<'a>(
    sql: &'a str,
    kind: StoredSqlKind,
    arena: &'a Arena,
) -> Result<TypeInputSyntax<'a>, super::eval::SqlError> {
    let mut parser = Parser::new(sql, arena).map_err(|error| super::parse_error_to_sql(&error))?;
    parser.capture_type_inputs();
    let mut expression = None;
    match kind {
        StoredSqlKind::Expression => {
            expression = Some(
                parser
                    .expression(0)
                    .map_err(|error| super::parse_error_to_sql(&error))?,
            );
        }
        StoredSqlKind::Statements => {
            while parser
                .next_stmt()
                .map_err(|error| super::parse_error_to_sql(&error))?
                .is_some()
            {}
        }
        _ => {
            return Err(crate::sql_err!(
                sqlstate::DATA_EXCEPTION,
                "stored type input has no expression source"
            ));
        }
    }
    if parser.peeked != Tok::Eof {
        return Err(crate::sql_err!(
            sqlstate::SYNTAX_ERROR,
            "trailing tokens after type input source"
        ));
    }
    Ok(TypeInputSyntax {
        casts: parser.type_input_candidates(),
        literals: parser.string_literals(),
        expression,
    })
}

/// Namespace positions come from grammar nodes, so table aliases and record
/// fields cannot be mistaken for namespaces during a stored definition edit.
pub(crate) fn schema_namespace_sources<'arena>(
    sql: &str,
    kind: StoredSqlKind,
    arena: &'arena Arena,
) -> Result<SchemaSqlSources<'arena>, super::eval::SqlError> {
    let sql = arena
        .alloc_str(sql)
        .map_err(|_| super::query::arena_full_pub())?;
    let mut parser = Parser::new(sql, arena).map_err(|error| super::parse_error_to_sql(&error))?;
    parser.namespace_sources = Some(core::cell::RefCell::new(SchemaSqlSourceCollector {
        namespaces: ArenaList::new(arena),
        catalog_literals: ArenaList::new(arena),
        string_literals: ArenaList::new(arena),
    }));
    let parsed = match kind {
        StoredSqlKind::Expression => parser.expression(0).map(|_| ()),
        StoredSqlKind::TypeName => parser.type_name_mod().map(|_| ()),
        StoredSqlKind::RoutineIdentity => parser.routine_identity().map(|_| ()),
        StoredSqlKind::OperatorName => parser
            .operator_qual_name("stored operator name is invalid")
            .map(|_| ()),
        StoredSqlKind::OperatorIdentity => parser.operator_identity().map(|_| ()),
        StoredSqlKind::Statements => (|| {
            while parser.peeked != Tok::Eof {
                let _ = parser.statement()?;
                if parser.peeked != Tok::Eof {
                    parser.expect_op(";")?;
                }
            }
            Ok(())
        })(),
    };
    parsed.map_err(|error| super::parse_error_to_sql(&error))?;
    if parser.peeked != Tok::Eof {
        return Err(crate::sql_err!(
            sqlstate::SYNTAX_ERROR,
            "trailing tokens after stored SQL"
        ));
    }
    let sources = parser
        .namespace_sources
        .as_ref()
        .expect("explicit namespace capture")
        .borrow();
    // The returned positions own no references into the source being edited.
    Ok(SchemaSqlSources {
        namespaces: arena
            .alloc_slice_copy(sources.namespaces.as_slice())
            .map_err(|_| super::query::arena_full_pub())?,
        catalog_literals: arena
            .alloc_slice_copy(sources.catalog_literals.as_slice())
            .map_err(|_| super::query::arena_full_pub())?,
    })
}

#[derive(Clone, Copy)]
pub(crate) struct ColumnSource<'a> {
    pub name: &'a str,
    pub start: usize,
    pub end: usize,
    pub qualifier_parts: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct ProjectionSource<'a> {
    pub name: &'a str,
    pub start: usize,
    pub end: usize,
}

struct QuerySourceCollector<'a> {
    columns: ArenaList<'a, ColumnSource<'a>>,
    projections: ArenaList<'a, ProjectionSource<'a>>,
}

pub(crate) struct QuerySources<'a> {
    pub columns: &'a [ColumnSource<'a>],
    pub projections: &'a [ProjectionSource<'a>],
}

/// Retain the exact identifier bytes that the parser bound as column nodes.
pub(crate) fn parse_query_with_column_sources<'a>(
    sql: &'a str,
    arena: &'a Arena,
) -> Result<(&'a Select<'a>, QuerySources<'a>), super::eval::SqlError> {
    let mut parser = Parser::new(sql, arena).map_err(|error| super::parse_error_to_sql(&error))?;
    parser.column_sources = Some(core::cell::RefCell::new(QuerySourceCollector {
        columns: ArenaList::new(arena),
        projections: ArenaList::new(arena),
    }));
    let select = parser
        .query_select()
        .map_err(|error| super::parse_error_to_sql(&error))?;
    if parser.peeked != Tok::Eof {
        return Err(crate::sql_err!(
            super::eval::sqlstate::SYNTAX_ERROR,
            "trailing tokens after stored query"
        ));
    }
    let collector = parser
        .column_sources
        .as_ref()
        .expect("explicit column source capture")
        .borrow();
    let sources = QuerySources {
        columns: collector.columns.as_slice(),
        projections: collector.projections.as_slice(),
    };
    let select = arena
        .alloc(select)
        .map_err(|_| super::query::arena_full_pub())?;
    Ok((select, sources))
}

/// Parses a stored view definition across PostgreSQL's complete query body,
/// including set operations and VALUES leaves.
pub fn parse_view_select<'a>(
    sql: &'a str,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, super::eval::SqlError> {
    parse_query(sql, arena)
}

/// Parses a query body — a SELECT, a set-operation tree (UNION/INTERSECT/
/// EXCEPT), or a VALUES list — into a `Select` (a genuine set-operation lands
/// in `set_body`). Stored views, CREATE TABLE AS, and materialized-view style
/// bodies share this boundary.
pub fn parse_query<'a>(
    sql: &'a str,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, super::eval::SqlError> {
    parse_query_with_string_parameters(sql, arena, &[])
}

pub(crate) fn parse_expression_with_type_inputs<'a>(
    sql: &'a str,
    bindings: &'a [super::type_inputs::Binding<'a>],
    arena: &'a Arena,
    validate: impl FnOnce(&Parser<'a>) -> Result<(), super::eval::SqlError>,
) -> Result<&'a Expr<'a>, super::eval::SqlError> {
    let mut parser = Parser::new(sql, arena).map_err(|error| super::parse_error_to_sql(&error))?;
    parser.set_type_inputs(bindings);
    let expression = parser
        .expression(0)
        .map_err(|error| super::parse_error_to_sql(&error))?;
    if parser.peeked != Tok::Eof {
        return Err(crate::sql_err!(
            sqlstate::SYNTAX_ERROR,
            "trailing tokens after expression"
        ));
    }
    validate(&parser)?;
    Ok(expression)
}

pub(crate) fn parse_query_with_type_inputs<'a>(
    sql: &'a str,
    bindings: &'a [super::type_inputs::Binding<'a>],
    arena: &'a Arena,
    validate: impl FnOnce(&Parser<'a>) -> Result<(), super::eval::SqlError>,
) -> Result<&'a Select<'a>, super::eval::SqlError> {
    let mut parser = Parser::new(sql, arena).map_err(|error| super::parse_error_to_sql(&error))?;
    parser.set_type_inputs(bindings);
    let select = parser
        .query_select()
        .map_err(|error| super::parse_error_to_sql(&error))?;
    if parser.peeked != Tok::Eof {
        return Err(crate::sql_err!(
            sqlstate::SYNTAX_ERROR,
            "trailing tokens after query"
        ));
    }
    validate(&parser)?;
    arena
        .alloc(select)
        .map(|select| &*select)
        .map_err(|_| super::query::arena_full_pub())
}

/// Analyze ordinary expression strings as parameters while preserving syntax
/// that requires literal tokens, including JSON_TABLE paths and typed constants.
/// Parameter positions are sorted source byte offsets; indices start at one.
pub(crate) fn parse_query_with_string_parameters<'a>(
    sql: &'a str,
    arena: &'a Arena,
    positions: &'a [usize],
) -> Result<&'a Select<'a>, super::eval::SqlError> {
    let to_sql = |m: &str| super::eval::SqlError {
        sqlstate: super::eval::SqlState::known(super::eval::sqlstate::SYNTAX_ERROR),
        message: crate::stack_format!(192, "invalid query: {}", m),
    };
    let mut parser = Parser::new(sql, arena).map_err(|e| to_sql(e.message.as_str()))?;
    parser.string_parameters = positions;
    let sel = parser
        .query_select()
        .map_err(|e| to_sql(e.message.as_str()))?;
    if parser.peeked != Tok::Eof {
        return Err(to_sql("trailing tokens after query"));
    }
    arena
        .alloc(sel)
        .map(|r| &*r)
        .map_err(|_| super::eval::SqlError {
            sqlstate: super::eval::SqlState::known(super::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED),
            message: crate::stack_format!(192, "query too large for SQL arena"),
        })
}

/// Cursor plans contain exactly one command. Parse the complete input before
/// checking that boundary, so a later syntax error retains its SQLSTATE.
pub(crate) fn parse_cursor_command<'a>(
    sql: &'a str,
    arena: &'a Arena,
    parameters: &'a [NamedParameter<'a>],
    inputs: &'a [super::type_inputs::Binding<'a>],
    validate: impl FnOnce(&Parser<'a>) -> Result<(), super::eval::SqlError>,
) -> Result<&'a Stmt<'a>, super::eval::SqlError> {
    let mut parser = Parser::new(sql, arena).map_err(|error| super::parse_error_to_sql(&error))?;
    parser.named_parameters = parameters;
    parser.set_type_inputs(inputs);
    let mut statement = None;
    let mut multiple = false;
    while let Some(parsed) = parser
        .next_stmt()
        .map_err(|error| super::parse_error_to_sql(&error))?
    {
        if statement.is_none() {
            statement = Some(parsed);
        } else {
            multiple = true;
        }
    }
    if multiple || statement.is_none() {
        return Err(crate::sql_err!(
            sqlstate::INVALID_CURSOR_DEFINITION,
            "cannot open multi-query plan as cursor"
        ));
    }
    validate(&parser)?;
    arena
        .alloc(statement.unwrap())
        .map(|statement| &*statement)
        .map_err(|_| {
            crate::sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "cursor query exceeds its SQL arena"
            )
        })
}

/// Parses exactly one stored statement. Rewrite rules retain action text in
/// the durable catalog and cross this boundary again only after recovery has
/// validated the text and its captured dependencies.
pub(crate) fn parse_stored_statement<'a>(
    sql: &'a str,
    arena: &'a Arena,
) -> Result<&'a Stmt<'a>, super::eval::SqlError> {
    let mut parser = Parser::new(sql, arena).map_err(|error| super::parse_error_to_sql(&error))?;
    let statement = parser
        .next_stmt()
        .map_err(|error| super::parse_error_to_sql(&error))?
        .ok_or_else(|| {
            crate::sql_err!(
                super::eval::sqlstate::SYNTAX_ERROR,
                "stored statement is empty"
            )
        })?;
    if parser
        .next_stmt()
        .map_err(|error| super::parse_error_to_sql(&error))?
        .is_some()
    {
        return Err(crate::sql_err!(
            super::eval::sqlstate::SYNTAX_ERROR,
            "stored action contains more than one statement"
        ));
    }
    arena
        .alloc(statement)
        .map(|statement| &*statement)
        .map_err(|_| {
            crate::sql_err!(
                super::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "stored statement exceeds the SQL arena"
            )
        })
}

/// Parses a single scalar expression (e.g. a stored CHECK predicate) into the
/// arena. The whole input must be one expression.
pub fn parse_expr<'a>(
    sql: &'a str,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, super::eval::SqlError> {
    let to_sql = |m: &str| super::eval::SqlError {
        sqlstate: super::eval::SqlState::known(super::eval::sqlstate::SYNTAX_ERROR),
        message: crate::stack_format!(192, "invalid expression: {}", m),
    };
    let mut parser = Parser::new(sql, arena).map_err(|e| to_sql(e.message.as_str()))?;
    let expression = parser
        .expression(0)
        .map_err(|e| to_sql(e.message.as_str()))?;
    if parser.peeked != Tok::Eof {
        return Err(to_sql("trailing tokens after expression"));
    }
    Ok(expression)
}

pub(crate) fn parse_expr_with_column_sources<'a>(
    sql: &'a str,
    arena: &'a Arena,
) -> Result<(&'a Expr<'a>, &'a [ColumnSource<'a>]), super::eval::SqlError> {
    let mut parser = Parser::new(sql, arena).map_err(|error| super::parse_error_to_sql(&error))?;
    parser.column_sources = Some(core::cell::RefCell::new(QuerySourceCollector {
        columns: ArenaList::new(arena),
        projections: ArenaList::new(arena),
    }));
    let expression = parser
        .expression(0)
        .map_err(|error| super::parse_error_to_sql(&error))?;
    if parser.peeked != Tok::Eof {
        return Err(crate::sql_err!(
            sqlstate::SYNTAX_ERROR,
            "trailing tokens after stored expression"
        ));
    }
    let collector = parser
        .column_sources
        .as_ref()
        .expect("explicit column source capture")
        .borrow();
    Ok((expression, collector.columns.as_slice()))
}

/// Parses one complete PostgreSQL type spelling for a procedural local.
/// Keeping this at the parser boundary prevents PL/pgSQL programs from
/// hand-normalizing aliases or silently losing a typmod.
pub(crate) fn parse_type_name<'a>(
    sql: &'a str,
    arena: &'a Arena,
) -> Result<(&'a str, i32), super::eval::SqlError> {
    let to_sql = |m: &str| super::eval::SqlError {
        sqlstate: super::eval::SqlState::known(super::eval::sqlstate::SYNTAX_ERROR),
        message: crate::stack_format!(192, "invalid type name: {}", m),
    };
    let mut parser = Parser::new(sql, arena).map_err(|e| to_sql(e.message.as_str()))?;
    let parsed = parser
        .type_name_mod()
        .map_err(|e| to_sql(e.message.as_str()))?;
    if parser.peeked != Tok::Eof {
        return Err(to_sql("trailing tokens after type name"));
    }
    Ok(parsed)
}

#[cfg(test)]
pub(crate) fn parse_expression<'a>(
    sql: &'a str,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, super::eval::SqlError> {
    let mut parser = Parser::new(sql, arena).map_err(|error| super::parse_error_to_sql(&error))?;
    let expression = parser
        .expression(0)
        .map_err(|error| super::parse_error_to_sql(&error))?;
    if parser.peeked != Tok::Eof {
        return Err(crate::sql_err!(
            sqlstate::SYNTAX_ERROR,
            "trailing tokens after expression"
        ));
    }
    Ok(expression)
}

pub(crate) fn parse_expression_list<'a>(
    sql: &'a str,
    arena: &'a Arena,
) -> Result<&'a [&'a Expr<'a>], super::eval::SqlError> {
    let parse = || -> Result<_, ParseError> {
        let mut parser = Parser::new(sql, arena)?;
        let mut expressions = ArenaList::new(arena);
        loop {
            let expression = parser.expression(0)?;
            parser.push(&mut expressions, expression)?;
            if !parser.eat_op(",")? {
                break;
            }
        }
        if parser.peeked != Tok::Eof {
            return Err(parser.err_here("trailing tokens after expression list"));
        }
        Ok(expressions.as_slice())
    };
    parse().map_err(|error| super::parse_error_to_sql(&error))
}

impl<'a> Parser<'a> {
    pub fn new(text: &'a str, arena: &'a Arena) -> Result<Self, ParseError> {
        let mut lexer = Lexer::new(text, arena);
        let peeked = lexer
            .next_token()
            .map_err(|error| ParseError::lexical(error, text))?;
        let peek_at = lexer.token_start();
        let mut parser = Self {
            text,
            lexer,
            peeked,
            peek_at,
            arena,
            max_param: 0,
            string_parameters: &[],
            previous_token: None,
            column_sources: None,
            namespace_sources: None,
            type_input_sources: None,
            type_inputs: &[],
            windows: ArenaList::new(arena),
            warnings: ArenaList::new(arena),
            source_copies: None,
            truncation_reported_to: 0,
            defer_analysis_errors: false,
            analysis_error: None,
            misplaced_default: None,
            last_default: 0,
            analysis_error_type: None,
            rejected_statement: None,
            analysis_error_reported: false,
            allow_into: false,
            into_clause: None,
            stop_default_at_not_null: false,
            parenthesized_primary: None,
            routine_parameters: [None; crate::storage::MAX_ROUTINE_ARGUMENTS],
            routine_name: None,
            named_parameters: &[],
            parameter_fields: core::cell::RefCell::new(ArenaList::new(arena)),
        };
        parser.note_token_source()?;
        Ok(parser)
    }

    /// Tokens copied or offset from their token start, recorded while
    /// parsing, which [`crate::sql::error_position`] maps back to the text.
    pub(crate) fn source_copies(&self) -> &'a [crate::sql::error_position::SourceCopy] {
        self.source_copies
            .as_ref()
            .map_or(&[], |copies| copies.borrow().as_slice())
    }

    /// Records where copied tokens came from, for [`Self::source_copies`].
    /// The caller must not rewind the arena while this parser lives: the
    /// record grows with every string and copied identifier.
    pub(crate) fn record_source_copies(&mut self) -> Result<(), ParseError> {
        self.source_copies = Some(core::cell::RefCell::new(ArenaList::new(self.arena)));
        // The first token was scanned before recording began.
        self.note_token_source()
    }

    /// Records that `composed`, a name the parser built from several tokens,
    /// is written starting at byte `at`.
    fn note_composed_source(&mut self, composed: &'a str, at: usize) -> Result<(), ParseError> {
        let copy = crate::sql::error_position::SourceCopy {
            address: composed.as_ptr() as usize,
            len: composed.len(),
            at,
        };
        let pushed = self
            .source_copies
            .as_ref()
            .is_none_or(|copies| copies.borrow_mut().push(copy).is_ok());
        if pushed {
            Ok(())
        } else {
            Err(self.arena_full("statement too large for SQL arena"))
        }
    }

    /// Records where the current token's text came from, and PostgreSQL's
    /// NOTICE when the lexer truncated it.
    fn note_token_source(&mut self) -> Result<(), ParseError> {
        let text = match self.peeked {
            Tok::Str(text) | Tok::QuotedIdent(text) => Some(text),
            Tok::Ident(text) => {
                let start = self.text.as_ptr() as usize;
                let address = text.as_ptr() as usize;
                (address < start || address > start + self.text.len()).then_some(text)
            }
            _ => None,
        };
        if let Some(text) = text
            && let Some(copies) = self.source_copies.as_ref()
        {
            let copy = crate::sql::error_position::SourceCopy {
                address: text.as_ptr() as usize,
                len: text.len(),
                at: self.peek_at,
            };
            if copies.borrow_mut().push(copy).is_err() {
                return Err(self.arena_full("statement too large for SQL arena"));
            }
        }
        self.note_truncated_identifier()
    }

    /// Records PostgreSQL's scanner NOTICE for an identifier the lexer cut
    /// to 63 bytes.
    fn note_truncated_identifier(&mut self) -> Result<(), ParseError> {
        let Some(full) = self.lexer.truncated_identifier() else {
            return Ok(());
        };
        let end = self.lexer.token_end();
        if end <= self.truncation_reported_to {
            return Ok(());
        }
        self.truncation_reported_to = end;
        let (Tok::Ident(truncated) | Tok::QuotedIdent(truncated)) = self.peeked else {
            unreachable!("only identifiers are truncated");
        };
        let message = self
            .arena
            .alloc_str_display(format_args!(
                "identifier \"{full}\" will be truncated to \"{truncated}\""
            ))
            .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
        self.notice(ParseNotice {
            warning: false,
            sqlstate: sqlstate::NAME_TOO_LONG,
            message,
            cursor: None,
            raw: true,
        })
    }

    /// The `$n.field` selections parsed so far.
    pub(crate) fn parameter_fields(&self) -> &'a [(u32, &'a str)] {
        self.parameter_fields.borrow().as_slice()
    }

    pub(crate) fn capture_type_inputs(&mut self) {
        if self.type_input_sources.is_none() {
            self.type_input_sources = Some(core::cell::RefCell::new(TypeInputSources {
                strings: ArenaList::new(self.arena),
                candidates: ArenaList::new(self.arena),
                restored: ArenaList::new(self.arena),
            }));
        }
    }

    pub(crate) fn type_input_candidates(&self) -> &'a [super::type_inputs::Candidate<'a>] {
        self.type_input_sources
            .as_ref()
            .map_or(&[], |sources| sources.borrow().candidates.as_slice())
    }

    pub(crate) fn string_literals(&self) -> &'a [(&'a Expr<'a>, NamespaceSource)] {
        self.type_input_sources
            .as_ref()
            .map_or(&[], |sources| sources.borrow().strings.as_slice())
    }

    pub(crate) fn add_contextual_type_input(
        &self,
        expression: &Expr<'_>,
        target_oid: i32,
        type_mod: i32,
    ) -> Result<(), super::eval::SqlError> {
        let Some(candidate) = super::type_inputs::contextual_candidate(
            self.string_literals(),
            expression,
            target_oid,
            type_mod,
        )?
        else {
            return Ok(());
        };
        let sources = self.type_input_sources.as_ref().ok_or_else(|| {
            crate::sql_err!(
                sqlstate::DATA_EXCEPTION,
                "SQL type input source capture is unavailable"
            )
        })?;
        if let Some(prior) = sources.borrow().candidates.as_slice().iter().find(|prior| {
            (prior.start, prior.end, prior.kind) == (candidate.start, candidate.end, candidate.kind)
        }) {
            if prior.target != candidate.target || prior.type_mod != candidate.type_mod {
                return Err(crate::sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "SQL type input expected type changed"
                ));
            }
            return Ok(());
        }
        sources
            .borrow_mut()
            .candidates
            .push(candidate)
            .map_err(|_| super::query::arena_full_pub())
    }

    pub(crate) fn reorder_type_input_candidates(
        &self,
        order: &[(usize, usize, super::type_inputs::InputKind)],
    ) -> Result<(), super::eval::SqlError> {
        let Some(first) = order.iter().map(|key| key.0).min() else {
            return Ok(());
        };
        let sources = self.type_input_sources.as_ref().ok_or_else(|| {
            crate::sql_err!(
                sqlstate::DATA_EXCEPTION,
                "SQL type input source capture is unavailable"
            )
        })?;
        let current = sources.borrow().candidates.as_slice();
        let key = |candidate: &super::type_inputs::Candidate<'_>| {
            (candidate.start, candidate.end, candidate.kind)
        };
        for (index, wanted) in order.iter().enumerate() {
            if order[..index].contains(wanted)
                || !current.iter().any(|candidate| key(candidate) == *wanted)
            {
                return Err(crate::sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "SQL type input order changed"
                ));
            }
        }
        let mut reordered = ArenaList::new(self.arena);
        for candidate in current.iter().filter(|candidate| candidate.start < first) {
            reordered
                .push(*candidate)
                .map_err(|_| super::query::arena_full_pub())?;
        }
        for wanted in order {
            let candidate = current
                .iter()
                .find(|candidate| key(candidate) == *wanted)
                .expect("checked input occurrence");
            reordered
                .push(*candidate)
                .map_err(|_| super::query::arena_full_pub())?;
        }
        for candidate in current
            .iter()
            .filter(|candidate| candidate.start >= first && !order.contains(&key(candidate)))
        {
            reordered
                .push(*candidate)
                .map_err(|_| super::query::arena_full_pub())?;
        }
        sources.borrow_mut().candidates = reordered;
        Ok(())
    }

    pub(crate) fn restored_type_inputs(
        &self,
    ) -> &'a [(
        super::type_inputs::Candidate<'a>,
        super::type_inputs::Binding<'a>,
    )] {
        self.type_input_sources
            .as_ref()
            .map_or(&[], |sources| sources.borrow().restored.as_slice())
    }

    pub(crate) fn type_inputs_complete(&self) -> bool {
        self.type_input_sources.is_some()
            && self.restored_type_inputs().len() == self.type_inputs.len()
    }

    pub(crate) fn set_type_inputs(&mut self, bindings: &'a [super::type_inputs::Binding<'a>]) {
        self.capture_type_inputs();
        self.type_inputs = bindings;
    }

    fn string_expression(
        &mut self,
        value: &'a str,
        source_at: usize,
    ) -> Result<&'a Expr<'a>, ParseError> {
        let source_end = self.previous_token.map_or(source_at, |(_, _, end)| end);
        let key = (
            source_at,
            source_end,
            super::type_inputs::InputKind::ImplicitLiteral,
        );
        if let Ok(index) = self
            .type_inputs
            .binary_search_by_key(&key, |binding| (binding.start, binding.end, binding.kind))
        {
            let binding = self.type_inputs[index];
            let expression = self.arena_expr(Expr::Str(value))?;
            if let Some(sources) = &self.type_input_sources
                && !sources
                    .borrow()
                    .restored
                    .as_slice()
                    .iter()
                    .any(|(_, prior)| (prior.start, prior.end, prior.kind) == key)
            {
                sources
                    .borrow_mut()
                    .restored
                    .push((
                        super::type_inputs::Candidate {
                            start: source_at,
                            end: source_end,
                            target: super::type_inputs::CandidateTarget::ContextualOid(
                                binding.type_oid,
                            ),
                            kind: binding.kind,
                            type_mod: binding.type_mod,
                            expression,
                        },
                        binding,
                    ))
                    .map_err(|_| self.arena_full("type input source map exceeds SQL arena"))?;
            }
            return self.arena_expr(Expr::BoundLiteral {
                value: crate::sql::ast::BoundLiteralValue::CheckedArrayInput(binding.encoded),
                type_oid: binding.type_oid,
                type_mod: binding.type_mod,
                collation: binding.collation,
                source: expression,
            });
        }
        if let Ok(index) = self.string_parameters.binary_search(&source_at) {
            let parameter = index
                .checked_add(1)
                .and_then(|index| u32::try_from(index).ok())
                .ok_or_else(|| self.arena_full("too many stored string literals"))?;
            self.max_param = self.max_param.max(parameter);
            self.arena_expr(Expr::Param(parameter))
        } else {
            self.arena_expr(Expr::Str(value))
        }
    }

    pub(crate) fn with_routine_parameters(
        mut self,
        routine_name: &'a str,
        parameters: &[crate::storage::RoutineArgumentDef],
    ) -> Result<Self, ParseError> {
        self.routine_name = Some(routine_name);
        for (index, parameter) in parameters.iter().enumerate() {
            if !parameter.name.as_str().is_empty() {
                let allocated = self
                    .arena
                    .alloc_str(parameter.name.as_str())
                    .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
                self.routine_parameters[index] = Some(allocated);
            }
        }
        Ok(self)
    }

    pub(super) fn routine_parameter(
        &self,
        name: &str,
    ) -> Option<(u32, crate::sql::ast::ParameterSource)> {
        self.routine_parameters
            .iter()
            .position(|parameter| parameter.is_some_and(|parameter| parameter == name))
            .map(|index| {
                (
                    index as u32 + 1,
                    crate::sql::ast::ParameterSource::SqlRoutine,
                )
            })
            .or_else(|| {
                self.named_parameters
                    .iter()
                    .find(|parameter| parameter.qualifier.is_none() && parameter.name == name)
                    .map(|parameter| {
                        (
                            parameter.index,
                            crate::sql::ast::ParameterSource::PlpgsqlCursor(parameter.conflict),
                        )
                    })
            })
    }

    pub(super) fn qualified_routine_parameter(
        &self,
        qualifier: &str,
        name: &str,
    ) -> Option<(u32, crate::sql::ast::ParameterSource)> {
        if let Some(parameter) = self
            .named_parameters
            .iter()
            .find(|parameter| parameter.qualifier == Some(qualifier) && parameter.name == name)
        {
            return Some((
                parameter.index,
                crate::sql::ast::ParameterSource::PlpgsqlCursor(parameter.conflict),
            ));
        }
        self.routine_name
            .is_some_and(|routine| routine == qualifier)
            .then(|| self.routine_parameter(name))
            .flatten()
            .or_else(|| self.routine_parameter_field(qualifier))
    }

    /// `parameter.field` in a SQL-language routine body; the qualifier names
    /// the parameter and the reference's column name is the field.
    pub(super) fn routine_parameter_field(
        &self,
        parameter: &str,
    ) -> Option<(u32, crate::sql::ast::ParameterSource)> {
        self.routine_parameters
            .iter()
            .position(|candidate| candidate.is_some_and(|candidate| candidate == parameter))
            .map(|index| {
                (
                    index as u32 + 1,
                    crate::sql::ast::ParameterSource::SqlRoutineField,
                )
            })
    }

    fn column_default_expression(&mut self) -> Result<&'a Expr<'a>, ParseError> {
        let prior = self.stop_default_at_not_null;
        self.stop_default_at_not_null = true;
        let parsed = self.expression(0);
        self.stop_default_at_not_null = prior;
        parsed
    }

    pub(crate) fn source_position(&self) -> usize {
        self.peek_at
    }

    pub(crate) fn source_range(&self, fragment: &str) -> Option<(usize, usize)> {
        let start = (fragment.as_ptr() as usize).checked_sub(self.text.as_ptr() as usize)?;
        let end = start.checked_add(fragment.len())?;
        (end <= self.text.len()
            && self.text.is_char_boundary(start)
            && self.text.is_char_boundary(end))
        .then_some((start, end))
    }

    pub fn max_param(&self) -> u32 {
        self.max_param
    }

    /// Next statement, or None at end of input. Consumes separators.
    pub fn next_stmt(&mut self) -> Result<Option<Stmt<'a>>, ParseError> {
        self.analysis_error_reported = false;
        self.analysis_error = None;
        self.analysis_error_type = None;
        self.rejected_statement = None;
        while self.peeked == Tok::Op(";") {
            self.advance()?;
        }
        if self.peeked == Tok::Eof {
            return Ok(None);
        }
        self.misplaced_default = None;
        let statement = self.statement()?;
        if let Some(at) = self.misplaced_default.take() {
            self.analysis_error(ParseError {
                at,
                message: StackStr::from_str("DEFAULT is not allowed in this context"),
                sqlstate: sqlstate::SYNTAX_ERROR,
                cursor: Some(at),
                hint: None,
                near: None,
                detail: None,
            })?;
        }
        match self.peeked {
            Tok::Op(";") | Tok::Eof => match self.analysis_error.take() {
                Some(error) => {
                    self.analysis_error_reported = true;
                    self.rejected_statement = Some(statement);
                    Err(error)
                }
                None => Ok(Some(statement)),
            },
            _ => Err(self.syntax_error_here()),
        }
    }

    /// The text this parser reads; notice and error offsets index it.
    pub(crate) fn text(&self) -> &'a str {
        self.text
    }

    /// Makes [`Self::next_stmt`] finish a statement that fails parse analysis
    /// before reporting the failure; see [`Self::analysis_error_reported`].
    pub(crate) fn defer_analysis_errors(&mut self) {
        self.defer_analysis_errors = true;
    }

    /// Whether the last [`Self::next_stmt`] error is one PostgreSQL raises
    /// during analysis: parsing can continue with the next statement, and
    /// the error belongs where that statement would have run.
    pub(crate) fn analysis_error_reported(&self) -> bool {
        self.analysis_error_reported
    }

    /// The statement whose deferred analysis error [`Self::next_stmt`] last
    /// returned, and the type a modifier error names.
    pub(crate) fn take_rejected_statement(&mut self) -> Option<(Stmt<'a>, Option<&'a str>)> {
        self.rejected_statement
            .take()
            .map(|statement| (statement, self.analysis_error_type.take()))
    }

    /// Reports an error PostgreSQL raises during parse analysis rather than
    /// in its grammar: immediately, or at the end of the statement when the
    /// caller asked to defer it.
    pub(super) fn analysis_error(&mut self, error: ParseError) -> Result<(), ParseError> {
        if !self.defer_analysis_errors {
            return Err(error);
        }
        if self.analysis_error.is_none() {
            self.analysis_error = Some(error);
        }
        Ok(())
    }

    /// TRUNCATE [TABLE] name [, ...] [RESTART IDENTITY | CONTINUE IDENTITY]
    /// [CASCADE | RESTRICT]. ONLY and `*` are accepted and meaningless here
    /// (no inheritance).
    /// `[schema.]name` — a possibly-qualified relation name in a statement
    /// position (DDL targets, DML tables).
    fn qual_name(&mut self, what: &str) -> Result<QualName<'a>, ParseError> {
        let source = NamespaceSource {
            start: self.peek_at,
            end: self.lexer.token_end(),
        };
        let at = self.peek_at;
        let first = self.col_ident(what)?;
        if self.eat_op(".")? {
            self.record_namespace(source)?;
            let second = self.col_ident(what)?;
            if self.eat_op(".")? {
                let (schema, name) = self.catalog_qualified_name(at, first, second, what)?;
                return Ok(QualName {
                    schema: Some(schema),
                    name,
                });
            }
            Ok(QualName {
                schema: Some(first),
                name: second,
            })
        } else {
            Ok(QualName::bare(first))
        }
    }

    /// The rest of a dotted name after `first.second` when another `.`
    /// follows: `database.schema.name` must name the session's database, and
    /// a longer name is improper, as in PostgreSQL's parse analysis.
    pub(super) fn catalog_qualified_name(
        &mut self,
        at: usize,
        first: &'a str,
        second: &'a str,
        what: &str,
    ) -> Result<(&'a str, &'a str), ParseError> {
        let third = self.col_ident(what)?;
        let mut written = stack_format!(PARSE_MESSAGE_BYTES, "{first}.{second}.{third}");
        let mut parts = 3;
        while self.eat_op(".")? {
            let part = self.col_ident(what)?;
            let _ = core::fmt::Write::write_fmt(&mut written, format_args!(".{part}"));
            parts += 1;
        }
        let error = |sqlstate: &'static str, message: core::fmt::Arguments<'_>| ParseError {
            at,
            message: stack_format!(PARSE_MESSAGE_BYTES, "{}", message),
            sqlstate,
            cursor: Some(at),
            hint: None,
            near: None,
            detail: None,
        };
        if parts > 3 {
            return Err(error(
                sqlstate::SYNTAX_ERROR,
                format_args!("improper qualified name (too many dotted names): {written}"),
            ));
        } else if !crate::sql::eval::funcs::system::is_current_database(first) {
            self.analysis_error(error(
                sqlstate::FEATURE_NOT_SUPPORTED,
                format_args!("cross-database references are not implemented: \"{written}\""),
            ))?;
        }
        Ok((second, third))
    }

    fn any_qual_name(&mut self, what: &str) -> Result<QualName<'a>, ParseError> {
        let source = NamespaceSource {
            start: self.peek_at,
            end: self.lexer.token_end(),
        };
        let at = self.peek_at;
        let first = self.any_ident(what)?;
        if self.eat_op(".")? {
            self.record_namespace(source)?;
            let second = self.any_ident(what)?;
            if self.eat_op(".")? {
                let (schema, name) = self.catalog_qualified_name(at, first, second, what)?;
                return Ok(QualName {
                    schema: Some(schema),
                    name,
                });
            }
            Ok(QualName {
                schema: Some(first),
                name: second,
            })
        } else {
            Ok(QualName::bare(first))
        }
    }

    /// A large-object OID: PostgreSQL's `NumericOnly`, converted with `oidin`
    /// and looked up when the statement runs, so a bad value is an analysis
    /// error.
    fn large_object_id(&mut self) -> Result<crate::sql::ast::LargeObjectId, ParseError> {
        let at = self.peek_at;
        let (text, _) = self.numeric_only()?;
        let failure = match crate::sql::eval::parse_oid(text) {
            Ok(oid) => match crate::sql::ast::LargeObjectId::parse(oid) {
                Some(id) => return Ok(id),
                None => (
                    sqlstate::UNDEFINED_OBJECT,
                    stack_format!(PARSE_MESSAGE_BYTES, "large object {oid} does not exist"),
                ),
            },
            Err(error) => (
                if error.sqlstate == sqlstate::NUMERIC_OUT_OF_RANGE {
                    sqlstate::NUMERIC_OUT_OF_RANGE
                } else {
                    sqlstate::INVALID_TEXT_REPRESENTATION
                },
                stack_format!(PARSE_MESSAGE_BYTES, "{}", error.message),
            ),
        };
        self.analysis_error(ParseError {
            at,
            message: failure.1,
            sqlstate: failure.0,
            cursor: None,
            hint: None,
            near: None,
            detail: None,
        })?;
        Ok(crate::sql::ast::LargeObjectId::parse(1).expect("one is a valid large-object OID"))
    }

    /// DECLARE name [BINARY] [INSENSITIVE|ASENSITIVE] [[NO] SCROLL] CURSOR
    /// [{WITH|WITHOUT} HOLD] FOR select ("declare" not yet consumed).
    fn declare_cursor(&mut self) -> Result<Stmt<'a>, ParseError> {
        let statement_start = self.peek_at;
        self.advance()?; // declare
        let name = self.col_ident("cursor name")?;
        let mut binary = false;
        let mut scroll = crate::sql::cursor::CursorScroll::Default;
        let mut insensitive = false;
        let mut asensitive = false;
        loop {
            if self.eat_ident("binary")? {
                binary = true;
            } else if self.eat_ident("insensitive")? {
                insensitive = true;
            } else if self.eat_ident("asensitive")? {
                asensitive = true;
            } else if self.eat_ident("scroll")? {
                if scroll == crate::sql::cursor::CursorScroll::NoScroll {
                    return Err(ParseError {
                        at: statement_start,
                        message: StackStr::from_str("cannot specify both SCROLL and NO SCROLL"),
                        sqlstate: sqlstate::INVALID_CURSOR_DEFINITION,
                        cursor: None,
                        hint: None,
                        near: None,
                        detail: None,
                    });
                }
                scroll = crate::sql::cursor::CursorScroll::Scroll;
            } else if self.eat_ident("no")? {
                self.expect_ident("scroll")?;
                if scroll == crate::sql::cursor::CursorScroll::Scroll {
                    return Err(ParseError {
                        at: statement_start,
                        message: StackStr::from_str("cannot specify both SCROLL and NO SCROLL"),
                        sqlstate: sqlstate::INVALID_CURSOR_DEFINITION,
                        cursor: None,
                        hint: None,
                        near: None,
                        detail: None,
                    });
                }
                scroll = crate::sql::cursor::CursorScroll::NoScroll;
            } else {
                break;
            }
        }
        if insensitive && asensitive {
            return Err(ParseError {
                at: statement_start,
                message: StackStr::from_str("cannot specify both ASENSITIVE and INSENSITIVE"),
                sqlstate: sqlstate::INVALID_CURSOR_DEFINITION,
                cursor: None,
                hint: None,
                near: None,
                detail: None,
            });
        }
        self.expect_ident("cursor")?;
        let hold = if self.eat_ident("with")? {
            self.expect_ident("hold")?;
            true
        } else {
            if self.eat_ident("without")? {
                self.expect_ident("hold")?;
            }
            false
        };
        self.expect_ident("for")?;
        // Capture the raw SELECT text; validated by parsing it now.
        let start = self.peek_at;
        let query = self.query_select()?;
        if query.with.iter().any(|cte| cte.dml.is_some()) {
            return Err(ParseError {
                at: statement_start,
                message: StackStr::from_str(
                    "DECLARE CURSOR must not contain data-modifying statements in WITH",
                ),
                sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                cursor: None,
                hint: None,
                near: None,
                detail: None,
            });
        }
        if let Some(clause) = query.locking.first() {
            let restriction = if hold {
                Some((
                    "DECLARE CURSOR WITH HOLD",
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "not supported",
                    "Holdable cursors must be READ ONLY.",
                ))
            } else if scroll == crate::sql::cursor::CursorScroll::Scroll {
                Some((
                    "DECLARE SCROLL CURSOR",
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "not supported",
                    "Scrollable cursors must be READ ONLY.",
                ))
            } else if insensitive {
                Some((
                    "DECLARE INSENSITIVE CURSOR",
                    "42P11",
                    "not valid",
                    "Insensitive cursors must be READ ONLY.",
                ))
            } else {
                None
            };
            if let Some((prefix, state, reason, detail)) = restriction {
                crate::sql::eval::stash_diagnostic(Some(StackStr::from_str(detail)), None);
                return Err(ParseError {
                    at: statement_start,
                    message: stack_format!(
                        PARSE_MESSAGE_BYTES,
                        "{} ... {} is {}",
                        prefix,
                        clause.strength.keyword(),
                        reason,
                    ),
                    sqlstate: state,
                    cursor: None,
                    hint: None,
                    near: None,
                    detail: None,
                });
            }
        }
        let end = self.peek_at;
        let sql = self.text[start..end].trim();
        let statement = self.text[statement_start..end].trim();
        Ok(Stmt::DeclareCursor {
            name,
            binary,
            scroll,
            hold,
            statement,
            sql,
        })
    }

    /// FETCH/MOVE [direction] [FROM|IN] cursor ("fetch"/"move" not consumed).
    fn fetch_cursor(&mut self, move_only: bool) -> Result<Stmt<'a>, ParseError> {
        use crate::sql::cursor::FetchMotion;
        self.advance()?; // fetch | move
        let signed_count = |p: &mut Self| -> Result<i32, ParseError> {
            let negative = p.eat_op("-")?;
            if !negative {
                let _ = p.eat_op("+")?;
            }
            match p.peeked {
                Tok::Num(text) => {
                    // PostgreSQL's SignedIconst requires an int32 magnitude.
                    let v: i32 = text.parse().map_err(|_| p.syntax_error_here())?;
                    p.advance()?;
                    Ok(if negative { -v } else { v })
                }
                _ => Err(p.syntax_error_here()),
            }
        };
        let motion = if self.eat_ident("next")? {
            FetchMotion::Count(1)
        } else if self.eat_ident("prior")? {
            FetchMotion::Count(-1)
        } else if self.eat_ident("first")? {
            FetchMotion::Absolute(1)
        } else if self.eat_ident("last")? {
            FetchMotion::Absolute(-1)
        } else if self.eat_ident("absolute")? {
            FetchMotion::Absolute(signed_count(self)?)
        } else if self.eat_ident("relative")? {
            FetchMotion::Relative(signed_count(self)?)
        } else if self.eat_ident("forward")? {
            if self.eat_ident("all")? {
                FetchMotion::All
            } else if matches!(self.peeked, Tok::Num(_) | Tok::Op("-" | "+")) {
                FetchMotion::Count(signed_count(self)?)
            } else {
                FetchMotion::Count(1)
            }
        } else if self.eat_ident("backward")? {
            if self.eat_ident("all")? {
                FetchMotion::BackwardAll
            } else if matches!(self.peeked, Tok::Num(_) | Tok::Op("-" | "+")) {
                match signed_count(self)? {
                    0 => FetchMotion::BackwardZero,
                    count => FetchMotion::Count(-count),
                }
            } else {
                FetchMotion::Count(-1)
            }
        } else if self.eat_ident("all")? {
            FetchMotion::All
        } else if matches!(self.peeked, Tok::Num(_) | Tok::Op("-" | "+")) {
            FetchMotion::Count(signed_count(self)?)
        } else {
            FetchMotion::Count(1)
        };
        if !self.eat_ident("from")? {
            let _ = self.eat_ident("in")?;
        }
        let name = self.col_ident("cursor name")?;
        Ok(Stmt::FetchCursor {
            name,
            motion,
            move_only,
        })
    }

    fn truncate(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.advance()?; // truncate
        let _ = self.eat_ident("table")?;
        let mut names: [QualName<'a>; 16] = [QualName::bare(""); 16];
        let mut n = 0usize;
        loop {
            let _ = self.eat_ident("only")?;
            if n == names.len() {
                return Err(self.err_here("too many tables in TRUNCATE"));
            }
            names[n] = self.qual_name("table name")?;
            n += 1;
            if self.peeked == Tok::Op("*") {
                self.advance()?;
            }
            if self.peeked == Tok::Op(",") {
                self.advance()?;
                continue;
            }
            break;
        }
        let restart_identity = if self.eat_ident("restart")? {
            self.expect_ident("identity")?;
            true
        } else {
            if self.eat_ident("continue")? {
                self.expect_ident("identity")?;
            }
            false
        };
        let cascade = if self.eat_ident("cascade")? {
            true
        } else {
            let _ = self.eat_ident("restrict")?;
            false
        };
        let tables = self
            .arena
            .alloc_slice_copy(&names[..n])
            .map_err(|_| self.err_here("statement too large"))?;
        Ok(Stmt::Truncate {
            tables,
            restart_identity,
            cascade,
        })
    }

    fn lock_table(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.advance()?; // lock
        let _ = self.eat_ident("table")?;
        let mut names = [QualName::bare(""); MAX_LOCK_TABLES];
        let mut count = 0usize;
        loop {
            let _ = self.eat_ident("only")?;
            if count == names.len() {
                return Err(self.err_here("too many tables in LOCK TABLE"));
            }
            names[count] = self.qual_name("table name")?;
            count += 1;
            if self.peeked == Tok::Op("*") {
                self.advance()?;
            }
            if self.peeked == Tok::Op(",") {
                self.advance()?;
                continue;
            }
            break;
        }
        let mode = if self.eat_ident("in")? {
            use super::ast::TableLockMode;
            let first = self.any_ident("lock mode")?;
            let mode = if first.eq_ignore_ascii_case("access") {
                if self.eat_ident("share")? {
                    TableLockMode::AccessShare
                } else {
                    self.expect_ident("exclusive")?;
                    TableLockMode::AccessExclusive
                }
            } else if first.eq_ignore_ascii_case("row") {
                if self.eat_ident("share")? {
                    TableLockMode::RowShare
                } else {
                    self.expect_ident("exclusive")?;
                    TableLockMode::RowExclusive
                }
            } else if first.eq_ignore_ascii_case("share") {
                if self.eat_ident("update")? {
                    self.expect_ident("exclusive")?;
                    TableLockMode::ShareUpdateExclusive
                } else if self.eat_ident("row")? {
                    self.expect_ident("exclusive")?;
                    TableLockMode::ShareRowExclusive
                } else {
                    TableLockMode::Share
                }
            } else if first.eq_ignore_ascii_case("exclusive") {
                TableLockMode::Exclusive
            } else {
                return Err(self.err_here("unrecognized lock mode"));
            };
            self.expect_ident("mode")?;
            mode
        } else {
            super::ast::TableLockMode::AccessExclusive
        };
        let nowait = self.eat_ident("nowait")?;
        let tables = self
            .arena
            .alloc_slice_copy(&names[..count])
            .map_err(|_| self.err_here("statement too large"))?;
        Ok(Stmt::LockTable {
            tables,
            mode,
            nowait,
        })
    }

    fn explain_bool(&mut self) -> Result<bool, ParseError> {
        if self.eat_ident("true")? || self.eat_ident("on")? {
            Ok(true)
        } else if self.eat_ident("false")? || self.eat_ident("off")? {
            Ok(false)
        } else {
            Ok(true)
        }
    }

    /// EXPLAIN [ANALYZE] [VERBOSE] statement and the parenthesized option
    /// grammar shared by current PostgreSQL releases.
    fn explain(&mut self) -> Result<Stmt<'a>, ParseError> {
        use crate::sql::ast::{ExplainFormat, ExplainOptions, ExplainSerialize};

        self.advance()?; // explain
        let mut options = ExplainOptions::DEFAULT;
        let mut summary_specified = false;
        let mut timing_enabled_explicitly = false;
        if self.eat_op("(")? {
            loop {
                let option = self.any_ident("EXPLAIN option")?;
                if option.eq_ignore_ascii_case("format") {
                    let format = self.any_ident("EXPLAIN format")?;
                    options.format = if format.eq_ignore_ascii_case("text") {
                        ExplainFormat::Text
                    } else if format.eq_ignore_ascii_case("json") {
                        ExplainFormat::Json
                    } else if format.eq_ignore_ascii_case("xml") {
                        ExplainFormat::Xml
                    } else if format.eq_ignore_ascii_case("yaml") {
                        ExplainFormat::Yaml
                    } else {
                        return Err(
                            self.err_here("unrecognized value for EXPLAIN option \"format\"")
                        );
                    };
                } else if option.eq_ignore_ascii_case("serialize") {
                    options.serialize = match self.peeked {
                        Tok::Op(",") | Tok::Op(")") => ExplainSerialize::Text,
                        Tok::Ident(format) if format.eq_ignore_ascii_case("none") => {
                            self.advance()?;
                            ExplainSerialize::None
                        }
                        Tok::Ident(format) if format.eq_ignore_ascii_case("text") => {
                            self.advance()?;
                            ExplainSerialize::Text
                        }
                        Tok::Ident(format) if format.eq_ignore_ascii_case("binary") => {
                            self.advance()?;
                            ExplainSerialize::Binary
                        }
                        Tok::Ident(_) => {
                            return Err(self
                                .err_here("unrecognized value for EXPLAIN option \"serialize\""));
                        }
                        _ => {
                            return Err(self.syntax_error_here());
                        }
                    };
                } else {
                    let enabled = self.explain_bool()?;
                    if option.eq_ignore_ascii_case("analyze")
                        || option.eq_ignore_ascii_case("analyse")
                    {
                        options.analyze = enabled;
                    } else if option.eq_ignore_ascii_case("verbose") {
                        options.verbose = enabled;
                    } else if option.eq_ignore_ascii_case("costs") {
                        options.costs = enabled;
                    } else if option.eq_ignore_ascii_case("settings") {
                        options.settings = enabled;
                    } else if option.eq_ignore_ascii_case("buffers") {
                        options.buffers = enabled;
                    } else if option.eq_ignore_ascii_case("wal") {
                        options.wal = enabled;
                    } else if option.eq_ignore_ascii_case("timing") {
                        options.timing = enabled;
                        timing_enabled_explicitly = enabled;
                    } else if option.eq_ignore_ascii_case("summary") {
                        options.summary = enabled;
                        summary_specified = true;
                    } else if option.eq_ignore_ascii_case("memory") {
                        options.memory = enabled;
                    } else if option.eq_ignore_ascii_case("generic_plan") {
                        options.plan_mode = if enabled {
                            ExplainPlanMode::Generic
                        } else {
                            ExplainPlanMode::Custom
                        };
                    } else {
                        return Err(self.err_here("unrecognized EXPLAIN option"));
                    }
                }
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
        } else {
            loop {
                if self.eat_ident("analyze")? || self.eat_ident("analyse")? {
                    options.analyze = true;
                } else if self.eat_ident("verbose")? {
                    options.verbose = true;
                } else {
                    break;
                }
            }
        }
        if !summary_specified {
            options.summary = options.analyze;
        }
        if options.buffers && !options.analyze {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(
                    PARSE_MESSAGE_BYTES,
                    "EXPLAIN option BUFFERS requires ANALYZE"
                ),
                sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                cursor: None,
                hint: None,
                near: None,
                detail: None,
            });
        }
        if options.wal && !options.analyze {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(PARSE_MESSAGE_BYTES, "EXPLAIN option WAL requires ANALYZE"),
                sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                cursor: None,
                hint: None,
                near: None,
                detail: None,
            });
        }
        if timing_enabled_explicitly && !options.analyze {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(
                    PARSE_MESSAGE_BYTES,
                    "EXPLAIN option TIMING requires ANALYZE"
                ),
                sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                cursor: None,
                hint: None,
                near: None,
                detail: None,
            });
        }
        if options.serialize != ExplainSerialize::None && !options.analyze {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(
                    PARSE_MESSAGE_BYTES,
                    "EXPLAIN option SERIALIZE requires ANALYZE"
                ),
                sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                cursor: None,
                hint: None,
                near: None,
                detail: None,
            });
        }
        if options.plan_mode == ExplainPlanMode::Generic && options.analyze {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(
                    PARSE_MESSAGE_BYTES,
                    "EXPLAIN options ANALYZE and GENERIC_PLAN cannot be used together"
                ),
                sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                cursor: None,
                hint: None,
                near: None,
                detail: None,
            });
        }
        let statement = self.statement()?;
        if matches!(statement, Stmt::Explain { .. }) {
            return Err(self.err_here("EXPLAIN cannot be nested"));
        }
        let statement = self
            .arena
            .alloc(statement)
            .map(|statement| &*statement)
            .map_err(|_| self.err_here("statement too large"))?;
        Ok(Stmt::Explain { options, statement })
    }

    fn statement(&mut self) -> Result<Stmt<'a>, ParseError> {
        match self.peeked {
            Tok::Ident("select" | "table" | "values") | Tok::Op("(") => self.query(),
            Tok::Ident("explain") => self.explain(),
            Tok::Ident("with") => self.with_query(),
            Tok::Ident("create") => self.create(),
            Tok::Ident("drop") => self.drop_stmt(),
            Tok::Ident("insert") => self.insert(),
            Tok::Ident("update") => self.update(),
            Tok::Ident("delete") => self.delete(),
            Tok::Ident("merge") => self.merge(),
            Tok::Ident("call") => self.call_procedure(),
            Tok::Ident("do") => self.do_block(),
            Tok::Ident("comment") => self.comment(),
            Tok::Ident("security") => self.security_label(),
            Tok::Ident("load") => self.load(),
            Tok::Ident("truncate") => self.truncate(),
            Tok::Ident("lock") => self.lock_table(),
            Tok::Ident("declare") => self.declare_cursor(),
            Tok::Ident("fetch") => self.fetch_cursor(false),
            Tok::Ident("move") => self.fetch_cursor(true),
            Tok::Ident("close") => {
                self.advance()?;
                if self.eat_ident("all")? {
                    return Ok(Stmt::CloseCursor(None));
                }
                Ok(Stmt::CloseCursor(Some(self.col_ident("cursor name")?)))
            }
            Tok::Ident("discard") => {
                self.advance()?;
                let target = if self.eat_ident("all")? {
                    crate::sql::ast::DiscardTarget::All
                } else if self.eat_ident("plans")? {
                    crate::sql::ast::DiscardTarget::Plans
                } else if self.eat_ident("sequences")? {
                    crate::sql::ast::DiscardTarget::Sequences
                } else if self.eat_ident("temporary")? || self.eat_ident("temp")? {
                    crate::sql::ast::DiscardTarget::Temporary
                } else {
                    return Err(self.syntax_error_here());
                };
                Ok(Stmt::Discard(target))
            }
            Tok::Ident("begin") => {
                self.advance()?;
                let characteristics = self.transaction_modifiers(true)?;
                Ok(Stmt::Begin(
                    characteristics,
                    crate::sql::ast::TransactionStart::Begin,
                ))
            }
            Tok::Ident("start") => {
                self.advance()?;
                self.expect_ident("transaction")?;
                let characteristics = self.transaction_modifiers(false)?;
                Ok(Stmt::Begin(
                    characteristics,
                    crate::sql::ast::TransactionStart::StartTransaction,
                ))
            }
            Tok::Ident("commit") | Tok::Ident("end") => {
                self.advance()?;
                if self.eat_ident("prepared")? {
                    Ok(Stmt::CommitPrepared(self.prepared_transaction_id()?))
                } else {
                    Ok(Stmt::Commit)
                }
            }
            Tok::Ident("rollback") | Tok::Ident("abort") => {
                self.advance()?;
                if self.eat_ident("prepared")? {
                    return Ok(Stmt::RollbackPrepared(self.prepared_transaction_id()?));
                }
                // ROLLBACK TO [SAVEPOINT] name rewinds to a savepoint; plain
                // ROLLBACK aborts the whole transaction.
                if self.eat_ident("to")? {
                    let _ = self.eat_ident("savepoint")?;
                    let name = self.any_ident("savepoint name")?;
                    Ok(Stmt::RollbackToSavepoint(name))
                } else {
                    Ok(Stmt::Rollback)
                }
            }
            Tok::Ident("savepoint") => {
                self.advance()?;
                let name = self.any_ident("savepoint name")?;
                Ok(Stmt::Savepoint(name))
            }
            Tok::Ident("release") => {
                self.advance()?;
                let _ = self.eat_ident("savepoint")?;
                let name = self.any_ident("savepoint name")?;
                Ok(Stmt::ReleaseSavepoint(name))
            }
            Tok::Ident("set") => {
                self.advance()?;
                if self.eat_ident("constraints")? {
                    let targets = if self.eat_ident("all")? {
                        ConstraintTargets::All
                    } else {
                        let mut names = ArenaList::new(self.arena);
                        loop {
                            let name = self.qual_name("constraint name")?;
                            self.push(&mut names, name)?;
                            if !self.eat_op(",")? {
                                break;
                            }
                        }
                        ConstraintTargets::Named(names.as_slice())
                    };
                    let mode = if self.eat_ident("deferred")? {
                        ConstraintMode::Deferred
                    } else {
                        self.expect_ident("immediate")?;
                        ConstraintMode::Immediate
                    };
                    return Ok(Stmt::SetConstraints { targets, mode });
                }
                let session_modifier = self.eat_ident("session")?;
                let local = if session_modifier {
                    false
                } else {
                    self.eat_ident("local")?
                };
                // SESSION is both the optional scope modifier and the first
                // word of SESSION AUTHORIZATION. Thus PostgreSQL accepts all
                // of SET SESSION AUTHORIZATION, SET LOCAL SESSION
                // AUTHORIZATION, and SET SESSION SESSION AUTHORIZATION.
                if (session_modifier && self.eat_ident("authorization")?)
                    || (self.eat_ident("session")? && {
                        self.expect_ident("authorization")?;
                        true
                    })
                {
                    let role = if self.eat_ident("default")? {
                        None
                    } else {
                        Some(self.any_ident("role name")?)
                    };
                    return Ok(Stmt::SetSessionAuthorization {
                        role,
                        local,
                        reset: false,
                    });
                }
                if self.eat_ident("role")? {
                    let role = if self.eat_ident("none")? {
                        None
                    } else {
                        Some(self.any_ident("role name")?)
                    };
                    return Ok(Stmt::SetRole {
                        role,
                        local,
                        reset: false,
                    });
                }
                let transaction = self.eat_ident("transaction")?;
                if transaction && (session_modifier || local) {
                    return Err(self.err_here("SET TRANSACTION does not take a scope modifier"));
                }
                if transaction && self.eat_ident("snapshot")? {
                    return Ok(Stmt::SetTransactionSnapshot(
                        self.str_literal("snapshot identifier")?,
                    ));
                }
                let target = if transaction {
                    Some(TransactionTarget::Current)
                } else if session_modifier && self.eat_ident("characteristics")? {
                    self.expect_ident("as")?;
                    self.expect_ident("transaction")?;
                    Some(TransactionTarget::SessionDefaults)
                } else {
                    None
                };
                if let Some(target) = target {
                    let characteristics = self.parse_transaction_characteristics()?;
                    if characteristics == TransactionCharacteristics::EMPTY {
                        return Err(self.syntax_error_here());
                    }
                    return Ok(Stmt::SetTransaction {
                        target,
                        characteristics,
                    });
                }
                if self.eat_ident("catalog")? {
                    return Ok(Stmt::SetCatalog(self.str_literal("catalog name")?));
                }
                if self.eat_ident("schema")? {
                    if !matches!(self.peeked, Tok::Str(_)) {
                        return Err(self.syntax_error_here());
                    }
                    let RoutineConfigValue::Value(value) =
                        self.configuration_value_list("search_path", false)?
                    else {
                        unreachable!("schema syntax requires a string")
                    };
                    return Ok(Stmt::Set {
                        name: "search_path",
                        value,
                        local,
                        syntax: SettingSyntax::DecodedValue,
                    });
                }
                if self.eat_ident("names")? {
                    let reset = matches!(self.peeked, Tok::Op(";") | Tok::Eof)
                        || self.eat_ident("default")?;
                    let value = if reset {
                        "DEFAULT"
                    } else {
                        self.str_literal("encoding name")?
                    };
                    return Ok(Stmt::Set {
                        name: "client_encoding",
                        value,
                        local,
                        syntax: if reset {
                            SettingSyntax::Default
                        } else {
                            SettingSyntax::DecodedValue
                        },
                    });
                }
                if self.eat_ident("xml")? {
                    self.expect_ident("option")?;
                    let value = if self.eat_ident("document")? {
                        "document"
                    } else {
                        self.expect_ident("content")?;
                        "content"
                    };
                    return Ok(Stmt::Set {
                        name: "xmloption",
                        value,
                        local,
                        syntax: SettingSyntax::DecodedValue,
                    });
                }
                let (name, syntax) = if self.eat_ident("time")? {
                    self.expect_ident("zone")?;
                    ("timezone", SettingSyntax::TimeZone)
                } else {
                    let n = self.configuration_parameter_name()?;
                    if self.eat_ident("from")? {
                        self.expect_ident("current")?;
                        return Ok(Stmt::Set {
                            name: n,
                            value: "",
                            local,
                            syntax: SettingSyntax::FromCurrent,
                        });
                    }
                    if !self.eat_op("=")? {
                        self.expect_ident("to")?;
                    }
                    (n, SettingSyntax::DecodedValue)
                };
                if syntax == SettingSyntax::TimeZone && self.eat_ident("interval")? {
                    let precision = if self.peeked == Tok::Op("(") {
                        Some(self.type_modifier("interval", true, None)?)
                    } else {
                        None
                    };
                    let literal = self.str_literal("time zone interval")?;
                    let (value, type_mod) = if let Some(type_mod) = precision {
                        (literal, type_mod)
                    } else {
                        let (value, type_mod) = self.interval_with_qualifier(literal, None)?;
                        let valid_range = matches!(
                            TypeMod::decode(super::types::ColType::Interval, type_mod),
                            TypeMod::None
                                | TypeMod::IntervalMod {
                                    range: IntervalRange::Hour,
                                    ..
                                }
                                | TypeMod::IntervalMod {
                                    range: IntervalRange::HourToMinute,
                                    ..
                                }
                        );
                        if !valid_range {
                            return Err(
                                self.err_here("time zone interval must be HOUR or HOUR TO MINUTE")
                            );
                        }
                        (value, type_mod)
                    };
                    return Ok(Stmt::Set {
                        name,
                        value,
                        local,
                        syntax: SettingSyntax::TimeZoneInterval(type_mod),
                    });
                }
                let (value, syntax) = if syntax == SettingSyntax::TimeZone {
                    if self.eat_ident("default")? || self.eat_ident("local")? {
                        ("DEFAULT", SettingSyntax::Default)
                    } else if let Tok::Str(value) | Tok::QuotedIdent(value) = self.peeked {
                        self.advance()?;
                        (value, SettingSyntax::DecodedValue)
                    } else {
                        (self.time_zone_setting_value()?, syntax)
                    }
                } else {
                    let numeric_timezone = name.eq_ignore_ascii_case("timezone")
                        && matches!(self.peeked, Tok::Num(_) | Tok::Op("+") | Tok::Op("-"));
                    match self.configuration_value_list(name, true)? {
                        RoutineConfigValue::Default => ("DEFAULT", SettingSyntax::Default),
                        RoutineConfigValue::Value(value) => (
                            value,
                            if numeric_timezone {
                                SettingSyntax::TimeZone
                            } else {
                                SettingSyntax::DecodedValue
                            },
                        ),
                        RoutineConfigValue::Current => {
                            unreachable!("FROM CURRENT is separate syntax")
                        }
                    }
                };
                Ok(Stmt::Set {
                    name,
                    value,
                    local,
                    syntax,
                })
            }
            Tok::Ident("reset") => {
                self.advance()?;
                if self.eat_ident("role")? {
                    Ok(Stmt::SetRole {
                        role: None,
                        local: false,
                        reset: true,
                    })
                } else if self.eat_ident("session")? {
                    self.expect_ident("authorization")?;
                    Ok(Stmt::SetSessionAuthorization {
                        role: None,
                        local: false,
                        reset: true,
                    })
                } else if self.eat_ident("all")? {
                    Ok(Stmt::Reset(None))
                } else if self.eat_ident("time")? {
                    self.expect_ident("zone")?;
                    Ok(Stmt::Reset(Some("timezone")))
                } else if self.eat_ident("transaction")? {
                    self.expect_ident("isolation")?;
                    self.expect_ident("level")?;
                    Ok(Stmt::Reset(Some("transaction_isolation")))
                } else {
                    Ok(Stmt::Reset(Some(self.configuration_parameter_name()?)))
                }
            }
            Tok::Ident("show") => {
                self.advance()?;
                if self.eat_ident("all")? {
                    return Ok(Stmt::ShowAll);
                }
                // SHOW TRANSACTION ISOLATION LEVEL, SHOW TIME ZONE — multi-word
                // spellings the SQL standard and JDBC use.
                if self.eat_ident("transaction")? {
                    self.expect_ident("isolation")?;
                    self.expect_ident("level")?;
                    return Ok(Stmt::Show("transaction_isolation"));
                }
                if self.eat_ident("time")? {
                    self.expect_ident("zone")?;
                    return Ok(Stmt::Show("timezone"));
                }
                let name = self.configuration_parameter_name()?;
                Ok(Stmt::Show(name))
            }
            Tok::Ident("checkpoint") => {
                self.advance()?;
                Ok(Stmt::Checkpoint)
            }
            Tok::Ident("cluster") => self.cluster(),
            Tok::Ident("reindex") => self.reindex(),
            Tok::Ident("vacuum") => self.vacuum_or_analyze(true),
            Tok::Ident("analyze") | Tok::Ident("analyse") => self.vacuum_or_analyze(false),
            Tok::Ident("refresh") => {
                self.advance()?;
                self.expect_ident("materialized")?;
                self.expect_ident("view")?;
                let name = self.qual_name("materialized view name")?;
                Ok(Stmt::RefreshMaterializedView { name })
            }
            Tok::Ident("alter") => self.alter_table(),
            Tok::Ident("import") => self.import_foreign_schema(),
            Tok::Ident("grant") => self.grant_statement(),
            Tok::Ident("revoke") => self.revoke_statement(),
            Tok::Ident("reassign") => {
                self.advance()?;
                self.expect_ident("owned")?;
                self.expect_ident("by")?;
                let roles = self.role_name_list("role name")?;
                self.expect_ident("to")?;
                let new_owner = self.any_ident("new owner")?;
                Ok(Stmt::ReassignOwned { roles, new_owner })
            }
            Tok::Ident("copy") => self.copy_statement(),
            Tok::Ident("prepare") => self.prepare(),
            Tok::Ident("execute") => self.execute_prepared(),
            Tok::Ident("deallocate") => {
                self.advance()?;
                let _ = self.eat_ident("prepare")?;
                if self.eat_ident("all")? {
                    return Ok(Stmt::Deallocate(None));
                }
                let name = self.any_ident("prepared statement name")?;
                Ok(Stmt::Deallocate(Some(name)))
            }
            Tok::Ident("listen") => {
                self.advance()?;
                Ok(Stmt::Listen(self.col_ident("channel name")?))
            }
            Tok::Ident("unlisten") => {
                self.advance()?;
                if self.eat_op("*")? {
                    Ok(Stmt::Unlisten(None))
                } else {
                    Ok(Stmt::Unlisten(Some(self.col_ident("channel name")?)))
                }
            }
            Tok::Ident("notify") => {
                self.advance()?;
                let channel = self.col_ident("channel name")?;
                let payload = if self.eat_op(",")? {
                    match self.peeked {
                        Tok::Str(s) => {
                            self.advance()?;
                            Some(s)
                        }
                        _ => return Err(self.syntax_error_here()),
                    }
                } else {
                    None
                };
                Ok(Stmt::Notify { channel, payload })
            }
            _ => Err(self.syntax_error_here()),
        }
    }

    fn do_block(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("do")?;
        let mut language = None;
        if self.eat_ident("language")? {
            language = Some(self.any_ident("procedural language")?);
        }
        let body = self.str_literal("anonymous code block")?;
        if self.eat_ident("language")? {
            if language.is_some() {
                return Err(self.err_here("conflicting or redundant options"));
            }
            language = Some(self.any_ident("procedural language")?);
        }
        let language = language.unwrap_or("plpgsql");
        if !language.eq_ignore_ascii_case("plpgsql") {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(
                    PARSE_MESSAGE_BYTES,
                    "language \"{}\" does not support inline code execution",
                    language
                ),
                sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                cursor: None,
                hint: None,
                near: None,
                detail: None,
            });
        }
        Ok(Stmt::Do { body })
    }

    fn call_procedure(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.advance()?;
        let name = self.qual_name("procedure name")?;
        self.expect_op("(")?;
        let mut arguments = ArenaList::new(self.arena);
        let mut argument_names = ArenaList::new(self.arena);
        let mut saw_named = false;
        let mut variadic = false;
        if !self.eat_op(")")? {
            loop {
                let this_variadic = self.eat_ident("variadic")?;
                let first = self.expression(0)?;
                let (argument, argument_name) = if self.eat_op("=>")? || self.eat_op(":=")? {
                    let argument_name = match first {
                        Expr::Column {
                            qualifier: None,
                            name,
                        } => *name,
                        _ => {
                            return Err(
                                self.err_here("procedure argument name must be an identifier")
                            );
                        }
                    };
                    let argument = self.expression(0)?;
                    saw_named = true;
                    (argument, Some(argument_name))
                } else {
                    if saw_named {
                        return Err(
                            self.err_here("positional argument cannot follow named argument")
                        );
                    }
                    (first, None)
                };
                self.push(&mut arguments, argument)?;
                self.push(&mut argument_names, argument_name)?;
                if this_variadic {
                    variadic = true;
                    if self.peeked != Tok::Op(")") {
                        return Err(self.err_here("VARIADIC argument must be last"));
                    }
                    self.advance()?;
                    break;
                }
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
        }
        Ok(Stmt::Call {
            name,
            arguments: arguments.as_slice(),
            argument_names: if saw_named {
                argument_names.as_slice()
            } else {
                &[]
            },
            variadic,
        })
    }

    /// A comma-separated projection list (used by SELECT and RETURNING).
    fn select_items(&mut self) -> Result<&'a [SelectItem<'a>], ParseError> {
        let mut items = ArenaList::new(self.arena);
        loop {
            let item = if self.peeked == Tok::Op("*") {
                let at = self.peek_at;
                self.advance()?;
                SelectItem::Wildcard(crate::sql::ast::SourcePosition::new(Some(at)))
            } else if let Some(table) = self.table_wildcard()? {
                // PostgreSQL expands `t.* AS x` like `t.*`, ignoring the label.
                if self.eat_ident("as")? {
                    let _ = self.any_ident("alias")?;
                }
                SelectItem::TableWildcard(table)
            } else {
                let expression_start = self.peek_at;
                let expression = self.expression(0)?;
                let expression_end = self
                    .previous_token
                    .expect("a parsed expression consumed a token")
                    .2;
                let alias = self.alias()?;
                if alias.is_none()
                    && let Some(sources) = &self.column_sources
                {
                    sources
                        .borrow_mut()
                        .projections
                        .push(ProjectionSource {
                            name: super::exec::derived_name(expression),
                            start: expression_start,
                            end: expression_end,
                        })
                        .map_err(|_| self.arena_full("projection source map exceeds SQL arena"))?;
                }
                // A parenthesized `(t.*)` as a whole select item expands to
                // the table's columns, exactly like `t.*` (PostgreSQL); only
                // inside a larger expression (`(t.*)::text`, `row_to_json(t.*)`)
                // does it stay a record.
                match expression {
                    Expr::WholeRow(table) if alias.is_none() => SelectItem::TableWildcard(table),
                    // `(record).*` parsed as the `*`-sentinel field access.
                    Expr::Field { base, field: "*" } if alias.is_none() => {
                        SelectItem::RecordStar(base)
                    }
                    _ => SelectItem::Expr { expression, alias },
                }
            };
            self.push(&mut items, item)?;
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(items.as_slice())
    }

    /// `t.*` (two tokens of lookahead: restores the parser when the item
    /// turns out to be an ordinary expression).
    fn table_wildcard(&mut self) -> Result<Option<&'a str>, ParseError> {
        let table = match self.peeked {
            Tok::Ident(name) | Tok::QuotedIdent(name) => name,
            _ => return Ok(None),
        };
        let mark = self.lexer.mark();
        let (saved_peeked, saved_peek_at) = (self.peeked, self.peek_at);
        let saved_previous = self.previous_token;
        self.advance()?;
        if self.peeked == Tok::Op(".") {
            self.advance()?;
            if self.peeked == Tok::Op("*") {
                self.advance()?;
                // `t.* IS NULL`, `t.* = u.*`: an operand continues it into an
                // expression over the whole row.
                let item_ends = match self.peeked {
                    Tok::Op("," | ")" | ";") | Tok::Eof => true,
                    Tok::Ident(word) => matches!(
                        word,
                        "as" | "from"
                            | "into"
                            | "where"
                            | "group"
                            | "having"
                            | "window"
                            | "order"
                            | "limit"
                            | "offset"
                            | "fetch"
                            | "for"
                            | "union"
                            | "intersect"
                            | "except"
                            | "returning"
                            | "on"
                    ),
                    _ => false,
                };
                if item_ends {
                    return Ok(Some(table));
                }
            }
        }
        self.previous_token = saved_previous;
        self.lexer.reset(mark);
        self.peeked = saved_peeked;
        self.peek_at = saved_peek_at;
        Ok(None)
    }

    fn returning(&mut self) -> Result<crate::sql::ast::Returning<'a>, ParseError> {
        use crate::sql::ast::Returning;
        if !self.eat_ident("returning")? {
            return Ok(Returning::NONE);
        }
        let (old_alias, new_alias) = if self.eat_ident("with")? {
            self.expect_op("(")?;
            let mut old_alias = None;
            let mut new_alias = None;
            loop {
                let is_old = if self.eat_ident("old")? {
                    true
                } else {
                    self.expect_ident("new")?;
                    false
                };
                self.expect_ident("as")?;
                let alias = self.col_ident("RETURNING output alias")?;
                let slot = if is_old {
                    &mut old_alias
                } else {
                    &mut new_alias
                };
                if slot.replace(alias).is_some() {
                    return Err(self.err_here("RETURNING output alias specified more than once"));
                }
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
            if old_alias == new_alias && old_alias.is_some() {
                return Err(self.err_here("RETURNING OLD and NEW output aliases must differ"));
            }
            (old_alias, new_alias)
        } else {
            (None, None)
        };
        Ok(Returning {
            items: self.select_items()?,
            old_alias,
            new_alias,
        })
    }

    /// A SELECT through HAVING, without the trailing ORDER BY / LIMIT / OFFSET
    /// (those belong to the enclosing query so a set operation can share them).
    fn select_core(&mut self) -> Result<Select<'a>, ParseError> {
        self.expect_ident("select")?;
        // The WINDOW clause is written after HAVING, but the names it defines
        // are used in the select list above it, so it is parsed ahead of the
        // list. The scope stays live past this function because the trailing
        // ORDER BY — parsed by our caller — may also use those names; the
        // caller restores the enclosing query's windows once it is done.
        self.windows.truncate(0);
        self.prescan_windows()?;
        let mut distinct_on: &'a [&'a Expr<'a>] = &[];
        let distinct = if self.eat_ident("distinct")? {
            // `DISTINCT ON (expr, ...)`: keep the first row per distinct key.
            if self.eat_ident("on")? {
                self.expect_op("(")?;
                let mut exprs = ArenaList::new(self.arena);
                loop {
                    let expression = self.expression(0)?;
                    self.push(&mut exprs, expression)?;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                distinct_on = exprs.as_slice();
            }
            true
        } else {
            let _ = self.eat_ident("all")?;
            false
        };
        // PostgreSQL permits an empty SELECT list, including before FROM.
        // DISTINCT requires a nonempty list; RETURNING uses select_items directly.
        let empty_list = !distinct
            && matches!(
                self.peeked,
                Tok::Eof
                    | Tok::Op(";" | ")")
                    | Tok::Ident(
                        "into"
                            | "from"
                            | "where"
                            | "group"
                            | "having"
                            | "window"
                            | "union"
                            | "intersect"
                            | "except"
                            | "order"
                            | "limit"
                            | "offset"
                            | "fetch"
                            | "for"
                    )
            );
        let items = if empty_list {
            &[][..]
        } else {
            self.select_items()?
        };

        // `SELECT ... INTO [TABLE] name ...` — a CREATE TABLE AS spelled the
        // old way. Legal only at the top level; a subquery / CTE / set-op branch
        // rejects it as PostgreSQL does. The clause's byte range is recorded so
        // the query can be reconstructed without it.
        if self.peeked == Tok::Ident("into") {
            let into_start = self.peek_at;
            self.advance()?;
            if !self.allow_into {
                return Err(self.err_here("SELECT ... INTO is not allowed here"));
            }
            if self.into_clause.is_some() {
                return Err(self.err_here("multiple INTO clauses in one query"));
            }
            let persistence = if self.eat_ident("unlogged")? {
                RelationPersistence::Unlogged
            } else if self.eat_ident("temporary")? || self.eat_ident("temp")? {
                RelationPersistence::Temporary
            } else {
                RelationPersistence::Permanent
            };
            let _ = self.eat_ident("table")?;
            let name = self.qual_name("table name")?;
            let into_end = self.peek_at;
            self.into_clause = Some((name, persistence, into_start, into_end));
        }

        let from = if self.eat_ident("from")? {
            Some(self.from_clause()?)
        } else {
            None
        };
        let where_clause = self.where_clause()?;
        let (group_by, grouping_sets, grouping_set_quantifier) = if self.eat_ident("group")? {
            self.expect_ident("by")?;
            let quantifier = if self.eat_ident("distinct")? {
                GroupingSetQuantifier::Distinct
            } else {
                let _ = self.eat_ident("all")?;
                GroupingSetQuantifier::All
            };
            let (group_by, grouping_sets) = self.group_by_clause()?;
            (group_by, grouping_sets, quantifier)
        } else {
            (&[][..], &[][..], GroupingSetQuantifier::All)
        };
        let having = if self.eat_ident("having")? {
            Some(self.expression(0)?)
        } else {
            None
        };
        // Consume the clause in its written position; the lookahead above left
        // the cursor before it.
        if self.eat_ident("window")? {
            self.window_definitions()?;
        }
        let mut windows = ArenaList::new(self.arena);
        for &(_, spec) in self.windows.as_slice().iter().flatten() {
            self.push(&mut windows, spec)?;
        }
        let windows = windows.as_slice();
        Ok(Select {
            items,
            distinct,
            distinct_on,
            windows,
            from,
            where_clause,
            group_by,
            grouping_set_quantifier,
            grouping_sets,
            having,
            order_by: &[],
            limit: None,
            offset: None,
            with_ties: false,
            with: &[],
            set_body: None,
            locking: &[],
        })
    }

    /// Parses this SELECT's `WINDOW` clause ahead of the cursor, then restores
    /// the cursor, so `OVER name` resolves while the select list — written
    /// before the clause — is being parsed.
    fn prescan_windows(&mut self) -> Result<(), ParseError> {
        // The overwhelming majority of queries have no WINDOW clause at all;
        // skip the token scan unless the word appears somewhere ahead.
        if !mentions_window(&self.text[self.peek_at..]) {
            return Ok(());
        }
        let mark = self.lexer.mark();
        let (peeked, peek_at) = (self.peeked, self.peek_at);
        let mut depth = 0usize;
        // `AS window` is a column label, not this clause — a reserved word is
        // allowed there, as PostgreSQL allows `SELECT 1 AS window`.
        let mut after_as = false;
        loop {
            if matches!(self.peeked, Tok::Ident("window")) && after_as {
                after_as = false;
                self.advance()?;
                continue;
            }
            after_as = matches!(self.peeked, Tok::Ident("as"));
            match self.peeked {
                Tok::Eof => break,
                Tok::Op("(") => depth += 1,
                // Leaving this SELECT's parentheses: the clause is not ours.
                Tok::Op(")") if depth == 0 => break,
                Tok::Op(")") => depth -= 1,
                Tok::Op(";") if depth == 0 => break,
                // A set operation ends this leaf; the next has its own clause.
                Tok::Ident("union" | "intersect" | "except") if depth == 0 => break,
                Tok::Ident("window") if depth == 0 => {
                    self.advance()?;
                    self.window_definitions()?;
                    break;
                }
                _ => {}
            }
            self.advance()?;
        }
        self.lexer.reset(mark);
        (self.peeked, self.peek_at) = (peeked, peek_at);
        Ok(())
    }

    /// Trailing ORDER BY / LIMIT / OFFSET (any may be absent).
    fn order_limit(&mut self) -> Result<OrderLimit<'a>, ParseError> {
        let mut order = ArenaList::new(self.arena);
        if self.eat_ident("order")? {
            self.expect_ident("by")?;
            loop {
                let expression = self.expression(0)?;
                let descending = self.order_direction()?;
                // Optional NULLS FIRST/LAST; PostgreSQL defaults NULLS LAST
                // for ASC and NULLS FIRST for DESC.
                let nulls_first = if self.eat_ident("nulls")? {
                    if self.eat_ident("first")? {
                        true
                    } else {
                        self.expect_ident("last")?;
                        false
                    }
                } else {
                    descending
                };
                self.push(
                    &mut order,
                    OrderBy {
                        expression,
                        descending,
                        nulls_first,
                    },
                )?;
                if !self.eat_op(",")? {
                    break;
                }
            }
        }
        let order_by = order.as_slice();
        // LIMIT and OFFSET accept either order, as in PostgreSQL.
        let mut limit = None;
        let mut offset = None;
        let mut with_ties = false;
        loop {
            if limit.is_none() && self.eat_ident("limit")? {
                // `LIMIT ALL` is the standard spelling of "no limit"; it leaves
                // the clause unset rather than binding an expression.
                if self.eat_ident("all")? {
                    continue;
                }
                limit = Some(self.expression(0)?);
            } else if offset.is_none() && self.eat_ident("offset")? {
                offset = Some(self.expression(0)?);
                // Accept the noise words ROW/ROWS.
                let _ = self.eat_ident("rows")? || self.eat_ident("row")?;
            } else if limit.is_none() && self.eat_ident("fetch")? {
                // `FETCH { FIRST | NEXT } [count] { ROW | ROWS } { ONLY | WITH
                // TIES }` — the SQL-standard spelling of LIMIT. The count
                // defaults to 1 when omitted.
                if !self.eat_ident("first")? {
                    self.expect_ident("next")?;
                }
                if self.eat_ident("row")? || self.eat_ident("rows")? {
                    limit = Some(&Expr::Int(1));
                } else {
                    limit = Some(self.expression(0)?);
                    if !(self.eat_ident("rows")? || self.eat_ident("row")?) {
                        return Err(self.syntax_error_here());
                    }
                }
                if self.eat_ident("with")? {
                    self.expect_ident("ties")?;
                    with_ties = true;
                } else {
                    self.expect_ident("only")?;
                }
            } else {
                break;
            }
        }
        if with_ties && order_by.is_empty() {
            return Err(self.err_here("WITH TIES cannot be specified without ORDER BY clause"));
        }
        Ok((order_by, limit, offset, with_ties))
    }

    /// Parses ASC, DESC, or PostgreSQL's ordering-operator spelling shared by
    /// query, aggregate, and window ORDER BY lists.
    pub(super) fn order_direction(&mut self) -> Result<bool, ParseError> {
        if self.eat_ident("using")? {
            let (schema, operator) = if self.eat_ident("operator")? {
                self.expect_op("(")?;
                let first = self.any_op_token()?;
                let pair = if self.eat_op(".")? {
                    (Some(first), self.any_op_token()?)
                } else {
                    (None, first)
                };
                self.expect_op(")")?;
                pair
            } else {
                (None, self.any_op_token()?)
            };
            if let Some(schema) = schema
                && schema != "pg_catalog"
            {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(
                        PARSE_MESSAGE_BYTES,
                        "operator {}.{} does not exist",
                        schema,
                        operator
                    ),
                    sqlstate: sqlstate::UNDEFINED_FUNCTION,
                    cursor: None,
                    hint: None,
                    near: None,
                    detail: None,
                });
            }
            match operator {
                "<" => Ok(false),
                ">" => Ok(true),
                _ => Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(
                        PARSE_MESSAGE_BYTES,
                        "operator {} is not a valid ordering operator",
                        operator
                    ),
                    sqlstate: sqlstate::WRONG_OBJECT_TYPE,
                    cursor: None,
                    hint: None,
                    near: None,
                    detail: None,
                }),
            }
        } else if self.eat_ident("desc")? {
            Ok(true)
        } else {
            self.eat_ident("asc")?;
            Ok(false)
        }
    }

    /// A subquery body: a set-operation tree of SELECTs, then the trailing
    /// ORDER BY / LIMIT / OFFSET applying to the whole result. A lone SELECT
    /// (no set operator) folds those clauses back into itself; a genuine
    /// set-operation is carried in `set_body`.
    fn select(&mut self) -> Result<Select<'a>, ParseError> {
        // This is the nesting boundary for every subquery, so it is where a
        // nested SELECT's named windows stop being visible. Swapping in a
        // fresh list (rather than truncating) keeps the enclosing definitions
        // intact: a nested list's growth would overwrite truncated entries.
        let enclosing_windows = core::mem::replace(&mut self.windows, ArenaList::new(self.arena));
        // A subquery / CTE / set-op branch is not a place `SELECT ... INTO` may
        // appear; forbid it here (select_core checks the flag).
        let saved_allow = self.allow_into;
        self.allow_into = false;
        let body = self.set_union()?;
        let (order_by, limit, offset, with_ties) = self.order_limit()?;
        // Row-locking clauses come last, after ORDER BY / LIMIT / OFFSET.
        let locking = self.locking_clauses()?;
        self.allow_into = saved_allow;
        self.windows = enclosing_windows;
        if let SetTree::Select(s) = body {
            let mut sel = **s;
            sel.order_by = order_by;
            sel.limit = limit;
            sel.offset = offset;
            sel.with_ties = with_ties;
            sel.locking = locking;
            return Ok(sel);
        }
        Ok(Select {
            items: &[],
            distinct: false,
            distinct_on: &[],
            windows: &[],
            from: None,
            where_clause: None,
            group_by: &[],
            grouping_set_quantifier: GroupingSetQuantifier::All,
            grouping_sets: &[],
            having: None,
            order_by,
            limit,
            offset,
            with_ties,
            with: &[],
            set_body: Some(body),
            locking,
        })
    }

    /// Parses any query expression into the Select-shaped representation used
    /// by stored definitions and query-bearing commands. WITH is part of that
    /// expression in PostgreSQL, not a top-level-only statement prefix.
    fn query_select(&mut self) -> Result<Select<'a>, ParseError> {
        if self.peeked != Tok::Ident("with") {
            return self.select();
        }
        match self.with_query()? {
            Stmt::Select(select) => Ok(select),
            Stmt::SetQuery(query) => Ok(Select {
                items: &[],
                distinct: false,
                distinct_on: &[],
                windows: &[],
                from: None,
                where_clause: None,
                group_by: &[],
                grouping_set_quantifier: GroupingSetQuantifier::All,
                grouping_sets: &[],
                having: None,
                order_by: query.order_by,
                limit: query.limit,
                offset: query.offset,
                with_ties: query.with_ties,
                with: query.with,
                set_body: Some(query.body),
                locking: query.locking,
            }),
            _ => Err(self.err_here("query must end in SELECT, TABLE, or VALUES")),
        }
    }

    /// Parses the trailing `FOR { UPDATE | NO KEY UPDATE | SHARE | KEY SHARE }
    /// [OF t, …] [NOWAIT | SKIP LOCKED]` row-locking clauses (zero or more).
    fn locking_clauses(&mut self) -> Result<&'a [LockClause<'a>], ParseError> {
        let mut clauses = ArenaList::new(self.arena);
        while self.eat_ident("for")? {
            let strength = if self.eat_ident("update")? {
                LockStrength::Update
            } else if self.eat_ident("no")? {
                self.expect_ident("key")?;
                self.expect_ident("update")?;
                LockStrength::NoKeyUpdate
            } else if self.eat_ident("share")? {
                LockStrength::Share
            } else if self.eat_ident("key")? {
                self.expect_ident("share")?;
                LockStrength::KeyShare
            } else {
                return Err(self.syntax_error_here());
            };
            let mut of = ArenaList::new(self.arena);
            if self.eat_ident("of")? {
                loop {
                    let table = self.col_ident("table name")?;
                    self.push(&mut of, table)?;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
            }
            let wait = if self.eat_ident("nowait")? {
                LockWait::NoWait
            } else if self.eat_ident("skip")? {
                self.expect_ident("locked")?;
                LockWait::SkipLocked
            } else {
                LockWait::Wait
            };
            self.push(
                &mut clauses,
                LockClause {
                    strength,
                    of: of.as_slice(),
                    wait,
                },
            )?;
        }
        Ok(clauses.as_slice())
    }

    /// A top-level query: a set-operation tree of SELECTs, then the trailing
    /// ORDER BY / LIMIT / OFFSET that apply to the whole result. A lone SELECT
    /// (no set operator) folds those clauses back into itself.
    /// `WITH [RECURSIVE] name [(col, ...)] AS (SELECT ...), ... <SELECT body>`.
    fn with_query(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("with")?;
        let recursive = self.eat_ident("recursive")?;
        let placeholder: &'a Select<'a> = self
            .arena
            .alloc(Select {
                items: &[],
                distinct: false,
                distinct_on: &[],
                windows: &[],
                from: None,
                where_clause: None,
                group_by: &[],
                grouping_set_quantifier: GroupingSetQuantifier::All,
                grouping_sets: &[],
                having: None,
                order_by: &[],
                limit: None,
                offset: None,
                with_ties: false,
                with: &[],
                set_body: None,
                locking: &[],
            })
            .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
        let mut ctes = ArenaList::new(self.arena);
        loop {
            let name = self.col_ident("CTE name")?;
            // Optional output-column rename list `name(c1, c2, ...)`.
            let columns = self.column_alias_list()?.unwrap_or(&[]);
            self.expect_ident("as")?;
            let materialization = if self.eat_ident("materialized")? {
                crate::sql::ast::CteMaterialization::Materialized
            } else if self.eat_ident("not")? {
                self.expect_ident("materialized")?;
                crate::sql::ast::CteMaterialization::NotMaterialized
            } else {
                crate::sql::ast::CteMaterialization::Default
            };
            self.expect_op("(")?;
            // A data-modifying CTE body is an INSERT/UPDATE/DELETE/MERGE (run once,
            // its RETURNING becomes the relation); anything else is a query.
            let (boxed, dml) = if matches!(
                self.peeked,
                Tok::Ident("insert")
                    | Tok::Ident("update")
                    | Tok::Ident("delete")
                    | Tok::Ident("merge")
            ) {
                let stmt = self.statement()?;
                let boxed_stmt = self
                    .arena
                    .alloc(stmt)
                    .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
                (placeholder, Some(&*boxed_stmt))
            } else {
                let q = self.query_select()?;
                let boxed = self
                    .arena
                    .alloc(q)
                    .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
                (&*boxed, None)
            };
            self.expect_op(")")?;
            let search = if self.eat_ident("search")? {
                let order = if self.eat_ident("breadth")? {
                    crate::sql::ast::CteSearchOrder::BreadthFirst
                } else {
                    self.expect_ident("depth")?;
                    crate::sql::ast::CteSearchOrder::DepthFirst
                };
                self.expect_ident("first")?;
                self.expect_ident("by")?;
                let columns = self.cte_clause_columns("SEARCH column")?;
                self.expect_ident("set")?;
                let sequence_column = self.col_ident("SEARCH sequence column")?;
                Some(crate::sql::ast::CteSearch {
                    order,
                    columns,
                    sequence_column,
                })
            } else {
                None
            };
            let cycle = if self.eat_ident("cycle")? {
                let columns = self.cte_clause_columns("CYCLE column")?;
                self.expect_ident("set")?;
                let mark_column = self.col_ident("CYCLE mark column")?;
                let mark = if self.eat_ident("to")? {
                    let value = self.expression(0)?;
                    self.expect_ident("default")?;
                    let default = self.expression(0)?;
                    crate::sql::ast::CteCycleMark::Custom { value, default }
                } else {
                    crate::sql::ast::CteCycleMark::Boolean
                };
                self.expect_ident("using")?;
                let path_column = self.col_ident("CYCLE path column")?;
                Some(crate::sql::ast::CteCycle {
                    columns,
                    mark_column,
                    mark,
                    path_column,
                })
            } else {
                None
            };
            if (search.is_some() || cycle.is_some()) && (!recursive || dml.is_some()) {
                return Err(self.err_here("WITH query is not recursive"));
            }
            self.push(
                &mut ctes,
                Cte {
                    name,
                    columns,
                    recursive,
                    materialization,
                    search,
                    cycle,
                    query: boxed,
                    dml,
                },
            )?;
            if !self.eat_op(",")? {
                break;
            }
        }
        let ctes = ctes.as_slice();
        match self.statement()? {
            Stmt::Select(mut sel) => {
                sel.with = ctes;
                Ok(Stmt::Select(sel))
            }
            Stmt::SetQuery(mut q) => {
                q.with = ctes;
                Ok(Stmt::SetQuery(q))
            }
            statement @ (Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) | Stmt::Merge(_)) => {
                let statement = self
                    .arena
                    .alloc(statement)
                    .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
                Ok(Stmt::With {
                    ctes,
                    statement: &*statement,
                })
            }
            _ => {
                Err(self
                    .err_here("WITH must be followed by SELECT, INSERT, UPDATE, DELETE, or MERGE"))
            }
        }
    }

    fn cte_clause_columns(&mut self, what: &'static str) -> Result<&'a [&'a str], ParseError> {
        let mut columns = ArenaList::new(self.arena);
        loop {
            let column = self.col_ident(what)?;
            self.push(&mut columns, column)?;
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(columns.as_slice())
    }

    fn query(&mut self) -> Result<Stmt<'a>, ParseError> {
        let enclosing_windows = core::mem::replace(&mut self.windows, ArenaList::new(self.arena));
        // A top-level query may carry `SELECT ... INTO table`; capture the whole
        // statement's byte span so it can be rewritten to CREATE TABLE AS.
        let stmt_start = self.peek_at;
        let saved_allow = self.allow_into;
        let saved_into = self.into_clause.take();
        self.allow_into = true;
        let body = self.set_union()?;
        let (order_by, limit, offset, with_ties) = self.order_limit()?;
        let locking = self.locking_clauses()?;
        let stmt_end = self.peek_at;
        self.allow_into = saved_allow;
        let into = self.into_clause.take();
        self.into_clause = saved_into;
        self.windows = enclosing_windows;
        if let Some((name, persistence, into_start, into_end)) = into {
            // Reconstruct the query without its INTO clause and hand it to the
            // CREATE TABLE AS machinery.
            let sql = self
                .arena
                .alloc_str_display(format_args!(
                    "{} {}",
                    self.text[stmt_start..into_start].trim_end(),
                    self.text[into_end..stmt_end].trim_start()
                ))
                .map_err(|_| self.arena_full("SELECT INTO query too large for the SQL arena"))?;
            return Ok(Stmt::CreateTableAs {
                name,
                columns: &[],
                sql: sql.trim(),
                with_data: true,
                if_not_exists: false,
                kind: CreateTableAsKind::SelectInto,
                options: crate::sql::ast::TableAsOptions::DEFAULT,
                persistence,
            });
        }
        if let SetTree::Select(s) = body {
            let mut sel = **s;
            sel.order_by = order_by;
            sel.limit = limit;
            sel.offset = offset;
            sel.with_ties = with_ties;
            sel.locking = locking;
            return Ok(Stmt::Select(sel));
        }
        Ok(Stmt::SetQuery(SetQuery {
            with: &[],
            body,
            order_by,
            limit,
            offset,
            with_ties,
            locking,
        }))
    }

    /// UNION / EXCEPT level (lowest precedence, left-associative).
    fn set_union(&mut self) -> Result<&'a SetTree<'a>, ParseError> {
        let mut left = self.set_intersect()?;
        loop {
            let operator = if self.eat_ident("union")? {
                SetOp::Union
            } else if self.eat_ident("except")? {
                SetOp::Except
            } else {
                break;
            };
            let all = self.set_all()?;
            let right = self.set_intersect()?;
            left = self.alloc_set(SetTree::Op {
                syntax: crate::sql::ast::SetOperationSyntax::Explicit,
                operator,
                all,
                left,
                right,
            })?;
        }
        Ok(left)
    }

    /// The rest of a set operation whose first leaf is `left`.
    fn set_union_after(&mut self, left: &'a SetTree<'a>) -> Result<&'a SetTree<'a>, ParseError> {
        let mut left = self.set_intersect_after(left)?;
        loop {
            let operator = if self.eat_ident("union")? {
                SetOp::Union
            } else if self.eat_ident("except")? {
                SetOp::Except
            } else {
                break;
            };
            let all = self.set_all()?;
            let right = self.set_intersect()?;
            left = self.alloc_set(SetTree::Op {
                syntax: crate::sql::ast::SetOperationSyntax::Explicit,
                operator,
                all,
                left,
                right,
            })?;
        }
        Ok(left)
    }

    /// INTERSECT level (binds tighter than UNION / EXCEPT).
    fn set_intersect(&mut self) -> Result<&'a SetTree<'a>, ParseError> {
        let left = self.set_leaf()?;
        self.set_intersect_after(left)
    }

    fn set_intersect_after(
        &mut self,
        mut left: &'a SetTree<'a>,
    ) -> Result<&'a SetTree<'a>, ParseError> {
        while self.eat_ident("intersect")? {
            let all = self.set_all()?;
            let right = self.set_leaf()?;
            left = self.alloc_set(SetTree::Op {
                syntax: crate::sql::ast::SetOperationSyntax::Explicit,
                operator: SetOp::Intersect,
                all,
                left,
                right,
            })?;
        }
        Ok(left)
    }

    fn set_leaf(&mut self) -> Result<&'a SetTree<'a>, ParseError> {
        // A parenthesized branch is itself a set-operation query, and may
        // carry its own trailing ORDER BY / LIMIT / OFFSET (applied to the
        // branch before the outer set operator combines it).
        if self.peeked == Tok::Op("(") {
            self.advance()?;
            let enclosing_windows =
                core::mem::replace(&mut self.windows, ArenaList::new(self.arena));
            let inner = self.set_union()?;
            let (order_by, limit, offset, with_ties) = self.order_limit()?;
            self.windows = enclosing_windows;
            self.expect_op(")")?;
            if order_by.is_empty() && limit.is_none() && offset.is_none() {
                return Ok(inner);
            }
            let sel = match inner {
                SetTree::Select(s) => {
                    let mut sel = **s;
                    sel.order_by = order_by;
                    sel.limit = limit;
                    sel.offset = offset;
                    sel.with_ties = with_ties;
                    sel
                }
                op => Select {
                    items: &[],
                    distinct: false,
                    distinct_on: &[],
                    windows: &[],
                    from: None,
                    where_clause: None,
                    group_by: &[],
                    grouping_set_quantifier: GroupingSetQuantifier::All,
                    grouping_sets: &[],
                    having: None,
                    order_by,
                    limit,
                    offset,
                    with_ties,
                    with: &[],
                    set_body: Some(op),
                    locking: &[],
                },
            };
            let boxed = self
                .arena
                .alloc(sel)
                .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
            return self.alloc_set(SetTree::Select(boxed));
        }
        // `VALUES (row), (row), ...` is a set-operator branch: desugar to
        // `SELECT row UNION ALL SELECT row ...` (each row a FROM-less SELECT).
        if self.peeked == Tok::Ident("values") {
            self.advance()?;
            let mut tree: Option<&'a SetTree<'a>> = None;
            loop {
                self.expect_op("(")?;
                let mut items = ArenaList::new(self.arena);
                loop {
                    // A VALUES column with no outer alias is named `columnN`,
                    // as PostgreSQL names it (a UNION-ALL takes its output names
                    // from the first branch, so naming every row is harmless).
                    let expression = self.expression(0)?;
                    let alias = self
                        .arena
                        .alloc_str_display(format_args!("column{}", items.len() + 1))
                        .map_err(|_| self.err_here("VALUES too large"))?;
                    self.push(
                        &mut items,
                        SelectItem::Expr {
                            expression,
                            alias: Some(alias),
                        },
                    )?;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                let sel = Select {
                    items: items.as_slice(),
                    distinct: false,
                    distinct_on: &[],
                    windows: &[],
                    from: None,
                    where_clause: None,
                    group_by: &[],
                    grouping_set_quantifier: GroupingSetQuantifier::All,
                    grouping_sets: &[],
                    having: None,
                    order_by: &[],
                    limit: None,
                    offset: None,
                    with_ties: false,
                    with: &[],
                    set_body: None,
                    locking: &[],
                };
                let leaf = self.alloc_set(SetTree::Select(
                    self.arena
                        .alloc(sel)
                        .map_err(|_| self.err_here("VALUES too large"))?,
                ))?;
                tree = Some(match tree {
                    None => leaf,
                    Some(l) => self.alloc_set(SetTree::Op {
                        syntax: crate::sql::ast::SetOperationSyntax::Values,
                        operator: SetOp::Union,
                        all: true,
                        left: l,
                        right: leaf,
                    })?,
                });
                if !self.eat_op(",")? {
                    break;
                }
            }
            return Ok(tree.expect("at least one VALUES row"));
        }
        // Lower TABLE to a typed SELECT so every query consumer shares one
        // execution path.
        if self.peeked == Tok::Ident("table") {
            self.advance()?;
            let inheritance = if self.eat_ident("only")? {
                RelationInheritance::Only
            } else {
                RelationInheritance::Descendants
            };
            let parenthesized = inheritance == RelationInheritance::Only && self.eat_op("(")?;
            let first = self.col_ident("table name")?;
            let (schema, table) = if self.eat_op(".")? {
                (Some(first), self.col_ident("table name")?)
            } else {
                (None, first)
            };
            if parenthesized {
                self.expect_op(")")?;
            }
            if inheritance == RelationInheritance::Descendants {
                let _ = self.eat_op("*")?;
            }
            let source = TableRef {
                schema,
                table,
                alias: None,
                subquery: None,
                func_args: None,
                func_argument_names: &[],
                func_variadic: false,
                func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                rows_from: None,
                col_alias: None,
                inheritance,
                sample: None,
                cte: None,
                with_ordinality: false,
                lateral: false,
                authorization_role: None,
                bound_table: None,
                view_access: None,
                join_members: None,
            };
            let select = Select {
                items: self.arena_slice(&[SelectItem::Wildcard(
                    crate::sql::ast::SourcePosition::UNKNOWN,
                )])?,
                distinct: false,
                distinct_on: &[],
                windows: &[],
                from: Some(FromClause {
                    base: source,
                    joins: &[],
                }),
                where_clause: None,
                group_by: &[],
                grouping_set_quantifier: GroupingSetQuantifier::All,
                grouping_sets: &[],
                having: None,
                order_by: &[],
                limit: None,
                offset: None,
                with_ties: false,
                with: &[],
                set_body: None,
                locking: &[],
            };
            return self.alloc_set(SetTree::Select(
                self.arena
                    .alloc(select)
                    .map_err(|_| self.arena_full("statement too large for SQL arena"))?,
            ));
        }
        let core = self.select_core()?;
        let core = self
            .arena
            .alloc(core)
            .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
        self.alloc_set(SetTree::Select(core))
    }

    /// `ALL` or `DISTINCT` after a set operator (DISTINCT is the default).
    fn set_all(&mut self) -> Result<bool, ParseError> {
        if self.eat_ident("all")? {
            Ok(true)
        } else {
            let _ = self.eat_ident("distinct")?;
            Ok(false)
        }
    }

    fn alloc_set(&mut self, tree: SetTree<'a>) -> Result<&'a SetTree<'a>, ParseError> {
        self.arena
            .alloc(tree)
            .map_err(|_| self.arena_full("statement too large for SQL arena"))
            .map(|t| t as &_)
    }

    fn table_ref(&mut self) -> Result<TableRef<'a>, ParseError> {
        // `LATERAL` may precede a subquery or a function FROM item, letting it
        // reference the FROM items to its left. It applies to whichever kind of
        // item follows, so it is captured here and stamped on the result.
        let lateral = self.eat_ident("lateral")?;
        let inheritance = if self.eat_ident("only")? {
            RelationInheritance::Only
        } else {
            RelationInheritance::Descendants
        };
        // ROWS FROM composes function scans in lockstep. Keep the member calls
        // typed as function-only TableRefs so ordinary relations, subqueries,
        // and nested groups cannot enter this state.
        if self.peeked == Tok::Ident("rows") {
            let mark = self.lexer.mark();
            let (saved_peeked, saved_peek_at) = (self.peeked, self.peek_at);
            self.advance()?;
            if self.eat_ident("from")? {
                self.expect_op("(")?;
                let mut functions = ArenaList::new(self.arena);
                loop {
                    let function = self.table_ref()?;
                    if function.func_args.is_none()
                        || function.rows_from.is_some()
                        || function.subquery.is_some()
                        || function.cte.is_some()
                        || function.with_ordinality
                        || function.lateral
                    {
                        return Err(self.err_here("ROWS FROM requires function calls"));
                    }
                    self.push(&mut functions, function)?;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                let with_ordinality = if self.eat_ident("with")? {
                    self.expect_ident("ordinality")?;
                    true
                } else {
                    false
                };
                let alias = if self.eat_ident("as")? {
                    Some(self.col_ident("alias")?)
                } else if let Tok::Ident(word) = self.peeked {
                    if is_column_name_keyword(word) {
                        None
                    } else {
                        self.advance()?;
                        Some(word)
                    }
                } else {
                    None
                };
                let col_alias = self.column_alias_list()?;
                return Ok(TableRef {
                    schema: None,
                    table: "",
                    alias,
                    subquery: None,
                    func_args: None,
                    func_argument_names: &[],
                    func_variadic: false,
                    func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                    rows_from: Some(functions.as_slice()),
                    col_alias,
                    inheritance: RelationInheritance::Descendants,
                    sample: None,
                    cte: None,
                    with_ordinality,
                    lateral,
                    authorization_role: None,
                    bound_table: None,
                    view_access: None,
                    join_members: None,
                });
            }
            self.lexer.reset(mark);
            self.peeked = saved_peeked;
            self.peek_at = saved_peek_at;
        }
        if self.peeked == Tok::Op("(")
            && inheritance == RelationInheritance::Descendants
            && !lateral
            && self.parenthesized_join_ahead()?
        {
            return self.parenthesized_join();
        }
        // Derived table: `(SELECT ...) [[AS] alias]`. PostgreSQL 16 and newer
        // permit an omitted alias; the scope layer gives that otherwise
        // unaddressable relation an internal name.
        if self.peeked == Tok::Op("(") && inheritance == RelationInheritance::Descendants {
            self.advance()?;
            let select = self.query_select()?;
            self.expect_op(")")?;
            let boxed = self
                .arena
                .alloc(select)
                .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
            // After `AS`, only an alias may follow: anything else, including
            // a reserved word, is PostgreSQL's syntax error at that token.
            let explicit_as = self.eat_ident("as")?;
            let alias = if let Tok::Ident(word) = self.peeked {
                if is_column_name_keyword(word) {
                    if explicit_as {
                        return Err(self.syntax_error_here());
                    }
                    None
                } else {
                    self.advance()?;
                    Some(word)
                }
            } else if explicit_as {
                return Err(self.syntax_error_here());
            } else {
                None
            };
            // Optional column-alias list `alias(c1, c2, ...)` renames the
            // derived table's output columns.
            let col_alias = self.column_alias_list()?;
            return Ok(TableRef {
                schema: None,
                table: "",
                alias,
                subquery: Some(boxed),
                func_args: None,
                func_argument_names: &[],
                func_variadic: false,
                func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                rows_from: None,
                col_alias,
                inheritance: RelationInheritance::Descendants,
                sample: None,
                cte: None,
                with_ordinality: false,
                lateral,
                authorization_role: None,
                bound_table: None,
                view_access: None,
                join_members: None,
            });
        }
        let parenthesized = inheritance == RelationInheritance::Only && self.eat_op("(")?;
        let source = NamespaceSource {
            start: self.peek_at,
            end: self.lexer.token_end(),
        };
        let quoted_function_name = matches!(self.peeked, Tok::QuotedIdent(_));
        let name_at = self.peek_at;
        let first = self.col_ident("table name")?;
        let (schema, table) = if self.eat_op(".")? {
            self.record_namespace(source)?;
            let second = self.col_ident("table name")?;
            if self.peeked == Tok::Op(".") && self.eat_op(".")? {
                let (schema, table) =
                    self.catalog_qualified_name(name_at, first, second, "table name")?;
                (Some(schema), table)
            } else {
                (Some(first), second)
            }
        } else {
            (None, first)
        };
        if parenthesized {
            self.expect_op(")")?;
        }
        if inheritance == RelationInheritance::Descendants {
            let _ = self.eat_op("*")?;
        }
        // Table function: `func(args) [WITH ORDINALITY] [AS] alias`. Only valid
        // immediately after the (possibly schema-qualified) name.
        let mut func_argument_names: &'a [Option<&'a str>] = &[];
        let mut func_variadic = false;
        let mut func_args = if schema.is_none()
            && !quoted_function_name
            && table == "json_table"
            && self.peeked == Tok::Op("(")
        {
            Some(self.json_table_arguments()?)
        } else if schema.is_none()
            && !quoted_function_name
            && table == "xmltable"
            && self.peeked == Tok::Op("(")
        {
            Some(self.xml_table_arguments()?)
        } else if self.peeked == Tok::Op("(") {
            self.advance()?;
            let mut args = ArenaList::new(self.arena);
            let mut names = ArenaList::new(self.arena);
            let mut saw_named = false;
            if self.peeked != Tok::Op(")") {
                loop {
                    let this_variadic = self.eat_ident("variadic")?;
                    let first = self.expression(0)?;
                    let (argument, argument_name) =
                        if self.eat_op("=>")? || self.eat_op(":=")? {
                            let argument_name = match first {
                                Expr::Column {
                                    qualifier: None,
                                    name,
                                } => *name,
                                _ => {
                                    return Err(self
                                        .err_here("routine argument name must be an identifier"));
                                }
                            };
                            let argument = self.expression(0)?;
                            saw_named = true;
                            (argument, Some(argument_name))
                        } else {
                            if saw_named {
                                return Err(self
                                    .err_here("positional argument cannot follow named argument"));
                            }
                            (first, None)
                        };
                    self.push(&mut args, argument)?;
                    self.push(&mut names, argument_name)?;
                    if this_variadic {
                        func_variadic = true;
                        if self.peeked != Tok::Op(")") {
                            return Err(self.err_here("VARIADIC argument must be last"));
                        }
                        break;
                    }
                    if !self.eat_op(",")? {
                        break;
                    }
                }
            }
            self.expect_op(")")?;
            if saw_named {
                func_argument_names = names.as_slice();
            }
            Some(args.as_slice())
        } else {
            None
        };
        if func_args.is_some() && inheritance == RelationInheritance::Only {
            return Err(self.err_here("ONLY requires a relation, not a function call"));
        }
        if lateral && func_args.is_none() {
            return Err(self.err_here("LATERAL requires a subquery or function call"));
        }
        // `WITH ORDINALITY` follows the argument list, before any alias.
        let with_ordinality = if func_args.is_some() && self.eat_ident("with")? {
            self.expect_ident("ordinality")?;
            true
        } else {
            false
        };
        let json_to_record = matches!(
            table,
            "json_to_record" | "jsonb_to_record" | "json_to_recordset" | "jsonb_to_recordset"
        ) && func_args.is_some();
        let alias = if self.eat_ident("as")? {
            if json_to_record && self.peeked == Tok::Op("(") {
                None
            } else {
                Some(self.col_ident("alias")?)
            }
        } else if let Tok::Ident(word) = self.peeked {
            if is_column_name_keyword(word) {
                None
            } else {
                self.advance()?;
                Some(word)
            }
        } else {
            None
        };
        let col_alias = if json_to_record && self.peeked == Tok::Op("(") {
            func_args = Some(self.json_record_column_definitions(
                func_args.expect("record function has arguments"),
            )?);
            None
        } else {
            self.column_alias_list()?
        };
        let sample = if func_args.is_none() && self.eat_ident("tablesample")? {
            Some(self.table_sample()?)
        } else {
            None
        };
        let source = TableRef {
            schema,
            table,
            alias,
            subquery: None,
            func_args,
            func_argument_names,
            func_variadic,
            func_source_position: crate::sql::ast::SourcePosition::new(
                func_args.map(|_| source.start),
            ),
            rows_from: None,
            col_alias,
            inheritance,
            sample,
            cte: None,
            with_ordinality,
            lateral,
            authorization_role: None,
            bound_table: None,
            view_access: None,
            join_members: None,
        };
        // PostgreSQL expands only an unqualified FROM UNNEST with multiple
        // positional inputs into independent catalog calls in lockstep.
        if source.schema.is_none()
            && source.table == "unnest"
            && !source.func_variadic
            && source.func_argument_names.is_empty()
            && let Some(arguments) = source.func_args
            && arguments.len() > 1
        {
            let mut functions = ArenaList::new(self.arena);
            for argument in arguments {
                let input = self.arena_slice(&[*argument])?;
                self.push(
                    &mut functions,
                    TableRef {
                        schema: Some("pg_catalog"),
                        table: "unnest",
                        alias: None,
                        func_args: Some(input),
                        with_ordinality: false,
                        lateral: false,
                        col_alias: None,
                        ..source
                    },
                )?;
            }
            return Ok(TableRef {
                table: "unnest",
                func_args: None,
                rows_from: Some(functions.as_slice()),
                ..source
            });
        }
        Ok(source)
    }

    /// Parses `JSON_TABLE` into an ordinary bounded table-function source.
    /// The synthetic expression nodes retain its recursive column grammar so
    /// description and execution consume the same validated representation.
    fn json_table_arguments(&mut self) -> Result<&'a [&'a Expr<'a>], ParseError> {
        self.expect_op("(")?;
        let context = self.expression(0)?;
        self.expect_op(",")?;
        let path = self.expression(0)?;
        let path_name = if self.eat_ident("as")? {
            let name = self.any_ident("JSON path name")?;
            self.arena_expr(Expr::Str(name))?
        } else {
            self.arena_expr(Expr::Null)?
        };
        let passing = if self.eat_ident("passing")? {
            let mut pairs = ArenaList::new(self.arena);
            loop {
                let value = self.expression(0)?;
                self.push(&mut pairs, value)?;
                self.expect_ident("as")?;
                let name = self.any_ident("JSON path variable name")?;
                let name = self.arena_expr(Expr::Str(name))?;
                self.push(&mut pairs, name)?;
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.plain_call("__json_table_passing", pairs.as_slice())?
        } else {
            self.plain_call("__json_table_passing", &[])?
        };
        self.expect_ident("columns")?;
        let columns = self.json_table_columns(0)?;
        let on_error = if self.eat_ident("error")? {
            self.expect_ident("on")?;
            self.expect_ident("error")?;
            "error"
        } else if self.eat_ident("empty")? {
            self.eat_ident("array")?;
            self.expect_ident("on")?;
            self.expect_ident("error")?;
            "empty"
        } else {
            "empty"
        };
        self.expect_op(")")?;
        let behavior = self.arena_expr(Expr::Str(on_error))?;
        self.arena_slice(&[context, path, path_name, passing, columns, behavior])
    }

    fn optional_xml_passing_mechanism(&mut self) -> Result<(), ParseError> {
        if self.eat_ident("by")? && !self.eat_ident("ref")? {
            self.expect_ident("value")?;
        }
        Ok(())
    }

    /// Parses XMLTABLE into a bounded table-function specification shared by
    /// static description, stored queries and execution.
    fn xml_table_arguments(&mut self) -> Result<&'a [&'a Expr<'a>], ParseError> {
        self.expect_op("(")?;
        let namespaces = if self.eat_ident("xmlnamespaces")? {
            self.expect_op("(")?;
            let mut pairs = ArenaList::new(self.arena);
            loop {
                let default = self.eat_ident("default")?;
                if default {
                    return Err(ParseError {
                        at: self.peek_at,
                        message: StackStr::from_str("DEFAULT namespace is not supported"),
                        sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                        cursor: None,
                        hint: None,
                        near: None,
                        detail: None,
                    });
                }
                let uri = self.expression(0)?;
                self.push(&mut pairs, uri)?;
                self.expect_ident("as")?;
                let prefix = self.any_ident("XML namespace prefix")?;
                let prefix = self.arena_expr(Expr::Str(prefix))?;
                self.push(&mut pairs, prefix)?;
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
            self.expect_op(",")?;
            self.plain_call("__xml_namespaces", pairs.as_slice())?
        } else {
            self.plain_call("__xml_namespaces", &[])?
        };
        let row_path = self.prefix()?;
        self.expect_ident("passing")?;
        self.optional_xml_passing_mechanism()?;
        let document = self.prefix()?;
        self.optional_xml_passing_mechanism()?;
        self.expect_ident("columns")?;
        let columns = self.xml_table_columns()?;
        self.expect_op(")")?;
        self.arena_slice(&[row_path, document, columns, namespaces])
    }

    fn xml_table_columns(&mut self) -> Result<&'a Expr<'a>, ParseError> {
        let null = self.arena_expr(Expr::Null)?;
        let mut columns = ArenaList::new(self.arena);
        loop {
            let name = self.col_ident("XMLTABLE column name")?;
            let name = self.arena_expr(Expr::Str(name))?;
            if self.eat_ident("for")? {
                self.expect_ident("ordinality")?;
                let column = self.plain_call("__xml_table_ordinality", &[name])?;
                self.push(&mut columns, column)?;
            } else {
                let (type_name, type_mod) = self.type_name_mod()?;
                let type_name = self.arena_expr(Expr::Str(type_name))?;
                let type_mod = self.arena_expr(Expr::Int(i64::from(type_mod)))?;
                let path = if self.eat_ident("path")? {
                    self.expression(0)?
                } else {
                    name
                };
                let default = if self.eat_ident("default")? {
                    self.column_default_expression()?
                } else {
                    null
                };
                let not_null = if self.eat_ident("not")? {
                    self.expect_ident("null")?;
                    true
                } else {
                    let _ = self.eat_ident("null")?;
                    false
                };
                let not_null = self.arena_expr(Expr::Bool(not_null))?;
                let column = self.plain_call(
                    "__xml_table_column",
                    &[name, type_name, type_mod, path, default, not_null],
                )?;
                self.push(&mut columns, column)?;
            }
            if !self.eat_op(",")? {
                break;
            }
        }
        self.plain_call("__xml_table_columns", columns.as_slice())
    }

    fn json_record_column_definitions(
        &mut self,
        function_args: &'a [&'a Expr<'a>],
    ) -> Result<&'a [&'a Expr<'a>], ParseError> {
        if function_args.len() != 1 {
            return Err(self.err_here("JSON record conversion takes one JSON argument"));
        }
        self.expect_op("(")?;
        let mut definitions = ArenaList::new(self.arena);
        loop {
            let name = self.col_ident("record column name")?;
            let (type_name, type_mod) = self.type_name_mod()?;
            let name = self.arena_expr(Expr::Str(name))?;
            let type_name = self.arena_expr(Expr::Str(type_name))?;
            let type_mod = self.arena_expr(Expr::Int(i64::from(type_mod)))?;
            let definition =
                self.plain_call("__json_record_column", &[name, type_name, type_mod])?;
            self.push(&mut definitions, definition)?;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        let definitions = self.plain_call("__json_record_columns", definitions.as_slice())?;
        self.arena_slice(&[function_args[0], definitions])
    }

    fn json_table_columns(&mut self, depth: u16) -> Result<&'a Expr<'a>, ParseError> {
        if depth >= 64 {
            return Err(self.err_here("JSON_TABLE columns are nested too deeply"));
        }
        self.expect_op("(")?;
        let mut columns = ArenaList::new(self.arena);
        loop {
            let column = self.json_table_column(depth)?;
            self.push(&mut columns, column)?;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        self.plain_call("__json_table_columns", columns.as_slice())
    }

    fn json_table_column(&mut self, depth: u16) -> Result<&'a Expr<'a>, ParseError> {
        if self.eat_ident("nested")? {
            self.eat_ident("path")?;
            let path = self.json_table_path()?;
            let path_name = if self.eat_ident("as")? {
                let name = self.any_ident("JSON path name")?;
                self.arena_expr(Expr::Str(name))?
            } else {
                self.arena_expr(Expr::Null)?
            };
            self.expect_ident("columns")?;
            let columns = self.json_table_columns(depth + 1)?;
            return self.plain_call("__json_table_nested", &[path, path_name, columns]);
        }

        let name = self.col_ident("JSON_TABLE column name")?;
        let name_expr = self.arena_expr(Expr::Str(name))?;
        if self.eat_ident("for")? {
            self.expect_ident("ordinality")?;
            return self.plain_call("__json_table_ordinality", &[name_expr]);
        }

        let (type_name, type_mod) = self.type_name_mod()?;
        let type_expr = self.arena_expr(Expr::Str(type_name))?;
        let type_mod_expr = self.arena_expr(Expr::Int(i64::from(type_mod)))?;
        if self.eat_ident("exists")? {
            let path = if self.eat_ident("path")? {
                self.json_table_path()?
            } else {
                self.json_table_default_path(name)?
            };
            let behavior = if self.eat_ident("error")? {
                "error"
            } else if self.eat_ident("true")? {
                "true"
            } else if self.eat_ident("unknown")? {
                "unknown"
            } else {
                let _ = self.eat_ident("false")?;
                "false"
            };
            if behavior != "false" || self.peeked == Tok::Ident("on") {
                self.expect_ident("on")?;
                self.expect_ident("error")?;
            }
            let behavior = self.arena_expr(Expr::Str(behavior))?;
            return self.plain_call(
                "__json_table_exists",
                &[name_expr, type_expr, type_mod_expr, path, behavior],
            );
        }

        let format_json = if self.eat_ident("format")? {
            self.expect_ident("json")?;
            if self.eat_ident("encoding")? {
                self.expect_ident("utf8")?;
            }
            true
        } else {
            false
        };
        let path = if self.eat_ident("path")? {
            self.json_table_path()?
        } else {
            self.json_table_default_path(name)?
        };
        let wrapper = if self.eat_ident("without")? {
            self.eat_ident("array")?;
            self.expect_ident("wrapper")?;
            "without"
        } else if self.eat_ident("with")? {
            let kind = if self.eat_ident("conditional")? {
                "conditional"
            } else {
                self.eat_ident("unconditional")?;
                "unconditional"
            };
            self.eat_ident("array")?;
            self.expect_ident("wrapper")?;
            kind
        } else {
            "without"
        };
        let quotes = if self.eat_ident("omit")? {
            self.expect_ident("quotes")?;
            if self.eat_ident("on")? {
                self.expect_ident("scalar")?;
                self.expect_ident("string")?;
            }
            "omit"
        } else if self.eat_ident("keep")? {
            self.expect_ident("quotes")?;
            if self.eat_ident("on")? {
                self.expect_ident("scalar")?;
                self.expect_ident("string")?;
            }
            "keep"
        } else {
            "keep"
        };
        let mut empty_code = "null";
        let mut error_code = "null";
        let null = self.arena_expr(Expr::Null)?;
        let mut empty_default = null;
        let mut error_default = empty_default;
        for _ in 0..2 {
            let (code, default) = if self.eat_ident("default")? {
                ("default", self.expression(0)?)
            } else if self.eat_ident("empty")? {
                if self.eat_ident("object")? {
                    ("object", null)
                } else {
                    self.eat_ident("array")?;
                    ("array", null)
                }
            } else if self.eat_ident("error")? {
                ("error", null)
            } else if self.eat_ident("null")? {
                ("null", null)
            } else {
                break;
            };
            self.expect_ident("on")?;
            if self.eat_ident("empty")? {
                empty_code = code;
                empty_default = default;
            } else {
                self.expect_ident("error")?;
                error_code = code;
                error_default = default;
            }
        }
        let format = self.arena_expr(Expr::Str(if format_json { "json" } else { "value" }))?;
        let wrapper = self.arena_expr(Expr::Str(wrapper))?;
        let quotes = self.arena_expr(Expr::Str(quotes))?;
        let empty_code = self.arena_expr(Expr::Str(empty_code))?;
        let error_code = self.arena_expr(Expr::Str(error_code))?;
        self.plain_call(
            "__json_table_value",
            &[
                name_expr,
                type_expr,
                type_mod_expr,
                path,
                format,
                wrapper,
                quotes,
                empty_default,
                error_default,
                empty_code,
                error_code,
            ],
        )
    }

    fn json_table_default_path(&self, name: &str) -> Result<&'a Expr<'a>, ParseError> {
        use core::fmt::Write as _;
        let mut path = StackStr::<512>::new();
        path.write_str("$.")
            .map_err(|_| self.err_here("JSON path too long"))?;
        super::json::write_json_raw_string(name, &mut path)
            .map_err(|_| self.err_here("JSON path too long"))?;
        if path.is_truncated() {
            return Err(self.err_here("JSON path too long"));
        }
        let path = self.arena_str(path.as_str())?;
        self.arena_expr(Expr::Str(path))
    }

    fn json_table_path(&mut self) -> Result<&'a Expr<'a>, ParseError> {
        let parameters = core::mem::take(&mut self.string_parameters);
        let path = self.expression(0);
        self.string_parameters = parameters;
        let path = path?;
        if !matches!(path, Expr::Str(_)) {
            return Err(self
                .err_here("only string constants are supported in JSON_TABLE path specification"));
        }
        Ok(path)
    }

    fn table_sample(&mut self) -> Result<TableSample<'a>, ParseError> {
        let method_at = self.peek_at;
        let method_name = self.any_ident("TABLESAMPLE method")?;
        let method = match method_name {
            "system" => TableSampleMethod::System,
            "bernoulli" => TableSampleMethod::Bernoulli,
            _ => {
                return Err(ParseError {
                    at: method_at,
                    message: stack_format!(
                        PARSE_MESSAGE_BYTES,
                        "tablesample method {} does not exist",
                        method_name
                    ),
                    sqlstate: sqlstate::UNDEFINED_OBJECT,
                    cursor: None,
                    hint: None,
                    near: None,
                    detail: None,
                });
            }
        };
        self.expect_op("(")?;
        let percentage = self.expression(0)?;
        if self.eat_op(",")? {
            let mut count = 1usize;
            loop {
                let _ = self.expression(0)?;
                count += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
            return Err(ParseError {
                at: method_at,
                message: stack_format!(
                    PARSE_MESSAGE_BYTES,
                    "tablesample method {} requires 1 argument, not {}",
                    method_name,
                    count
                ),
                sqlstate: sqlstate::INVALID_TABLESAMPLE_ARGUMENT,
                cursor: None,
                hint: None,
                near: None,
                detail: None,
            });
        }
        self.expect_op(")")?;
        let repeatable = if self.eat_ident("repeatable")? {
            self.expect_op("(")?;
            let seed = self.expression(0)?;
            self.expect_op(")")?;
            Some(seed)
        } else {
            None
        };
        Ok(TableSample {
            method,
            percentage,
            repeatable,
        })
    }

    /// Parses an optional column-alias list `(col1, col2, ...)` following a FROM
    /// item's correlation name. Returns `None` when there is no list.
    fn column_alias_list(&mut self) -> Result<Option<&'a [&'a str]>, ParseError> {
        if self.peeked != Tok::Op("(") {
            return Ok(None);
        }
        self.advance()?;
        let mut columns = ArenaList::new(self.arena);
        loop {
            let column = self.col_ident("column alias")?;
            self.push(&mut columns, column)?;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(Some(columns.as_slice()))
    }

    /// Whether the `(` in `peeked` opens PostgreSQL's parenthesized
    /// `joined_table` rather than a query. Looks ahead without consuming.
    fn parenthesized_join_ahead(&mut self) -> Result<bool, ParseError> {
        let mark = self.lexer.mark();
        let join = self.parenthesized_join_tail();
        self.lexer.reset(mark);
        join
    }

    /// Classifies the parenthesized item whose `(` was just lexed and lexes
    /// through its matching `)`. A query starts with a query keyword; a
    /// nested parenthesized item followed by anything but a set operation
    /// or a query's trailing clause is a table reference inside a join.
    fn parenthesized_join_tail(&mut self) -> Result<bool, ParseError> {
        let mut depth = 1usize;
        let join = match self.lex_ahead()? {
            Tok::Ident("select" | "values" | "with" | "table") => false,
            Tok::Op("(") => {
                let inner = self.parenthesized_join_tail()?;
                match self.lex_ahead()? {
                    Tok::Op(")") => return Ok(inner),
                    Tok::Ident(
                        "union" | "intersect" | "except" | "order" | "limit" | "offset" | "fetch"
                        | "for",
                    ) => false,
                    Tok::Op("(") => {
                        depth += 1;
                        true
                    }
                    Tok::Eof => return Ok(true),
                    _ => true,
                }
            }
            Tok::Op(")") | Tok::Eof => return Ok(false),
            _ => true,
        };
        loop {
            match self.lex_ahead()? {
                Tok::Op("(") => depth += 1,
                Tok::Op(")") => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(join);
                    }
                }
                Tok::Eof => return Ok(join),
                _ => {}
            }
        }
    }

    fn lex_ahead(&mut self) -> Result<Tok<'a>, ParseError> {
        self.lexer
            .next_token()
            .map_err(|error| ParseError::lexical(error, self.text))
    }

    /// `( joined_table ) [[AS] alias [(columns)]]`. With an alias the join is
    /// an ordinary derived table; without one it exposes its members.
    fn parenthesized_join(&mut self) -> Result<TableRef<'a>, ParseError> {
        let star_at = self.peek_at;
        self.expect_op("(")?;
        let mut inner = self.join_chain(false)?;
        if inner.joins.is_empty() {
            // `((a JOIN b ...))`: a parenthesized join is itself a joined
            // table; anything else in parentheses is not.
            match inner.base {
                TableRef {
                    alias: None,
                    join_members: Some(_),
                    subquery:
                        Some(Select {
                            from: Some(from), ..
                        }),
                    ..
                } => inner = *from,
                _ => return Err(self.syntax_error_here()),
            }
        }
        self.expect_op(")")?;
        let explicit_as = self.eat_ident("as")?;
        let alias = match self.peeked {
            Tok::Ident(word) if explicit_as || !is_column_name_keyword(word) => {
                Some(self.col_ident("alias")?)
            }
            Tok::QuotedIdent(_) => Some(self.col_ident("alias")?),
            _ if explicit_as => return Err(self.syntax_error_here()),
            _ => None,
        };
        let col_alias = if alias.is_some() {
            self.column_alias_list()?
        } else {
            None
        };
        self.joined_table_ref(inner, alias, col_alias, star_at)
    }

    /// The derived table a parenthesized join runs as (see
    /// [`TableRef::join_members`]).
    fn joined_table_ref(
        &mut self,
        inner: FromClause<'a>,
        alias: Option<&'a str>,
        col_alias: Option<&'a [&'a str]>,
        star_at: usize,
    ) -> Result<TableRef<'a>, ParseError> {
        let star = SelectItem::Wildcard(crate::sql::ast::SourcePosition::new(Some(star_at)));
        let mut members = ArenaList::new(self.arena);
        for table in core::iter::once(&inner.base).chain(inner.joins.iter().map(|j| &j.table)) {
            if table.alias.is_none()
                && let Some(nested) = table.join_members
            {
                for member in nested {
                    self.push(&mut members, *member)?;
                }
            } else if let Some(name) = table
                .alias
                .or((table.subquery.is_none() && !table.table.is_empty()).then_some(table.table))
            {
                self.push(
                    &mut members,
                    JoinMember {
                        name,
                        using_alias: false,
                    },
                )?;
            }
        }
        for join in inner.joins {
            if let Some(name) = join.using.and_then(|using| using.alias) {
                self.push(
                    &mut members,
                    JoinMember {
                        name,
                        using_alias: true,
                    },
                )?;
            }
        }
        let members = members.as_slice();
        let items = if alias.is_some() {
            self.arena_slice(&[star])?
        } else {
            let mut items = ArenaList::new(self.arena);
            self.push(&mut items, star)?;
            for member in members {
                self.push(&mut items, SelectItem::TableWildcard(member.name))?;
            }
            for member in members.iter().filter(|member| !member.using_alias) {
                let row = self.arena_expr(Expr::WholeRow(member.name))?;
                let presence = self.plain_call("pg_catalog.num_nulls", &[row])?;
                self.push(
                    &mut items,
                    SelectItem::Expr {
                        expression: presence,
                        alias: None,
                    },
                )?;
            }
            items.as_slice()
        };
        let join_members = Some(members);
        let lateral = core::iter::once(&inner.base)
            .chain(inner.joins.iter().map(|j| &j.table))
            .any(|table| table.lateral);
        let select = self
            .arena
            .alloc(Select {
                items,
                distinct: false,
                distinct_on: &[],
                windows: &[],
                from: Some(inner),
                where_clause: None,
                group_by: &[],
                grouping_set_quantifier: crate::sql::ast::GroupingSetQuantifier::All,
                grouping_sets: &[],
                having: None,
                order_by: &[],
                limit: None,
                offset: None,
                with_ties: false,
                with: &[],
                set_body: None,
                locking: &[],
            })
            .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
        Ok(TableRef {
            schema: None,
            table: "",
            alias,
            subquery: Some(select),
            func_args: None,
            func_argument_names: &[],
            func_variadic: false,
            func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
            rows_from: None,
            col_alias,
            inheritance: RelationInheritance::Descendants,
            sample: None,
            cte: None,
            with_ordinality: false,
            lateral,
            authorization_role: None,
            bound_table: None,
            view_access: None,
            join_members,
        })
    }

    #[expect(
        clippy::wrong_self_convention,
        reason = "parses the FROM clause; not a conversion"
    )]
    fn from_clause(&mut self) -> Result<FromClause<'a>, ParseError> {
        self.parse_from_list(false)
    }

    /// `UPDATE ... FROM` and `DELETE ... USING` lists, which follow their
    /// target as if after a comma.
    fn dml_from_clause(&mut self) -> Result<FromClause<'a>, ParseError> {
        self.parse_from_list(true)
    }

    fn parse_from_list(&mut self, after_target: bool) -> Result<FromClause<'a>, ParseError> {
        let from = self.join_chain(true)?;
        // Joins run left-deep, which equals PostgreSQL's tree except where a
        // RIGHT or FULL join follows a comma: `a, b RIGHT JOIN c ON ...`
        // joins `c` to `b` alone, then pairs every result with `a`. Such a
        // comma-separated item runs as its own parenthesized join.
        let first_item_end = from
            .joins
            .iter()
            .position(|join| join.kind == JoinKind::Comma)
            .unwrap_or(from.joins.len());
        let from = if after_target
            && from.joins[..first_item_end]
                .iter()
                .any(|join| matches!(join.kind, JoinKind::Right | JoinKind::Full))
        {
            let inner = FromClause {
                base: from.base,
                joins: self.arena_slice(&from.joins[..first_item_end])?,
            };
            FromClause {
                base: self.joined_table_ref(inner, None, None, 0)?,
                joins: self.arena_slice(&from.joins[first_item_end..])?,
            }
        } else {
            from
        };
        let Some(first_comma) = from
            .joins
            .iter()
            .position(|join| join.kind == JoinKind::Comma)
        else {
            return Ok(from);
        };
        let needs_group = |items: &[Join<'a>]| {
            items
                .iter()
                .any(|join| matches!(join.kind, JoinKind::Right | JoinKind::Full))
        };
        if !needs_group(&from.joins[first_comma..]) {
            return Ok(from);
        }
        let mut joins = ArenaList::new(self.arena);
        for join in &from.joins[..first_comma] {
            self.push(&mut joins, *join)?;
        }
        let mut start = first_comma;
        while start < from.joins.len() {
            let end = from.joins[start + 1..]
                .iter()
                .position(|join| join.kind == JoinKind::Comma)
                .map_or(from.joins.len(), |offset| start + 1 + offset);
            let item = &from.joins[start..end];
            if item.len() > 1 && needs_group(&item[1..]) {
                let inner = FromClause {
                    base: item[0].table,
                    joins: self.arena_slice(&item[1..])?,
                };
                let table = self.joined_table_ref(inner, None, None, 0)?;
                self.push(
                    &mut joins,
                    Join {
                        table,
                        kind: JoinKind::Comma,
                        on: None,
                        using: None,
                        natural: false,
                    },
                )?;
            } else {
                for join in item {
                    self.push(&mut joins, *join)?;
                }
            }
            start = end;
        }
        Ok(FromClause {
            base: from.base,
            joins: joins.as_slice(),
        })
    }

    /// A FROM list (`commas` true) or the join chain inside a parenthesized
    /// join, where a comma ends the chain.
    fn join_chain(&mut self, commas: bool) -> Result<FromClause<'a>, ParseError> {
        let base = self.table_ref()?;
        let mut joins = ArenaList::new(self.arena);
        loop {
            let natural = self.eat_ident("natural")?;
            let kind = if natural {
                if self.eat_ident("inner")? {
                    self.expect_ident("join")?;
                    JoinKind::Inner
                } else if self.eat_ident("left")? {
                    let _ = self.eat_ident("outer")?;
                    self.expect_ident("join")?;
                    JoinKind::Left
                } else if self.eat_ident("right")? {
                    let _ = self.eat_ident("outer")?;
                    self.expect_ident("join")?;
                    JoinKind::Right
                } else if self.eat_ident("full")? {
                    let _ = self.eat_ident("outer")?;
                    self.expect_ident("join")?;
                    JoinKind::Full
                } else {
                    self.expect_ident("join")?;
                    JoinKind::Inner
                }
            } else if commas && self.eat_op(",")? {
                JoinKind::Comma
            } else if self.eat_ident("cross")? {
                self.expect_ident("join")?;
                JoinKind::Cross
            } else if self.eat_ident("inner")? {
                self.expect_ident("join")?;
                JoinKind::Inner
            } else if self.eat_ident("left")? {
                let _ = self.eat_ident("outer")?;
                self.expect_ident("join")?;
                JoinKind::Left
            } else if self.eat_ident("right")? {
                let _ = self.eat_ident("outer")?;
                self.expect_ident("join")?;
                JoinKind::Right
            } else if self.eat_ident("full")? {
                let _ = self.eat_ident("outer")?;
                self.expect_ident("join")?;
                JoinKind::Full
            } else if self.eat_ident("join")? {
                JoinKind::Inner
            } else {
                break;
            };
            let table = self.table_ref()?;
            let mut using = None;
            let on = if natural || kind.is_cross() {
                None
            } else if self.eat_ident("using")? {
                // The merged-column semantics (single output column, resolved
                // against the whole left join tree) are applied at plan time,
                // where the joined tables' columns are known.
                self.expect_op("(")?;
                let mut cols = ArenaList::new(self.arena);
                loop {
                    let column = self.col_ident("column name")?;
                    self.push(&mut cols, column)?;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                let columns = cols.as_slice();
                let alias = if self.eat_ident("as")? {
                    Some(self.col_ident("join USING alias")?)
                } else {
                    None
                };
                using = Some(JoinUsing { columns, alias });
                None
            } else {
                self.expect_ident("on")?;
                Some(self.expression(0)?)
            };
            self.push(
                &mut joins,
                Join {
                    table,
                    kind,
                    on,
                    using,
                    natural,
                },
            )?;
        }
        Ok(FromClause {
            base,
            joins: joins.as_slice(),
        })
    }

    /// PostgreSQL's `opt_program copy_file_name copy_delimiter` after `TO` or
    /// `FROM`. `STDIN` and `STDOUT` both name the client in either direction;
    /// a server file or program is rejected when the statement would run.
    fn copy_client_target(&mut self, from: bool) -> Result<Option<(&'a str, usize)>, ParseError> {
        let at = self.peek_at;
        let program = self.eat_ident("program")?;
        let server_side = match self.peeked {
            Tok::Ident("stdin" | "stdout") if !program => false,
            Tok::Str(_) => true,
            _ => return Err(self.syntax_error_here()),
        };
        self.advance()?;
        if server_side {
            self.analysis_error(ParseError {
                at,
                message: stack_format!(
                    PARSE_MESSAGE_BYTES,
                    "COPY {} a server {} is not supported",
                    if from { "from" } else { "to" },
                    if program { "program" } else { "file" }
                ),
                near: None,
                detail: None,
                sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                cursor: None,
                hint: Some("Anyone can COPY to stdout or from stdin. psql's \\copy command also works for anyone."),
            })?;
        }
        // The pre-7.3 `[USING] DELIMITERS 'x'` clause, a DELIMITER option.
        let using = self.eat_ident("using")?;
        if !using && self.peeked != Tok::Ident("delimiters") {
            return Ok(None);
        }
        let at = self.peek_at;
        self.expect_ident("delimiters")?;
        let Tok::Str(delimiter) = self.peeked else {
            return Err(self.syntax_error_here());
        };
        self.advance()?;
        Ok(Some((delimiter, at)))
    }

    /// `COPY table [(col, ...)] FROM STDIN | TO STDOUT [[WITH] (options)]`.
    /// The resolved option state is typed before execution so streaming COPY
    /// cannot accept an option then lose it between protocol messages.
    fn copy_statement(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("copy")?;
        // `COPY (query) TO STDOUT`: a parenthesized query stands in for a table
        // (a column list requires a table, so `(` here is always a query).
        if self.peeked == Tok::Op("(") {
            self.advance()?;
            let start = self.peek_at;
            let _ = self.query_select()?;
            let end = self.peek_at;
            self.expect_op(")")?;
            let query = self.text[start..end].trim();
            self.expect_ident("to")?;
            let delimiters = self.copy_client_target(false)?;
            let options = self.copy_options(false, delimiters)?;
            return Ok(Stmt::Copy(crate::sql::ast::CopyStmt {
                table: QualName {
                    schema: None,
                    name: "",
                },
                columns: &[],
                query: Some(query),
                to: true,
                options,
                where_clause: None,
                where_text: None,
            }));
        }
        let table = self.qual_name("table name")?;
        let mut columns = [""; crate::storage::MAX_COLUMNS];
        let mut n_columns = 0usize;
        if self.peeked == Tok::Op("(") {
            self.advance()?;
            loop {
                if n_columns == columns.len() {
                    return Err(self.limit("COPY column list", columns.len()));
                }
                columns[n_columns] = self.col_ident("column name")?;
                n_columns += 1;
                if self.peeked == Tok::Op(",") {
                    self.advance()?;
                    continue;
                }
                break;
            }
            if self.peeked != Tok::Op(")") {
                return Err(self.syntax_error_here());
            }
            self.advance()?;
        }
        let to = if self.eat_ident("to")? {
            true
        } else if self.eat_ident("from")? {
            false
        } else {
            return Err(self.syntax_error_here());
        };
        let delimiters = self.copy_client_target(!to)?;
        let options = self.copy_options(!to, delimiters)?;
        let (where_clause, where_text) = if self.eat_ident("where")? {
            let start = self.peek_at;
            let predicate = self.expression(0)?;
            (
                Some(predicate),
                Some(self.text[start..self.peek_at].trim_end()),
            )
        } else {
            (None, None)
        };
        let columns = self.arena_slice(&columns[..n_columns])?;
        Ok(Stmt::Copy(crate::sql::ast::CopyStmt {
            table,
            columns,
            query: None,
            to,
            options,
            where_clause,
            where_text,
        }))
    }

    /// A required single-quoted string literal (e.g. an enum label).
    /// `_what` names the expected item for readers; PostgreSQL's error names
    /// only the token found.
    pub(super) fn str_literal(&mut self, _what: &'static str) -> Result<&'a str, ParseError> {
        match self.peeked {
            Tok::Str(s) => {
                self.advance()?;
                Ok(s)
            }
            _ => Err(self.syntax_error_here()),
        }
    }

    /// The tail of `ALTER [COLUMN] col [SET DATA] TYPE newtype [USING expr]`,
    /// after the `TYPE` keyword.
    fn alter_column_type(&mut self, column: &'a str) -> Result<AlterAction<'a>, ParseError> {
        let (type_name, type_mod) = self.type_name_mod()?;
        let collation = if self.eat_ident("collate")? {
            Some(self.collation_name()?)
        } else {
            None
        };
        let using = if self.eat_ident("using")? {
            Some(self.expression(0)?)
        } else {
            None
        };
        Ok(AlterAction::AlterColumnType {
            column,
            type_name,
            type_mod,
            collation,
            using,
        })
    }

    /// VACUUM / ANALYZE with their shared shape: an optional parenthesized or
    /// bare option list, then an optional comma-separated table list, each
    /// table with an optional column list.
    fn vacuum_or_analyze(&mut self, is_vacuum: bool) -> Result<Stmt<'a>, ParseError> {
        self.advance()?; // the VACUUM / ANALYZE keyword
        let mut options = crate::sql::ast::VacuumOptions {
            analyze: !is_vacuum,
            ..crate::sql::ast::VacuumOptions::DEFAULT
        };
        let mut saw_full = false;
        let mut saw_analyze = false;
        if self.eat_op("(")? {
            loop {
                let option = self.any_ident("VACUUM option")?;
                let value = {
                    let _ = self.eat_op("=")?;
                    self.explain_bool()?
                };
                if option.eq_ignore_ascii_case("full") && is_vacuum {
                    if core::mem::replace(&mut saw_full, true) {
                        return Err(self.err_here("VACUUM option FULL specified more than once"));
                    }
                    options.full = value;
                } else if (option.eq_ignore_ascii_case("analyze")
                    || option.eq_ignore_ascii_case("analyse"))
                    && is_vacuum
                {
                    if core::mem::replace(&mut saw_analyze, true) {
                        return Err(self.err_here("VACUUM option ANALYZE specified more than once"));
                    }
                    options.analyze = value;
                } else {
                    return Err(ParseError {
                        at: self.peek_at,
                        message: stack_format!(
                            PARSE_MESSAGE_BYTES,
                            "unsupported {} option \"{}\"",
                            if is_vacuum { "VACUUM" } else { "ANALYZE" },
                            option
                        ),
                        sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                        cursor: None,
                        hint: None,
                        near: None,
                        detail: None,
                    });
                }
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
        } else {
            loop {
                if self.eat_ident("full")? && is_vacuum {
                    if core::mem::replace(&mut saw_full, true) {
                        return Err(self.err_here("VACUUM option FULL specified more than once"));
                    }
                    options.full = true;
                    continue;
                }
                if self.eat_ident("analyze")? || self.eat_ident("analyse")? {
                    if !is_vacuum {
                        return Err(ParseError {
                            at: self.peek_at,
                            message: stack_format!(
                                PARSE_MESSAGE_BYTES,
                                "unsupported ANALYZE option \"ANALYZE\""
                            ),
                            sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                            cursor: None,
                            hint: None,
                            near: None,
                            detail: None,
                        });
                    }
                    if core::mem::replace(&mut saw_analyze, true) {
                        return Err(self.err_here("VACUUM option ANALYZE specified more than once"));
                    }
                    options.analyze = true;
                    continue;
                }
                if matches!(
                    self.peeked,
                    Tok::Ident(
                        "full"
                            | "freeze"
                            | "verbose"
                            | "skip_locked"
                            | "disable_page_skipping"
                            | "index_cleanup"
                            | "process_main"
                            | "process_toast"
                            | "truncate"
                            | "parallel"
                            | "skip_database_stats"
                            | "only_database_stats"
                            | "buffer_usage_limit"
                    )
                ) {
                    let option = self.any_ident("maintenance option")?;
                    return Err(ParseError {
                        at: self.peek_at,
                        message: stack_format!(
                            PARSE_MESSAGE_BYTES,
                            "unsupported {} option \"{}\"",
                            if is_vacuum { "VACUUM" } else { "ANALYZE" },
                            option
                        ),
                        sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                        cursor: None,
                        hint: None,
                        near: None,
                        detail: None,
                    });
                }
                break;
            }
        }
        // An optional table list, each with an optional column list.
        let empty_target = crate::sql::ast::MaintenanceTarget {
            table: QualName::bare(""),
            columns: &[],
            inheritance: crate::sql::ast::RelationInheritance::Descendants,
        };
        let mut targets = [empty_target; 64];
        let mut target_count = 0usize;
        if !matches!(self.peeked, Tok::Op(";") | Tok::Eof) {
            loop {
                if target_count == targets.len() {
                    return Err(self.limit("maintenance target list", targets.len()));
                }
                let inheritance = if self.eat_ident("only")? {
                    crate::sql::ast::RelationInheritance::Only
                } else {
                    crate::sql::ast::RelationInheritance::Descendants
                };
                let table = self.qual_name("table name")?;
                if self.eat_op("*")? && inheritance == crate::sql::ast::RelationInheritance::Only {
                    return Err(self.err_here("ONLY and * cannot be used together"));
                }
                let mut column_names = [""; crate::storage::MAX_COLUMNS];
                let mut column_count = 0usize;
                if self.eat_op("(")? {
                    loop {
                        if column_count == column_names.len() {
                            return Err(self.limit("maintenance column list", column_names.len()));
                        }
                        column_names[column_count] = self.col_ident("column name")?;
                        column_count += 1;
                        if !self.eat_op(",")? {
                            break;
                        }
                    }
                    self.expect_op(")")?;
                }
                targets[target_count] = crate::sql::ast::MaintenanceTarget {
                    table,
                    columns: self.arena_slice(&column_names[..column_count])?,
                    inheritance,
                };
                target_count += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
        }
        let targets = self.arena_slice(&targets[..target_count])?;
        Ok(if is_vacuum {
            Stmt::Vacuum { targets, options }
        } else {
            Stmt::Analyze(targets)
        })
    }

    fn role_name_list(&mut self, what: &str) -> Result<&'a [&'a str], ParseError> {
        let mut names = ArenaList::new(self.arena);
        loop {
            let name = self.any_ident(what)?;
            self.push(&mut names, name)?;
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(names.as_slice())
    }

    fn privilege_start(&self) -> bool {
        matches!(
            self.peeked,
            Tok::Ident(
                "all"
                    | "select"
                    | "insert"
                    | "update"
                    | "delete"
                    | "truncate"
                    | "references"
                    | "trigger"
                    | "usage"
                    | "create"
                    | "execute"
                    | "maintain"
                    | "connect"
                    | "temporary"
                    | "temp"
                    | "set"
                    | "alter"
            )
        )
    }

    fn grant_statement(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("grant")?;
        if !self.privilege_start() {
            return self.grant_role_after_keyword();
        }
        let privileges = self.privilege_list()?;
        self.expect_ident("on")?;
        let target = self.privilege_target()?;
        self.expect_ident("to")?;
        let grantees = self.role_name_list("grantee")?;
        let grant_option = if self.eat_ident("with")? {
            self.expect_ident("grant")?;
            self.expect_ident("option")?;
            true
        } else {
            false
        };
        let grantor = if self.eat_ident("granted")? {
            self.expect_ident("by")?;
            Some(self.any_ident("grantor role name")?)
        } else {
            None
        };
        if let crate::sql::ast::PrivilegeTarget::Parameters(names) = target {
            return Ok(Stmt::GrantParameterPrivileges {
                privileges: self.parameter_privileges(privileges)?,
                names,
                grantees,
                grant_option,
                grantor,
            });
        }
        Ok(Stmt::GrantPrivileges {
            privileges,
            target,
            grantees,
            grant_option,
            grantor,
        })
    }

    fn grant_role_after_keyword(&mut self) -> Result<Stmt<'a>, ParseError> {
        let roles = self.role_name_list("role name")?;
        self.expect_ident("to")?;
        let members = self.role_name_list("member role name")?;
        let mut options = crate::sql::ast::RoleMembershipPatch::EMPTY;
        if self.eat_ident("with")? {
            loop {
                if self.eat_ident("admin")? {
                    if options.admin.is_some() {
                        return Err(self.err_here("ADMIN option specified more than once"));
                    }
                    options.admin = Some(
                        if self.peeked == Tok::Ident("true") || self.peeked == Tok::Ident("false") {
                            self.role_option_boolean()?
                        } else {
                            let _ = self.eat_ident("option")?;
                            true
                        },
                    );
                } else if self.eat_ident("inherit")? {
                    if options.inherit.is_some() {
                        return Err(self.err_here("INHERIT option specified more than once"));
                    }
                    options.inherit = Some(self.role_option_boolean()?);
                } else if self.eat_ident("set")? {
                    if options.set.is_some() {
                        return Err(self.err_here("SET option specified more than once"));
                    }
                    options.set = Some(self.role_option_boolean()?);
                } else {
                    break;
                }
                let _ = self.eat_op(",")?;
            }
        }
        let grantor = if self.eat_ident("granted")? {
            self.expect_ident("by")?;
            Some(self.any_ident("grantor role name")?)
        } else {
            None
        };
        Ok(Stmt::GrantRole {
            syntax: crate::sql::ast::RoleMembershipSyntax::GrantRevoke,
            roles,
            members,
            options,
            grantor,
        })
    }

    fn privilege_list(&mut self) -> Result<&'a [crate::sql::ast::PrivilegeSpec<'a>], ParseError> {
        use crate::sql::ast::{Privilege, PrivilegeSpec};
        let mut privileges = [PrivilegeSpec {
            privilege: Privilege::All,
            columns: &[],
        }; 16];
        let mut count = 0usize;
        loop {
            if count == privileges.len() {
                return Err(self.limit("privileges", privileges.len()));
            }
            let privilege = if self.eat_ident("all")? {
                let _ = self.eat_ident("privileges")?;
                Privilege::All
            } else if self.eat_ident("select")? {
                Privilege::Select
            } else if self.eat_ident("insert")? {
                Privilege::Insert
            } else if self.eat_ident("update")? {
                Privilege::Update
            } else if self.eat_ident("delete")? {
                Privilege::Delete
            } else if self.eat_ident("truncate")? {
                Privilege::Truncate
            } else if self.eat_ident("references")? {
                Privilege::References
            } else if self.eat_ident("trigger")? {
                Privilege::Trigger
            } else if self.eat_ident("usage")? {
                Privilege::Usage
            } else if self.eat_ident("create")? {
                Privilege::Create
            } else if self.eat_ident("execute")? {
                Privilege::Execute
            } else if self.eat_ident("maintain")? {
                Privilege::Maintain
            } else if self.eat_ident("connect")? {
                Privilege::Connect
            } else if self.eat_ident("temporary")? || self.eat_ident("temp")? {
                Privilege::Temporary
            } else if self.eat_ident("set")? {
                Privilege::Set
            } else if self.eat_ident("alter")? {
                self.expect_ident("system")?;
                Privilege::AlterSystem
            } else {
                return Err(self.syntax_error_here());
            };
            let columns = if self.eat_op("(")? {
                if !matches!(
                    privilege,
                    Privilege::All
                        | Privilege::Select
                        | Privilege::Insert
                        | Privilege::Update
                        | Privilege::References
                ) {
                    return Err(self.err_here("privilege does not support a column list"));
                }
                let mut columns = ArenaList::new(self.arena);
                loop {
                    let column = self.col_ident("column name")?;
                    self.push(&mut columns, column)?;
                    if self.eat_op(")")? {
                        break;
                    }
                    self.expect_op(",")?;
                }
                columns.as_slice()
            } else {
                &[]
            };
            privileges[count] = PrivilegeSpec { privilege, columns };
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.arena_slice(&privileges[..count])
    }

    fn parameter_privileges(
        &self,
        specifications: &[crate::sql::ast::PrivilegeSpec<'_>],
    ) -> Result<crate::sql::ast::ParameterPrivileges, ParseError> {
        use crate::sql::ast::{ParameterPrivileges, Privilege};
        let mut result = ParameterPrivileges::NONE;
        for specification in specifications {
            if !specification.columns.is_empty() {
                return Err(self.err_here("parameter privileges cannot name columns"));
            }
            result = result.union(match specification.privilege {
                Privilege::All => ParameterPrivileges::ALL,
                Privilege::Set => ParameterPrivileges::SET,
                Privilege::AlterSystem => ParameterPrivileges::ALTER_SYSTEM,
                _ => return Err(self.err_here("invalid privilege type for parameter")),
            });
        }
        Ok(result)
    }

    fn default_privilege_list(&mut self) -> Result<&'a [crate::sql::ast::Privilege], ParseError> {
        let specifications = self.privilege_list()?;
        let mut privileges = [crate::sql::ast::Privilege::All; 12];
        for (index, specification) in specifications.iter().enumerate() {
            if !specification.columns.is_empty() {
                return Err(self.err_here("default privileges cannot name columns"));
            }
            privileges[index] = specification.privilege;
        }
        self.arena_slice(&privileges[..specifications.len()])
    }

    fn privilege_target(&mut self) -> Result<crate::sql::ast::PrivilegeTarget<'a>, ParseError> {
        use crate::sql::ast::{
            ParameterName, PrivilegeObjectKind, PrivilegeTarget, RoutineTargetKind,
        };
        if self.eat_ident("parameter")? {
            let mut names = ArenaList::new(self.arena);
            loop {
                let name = self.configuration_parameter_name()?;
                let name = ParameterName::parse(name)
                    .ok_or_else(|| self.err_here("invalid configuration parameter name"))?;
                self.push(&mut names, name)?;
                if !self.eat_op(",")? {
                    break;
                }
            }
            return Ok(PrivilegeTarget::Parameters(names.as_slice()));
        }
        if self.eat_ident("large")? {
            self.expect_ident("object")?;
            let mut objects = ArenaList::new(self.arena);
            loop {
                let object = self.large_object_id()?;
                self.push(&mut objects, object)?;
                if !self.eat_op(",")? {
                    break;
                }
            }
            return Ok(PrivilegeTarget::LargeObjects(objects.as_slice()));
        }
        let kind = if self.eat_ident("all")? {
            if self.eat_ident("tables")? {
                self.expect_ident("in")?;
                self.expect_ident("schema")?;
                PrivilegeObjectKind::AllTablesInSchema
            } else if self.eat_ident("sequences")? {
                self.expect_ident("in")?;
                self.expect_ident("schema")?;
                PrivilegeObjectKind::AllSequencesInSchema
            } else if self.eat_ident("functions")? {
                self.expect_ident("in")?;
                self.expect_ident("schema")?;
                PrivilegeObjectKind::AllFunctionsInSchema
            } else if self.eat_ident("procedures")? {
                self.expect_ident("in")?;
                self.expect_ident("schema")?;
                PrivilegeObjectKind::AllProceduresInSchema
            } else if self.eat_ident("routines")? {
                self.expect_ident("in")?;
                self.expect_ident("schema")?;
                PrivilegeObjectKind::AllRoutinesInSchema
            } else {
                return Err(self.syntax_error_here());
            }
        } else if self.eat_ident("function")? {
            return self.routine_privilege_target(RoutineTargetKind::Function);
        } else if self.eat_ident("procedure")? {
            return self.routine_privilege_target(RoutineTargetKind::Procedure);
        } else if self.eat_ident("routine")? {
            return self.routine_privilege_target(RoutineTargetKind::Either);
        } else if self.eat_ident("table")? {
            PrivilegeObjectKind::Table
        } else if self.eat_ident("sequence")? {
            PrivilegeObjectKind::Sequence
        } else if self.eat_ident("schema")? {
            PrivilegeObjectKind::Schema
        } else if self.eat_ident("tablespace")? {
            PrivilegeObjectKind::Tablespace
        } else if self.eat_ident("database")? {
            PrivilegeObjectKind::Database
        } else if self.eat_ident("type")? || self.eat_ident("domain")? {
            PrivilegeObjectKind::Type
        } else if self.eat_ident("language")? {
            PrivilegeObjectKind::ProceduralLanguage
        } else if self.eat_ident("foreign")? {
            if self.eat_ident("data")? {
                self.expect_ident("wrapper")?;
                PrivilegeObjectKind::ForeignDataWrapper
            } else {
                self.expect_ident("server")?;
                PrivilegeObjectKind::ForeignServer
            }
        } else {
            // TABLE is PostgreSQL's default object kind.
            PrivilegeObjectKind::Table
        };
        let mut names = ArenaList::new(self.arena);
        loop {
            let name = if matches!(
                kind,
                PrivilegeObjectKind::Schema
                    | PrivilegeObjectKind::Tablespace
                    | PrivilegeObjectKind::Database
                    | PrivilegeObjectKind::ForeignDataWrapper
                    | PrivilegeObjectKind::ForeignServer
                    | PrivilegeObjectKind::AllTablesInSchema
                    | PrivilegeObjectKind::AllSequencesInSchema
                    | PrivilegeObjectKind::AllFunctionsInSchema
                    | PrivilegeObjectKind::AllProceduresInSchema
                    | PrivilegeObjectKind::AllRoutinesInSchema
                    | PrivilegeObjectKind::ProceduralLanguage
            ) {
                QualName::bare(self.col_ident("schema name")?)
            } else {
                self.qual_name("object name")?
            };
            self.push(&mut names, name)?;
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(PrivilegeTarget::Objects {
            kind,
            names: names.as_slice(),
        })
    }

    fn configuration_parameter_name(&mut self) -> Result<&'a str, ParseError> {
        let first = self.col_ident("configuration parameter")?;
        if !self.eat_op(".")? {
            return Ok(first);
        }
        let mut bytes = ArenaList::new(self.arena);
        for byte in first.as_bytes() {
            bytes
                .push(*byte)
                .map_err(|_| self.arena_full("configuration name exceeds SQL arena"))?;
        }
        loop {
            bytes
                .push(b'.')
                .map_err(|_| self.arena_full("configuration name exceeds SQL arena"))?;
            let component = self.col_ident("configuration parameter component")?;
            for byte in component.as_bytes() {
                bytes
                    .push(*byte)
                    .map_err(|_| self.arena_full("configuration name exceeds SQL arena"))?;
            }
            if !self.eat_op(".")? {
                break;
            }
        }
        Ok(core::str::from_utf8(bytes.as_slice()).expect("decoded configuration name is UTF-8"))
    }

    fn routine_privilege_target(
        &mut self,
        kind: crate::sql::ast::RoutineTargetKind,
    ) -> Result<crate::sql::ast::PrivilegeTarget<'a>, ParseError> {
        use crate::sql::ast::{PrivilegeTarget, RoutineIdentity};
        let mut identities = ArenaList::new(self.arena);
        loop {
            let name = self.qual_name("routine name")?;
            self.expect_op("(")?;
            let mut argument_types = [crate::sql::ast::RoutineArgumentType::EMPTY;
                crate::storage::MAX_ROUTINE_ARGUMENTS];
            let mut argument_count = 0usize;
            if !self.eat_op(")")? {
                loop {
                    if argument_count == argument_types.len() {
                        return Err(self.limit("routine arguments", argument_types.len()));
                    }
                    let (argument_type, input) = self.routine_identity_argument()?;
                    if input {
                        argument_types[argument_count] = argument_type;
                        argument_count += 1;
                    }
                    if self.eat_op(")")? {
                        break;
                    }
                    self.expect_op(",")?;
                }
            }
            let argument_types = self.arena_slice(&argument_types[..argument_count])?;
            self.push(
                &mut identities,
                RoutineIdentity {
                    name,
                    argument_types,
                    signature_is_explicit: true,
                },
            )?;
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(PrivilegeTarget::Routines {
            kind,
            identities: identities.as_slice(),
        })
    }

    fn role_option_boolean(&mut self) -> Result<bool, ParseError> {
        if self.eat_ident("true")? {
            Ok(true)
        } else if self.eat_ident("false")? {
            Ok(false)
        } else {
            Err(self.syntax_error_here())
        }
    }

    fn revoke_statement(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("revoke")?;
        for (name, option) in [
            ("admin", crate::sql::ast::RoleMembershipOption::Admin),
            ("inherit", crate::sql::ast::RoleMembershipOption::Inherit),
        ] {
            if self.eat_ident(name)? {
                self.expect_ident("option")?;
                self.expect_ident("for")?;
                return self.revoke_role_after_keyword(Some(option));
            }
        }
        if self.peeked == Tok::Ident("set") && self.next_token_is_ident("option")? {
            self.advance()?;
            self.expect_ident("option")?;
            self.expect_ident("for")?;
            return self
                .revoke_role_after_keyword(Some(crate::sql::ast::RoleMembershipOption::Set));
        }
        let grant_option_only = if self.eat_ident("grant")? {
            self.expect_ident("option")?;
            self.expect_ident("for")?;
            true
        } else {
            false
        };
        if !self.privilege_start() {
            return self.revoke_role_after_keyword(None);
        }
        let privileges = self.privilege_list()?;
        self.expect_ident("on")?;
        let target = self.privilege_target()?;
        self.expect_ident("from")?;
        let grantees = self.role_name_list("grantee")?;
        let grantor = if self.eat_ident("granted")? {
            self.expect_ident("by")?;
            Some(self.any_ident("grantor role name")?)
        } else {
            None
        };
        let cascade = if self.eat_ident("cascade")? {
            true
        } else {
            let _ = self.eat_ident("restrict")?;
            false
        };
        if let crate::sql::ast::PrivilegeTarget::Parameters(names) = target {
            return Ok(Stmt::RevokeParameterPrivileges {
                grant_option_only,
                privileges: self.parameter_privileges(privileges)?,
                names,
                grantees,
                grantor,
                cascade,
            });
        }
        Ok(Stmt::RevokePrivileges {
            grant_option_only,
            privileges,
            target,
            grantees,
            grantor,
            cascade,
        })
    }

    fn revoke_role_after_keyword(
        &mut self,
        option: Option<crate::sql::ast::RoleMembershipOption>,
    ) -> Result<Stmt<'a>, ParseError> {
        let roles = self.role_name_list("role name")?;
        self.expect_ident("from")?;
        let members = self.role_name_list("member role name")?;
        let grantor = if self.eat_ident("granted")? {
            self.expect_ident("by")?;
            Some(self.any_ident("grantor role name")?)
        } else {
            None
        };
        let cascade = if self.eat_ident("cascade")? {
            true
        } else {
            let _ = self.eat_ident("restrict")?;
            false
        };
        Ok(Stmt::RevokeRole {
            syntax: crate::sql::ast::RoleMembershipSyntax::GrantRevoke,
            roles,
            members,
            option,
            grantor,
            cascade,
        })
    }

    /// `REINDEX [ ( options ) ] target [ CONCURRENTLY ] [ name ]`.
    fn reindex(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("reindex")?;
        let mut build = IndexBuildMode::Blocking;
        let mut saw_concurrently = false;
        let mut tablespace = None;
        let mut verbose = false;
        let mut saw_verbose = false;
        if self.eat_op("(")? {
            loop {
                let option = self.any_ident("REINDEX option")?;
                if option.eq_ignore_ascii_case("concurrently") {
                    if core::mem::replace(&mut saw_concurrently, true) {
                        return Err(self.err_here("REINDEX option specified more than once"));
                    }
                    let value = if self.peeked == Tok::Op(",") || self.peeked == Tok::Op(")") {
                        true
                    } else {
                        let _ = self.eat_op("=")?;
                        self.role_option_boolean()?
                    };
                    build = if value {
                        IndexBuildMode::Concurrent
                    } else {
                        IndexBuildMode::Blocking
                    };
                } else if option.eq_ignore_ascii_case("verbose") {
                    if core::mem::replace(&mut saw_verbose, true) {
                        return Err(self.err_here("REINDEX option specified more than once"));
                    }
                    verbose = if self.peeked == Tok::Op(",") || self.peeked == Tok::Op(")") {
                        true
                    } else {
                        let _ = self.eat_op("=")?;
                        self.role_option_boolean()?
                    };
                } else if option.eq_ignore_ascii_case("tablespace") {
                    if tablespace.is_some() {
                        return Err(self.err_here("REINDEX option specified more than once"));
                    }
                    let _ = self.eat_op("=")?;
                    tablespace = Some(self.any_ident("tablespace name")?);
                } else {
                    return Err(ParseError {
                        at: self.peek_at,
                        message: stack_format!(
                            PARSE_MESSAGE_BYTES,
                            "unrecognized REINDEX option \"{}\"",
                            option
                        ),
                        sqlstate: sqlstate::SYNTAX_ERROR,
                        cursor: None,
                        hint: None,
                        near: None,
                        detail: None,
                    });
                }
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
        }
        let target = if self.eat_ident("index")? {
            ReindexTarget::Index
        } else if self.eat_ident("table")? {
            ReindexTarget::Table
        } else if self.eat_ident("schema")? {
            ReindexTarget::Schema
        } else if self.eat_ident("database")? {
            ReindexTarget::Database
        } else if self.eat_ident("system")? {
            ReindexTarget::System
        } else {
            return Err(self.syntax_error_here());
        };
        if self.eat_ident("concurrently")? {
            if saw_concurrently {
                return Err(self.err_here("CONCURRENTLY specified more than once"));
            }
            build = IndexBuildMode::Concurrent;
        }
        let name = if matches!(target, ReindexTarget::Database | ReindexTarget::System)
            && matches!(self.peeked, Tok::Op(";") | Tok::Eof)
        {
            None
        } else if matches!(target, ReindexTarget::Database | ReindexTarget::System) {
            Some(QualName::bare(self.any_ident("database name")?))
        } else {
            Some(self.qual_name("reindex target")?)
        };
        Ok(Stmt::Reindex {
            target,
            name,
            options: ReindexOptions {
                build,
                tablespace,
                verbose,
            },
        })
    }

    /// `CLUSTER [VERBOSE] [table [USING index]]`. The no-target form is a
    /// closed `All` state; a table can carry an optional index and nothing
    /// else can reach execution.
    fn cluster(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("cluster")?;
        let verbose = self.eat_ident("verbose")?;
        let target = if matches!(self.peeked, Tok::Op(";") | Tok::Eof) {
            crate::sql::ast::ClusterTarget::All
        } else {
            let table = self.qual_name("table name")?;
            let index = if self.eat_ident("using")? {
                Some(self.qual_name("index name")?)
            } else {
                None
            };
            crate::sql::ast::ClusterTarget::Table { table, index }
        };
        Ok(Stmt::Cluster { target, verbose })
    }

    fn subscription_publication_change(
        &mut self,
    ) -> Result<(&'a [&'a str], SubscriptionPublicationRefresh), ParseError> {
        let mut publications = ArenaList::new(self.arena);
        loop {
            let publication = self.any_ident("publication name")?;
            self.push(&mut publications, publication)?;
            if !self.eat_op(",")? {
                break;
            }
        }
        let mut refresh = true;
        let mut copy_data = true;
        let mut saw_refresh = false;
        let mut saw_copy_data = false;
        if self.eat_ident("with")? {
            self.expect_op("(")?;
            loop {
                let option = self.any_ident("publication option")?;
                if option.eq_ignore_ascii_case("refresh") {
                    if core::mem::replace(&mut saw_refresh, true) {
                        return Err(self.err_here("duplicate publication option refresh"));
                    }
                    let _ = self.eat_op("=")?;
                    refresh = self.role_option_boolean()?;
                } else if option.eq_ignore_ascii_case("copy_data") {
                    if core::mem::replace(&mut saw_copy_data, true) {
                        return Err(self.err_here("duplicate publication option copy_data"));
                    }
                    let _ = self.eat_op("=")?;
                    copy_data = self.role_option_boolean()?;
                } else {
                    return Err(self.err_here("unrecognized publication option"));
                }
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
        }
        if !refresh && saw_copy_data {
            return Err(self.err_here("copy_data requires refresh = true"));
        }
        Ok((
            publications.as_slice(),
            if refresh {
                SubscriptionPublicationRefresh::Refresh { copy_data }
            } else {
                SubscriptionPublicationRefresh::NoRefresh
            },
        ))
    }

    fn subscription_settings_patch(&mut self) -> Result<SubscriptionSettingsPatch<'a>, ParseError> {
        let mut patch = SubscriptionSettingsPatch {
            slot: None,
            binary: None,
            streaming: None,
            synchronous_commit: None,
            two_phase: None,
            disable_on_error: None,
            password_required: None,
            run_as_owner: None,
            origin: None,
            failover: None,
        };
        self.expect_op("(")?;
        loop {
            let key = self.any_ident("subscription setting")?;
            if key.eq_ignore_ascii_case("slot_name") {
                if patch.slot.is_some() {
                    return Err(self.err_here("duplicate subscription setting slot_name"));
                }
                self.expect_op("=")?;
                patch.slot = Some(if self.eat_ident("none")? {
                    SubscriptionSlotSetting::Absent
                } else {
                    let value = match self.peeked {
                        Tok::Ident(value) | Tok::Str(value) => value,
                        _ => return Err(self.err_here("slot_name requires a name or NONE")),
                    };
                    self.advance()?;
                    SubscriptionSlotSetting::Named(value)
                });
            } else if key.eq_ignore_ascii_case("binary") {
                if patch.binary.is_some() {
                    return Err(self.err_here("duplicate subscription setting binary"));
                }
                patch.binary = Some(self.subscription_bool_option(key)?);
            } else if key.eq_ignore_ascii_case("streaming") {
                if patch.streaming.is_some() {
                    return Err(self.err_here("duplicate subscription setting streaming"));
                }
                patch.streaming = Some(self.subscription_streaming()?);
            } else if key.eq_ignore_ascii_case("synchronous_commit") {
                if patch.synchronous_commit.is_some() {
                    return Err(self.err_here("duplicate subscription setting synchronous_commit"));
                }
                patch.synchronous_commit = Some(self.subscription_synchronous_commit()?);
            } else if key.eq_ignore_ascii_case("two_phase") {
                if patch.two_phase.is_some() {
                    return Err(self.err_here("duplicate subscription setting two_phase"));
                }
                patch.two_phase = Some(self.subscription_bool_option(key)?);
            } else if key.eq_ignore_ascii_case("disable_on_error") {
                if patch.disable_on_error.is_some() {
                    return Err(self.err_here("duplicate subscription setting disable_on_error"));
                }
                patch.disable_on_error = Some(self.subscription_bool_option(key)?);
            } else if key.eq_ignore_ascii_case("password_required") {
                if patch.password_required.is_some() {
                    return Err(self.err_here("duplicate subscription setting password_required"));
                }
                patch.password_required = Some(self.subscription_bool_option(key)?);
            } else if key.eq_ignore_ascii_case("run_as_owner") {
                if patch.run_as_owner.is_some() {
                    return Err(self.err_here("duplicate subscription setting run_as_owner"));
                }
                patch.run_as_owner = Some(self.subscription_bool_option(key)?);
            } else if key.eq_ignore_ascii_case("origin") {
                if patch.origin.is_some() {
                    return Err(self.err_here("duplicate subscription setting origin"));
                }
                patch.origin = Some(self.subscription_origin()?);
            } else if key.eq_ignore_ascii_case("failover") {
                if patch.failover.is_some() {
                    return Err(self.err_here("duplicate subscription setting failover"));
                }
                patch.failover = Some(self.subscription_bool_option(key)?);
            } else {
                return Err(self.err_here("unrecognized subscription setting"));
            }
            if self.eat_op(")")? {
                break;
            }
            self.expect_op(",")?;
        }
        Ok(patch)
    }

    fn subscription_lsn(value: &str) -> Option<u64> {
        let (high, low) = value.split_once('/')?;
        if high.is_empty() || low.is_empty() || high.len() > 8 || low.len() > 8 {
            return None;
        }
        Some((u64::from_str_radix(high, 16).ok()? << 32) | u64::from_str_radix(low, 16).ok()?)
    }

    fn alter_table(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("alter")?;
        if self.eat_ident("large")? {
            self.expect_ident("object")?;
            let oid = self.large_object_id()?;
            self.expect_ident("owner")?;
            self.expect_ident("to")?;
            return Ok(Stmt::AlterLargeObjectOwner {
                oid,
                role: self.any_ident("role name")?,
            });
        }
        if self.eat_ident("system")? {
            if self.eat_ident("set")? {
                let name = self.configuration_parameter_name()?;
                if !self.eat_op("=")? {
                    self.expect_ident("to")?;
                }
                let value = match self.configuration_value_list(name, true)? {
                    RoutineConfigValue::Value(value) => Some(value),
                    RoutineConfigValue::Default => None,
                    RoutineConfigValue::Current => unreachable!("ALTER SYSTEM has no FROM CURRENT"),
                };
                return Ok(Stmt::AlterSystem {
                    name: Some(name),
                    value,
                });
            }
            self.expect_ident("reset")?;
            let name = if self.eat_ident("all")? {
                None
            } else {
                Some(self.configuration_parameter_name()?)
            };
            return Ok(Stmt::AlterSystem { name, value: None });
        }
        if self.eat_ident("default")? {
            self.expect_ident("privileges")?;
            return self.alter_default_privileges();
        }
        if self.eat_ident("rule")? {
            let name = self.col_ident("rule name")?;
            self.expect_ident("on")?;
            let table = self.qual_name("rule relation")?;
            self.expect_ident("rename")?;
            self.expect_ident("to")?;
            let new_name = self.col_ident("new rule name")?;
            return Ok(Stmt::AlterRule {
                name,
                table,
                new_name,
            });
        }
        if self.eat_ident("foreign")? {
            if self.eat_ident("data")? {
                self.expect_ident("wrapper")?;
                return self.alter_foreign_data_wrapper();
            }
            self.expect_ident("table")?;
            return self.alter_table_relation(true);
        }
        if self.eat_ident("server")? {
            return self.alter_foreign_server();
        }
        if self.eat_ident("user")? {
            if self.eat_ident("mapping")? {
                return self.alter_user_mapping();
            }
            return self.alter_role();
        }
        if self.eat_ident("role")? {
            return self.alter_role();
        }
        if self.eat_ident("group")? {
            return self.alter_group();
        }
        if self.eat_ident("collation")? {
            let name = self.qual_name("collation name")?;
            let action = if self.eat_ident("refresh")? {
                self.expect_ident("version")?;
                crate::sql::ast::AlterCollationAction::RefreshVersion
            } else if self.eat_ident("rename")? {
                self.expect_ident("to")?;
                crate::sql::ast::AlterCollationAction::Rename(self.any_ident("new collation name")?)
            } else if self.eat_ident("owner")? {
                self.expect_ident("to")?;
                crate::sql::ast::AlterCollationAction::Owner(self.any_ident("role name")?)
            } else if self.eat_ident("set")? {
                self.expect_ident("schema")?;
                crate::sql::ast::AlterCollationAction::SetSchema(self.any_ident("schema name")?)
            } else {
                return Err(self.syntax_error_here());
            };
            return Ok(Stmt::AlterCollation { name, action });
        }
        if self.eat_ident("text")? {
            self.expect_ident("search")?;
            return self.alter_text_search();
        }
        if self.eat_ident("conversion")? {
            let name = self.qual_name("conversion name")?;
            let action = if self.eat_ident("rename")? {
                self.expect_ident("to")?;
                crate::sql::ast::AlterConversionAction::Rename(
                    self.any_ident("new conversion name")?,
                )
            } else if self.eat_ident("owner")? {
                self.expect_ident("to")?;
                crate::sql::ast::AlterConversionAction::Owner(self.any_ident("role name")?)
            } else if self.eat_ident("set")? {
                self.expect_ident("schema")?;
                crate::sql::ast::AlterConversionAction::SetSchema(self.any_ident("schema name")?)
            } else {
                return Err(self.syntax_error_here());
            };
            return Ok(Stmt::AlterConversion { name, action });
        }
        if self.eat_ident("publication")? {
            return self.alter_publication();
        }
        if self.eat_ident("subscription")? {
            let name = self.any_ident("subscription name")?;
            let action = if self.eat_ident("owner")? {
                self.expect_ident("to")?;
                AlterSubscriptionAction::SetOwner(self.any_ident("role name")?)
            } else if self.eat_ident("rename")? {
                self.expect_ident("to")?;
                AlterSubscriptionAction::Rename(self.any_ident("new subscription name")?)
            } else if self.eat_ident("enable")? {
                AlterSubscriptionAction::Enable
            } else if self.eat_ident("disable")? {
                AlterSubscriptionAction::Disable
            } else if self.eat_ident("connection")? {
                AlterSubscriptionAction::SetConnection(
                    self.str_literal("subscription connection string")?,
                )
            } else if self.eat_ident("set")? {
                if self.peeked == Tok::Op("(") {
                    AlterSubscriptionAction::SetOptions(self.subscription_settings_patch()?)
                } else {
                    self.expect_ident("publication")?;
                    let (publications, refresh) = self.subscription_publication_change()?;
                    AlterSubscriptionAction::SetPublications {
                        publications,
                        refresh,
                    }
                }
            } else if self.eat_ident("add")? {
                self.expect_ident("publication")?;
                let (publications, refresh) = self.subscription_publication_change()?;
                AlterSubscriptionAction::AddPublications {
                    publications,
                    refresh,
                }
            } else if self.eat_ident("drop")? {
                self.expect_ident("publication")?;
                let (publications, refresh) = self.subscription_publication_change()?;
                AlterSubscriptionAction::DropPublications {
                    publications,
                    refresh,
                }
            } else if self.eat_ident("refresh")? {
                self.expect_ident("publication")?;
                let copy_data = if self.eat_ident("with")? {
                    self.expect_op("(")?;
                    self.expect_ident("copy_data")?;
                    let _ = self.eat_op("=")?;
                    let copy_data = self.role_option_boolean()?;
                    self.expect_op(")")?;
                    copy_data
                } else {
                    true
                };
                AlterSubscriptionAction::RefreshPublications { copy_data }
            } else if self.eat_ident("skip")? {
                self.expect_op("(")?;
                self.expect_ident("lsn")?;
                self.expect_op("=")?;
                let lsn = if self.eat_ident("none")? {
                    None
                } else {
                    let value = match self.peeked {
                        Tok::Ident(value) | Tok::Str(value) => value,
                        _ => return Err(self.err_here("subscription skip LSN is invalid")),
                    };
                    self.advance()?;
                    Some(
                        Self::subscription_lsn(value)
                            .ok_or_else(|| self.err_here("subscription skip LSN is invalid"))?,
                    )
                };
                self.expect_op(")")?;
                AlterSubscriptionAction::Skip { lsn }
            } else {
                return Err(self.err_here("invalid ALTER SUBSCRIPTION action"));
            };
            return Ok(Stmt::AlterSubscription { name, action });
        }
        if self.eat_ident("event")? {
            self.expect_ident("trigger")?;
            return self.alter_event_trigger();
        }
        if self.eat_ident("trigger")? {
            return self.alter_trigger();
        }
        if self.eat_ident("policy")? {
            return self.alter_policy();
        }
        if self.eat_ident("statistics")? {
            return self.alter_statistics();
        }
        if self.eat_ident("index")? {
            return self.alter_index();
        }
        if self.eat_ident("tablespace")? {
            return self.alter_tablespace();
        }
        if self.eat_ident("database")? {
            return self.alter_database();
        }
        if self.eat_ident("extension")? {
            return self.alter_extension();
        }
        if self.eat_ident("language")? {
            let name = self.col_ident("language name")?;
            let action = if self.eat_ident("rename")? {
                self.expect_ident("to")?;
                crate::sql::ast::AlterLanguageAction::Rename(self.col_ident("new language name")?)
            } else {
                self.expect_ident("owner")?;
                self.expect_ident("to")?;
                crate::sql::ast::AlterLanguageAction::SetOwner(self.any_ident("role name")?)
            };
            return Ok(Stmt::AlterLanguage { name, action });
        }
        if self.eat_ident("aggregate")? {
            return self.alter_aggregate();
        }
        if self.eat_ident("operator")? {
            if self.eat_ident("family")? {
                return self.alter_operator_family();
            }
            if self.eat_ident("class")? {
                return self.alter_operator_class();
            }
            return self.alter_operator();
        }
        if self.eat_ident("function")? {
            return self.alter_routine(crate::sql::ast::RoutineTargetKind::Function);
        }
        if self.eat_ident("procedure")? {
            return self.alter_routine(crate::sql::ast::RoutineTargetKind::Procedure);
        }
        if self.eat_ident("routine")? {
            return self.alter_routine(crate::sql::ast::RoutineTargetKind::Either);
        }
        if self.eat_ident("schema")? {
            let name = self.col_ident("schema name")?;
            let action = if self.eat_ident("rename")? {
                self.expect_ident("to")?;
                crate::sql::ast::AlterSchemaAction::RenameTo(self.col_ident("new schema name")?)
            } else {
                self.expect_ident("owner")?;
                self.expect_ident("to")?;
                crate::sql::ast::AlterSchemaAction::OwnerTo(self.any_ident("role name")?)
            };
            return Ok(Stmt::AlterSchema { name, action });
        }
        if self.eat_ident("materialized")? {
            self.expect_ident("view")?;
            return self.alter_materialized_view();
        }
        if self.eat_ident("view")? {
            return self.alter_view();
        }
        if self.eat_ident("sequence")? {
            return self.alter_sequence();
        }
        if self.eat_ident("domain")? {
            return self.alter_domain();
        }
        if self.eat_ident("type")? {
            return self.alter_type();
        }
        self.expect_ident("table")?;
        if self.eat_ident("all")? {
            self.expect_ident("in")?;
            self.expect_ident("tablespace")?;
            let source = self.any_ident("tablespace name")?;
            let owners = if self.eat_ident("owned")? {
                self.expect_ident("by")?;
                self.role_name_list("role name")?
            } else {
                &[]
            };
            self.expect_ident("set")?;
            self.expect_ident("tablespace")?;
            let target = self.any_ident("tablespace name")?;
            let nowait = self.eat_ident("nowait")?;
            return Ok(Stmt::AlterTablesTablespace {
                source,
                owners,
                target,
                nowait,
            });
        }
        self.alter_table_relation(false)
    }

    fn alter_table_statement(foreign: bool, statement: AlterTable<'a>) -> Stmt<'a> {
        if foreign {
            Stmt::AlterForeignTable(statement)
        } else {
            Stmt::AlterTable(statement)
        }
    }

    /// `ALTER VIEW` has a deliberately smaller grammar than `ALTER TABLE`.
    /// Keeping it separate prevents table-only mutation states from reaching a
    /// stored query merely because PostgreSQL permits a few historical
    /// `ALTER TABLE view` spellings.
    fn alter_view(&mut self) -> Result<Stmt<'a>, ParseError> {
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.qual_name("view name")?;
        if self.peeked == Tok::Ident("owner") {
            return self.alter_owner(AlterOwnerKind::View, name, if_exists);
        }
        let action = if self.eat_ident("rename")? {
            if self.eat_ident("to")? {
                crate::sql::ast::AlterViewAction::RenameTo(self.col_ident("new view name")?)
            } else {
                let _ = self.eat_ident("column")?;
                let from = self.col_ident("view column name")?;
                self.expect_ident("to")?;
                let to = self.col_ident("new view column name")?;
                crate::sql::ast::AlterViewAction::RenameColumn { from, to }
            }
        } else {
            // SET SCHEMA, options and column defaults parse as ALTER TABLE
            // subcommands, which may form a list.
            crate::sql::ast::AlterViewAction::TableActions(
                self.alter_view_table_actions(name, if_exists)?,
            )
        };
        Ok(Stmt::AlterView {
            name,
            if_exists,
            action,
        })
    }

    /// `ALTER TABLE` subcommands written after `ALTER VIEW` or `ALTER
    /// MATERIALIZED VIEW name`.
    fn alter_view_table_actions(
        &mut self,
        name: QualName<'a>,
        if_exists: bool,
    ) -> Result<&'a [crate::sql::ast::AlterAction<'a>], ParseError> {
        match self.alter_table_actions(false, name, if_exists, false)? {
            Stmt::AlterTable(alter) => Ok(alter.actions),
            _ => Err(self.err_here("expected ALTER TABLE subcommands")),
        }
    }

    fn alter_materialized_view(&mut self) -> Result<Stmt<'a>, ParseError> {
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.qual_name("materialized view name")?;
        if self.peeked == Tok::Ident("depends") || self.peeked == Tok::Ident("no") {
            if if_exists {
                return Err(self.err_here("IF EXISTS is not allowed with DEPENDS ON EXTENSION"));
            }
            let enabled = !self.eat_ident("no")?;
            self.expect_ident("depends")?;
            self.expect_ident("on")?;
            self.expect_ident("extension")?;
            return Ok(Stmt::AlterMaterializedViewExtensionDependency {
                name,
                extension: self.col_ident("extension name")?,
                enabled,
            });
        }
        if self.peeked == Tok::Ident("owner") {
            return self.alter_owner(AlterOwnerKind::MaterializedView, name, if_exists);
        }
        let action = if self.eat_ident("rename")? {
            if self.eat_ident("to")? {
                crate::sql::ast::AlterMaterializedViewAction::RenameTo(
                    self.col_ident("new materialized view name")?,
                )
            } else {
                let _ = self.eat_ident("column")?;
                let from = self.col_ident("materialized view column name")?;
                self.expect_ident("to")?;
                let to = self.col_ident("new materialized view column name")?;
                crate::sql::ast::AlterMaterializedViewAction::TableActions(
                    self.arena_slice(&[crate::sql::ast::AlterAction::RenameColumn { from, to }])?,
                )
            }
        } else {
            if self.peeked == Tok::Ident("set") {
                self.expect_ident("set")?;
                if self.eat_ident("schema")? {
                    return Ok(Stmt::AlterMaterializedView {
                        name,
                        if_exists,
                        action: crate::sql::ast::AlterMaterializedViewAction::SetSchema(
                            self.col_ident("schema name")?,
                        ),
                    });
                }
                let action = self.alter_table_set_action()?;
                crate::sql::ast::AlterMaterializedViewAction::TableActions(
                    self.arena_slice(&[action])?,
                )
            } else {
                crate::sql::ast::AlterMaterializedViewAction::TableActions(
                    self.alter_view_table_actions(name, if_exists)?,
                )
            }
        };
        Ok(Stmt::AlterMaterializedView {
            name,
            if_exists,
            action,
        })
    }

    fn view_options(&mut self) -> Result<&'a [crate::sql::ast::ViewOption], ParseError> {
        let (storage, options) = self.relation_options()?;
        if storage.fillfactor.is_some() {
            return Err(self.unrecognized_parameter("fillfactor"));
        }
        Ok(options)
    }

    fn unrecognized_parameter(&self, option: &str) -> ParseError {
        ParseError {
            at: self.peek_at,
            message: stack_format!(PARSE_MESSAGE_BYTES, "unrecognized parameter \"{}\"", option),
            sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
            cursor: None,
            hint: None,
            near: None,
            detail: None,
        }
    }

    /// A `SET (...)` list of table storage parameters and view options,
    /// through `)`. Which of them a relation has is checked when the
    /// statement runs, as PostgreSQL validates them against its kind.
    fn relation_options(
        &mut self,
    ) -> Result<
        (
            crate::sql::ast::RelationStorageOptions,
            &'a [crate::sql::ast::ViewOption],
        ),
        ParseError,
    > {
        use crate::sql::ast::{
            RelationStorageOptions, ViewCheckOption, ViewOption, ViewOptionValue,
        };
        let mut storage = RelationStorageOptions::DEFAULT;
        let mut options = [ViewOption {
            value: ViewOptionValue::SecurityInvoker(false),
            spelling: crate::storage::ReloptionSpelling::EMPTY,
        }; 3];
        let mut count = 0usize;
        let mut seen = [false; 4];
        loop {
            let option = self.any_ident("relation option name")?;
            let index = if option.eq_ignore_ascii_case("security_invoker") {
                let (enabled, spelling) = self.reloption_bool(option)?;
                options[count] = ViewOption {
                    value: ViewOptionValue::SecurityInvoker(enabled),
                    spelling,
                };
                0
            } else if option.eq_ignore_ascii_case("security_barrier") {
                let (enabled, spelling) = self.reloption_bool(option)?;
                options[count] = ViewOption {
                    value: ViewOptionValue::SecurityBarrier(enabled),
                    spelling,
                };
                1
            } else if option.eq_ignore_ascii_case("check_option") {
                self.expect_op("=")?;
                let value = match self.peeked {
                    Tok::Ident(value) | Tok::QuotedIdent(value) | Tok::Str(value) => value,
                    _ => return Err(self.syntax_error_here()),
                };
                let spelling = crate::storage::ReloptionSpelling::new(value);
                let checked = if value.eq_ignore_ascii_case("local") {
                    ViewCheckOption::Local
                } else if value.eq_ignore_ascii_case("cascaded") {
                    ViewCheckOption::Cascaded
                } else {
                    return Err(ParseError {
                        at: self.peek_at,
                        message: stack_format!(
                            PARSE_MESSAGE_BYTES,
                            "invalid value for enum option \"check_option\": {}",
                            value
                        ),
                        sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                        cursor: None,
                        hint: None,
                        near: None,
                        detail: Some("Valid values are \"local\" and \"cascaded\"."),
                    });
                };
                self.advance()?;
                options[count] = ViewOption {
                    value: ViewOptionValue::CheckOption(checked),
                    spelling: spelling.expect("check option values are spellings"),
                };
                2
            } else if option.eq_ignore_ascii_case("fillfactor") {
                self.expect_op("=")?;
                storage.fillfactor = Some(self.integer_reloption("fillfactor", 10, 100)? as u8);
                3
            } else {
                return Err(self.unrecognized_parameter(option));
            };
            if core::mem::replace(&mut seen[index], true) {
                return Err(Self::duplicate_reloption(self.peek_at, option));
            }
            if index < 3 {
                count += 1;
            }
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        Ok((storage, self.arena_slice(&options[..count])?))
    }

    /// A boolean reloption and its spelling: absent means `true`; otherwise
    /// any spelling PostgreSQL's `parse_bool` accepts (`on`, `yes`, `1`,
    /// `t`, `of`, ...).
    fn reloption_bool(
        &mut self,
        option: &str,
    ) -> Result<(bool, crate::storage::ReloptionSpelling), ParseError> {
        if !self.eat_op("=")? {
            return Ok((
                true,
                crate::storage::ReloptionSpelling::new("true").expect("a spelling"),
            ));
        }
        let at = self.peek_at;
        let value = match self.peeked {
            Tok::Ident(value) | Tok::QuotedIdent(value) | Tok::Str(value) | Tok::Num(value) => {
                value
            }
            _ => return Err(self.err_here("syntax error")),
        };
        self.advance()?;
        // Unlike boolean input, reloption parsing does not trim whitespace.
        (value.trim() == value)
            .then(|| crate::sql::eval::parse_bool(value).ok())
            .flatten()
            .zip(crate::storage::ReloptionSpelling::new(value))
            .ok_or_else(|| {
                Self::invalid_reloption(
                    at,
                    stack_format!(
                        PARSE_MESSAGE_BYTES,
                        "invalid value for boolean option \"{}\": {}",
                        option,
                        value
                    ),
                )
            })
    }

    /// PostgreSQL validates reloptions after parsing, as `22023`.
    fn invalid_reloption(
        at: usize,
        message: crate::util::StackStr<PARSE_MESSAGE_BYTES>,
    ) -> ParseError {
        ParseError {
            at,
            message,
            sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
            cursor: None,
            hint: None,
            near: None,
            detail: None,
        }
    }

    fn duplicate_reloption(at: usize, option: &str) -> ParseError {
        Self::invalid_reloption(
            at,
            stack_format!(
                PARSE_MESSAGE_BYTES,
                "parameter \"{}\" specified more than once",
                option
            ),
        )
    }

    /// One `ALTER TABLE ... SET` subcommand, after `SET`.
    fn alter_table_set_action(&mut self) -> Result<AlterAction<'a>, ParseError> {
        Ok(if self.eat_ident("schema")? {
            AlterAction::SetSchema(self.col_ident("schema name")?)
        } else if self.eat_ident("logged")? {
            AlterAction::SetPersistence(crate::sql::ast::RelationPersistence::Permanent)
        } else if self.eat_ident("unlogged")? {
            AlterAction::SetPersistence(crate::sql::ast::RelationPersistence::Unlogged)
        } else if self.eat_ident("tablespace")? {
            AlterAction::SetTablespace(self.col_ident("tablespace name")?)
        } else if self.eat_ident("without")? {
            if self.eat_ident("cluster")? {
                AlterAction::ClearCluster
            } else {
                self.expect_ident("oids")?;
                AlterAction::SetWithoutOids
            }
        } else if self.eat_op("(")? {
            match self.relation_options()? {
                (storage, []) => AlterAction::SetStorageOptions(storage),
                (storage, options) => AlterAction::SetViewOptions { options, storage },
            }
        } else {
            self.expect_ident("access")?;
            self.expect_ident("method")?;
            if self.eat_ident("default")? {
                AlterAction::ResetAccessMethod
            } else {
                let method = self.any_ident("table access method")?;
                AlterAction::SetAccessMethod(if method == "heap" {
                    crate::sql::ast::TableAccessMethod::Heap
                } else {
                    crate::sql::ast::TableAccessMethod::Named(method)
                })
            }
        })
    }

    fn alter_table_relation(&mut self, foreign: bool) -> Result<Stmt<'a>, ParseError> {
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let only = self.eat_ident("only")?;
        let table = self.qual_name("table name")?;
        if self.peeked == Tok::Ident("owner") {
            return self.alter_owner(
                if foreign {
                    AlterOwnerKind::ForeignTable
                } else {
                    AlterOwnerKind::Table
                },
                table,
                if_exists,
            );
        }
        self.alter_table_actions(foreign, table, if_exists, only)
    }

    /// The subcommands after `ALTER TABLE name`, which `ALTER VIEW` and
    /// `ALTER MATERIALIZED VIEW` also accept; their execution rejects the
    /// ones the relation's kind does not support.
    fn alter_table_actions(
        &mut self,
        foreign: bool,
        table: QualName<'a>,
        if_exists: bool,
        only: bool,
    ) -> Result<Stmt<'a>, ParseError> {
        // RENAME …, SET SCHEMA and partition attachment are standalone forms:
        // PostgreSQL does not combine them with a comma-separated subcommand
        // list, so they parse to a single-element list on their own.
        let mut buffer = ArenaList::new(self.arena);
        if self.eat_ident("set")? {
            if self.eat_ident("schema")? {
                let action = AlterAction::SetSchema(self.col_ident("schema name")?);
                return Ok(Self::alter_table_statement(
                    foreign,
                    AlterTable {
                        table,
                        if_exists,
                        only,
                        actions: self.arena_slice(&[action])?,
                    },
                ));
            }
            let action = self.alter_table_set_action()?;
            self.push(&mut buffer, action)?;
            if !self.eat_op(",")? {
                return Ok(Self::alter_table_statement(
                    foreign,
                    AlterTable {
                        table,
                        if_exists,
                        only,
                        actions: buffer.as_slice(),
                    },
                ));
            }
        } else if matches!(self.peeked, Tok::Ident("attach" | "detach")) {
            let mut column_constraints = ArenaList::new(self.arena);
            let action = self.alter_table_cmd(foreign, table.name, &mut column_constraints)?;
            return Ok(Self::alter_table_statement(
                foreign,
                AlterTable {
                    table,
                    if_exists,
                    only,
                    actions: self.arena_slice(&[action])?,
                },
            ));
        }
        if self.eat_ident("rename")? {
            let action = if self.eat_ident("to")? {
                AlterAction::RenameTable(self.col_ident("new table name")?)
            } else if self.eat_ident("constraint")? {
                let from = self.col_ident("constraint name")?;
                self.expect_ident("to")?;
                let to = self.col_ident("new constraint name")?;
                AlterAction::RenameConstraint { from, to }
            } else {
                let _ = self.eat_ident("column")?;
                let from = self.col_ident("column name")?;
                self.expect_ident("to")?;
                let to = self.col_ident("new column name")?;
                AlterAction::RenameColumn { from, to }
            };
            return Ok(Self::alter_table_statement(
                foreign,
                AlterTable {
                    table,
                    if_exists,
                    only,
                    actions: self.arena_slice(&[action])?,
                },
            ));
        }
        // Otherwise a comma-separated list of subcommands.
        loop {
            if matches!(self.peeked, Tok::Ident("attach" | "detach" | "rename")) {
                return Err(self.syntax_error_here());
            }
            let mut column_constraints = ArenaList::new(self.arena);
            let action = self.alter_table_cmd(foreign, table.name, &mut column_constraints)?;
            self.push(&mut buffer, action)?;
            for constraint in column_constraints.as_slice() {
                self.push(&mut buffer, AlterAction::AddConstraint(*constraint))?;
            }
            if !self.eat_op(",")? {
                break;
            }
        }
        // PostgreSQL executes the subcommands in a fixed pass order, not the
        // written order, so a constraint can reference a column added later in
        // the same statement. A stable insertion sort by pass keeps the written
        // order within a pass and allocates nothing.
        let actions = buffer
            .as_mut_slice()
            .map_err(|_| self.arena_full("ALTER TABLE actions exceed statement arena"))?;
        for i in 1..actions.len() {
            let mut j = i;
            while j > 0 && alter_pass(&actions[j - 1]) > alter_pass(&actions[j]) {
                actions.swap(j - 1, j);
                j -= 1;
            }
        }
        Ok(Self::alter_table_statement(
            foreign,
            AlterTable {
                table,
                if_exists,
                only,
                actions: buffer.as_slice(),
            },
        ))
    }

    fn text_search_token_types(&mut self) -> Result<&'a [&'a str], ParseError> {
        let mut values = [""; 32];
        let mut count = 0usize;
        loop {
            if count == values.len() {
                return Err(self.err_here("too many text search token types"));
            }
            values[count] = self.col_ident("text search token type")?;
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.arena_slice(&values[..count])
    }

    fn text_search_dictionaries(&mut self) -> Result<&'a [QualName<'a>], ParseError> {
        let mut values = [QualName {
            schema: None,
            name: "",
        }; 32];
        let mut count = 0usize;
        loop {
            if count == values.len() {
                return Err(self.err_here("too many text search dictionaries"));
            }
            values[count] = self.any_qual_name("text search dictionary")?;
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.arena_slice(&values[..count])
    }

    fn alter_text_search(&mut self) -> Result<Stmt<'a>, ParseError> {
        let kind = if self.eat_ident("parser")? {
            TextSearchObjectKind::Parser
        } else if self.eat_ident("template")? {
            TextSearchObjectKind::Template
        } else if self.eat_ident("dictionary")? {
            TextSearchObjectKind::Dictionary
        } else if self.eat_ident("configuration")? {
            TextSearchObjectKind::Configuration
        } else {
            return Err(self.syntax_error_here());
        };
        let name = self.any_qual_name(kind.noun())?;
        let action = if self.eat_ident("rename")? {
            self.expect_ident("to")?;
            AlterTextSearchAction::Rename(self.any_ident("new text search object name")?)
        } else if self.eat_ident("owner")? {
            if !matches!(
                kind,
                TextSearchObjectKind::Dictionary | TextSearchObjectKind::Configuration
            ) {
                return Err(self.err_here("this text search object has no owner"));
            }
            self.expect_ident("to")?;
            AlterTextSearchAction::Owner(self.any_ident("role name")?)
        } else if self.eat_ident("set")? {
            self.expect_ident("schema")?;
            AlterTextSearchAction::SetSchema(self.any_ident("schema name")?)
        } else if kind == TextSearchObjectKind::Dictionary && self.eat_op("(")? {
            let options = self.text_search_options()?;
            self.expect_op(")")?;
            AlterTextSearchAction::DictionaryOptions(options)
        } else if kind == TextSearchObjectKind::Configuration {
            let mapping = if self.eat_ident("add")? {
                self.expect_ident("mapping")?;
                self.expect_ident("for")?;
                let token_types = self.text_search_token_types()?;
                self.expect_ident("with")?;
                TextSearchMappingAction::Set {
                    replace_existing: false,
                    token_types,
                    dictionaries: self.text_search_dictionaries()?,
                }
            } else if self.eat_ident("alter")? {
                self.expect_ident("mapping")?;
                let token_types = if self.eat_ident("for")? {
                    Some(self.text_search_token_types()?)
                } else {
                    None
                };
                if self.eat_ident("replace")? {
                    let old = self.any_qual_name("old text search dictionary")?;
                    self.expect_ident("with")?;
                    TextSearchMappingAction::Replace {
                        token_types,
                        old,
                        new: self.any_qual_name("new text search dictionary")?,
                    }
                } else {
                    let Some(token_types) = token_types else {
                        return Err(self.syntax_error_here());
                    };
                    self.expect_ident("with")?;
                    TextSearchMappingAction::Set {
                        replace_existing: true,
                        token_types,
                        dictionaries: self.text_search_dictionaries()?,
                    }
                }
            } else if self.eat_ident("drop")? {
                self.expect_ident("mapping")?;
                let if_exists = if self.eat_ident("if")? {
                    self.expect_ident("exists")?;
                    true
                } else {
                    false
                };
                self.expect_ident("for")?;
                TextSearchMappingAction::Drop {
                    if_exists,
                    token_types: self.text_search_token_types()?,
                }
            } else {
                return Err(self.syntax_error_here());
            };
            AlterTextSearchAction::ConfigurationMapping(mapping)
        } else {
            return Err(self.syntax_error_here());
        };
        Ok(Stmt::AlterTextSearch { kind, name, action })
    }

    fn alter_default_privileges(&mut self) -> Result<Stmt<'a>, ParseError> {
        use crate::sql::ast::{DefaultPrivilegeAction, DefaultPrivilegeObjectKind};

        let roles = if self.eat_ident("for")? {
            if !self.eat_ident("role")? {
                self.expect_ident("user")?;
            }
            self.role_name_list("role name")?
        } else {
            &[]
        };
        let schemas = if self.eat_ident("in")? {
            self.expect_ident("schema")?;
            self.role_name_list("schema name")?
        } else {
            &[]
        };

        let grant = if self.eat_ident("grant")? {
            true
        } else {
            self.expect_ident("revoke")?;
            false
        };
        let grant_option_only = if !grant && self.eat_ident("grant")? {
            self.expect_ident("option")?;
            self.expect_ident("for")?;
            true
        } else {
            false
        };
        let privileges = self.default_privilege_list()?;
        self.expect_ident("on")?;
        let kind = if self.eat_ident("tables")? {
            DefaultPrivilegeObjectKind::Tables
        } else if self.eat_ident("sequences")? {
            DefaultPrivilegeObjectKind::Sequences
        } else if self.eat_ident("functions")? || self.eat_ident("routines")? {
            DefaultPrivilegeObjectKind::Functions
        } else if self.eat_ident("types")? {
            DefaultPrivilegeObjectKind::Types
        } else if self.eat_ident("schemas")? {
            DefaultPrivilegeObjectKind::Schemas
        } else {
            return Err(self.syntax_error_here());
        };
        if !schemas.is_empty() && kind == DefaultPrivilegeObjectKind::Schemas {
            return Err(ParseError {
                at: self.peek_at,
                message: crate::util::StackStr::from_str(
                    "cannot use IN SCHEMA clause when using GRANT/REVOKE ON SCHEMAS",
                ),
                sqlstate: sqlstate::INVALID_GRANT_OPERATION,
                cursor: None,
                hint: None,
                near: None,
                detail: None,
            });
        }
        if grant {
            self.expect_ident("to")?;
            let grantees = self.role_name_list("grantee")?;
            let grant_option = if self.eat_ident("with")? {
                self.expect_ident("grant")?;
                self.expect_ident("option")?;
                true
            } else {
                false
            };
            Ok(Stmt::AlterDefaultPrivileges {
                roles,
                schemas,
                action: DefaultPrivilegeAction::Grant {
                    privileges,
                    kind,
                    grantees,
                    grant_option,
                },
            })
        } else {
            self.expect_ident("from")?;
            let grantees = self.role_name_list("grantee")?;
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            Ok(Stmt::AlterDefaultPrivileges {
                roles,
                schemas,
                action: DefaultPrivilegeAction::Revoke {
                    grant_option_only,
                    privileges,
                    kind,
                    grantees,
                    cascade,
                },
            })
        }
    }

    pub(super) fn alter_owner(
        &mut self,
        kind: crate::sql::ast::AlterOwnerKind,
        name: QualName<'a>,
        if_exists: bool,
    ) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("owner")?;
        self.expect_ident("to")?;
        let role = self.any_ident("role name")?;
        Ok(Stmt::AlterOwner {
            kind,
            name,
            role,
            if_exists,
        })
    }

    /// Parses PostgreSQL's closed per-column storage policy. `DEFAULT` is
    /// accepted only where the CREATE TABLE grammar allows it; callers retain
    /// absence separately so execution can select the type-derived policy.
    fn column_storage(&mut self, allow_default: bool) -> Result<Option<ColumnStorage>, ParseError> {
        if allow_default && self.eat_ident("default")? {
            return Ok(None);
        }
        let storage = match self.any_ident("column storage")? {
            value if value.eq_ignore_ascii_case("plain") => ColumnStorage::Plain,
            value if value.eq_ignore_ascii_case("external") => ColumnStorage::External,
            value if value.eq_ignore_ascii_case("extended") => ColumnStorage::Extended,
            value if value.eq_ignore_ascii_case("main") => ColumnStorage::Main,
            _ => return Err(self.err_here("unrecognized column storage")),
        };
        Ok(Some(storage))
    }

    /// Parses the exact compression alphabet PostgreSQL exposes in table DDL.
    fn column_compression(&mut self) -> Result<ColumnCompression, ParseError> {
        if self.eat_ident("default")? {
            return Ok(ColumnCompression::Default);
        }
        match self.any_ident("column compression")? {
            value if value.eq_ignore_ascii_case("pglz") => Ok(ColumnCompression::Pglz),
            value if value.eq_ignore_ascii_case("lz4") => Ok(ColumnCompression::Lz4),
            _ => Err(self.err_here("unrecognized column compression")),
        }
    }

    /// The relation-level subcommands of an `ALTER TABLE` list: trigger,
    /// rule and row-security switches, `CLUSTER ON`, `OF`, `NOT OF`, and
    /// `NO INHERIT`.
    fn alter_table_relation_cmd(&mut self) -> Result<Option<AlterAction<'a>>, ParseError> {
        use crate::sql::ast::{TriggerEnableMode, TriggerEnableTarget};
        if self.eat_ident("cluster")? {
            self.expect_ident("on")?;
            // PostgreSQL's grammar takes an unqualified name, looked up in the
            // table's schema.
            return Ok(Some(AlterAction::SetCluster(QualName {
                schema: None,
                name: self.col_ident("index name")?,
            })));
        }
        if self.eat_ident("of")? {
            let type_name = self.qual_name("composite type name")?;
            return Ok(Some(AlterAction::SetTypeMembership(Some(type_name))));
        }
        if self.eat_ident("not")? {
            self.expect_ident("of")?;
            return Ok(Some(AlterAction::SetTypeMembership(None)));
        }
        let row_level_security = |parser: &mut Self| -> Result<(), ParseError> {
            parser.expect_ident("row")?;
            parser.expect_ident("level")?;
            parser.expect_ident("security")
        };
        if self.eat_ident("force")? {
            row_level_security(self)?;
            return Ok(Some(AlterAction::SetRowLevelSecurity(
                RowLevelSecurityAlteration::Force,
            )));
        }
        if self.eat_ident("no")? {
            if self.eat_ident("inherit")? {
                return Ok(Some(AlterAction::SetInheritance {
                    parent: self.qual_name("parent relation name")?,
                    inherit: false,
                }));
            }
            self.expect_ident("force")?;
            row_level_security(self)?;
            return Ok(Some(AlterAction::SetRowLevelSecurity(
                RowLevelSecurityAlteration::NoForce,
            )));
        }
        let enabled = if self.eat_ident("enable")? {
            if self.peeked == Tok::Ident("row") {
                row_level_security(self)?;
                return Ok(Some(AlterAction::SetRowLevelSecurity(
                    RowLevelSecurityAlteration::Enable,
                )));
            }
            if self.eat_ident("replica")? {
                TriggerEnableMode::Replica
            } else if self.eat_ident("always")? {
                TriggerEnableMode::Always
            } else {
                TriggerEnableMode::Origin
            }
        } else if self.eat_ident("disable")? {
            if self.peeked == Tok::Ident("row") {
                row_level_security(self)?;
                return Ok(Some(AlterAction::SetRowLevelSecurity(
                    RowLevelSecurityAlteration::Disable,
                )));
            }
            TriggerEnableMode::Disabled
        } else {
            return Ok(None);
        };
        if self.eat_ident("rule")? {
            if matches!(self.peeked, Tok::Ident("all" | "user")) {
                return Err(self.syntax_error_here());
            }
            return Ok(Some(AlterAction::SetRuleEnabled {
                name: self.col_ident("rewrite rule name")?,
                enabled,
            }));
        }
        self.expect_ident("trigger")?;
        let target = match (enabled, self.peeked) {
            (
                TriggerEnableMode::Origin | TriggerEnableMode::Disabled,
                Tok::Ident(word @ ("all" | "user")),
            ) => {
                self.advance()?;
                if word == "all" {
                    TriggerEnableTarget::All
                } else {
                    TriggerEnableTarget::User
                }
            }
            _ => TriggerEnableTarget::Name(self.col_ident("trigger name")?),
        };
        Ok(Some(AlterAction::SetTriggerEnabled { target, enabled }))
    }

    /// One ADD / DROP / ALTER subcommand of an ALTER TABLE (the comma-listable
    /// forms; RENAME and SET SCHEMA are handled by the caller).
    /// One ALTER TABLE subcommand. The constraints written on an added
    /// column are appended to `column_constraints`; PostgreSQL adds them as
    /// separate constraint subcommands once the column exists.
    fn alter_table_cmd(
        &mut self,
        foreign: bool,
        table: &str,
        column_constraints: &mut ArenaList<'a, TableConstraint<'a>>,
    ) -> Result<AlterAction<'a>, ParseError> {
        if let Some(action) = self.alter_table_relation_cmd()? {
            return Ok(action);
        }
        if self.eat_ident("options")? {
            Ok(AlterAction::SetForeignOptions(
                self.alter_foreign_options()?,
            ))
        } else if self.eat_ident("attach")? {
            self.expect_ident("partition")?;
            let child = self.qual_name("partition name")?;
            let bound = self.partition_bound()?;
            Ok(AlterAction::AttachPartition { child, bound })
        } else if self.eat_ident("detach")? {
            self.expect_ident("partition")?;
            let child = self.qual_name("partition name")?;
            let mode = if self.eat_ident("concurrently")? {
                crate::sql::ast::PartitionDetachMode::Concurrent
            } else if self.eat_ident("finalize")? {
                crate::sql::ast::PartitionDetachMode::Finalize
            } else {
                crate::sql::ast::PartitionDetachMode::Immediate
            };
            Ok(AlterAction::DetachPartition { child, mode })
        } else if self.eat_ident("inherit")? {
            Ok(AlterAction::SetInheritance {
                parent: self.qual_name("parent relation name")?,
                inherit: true,
            })
        } else if self.eat_ident("replica")? {
            self.expect_ident("identity")?;
            let target = if self.eat_ident("default")? {
                crate::sql::ast::ReplicaIdentityTarget::Default
            } else if self.eat_ident("full")? {
                crate::sql::ast::ReplicaIdentityTarget::Full
            } else if self.eat_ident("nothing")? {
                crate::sql::ast::ReplicaIdentityTarget::Nothing
            } else {
                self.expect_ident("using")?;
                self.expect_ident("index")?;
                crate::sql::ast::ReplicaIdentityTarget::Index(self.qual_name("index name")?)
            };
            Ok(AlterAction::SetReplicaIdentity(target))
        } else if self.eat_ident("set")? {
            // SET SCHEMA stands alone.
            if self.peeked == Tok::Ident("schema") {
                return Err(self.syntax_error_here());
            }
            self.alter_table_set_action()
        } else if self.eat_ident("reset")? {
            self.expect_op("(")?;
            let names = self.relation_storage_option_names()?;
            self.expect_op(")")?;
            Ok(AlterAction::ResetStorageOptions(names))
        } else if self.eat_ident("add")? {
            // ADD [CONSTRAINT name] <table constraint> vs ADD [COLUMN] <def>.
            let constraint_at = SourcePosition::new(Some(self.peek_at));
            if self.eat_ident("constraint")? {
                let cname = self.col_ident("constraint name")?;
                return self.alter_table_add_constraint(Some(cname), constraint_at);
            }
            if matches!(
                self.peeked,
                Tok::Ident("primary")
                    | Tok::Ident("unique")
                    | Tok::Ident("check")
                    | Tok::Ident("foreign")
                    | Tok::Ident("exclude")
            ) {
                return self.alter_table_add_constraint(None, constraint_at);
            }
            let _ = self.eat_ident("column")?;
            let if_not_exists = if self.eat_ident("if")? {
                self.expect_ident("not")?;
                self.expect_ident("exists")?;
                true
            } else {
                false
            };
            let name = self.col_ident("column name")?;
            let (type_name, type_mod) = self.type_name_mod()?;
            let foreign_options = if foreign && self.eat_ident("options")? {
                self.foreign_options()?
            } else {
                &[]
            };
            let storage = if self.eat_ident("storage")? {
                if foreign {
                    return Err(self.err_here("foreign columns do not support STORAGE"));
                }
                self.column_storage(true)?
            } else {
                None
            };
            let compression = if self.eat_ident("compression")? {
                if foreign {
                    return Err(self.err_here("foreign columns do not support COMPRESSION"));
                }
                self.column_compression()?
            } else {
                ColumnCompression::Default
            };
            let collation = if self.eat_ident("collate")? {
                self.collation_name()?
            } else {
                crate::sql::ast::ParsedCollation::DEFAULT
            };
            let clauses = self.column_clauses(name, table, false, column_constraints)?;
            Ok(AlterAction::AddColumn {
                definition: ColumnDef {
                    source_position: constraint_at,
                    name,
                    type_name,
                    type_mod,
                    collation,
                    foreign_options,
                    storage,
                    compression,
                    not_null: clauses.not_null,
                    not_null_name: clauses.not_null_name,
                    not_null_inheritable: clauses.not_null_inheritable,
                    unique: clauses.unique,
                    primary: clauses.primary,
                    default: clauses.default,
                    default_text: clauses.default_text,
                    generated_text: clauses.generated_text,
                    identity: clauses.identity,
                },
                if_not_exists,
            })
        } else if self.eat_ident("drop")? {
            if self.eat_ident("constraint")? {
                let if_exists = self.eat_ident("if")? && {
                    self.expect_ident("exists")?;
                    true
                };
                let name = self.col_ident("constraint name")?;
                let cascade = if self.eat_ident("cascade")? {
                    true
                } else {
                    let _ = self.eat_ident("restrict")?;
                    false
                };
                Ok(AlterAction::DropConstraint {
                    name,
                    if_exists,
                    cascade,
                })
            } else {
                let _ = self.eat_ident("column")?;
                let if_exists = self.eat_ident("if")? && {
                    self.expect_ident("exists")?;
                    true
                };
                let name = self.col_ident("column name")?;
                let cascade = if self.eat_ident("cascade")? {
                    true
                } else {
                    let _ = self.eat_ident("restrict")?;
                    false
                };
                Ok(AlterAction::DropColumn {
                    name,
                    if_exists,
                    cascade,
                })
            }
        } else if self.eat_ident("validate")? {
            self.expect_ident("constraint")?;
            Ok(AlterAction::ValidateConstraint(
                self.col_ident("constraint name")?,
            ))
        } else if self.eat_ident("alter")? {
            if self.eat_ident("constraint")? {
                let name = self.col_ident("constraint name")?;
                if self.eat_ident("inherit")? {
                    return Ok(AlterAction::AlterConstraint {
                        name,
                        alteration: ConstraintAlteration::NotNullInheritance { inherit: true },
                    });
                }
                let mut alteration = crate::sql::ast::ForeignKeyConstraintAlteration {
                    deferrable: None,
                    initially: None,
                    enforced: None,
                };
                if self.eat_ident("no")? {
                    if self.eat_ident("inherit")? {
                        return Ok(AlterAction::AlterConstraint {
                            name,
                            alteration: ConstraintAlteration::NotNullInheritance { inherit: false },
                        });
                    }
                    self.expect_ident("deferrable")?;
                    alteration.deferrable = Some(false);
                }
                loop {
                    if self.eat_ident("deferrable")? {
                        if alteration.deferrable.replace(true).is_some() {
                            return Err(self.err_here("duplicate DEFERRABLE clause"));
                        }
                    } else if self.eat_ident("not")? {
                        self.expect_ident("enforced")?;
                        if alteration.enforced.replace(false).is_some() {
                            return Err(self.err_here("duplicate NOT ENFORCED clause"));
                        }
                    } else if self.eat_ident("initially")? {
                        let mode = if self.eat_ident("deferred")? {
                            ConstraintMode::Deferred
                        } else {
                            self.expect_ident("immediate")?;
                            ConstraintMode::Immediate
                        };
                        if alteration.initially.replace(mode).is_some() {
                            return Err(self.err_here("duplicate INITIALLY clause"));
                        }
                    } else if self.eat_ident("enforced")? {
                        if alteration.enforced.replace(true).is_some() {
                            return Err(self.err_here("duplicate ENFORCED clause"));
                        }
                    } else {
                        break;
                    }
                }
                if alteration.deferrable.is_none()
                    && alteration.initially.is_none()
                    && alteration.enforced.is_none()
                {
                    return Err(self.syntax_error_here());
                }
                if alteration.deferrable == Some(false)
                    && alteration.initially == Some(ConstraintMode::Deferred)
                {
                    return Err(
                        self.err_here("constraint declared INITIALLY DEFERRED must be DEFERRABLE")
                    );
                }
                return Ok(AlterAction::AlterConstraint {
                    name,
                    alteration: ConstraintAlteration::ForeignKey(alteration),
                });
            }
            // ALTER [COLUMN] col { SET DEFAULT e | DROP DEFAULT | SET NOT NULL
            // | DROP NOT NULL }.
            let _ = self.eat_ident("column")?;
            let column = self.col_ident("column name")?;
            if self.eat_ident("options")? {
                return Ok(AlterAction::SetColumnForeignOptions {
                    column,
                    options: self.alter_foreign_options()?,
                });
            }
            // `TYPE t` and `SET DATA TYPE t` are the same column-type change;
            // `SET DEFAULT`/`SET NOT NULL` and `DROP DEFAULT`/`DROP NOT NULL`
            // are the other four ALTER COLUMN forms.
            if self.eat_ident("type")? {
                self.alter_column_type(column)
            } else if self.eat_ident("set")? {
                if self.eat_op("(")? {
                    let options = self.column_statistics_options()?;
                    self.expect_op(")")?;
                    Ok(AlterAction::SetStatisticsOptions { column, options })
                } else if self.eat_ident("data")? {
                    self.expect_ident("type")?;
                    self.alter_column_type(column)
                } else if self.eat_ident("default")? {
                    let start = self.peek_at;
                    let value = self.expression(0)?;
                    let value_text = self.text[start..self.peek_at].trim_end();
                    Ok(AlterAction::SetDefault {
                        column,
                        value,
                        value_text,
                    })
                } else if self.eat_ident("statistics")? {
                    let target = if self.eat_ident("default")? {
                        -1
                    } else {
                        i16::try_from(self.seq_int()?)
                            .ok()
                            .filter(|target| (-1..=10_000).contains(target))
                            .ok_or_else(|| self.err_here("statistics target is out of range"))?
                    };
                    Ok(AlterAction::SetStatistics { column, target })
                } else if self.eat_ident("storage")? {
                    match self.column_storage(true)? {
                        Some(storage) => Ok(AlterAction::SetStorage { column, storage }),
                        None => Ok(AlterAction::ResetStorage { column }),
                    }
                } else if self.eat_ident("compression")? {
                    let compression = self.column_compression()?;
                    Ok(AlterAction::SetCompression {
                        column,
                        compression,
                    })
                } else if self.eat_ident("generated")? {
                    let always = if self.eat_ident("always")? {
                        true
                    } else {
                        self.expect_ident("by")?;
                        self.expect_ident("default")?;
                        false
                    };
                    Ok(AlterAction::SetIdentityMode { column, always })
                } else if self.eat_ident("expression")? {
                    self.expect_ident("as")?;
                    self.expect_op("(")?;
                    let start = self.peek_at;
                    let _ = self.expression(0)?;
                    let expression_text = self.text[start..self.peek_at].trim_end();
                    self.expect_op(")")?;
                    Ok(AlterAction::SetGeneratedExpression {
                        column,
                        expression_text,
                    })
                } else if matches!(
                    self.peeked,
                    Tok::Ident(
                        "increment"
                            | "minvalue"
                            | "maxvalue"
                            | "start"
                            | "cache"
                            | "cycle"
                            | "no"
                            | "restart"
                    )
                ) {
                    Ok(AlterAction::AlterIdentitySequence {
                        column,
                        options: self.seq_options(true)?,
                    })
                } else {
                    self.expect_ident("not")?;
                    self.expect_ident("null")?;
                    Ok(AlterAction::SetNotNull { column })
                }
            } else if self.eat_ident("reset")? {
                self.expect_op("(")?;
                let options = self.column_statistics_option_names()?;
                self.expect_op(")")?;
                Ok(AlterAction::ResetStatisticsOptions { column, options })
            } else if self.eat_ident("drop")? {
                if self.eat_ident("default")? {
                    Ok(AlterAction::DropDefault { column })
                } else if self.eat_ident("expression")? {
                    let if_exists = if self.eat_ident("if")? {
                        self.expect_ident("exists")?;
                        true
                    } else {
                        false
                    };
                    Ok(AlterAction::DropGeneratedExpression { column, if_exists })
                } else if self.eat_ident("identity")? {
                    let if_exists = if self.eat_ident("if")? {
                        self.expect_ident("exists")?;
                        true
                    } else {
                        false
                    };
                    Ok(AlterAction::DropIdentity { column, if_exists })
                } else {
                    self.expect_ident("not")?;
                    self.expect_ident("null")?;
                    Ok(AlterAction::DropNotNull { column })
                }
            } else if self.eat_ident("add")? {
                // ALTER COLUMN col ADD GENERATED ... AS IDENTITY.
                self.expect_ident("generated")?;
                match self.generated_clause()? {
                    crate::sql::ast::ColGen::Identity(spec) => {
                        Ok(AlterAction::AddIdentity { column, spec })
                    }
                    crate::sql::ast::ColGen::Generated(_) => {
                        Err(self.err_here("a column cannot be turned into a generated column"))
                    }
                }
            } else if self.eat_ident("restart")? {
                let restart = if self.eat_ident("with")?
                    || matches!(self.peeked, Tok::Num(_))
                    || self.peeked == Tok::Op("-")
                {
                    Some(self.seq_int()?)
                } else {
                    None
                };
                Ok(AlterAction::AlterIdentitySequence {
                    column,
                    options: crate::sql::ast::SeqOptions {
                        restart: Some(restart),
                        ..crate::sql::ast::SeqOptions::EMPTY
                    },
                })
            } else {
                Err(self.syntax_error_here())
            }
        } else {
            Err(self.syntax_error_here())
        }
    }

    fn column_statistics_distinct(&mut self) -> Result<StatisticsDistinct, ParseError> {
        let negative = self.eat_op("-")?;
        let Tok::Num(raw) = self.peeked else {
            return Err(self.syntax_error_here());
        };
        let value = raw
            .parse::<f64>()
            .map_err(|_| self.err_here("invalid statistics option value"))?;
        self.advance()?;
        let value = if negative { -value } else { value };
        StatisticsDistinct::new(value).ok_or_else(|| ParseError {
            at: self.peek_at,
            message: stack_format!(
                PARSE_MESSAGE_BYTES,
                "statistics option value {} is out of range",
                value
            ),
            sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
            cursor: None,
            hint: None,
            near: None,
            detail: None,
        })
    }

    fn column_statistics_options(&mut self) -> Result<ColumnStatisticsOptions, ParseError> {
        let mut options = ColumnStatisticsOptions::DEFAULT;
        loop {
            let name = self.any_ident("column statistics option")?;
            self.expect_op("=")?;
            let value = self.column_statistics_distinct()?;
            if name.eq_ignore_ascii_case("n_distinct") {
                if options.n_distinct().is_some() {
                    return Err(self.err_here("column statistics option specified more than once"));
                }
                options.set_n_distinct(value);
            } else if name.eq_ignore_ascii_case("n_distinct_inherited") {
                if options.n_distinct_inherited().is_some() {
                    return Err(self.err_here("column statistics option specified more than once"));
                }
                options.set_n_distinct_inherited(value);
            } else {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(
                        PARSE_MESSAGE_BYTES,
                        "unrecognized parameter \"{}\"",
                        name
                    ),
                    sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                    cursor: None,
                    hint: None,
                    near: None,
                    detail: None,
                });
            }
            if !self.eat_op(",")? {
                return Ok(options);
            }
        }
    }

    fn column_statistics_option_names(
        &mut self,
    ) -> Result<ColumnStatisticsOptionNames, ParseError> {
        let mut names = ColumnStatisticsOptionNames::DEFAULT;
        loop {
            let name = self.any_ident("column statistics option")?;
            let selected = if name.eq_ignore_ascii_case("n_distinct") {
                &mut names.n_distinct
            } else if name.eq_ignore_ascii_case("n_distinct_inherited") {
                &mut names.n_distinct_inherited
            } else {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(
                        PARSE_MESSAGE_BYTES,
                        "unrecognized parameter \"{}\"",
                        name
                    ),
                    sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                    cursor: None,
                    hint: None,
                    near: None,
                    detail: None,
                });
            };
            if core::mem::replace(selected, true) {
                return Err(self.err_here("column statistics option specified more than once"));
            }
            if !self.eat_op(",")? {
                return Ok(names);
            }
        }
    }

    /// Parses the constraint form of ALTER TABLE ADD. An existing unique
    /// index is a distinct input state: its columns are catalog identities,
    /// not a second spelling to resolve later from a column list.
    fn alter_table_add_constraint(
        &mut self,
        name: Option<&'a str>,
        source_position: SourcePosition,
    ) -> Result<AlterAction<'a>, ParseError> {
        let primary = if self.eat_ident("primary")? {
            self.expect_ident("key")?;
            Some(true)
        } else if self.eat_ident("unique")? {
            Some(false)
        } else {
            None
        };
        let Some(primary) = primary else {
            return Ok(AlterAction::AddConstraint(self.table_constraint(
                name,
                source_position,
                ddl::ConstraintSite::AlterTable,
            )?));
        };
        if self.eat_ident("using")? {
            self.expect_ident("index")?;
            let index = self.qual_name("index name")?;
            let timing = self.constraint_timing(if primary {
                ddl::ConstraintAttributeRules::PRIMARY_KEY
            } else {
                ddl::ConstraintAttributeRules::UNIQUE
            })?;
            return Ok(AlterAction::AttachIndexConstraint {
                name,
                index,
                primary,
                timing,
            });
        }
        let nulls_not_distinct = if primary {
            false
        } else {
            self.unique_nulls_not_distinct()?
        };
        let columns = self.column_name_list()?;
        let timing = self.constraint_timing(if primary {
            ddl::ConstraintAttributeRules::PRIMARY_KEY
        } else {
            ddl::ConstraintAttributeRules::UNIQUE
        })?;
        Ok(AlterAction::AddConstraint(if primary {
            TableConstraint::PrimaryKey {
                source_position,
                name,
                columns,
                timing,
            }
        } else {
            TableConstraint::Unique {
                source_position,
                name,
                columns,
                timing,
                nulls_not_distinct,
            }
        }))
    }

    fn prepare(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("prepare")?;
        if self.eat_ident("transaction")? {
            return Ok(Stmt::PrepareTransaction(self.prepared_transaction_id()?));
        }
        let name = self.any_ident("prepared statement name")?;
        // Declared parameter types, if any; they constrain EXECUTE arguments.
        let mut ptypes = ArenaList::new(self.arena);
        if self.peeked == Tok::Op("(") {
            self.advance()?;
            loop {
                let ptype = self.type_name()?;
                self.push(&mut ptypes, ptype)?;
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
        }
        self.expect_ident("as")?;
        let start = self.peek_at;
        // Validate the body by parsing it; the raw text is what is stored.
        let _ = self.statement()?;
        let end = self.peek_at;
        let sql = self.text[start..end].trim();
        Ok(Stmt::Prepare {
            name,
            sql,
            param_types: ptypes.as_slice(),
        })
    }

    fn prepared_transaction_id(&mut self) -> Result<PreparedTransactionId, ParseError> {
        let value = self.str_literal("transaction identifier")?;
        PreparedTransactionId::parse(value).ok_or(ParseError {
            at: self.peek_at,
            message: stack_format!(
                PARSE_MESSAGE_BYTES,
                "transaction identifier \"{}\" is too long",
                value
            ),
            sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
            cursor: None,
            hint: None,
            near: None,
            detail: None,
        })
    }

    fn execute_prepared(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("execute")?;
        let name = self.any_ident("prepared statement name")?;
        let mut args = ArenaList::new(self.arena);
        if self.peeked == Tok::Op("(") {
            self.advance()?;
            if self.peeked != Tok::Op(")") {
                loop {
                    let arg = self.expression(0)?;
                    self.push(&mut args, arg)?;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
            }
            self.expect_op(")")?;
        }
        Ok(Stmt::ExecutePrepared {
            name,
            args: args.as_slice(),
        })
    }

    fn insert(&mut self) -> Result<Stmt<'a>, ParseError> {
        let source_position = crate::sql::ast::SourcePosition::new(Some(self.peek_at));
        self.expect_ident("insert")?;
        self.expect_ident("into")?;
        let table = self.qual_name("table name")?;
        let alias = if self.eat_ident("as")? {
            Some(self.col_ident("table alias")?)
        } else {
            None
        };
        // `(` starts the column list unless it opens a parenthesized query.
        let targets = if self.peeked == Tok::Op("(")
            && !matches!(
                self.lookahead(1)?,
                Tok::Ident("select" | "values" | "with" | "table") | Tok::Op("(")
            ) {
            self.insert_targets()?
        } else {
            InsertTargets::EMPTY
        };
        // OVERRIDING { SYSTEM | USER } VALUE for identity columns.
        let overriding = if self.eat_ident("overriding")? {
            let mode = if self.eat_ident("system")? {
                crate::sql::ast::Overriding::System
            } else {
                self.expect_ident("user")?;
                crate::sql::ast::Overriding::User
            };
            self.expect_ident("value")?;
            mode
        } else {
            crate::sql::ast::Overriding::None
        };
        // Source is either VALUES (...), ... or any PostgreSQL query body.
        let mut rows: ArenaList<'a, &'a [&'a Expr<'a>]> = ArenaList::new(self.arena);
        let mut select = None;
        if matches!(
            self.peeked,
            Tok::Ident("select" | "table" | "with") | Tok::Op("(")
        ) {
            let sel = self.query_select()?;
            select = Some(
                self.arena
                    .alloc(sel)
                    .map_err(|_| self.arena_full("statement too large for SQL arena"))?
                    as &_,
            );
        } else if self.eat_ident("default")? {
            // `DEFAULT VALUES` inserts one row of nothing but defaults, which
            // is exactly a row of `DEFAULT` markers over no named columns.
            self.expect_ident("values")?;
            self.push(&mut rows, &[])?;
        } else {
            self.expect_ident("values")?;
            let mut first_default = None;
            loop {
                self.expect_op("(")?;
                let mut row = ArenaList::new(self.arena);
                loop {
                    let value = self.value_or_default()?;
                    if matches!(value, Expr::DefaultMarker) {
                        first_default.get_or_insert(self.last_default);
                    }
                    self.push(&mut row, value)?;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                let row = row.as_slice();
                self.push(&mut rows, row)?;
                if !self.eat_op(",")? {
                    break;
                }
            }
            // `VALUES ...` followed by a set operation, ORDER BY or LIMIT is
            // the first part of a query, as PostgreSQL's grammar reads it.
            if matches!(
                self.peeked,
                Tok::Ident(
                    "union" | "intersect" | "except" | "order" | "limit" | "offset" | "fetch"
                )
            ) {
                // A query's VALUES cannot hold DEFAULT.
                if let Some(at) = first_default {
                    self.misplaced_default =
                        Some(self.misplaced_default.map_or(at, |first| first.min(at)));
                }
                let query = self.insert_values_query(rows.as_slice())?;
                select = Some(
                    self.arena
                        .alloc(query)
                        .map_err(|_| self.arena_full("statement too large for SQL arena"))?
                        as &_,
                );
                rows = ArenaList::new(self.arena);
            }
        }
        let mut columns = targets.names;
        if !targets.steps.is_empty() {
            // VALUES rows of another width than the targets read as the
            // query the subscripted targets select from, which reports the
            // count mismatch at the target or expression PostgreSQL names.
            let written = rows.borrowed_slice();
            if select.is_none()
                && let Some(width) = written.first().map(|row| row.len())
                && width != targets.names.len()
                && written.iter().all(|row| {
                    row.len() == width
                        && !row.iter().any(|value| matches!(value, Expr::DefaultMarker))
                })
            {
                let query = self.insert_values_query(written)?;
                select = Some(
                    self.arena
                        .alloc(query)
                        .map_err(|_| self.arena_full("statement too large for SQL arena"))?
                        as &_,
                );
                rows = ArenaList::new(self.arena);
            }
            if let Some(query) = select {
                // Select the assignments from the query's output columns.
                const SOURCE: &str = crate::sql::ast::INSERT_SUBSCRIPT_SOURCE;
                let names = self
                    .arena
                    .alloc_slice_with(targets.names.len(), |_| "")
                    .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
                for (index, name) in names.iter_mut().enumerate() {
                    *name = self
                        .arena
                        .alloc_str_display(format_args!("column{}", index + 1))
                        .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
                    // An output without a written value is reported at its
                    // target.
                    if let Some(copies) = &self.source_copies
                        && copies
                            .borrow_mut()
                            .push(crate::sql::error_position::SourceCopy {
                                address: name.as_ptr() as usize,
                                len: name.len(),
                                at: targets.positions[index],
                            })
                            .is_err()
                    {
                        return Err(self.arena_full("statement too large for SQL arena"));
                    }
                }
                let names = &*names;
                let values = self.insert_target_values(&targets, &mut |parser, target| {
                    parser.arena_expr(Expr::Column {
                        qualifier: Some(SOURCE),
                        name: names[target],
                    })
                })?;
                let mut items = ArenaList::new(self.arena);
                for expression in values {
                    self.push(
                        &mut items,
                        SelectItem::Expr {
                            expression,
                            alias: None,
                        },
                    )?;
                }
                let wrapped = Select {
                    items: items.as_slice(),
                    distinct: false,
                    distinct_on: &[],
                    windows: &[],
                    from: Some(FromClause {
                        base: TableRef {
                            schema: None,
                            table: "",
                            alias: Some(SOURCE),
                            subquery: Some(query),
                            func_args: None,
                            func_argument_names: &[],
                            func_variadic: false,
                            func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                            rows_from: None,
                            col_alias: Some(names),
                            inheritance: crate::sql::ast::RelationInheritance::Descendants,
                            sample: None,
                            cte: None,
                            with_ordinality: false,
                            lateral: false,
                            authorization_role: None,
                            bound_table: None,
                            view_access: None,
                            join_members: None,
                        },
                        joins: &[],
                    }),
                    where_clause: None,
                    group_by: &[],
                    grouping_sets: &[],
                    having: None,
                    order_by: &[],
                    limit: None,
                    offset: None,
                    with_ties: false,
                    with: &[],
                    set_body: None,
                    locking: &[],
                    ..*query
                };
                select = Some(
                    self.arena
                        .alloc(wrapped)
                        .map_err(|_| self.arena_full("statement too large for SQL arena"))?
                        as &_,
                );
            } else {
                let rows = rows
                    .as_mut_slice()
                    .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
                for row in rows.iter_mut() {
                    let written = *row;
                    if written.len() != targets.names.len() {
                        // The executor reports the count mismatch.
                        continue;
                    }
                    *row =
                        self.insert_target_values(&targets, &mut |_, target| Ok(written[target]))?;
                }
            }
            columns = self.insert_target_columns(&targets)?;
        }
        let on_conflict = self.on_conflict()?;
        let returning = self.returning()?;
        Ok(Stmt::Insert(Insert {
            source_position,
            table,
            alias,
            columns,
            rows: rows.as_slice(),
            select,
            on_conflict,
            returning,
            overriding,
            values_defaults: &[],
        }))
    }

    /// A query whose first leaf is the already-parsed `VALUES` `rows`.
    fn insert_values_query(
        &mut self,
        rows: &[&'a [&'a Expr<'a>]],
    ) -> Result<Select<'a>, ParseError> {
        let mut tree: Option<&'a SetTree<'a>> = None;
        for row in rows {
            let mut items = ArenaList::new(self.arena);
            for expression in row.iter().copied() {
                let alias = self
                    .arena
                    .alloc_str_display(format_args!("column{}", items.len() + 1))
                    .map_err(|_| self.err_here("VALUES too large"))?;
                self.push(
                    &mut items,
                    SelectItem::Expr {
                        expression,
                        alias: Some(alias),
                    },
                )?;
            }
            let leaf = self.alloc_set(SetTree::Select(
                self.arena
                    .alloc(Select {
                        items: items.as_slice(),
                        ..Select::EMPTY
                    })
                    .map_err(|_| self.err_here("VALUES too large"))?,
            ))?;
            tree = Some(match tree {
                None => leaf,
                Some(left) => self.alloc_set(SetTree::Op {
                    syntax: crate::sql::ast::SetOperationSyntax::Values,
                    operator: SetOp::Union,
                    all: true,
                    left,
                    right: leaf,
                })?,
            });
        }
        let first = tree.ok_or_else(|| self.syntax_error_here())?;
        let enclosing_windows = core::mem::replace(&mut self.windows, ArenaList::new(self.arena));
        let saved_allow = self.allow_into;
        self.allow_into = false;
        let body = self.set_union_after(first)?;
        let (order_by, limit, offset, with_ties) = self.order_limit()?;
        let locking = self.locking_clauses()?;
        self.allow_into = saved_allow;
        self.windows = enclosing_windows;
        Ok(Select {
            order_by,
            limit,
            offset,
            with_ties,
            set_body: Some(body),
            locking,
            ..Select::EMPTY
        })
    }

    /// `(column[indirection], ...)` of an INSERT, with PostgreSQL's
    /// checkInsertTargets: a whole column appears once; several targets
    /// through indirection may share a column.
    fn insert_targets(&mut self) -> Result<InsertTargets<'a>, ParseError> {
        let mut names = ArenaList::new(self.arena);
        let mut steps: ArenaList<'a, (usize, &'a [Indirection<'a>])> = ArenaList::new(self.arena);
        let mut positions = ArenaList::new(self.arena);
        self.expect_op("(")?;
        loop {
            let column_at = self.peek_at;
            let column = self.col_ident("column name")?;
            let path = self.assignment_indirection()?;
            let written = names.borrowed_slice();
            let whole = written.iter().enumerate().any(|(index, name)| {
                *name == column && !steps.borrowed_slice().iter().any(|(at, _)| *at == index)
            });
            if whole || (path.is_empty() && written.contains(&column)) {
                self.analysis_error(ParseError {
                    at: column_at,
                    message: stack_format!(
                        PARSE_MESSAGE_BYTES,
                        "column \"{}\" specified more than once",
                        column
                    ),
                    sqlstate: sqlstate::DUPLICATE_COLUMN,
                    cursor: Some(column_at),
                    hint: None,
                    near: None,
                    detail: None,
                })?;
            }
            if !path.is_empty() {
                self.push(&mut steps, (names.len(), path))?;
            }
            self.push(&mut names, column)?;
            self.push(&mut positions, column_at)?;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(InsertTargets {
            names: names.as_slice(),
            steps: steps.as_slice(),
            positions: positions.as_slice(),
        })
    }

    /// The INSERT columns after targets of one column merge.
    fn insert_target_columns(
        &mut self,
        targets: &InsertTargets<'a>,
    ) -> Result<&'a [&'a str], ParseError> {
        let mut columns = ArenaList::new(self.arena);
        for (index, name) in targets.names.iter().enumerate() {
            if targets.kept(index) {
                self.push(&mut columns, *name)?;
            }
        }
        Ok(columns.as_slice())
    }

    /// One value per INSERT column from the values written to its targets.
    /// PostgreSQL assigns a target with indirection into a NULL of its
    /// column's type, and merged targets left to right.
    fn insert_target_values(
        &mut self,
        targets: &InsertTargets<'a>,
        value_at: &mut dyn FnMut(&mut Self, usize) -> Result<&'a Expr<'a>, ParseError>,
    ) -> Result<&'a [&'a Expr<'a>], ParseError> {
        let mut values = ArenaList::new(self.arena);
        for (index, name) in targets.names.iter().enumerate() {
            if !targets.kept(index) {
                continue;
            }
            let merged = targets.merged(name);
            let mut value = None;
            for (target, target_name) in targets.names.iter().enumerate().skip(index) {
                if target_name != name || (target != index && !merged) {
                    continue;
                }
                let written = value_at(self, target)?;
                value = Some(match targets.path(target) {
                    Some(path) => {
                        self.reject_default_through(written, path, targets.positions[target])?;
                        let base = match value {
                            Some(previous) => previous,
                            None => self.arena_expr(Expr::Null)?,
                        };
                        self.indirection_assignment(base, written, path)?
                    }
                    None => written,
                });
            }
            self.push(&mut values, value.expect("a kept target has a value"))?;
        }
        Ok(values.as_slice())
    }

    /// PostgreSQL cannot assign DEFAULT through a field or subscript.
    fn reject_default_through(
        &mut self,
        value: &Expr<'a>,
        path: &[Indirection<'a>],
        target_at: usize,
    ) -> Result<(), ParseError> {
        if !matches!(value, Expr::DefaultMarker) {
            return Ok(());
        }
        let field = path
            .iter()
            .any(|step| matches!(step, Indirection::Field(_)));
        self.analysis_error(ParseError {
            at: target_at,
            message: StackStr::from_str(if field {
                "cannot set a subfield to DEFAULT"
            } else {
                "cannot set an array element to DEFAULT"
            }),
            sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
            cursor: Some(target_at),
            hint: None,
            near: None,
            detail: None,
        })
    }

    /// Parses a DML `SET` list into its scalar execution form. A row target is
    /// expanded here so every executor shares the same assignment semantics.
    fn assignment_list(&mut self) -> Result<&'a [(&'a str, &'a Expr<'a>)], ParseError> {
        let mut assignments: ArenaList<'a, (&'a str, &'a Expr<'a>)> = ArenaList::new(self.arena);
        // Whether each assignment writes through indirection (a field or
        // subscript), which lets later ones to its column build on it.
        let mut indirect: ArenaList<'a, bool> = ArenaList::new(self.arena);
        loop {
            let mut names = ArenaList::new(self.arena);
            let mut paths: ArenaList<'a, &'a [Indirection<'a>]> = ArenaList::new(self.arena);
            let mut positions = ArenaList::new(self.arena);
            let multiple = self.eat_op("(")?;
            loop {
                self.push(&mut positions, self.peek_at)?;
                let name = self.col_ident("column name")?;
                self.push(&mut names, name)?;
                let path = self.assignment_indirection()?;
                self.push(&mut paths, path)?;
                if !multiple || !self.eat_op(",")? {
                    break;
                }
            }
            if multiple {
                self.expect_op(")")?;
            }
            self.expect_op("=")?;

            let name_count = names.len();
            let mut values = ArenaList::new(self.arena);
            let value_count = if !multiple {
                let value = self.value_or_default()?;
                self.push(&mut values, value)?;
                1
            } else if self.eat_ident("row")? {
                self.expect_op("(")?;
                self.assignment_row_values(&mut values)?;
                values.len()
            } else if self.peeked == Tok::Op("(") {
                let open_at = self.peek_at;
                self.advance()?;
                if matches!(self.peeked, Tok::Ident("select") | Tok::Ident("with")) {
                    let select = self.query_select()?;
                    self.expect_op(")")?;
                    let select = self
                        .arena
                        .alloc(select)
                        .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
                    let arity = u8::try_from(name_count)
                        .map_err(|_| self.err_here("assignment target is too wide"))?;
                    let record =
                        self.arena_expr_at(Expr::RowSubquery { select, arity }, open_at)?;
                    for index in 0..name_count {
                        let field = self.arena_expr(Expr::RecordFieldIndex {
                            base: record,
                            index: index as u16,
                        })?;
                        self.push(&mut values, field)?;
                    }
                    name_count
                } else {
                    self.assignment_row_values(&mut values)?;
                    values.len()
                }
            } else {
                return Err(self.err_here(
                    "source for a multiple-column assignment must be a row expression or sub-SELECT",
                ));
            };
            if value_count != name_count {
                if value_count > name_count {
                    return Err(self.err_here("number of columns does not match number of values"));
                }
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(
                        PARSE_MESSAGE_BYTES,
                        "number of columns does not match number of values"
                    ),
                    sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                    cursor: None,
                    hint: None,
                    near: None,
                    detail: None,
                });
            }
            for (((name, value), path), at) in names
                .as_slice()
                .iter()
                .zip(values.as_slice())
                .zip(paths.as_slice())
                .zip(positions.as_slice())
            {
                if !path.is_empty() {
                    self.reject_default_through(value, path, *at)?;
                }
                let previous = assignments
                    .borrowed_slice()
                    .iter()
                    .position(|(previous, _)| previous == name);
                if path.is_empty() {
                    if previous.is_some() {
                        return Err(self.multiple_assignments(name));
                    }
                    self.push(&mut assignments, (*name, *value))?;
                    self.push(&mut indirect, false)?;
                    continue;
                }
                // Assignments through indirection to one column apply left to
                // right, each to the value the earlier ones built.
                let base = match previous {
                    Some(previous) if indirect.borrowed_slice()[previous] => {
                        assignments.borrowed_slice()[previous].1
                    }
                    Some(_) => return Err(self.multiple_assignments(name)),
                    None => self.arena_expr(Expr::Column {
                        qualifier: None,
                        name,
                    })?,
                };
                let assigned = self.indirection_assignment(base, value, path)?;
                match previous {
                    Some(previous) => {
                        assignments
                            .as_mut_slice()
                            .map_err(|_| self.arena_full("assignments exceed statement arena"))?
                            [previous]
                            .1 = assigned;
                    }
                    None => {
                        self.push(&mut assignments, (*name, assigned))?;
                        self.push(&mut indirect, true)?;
                    }
                }
            }
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(assignments.as_slice())
    }

    /// PostgreSQL's rewriter rejects this without a cursor position.
    fn multiple_assignments(&self, name: &str) -> ParseError {
        ParseError {
            at: self.peek_at,
            message: stack_format!(
                PARSE_MESSAGE_BYTES,
                "multiple assignments to same column \"{}\"",
                name
            ),
            sqlstate: sqlstate::SYNTAX_ERROR,
            cursor: None,
            hint: None,
            near: None,
            detail: None,
        }
    }

    /// The fields (`.x`) and subscripts (`[i]`) after an assignment target's
    /// column.
    fn assignment_indirection(&mut self) -> Result<&'a [Indirection<'a>], ParseError> {
        let mut steps = ArenaList::new(self.arena);
        loop {
            if self.eat_op(".")? {
                let field = self.col_ident("field name")?;
                self.push(&mut steps, Indirection::Field(field))?;
            } else if self.peeked == Tok::Op("[") {
                let subscripts = self.assignment_subscripts()?;
                self.push(&mut steps, Indirection::Subscripts(subscripts))?;
            } else {
                break;
            }
        }
        Ok(steps.as_slice())
    }

    /// `base<path> := value`. Subscripts alone use the subscript assignment
    /// calls; a path through a field is one `__assign_path(base, value,
    /// type, kinds, operands...)` call, `type` being the base's type OID when
    /// the base is an untyped NULL (`f` steps carry a field name, `e` steps
    /// an element subscript).
    fn indirection_assignment(
        &mut self,
        base: &'a Expr<'a>,
        value: &'a Expr<'a>,
        path: &[Indirection<'a>],
    ) -> Result<&'a Expr<'a>, ParseError> {
        if let [Indirection::Subscripts(subscripts)] = path {
            return self.subscript_assignment(base, value, subscripts);
        }
        let mut kinds = StackStr::<32>::new();
        let mut operands = ArenaList::new(self.arena);
        for (at, step) in path.iter().enumerate() {
            use core::fmt::Write as _;
            match step {
                Indirection::Field(name) => {
                    let _ = kinds.write_char('f');
                    let name = self.arena_expr(Expr::Str(name))?;
                    self.push(&mut operands, name)?;
                }
                Indirection::Subscripts(subscripts) => {
                    let sliced = subscripts
                        .iter()
                        .any(|subscript| matches!(subscript, AssignmentSubscript::Slice { .. }));
                    if sliced && at + 1 != path.len() {
                        return Err(ParseError {
                            at: self.peek_at,
                            message: stack_format!(
                                PARSE_MESSAGE_BYTES,
                                "assignment through an array slice is not supported"
                            ),
                            sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                            cursor: None,
                            hint: None,
                            near: None,
                            detail: None,
                        });
                    }
                    for subscript in *subscripts {
                        // A list holding any slice is a slice throughout.
                        let (kind, lower, upper) = match subscript {
                            AssignmentSubscript::Element(index) if sliced => {
                                ('u', None, Some(*index))
                            }
                            AssignmentSubscript::Element(index) => ('e', Some(*index), None),
                            AssignmentSubscript::Slice { lower, upper } => (
                                match (lower, upper) {
                                    (Some(_), Some(_)) => 's',
                                    (Some(_), None) => 'l',
                                    (None, Some(_)) => 'u',
                                    (None, None) => 'a',
                                },
                                *lower,
                                *upper,
                            ),
                        };
                        let _ = kinds.write_char(kind);
                        for bound in [lower, upper].into_iter().flatten() {
                            self.push(&mut operands, bound)?;
                        }
                    }
                }
            }
        }
        if kinds.is_truncated() {
            return Err(self.err_here("assignment target is too deep"));
        }
        let mut call_args = ArenaList::new(self.arena);
        self.push(&mut call_args, base)?;
        self.push(&mut call_args, value)?;
        let untyped = self.arena_expr(Expr::Null)?;
        self.push(&mut call_args, untyped)?;
        let kinds = self.arena_str(kinds.as_str())?;
        let kinds = self.arena_expr(Expr::Str(kinds))?;
        self.push(&mut call_args, kinds)?;
        for operand in operands.as_slice() {
            self.push(&mut call_args, *operand)?;
        }
        self.plain_call("__assign_path", call_args.as_slice())
    }

    /// The `[i]`, `[lower:upper]` subscripts after an assignment target.
    fn assignment_subscripts(&mut self) -> Result<&'a [AssignmentSubscript<'a>], ParseError> {
        let mut subscripts = ArenaList::new(self.arena);
        while self.eat_op("[")? {
            let lower = if self.peeked == Tok::Op(":") {
                None
            } else {
                Some(self.expression(0)?)
            };
            let subscript = if self.eat_op(":")? {
                let upper = if self.peeked == Tok::Op("]") {
                    None
                } else {
                    Some(self.expression(0)?)
                };
                AssignmentSubscript::Slice { lower, upper }
            } else {
                AssignmentSubscript::Element(lower.expect("an element subscript has a value"))
            };
            self.expect_op("]")?;
            self.push(&mut subscripts, subscript)?;
        }
        Ok(subscripts.as_slice())
    }

    /// `base[subscripts] := value` as the call execution evaluates: element
    /// subscripts serve arrays, jsonb and geometric types alike; a slice is
    /// array-only and carries each subscript's kind.
    fn subscript_assignment(
        &mut self,
        base: &'a Expr<'a>,
        value: &'a Expr<'a>,
        subscripts: &[AssignmentSubscript<'a>],
    ) -> Result<&'a Expr<'a>, ParseError> {
        let mut call_args = ArenaList::new(self.arena);
        self.push(&mut call_args, base)?;
        self.push(&mut call_args, value)?;
        let sliced = subscripts
            .iter()
            .any(|subscript| matches!(subscript, AssignmentSubscript::Slice { .. }));
        if !sliced {
            for subscript in subscripts {
                if let AssignmentSubscript::Element(index) = subscript {
                    self.push(&mut call_args, *index)?;
                }
            }
            return self.plain_call("__jsonb_subscript_set", call_args.as_slice());
        }
        let mut kinds = StackStr::<16>::new();
        let mut bounds = ArenaList::new(self.arena);
        for subscript in subscripts {
            use core::fmt::Write as _;
            let kind = match subscript {
                AssignmentSubscript::Element(index) => {
                    self.push(&mut bounds, *index)?;
                    'e'
                }
                AssignmentSubscript::Slice { lower, upper } => {
                    if let Some(lower) = lower {
                        self.push(&mut bounds, *lower)?;
                    }
                    if let Some(upper) = upper {
                        self.push(&mut bounds, *upper)?;
                    }
                    match (lower, upper) {
                        (Some(_), Some(_)) => 's',
                        (Some(_), None) => 'l',
                        (None, Some(_)) => 'u',
                        (None, None) => 'a',
                    }
                }
            };
            let _ = kinds.write_char(kind);
        }
        if kinds.is_truncated() {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(
                    PARSE_MESSAGE_BYTES,
                    "number of array dimensions ({}) exceeds the maximum allowed (6)",
                    subscripts.len()
                ),
                sqlstate: sqlstate::PROGRAM_LIMIT_EXCEEDED,
                cursor: None,
                hint: None,
                near: None,
                detail: None,
            });
        }
        let kinds = self.arena_str(kinds.as_str())?;
        let kinds = self.arena_expr(Expr::Str(kinds))?;
        self.push(&mut call_args, kinds)?;
        for bound in bounds.as_slice() {
            self.push(&mut call_args, *bound)?;
        }
        self.plain_call("__array_subscript_set", call_args.as_slice())
    }

    /// Parses the comma-separated body after a multi-column assignment's
    /// opening parenthesis and consumes its closing parenthesis.
    /// One INSERT or assigned value, where a whole `DEFAULT` is allowed.
    fn value_or_default(&mut self) -> Result<&'a Expr<'a>, ParseError> {
        let value = self.expression(0)?;
        if matches!(value, Expr::DefaultMarker) && self.misplaced_default == Some(self.last_default)
        {
            self.misplaced_default = None;
        }
        Ok(value)
    }

    fn assignment_row_values(
        &mut self,
        values: &mut ArenaList<'a, &'a Expr<'a>>,
    ) -> Result<(), ParseError> {
        loop {
            let value = self.value_or_default()?;
            self.push(values, value)?;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(())
    }

    /// `ON CONFLICT [(columns) | ON CONSTRAINT name] DO {NOTHING | UPDATE SET a
    /// = e, ... [WHERE cond]}`.
    fn on_conflict(&mut self) -> Result<Option<OnConflict<'a>>, ParseError> {
        if !self.eat_ident("on")? {
            return Ok(None);
        }
        self.expect_ident("conflict")?;
        let mut target = ArenaList::new(self.arena);
        let mut constraint = None;
        if self.eat_op("(")? {
            loop {
                let start = self.peek_at;
                let expression = self.expression(0)?;
                let expression_text = self.text[start..self.peek_at].trim_end();
                self.push(
                    &mut target,
                    crate::sql::ast::OnConflictTarget {
                        column: match expression {
                            Expr::Column {
                                qualifier: None,
                                name,
                            } => Some(*name),
                            _ => None,
                        },
                        expression,
                        expression_text,
                    },
                )?;
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
        } else if self.eat_ident("on")? {
            // `ON CONFLICT ON CONSTRAINT name`.
            self.expect_ident("constraint")?;
            constraint = Some(self.col_ident("constraint name")?);
        }
        self.expect_ident("do")?;
        let (update, update_where) = if self.eat_ident("nothing")? {
            (None, None)
        } else {
            self.expect_ident("update")?;
            self.expect_ident("set")?;
            let assigns = self.assignment_list()?;
            let where_clause = if self.eat_ident("where")? {
                Some(self.expression(0)?)
            } else {
                None
            };
            (Some(assigns), where_clause)
        };
        Ok(Some(OnConflict {
            target: target.as_slice(),
            constraint,
            update,
            update_where,
        }))
    }

    fn update(&mut self) -> Result<Stmt<'a>, ParseError> {
        let source_position = crate::sql::ast::SourcePosition::new(Some(self.peek_at));
        self.expect_ident("update")?;
        let table = self.qual_name("table name")?;
        let alias =
            if self.eat_ident("as")? || matches!(self.peeked, Tok::Ident(word) if word != "set") {
                Some(self.col_ident("table alias")?)
            } else {
                None
            };
        self.expect_ident("set")?;
        let assignments = self.assignment_list()?;
        let from = if self.eat_ident("from")? {
            let fc = self.dml_from_clause()?;
            Some(
                &*self
                    .arena
                    .alloc(fc)
                    .map_err(|_| self.arena_full("FROM too large for SQL arena"))?,
            )
        } else {
            None
        };
        let where_clause = self.where_clause()?;
        let returning = self.returning()?;
        Ok(Stmt::Update(Update {
            source_position,
            table,
            alias,
            assignments,
            from,
            where_clause,
            returning,
        }))
    }

    /// `MERGE INTO [ONLY] target [*] [AS alias] USING source [AS alias] ON cond
    /// { WHEN MATCHED | WHEN NOT MATCHED [BY SOURCE | BY TARGET] ... }...`.
    fn merge(&mut self) -> Result<Stmt<'a>, ParseError> {
        let source_position = crate::sql::ast::SourcePosition::new(Some(self.peek_at));
        use crate::sql::ast::{Merge, MergeMatchKind, MergeWhen};
        self.expect_ident("merge")?;
        self.expect_ident("into")?;
        let target_inheritance = if self.eat_ident("only")? {
            RelationInheritance::Only
        } else {
            RelationInheritance::Descendants
        };
        let parenthesized = target_inheritance == RelationInheritance::Only && self.eat_op("(")?;
        let target = self.qual_name("target table")?;
        if parenthesized {
            self.expect_op(")")?;
        }
        if target_inheritance == RelationInheritance::Descendants {
            let _ = self.eat_op("*")?;
        }
        let target_alias =
            if self.eat_ident("as")? || matches!(self.peeked, Tok::Ident(w) if w != "using") {
                Some(self.col_ident("target alias")?)
            } else {
                None
            };
        self.expect_ident("using")?;
        let source = self.table_ref()?;
        self.expect_ident("on")?;
        let on = self.expression(0)?;
        let mut whens = ArenaList::new(self.arena);
        let mut unconditional = [false; 3];
        while self.eat_ident("when")? {
            let kind = if self.eat_ident("not")? {
                self.expect_ident("matched")?;
                if self.eat_ident("by")? {
                    if self.eat_ident("source")? {
                        MergeMatchKind::NotMatchedBySource
                    } else {
                        self.expect_ident("target")?;
                        MergeMatchKind::NotMatchedByTarget
                    }
                } else {
                    MergeMatchKind::NotMatchedByTarget
                }
            } else {
                self.expect_ident("matched")?;
                MergeMatchKind::Matched
            };
            let cond = if self.eat_ident("and")? {
                Some(self.expression(0)?)
            } else {
                None
            };
            self.expect_ident("then")?;
            let when = match kind {
                MergeMatchKind::Matched => MergeWhen::Matched {
                    cond,
                    action: self.merge_target_action()?,
                },
                MergeMatchKind::NotMatchedBySource => MergeWhen::NotMatchedBySource {
                    cond,
                    action: self.merge_target_action()?,
                },
                MergeMatchKind::NotMatchedByTarget => MergeWhen::NotMatchedByTarget {
                    cond,
                    action: self.merge_source_action()?,
                },
            };
            if unconditional[kind.index()] {
                return Err(self.err_here(
                    "unreachable WHEN clause specified after unconditional WHEN clause",
                ));
            }
            if when.condition().is_none() {
                unconditional[kind.index()] = true;
            }
            self.push(&mut whens, when)?;
        }
        if whens.is_empty() {
            return Err(self.err_here("MERGE requires at least one WHEN clause"));
        }
        let returning = self.returning()?;
        Ok(Stmt::Merge(Merge {
            source_position,
            target,
            target_inheritance,
            target_alias,
            source,
            on,
            whens: whens.as_slice(),
            returning,
        }))
    }

    /// An action for a candidate that has a target row.
    fn merge_target_action(
        &mut self,
    ) -> Result<crate::sql::ast::MergeTargetAction<'a>, ParseError> {
        use crate::sql::ast::MergeTargetAction;
        if self.eat_ident("do")? {
            self.expect_ident("nothing")?;
            return Ok(MergeTargetAction::DoNothing);
        }
        if self.eat_ident("update")? {
            self.expect_ident("set")?;
            Ok(MergeTargetAction::Update(self.assignment_list()?))
        } else {
            self.expect_ident("delete")?;
            Ok(MergeTargetAction::Delete)
        }
    }

    /// An action for a candidate that has a source row but no target row.
    fn merge_source_action(
        &mut self,
    ) -> Result<crate::sql::ast::MergeSourceAction<'a>, ParseError> {
        use crate::sql::ast::MergeSourceAction;
        if self.eat_ident("do")? {
            self.expect_ident("nothing")?;
            return Ok(MergeSourceAction::DoNothing);
        }
        self.expect_ident("insert")?;
        let targets = if self.peeked == Tok::Op("(") {
            self.insert_targets()?
        } else {
            InsertTargets::EMPTY
        };
        let mut columns = targets.names;
        let overriding = if self.eat_ident("overriding")? {
            let mode = if self.eat_ident("system")? {
                crate::sql::ast::Overriding::System
            } else {
                self.expect_ident("user")?;
                crate::sql::ast::Overriding::User
            };
            self.expect_ident("value")?;
            mode
        } else {
            crate::sql::ast::Overriding::None
        };
        if self.eat_ident("default")? {
            self.expect_ident("values")?;
            if !matches!(overriding, crate::sql::ast::Overriding::None) {
                return Err(self.err_here("OVERRIDING is not allowed with DEFAULT VALUES"));
            }
            return Ok(MergeSourceAction::Insert {
                columns,
                values: &[],
                default_values: true,
                overriding,
            });
        }
        self.expect_ident("values")?;
        self.expect_op("(")?;
        let mut vals = ArenaList::new(self.arena);
        loop {
            let value = self.value_or_default()?;
            self.push(&mut vals, value)?;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        let mut values = vals.as_slice();
        if !targets.steps.is_empty() && values.len() == targets.names.len() {
            values = self.insert_target_values(&targets, &mut |_, target| Ok(values[target]))?;
            columns = self.insert_target_columns(&targets)?;
        }
        Ok(MergeSourceAction::Insert {
            columns,
            values,
            default_values: false,
            overriding,
        })
    }

    fn comment_target(&mut self) -> Result<crate::sql::ast::CommentTarget<'a>, ParseError> {
        use crate::sql::ast::{CommentRelKind, CommentTarget, RoutineTargetKind};
        let target = if self.eat_ident("table")? {
            CommentTarget::Relation {
                kind: CommentRelKind::Table,
                name: self.qual_name("table name")?,
            }
        } else if self.eat_ident("view")? {
            CommentTarget::Relation {
                kind: CommentRelKind::View,
                name: self.qual_name("view name")?,
            }
        } else if self.eat_ident("materialized")? {
            self.expect_ident("view")?;
            CommentTarget::Relation {
                kind: CommentRelKind::MaterializedView,
                name: self.qual_name("materialized view name")?,
            }
        } else if self.eat_ident("index")? {
            CommentTarget::Relation {
                kind: CommentRelKind::Index,
                name: self.qual_name("index name")?,
            }
        } else if self.eat_ident("sequence")? {
            CommentTarget::Relation {
                kind: CommentRelKind::Sequence,
                name: self.qual_name("sequence name")?,
            }
        } else if self.eat_ident("schema")? {
            CommentTarget::Schema(self.col_ident("schema name")?)
        } else if self.eat_ident("tablespace")? {
            CommentTarget::Tablespace(self.col_ident("tablespace name")?)
        } else if self.eat_ident("database")? {
            CommentTarget::Database(self.col_ident("database name")?)
        } else if self.eat_ident("large")? {
            self.expect_ident("object")?;
            CommentTarget::LargeObject(self.large_object_id()?)
        } else if self.eat_ident("collation")? {
            CommentTarget::Collation(self.qual_name("collation name")?)
        } else if self.eat_ident("conversion")? {
            CommentTarget::Conversion(self.qual_name("conversion name")?)
        } else if self.eat_ident("text")? {
            self.expect_ident("search")?;
            let kind = if self.eat_ident("parser")? {
                TextSearchObjectKind::Parser
            } else if self.eat_ident("template")? {
                TextSearchObjectKind::Template
            } else if self.eat_ident("dictionary")? {
                TextSearchObjectKind::Dictionary
            } else if self.eat_ident("configuration")? {
                TextSearchObjectKind::Configuration
            } else {
                return Err(self.syntax_error_here());
            };
            CommentTarget::TextSearch {
                kind,
                name: self.any_qual_name("text search object name")?,
            }
        } else if self.eat_ident("event")? {
            self.expect_ident("trigger")?;
            CommentTarget::EventTrigger(self.col_ident("event trigger name")?)
        } else if self.eat_ident("extension")? {
            CommentTarget::Extension(self.col_ident("extension name")?)
        } else if self.eat_ident("trigger")? {
            let name = self.col_ident("trigger name")?;
            self.expect_ident("on")?;
            CommentTarget::Trigger(crate::sql::ast::TriggerIdentity {
                name,
                table: self.qual_name("trigger table")?,
            })
        } else if self.eat_ident("rule")? {
            let name = self.col_ident("rule name")?;
            self.expect_ident("on")?;
            CommentTarget::Rule(crate::sql::ast::TriggerIdentity {
                name,
                table: self.qual_name("rule relation")?,
            })
        } else if self.eat_ident("policy")? {
            let name = self.col_ident("policy name")?;
            self.expect_ident("on")?;
            CommentTarget::Policy(crate::sql::ast::PolicyIdentity {
                name,
                table: self.qual_name("policy relation")?,
            })
        } else if self.eat_ident("constraint")? {
            let name = self.col_ident("constraint name")?;
            self.expect_ident("on")?;
            CommentTarget::Constraint {
                name,
                table: self.qual_name("constraint table")?,
            }
        } else if self.eat_ident("statistics")? {
            CommentTarget::Statistics(self.qual_name("statistics name")?)
        } else if self.eat_ident("role")? {
            CommentTarget::Role(self.col_ident("role name")?)
        } else if self.eat_ident("access")? {
            self.expect_ident("method")?;
            CommentTarget::AccessMethod(self.col_ident("access method name")?)
        } else if self.eat_ident("procedural")? {
            self.expect_ident("language")?;
            CommentTarget::ProceduralLanguage(self.col_ident("procedural language name")?)
        } else if self.eat_ident("cast")? {
            self.expect_op("(")?;
            let source_type = self.unmodified_type_name()?;
            self.expect_ident("as")?;
            let target_type = self.unmodified_type_name()?;
            self.expect_op(")")?;
            CommentTarget::Cast {
                source_type,
                target_type,
            }
        } else if self.eat_ident("operator")? {
            if self.eat_ident("family")? {
                let name = self.qual_name("operator family name")?;
                self.expect_ident("using")?;
                CommentTarget::OperatorFamily {
                    name,
                    method: self.access_method()?,
                }
            } else if self.eat_ident("class")? {
                let name = self.qual_name("operator class name")?;
                self.expect_ident("using")?;
                CommentTarget::OperatorClass {
                    name,
                    method: self.access_method()?,
                }
            } else {
                CommentTarget::Operator(self.operator_identity()?)
            }
        } else if self.eat_ident("server")? {
            CommentTarget::ForeignServer(self.col_ident("foreign server name")?)
        } else if self.eat_ident("foreign")? {
            if self.eat_ident("data")? {
                self.expect_ident("wrapper")?;
                CommentTarget::ForeignDataWrapper(self.col_ident("foreign-data wrapper name")?)
            } else {
                self.expect_ident("table")?;
                CommentTarget::Relation {
                    kind: CommentRelKind::ForeignTable,
                    name: self.qual_name("foreign table name")?,
                }
            }
        } else if self.eat_ident("publication")? {
            CommentTarget::Publication(self.col_ident("publication name")?)
        } else if self.eat_ident("subscription")? {
            CommentTarget::Subscription(self.col_ident("subscription name")?)
        } else if self.eat_ident("function")? {
            CommentTarget::Routine {
                kind: RoutineTargetKind::Function,
                identity: self.routine_identity()?,
            }
        } else if self.eat_ident("aggregate")? {
            CommentTarget::Routine {
                kind: RoutineTargetKind::Aggregate,
                identity: self.routine_identity()?,
            }
        } else if self.eat_ident("procedure")? {
            CommentTarget::Routine {
                kind: RoutineTargetKind::Procedure,
                identity: self.routine_identity()?,
            }
        } else if self.eat_ident("routine")? {
            CommentTarget::Routine {
                kind: RoutineTargetKind::Either,
                identity: self.routine_identity()?,
            }
        } else if self.eat_ident("type")? {
            CommentTarget::Type {
                name: self.comment_type_name()?,
                domain_only: false,
            }
        } else if self.eat_ident("domain")? {
            CommentTarget::Type {
                name: self.comment_type_name()?,
                domain_only: true,
            }
        } else if self.eat_ident("column")? {
            // `[schema.]table.column`: the last dotted part is the column.
            let first = self.col_ident("column reference")?;
            self.expect_op(".")?;
            let second = self.col_ident("column reference")?;
            if self.eat_op(".")? {
                let third = self.col_ident("column reference")?;
                CommentTarget::Column {
                    relation: QualName {
                        schema: Some(first),
                        name: second,
                    },
                    column: third,
                }
            } else {
                CommentTarget::Column {
                    relation: QualName::bare(first),
                    column: second,
                }
            }
        } else {
            return Err(self.err_here("unsupported COMMENT ON object type"));
        };
        Ok(target)
    }

    fn comment_text(&mut self) -> Result<Option<&'a str>, ParseError> {
        self.expect_ident("is")?;
        let text = match self.expression(0)? {
            Expr::Str(s) => Some(*s),
            Expr::Null => None,
            _ => return Err(self.err_here("COMMENT value must be a string literal or NULL")),
        };
        Ok(text)
    }

    /// `COMMENT ON <object> IS { 'text' | NULL }`.
    fn comment(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("comment")?;
        self.expect_ident("on")?;
        let target = self.comment_target()?;
        let text = self.comment_text()?;
        Ok(Stmt::Comment { target, text })
    }

    fn security_label_target(
        &mut self,
    ) -> Result<crate::sql::ast::SecurityLabelTarget<'a>, ParseError> {
        use crate::sql::ast::{SecurityLabelRelationKind, SecurityLabelTarget};
        let target = if self.eat_ident("table")? {
            SecurityLabelTarget::Relation {
                kind: SecurityLabelRelationKind::Table,
                name: self.qual_name("table name")?,
            }
        } else if self.eat_ident("column")? {
            let first = self.col_ident("column reference")?;
            self.expect_op(".")?;
            let second = self.col_ident("column reference")?;
            if self.eat_op(".")? {
                let third = self.col_ident("column reference")?;
                SecurityLabelTarget::Column {
                    relation: QualName {
                        schema: Some(first),
                        name: second,
                    },
                    column: third,
                }
            } else {
                SecurityLabelTarget::Column {
                    relation: QualName::bare(first),
                    column: second,
                }
            }
        } else if self.eat_ident("aggregate")? {
            SecurityLabelTarget::Aggregate(self.aggregate_identity()?)
        } else if self.eat_ident("database")? {
            SecurityLabelTarget::Database(self.col_ident("database name")?)
        } else if self.eat_ident("domain")? {
            SecurityLabelTarget::Type {
                name: self.comment_type_name()?,
                domain_only: true,
            }
        } else if self.eat_ident("event")? {
            self.expect_ident("trigger")?;
            SecurityLabelTarget::EventTrigger(self.col_ident("event trigger name")?)
        } else if self.eat_ident("foreign")? {
            self.expect_ident("table")?;
            SecurityLabelTarget::Relation {
                kind: SecurityLabelRelationKind::ForeignTable,
                name: self.qual_name("foreign table name")?,
            }
        } else if self.eat_ident("function")? {
            SecurityLabelTarget::Routine {
                kind: RoutineTargetKind::Function,
                identity: self.routine_identity()?,
            }
        } else if self.eat_ident("large")? {
            self.expect_ident("object")?;
            SecurityLabelTarget::LargeObject(self.large_object_id()?)
        } else if self.eat_ident("materialized")? {
            self.expect_ident("view")?;
            SecurityLabelTarget::Relation {
                kind: SecurityLabelRelationKind::MaterializedView,
                name: self.qual_name("materialized view name")?,
            }
        } else if self.eat_ident("procedural")? {
            self.expect_ident("language")?;
            SecurityLabelTarget::ProceduralLanguage(self.col_ident("procedural language name")?)
        } else if self.eat_ident("language")? {
            SecurityLabelTarget::ProceduralLanguage(self.col_ident("procedural language name")?)
        } else if self.eat_ident("procedure")? {
            SecurityLabelTarget::Routine {
                kind: RoutineTargetKind::Procedure,
                identity: self.routine_identity()?,
            }
        } else if self.eat_ident("publication")? {
            SecurityLabelTarget::Publication(self.col_ident("publication name")?)
        } else if self.eat_ident("role")? {
            SecurityLabelTarget::Role(self.col_ident("role name")?)
        } else if self.eat_ident("routine")? {
            SecurityLabelTarget::Routine {
                kind: RoutineTargetKind::Either,
                identity: self.routine_identity()?,
            }
        } else if self.eat_ident("schema")? {
            SecurityLabelTarget::Schema(self.col_ident("schema name")?)
        } else if self.eat_ident("sequence")? {
            SecurityLabelTarget::Relation {
                kind: SecurityLabelRelationKind::Sequence,
                name: self.qual_name("sequence name")?,
            }
        } else if self.eat_ident("subscription")? {
            SecurityLabelTarget::Subscription(self.col_ident("subscription name")?)
        } else if self.eat_ident("tablespace")? {
            SecurityLabelTarget::Tablespace(self.col_ident("tablespace name")?)
        } else if self.eat_ident("type")? {
            SecurityLabelTarget::Type {
                name: self.comment_type_name()?,
                domain_only: false,
            }
        } else if self.eat_ident("view")? {
            SecurityLabelTarget::Relation {
                kind: SecurityLabelRelationKind::View,
                name: self.qual_name("view name")?,
            }
        } else {
            return Err(self.err_here("unsupported SECURITY LABEL ON object type"));
        };
        Ok(target)
    }

    fn security_label(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("security")?;
        self.expect_ident("label")?;
        let provider = if self.eat_ident("for")? {
            Some(self.col_ident("security label provider")?)
        } else {
            None
        };
        self.expect_ident("on")?;
        let target = self.security_label_target()?;
        let label = self.comment_text()?;
        Ok(Stmt::SecurityLabel {
            provider,
            target,
            label,
        })
    }

    fn load(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("load")?;
        let Expr::Str(path) = self.expression(0)? else {
            return Err(self.err_here("LOAD path must be a string literal"));
        };
        Ok(Stmt::Load(path))
    }

    /// A type name in `COMMENT ON TYPE/DOMAIN`: keep a user schema qualifier,
    /// while canonicalizing PostgreSQL's multi-word built-in spellings.
    fn comment_type_name(&mut self) -> Result<&'a str, ParseError> {
        let first = self.any_ident("type name")?;
        if self.eat_op(".")? {
            let second = self.any_ident("type name")?;
            if self.eat_op("[")? {
                self.expect_op("]")?;
                return self.arena_str(stack_format!(144, "{}.{}[]", first, second).as_str());
            }
            return self.arena_str(stack_format!(144, "{}.{}", first, second).as_str());
        }
        let mut name = first;
        if name == "double" {
            self.expect_ident("precision")?;
            name = "float8";
        } else if name == "bit" && self.eat_ident("varying")? {
            name = "varbit";
        } else if (name == "character" || name == "char") && self.eat_ident("varying")? {
            name = "varchar";
        } else if name == "timestamp" || name == "time" {
            if self.eat_ident("with")? {
                self.expect_ident("time")?;
                self.expect_ident("zone")?;
                name = if name == "timestamp" {
                    "timestamptz"
                } else {
                    "timetz"
                };
            } else if self.eat_ident("without")? {
                self.expect_ident("time")?;
                self.expect_ident("zone")?;
            }
        }
        if self.eat_op("[")? {
            self.expect_op("]")?;
            return self.arena_str(stack_format!(144, "{}[]", name).as_str());
        }
        Ok(name)
    }

    fn delete(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("delete")?;
        self.expect_ident("from")?;
        let table = self.qual_name("table name")?;
        let alias = if self.eat_ident("as")?
            || matches!(self.peeked, Tok::Ident(word) if word != "using" && word != "where" && word != "returning")
        {
            Some(self.col_ident("table alias")?)
        } else {
            None
        };
        let using = if self.eat_ident("using")? {
            let fc = self.dml_from_clause()?;
            Some(
                &*self
                    .arena
                    .alloc(fc)
                    .map_err(|_| self.arena_full("USING too large for SQL arena"))?,
            )
        } else {
            None
        };
        let where_clause = self.where_clause()?;
        let returning = self.returning()?;
        Ok(Stmt::Delete(Delete {
            table,
            alias,
            using,
            where_clause,
            returning,
        }))
    }

    fn where_clause(&mut self) -> Result<Option<&'a Expr<'a>>, ParseError> {
        if self.eat_ident("where")? {
            Ok(Some(self.expression(0)?))
        } else {
            Ok(None)
        }
    }

    /// Multi-word type names are normalized: `double precision` → float8.
    /// Base type name plus its PostgreSQL atttypmod (-1 when there is no
    /// `(...)`). varchar/char carry a length; numeric/decimal carry
    /// (precision[, scale]); any other type with a modifier is a loud error.
    fn type_name_mod(&mut self) -> Result<(&'a str, i32), ParseError> {
        let (name, type_mod, _) = self.type_name_written()?;
        Ok((name, type_mod))
    }

    /// Parses a type name and also returns the parts of PostgreSQL's written
    /// form (its `TypeNameToString`) for routine identity diagnostics.
    fn type_name_written(&mut self) -> Result<(&'a str, i32, WrittenTypeName<'a>), ParseError> {
        let source = NamespaceSource {
            start: self.peek_at,
            end: self.lexer.token_end(),
        };
        let mut quoted = matches!(self.peeked, Tok::QuotedIdent(_));
        // A fully reserved word never names a type (`f(x any)`); `"any"` does.
        if let Tok::Ident(word) = self.peeked
            && keyword_category(word) == Some(Keyword::Reserved)
        {
            return Err(self.syntax_error_here());
        }
        let mut name = self.any_ident("type name")?;
        let mut written = WrittenTypeName {
            schema: None,
            name,
            array: false,
        };
        // PostgreSQL's grammar turns these unquoted keywords into
        // `pg_catalog`-qualified internal names.
        let keyword = !quoted
            && matches!(
                name,
                "int"
                    | "integer"
                    | "smallint"
                    | "bigint"
                    | "real"
                    | "float"
                    | "double"
                    | "decimal"
                    | "dec"
                    | "numeric"
                    | "boolean"
                    | "bit"
                    | "character"
                    | "char"
                    | "varchar"
                    | "timestamp"
                    | "time"
                    | "interval"
                    | "json"
                    | "national"
                    | "nchar"
            );
        // System-qualified built-ins normalize to their bare spelling. A user
        // schema is part of the type identity and must survive parsing; dropping
        // it aliases same-named domains/enums in different schemas.
        if self.peeked == Tok::Op(".") {
            self.record_namespace(source)?;
            let schema = name;
            self.advance()?;
            let base_quoted = matches!(self.peeked, Tok::QuotedIdent(_));
            let base = self.any_ident("type name")?;
            written = WrittenTypeName {
                schema: Some(schema),
                name: base,
                array: false,
            };
            name = if schema == "pg_catalog" {
                quoted = base_quoted;
                base
            } else {
                quoted = false;
                let composed = self
                    .arena
                    .alloc_str(stack_format!(128, "{}.{}", schema, base).as_str())
                    .map_err(|_| self.err_here("type name too long"))?;
                self.note_composed_source(composed, source.start)?;
                composed
            };
        }
        // SQL `char` is the blank-padded character type. Only the quoted
        // catalog spelling names PostgreSQL's distinct one-byte OID 18 type.
        if quoted && name == "char" {
            name = "\"char\"";
        }
        // `NATIONAL CHARACTER`, `NATIONAL CHAR` and `NCHAR` are SQL's
        // spellings of `character`, with or without `VARYING`.
        if keyword && name == "national" {
            if !self.eat_ident("character")? {
                self.expect_ident("char")?;
            }
            name = "character";
        } else if keyword && name == "nchar" {
            name = "character";
        }
        if name == "double" {
            self.expect_ident("precision")?;
            name = "float8";
        }
        // `bit varying [(n)]` is the `varbit` type.
        if name == "bit" && self.eat_ident("varying")? {
            name = "varbit";
        }
        // `character varying` / `char varying` is `varchar`.
        if (name == "character" || name == "char") && self.eat_ident("varying")? {
            name = "varchar";
        }
        let standard_temporal = keyword && (name == "timestamp" || name == "time");
        let mut leading_typmods = None;
        if standard_temporal && self.peeked == Tok::Op("(") {
            leading_typmods = Some(self.written_typmods(true)?);
        }
        if standard_temporal {
            if self.eat_ident("with")? {
                self.expect_ident("time")?;
                self.expect_ident("zone")?;
                name = if name == "timestamp" {
                    "timestamptz"
                } else {
                    "timetz"
                };
            } else if self.eat_ident("without")? {
                self.expect_ident("time")?;
                self.expect_ident("zone")?;
            }
        }
        let mut typmods = leading_typmods;
        let type_mod = if typmods.is_some() {
            -1
        } else if standard_temporal && self.peeked == Tok::Op("(") {
            return Err(self.syntax_error_here());
        } else if keyword && name == "float" && self.peeked == Tok::Op("(") {
            name = self.float_precision_type()?;
            -1
        } else if self.peeked == Tok::Op("(") {
            // PostgreSQL's keyword spellings without modifiers (`integer`,
            // `boolean`, `double precision`, `json`) end before `(`.
            if keyword
                && !matches!(
                    name,
                    "numeric"
                        | "decimal"
                        | "dec"
                        | "bit"
                        | "varbit"
                        | "character"
                        | "char"
                        | "varchar"
                        | "interval"
                )
            {
                return Err(self.syntax_error_here());
            }
            let integer_constant_only =
                keyword && matches!(name, "character" | "char" | "varchar" | "interval");
            typmods = Some(self.written_typmods(integer_constant_only)?);
            -1
        } else if name == "interval" {
            self.interval_range_modifier(source.start)?.unwrap_or(-1)
        } else if name == "char" || name == "character" {
            // Bare `char`/`character` is char(1) in PostgreSQL (`'ab'::char`
            // is 'a'); only the internal name `bpchar` means unlimited.
            TypeMod::Length(1).encode()
        } else if name == "bit" {
            TypeMod::BitLength(1).encode()
        } else {
            -1
        };
        if keyword && written.schema.is_none() {
            written = WrittenTypeName {
                schema: Some("pg_catalog"),
                name: match name {
                    "int" | "integer" => "int4",
                    "smallint" => "int2",
                    "bigint" => "int8",
                    "real" => "float4",
                    "float" => "float8",
                    "decimal" | "dec" => "numeric",
                    "boolean" => "bool",
                    "character" | "char" => "bpchar",
                    other => other,
                },
                array: false,
            };
        }
        // `[]`, `[n]` and `ARRAY [n]` all spell the same array type:
        // PostgreSQL ignores array bounds and dimension counts in type names.
        let mut array = false;
        if self.eat_ident("array")? {
            array = true;
            if self.eat_op("[")? {
                self.array_bound()?;
            }
        } else {
            while self.eat_op("[")? {
                array = true;
                self.array_bound()?;
            }
        }
        written.array = array;
        let type_mod = match typmods {
            Some(typmods) => {
                let display = written;
                self.checked_typmod(name, typmods, Some(source.start), format_args!("{display}"))?
            }
            None => type_mod,
        };
        if array {
            let array = self
                .arena
                .alloc_str(stack_format!(132, "{}[]", name).as_str())
                .map_err(|_| self.err_here("type name too long"))?;
            self.note_composed_source(array, source.start)?;
            return Ok((array, type_mod, written));
        }
        Ok((name, type_mod, written))
    }

    /// The rest of a type name's array bound after `[`: an optional integer
    /// and `]`.
    fn array_bound(&mut self) -> Result<(), ParseError> {
        if let Tok::Num(text) = self.peeked {
            if text.parse::<i32>().is_err() {
                return Err(self.syntax_error_here());
            }
            self.advance()?;
        }
        self.expect_op("]")
    }

    /// SQL `float(p)` chooses one of two concrete PostgreSQL types; `p` is not
    /// an atttypmod. Resolve the representation while parsing so execution and
    /// catalogs never carry an unvalidated precision beside a float value.
    fn float_precision_type(&mut self) -> Result<&'a str, ParseError> {
        self.expect_op("(")?;
        let precision_at = self.peek_at;
        let Some(precision) = (match self.peeked {
            Tok::Num(text) => crate::sql::eval::parse_int_literal(text)
                .and_then(|value| i32::try_from(value).ok()),
            _ => None,
        }) else {
            return Err(self.syntax_error_here());
        };
        self.advance()?;
        self.expect_op(")")?;
        let error = |message: &str| ParseError {
            at: precision_at,
            message: StackStr::from_str(message),
            sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
            cursor: Some(precision_at),
            hint: None,
            near: None,
            detail: None,
        };
        if precision < 1 {
            return Err(error("precision for type float must be at least 1 bit"));
        }
        if precision > 53 {
            return Err(error("precision for type float must be less than 54 bits"));
        }
        Ok(if precision <= 24 { "float4" } else { "float8" })
    }

    /// Parses the closed set of PostgreSQL interval field ranges after an
    /// `interval` type name. The raw bit mask is produced only by
    /// `IntervalRange::encode`, so invalid field combinations cannot enter a
    /// stored or wire type modifier.
    fn interval_range_modifier(&mut self, type_at: usize) -> Result<Option<i32>, ParseError> {
        let Tok::Ident(first_name) = self.peeked else {
            return Ok(None);
        };
        let Some(first) = IntervalField::parse(first_name) else {
            return Ok(None);
        };
        self.advance()?;
        // PostgreSQL's grammar has a production for each valid field range,
        // so any other pair, or a precision after a field other than SECOND,
        // is a syntax error at the token where it stops.
        if self.peeked == Tok::Ident("to")
            && matches!(first, IntervalField::Month | IntervalField::Second)
        {
            return Err(self.syntax_error_here());
        }
        let last = if self.eat_ident("to")? {
            let Tok::Ident(last_name) = self.peeked else {
                return Err(self.syntax_error_here());
            };
            let Some(last) = IntervalField::parse(last_name).filter(|last| {
                IntervalRange::from_bounds(first, *last).is_some() && *last != first
            }) else {
                return Err(self.syntax_error_here());
            };
            self.advance()?;
            last
        } else {
            first
        };
        let range = IntervalRange::from_bounds(first, last).expect("checked above");
        let precision = if self.peeked == Tok::Op("(") {
            if last != IntervalField::Second {
                return Err(self.syntax_error_here());
            }
            match TypeMod::decode(
                super::types::ColType::Interval,
                self.type_modifier("interval", true, Some(type_at))?,
            ) {
                TypeMod::IntervalMod {
                    precision: Some(precision),
                    ..
                } => Some(precision),
                _ => unreachable!("interval precision parser returns an interval modifier"),
            }
        } else {
            None
        };
        Ok(Some(TypeMod::IntervalMod { range, precision }.encode()))
    }

    /// A type name for a prepared-statement parameter: PostgreSQL parses and
    /// then ignores any modifier here (`PREPARE q(varchar(2))` does not
    /// truncate its argument — verified against 18.4), so the modifier is
    /// accepted and dropped.
    fn type_name(&mut self) -> Result<&'a str, ParseError> {
        let (name, _type_mod) = self.type_name_mod()?;
        Ok(name)
    }

    fn alias(&mut self) -> Result<Option<&'a str>, ParseError> {
        if self.eat_ident("as")? {
            return Ok(Some(self.any_ident("alias")?));
        }
        // Bare alias: an identifier that is not a clause keyword.
        if let Tok::Ident(name) = self.peeked {
            let reserved = matches!(
                name,
                "from"
                    | "where"
                    | "order"
                    | "limit"
                    | "group"
                    | "having"
                    | "union"
                    | "intersect"
                    | "except"
                    | "window"
                    | "for"
                    | "fetch"
                    | "into"
                    | "and"
                    | "or"
                    | "is"
                    | "as"
                    | "asc"
                    | "desc"
                    | "offset"
                    | "returning"
                    | "on"
            );
            if !reserved {
                self.advance()?;
                return Ok(Some(name));
            }
        }
        if let Tok::QuotedIdent(name) = self.peeked {
            self.advance()?;
            return Ok(Some(name));
        }
        Ok(None)
    }

    fn transaction_modifiers(
        &mut self,
        allow_work: bool,
    ) -> Result<TransactionCharacteristics, ParseError> {
        if allow_work && !self.eat_ident("work")? {
            let _ = self.eat_ident("transaction")?;
        }
        self.parse_transaction_characteristics()
    }

    fn parse_transaction_characteristics(
        &mut self,
    ) -> Result<TransactionCharacteristics, ParseError> {
        let mut parsed = TransactionCharacteristics::EMPTY;
        loop {
            let before = parsed;
            if self.eat_ident("isolation")? {
                self.expect_ident("level")?;
                let isolation = if self.eat_ident("serializable")? {
                    TransactionIsolation::Serializable
                } else if self.eat_ident("repeatable")? {
                    self.expect_ident("read")?;
                    TransactionIsolation::RepeatableRead
                } else {
                    self.expect_ident("read")?;
                    if self.eat_ident("committed")? {
                        TransactionIsolation::ReadCommitted
                    } else if self.eat_ident("uncommitted")? {
                        TransactionIsolation::ReadUncommitted
                    } else {
                        return Err(self.syntax_error_here());
                    }
                };
                if parsed.isolation.replace(isolation).is_some() {
                    return Err(self.err_here("conflicting or redundant transaction options"));
                }
            } else if self.eat_ident("read")? {
                let read_only = if self.eat_ident("only")? {
                    true
                } else {
                    self.expect_ident("write")?;
                    false
                };
                if parsed.read_only.replace(read_only).is_some() {
                    return Err(self.err_here("conflicting or redundant transaction options"));
                }
            } else if self.eat_ident("deferrable")? {
                if parsed.deferrable.replace(true).is_some() {
                    return Err(self.err_here("conflicting or redundant transaction options"));
                }
            } else if self.eat_ident("not")? {
                self.expect_ident("deferrable")?;
                if parsed.deferrable.replace(false).is_some() {
                    return Err(self.err_here("conflicting or redundant transaction options"));
                }
            }
            if parsed == before {
                break;
            }
            if self.eat_op(",")? && matches!(self.peeked, Tok::Op(";") | Tok::Eof) {
                return Err(self.syntax_error_here());
            }
        }
        Ok(parsed)
    }

    fn configuration_value_list(
        &mut self,
        name: &str,
        allow_list: bool,
    ) -> Result<RoutineConfigValue<'a>, ParseError> {
        if self.eat_ident("default")? {
            return Ok(RoutineConfigValue::Default);
        }
        let mut bytes = ArenaList::new(self.arena);
        let mut count = 0;
        loop {
            let negative = self.eat_op("-")?;
            let positive = !negative && self.eat_op("+")?;
            if (positive || negative) && !matches!(self.peeked, Tok::Num(_)) {
                return Err(self.syntax_error_here());
            }
            let (value, identifier) = match self.peeked {
                Tok::Ident(value) | Tok::QuotedIdent(value) => (value, true),
                Tok::Str(value) | Tok::Num(value) => (value, false),
                _ => return Err(self.syntax_error_here()),
            };
            let path = name.eq_ignore_ascii_case("search_path");
            if path && identifier && value.len() > 63 {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(
                        PARSE_MESSAGE_BYTES,
                        "search_path identifier is longer than 63 bytes"
                    ),
                    sqlstate: sqlstate::NAME_TOO_LONG,
                    cursor: None,
                    hint: None,
                    near: None,
                    detail: None,
                });
            }
            if count != 0 {
                for byte in b", " {
                    bytes
                        .push(*byte)
                        .map_err(|_| self.arena_full("configuration value exceeds SQL arena"))?;
                }
            }
            let quoted = path && super::eval::ident_needs_quotes(value);
            if quoted {
                bytes
                    .push(b'"')
                    .map_err(|_| self.arena_full("configuration value exceeds SQL arena"))?;
            }
            if negative {
                bytes
                    .push(b'-')
                    .map_err(|_| self.arena_full("configuration value exceeds SQL arena"))?;
            }
            for byte in value.as_bytes() {
                bytes
                    .push(*byte)
                    .map_err(|_| self.arena_full("configuration value exceeds SQL arena"))?;
                if quoted && *byte == b'"' {
                    bytes
                        .push(b'"')
                        .map_err(|_| self.arena_full("configuration value exceeds SQL arena"))?;
                }
            }
            if quoted {
                bytes
                    .push(b'"')
                    .map_err(|_| self.arena_full("configuration value exceeds SQL arena"))?;
            }
            count += 1;
            self.advance()?;
            if !self.eat_op(",")? {
                break;
            }
            if !allow_list {
                return Err(self.syntax_error_here());
            }
            if !name.eq_ignore_ascii_case("search_path") && !name.eq_ignore_ascii_case("datestyle")
            {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(
                        PARSE_MESSAGE_BYTES,
                        "parameter \"{}\" requires a single value",
                        name
                    ),
                    sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                    cursor: None,
                    hint: None,
                    near: None,
                    detail: None,
                });
            }
        }
        Ok(RoutineConfigValue::Value(
            core::str::from_utf8(bytes.as_slice()).expect("decoded configuration value is UTF-8"),
        ))
    }

    fn time_zone_setting_value(&mut self) -> Result<&'a str, ParseError> {
        let start = self.peek_at;
        if (self.eat_op("+")? || self.eat_op("-")?) && !matches!(self.peeked, Tok::Num(_)) {
            return Err(self.syntax_error_here());
        }
        match self.peeked {
            Tok::Ident(_) | Tok::QuotedIdent(_) | Tok::Str(_) | Tok::Num(_) => {
                self.advance()?;
                Ok(self.text[start..self.peek_at].trim())
            }
            _ => Err(self.syntax_error_here()),
        }
    }

    // --- token helpers ---

    fn record_namespace(&self, source: NamespaceSource) -> Result<(), ParseError> {
        if let Some(sources) = &self.namespace_sources {
            sources
                .borrow_mut()
                .namespaces
                .push(source)
                .map_err(|_| self.arena_full("namespace source map exceeds SQL arena"))?;
        }
        Ok(())
    }

    fn record_string_literal(
        &self,
        expression: &'a Expr<'a>,
        source: NamespaceSource,
    ) -> Result<(), ParseError> {
        if let Some(sources) = &self.type_input_sources {
            let original = match expression {
                Expr::BoundLiteral { source, .. } => *source,
                _ => expression,
            };
            sources
                .borrow_mut()
                .strings
                .push((original, source))
                .map_err(|_| self.arena_full("type input source map exceeds SQL arena"))?;
        }
        if let Some(sources) = &self.namespace_sources {
            sources
                .borrow_mut()
                .string_literals
                .push((expression, source))
                .map_err(|_| self.arena_full("literal source map exceeds SQL arena"))?;
        }
        Ok(())
    }

    fn record_catalog_literal(
        &self,
        expression: &'a Expr<'a>,
        kind: CatalogLiteralKind,
    ) -> Result<(), ParseError> {
        if let Some(sources) = &self.namespace_sources
            && let Expr::Str(value) = expression
        {
            let source = sources.borrow().string_literals.as_slice().iter().find_map(
                |(candidate, source)| core::ptr::eq(*candidate, expression).then_some(*source),
            );
            let Some(source) = source else {
                return Ok(());
            };
            let value = self
                .arena
                .alloc_str(value)
                .map_err(|_| self.arena_full("bound literal source exceeds SQL arena"))?;
            sources
                .borrow_mut()
                .catalog_literals
                .push(CatalogLiteralSource {
                    kind,
                    position: source,
                    value,
                })
                .map_err(|_| self.arena_full("bound literal source map exceeds SQL arena"))?;
        }
        Ok(())
    }

    fn advance(&mut self) -> Result<(), ParseError> {
        self.previous_token = Some((self.peeked, self.peek_at, self.lexer.token_end()));
        self.peeked = self
            .lexer
            .next_token()
            .map_err(|error| ParseError::lexical(error, self.text))?;
        self.peek_at = self.lexer.token_start();
        self.note_token_source()
    }

    /// Looks past the current token without consuming it.  The only caller
    /// resolves PostgreSQL's `REVOKE SET` grammar ambiguity: `SET OPTION FOR`
    /// is role membership syntax; every other `SET` starts an object or
    /// parameter privilege list.
    fn next_token_is_ident(&mut self, word: &str) -> Result<bool, ParseError> {
        let mark = self.lexer.mark();
        let next = self
            .lexer
            .next_token()
            .map_err(|error| ParseError::lexical(error, self.text))?;
        self.lexer.reset(mark);
        Ok(next == Tok::Ident(word))
    }

    /// Whether the current identifier starts a call: `lower(a)`.
    fn peek_is_function_call(&mut self) -> Result<bool, ParseError> {
        if !matches!(self.peeked, Tok::Ident(_) | Tok::QuotedIdent(_)) {
            return Ok(false);
        }
        let mark = self.lexer.mark();
        let mut next = self
            .lexer
            .next_token()
            .map_err(|error| ParseError::lexical(error, self.text))?;
        // A qualified name: `schema.function(`.
        while next == Tok::Op(".") {
            let name = self
                .lexer
                .next_token()
                .map_err(|error| ParseError::lexical(error, self.text))?;
            if !matches!(name, Tok::Ident(_) | Tok::QuotedIdent(_)) {
                break;
            }
            next = self
                .lexer
                .next_token()
                .map_err(|error| ParseError::lexical(error, self.text))?;
        }
        self.lexer.reset(mark);
        Ok(next == Tok::Op("("))
    }

    fn eat_op(&mut self, operator: &str) -> Result<bool, ParseError> {
        if self.peeked == Tok::Op(operator) {
            self.advance()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn expect_op(&mut self, operator: &str) -> Result<(), ParseError> {
        if !self.eat_op(operator)? {
            return Err(self.syntax_error_here());
        }
        Ok(())
    }

    fn eat_ident(&mut self, word: &str) -> Result<bool, ParseError> {
        if self.peeked == Tok::Ident(word) {
            self.advance()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn expect_ident(&mut self, word: &str) -> Result<(), ParseError> {
        if !self.eat_ident(word)? {
            return Err(self.syntax_error_here());
        }
        Ok(())
    }

    /// [`Self::any_ident`] for a `ColId` position — a column name, a table name,
    /// or a bare alias. An unquoted keyword PostgreSQL rejects there is a syntax
    /// error; quoting it (`"select"`) always makes it a plain identifier.
    fn col_ident(&mut self, what: &str) -> Result<&'a str, ParseError> {
        if let Tok::Ident(word) = self.peeked
            && is_column_name_keyword(word)
        {
            return Err(self.syntax_error_here());
        }
        self.any_ident(what)
    }

    fn type_function_ident(&mut self, what: &str) -> Result<&'a str, ParseError> {
        if let Tok::Ident(word) = self.peeked
            && is_type_function_name_keyword(word)
        {
            return Err(self.syntax_error_here());
        }
        self.any_ident(what)
    }

    /// Records a notice for the engine to emit before this statement runs.
    fn notice(&mut self, notice: ParseNotice<'a>) -> Result<(), ParseError> {
        self.warnings
            .push(notice)
            .map_err(|_| self.arena_full("statement too large for SQL arena"))
    }

    /// Takes the notices raised since the last call, in the order parsed.
    /// The returned slice stays valid: the replacement list starts empty, so
    /// later notices allocate fresh arena storage.
    pub fn take_warnings(&mut self) -> &'a [ParseNotice<'a>] {
        let taken = core::mem::replace(&mut self.warnings, ArenaList::new(self.arena));
        taken.as_slice()
    }

    /// Unquoted or quoted identifier.
    /// `_what` names the expected item for readers; PostgreSQL's error names
    /// only the token found.
    fn any_ident(&mut self, _what: &str) -> Result<&'a str, ParseError> {
        match self.peeked {
            Tok::Ident(name) | Tok::QuotedIdent(name) => {
                self.advance()?;
                Ok(name)
            }
            _ => Err(self.syntax_error_here()),
        }
    }

    /// A column reference of four or more names after `catalog.schema.table`:
    /// `database.schema.table.column` must name the session's database, and a
    /// longer reference is improper, as in PostgreSQL's parse analysis.
    pub(super) fn catalog_qualified_column(
        &mut self,
        at: usize,
        catalog: &'a str,
        schema: &'a str,
        table: &'a str,
    ) -> Result<&'a Expr<'a>, ParseError> {
        let mut written = stack_format!(PARSE_MESSAGE_BYTES, "{catalog}.{schema}.{table}");
        let mut last = None;
        let mut parts = 3;
        while self.eat_op(".")? {
            let part = if self.eat_op("*")? {
                "*"
            } else {
                self.any_ident("column name")?
            };
            let _ = core::fmt::Write::write_fmt(&mut written, format_args!(".{part}"));
            last = Some(part);
            parts += 1;
            if part == "*" {
                break;
            }
        }
        let error = |sqlstate: &'static str, message: core::fmt::Arguments<'_>| ParseError {
            at,
            message: stack_format!(PARSE_MESSAGE_BYTES, "{}", message),
            sqlstate,
            cursor: Some(at),
            hint: None,
            near: None,
            detail: None,
        };
        if parts > 4 {
            self.analysis_error(error(
                sqlstate::SYNTAX_ERROR,
                format_args!("improper qualified name (too many dotted names): {written}"),
            ))?;
            return self.arena_expr(Expr::Null);
        }
        if !crate::sql::eval::funcs::system::is_current_database(catalog) {
            self.analysis_error(error(
                sqlstate::FEATURE_NOT_SUPPORTED,
                format_args!("cross-database references are not implemented: {written}"),
            ))?;
            return self.arena_expr(Expr::Null);
        }
        match last.expect("a fourth name was read") {
            "*" => {
                let qualifier = self.composed_qualifier(schema, table)?;
                self.arena_expr(Expr::WholeRow(qualifier))
            }
            name => self.arena_expr(Expr::SchemaColumn {
                schema,
                table,
                name,
            }),
        }
    }

    /// `schema.table` composed into one arena string — the qualifier form a
    /// three-part star (`schema.table.*`) resolves through.
    pub(super) fn composed_qualifier(
        &self,
        schema: &str,
        table: &str,
    ) -> Result<&'a str, ParseError> {
        let text = crate::stack_format!(130, "{}.{}", schema, table);
        self.arena.alloc_str(text.as_str()).map_err(|_| ParseError {
            at: self.peek_at,
            message: crate::stack_format!(PARSE_MESSAGE_BYTES, "statement too large for SQL arena"),
            sqlstate: crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
            cursor: None,
            hint: None,
            near: None,
            detail: None,
        })
    }

    /// An expression node whose error position is `at`, the start of its
    /// written text (a scalar subquery's opening parenthesis).
    pub(super) fn arena_expr_at(&self, e: Expr<'a>, at: usize) -> Result<&'a Expr<'a>, ParseError> {
        let expression = self
            .arena
            .alloc(e)
            .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
        if let Some(copies) = &self.source_copies {
            let mut copies = copies.borrow_mut();
            if copies
                .push(crate::sql::error_position::SourceCopy::node(expression, at))
                .is_err()
            {
                return Err(self.arena_full("statement too large for SQL arena"));
            }
            // Rewrites copy a sublink node but share its query, which keeps
            // the sublink's position.
            if let Some(select) = crate::sql::error_position::sublink_query(expression)
                && copies
                    .push(crate::sql::error_position::SourceCopy::query(select, at))
                    .is_err()
            {
                return Err(self.arena_full("statement too large for SQL arena"));
            }
        }
        Ok(expression)
    }

    /// Records where the `WHEN` comparing `value` with a simple `CASE`'s
    /// operand was written.
    pub(super) fn record_case_when(&self, value: &Expr<'a>, at: usize) -> Result<(), ParseError> {
        if let Some(copies) = &self.source_copies
            && copies
                .borrow_mut()
                .push(crate::sql::error_position::SourceCopy::case_when(value, at))
                .is_err()
        {
            return Err(self.arena_full("statement too large for SQL arena"));
        }
        Ok(())
    }

    fn arena_expr(&self, e: Expr<'a>) -> Result<&'a Expr<'a>, ParseError> {
        // A constant keeps no source text; its node records where its token
        // was written, so an error about it can point there.
        if matches!(
            e,
            Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) | Expr::Null | Expr::Param(_)
        ) && let Some((_, start, _)) = self.previous_token
            && let Some(copies) = &self.source_copies
        {
            let expression = self
                .arena
                .alloc(e)
                .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
            let node = crate::sql::error_position::SourceCopy::node(expression, start);
            if copies.borrow_mut().push(node).is_err() {
                return Err(self.arena_full("statement too large for SQL arena"));
            }
            return Ok(expression);
        }
        if let Some(sources) = &self.column_sources
            && let Expr::Column { name, .. } | Expr::SchemaColumn { name, .. } = e
            && let Some((Tok::Ident(token) | Tok::QuotedIdent(token), start, end)) =
                self.previous_token
            && token == name
        {
            sources
                .borrow_mut()
                .columns
                .push(ColumnSource {
                    name,
                    start,
                    end,
                    qualifier_parts: match e {
                        Expr::Column {
                            qualifier: Some(_), ..
                        } => 1,
                        Expr::SchemaColumn { .. } => 2,
                        _ => 0,
                    },
                })
                .map_err(|_| self.arena_full("column source map exceeds SQL arena"))?;
        }
        if let Expr::Field { base, field } = e
            && let Expr::Param(index) = base
            && field != "*"
        {
            self.parameter_fields
                .borrow_mut()
                .push((*index, field))
                .map_err(|_| self.arena_full("parameter field list exceeds SQL arena"))?;
        }
        let expression = self
            .arena
            .alloc(e)
            .map(|m| &*m)
            .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
        if let Some(sources) = &self.type_input_sources
            && let Expr::Cast {
                operand,
                type_name,
                type_mod,
                ..
            } = expression
            && let Expr::Str(_) = operand
        {
            let source =
                sources
                    .borrow()
                    .strings
                    .as_slice()
                    .iter()
                    .find_map(|(candidate, source)| {
                        core::ptr::eq(*candidate, *operand).then_some(*source)
                    });
            if let Some(source) = source {
                let end = self.previous_token.map_or(source.end, |(_, _, end)| end);
                let key = (
                    source.start,
                    end,
                    super::type_inputs::InputKind::ExplicitCast,
                );
                if let Ok(index) = self.type_inputs.binary_search_by_key(&key, |binding| {
                    (binding.start, binding.end, binding.kind)
                }) {
                    let binding = self.type_inputs[index];
                    if binding.type_mod != *type_mod {
                        return Err(self.err_here("bound type input has a different type modifier"));
                    }
                    if !sources
                        .borrow()
                        .restored
                        .as_slice()
                        .iter()
                        .any(|(_, prior)| (prior.start, prior.end, prior.kind) == key)
                    {
                        sources
                            .borrow_mut()
                            .restored
                            .push((
                                super::type_inputs::Candidate {
                                    start: source.start,
                                    end,
                                    target: super::type_inputs::CandidateTarget::TypeName(
                                        type_name,
                                    ),
                                    kind: super::type_inputs::InputKind::ExplicitCast,
                                    type_mod: *type_mod,
                                    expression,
                                },
                                binding,
                            ))
                            .map_err(|_| {
                                self.arena_full("type input source map exceeds SQL arena")
                            })?;
                    }
                    return self
                        .arena
                        .alloc(Expr::BoundLiteral {
                            value: crate::sql::ast::BoundLiteralValue::CheckedArrayInput(
                                binding.encoded,
                            ),
                            type_oid: binding.type_oid,
                            type_mod: binding.type_mod,
                            collation: binding.collation,
                            source: expression,
                        })
                        .map(|expression| &*expression)
                        .map_err(|_| self.arena_full("bound type input exceeds SQL arena"));
                }
                sources
                    .borrow_mut()
                    .candidates
                    .push(super::type_inputs::Candidate {
                        start: source.start,
                        end,
                        target: super::type_inputs::CandidateTarget::TypeName(type_name),
                        kind: super::type_inputs::InputKind::ExplicitCast,
                        type_mod: *type_mod,
                        expression,
                    })
                    .map_err(|_| self.arena_full("type input source map exceeds SQL arena"))?;
            }
        }
        Ok(expression)
    }

    /// Parses a flat, deduplicated grouping-expression list plus PostgreSQL's
    /// expanded grouping sets. Set bitmaps grow with the expression list and
    /// the expanded set count has PostgreSQL's exact 4,096 boundary.
    fn group_by_clause(
        &mut self,
    ) -> Result<(&'a [&'a Expr<'a>], &'a [GroupingSet<'a>]), ParseError> {
        let mut flat = ArenaList::new(self.arena);
        let mut acc = ArenaList::new(self.arena);
        self.push(&mut acc, GroupingSet::EMPTY)?;
        let mut explicit = false;
        loop {
            let mut elem = ArenaList::new(self.arena);
            if self.peeked == Tok::Ident("rollup") || self.peeked == Tok::Ident("cube") {
                let is_cube = self.peeked == Tok::Ident("cube");
                self.advance()?;
                self.expect_op("(")?;
                let terms = self.grouping_term_list(&mut flat)?;
                self.expect_op(")")?;
                self.expand_grouping_terms(is_cube, terms.as_slice(), &mut elem)?;
                explicit = true;
            } else if self.peeked == Tok::Ident("grouping") {
                self.advance()?;
                self.expect_ident("sets")?;
                self.expect_op("(")?;
                loop {
                    self.grouping_set_member(&mut flat, &mut elem)?;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                explicit = true;
            } else {
                let set = self.grouping_term(&mut flat)?;
                self.push(&mut elem, set)?;
            }

            let product = acc
                .len()
                .checked_mul(elem.len())
                .ok_or_else(|| self.grouping_sets_limit())?;
            if product > MAX_GROUPING_SETS {
                return Err(self.grouping_sets_limit());
            }
            let mut combined = ArenaList::new(self.arena);
            for &left in acc.as_slice() {
                for &right in elem.as_slice() {
                    let set = self.union_grouping_sets(left, right)?;
                    self.push(&mut combined, set)?;
                }
            }
            acc = combined;
            if !self.eat_op(",")? {
                break;
            }
        }
        let grouping_sets = if explicit { acc.as_slice() } else { &[] };
        Ok((flat.as_slice(), grouping_sets))
    }

    fn grouping_set_from_words(
        &self,
        words: &[u64; MAX_GROUP_WORDS],
    ) -> Result<GroupingSet<'a>, ParseError> {
        let len = words
            .iter()
            .rposition(|word| *word != 0)
            .map_or(0, |index| index + 1);
        if len == 0 {
            Ok(GroupingSet::EMPTY)
        } else {
            Ok(GroupingSet {
                words: self.arena_slice(&words[..len])?,
            })
        }
    }

    fn union_grouping_sets(
        &self,
        left: GroupingSet<'a>,
        right: GroupingSet<'a>,
    ) -> Result<GroupingSet<'a>, ParseError> {
        let mut words = [0u64; MAX_GROUP_WORDS];
        for (index, word) in words.iter_mut().enumerate() {
            *word = left.words.get(index).copied().unwrap_or(0)
                | right.words.get(index).copied().unwrap_or(0);
        }
        self.grouping_set_from_words(&words)
    }

    fn combine_grouping_sets(
        &self,
        sets: &[GroupingSet<'a>],
    ) -> Result<GroupingSet<'a>, ParseError> {
        let mut words = [0u64; MAX_GROUP_WORDS];
        for set in sets {
            for (target, &word) in words.iter_mut().zip(set.words) {
                *target |= word;
            }
        }
        self.grouping_set_from_words(&words)
    }

    /// Interns a grouping expression and returns its flat-list index.
    fn intern_group(
        &self,
        flat: &mut ArenaList<'a, &'a Expr<'a>>,
        expression: &'a Expr<'a>,
    ) -> Result<usize, ParseError> {
        if let Some(index) = flat
            .as_slice()
            .iter()
            .position(|existing| **existing == *expression)
        {
            return Ok(index);
        }
        if flat.len() == MAX_GROUP_TERMS {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(
                    PARSE_MESSAGE_BYTES,
                    "target lists can have at most 1664 entries"
                ),
                sqlstate: sqlstate::TOO_MANY_COLUMNS,
                cursor: None,
                hint: None,
                near: None,
                detail: None,
            });
        }
        let index = flat.len();
        self.push(flat, expression)?;
        Ok(index)
    }

    /// Parses one bare or parenthesized compound grouping term.
    fn grouping_term(
        &mut self,
        flat: &mut ArenaList<'a, &'a Expr<'a>>,
    ) -> Result<GroupingSet<'a>, ParseError> {
        let mut words = [0u64; MAX_GROUP_WORDS];
        if self.peeked == Tok::Op("(") && self.paren_is_group_list()? {
            self.advance()?;
            if self.peeked != Tok::Op(")") {
                loop {
                    let expression = self.expression(0)?;
                    let index = self.intern_group(flat, expression)?;
                    words[index / 64] |= 1u64 << (index % 64);
                    if !self.eat_op(",")? {
                        break;
                    }
                }
            }
            self.expect_op(")")?;
        } else {
            let expression = self.expression(0)?;
            let index = self.intern_group(flat, expression)?;
            words[index / 64] |= 1u64 << (index % 64);
        }
        self.grouping_set_from_words(&words)
    }

    fn grouping_term_list(
        &mut self,
        flat: &mut ArenaList<'a, &'a Expr<'a>>,
    ) -> Result<ArenaList<'a, GroupingSet<'a>>, ParseError> {
        let mut terms = ArenaList::new(self.arena);
        loop {
            let term = self.grouping_term(flat)?;
            self.push(&mut terms, term)?;
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(terms)
    }

    fn expand_grouping_terms(
        &self,
        cube: bool,
        terms: &[GroupingSet<'a>],
        output: &mut ArenaList<'a, GroupingSet<'a>>,
    ) -> Result<(), ParseError> {
        if cube {
            if terms.len() > MAX_CUBE_TERMS {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(PARSE_MESSAGE_BYTES, "CUBE is limited to 12 elements"),
                    sqlstate: sqlstate::TOO_MANY_COLUMNS,
                    cursor: None,
                    hint: None,
                    near: None,
                    detail: None,
                });
            }
            for subset in 0usize..(1usize << terms.len()) {
                let mut words = [0u64; MAX_GROUP_WORDS];
                for (index, term) in terms.iter().enumerate() {
                    if subset & (1usize << index) != 0 {
                        for (target, &word) in words.iter_mut().zip(term.words) {
                            *target |= word;
                        }
                    }
                }
                let set = self.grouping_set_from_words(&words)?;
                self.push(output, set)?;
            }
        } else {
            for keep in (0..=terms.len()).rev() {
                let set = self.combine_grouping_sets(&terms[..keep])?;
                self.push(output, set)?;
            }
        }
        Ok(())
    }

    /// Parses one member of a `GROUPING SETS (...)` list.
    fn grouping_set_member(
        &mut self,
        flat: &mut ArenaList<'a, &'a Expr<'a>>,
        output: &mut ArenaList<'a, GroupingSet<'a>>,
    ) -> Result<(), ParseError> {
        if self.peeked == Tok::Ident("rollup") || self.peeked == Tok::Ident("cube") {
            let is_cube = self.peeked == Tok::Ident("cube");
            self.advance()?;
            self.expect_op("(")?;
            let terms = self.grouping_term_list(flat)?;
            self.expect_op(")")?;
            self.expand_grouping_terms(is_cube, terms.as_slice(), output)
        } else {
            let set = self.grouping_term(flat)?;
            if output.len() == MAX_GROUPING_SETS {
                return Err(self.grouping_sets_limit());
            }
            self.push(output, set)
        }
    }

    fn grouping_sets_limit(&self) -> ParseError {
        ParseError {
            at: self.peek_at,
            message: stack_format!(
                PARSE_MESSAGE_BYTES,
                "too many grouping sets present (maximum 4096)"
            ),
            sqlstate: sqlstate::STATEMENT_TOO_COMPLEX,
            cursor: None,
            hint: None,
            near: None,
            detail: None,
        }
    }

    /// With `(` peeked at grouping-term position, reports whether it opens a
    /// multi-column grouping list — `()` or `(a, b, ...)` — as opposed to a
    /// scalar parenthesized expression like `(a + b)` or `(x + 1) * 2`. It
    /// Whether the token after the peeked one is `::` — a cloned-lexer
    /// lookahead, used to keep a unary minus from folding into a literal the
    /// cast binds tighter to.
    pub(super) fn next_is_cast(&self) -> Result<bool, ParseError> {
        let mut lexer = self.lexer.clone();
        Ok(matches!(
            lexer
                .next_token()
                .map_err(|error| ParseError::lexical(error, self.text))?,
            Tok::Op("::")
        ))
    }

    /// scans a cloned lexer to the matching close paren: a top-level comma is
    /// never valid inside a scalar `( ... )`, so seeing one (or an immediate
    /// close, the empty grand-total level) unambiguously marks a grouping list.
    fn paren_is_group_list(&self) -> Result<bool, ParseError> {
        let mut lexer = self.lexer.clone();
        let mut depth = 1usize; // the peeked `(` is already consumed by the real lexer
        let mut tokens = 0usize;
        loop {
            let tok = lexer
                .next_token()
                .map_err(|error| ParseError::lexical(error, self.text))?;
            tokens += 1;
            match tok {
                Tok::Op("(") | Tok::Op("[") => depth += 1,
                Tok::Op(")") | Tok::Op("]") => {
                    depth -= 1;
                    // Matching close with no top-level comma: an empty list
                    // `()` (the first token closed it) is a grand-total level;
                    // anything else was a scalar `( ... )`.
                    if depth == 0 {
                        return Ok(tokens == 1);
                    }
                }
                Tok::Op(",") if depth == 1 => return Ok(true),
                Tok::Eof => return Ok(false),
                _ => {}
            }
        }
    }

    fn arena_slice<T: Copy>(&self, items: &[T]) -> Result<&'a [T], ParseError> {
        self.arena
            .alloc_slice_copy(items)
            .map(|m| &*m)
            .map_err(|_| self.arena_full("statement too large for SQL arena"))
    }

    /// Grows an arena-backed list, mapping arena exhaustion to a loud parse
    /// error. Statement lists use this rather than fixed stack staging, so
    /// only the statement's own arena bounds their width.
    fn push<T: Copy + 'a>(&self, list: &mut ArenaList<'a, T>, value: T) -> Result<(), ParseError> {
        list.push(value)
            .map_err(|_| self.arena_full("statement too large for SQL arena"))
    }

    fn arena_str(&self, s: &str) -> Result<&'a str, ParseError> {
        self.arena
            .alloc_str(s)
            .map_err(|_| self.arena_full("statement too large for SQL arena"))
    }

    /// PostgreSQL's generic grammar failure, naming the token where parsing
    /// stopped. Every failed expectation reports this form.
    fn syntax_error_here(&self) -> ParseError {
        if self.peeked == Tok::Eof {
            ParseError {
                at: self.peek_at,
                cursor: Some(self.peek_at),
                hint: None,
                message: StackStr::from_str("syntax error at end of input"),
                near: None,
                detail: None,
                sqlstate: sqlstate::SYNTAX_ERROR,
            }
        } else {
            self.syntax_error_near(self.peek_at, self.lexer.token_end())
        }
    }

    /// The grammar failure at an already consumed token spanning
    /// `start..end`, echoed as the client wrote it.
    fn syntax_error_near(&self, start: usize, end: usize) -> ParseError {
        ParseError {
            at: start,
            cursor: Some(start),
            hint: None,
            message: stack_format!(
                PARSE_MESSAGE_BYTES,
                "syntax error at or near \"{}\"",
                &self.text[start..end]
            ),
            near: Some((start, end)),
            detail: None,
            sqlstate: sqlstate::SYNTAX_ERROR,
        }
    }

    /// PostgreSQL's column NULL/NOT NULL conflict, at the second constraint.
    fn conflicting_null_declarations(&self, at: usize, column: &str, table: &str) -> ParseError {
        ParseError {
            at,
            cursor: Some(at),
            hint: None,
            message: stack_format!(
                PARSE_MESSAGE_BYTES,
                "conflicting NULL/NOT NULL declarations for column \"{}\" of table \"{}\"",
                column,
                table
            ),
            sqlstate: sqlstate::SYNTAX_ERROR,
            near: None,
            detail: None,
        }
    }

    /// PostgreSQL's domain NULL/NOT NULL conflict, at the second constraint.
    fn conflicting_domain_nullability(&self, at: usize) -> ParseError {
        ParseError {
            at,
            cursor: Some(at),
            hint: None,
            message: StackStr::from_str("conflicting NULL/NOT NULL constraints"),
            sqlstate: sqlstate::SYNTAX_ERROR,
            near: None,
            detail: None,
        }
    }

    fn err_here(&self, message: &'static str) -> ParseError {
        ParseError::new(self.peek_at, message)
    }

    /// A CREATE FUNCTION definition error that PostgreSQL raises after
    /// parsing, with `42P13` and no cursor position.
    fn invalid_function_definition(&self, message: &'static str) -> ParseError {
        ParseError {
            at: self.peek_at,
            message: StackStr::from_str(message),
            sqlstate: sqlstate::INVALID_FUNCTION_DEFINITION,
            cursor: None,
            hint: None,
            near: None,
            detail: None,
        }
    }

    /// Statement-arena exhaustion is a resource limit, not a syntax error.
    fn arena_full(&self, message: &'static str) -> ParseError {
        ParseError {
            at: self.peek_at,
            message: stack_format!(PARSE_MESSAGE_BYTES, "{}", message),
            sqlstate: sqlstate::PROGRAM_LIMIT_EXCEEDED,
            cursor: None,
            hint: None,
            near: None,
            detail: None,
        }
    }

    fn limit(&self, what: &'static str, max: usize) -> ParseError {
        ParseError {
            at: self.peek_at,
            message: stack_format!(
                PARSE_MESSAGE_BYTES,
                "{} exceeds fixed limit of {}",
                what,
                max
            ),
            sqlstate: sqlstate::PROGRAM_LIMIT_EXCEEDED,
            cursor: None,
            hint: None,
            near: None,
            detail: None,
        }
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;
    use crate::mem::Budget;

    #[test]
    fn schema_sql_rewrite_sources_distinguish_namespaces_from_aliases_and_comments() {
        let mut budget = crate::mem::budget::Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "namespace source test", 1 << 20).unwrap();
        let sql = "SELECT old.items.id, old.id, old.score(old.id)::old.kind COLLATE old.ordering FROM old /* namespace */ .items AS old WHERE old.id OPERATOR(old.=) 1";
        let expected = [
            "old.items.id",
            "old.score",
            "old.kind",
            "old.ordering",
            "old /* namespace */",
            "old.=",
        ]
        .map(|name| sql.find(name).unwrap());
        crate::mem::guard::forbid_alloc(|| {
            let sources = schema_namespace_sources(sql, StoredSqlKind::Statements, &arena).unwrap();
            assert_eq!(sources.namespaces.len(), expected.len());
            assert!(sources.catalog_literals.is_empty());
            for source in sources.namespaces {
                assert!(expected.contains(&source.start));
                assert_eq!(&sql[source.start..source.end], "old");
            }
        });
    }

    /// The pre-arena parser's fixed list boundary; tests cross it to prove
    /// the boundary is gone.
    const FORMER_LIST_LIMIT: usize = 64;

    fn with_parser<R>(text: &str, f: impl FnOnce(&mut Parser) -> R) -> R {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "test", 1 << 18).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut p = Parser::new(text, &arena).unwrap();
            f(&mut p)
        })
    }

    #[test]
    fn column_storage_alter_consumes_the_policy() {
        with_parser(
            "ALTER TABLE storage_probe ALTER COLUMN value SET STORAGE EXTENDED",
            |parser| {
                let statement = parser.statement().unwrap();
                assert!(matches!(statement, Stmt::AlterTable(_)));
                assert_eq!(parser.peeked, Tok::Eof);
            },
        );
        with_parser(
            "ALTER TABLE storage_probe ALTER COLUMN value SET STORAGE EXTENDED; \
             ALTER TABLE storage_probe ALTER COLUMN value SET COMPRESSION DEFAULT; \
             ALTER TABLE storage_probe ALTER COLUMN value SET COMPRESSION pglz; \
             ALTER TABLE storage_probe ALTER COLUMN value SET STORAGE DEFAULT",
            |parser| {
                for _ in 0..4 {
                    assert!(matches!(
                        parser.next_stmt().unwrap(),
                        Some(Stmt::AlterTable(_))
                    ));
                }
                assert!(parser.next_stmt().unwrap().is_none());
            },
        );
        with_parser(
            "CREATE TABLE storage_clone (LIKE storage_probe INCLUDING STORAGE INCLUDING COMPRESSION)",
            |parser| {
                let Some(Stmt::CreateTable(table)) = parser.next_stmt().unwrap() else {
                    panic!("LIKE table definition did not parse")
                };
                assert_eq!(table.likes.len(), 1);
                assert!(table.likes[0].storage);
                assert!(table.likes[0].compression);
            },
        );
        with_parser(
            "CREATE TABLE storage_clone (LIKE storage_probe INCLUDING COMMENTS INCLUDING STATISTICS)",
            |parser| {
                let Some(Stmt::CreateTable(table)) = parser.next_stmt().unwrap() else {
                    panic!("LIKE catalog metadata did not parse")
                };
                assert!(table.likes[0].comments);
                assert!(table.likes[0].statistics);
            },
        );
        with_parser(
            "CREATE TABLE storage_clone (LIKE storage_probe INCLUDING ALL EXCLUDING DEFAULTS)",
            |parser| {
                let Some(Stmt::CreateTable(table)) = parser.next_stmt().unwrap() else {
                    panic!("LIKE INCLUDING ALL did not parse")
                };
                assert!(!table.likes[0].defaults);
                assert!(table.likes[0].storage);
                assert!(table.likes[0].compression);
                assert!(table.likes[0].comments);
                assert!(table.likes[0].statistics);
            },
        );
    }

    #[test]
    fn alter_table_control_plane_forms_are_closed_parse_states() {
        with_parser(
            "ALTER TABLE control_rows CLUSTER ON control_rows_id; \
             ALTER TABLE control_rows SET WITHOUT CLUSTER; \
             ALTER TABLE control_rows SET WITHOUT OIDS; \
             ALTER TABLE control_rows ADD COLUMN IF NOT EXISTS label text; \
             ALTER TABLE control_rows OF control_row; \
             ALTER TABLE control_rows NOT OF; \
             ALTER TABLE control_rows SET ACCESS METHOD DEFAULT; \
             ALTER TABLE ALL IN TABLESPACE old_space OWNED BY postgres \
               SET TABLESPACE new_space NOWAIT",
            |parser| {
                let Some(Stmt::AlterTable(cluster)) = parser.next_stmt().unwrap() else {
                    panic!("CLUSTER ON did not parse as ALTER TABLE")
                };
                assert!(matches!(cluster.actions, [AlterAction::SetCluster(_)]));
                let Some(Stmt::AlterTable(clear)) = parser.next_stmt().unwrap() else {
                    panic!("SET WITHOUT CLUSTER did not parse as ALTER TABLE")
                };
                assert!(matches!(clear.actions, [AlterAction::ClearCluster]));
                let Some(Stmt::AlterTable(oids)) = parser.next_stmt().unwrap() else {
                    panic!("SET WITHOUT OIDS did not parse as ALTER TABLE")
                };
                assert!(matches!(oids.actions, [AlterAction::SetWithoutOids]));
                let Some(Stmt::AlterTable(add)) = parser.next_stmt().unwrap() else {
                    panic!("ADD COLUMN IF NOT EXISTS did not parse as ALTER TABLE")
                };
                assert!(matches!(
                    add.actions,
                    [AlterAction::AddColumn {
                        if_not_exists: true,
                        ..
                    }]
                ));
                let Some(Stmt::AlterTable(of)) = parser.next_stmt().unwrap() else {
                    panic!("OF did not parse as ALTER TABLE")
                };
                assert!(matches!(
                    of.actions,
                    [AlterAction::SetTypeMembership(Some(_))]
                ));
                let Some(Stmt::AlterTable(not_of)) = parser.next_stmt().unwrap() else {
                    panic!("NOT OF did not parse as ALTER TABLE")
                };
                assert!(matches!(
                    not_of.actions,
                    [AlterAction::SetTypeMembership(None)]
                ));
                let Some(Stmt::AlterTable(default_access_method)) = parser.next_stmt().unwrap()
                else {
                    panic!("SET ACCESS METHOD DEFAULT did not parse as ALTER TABLE")
                };
                assert!(matches!(
                    default_access_method.actions,
                    [AlterAction::ResetAccessMethod]
                ));
                let Some(Stmt::AlterTablesTablespace {
                    source,
                    owners,
                    target,
                    nowait,
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("ALTER TABLE ALL IN TABLESPACE did not have a closed parse state")
                };
                assert_eq!(source, "old_space");
                assert_eq!(owners, ["postgres"]);
                assert_eq!(target, "new_space");
                assert!(nowait);
            },
        );
    }

    #[test]
    fn column_statistics_options_are_typed_and_bounded() {
        with_parser(
            "ALTER TABLE statistics_rows ALTER COLUMN value \
             SET (n_distinct = 7, n_distinct_inherited = -0.5); \
             ALTER TABLE statistics_rows ALTER COLUMN value \
             RESET (n_distinct, n_distinct_inherited)",
            |parser| {
                for _ in 0..2 {
                    assert!(matches!(
                        parser.next_stmt().unwrap(),
                        Some(Stmt::AlterTable(_))
                    ));
                }
                assert!(parser.next_stmt().unwrap().is_none());
            },
        );
        with_parser(
            "ALTER TABLE statistics_rows ALTER COLUMN value SET (n_distinct = -1.1)",
            |parser| assert!(parser.statement().is_err()),
        );
        with_parser(
            "ALTER TABLE statistics_rows ALTER COLUMN value SET (unknown = 1)",
            |parser| assert!(parser.statement().is_err()),
        );
    }

    #[test]
    fn select_literals_with_aliases() {
        with_parser("SELECT 1, 'x' AS name, 2.5 half", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(s.items.len(), 3);
            let SelectItem::Expr { expression, alias } = s.items[1] else {
                panic!()
            };
            assert_eq!(*expression, Expr::Str("x"));
            assert_eq!(alias, Some("name"));
            let SelectItem::Expr { alias, .. } = s.items[2] else {
                panic!()
            };
            assert_eq!(alias, Some("half"));
            assert!(p.next_stmt().unwrap().is_none());
        });
    }

    #[test]
    fn foreign_data_ddl_is_typed_without_runtime_allocation() {
        with_parser(
            "CREATE FOREIGN DATA WRAPPER fdw HANDLER postgres_fdw_handler \
                 VALIDATOR postgres_fdw_validator OPTIONS (extensions 'hstore'); \
             CREATE SERVER upstream TYPE 'postgresql' VERSION '18' \
                 FOREIGN DATA WRAPPER fdw OPTIONS (host 'db'); \
             CREATE USER MAPPING FOR CURRENT_USER SERVER upstream OPTIONS (user 'remote'); \
             CREATE FOREIGN TABLE remote_items \
                 (id integer OPTIONS (column_name 'remote_id')) \
                 SERVER upstream OPTIONS (schema_name 'public'); \
             ALTER FOREIGN DATA WRAPPER fdw OPTIONS (SET extensions 'citext'); \
             ALTER SERVER upstream VERSION NULL OPTIONS (ADD dbname 'postgres'); \
             ALTER USER MAPPING FOR CURRENT_USER SERVER upstream OPTIONS (SET user 'changed'); \
             ALTER FOREIGN TABLE remote_items OPTIONS (SET schema_name 'changed'), \
                 ALTER COLUMN id OPTIONS (SET column_name 'id2'); \
             IMPORT FOREIGN SCHEMA public EXCEPT (skip_me) \
                 FROM SERVER upstream INTO public OPTIONS (import_default 'true'); \
             DROP FOREIGN TABLE remote_items; \
             DROP USER MAPPING FOR CURRENT_USER SERVER upstream; \
             DROP SERVER upstream; \
             DROP FOREIGN DATA WRAPPER fdw",
            |parser| {
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::CreateForeignDataWrapper(_))
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::CreateForeignServer(_))
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::CreateUserMapping(_))
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::CreateForeignTable(_))
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::AlterForeignDataWrapper { .. })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::AlterForeignServer { .. })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::AlterUserMapping(_))
                ));
                let Some(Stmt::AlterForeignTable(alter)) = parser.next_stmt().unwrap() else {
                    panic!("expected typed ALTER FOREIGN TABLE")
                };
                assert!(
                    alter
                        .actions
                        .iter()
                        .any(|action| matches!(action, AlterAction::SetForeignOptions(_)))
                );
                assert!(alter.actions.iter().any(|action| matches!(
                    action,
                    AlterAction::SetColumnForeignOptions { column: "id", .. }
                )));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::ImportForeignSchema(_))
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::DropForeignTable(_))
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::DropUserMapping(_))
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::DropForeignServer { .. })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::DropForeignDataWrapper { .. })
                ));
                assert!(parser.next_stmt().unwrap().is_none());
            },
        );
        with_parser(
            "ALTER FOREIGN TABLE remote_items SERVER upstream",
            |parser| {
                assert!(parser.next_stmt().is_err());
            },
        );
    }

    #[test]
    fn pg_dump_can_lock_a_complete_bounded_catalog() {
        let mut sql = String::from("LOCK TABLE ");
        for index in 0..FORMER_LIST_LIMIT {
            if index != 0 {
                sql.push_str(", ");
            }
            sql.push_str(&format!("dump_relation_{index}"));
        }
        sql.push_str(" IN ACCESS SHARE MODE");
        with_parser(&sql, |parser| {
            let Stmt::LockTable { tables, mode, .. } = parser.next_stmt().unwrap().unwrap() else {
                panic!("expected LOCK TABLE")
            };
            assert_eq!(tables.len(), FORMER_LIST_LIMIT);
            assert_eq!(mode, TableLockMode::AccessShare);
        });
    }

    #[test]
    fn composite_star_keeps_its_structural_position() {
        with_parser(
            "SELECT (NULL::address).*, ROW((NULL::address).*, 42)::text",
            |parser| {
                let Stmt::Select(select) = parser.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert!(matches!(select.items[0], SelectItem::RecordStar(_)));
                let SelectItem::Expr {
                    expression:
                        Expr::Cast {
                            operand:
                                Expr::Call {
                                    name: "row", args, ..
                                },
                            ..
                        },
                    ..
                } = select.items[1]
                else {
                    panic!()
                };
                assert!(matches!(args[0], Expr::Field { field: "*", .. }));
            },
        );
    }

    #[test]
    fn quantified_subqueries_are_typed_at_the_parse_boundary() {
        with_parser(
            "SELECT ROW(1, 2) < ANY (SELECT 1, 3), 1 = ANY (SELECT 1)",
            |p| {
                let Stmt::Select(select) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                let SelectItem::Expr { expression, .. } = select.items[0] else {
                    panic!()
                };
                assert!(matches!(
                    expression,
                    Expr::QuantifiedSubquery {
                        operator: BinaryOp::Lt,
                        all: false,
                        ..
                    }
                ));
                let SelectItem::Expr { expression, .. } = select.items[1] else {
                    panic!()
                };
                assert!(matches!(
                    expression,
                    Expr::InSubquery { negated: false, .. }
                ));
            },
        );
    }

    #[test]
    fn cte_materialization_and_query_bodies_are_typed_at_parse_time() {
        with_parser(
            "WITH a AS MATERIALIZED (SELECT 1), \
                  b AS NOT MATERIALIZED (SELECT * FROM a) \
             SELECT * FROM b",
            |parser| {
                let Stmt::Select(select) = parser.next_stmt().unwrap().unwrap() else {
                    panic!("expected WITH query")
                };
                assert_eq!(
                    select.with[0].materialization,
                    crate::sql::ast::CteMaterialization::Materialized
                );
                assert_eq!(
                    select.with[1].materialization,
                    crate::sql::ast::CteMaterialization::NotMaterialized
                );
            },
        );
        for query in [
            "CREATE TABLE copied AS WITH value AS (SELECT 1 AS id) SELECT id FROM value",
            "CREATE VIEW copied_view AS WITH value AS (SELECT 1 AS id) SELECT id FROM value",
            "COPY (WITH value AS (SELECT 1 AS id) SELECT id FROM value) TO STDOUT",
            "DECLARE copied_cursor CURSOR FOR WITH value AS (SELECT 1 AS id) SELECT id FROM value",
        ] {
            with_parser(query, |parser| {
                assert!(parser.next_stmt().unwrap().is_some(), "rejected {query}")
            });
        }
    }

    #[test]
    fn partition_declarations_are_typed_at_the_parse_boundary() {
        with_parser(
            "CREATE TABLE sales (sold_on date, amount int) PARTITION BY RANGE (sold_on); \
             CREATE TABLE sales_2026 PARTITION OF sales FOR VALUES FROM ('2026-01-01') TO ('2027-01-01'); \
             CREATE TABLE sales_other PARTITION OF sales DEFAULT",
            |p| {
                let Stmt::CreateTable(parent) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert!(
                    matches!(parent.partition, PartitionClause::By { strategy: PartitionStrategy::Range, columns } if columns == ["sold_on"])
                );
                let Stmt::CreateTable(child) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert!(
                    matches!(child.partition, PartitionClause::Of { bound: PartitionBound::Range { from, to }, .. } if from.len() == 1 && to.len() == 1)
                );
                let Stmt::CreateTable(default) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert!(matches!(
                    default.partition,
                    PartitionClause::Of {
                        bound: PartitionBound::Default,
                        ..
                    }
                ));
            },
        );
    }

    #[test]
    fn table_lifecycle_boundaries_are_closed_parse_states() {
        with_parser(
            "CREATE TABLE inherited_child (id integer) WITH (fillfactor = 80) INHERITS (parent_one, parent_two); \
             CREATE TABLE typed_child OF public.row_type; \
             ALTER TABLE inherited_child SET (fillfactor = 70); \
             ALTER TABLE inherited_child RESET (fillfactor); \
             ALTER TABLE partitioned_parent DETACH PARTITION partitioned_child CONCURRENTLY; \
             ALTER TABLE partitioned_parent DETACH PARTITION partitioned_child FINALIZE",
            |parser| {
                let Some(Stmt::CreateTable(inherited)) = parser.next_stmt().unwrap() else {
                    panic!("inheritance definition did not parse")
                };
                assert_eq!(inherited.storage_options.fillfactor, Some(80));
                assert!(matches!(
                    inherited.membership,
                    crate::sql::ast::TableMembership::Inherits(parents)
                        if parents == [QualName::bare("parent_one"), QualName::bare("parent_two")]
                ));
                let Some(Stmt::CreateTable(typed)) = parser.next_stmt().unwrap() else {
                    panic!("typed table definition did not parse")
                };
                assert!(matches!(
                    typed.membership,
                    crate::sql::ast::TableMembership::OfType(QualName {
                        schema: Some("public"),
                        name: "row_type"
                    })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::AlterTable(AlterTable { actions, .. }))
                        if matches!(actions, [AlterAction::SetStorageOptions(options)] if options.fillfactor == Some(70))
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::AlterTable(AlterTable { actions, .. }))
                        if matches!(actions, [AlterAction::ResetStorageOptions(names)] if names.fillfactor)
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::AlterTable(AlterTable { actions, .. }))
                        if matches!(actions, [AlterAction::DetachPartition { mode: crate::sql::ast::PartitionDetachMode::Concurrent, .. }])
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::AlterTable(AlterTable { actions, .. }))
                        if matches!(actions, [AlterAction::DetachPartition { mode: crate::sql::ast::PartitionDetachMode::Finalize, .. }])
                ));
            },
        );
    }

    #[test]
    fn default_partition_uses_postgresqls_bound_syntax() {
        with_parser(
            "CREATE TABLE leaf PARTITION OF parent FOR VALUES DEFAULT",
            |p| assert!(p.next_stmt().is_err()),
        );
    }

    #[test]
    fn create_schema_keeps_its_closed_element_grammar_and_role_identity_typed() {
        with_parser(
            "CREATE SCHEMA AUTHORIZATION CURRENT_ROLE \
             CREATE TABLE entries(value integer) \
             CREATE TRIGGER audit AFTER INSERT ON entries FOR EACH ROW \
               EXECUTE FUNCTION public.audit_fn() \
             GRANT SELECT ON TABLE entries TO public",
            |parser| {
                let Some(Stmt::CreateSchema {
                    name,
                    authorization,
                    elements,
                    ..
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("CREATE SCHEMA did not retain its typed statement")
                };
                assert_eq!(name, SchemaName::Authorization);
                assert_eq!(authorization, Some(SchemaAuthorization::CurrentRole));
                assert!(matches!(
                    elements,
                    [
                        CreateSchemaElement::Table(_),
                        CreateSchemaElement::Trigger(_),
                        CreateSchemaElement::Grant { .. },
                    ]
                ));
            },
        );

        with_parser(
            "CREATE SCHEMA invalid CREATE DOMAIN value AS integer",
            |parser| assert!(parser.next_stmt().is_err()),
        );
        with_parser(
            "CREATE SCHEMA IF NOT EXISTS invalid CREATE TABLE value(id integer)",
            |parser| {
                assert_eq!(
                    parser.next_stmt().unwrap_err().sqlstate,
                    sqlstate::FEATURE_NOT_SUPPORTED
                );
            },
        );
    }

    #[test]
    fn aggregate_extension_dependencies_are_rejected_at_parse_time() {
        for input in [
            "ALTER AGGREGATE public.total(integer) DEPENDS ON EXTENSION reporting",
            "ALTER AGGREGATE public.total(integer) NO DEPENDS ON EXTENSION reporting",
        ] {
            with_parser(input, |parser| {
                assert_eq!(
                    parser.next_stmt().unwrap_err().sqlstate,
                    sqlstate::SYNTAX_ERROR
                );
            });
        }
    }

    #[test]
    fn publication_options_parse_without_heap_allocation() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "publication parser", 1 << 18).unwrap();
        let mut parser = Parser::new(
            "CREATE PUBLICATION changes FOR ALL TABLES WITH (publish = 'INSERT, Update, delete')",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::CreatePublication {
                all_tables,
                publish,
                ..
            }) = parser.next_stmt().unwrap()
            else {
                panic!("publication statement did not parse")
            };
            assert!(all_tables);
            assert!(publish.insert && publish.update && publish.delete);
            assert!(!publish.truncate);
        });
    }

    #[test]
    fn duplicate_publication_options_fail_at_the_parse_boundary() {
        for sql in [
            "CREATE PUBLICATION changes WITH (publish = 'insert', publish = 'update')",
            "CREATE PUBLICATION changes WITH (publish_via_partition_root = true, publish_via_partition_root = false)",
            "ALTER PUBLICATION changes SET (publish = 'insert', publish = 'delete')",
            "ALTER PUBLICATION changes SET (publish_via_partition_root = true, publish_via_partition_root = false)",
        ] {
            let mut budget = Budget::new(1 << 20);
            let arena = Arena::new(&mut budget, "publication parser", 1 << 18).unwrap();
            let mut parser = Parser::new(sql, &arena).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                let error = parser.next_stmt().unwrap_err();
                assert_eq!(error.message.as_str(), "conflicting or redundant options");
            });
        }
    }

    #[test]
    fn cluster_metadata_escape_errors_preserve_postgresql_sqlstates() {
        for (sql, state) in [
            (r"SELECT E'\000'", sqlstate::CHARACTER_NOT_IN_REPERTOIRE),
            (r"SELECT E'\xff'", sqlstate::CHARACTER_NOT_IN_REPERTOIRE),
            (
                r"PREPARE TRANSACTION E'\x00'",
                sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
            ),
            (r"COMMIT PREPARED E'\u0'", sqlstate::INVALID_ESCAPE_SEQUENCE),
            (r"ROLLBACK PREPARED E'\U00110000'", sqlstate::SYNTAX_ERROR),
            (r"SELECT E'\uD800'", sqlstate::SYNTAX_ERROR),
        ] {
            let mut budget = Budget::new(1 << 20);
            let arena = Arena::new(&mut budget, "escape parser", 1 << 16).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                let mut parser = Parser::new(sql, &arena).unwrap();
                assert_eq!(parser.next_stmt().unwrap_err().sqlstate, state);
            });
        }
    }

    #[test]
    fn empty_publication_parse_without_heap_allocation() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "empty publication parser", 1 << 18).unwrap();
        let mut parser = Parser::new("CREATE PUBLICATION changes", &arena).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::CreatePublication {
                all_tables, tables, ..
            }) = parser.next_stmt().unwrap()
            else {
                panic!("empty CREATE PUBLICATION did not parse")
            };
            assert!(!all_tables);
            assert!(tables.is_empty());
        });
    }

    #[test]
    fn alter_publication_is_a_typed_operation_without_allocation() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "alter publication parser", 1 << 18).unwrap();
        let mut parser = Parser::new(
            "ALTER PUBLICATION changes ADD TABLE public.orders, archive.orders",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::AlterPublication { name, action }) = parser.next_stmt().unwrap() else {
                panic!("ALTER PUBLICATION did not parse")
            };
            assert_eq!(name, "changes");
            let crate::sql::ast::AlterPublicationAction::AddTargets { tables, schemas } = action
            else {
                panic!("membership action lost its operation")
            };
            assert_eq!(tables.len(), 2);
            assert!(schemas.is_empty());
            assert_eq!(tables[0].relation.schema, Some("public"));
            assert_eq!(tables[1].relation.schema, Some("archive"));
        });
    }

    #[test]
    fn publication_column_lists_remain_attached_to_their_relation_without_allocation() {
        with_parser(
            "CREATE PUBLICATION changes FOR TABLE public.orders (id, total) WHERE (id > 0), archive.orders",
            |parser| {
                let Some(Stmt::CreatePublication { tables, .. }) = parser.next_stmt().unwrap()
                else {
                    panic!("publication column list did not parse")
                };
                assert_eq!(tables.len(), 2);
                assert_eq!(tables[0].relation.schema, Some("public"));
                assert_eq!(tables[0].relation.name, "orders");
                assert_eq!(tables[0].columns, ["id", "total"]);
                assert_eq!(tables[0].filter_text, Some("id > 0"));
                assert!(matches!(tables[0].filter, Some(Expr::Binary { .. })));
                assert!(tables[1].columns.is_empty());
                assert!(tables[1].filter.is_none());
            },
        );
    }

    #[test]
    fn publication_descendant_selection_is_typed_at_the_parse_boundary() {
        with_parser(
            "CREATE PUBLICATION changes FOR TABLE parent, ONLY isolated, explicit *",
            |parser| {
                let Some(Stmt::CreatePublication { tables, .. }) = parser.next_stmt().unwrap()
                else {
                    panic!("publication descendants did not parse")
                };
                assert!(matches!(
                    tables[0].descendants,
                    crate::sql::ast::PublicationDescendants::Include
                ));
                assert!(matches!(
                    tables[1].descendants,
                    crate::sql::ast::PublicationDescendants::Only
                ));
                assert!(matches!(
                    tables[2].descendants,
                    crate::sql::ast::PublicationDescendants::Include
                ));
            },
        );
        for source in [
            "CREATE PUBLICATION changes FOR TABLE ONLY parent *",
            "ALTER PUBLICATION changes DROP TABLE parent (id)",
            "ALTER PUBLICATION changes DROP TABLE parent WHERE (id > 0)",
        ] {
            let mut budget = Budget::new(1 << 20);
            let arena =
                Arena::new(&mut budget, "invalid publication descendants", 1 << 18).unwrap();
            let mut parser = Parser::new(source, &arena).unwrap();
            assert!(parser.next_stmt().is_err(), "{source}");
        }
    }

    #[test]
    fn alter_publication_set_table_does_not_consume_the_set_keyword_twice() {
        with_parser("ALTER PUBLICATION changes SET TABLE orders", |parser| {
            let Some(Stmt::AlterPublication { action, .. }) = parser.next_stmt().unwrap() else {
                panic!("SET TABLE did not parse")
            };
            let crate::sql::ast::AlterPublicationAction::SetTargets { tables, schemas } = action
            else {
                panic!("SET TABLE parsed as another ALTER PUBLICATION action")
            };
            assert_eq!(tables[0].relation, QualName::bare("orders"));
            assert!(tables[0].columns.is_empty());
            assert!(schemas.is_empty());
        });
    }

    #[test]
    fn alter_subscription_definition_operations_are_typed_without_allocation() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "alter subscription parser", 1 << 18).unwrap();
        let mut parser = Parser::new(
            "ALTER SUBSCRIPTION apply_changes CONNECTION 'host=127.0.0.2 port=5432'; \
             ALTER SUBSCRIPTION apply_changes SET PUBLICATION sales, inventory WITH (refresh = false)",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::AlterSubscription { action, .. }) = parser.next_stmt().unwrap() else {
                panic!("ALTER SUBSCRIPTION CONNECTION did not parse")
            };
            assert_eq!(
                action,
                crate::sql::ast::AlterSubscriptionAction::SetConnection("host=127.0.0.2 port=5432")
            );
            let Some(Stmt::AlterSubscription { action, .. }) = parser.next_stmt().unwrap() else {
                panic!("ALTER SUBSCRIPTION SET PUBLICATION did not parse")
            };
            let crate::sql::ast::AlterSubscriptionAction::SetPublications {
                publications,
                refresh,
            } = action
            else {
                panic!("SET PUBLICATION lost its typed publication list")
            };
            assert_eq!(publications, ["sales", "inventory"]);
            assert_eq!(
                refresh,
                crate::sql::ast::SubscriptionPublicationRefresh::NoRefresh
            );
        });
    }

    #[test]
    fn alter_type_set_schema_is_a_typed_operation_without_allocation() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "alter type parser", 1 << 18).unwrap();
        let mut parser = Parser::new("ALTER TYPE public.state SET SCHEMA archive", &arena).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::AlterType { name, action }) = parser.next_stmt().unwrap() else {
                panic!("ALTER TYPE did not parse")
            };
            assert_eq!(
                name,
                QualName {
                    schema: Some("public"),
                    name: "state"
                }
            );
            assert_eq!(
                action,
                crate::sql::ast::AlterTypeAction::SetSchema("archive")
            );
        });
    }

    #[test]
    fn alter_materialized_view_lifecycle_is_typed_without_runtime_allocation() {
        with_parser(
            "ALTER MATERIALIZED VIEW IF EXISTS public.snapshot RENAME TO archive; \
             ALTER MATERIALIZED VIEW public.archive SET SCHEMA reporting; \
             ALTER MATERIALIZED VIEW reporting.archive SET TABLESPACE cold_store",
            |parser| {
                let Some(Stmt::AlterMaterializedView {
                    name,
                    if_exists,
                    action,
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("ALTER MATERIALIZED VIEW RENAME did not parse")
                };
                assert_eq!(
                    name,
                    QualName {
                        schema: Some("public"),
                        name: "snapshot"
                    }
                );
                assert!(if_exists);
                assert_eq!(
                    action,
                    crate::sql::ast::AlterMaterializedViewAction::RenameTo("archive")
                );
                let Some(Stmt::AlterMaterializedView { action, .. }) = parser.next_stmt().unwrap()
                else {
                    panic!("ALTER MATERIALIZED VIEW SET SCHEMA did not parse")
                };
                assert_eq!(
                    action,
                    crate::sql::ast::AlterMaterializedViewAction::SetSchema("reporting")
                );
                let Some(Stmt::AlterMaterializedView { action, .. }) = parser.next_stmt().unwrap()
                else {
                    panic!("ALTER MATERIALIZED VIEW SET TABLESPACE did not parse")
                };
                assert_eq!(
                    action,
                    crate::sql::ast::AlterMaterializedViewAction::TableActions(&[
                        crate::sql::ast::AlterAction::SetTablespace("cold_store")
                    ])
                );
            },
        );
        with_parser(
            "ALTER MATERIALIZED VIEW snapshot ALTER COLUMN value SET STATISTICS 77, \
             SET (fillfactor = 90), RESET (fillfactor)",
            |parser| {
                let Some(Stmt::AlterMaterializedView { action, .. }) = parser.next_stmt().unwrap()
                else {
                    panic!("ALTER MATERIALIZED VIEW metadata actions did not parse")
                };
                let crate::sql::ast::AlterMaterializedViewAction::TableActions(actions) = action
                else {
                    panic!("ALTER MATERIALIZED VIEW metadata lost its typed action list")
                };
                assert_eq!(actions.len(), 3);
                assert!(matches!(
                    actions[0],
                    crate::sql::ast::AlterAction::SetStatistics {
                        column: "value",
                        target: 77
                    }
                ));
                assert!(matches!(
                    actions[1],
                    crate::sql::ast::AlterAction::SetStorageOptions(options)
                    if options.fillfactor == Some(90)
                ));
                assert!(matches!(
                    actions[2],
                    crate::sql::ast::AlterAction::ResetStorageOptions(options)
                    if options.fillfactor
                ));
            },
        );
    }

    #[test]
    fn cluster_target_is_closed_and_allocation_free() {
        with_parser(
            "CLUSTER VERBOSE public.rows USING public.rows_id; CLUSTER",
            |parser| {
                let Some(Stmt::Cluster { target, verbose }) = parser.next_stmt().unwrap() else {
                    panic!("CLUSTER target did not parse")
                };
                assert!(verbose);
                assert_eq!(
                    target,
                    crate::sql::ast::ClusterTarget::Table {
                        table: QualName {
                            schema: Some("public"),
                            name: "rows",
                        },
                        index: Some(QualName {
                            schema: Some("public"),
                            name: "rows_id",
                        }),
                    }
                );
                let Some(Stmt::Cluster { target, verbose }) = parser.next_stmt().unwrap() else {
                    panic!("CLUSTER all did not parse")
                };
                assert!(!verbose);
                assert_eq!(target, crate::sql::ast::ClusterTarget::All);
            },
        );
    }

    #[test]
    fn row_trigger_lifecycle_is_typed_without_allocation() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "trigger parser", 1 << 18).unwrap();
        let mut parser = Parser::new(
            "CREATE TRIGGER audit_change BEFORE INSERT OR UPDATE OR DELETE ON public.orders \
             FOR EACH ROW EXECUTE FUNCTION audit_row('audit'); \
             ALTER TABLE public.orders DISABLE TRIGGER audit_change; \
             DROP TRIGGER IF EXISTS audit_change ON public.orders",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::CreateTrigger(trigger)) = parser.next_stmt().unwrap() else {
                panic!("CREATE TRIGGER did not parse")
            };
            assert_eq!(trigger.name, "audit_change");
            assert_eq!(trigger.timing, crate::sql::ast::TriggerTiming::Before);
            assert_eq!(
                trigger.events,
                [
                    crate::sql::ast::TriggerEvent::Insert,
                    crate::sql::ast::TriggerEvent::Update,
                    crate::sql::ast::TriggerEvent::Delete,
                ]
            );
            assert_eq!(
                trigger.table,
                QualName {
                    schema: Some("public"),
                    name: "orders"
                }
            );
            assert_eq!(trigger.function, QualName::bare("audit_row"));
            assert_eq!(trigger.arguments, ["audit"]);
            let Some(Stmt::AlterTable(alter)) = parser.next_stmt().unwrap() else {
                panic!("ALTER TABLE trigger mode did not parse")
            };
            assert_eq!(
                alter.table,
                QualName {
                    schema: Some("public"),
                    name: "orders"
                }
            );
            assert_eq!(
                alter.actions,
                [crate::sql::ast::AlterAction::SetTriggerEnabled {
                    target: crate::sql::ast::TriggerEnableTarget::Name("audit_change"),
                    enabled: crate::sql::ast::TriggerEnableMode::Disabled,
                }]
            );
            let Some(Stmt::DropTrigger {
                trigger,
                if_exists,
                cascade,
            }) = parser.next_stmt().unwrap()
            else {
                panic!("DROP TRIGGER did not parse")
            };
            assert!(if_exists);
            assert!(!cascade);
            assert_eq!(
                trigger,
                crate::sql::ast::TriggerIdentity {
                    name: "audit_change",
                    table: QualName {
                        schema: Some("public"),
                        name: "orders"
                    },
                }
            );
        });
    }

    #[test]
    fn event_trigger_lifecycle_is_typed_without_allocation() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "event trigger parser", 1 << 18).unwrap();
        let mut parser = Parser::new(
            "CREATE EVENT TRIGGER audit_ddl ON ddl_command_end \
             WHEN TAG IN ('CREATE TABLE', 'ALTER TABLE') \
             EXECUTE FUNCTION public.audit_ddl(); \
             ALTER EVENT TRIGGER audit_ddl ENABLE REPLICA; \
             DROP EVENT TRIGGER IF EXISTS audit_ddl CASCADE",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::CreateEventTrigger(trigger)) = parser.next_stmt().unwrap() else {
                panic!("CREATE EVENT TRIGGER did not parse")
            };
            assert_eq!(trigger.name, "audit_ddl");
            assert_eq!(
                trigger.event,
                crate::sql::ast::EventTriggerEvent::DdlCommandEnd
            );
            assert_eq!(trigger.tags, ["CREATE TABLE", "ALTER TABLE"]);
            assert_eq!(
                trigger.function,
                QualName {
                    schema: Some("public"),
                    name: "audit_ddl",
                }
            );
            let Some(Stmt::AlterEventTrigger { name, action }) = parser.next_stmt().unwrap() else {
                panic!("ALTER EVENT TRIGGER did not parse")
            };
            assert_eq!(name, "audit_ddl");
            assert_eq!(
                action,
                crate::sql::ast::AlterEventTriggerAction::SetEnabled(
                    crate::sql::ast::TriggerEnableMode::Replica
                )
            );
            let Some(Stmt::DropEventTrigger {
                name,
                if_exists,
                cascade,
            }) = parser.next_stmt().unwrap()
            else {
                panic!("DROP EVENT TRIGGER did not parse")
            };
            assert_eq!(name, "audit_ddl");
            assert!(if_exists);
            assert!(cascade);
        });
    }

    #[test]
    fn table_producing_ddl_keeps_its_postgresql_command_kind() {
        with_parser(
            "CREATE TABLE from_query AS SELECT 1; \
             CREATE MATERIALIZED VIEW materialized_query USING heap WITH (fillfactor = 75) AS SELECT 1; \
             CREATE UNLOGGED MATERIALIZED VIEW unlogged_materialized_query AS SELECT 1; \
             SELECT 1 INTO selected_query",
            |parser| {
                let Some(Stmt::CreateTableAs { kind, .. }) = parser.next_stmt().unwrap() else {
                    panic!("CREATE TABLE AS did not parse")
                };
                assert_eq!(kind, crate::sql::ast::CreateTableAsKind::Table);
                let Some(Stmt::CreateTableAs { kind, options, .. }) = parser.next_stmt().unwrap()
                else {
                    panic!("CREATE MATERIALIZED VIEW did not parse")
                };
                assert_eq!(kind, crate::sql::ast::CreateTableAsKind::MaterializedView);
                assert_eq!(options.storage_options.fillfactor, Some(75));
                let Some(Stmt::CreateTableAs {
                    kind, persistence, ..
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("CREATE UNLOGGED MATERIALIZED VIEW did not parse")
                };
                assert_eq!(kind, crate::sql::ast::CreateTableAsKind::MaterializedView);
                assert_eq!(persistence, crate::sql::ast::RelationPersistence::Unlogged);
                let Some(Stmt::CreateTableAs { kind, .. }) = parser.next_stmt().unwrap() else {
                    panic!("SELECT INTO did not parse")
                };
                assert_eq!(kind, crate::sql::ast::CreateTableAsKind::SelectInto);
            },
        );
    }

    #[test]
    fn instead_of_view_trigger_is_typed_and_illegal_forms_fail_at_parse_boundary() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "instead of trigger parser", 1 << 18).unwrap();
        let mut parser = Parser::new(
            "CREATE TRIGGER write_view INSTEAD OF INSERT OR UPDATE OR DELETE ON public.orders_view \
             FOR EACH ROW EXECUTE FUNCTION write_orders()",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::CreateTrigger(trigger)) = parser.next_stmt().unwrap() else {
                panic!("INSTEAD OF trigger did not parse")
            };
            assert_eq!(trigger.timing, crate::sql::ast::TriggerTiming::InsteadOf);
            assert_eq!(trigger.level, crate::sql::ast::TriggerLevel::Row);
        });
        for source in [
            "CREATE TRIGGER bad INSTEAD OF INSERT ON orders_view FOR EACH STATEMENT EXECUTE FUNCTION f()",
            "CREATE TRIGGER bad INSTEAD OF TRUNCATE ON orders_view FOR EACH ROW EXECUTE FUNCTION f()",
            "CREATE TRIGGER bad INSTEAD OF UPDATE OF id ON orders_view FOR EACH ROW EXECUTE FUNCTION f()",
        ] {
            let mut budget = Budget::new(1 << 20);
            let arena = Arena::new(&mut budget, "invalid instead of trigger", 1 << 18).unwrap();
            let mut parser = Parser::new(source, &arena).unwrap();
            assert!(parser.next_stmt().is_err(), "{source}");
        }
    }

    #[test]
    fn transition_table_trigger_is_typed_and_illegal_forms_fail_at_parse_boundary() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "transition trigger parser", 1 << 18).unwrap();
        let mut parser = Parser::new(
            "CREATE TRIGGER audit_change AFTER UPDATE ON public.orders \
             REFERENCING OLD TABLE AS old_orders NEW TABLE AS new_orders \
             FOR EACH STATEMENT EXECUTE FUNCTION audit_statement()",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::CreateTrigger(trigger)) = parser.next_stmt().unwrap() else {
                panic!("CREATE TRIGGER did not parse")
            };
            assert_eq!(
                trigger.transition_tables,
                crate::sql::ast::TriggerTransitionTables::OldNew {
                    old: "old_orders",
                    new: "new_orders",
                }
            );
        });
        for source in [
            "CREATE TRIGGER bad BEFORE UPDATE ON orders REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION f()",
            "CREATE TRIGGER bad AFTER INSERT ON orders REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION f()",
            "CREATE TRIGGER bad AFTER UPDATE ON orders REFERENCING OLD TABLE AS rows NEW TABLE AS rows FOR EACH STATEMENT EXECUTE FUNCTION f()",
            "CREATE TRIGGER bad AFTER UPDATE OF amount ON orders REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION f()",
        ] {
            let mut budget = Budget::new(1 << 20);
            let arena =
                Arena::new(&mut budget, "invalid transition trigger parser", 1 << 18).unwrap();
            let mut parser = Parser::new(source, &arena).unwrap();
            assert!(parser.next_stmt().is_err(), "{source}");
        }
    }

    #[test]
    fn publication_schema_targets_are_typed_without_allocation() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "publication schema parser", 1 << 18).unwrap();
        let mut parser = Parser::new(
            "CREATE PUBLICATION changes FOR TABLE public.orders, TABLES IN SCHEMA archive, current_schema()",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let Some(Stmt::CreatePublication {
                tables, schemas, ..
            }) = parser.next_stmt().unwrap()
            else {
                panic!("publication schema targets did not parse")
            };
            assert_eq!(tables[0].relation.schema, Some("public"));
            assert_eq!(tables[0].relation.name, "orders");
            assert!(tables[0].columns.is_empty());
            assert_eq!(schemas, ["archive", "public"]);
        });
    }

    #[test]
    fn publication_owner_change_is_typed_without_allocation() {
        with_parser(
            "ALTER PUBLICATION changes OWNER TO replication_owner",
            |parser| {
                let Some(Stmt::AlterPublication { action, .. }) = parser.next_stmt().unwrap()
                else {
                    panic!("publication owner change did not parse")
                };
                assert_eq!(
                    action,
                    crate::sql::ast::AlterPublicationAction::SetOwner("replication_owner")
                );
            },
        );
    }

    #[test]
    fn role_membership_options_are_typed_as_patches() {
        with_parser(
            "GRANT parent TO child WITH ADMIN FALSE, INHERIT FALSE, SET TRUE GRANTED BY manager; \
             REVOKE INHERIT OPTION FOR parent FROM child GRANTED BY manager CASCADE; \
             ALTER ROLE child IN DATABASE postgres SET search_path TO app, public; \
             ALTER ROLE ALL IN DATABASE postgres RESET ALL; \
             GRANT SELECT (id, payload), UPDATE (payload) ON orders TO child",
            |parser| {
                let Some(Stmt::GrantRole {
                    options, grantor, ..
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("role grant did not parse")
                };
                assert_eq!(options.admin, Some(false));
                assert_eq!(options.inherit, Some(false));
                assert_eq!(options.set, Some(true));
                assert_eq!(grantor, Some("manager"));

                let Some(Stmt::RevokeRole {
                    option,
                    grantor,
                    cascade,
                    ..
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("role revoke did not parse")
                };
                assert_eq!(option, Some(crate::sql::ast::RoleMembershipOption::Inherit));
                assert_eq!(grantor, Some("manager"));
                assert!(cascade);

                let Some(Stmt::AlterRoleSetting {
                    role,
                    database,
                    action: crate::sql::ast::RoleSettingAction::Set { name, value },
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("role setting did not parse")
                };
                assert_eq!(
                    role,
                    Some(crate::sql::ast::RoleSpecification::Name("child"))
                );
                assert_eq!(database, Some("postgres"));
                assert_eq!(name, "search_path");
                assert_eq!(
                    value,
                    crate::sql::ast::RoutineConfigValue::Value("app, public")
                );

                let Some(Stmt::AlterRoleSetting {
                    role: None,
                    database: Some("postgres"),
                    action: crate::sql::ast::RoleSettingAction::Reset(None),
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("all-role setting reset did not parse")
                };

                let Some(Stmt::GrantPrivileges { privileges, .. }) = parser.next_stmt().unwrap()
                else {
                    panic!("column privileges did not parse")
                };
                assert_eq!(privileges.len(), 2);
                assert_eq!(privileges[0].privilege, crate::sql::ast::Privilege::Select);
                assert_eq!(privileges[0].columns, ["id", "payload"]);
                assert_eq!(privileges[1].privilege, crate::sql::ast::Privilege::Update);
                assert_eq!(privileges[1].columns, ["payload"]);
            },
        );
        with_parser("GRANT DELETE (id) ON orders TO child", |parser| {
            assert!(parser.next_stmt().is_err());
        });
    }

    #[test]
    fn legacy_alter_group_is_typed_as_role_membership() {
        with_parser(
            "ALTER GROUP parent ADD USER child, other; \
             ALTER GROUP parent DROP USER child; \
             ALTER GROUP parent RENAME TO renamed",
            |parser| {
                let Some(Stmt::GrantRole {
                    syntax,
                    roles,
                    members,
                    options,
                    grantor,
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("ALTER GROUP ADD USER did not parse")
                };
                assert_eq!(syntax, crate::sql::ast::RoleMembershipSyntax::AlterGroup);
                assert_eq!(roles, ["parent"]);
                assert_eq!(members, ["child", "other"]);
                assert_eq!(options, crate::sql::ast::RoleMembershipPatch::EMPTY);
                assert_eq!(grantor, None);

                let Some(Stmt::RevokeRole {
                    syntax,
                    roles,
                    members,
                    option,
                    grantor,
                    cascade,
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("ALTER GROUP DROP USER did not parse")
                };
                assert_eq!(syntax, crate::sql::ast::RoleMembershipSyntax::AlterGroup);
                assert_eq!(roles, ["parent"]);
                assert_eq!(members, ["child"]);
                assert_eq!(option, None);
                assert_eq!(grantor, None);
                assert!(!cascade);

                let Some(Stmt::AlterRoleRename { role, new_name }) = parser.next_stmt().unwrap()
                else {
                    panic!("ALTER GROUP RENAME did not parse")
                };
                assert_eq!(
                    (role, new_name),
                    (
                        crate::sql::ast::RoleSpecification::Name("parent"),
                        "renamed"
                    )
                );
            },
        );
        with_parser("ALTER GROUP parent NOLOGIN", |parser| {
            assert!(parser.next_stmt().is_err());
        });
    }

    #[test]
    fn role_aliases_and_special_targets_are_closed_parse_states() {
        with_parser(
            "CREATE GROUP bundle IN GROUP parent USER member ADMIN administrator; \
             ALTER ROLE CURRENT_ROLE SET application_name TO role_default; \
             ALTER USER SESSION_USER RESET application_name; \
             ALTER ROLE \"current_user\" NOLOGIN; \
             ALTER ROLE \"all\" LOGIN",
            |parser| {
                let Some(Stmt::CreateRole { memberships, .. }) = parser.next_stmt().unwrap() else {
                    panic!("CREATE GROUP aliases did not parse")
                };
                assert_eq!(memberships.in_roles, ["parent"]);
                assert_eq!(memberships.role_members, ["member"]);
                assert_eq!(memberships.admin_members, ["administrator"]);

                let Some(Stmt::AlterRoleSetting {
                    role: Some(crate::sql::ast::RoleSpecification::CurrentRole),
                    ..
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("CURRENT_ROLE did not parse as a role specification")
                };
                let Some(Stmt::AlterRoleSetting {
                    role: Some(crate::sql::ast::RoleSpecification::SessionUser),
                    ..
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("SESSION_USER did not parse as a role specification")
                };
                let Some(Stmt::AlterRole {
                    role: crate::sql::ast::RoleSpecification::Name("current_user"),
                    ..
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("quoted current_user did not remain a named role")
                };
                let Some(Stmt::AlterRole {
                    role: crate::sql::ast::RoleSpecification::Name("all"),
                    ..
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("quoted all did not remain a named role")
                };
            },
        );
    }

    #[test]
    fn role_password_forms_are_typed_at_the_parse_boundary() {
        let server = crate::pg::auth::ScramServer::derive("imported-password", [9; 16], 4096);
        let mut salt = crate::util::StackStr::<512>::new();
        let mut stored_key = crate::util::StackStr::<512>::new();
        let mut server_key = crate::util::StackStr::<512>::new();
        crate::pg::auth::b64_encode(server.salt.as_bytes(), &mut salt);
        crate::pg::auth::b64_encode(&server.stored_key, &mut stored_key);
        crate::pg::auth::b64_encode(&server.server_key, &mut server_key);
        let verifier = format!(
            "SCRAM-SHA-256${}:{}${}:{}",
            server.iterations,
            salt.as_str(),
            stored_key.as_str(),
            server_key.as_str()
        );
        with_parser(
            &format!(
                "CREATE USER imported LOGIN ENCRYPTED PASSWORD '{verifier}'; ALTER USER imported ENCRYPTED PASSWORD '{verifier}'"
            ),
            |parser| {
                let Some(Stmt::CreateRole { options, .. }) = parser.next_stmt().unwrap() else {
                    panic!("CREATE USER did not parse")
                };
                assert!(matches!(
                    options.password,
                    Some(Some(crate::sql::ast::RolePasswordSpec::ScramVerifier(_)))
                ));
                let Some(Stmt::AlterRole { options, .. }) = parser.next_stmt().unwrap() else {
                    panic!("ALTER USER did not parse")
                };
                assert!(matches!(
                    options.password,
                    Some(Some(crate::sql::ast::RolePasswordSpec::ScramVerifier(_)))
                ));
            },
        );
        with_parser(
            "CREATE ROLE malformed PASSWORD 'SCRAM-SHA-256$4096:not-base64$bad:bad'",
            |parser| {
                let Some(Stmt::CreateRole { options, .. }) = parser.next_stmt().unwrap() else {
                    panic!("malformed verifier did not parse as plaintext")
                };
                assert!(matches!(
                    options.password,
                    Some(Some(crate::sql::ast::RolePasswordSpec::Plaintext(_)))
                ));
            },
        );
        with_parser(
            "CREATE ROLE imported_md5 PASSWORD 'md547fcb6615d41c53cd39822141eb05da2'",
            |parser| {
                let Some(Stmt::CreateRole { options, .. }) = parser.next_stmt().unwrap() else {
                    panic!("MD5 verifier did not parse")
                };
                assert_eq!(
                    options.password,
                    Some(Some(crate::sql::ast::RolePasswordSpec::Md5Verifier(
                        crate::storage::Md5Verifier {
                            hash: *b"47fcb6615d41c53cd39822141eb05da2",
                        },
                    )))
                );
            },
        );
        with_parser(
            "ALTER ROLE imported UNENCRYPTED PASSWORD 'secret'",
            |parser| {
                assert_eq!(
                    parser.next_stmt().unwrap_err().sqlstate,
                    crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED
                );
            },
        );
        let too_long_salt = [9; crate::pg::auth::SCRAM_SALT_MAX + 1];
        let mut encoded_salt = crate::util::StackStr::<512>::new();
        crate::pg::auth::b64_encode(&too_long_salt, &mut encoded_salt);
        let too_long_verifier = format!(
            "SCRAM-SHA-256$4096:{}${}:{}",
            encoded_salt.as_str(),
            stored_key.as_str(),
            server_key.as_str()
        );
        with_parser(
            &format!("CREATE ROLE oversized PASSWORD '{too_long_verifier}'"),
            |parser| {
                assert_eq!(
                    parser.next_stmt().unwrap_err().sqlstate,
                    crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED
                );
            },
        );
        with_parser("CREATE ROLE old_sysid SYSID 7", |parser| {
            let Some(Stmt::CreateRole { options, .. }) = parser.next_stmt().unwrap() else {
                panic!("obsolete SYSID did not parse")
            };
            assert_eq!(options.sysid, Some(7));
        });
        with_parser("CREATE ROLE invalid_expiry VALID UNTIL NULL", |parser| {
            assert!(parser.next_stmt().is_err());
        });
        for source in [
            "CREATE ROLE duplicate_role_options LOGIN NOLOGIN",
            "ALTER ROLE duplicate_role_options PASSWORD 'one' ENCRYPTED PASSWORD 'two'",
            "CREATE ROLE duplicate_role_options VALID UNTIL 'infinity' VALID UNTIL 'infinity'",
            "CREATE ROLE duplicate_role_options IN ROLE parent IN GROUP parent",
            "CREATE ROLE duplicate_role_options ROLE member ROLE member",
            "CREATE ROLE duplicate_role_options ADMIN member ADMIN member",
        ] {
            with_parser(source, |parser| {
                assert_eq!(
                    parser.next_stmt().unwrap_err().sqlstate,
                    crate::sql::eval::sqlstate::SYNTAX_ERROR,
                    "{source}"
                );
            });
        }
        with_parser("CREATE ROLE empty_password PASSWORD ''", |parser| {
            let Some(Stmt::CreateRole { options, .. }) = parser.next_stmt().unwrap() else {
                panic!("empty password did not parse")
            };
            assert_eq!(options.password, Some(None));
        });
    }

    #[test]
    fn publication_rename_is_typed_without_allocation() {
        with_parser(
            "ALTER PUBLICATION changes RENAME TO renamed_changes",
            |parser| {
                let Some(Stmt::AlterPublication { action, .. }) = parser.next_stmt().unwrap()
                else {
                    panic!("publication rename did not parse")
                };
                assert_eq!(
                    action,
                    crate::sql::ast::AlterPublicationAction::Rename("renamed_changes")
                );
            },
        );
    }

    #[test]
    fn parameter_privileges_are_a_closed_typed_statement() {
        with_parser(
            "GRANT SET, ALTER SYSTEM ON PARAMETER event_triggers TO operator; \
             REVOKE SET ON PARAMETER event_triggers FROM operator RESTRICT; \
             REVOKE GRANT OPTION FOR SET ON PARAMETER event_triggers FROM operator CASCADE",
            |parser| {
                let Some(Stmt::GrantParameterPrivileges {
                    privileges,
                    names,
                    grant_option,
                    ..
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("parameter GRANT did not produce a typed statement")
                };
                assert!(privileges.contains(crate::sql::ast::ParameterPrivileges::SET));
                assert!(privileges.contains(crate::sql::ast::ParameterPrivileges::ALTER_SYSTEM));
                assert_eq!(names[0].as_str(), "event_triggers");
                assert!(!grant_option);
                let Some(Stmt::RevokeParameterPrivileges {
                    grant_option_only,
                    privileges,
                    cascade,
                    ..
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("parameter REVOKE did not produce a typed statement")
                };
                assert!(!grant_option_only && !cascade);
                assert_eq!(privileges, crate::sql::ast::ParameterPrivileges::SET);
                let Some(Stmt::RevokeParameterPrivileges {
                    grant_option_only,
                    privileges,
                    cascade,
                    ..
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("parameter REVOKE did not produce a typed statement")
                };
                assert!(grant_option_only && cascade);
                assert_eq!(privileges, crate::sql::ast::ParameterPrivileges::SET);
            },
        );
    }

    #[test]
    fn index_rename_is_typed_without_allocation() {
        with_parser(
            "ALTER INDEX IF EXISTS public.old_index RENAME TO new_index",
            |parser| {
                let Some(Stmt::AlterIndex {
                    name,
                    if_exists,
                    action,
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("index rename did not parse")
                };
                assert_eq!(
                    name,
                    QualName {
                        schema: Some("public"),
                        name: "old_index"
                    }
                );
                assert!(if_exists);
                assert_eq!(
                    action,
                    crate::sql::ast::AlterIndexAction::Rename("new_index")
                );
            },
        );
    }

    #[test]
    fn operator_selectivity_options_are_typed_and_immutable_attributes_reject() {
        with_parser(
            "CREATE OPERATOR === (FUNCTION = op_same, LEFTARG = integer, RIGHTARG = integer); \
             ALTER OPERATOR === (integer, integer) SET (RESTRICT = NONE, JOIN = NONE)",
            |parser| {
                let Some(Stmt::CreateOperator(operator)) = parser.next_stmt().unwrap() else {
                    panic!("CREATE OPERATOR did not parse")
                };
                assert_eq!(operator.name.name, "===");
                let Some(Stmt::AlterOperator { action, .. }) = parser.next_stmt().unwrap() else {
                    panic!("ALTER OPERATOR did not parse")
                };
                assert_eq!(
                    action,
                    crate::sql::ast::AlterOperatorAction::ResetSelectivity(
                        crate::sql::ast::OperatorSelectivityReset {
                            restrict: true,
                            join: true,
                        }
                    )
                );
            },
        );
        for (source, expected) in [
            (
                "ALTER OPERATOR === (integer, integer) SET (COMMUTATOR = NONE)",
                "commutator",
            ),
            (
                "ALTER OPERATOR === (integer, integer) SET (NEGATOR = NONE)",
                "negator",
            ),
            (
                "ALTER OPERATOR === (integer, integer) SET (HASHES = NONE)",
                "hashes",
            ),
            (
                "ALTER OPERATOR === (integer, integer) SET (MERGES = NONE)",
                "merges",
            ),
        ] {
            with_parser(source, |parser| {
                let error = parser.next_stmt().unwrap_err();
                assert_eq!(
                    error.sqlstate,
                    crate::sql::eval::sqlstate::INVALID_OBJECT_DEFINITION
                );
                assert!(error.message.as_str().contains(expected), "{error:?}");
            });
        }
    }

    #[test]
    fn index_and_tablespace_lifecycle_is_typed_without_allocation() {
        with_parser(
            "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS value_idx ON ONLY public.items \
             (id DESC NULLS LAST, value COLLATE \"C\" text_ops) INCLUDE (payload) \
             NULLS NOT DISTINCT WITH (fillfactor=80, deduplicate_items=off) \
             TABLESPACE fast WHERE id > 0; \
             ALTER INDEX value_idx ATTACH PARTITION value_idx_p0; \
             REINDEX (VERBOSE, TABLESPACE fast, CONCURRENTLY) INDEX value_idx; \
             DROP INDEX CONCURRENTLY IF EXISTS value_idx; \
             CREATE TABLESPACE fast OWNER postgres LOCATION '/object-prefix/fast' \
             WITH (random_page_cost=0, seq_page_cost=1e300, effective_io_concurrency=8); \
             ALTER TABLESPACE fast RESET (random_page_cost, effective_io_concurrency); \
             GRANT CREATE ON TABLESPACE fast TO PUBLIC; \
             COMMENT ON TABLESPACE fast IS 'placement'; \
             DROP TABLESPACE IF EXISTS fast",
            |parser| {
                let Some(Stmt::CreateIndex {
                    build,
                    scope,
                    if_not_exists,
                    columns,
                    include_columns,
                    nulls_not_distinct,
                    options,
                    tablespace,
                    unique,
                    ..
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("CREATE INDEX did not produce a typed index command")
                };
                assert_eq!(build, IndexBuildMode::Concurrent);
                assert_eq!(scope, IndexTargetScope::Only);
                assert!(if_not_exists && nulls_not_distinct && unique);
                assert_eq!(columns.len(), 2);
                assert_eq!(columns[1].column, Some("value"));
                assert!(matches!(
                    columns[1].expression,
                    Expr::Column { name: "value", .. }
                ));
                assert!(columns[1].collation.is_some());
                assert_eq!(include_columns, ["payload"]);
                assert_eq!(options.fillfactor, Some(80));
                assert_eq!(tablespace, Some("fast"));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::AlterIndex {
                        action: AlterIndexAction::AttachPartition(_),
                        ..
                    })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::Reindex { .. })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::DropIndex {
                        build: IndexBuildMode::Concurrent,
                        ..
                    })
                ));
                let Some(Stmt::CreateTablespace { options, .. }) = parser.next_stmt().unwrap()
                else {
                    panic!("CREATE TABLESPACE did not produce typed options")
                };
                assert_eq!(options.random_page_cost.unwrap().value(), 0.0);
                assert_eq!(options.seq_page_cost.unwrap().value(), 1e300);
                assert_eq!(options.effective_io_concurrency, Some(8));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::AlterTablespace {
                        action: AlterTablespaceAction::ResetOptions(_),
                        ..
                    })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::GrantPrivileges { .. })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::Comment {
                        target: CommentTarget::Tablespace("fast"),
                        ..
                    })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::DropTablespace {
                        name: "fast",
                        if_exists: true
                    })
                ));
                assert!(parser.next_stmt().unwrap().is_none());
            },
        );
    }

    #[test]
    fn grouping_sets_expansion() {
        let low_word = |set: GroupingSet<'_>| set.words.first().copied().unwrap_or(0);
        let assert_words = |sets: &[GroupingSet<'_>], expected: &[u64]| {
            assert!(
                sets.iter()
                    .copied()
                    .map(low_word)
                    .eq(expected.iter().copied()),
                "unexpected grouping sets: {sets:?}"
            );
        };
        // Plain GROUP BY: no explicit sets, all columns implied.
        with_parser("SELECT a FROM t GROUP BY a, b", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(s.group_by.len(), 2);
            assert!(s.grouping_sets.is_empty());
            assert_eq!(s.grouping_set_quantifier, GroupingSetQuantifier::All);
        });
        with_parser(
            "SELECT a FROM t GROUP BY DISTINCT GROUPING SETS ((a), (a))",
            |p| {
                let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert_eq!(s.grouping_set_quantifier, GroupingSetQuantifier::Distinct);
                assert_words(s.grouping_sets, &[1, 1]);
            },
        );
        // ROLLUP(a, b) -> {a,b}, {a}, {} (bits index group_by = [a, b]).
        with_parser("SELECT a FROM t GROUP BY ROLLUP(a, b)", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(s.group_by.len(), 2);
            assert_words(s.grouping_sets, &[0b11, 0b01, 0b00]);
        });
        // CUBE(a, b) -> all four subsets.
        with_parser("SELECT a FROM t GROUP BY CUBE(a, b)", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(s.grouping_sets.len(), 4);
            for expected in [0b00, 0b01, 0b10, 0b11] {
                assert!(
                    s.grouping_sets
                        .iter()
                        .copied()
                        .map(low_word)
                        .any(|set| set == expected)
                );
            }
        });
        // Explicit GROUPING SETS, including the empty grand-total set.
        with_parser(
            "SELECT a FROM t GROUP BY GROUPING SETS ((a, b), (a), ())",
            |p| {
                let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert_words(s.grouping_sets, &[0b11, 0b01, 0b00]);
            },
        );
        // Cross product: a, ROLLUP(b, c) -> a always set, times {bc},{b},{}.
        with_parser("SELECT a FROM t GROUP BY a, ROLLUP(b, c)", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(s.group_by.len(), 3); // a, b, c
            assert_words(s.grouping_sets, &[0b111, 0b011, 0b001]);
        });
        // A parenthesized scalar must not be read as a grouping list.
        with_parser("SELECT a FROM t GROUP BY (a + 1) * 2", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(s.group_by.len(), 1);
            assert!(s.grouping_sets.is_empty());
        });
    }

    #[test]
    fn grouping_capacity_matches_postgresql() {
        use core::fmt::Write;

        fn parse_with(source: &str, check: impl FnOnce(Result<Stmt<'_>, ParseError>)) {
            let mut budget = Budget::new(32 << 20);
            let arena = Arena::new(&mut budget, "grouping capacity", 16 << 20).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                let mut parser = Parser::new(source, &arena).unwrap();
                let statement = match parser.next_stmt() {
                    Ok(Some(statement)) => Ok(statement),
                    Ok(None) => Err(ParseError::new(0, "expected statement")),
                    Err(error) => Err(error),
                };
                check(statement);
            });
        }

        let mut flat = String::from("SELECT 0 GROUP BY ");
        for index in 0..MAX_GROUP_TERMS {
            if index != 0 {
                flat.push(',');
            }
            write!(flat, "x+{index}").unwrap();
        }
        parse_with(&flat, |statement| {
            let Stmt::Select(select) = statement.unwrap() else {
                panic!()
            };
            assert_eq!(select.group_by.len(), MAX_GROUP_TERMS);
        });
        write!(flat, ",x+{MAX_GROUP_TERMS}").unwrap();
        parse_with(&flat, |statement| {
            assert_eq!(statement.unwrap_err().sqlstate, sqlstate::TOO_MANY_COLUMNS);
        });

        let mut sets = String::from("SELECT 0 GROUP BY GROUPING SETS (");
        for index in 0..MAX_GROUPING_SETS {
            if index != 0 {
                sets.push(',');
            }
            sets.push_str("()");
        }
        sets.push(')');
        parse_with(&sets, |statement| {
            let Stmt::Select(select) = statement.unwrap() else {
                panic!()
            };
            assert_eq!(select.grouping_sets.len(), MAX_GROUPING_SETS);
        });
        sets.insert_str(sets.len() - 1, ",()");
        parse_with(&sets, |statement| {
            assert_eq!(
                statement.unwrap_err().sqlstate,
                sqlstate::STATEMENT_TOO_COMPLEX
            );
        });

        let cube = format!(
            "SELECT 0 GROUP BY CUBE ({})",
            (0..MAX_CUBE_TERMS)
                .map(|index| format!("x+{index}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        parse_with(&cube, |statement| {
            let Stmt::Select(select) = statement.unwrap() else {
                panic!()
            };
            assert_eq!(select.grouping_sets.len(), MAX_GROUPING_SETS);
        });
        let too_wide_cube = format!("{},{})", &cube[..cube.len() - 1], "x+12");
        parse_with(&too_wide_cube, |statement| {
            assert_eq!(statement.unwrap_err().sqlstate, sqlstate::TOO_MANY_COLUMNS);
        });
    }

    #[test]
    fn derived_table_column_alias_list() {
        with_parser("SELECT * FROM (VALUES (1,'a')) AS v(id, name)", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            let base = &s.from.unwrap().base;
            assert_eq!(base.alias, Some("v"));
            assert_eq!(base.col_alias, Some(&["id", "name"][..]));
            assert!(base.subquery.is_some());
        });
    }

    #[test]
    fn derived_table_alias_is_optional() {
        with_parser("SELECT * FROM (VALUES (1, 'a'))", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            let base = &s.from.unwrap().base;
            assert_eq!(base.alias, None);
            assert!(base.subquery.is_some());
        });
        with_parser("SELECT * FROM (SELECT 1) AS", |p| {
            let error = p.next_stmt().unwrap_err();
            assert_eq!(error.message.as_str(), "syntax error at end of input");
        });
        with_parser("SELECT * FROM (SELECT 1) AS select", |p| {
            let error = p.next_stmt().unwrap_err();
            assert_eq!(error.message.as_str(), "syntax error at or near \"select\"");
        });
    }

    #[test]
    fn table_function_column_alias() {
        with_parser("SELECT * FROM generate_series(1,3) AS g(x)", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            let base = &s.from.unwrap().base;
            assert_eq!(base.alias, Some("g"));
            assert_eq!(base.col_alias, Some(&["x"][..]));
            assert!(base.func_args.is_some());
        });
    }

    #[test]
    fn rows_from_is_a_nonempty_typed_function_group() {
        with_parser(
            "SELECT * FROM LATERAL ROWS FROM (generate_series(1,3), unnest(ARRAY[4,5])) \
             WITH ORDINALITY AS expanded(series, value, ordinality)",
            |p| {
                let Stmt::Select(select) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                let source = &select.from.unwrap().base;
                assert!(source.lateral);
                assert!(source.with_ordinality);
                assert_eq!(source.alias, Some("expanded"));
                assert_eq!(
                    source.col_alias,
                    Some(&["series", "value", "ordinality"][..])
                );
                let functions = source.rows_from.expect("typed ROWS FROM group");
                assert_eq!(functions.len(), 2);
                assert_eq!(functions[0].table, "generate_series");
                assert_eq!(functions[1].table, "unnest");
                assert!(functions.iter().all(TableRef::is_function_source));
            },
        );
        for sql in [
            "SELECT * FROM ROWS FROM ()",
            "SELECT * FROM ROWS FROM (ordinary_table)",
            "SELECT * FROM ROWS FROM (ROWS FROM (generate_series(1,2)))",
        ] {
            with_parser(sql, |p| assert!(p.next_stmt().is_err(), "{sql}"));
        }
    }

    #[test]
    fn precedence_and_parens() {
        with_parser("SELECT 1 + 2 * 3, (1 + 2) * 3", |p| {
            let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            let SelectItem::Expr { expression, .. } = s.items[0] else {
                panic!()
            };
            // 1 + (2 * 3)
            let Expr::Binary {
                operator: BinaryOp::Add,
                left,
                right,
                ..
            } = expression
            else {
                panic!()
            };
            assert_eq!(**left, Expr::Int(1));
            assert!(matches!(
                right,
                Expr::Binary {
                    operator: BinaryOp::Mul,
                    ..
                }
            ));
            let SelectItem::Expr { expression, .. } = s.items[1] else {
                panic!()
            };
            assert!(matches!(
                expression,
                Expr::Binary {
                    operator: BinaryOp::Mul,
                    ..
                }
            ));
        });
    }

    #[test]
    fn full_select_shape() {
        with_parser(
            "SELECT a, b FROM t WHERE a > 1 AND b = 'x' ORDER BY a DESC, b LIMIT 10",
            |p| {
                let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert_eq!(s.from.unwrap().base.table, "t");
                assert!(matches!(
                    s.where_clause.unwrap(),
                    Expr::Binary {
                        operator: BinaryOp::And,
                        ..
                    }
                ));
                assert_eq!(s.order_by.len(), 2);
                assert!(s.order_by[0].descending);
                assert!(!s.order_by[1].descending);
                assert_eq!(s.limit, Some(&Expr::Int(10)));
            },
        );
        with_parser(
            "SELECT a FROM t ORDER BY a USING > NULLS LAST, a USING OPERATOR(pg_catalog.<)",
            |p| {
                let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert!(s.order_by[0].descending);
                assert!(!s.order_by[0].nulls_first);
                assert!(!s.order_by[1].descending);
                assert!(!s.order_by[1].nulls_first);
            },
        );
        with_parser("SELECT a FROM t ORDER BY a USING =", |p| {
            let error = p.next_stmt().unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::WRONG_OBJECT_TYPE);
        });
        with_parser(
            "SELECT array_agg(a ORDER BY a USING >), \
                    row_number() OVER (ORDER BY a USING OPERATOR(pg_catalog.<)) FROM t",
            |p| {
                let Stmt::Select(select) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                let SelectItem::Expr {
                    expression:
                        Expr::Call {
                            order_by: aggregate_order,
                            ..
                        },
                    ..
                } = select.items[0]
                else {
                    panic!()
                };
                assert!(aggregate_order[0].descending);
                let SelectItem::Expr {
                    expression:
                        Expr::Call {
                            over: Some(over), ..
                        },
                    ..
                } = select.items[1]
                else {
                    panic!()
                };
                assert!(!over.order_by[0].descending);
            },
        );
    }

    #[test]
    fn ddl_and_dml() {
        with_parser(
            "CREATE TABLE t (id int NOT NULL, name text, score double precision);
             INSERT INTO t (id, name) VALUES (1, 'a'), (2, NULL);
             UPDATE t SET name = 'b' WHERE id = 1;
             DELETE FROM t WHERE id = 2;
             DROP TABLE IF EXISTS t",
            |p| {
                let Stmt::CreateTable(c) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert_eq!(c.name, QualName::bare("t"));
                assert_eq!(c.columns.len(), 3);
                assert!(c.columns[0].not_null);
                assert_eq!(c.columns[2].type_name, "float8");

                let Stmt::Insert(i) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert_eq!(i.columns, &["id", "name"]);
                assert_eq!(i.rows.len(), 2);
                assert_eq!(*i.rows[1][1], Expr::Null);

                assert!(matches!(p.next_stmt().unwrap().unwrap(), Stmt::Update(_)));
                assert!(matches!(p.next_stmt().unwrap().unwrap(), Stmt::Delete(_)));
                let Stmt::DropTable(d) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert!(d.if_exists);
            },
        );
    }

    #[test]
    fn view_security_barrier_keeps_default_and_explicit_false_distinct() {
        with_parser(
            "CREATE VIEW default_barrier AS SELECT 1; \
             CREATE VIEW disabled_barrier WITH (security_barrier = false) AS SELECT 1; \
             CREATE VIEW enabled_barrier WITH (security_barrier = true) AS SELECT 1",
            |parser| {
                for expected in [
                    crate::sql::ast::ViewSecurityBarrier::Default,
                    crate::sql::ast::ViewSecurityBarrier::Disabled,
                    crate::sql::ast::ViewSecurityBarrier::Enabled,
                ] {
                    let Some(Stmt::CreateView {
                        security_barrier, ..
                    }) = parser.next_stmt().unwrap()
                    else {
                        panic!("CREATE VIEW did not parse into a typed view statement")
                    };
                    assert_eq!(security_barrier, expected);
                }
            },
        );
    }

    #[test]
    fn temporary_view_spellings_retain_typed_persistence() {
        with_parser(
            "CREATE TEMP VIEW first AS SELECT 1; \
             CREATE TEMPORARY VIEW second AS SELECT 2; \
             CREATE OR REPLACE TEMP VIEW third AS SELECT 3",
            |parser| {
                for expected in ["first", "second", "third"] {
                    let Some(Stmt::CreateView {
                        name, persistence, ..
                    }) = parser.next_stmt().unwrap()
                    else {
                        panic!("temporary view did not parse into CREATE VIEW")
                    };
                    assert_eq!(name.name, expected);
                    assert_eq!(persistence, crate::sql::ast::RelationPersistence::Temporary);
                }
            },
        );
    }

    #[test]
    fn constraint_attributes_reject_unrepresentable_states_at_parse_time() {
        for (sql, message, sqlstate, cursor) in [
            (
                "CREATE TABLE t (id integer UNIQUE ENFORCED)",
                "misplaced ENFORCED clause",
                "42601",
                34,
            ),
            (
                "CREATE TABLE t (id integer CHECK (id > 0) DEFERRABLE)",
                "misplaced DEFERRABLE clause",
                "42601",
                42,
            ),
            (
                "CREATE TABLE t (id integer REFERENCES r DEFERRABLE NOT DEFERRABLE)",
                "multiple DEFERRABLE/NOT DEFERRABLE clauses not allowed",
                "42601",
                51,
            ),
            (
                "CREATE TABLE t (id integer REFERENCES r INITIALLY DEFERRED NOT DEFERRABLE)",
                "constraint declared INITIALLY DEFERRED must be DEFERRABLE",
                "42601",
                59,
            ),
            (
                "CREATE TABLE t (id integer, CHECK (id > 0) NOT VALID NO INHERIT DEFERRABLE)",
                "CHECK constraints cannot be marked DEFERRABLE",
                "0A000",
                43,
            ),
            (
                "CREATE TABLE t (id integer, UNIQUE (id) DEFERRABLE NOT DEFERRABLE)",
                "conflicting constraint properties",
                "42601",
                51,
            ),
            (
                "CREATE TABLE t (id integer, EXCLUDE USING gist (id WITH &&) NOT VALID)",
                "EXCLUDE constraints cannot be marked NOT VALID",
                "0A000",
                60,
            ),
        ] {
            with_parser(sql, |parser| {
                let error = parser.next_stmt().expect_err(sql);
                assert_eq!(error.message.as_str(), message, "{sql}");
                assert_eq!(error.sqlstate, sqlstate, "{sql}");
                assert_eq!(error.cursor, Some(cursor), "{sql}");
            });
        }
        // INITIALLY DEFERRED implies DEFERRABLE, and a new ordinary table
        // marks a NOT VALID check valid, as PostgreSQL does.
        with_parser(
            "CREATE TABLE t (id integer UNIQUE INITIALLY DEFERRED, \
             CHECK (id > 0) NOT VALID)",
            |parser| {
                let Stmt::CreateTable(table) = parser.next_stmt().unwrap().unwrap() else {
                    panic!("CREATE TABLE did not parse");
                };
                assert!(matches!(
                    table.constraints,
                    [
                        TableConstraint::Unique {
                            timing: ConstraintTiming::Deferrable(ConstraintMode::Deferred),
                            ..
                        },
                        TableConstraint::Check {
                            validation: ConstraintValidation::EnforcedValidated,
                            ..
                        },
                    ]
                ));
            },
        );
        with_parser(
            "ALTER TABLE t ADD CONSTRAINT positive CHECK (id > 0) NOT VALID",
            |parser| assert!(parser.next_stmt().unwrap().is_some()),
        );
        with_parser(
            "ALTER TABLE t ALTER CONSTRAINT t_id_not_null NO INHERIT; \
             ALTER TABLE t ALTER CONSTRAINT t_id_not_null INHERIT",
            |parser| {
                assert!(matches!(
                    parser.next_stmt().unwrap().unwrap(),
                    Stmt::AlterTable(AlterTable {
                        actions: [AlterAction::AlterConstraint {
                            alteration: ConstraintAlteration::NotNullInheritance { inherit: false },
                            ..
                        }],
                        ..
                    })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap().unwrap(),
                    Stmt::AlterTable(AlterTable {
                        actions: [AlterAction::AlterConstraint {
                            alteration: ConstraintAlteration::NotNullInheritance { inherit: true },
                            ..
                        }],
                        ..
                    })
                ));
            },
        );
    }

    #[test]
    fn attached_index_constraints_preserve_the_index_identity_in_the_ast() {
        with_parser(
            "ALTER TABLE public.items ADD CONSTRAINT items_pkey \
             PRIMARY KEY USING INDEX public.items_id_idx; \
             ALTER TABLE public.items ADD UNIQUE USING INDEX items_code_idx",
            |parser| {
                let Some(Stmt::AlterTable(first)) = parser.next_stmt().unwrap() else {
                    panic!()
                };
                assert!(matches!(
                    first.actions,
                    [AlterAction::AttachIndexConstraint {
                        name: Some("items_pkey"),
                        index: QualName {
                            schema: Some("public"),
                            name: "items_id_idx"
                        },
                        primary: true,
                        timing: ConstraintTiming::NotDeferrable,
                    }]
                ));
                let Some(Stmt::AlterTable(second)) = parser.next_stmt().unwrap() else {
                    panic!()
                };
                assert!(matches!(
                    second.actions,
                    [AlterAction::AttachIndexConstraint {
                        name: None,
                        index: QualName {
                            schema: None,
                            name: "items_code_idx"
                        },
                        primary: false,
                        timing: ConstraintTiming::NotDeferrable,
                    }]
                ));
            },
        );
    }

    #[test]
    fn named_variadic_arguments_keep_their_names_in_every_call_form() {
        with_parser(
            "SELECT f(marker=>7,VARIADIC \"Members\"=>ARRAY[]::integer[]); \
             SELECT * FROM f(marker=>7,VARIADIC \"Members\"=>ARRAY[]::integer[]); \
             CALL p(marker=>7,VARIADIC \"Members\"=>ARRAY[]::integer[])",
            |parser| {
                let Stmt::Select(select) = parser.next_stmt().unwrap().unwrap() else {
                    panic!("scalar call")
                };
                let SelectItem::Expr {
                    expression:
                        Expr::Call {
                            argument_names,
                            variadic,
                            ..
                        },
                    ..
                } = select.items[0]
                else {
                    panic!("scalar function expression")
                };
                assert!(*variadic);
                assert_eq!(*argument_names, [Some("marker"), Some("Members")]);
                let Stmt::Select(select) = parser.next_stmt().unwrap().unwrap() else {
                    panic!("table call")
                };
                let table = &select.from.unwrap().base;
                assert!(table.func_variadic);
                assert_eq!(table.func_argument_names, [Some("marker"), Some("Members")]);
                let Stmt::Call {
                    argument_names,
                    variadic,
                    ..
                } = parser.next_stmt().unwrap().unwrap()
                else {
                    panic!("procedure call")
                };
                assert!(variadic);
                assert_eq!(argument_names, [Some("marker"), Some("Members")]);
            },
        );
        for sql in [
            "SELECT f(marker=>7,VARIADIC ARRAY[]::integer[])",
            "SELECT * FROM f(marker=>7,VARIADIC ARRAY[]::integer[])",
            "CALL p(marker=>7,VARIADIC ARRAY[]::integer[])",
            "SELECT f(VARIADIC members=>ARRAY[]::integer[],marker=>7)",
            "SELECT * FROM f(VARIADIC members=>ARRAY[]::integer[],marker=>7)",
            "CALL p(VARIADIC members=>ARRAY[]::integer[],marker=>7)",
        ] {
            with_parser(sql, |parser| {
                assert!(parser.next_stmt().is_err(), "accepted {sql}");
            });
        }
    }

    #[test]
    fn routine_parameter_names_obey_postgresql_type_function_identifier_grammar() {
        for sql in [
            "CREATE FUNCTION f(values integer) RETURNS integer LANGUAGE SQL AS 'SELECT 1'",
            "CREATE PROCEDURE p(VARIADIC values integer[]) LANGUAGE SQL AS 'SELECT 1'",
            "ALTER FUNCTION f(values integer) RENAME TO g",
            "DROP PROCEDURE p(values integer)",
            "CREATE FUNCTION f() RETURNS TABLE (position integer) LANGUAGE SQL AS 'SELECT 1'",
            "CREATE FUNCTION f() RETURNS TABLE (values integer) LANGUAGE SQL AS 'SELECT 1'",
            "CREATE FUNCTION f() RETURNS TABLE (select integer) LANGUAGE SQL AS 'SELECT 1'",
        ] {
            with_parser(sql, |parser| {
                assert!(parser.next_stmt().is_err(), "accepted {sql}")
            });
        }
        for sql in [
            "CREATE FUNCTION f(\"values\" integer) RETURNS integer LANGUAGE SQL AS 'SELECT 1'",
            "CREATE FUNCTION f(begin integer, set text) RETURNS integer LANGUAGE SQL AS 'SELECT 1'",
            "CREATE FUNCTION f(left integer) RETURNS integer LANGUAGE SQL AS 'SELECT 1'",
            "CREATE FUNCTION f() RETURNS TABLE (\"position\" integer) LANGUAGE SQL AS 'SELECT 1'",
            "CREATE FUNCTION f() RETURNS TABLE (left integer) LANGUAGE SQL AS 'SELECT 1'",
            "CREATE FUNCTION f() RETURNS TABLE (begin integer, set text) LANGUAGE SQL AS 'SELECT 1, NULL'",
        ] {
            with_parser(sql, |parser| {
                assert!(parser.next_stmt().unwrap().is_some(), "rejected {sql}")
            });
        }
    }

    #[test]
    fn multi_word_type_aliases_retain_array_suffixes() {
        with_parser(
            "CREATE TABLE type_alias_arrays (values double precision[], moments timestamp with time zone[])",
            |p| {
                let Stmt::CreateTable(create) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert_eq!(create.columns[0].type_name, "float8[]");
                assert_eq!(create.columns[1].type_name, "timestamptz[]");
            },
        );
    }

    #[test]
    fn casts_is_null_and_txn() {
        with_parser(
            "SELECT 1::bigint, NULL IS NULL, 2 IS NOT NULL; BEGIN; COMMIT; ROLLBACK",
            |p| {
                let Stmt::Select(s) = p.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                let SelectItem::Expr { expression, .. } = s.items[0] else {
                    panic!()
                };
                assert!(matches!(
                    expression,
                    Expr::Cast {
                        type_name: "bigint",
                        ..
                    }
                ));
                let SelectItem::Expr { expression, .. } = s.items[2] else {
                    panic!()
                };
                assert!(matches!(expression, Expr::IsNull { negated: true, .. }));
                assert!(matches!(
                    p.next_stmt().unwrap().unwrap(),
                    Stmt::Begin(TransactionCharacteristics::EMPTY, _)
                ));
                assert!(matches!(p.next_stmt().unwrap().unwrap(), Stmt::Commit));
                assert!(matches!(p.next_stmt().unwrap().unwrap(), Stmt::Rollback));
            },
        );
    }

    #[test]
    fn transforms_keep_direction_and_function_identity_typed() {
        with_parser(
            "CREATE OR REPLACE TRANSFORM FOR public.payload LANGUAGE plpgsql \
             (TO SQL WITH FUNCTION public.to_payload(internal), \
              FROM SQL WITH FUNCTION public.from_payload(internal)); \
             CREATE TRANSFORM FOR public.payload LANGUAGE plpgsql \
             (TO SQL WITH FUNCTION public.to_payload(internal)); \
             DROP TRANSFORM IF EXISTS FOR public.payload LANGUAGE plpgsql CASCADE",
            |parser| {
                let Some(Stmt::CreateTransform(create)) = parser.next_stmt().unwrap() else {
                    panic!("CREATE OR REPLACE TRANSFORM did not parse")
                };
                assert!(create.or_replace);
                assert_eq!(create.type_name, "public.payload");
                assert_eq!(create.language, "plpgsql");
                let from = create.from_sql.expect("missing FROM SQL function");
                assert_eq!(from.name.schema, Some("public"));
                assert_eq!(from.name.name, "from_payload");
                assert_eq!(from.argument_types, ["internal"]);
                let to = create.to_sql.expect("missing TO SQL function");
                assert_eq!(to.name.schema, Some("public"));
                assert_eq!(to.name.name, "to_payload");
                assert_eq!(to.argument_types, ["internal"]);

                let Some(Stmt::CreateTransform(create)) = parser.next_stmt().unwrap() else {
                    panic!("one-direction CREATE TRANSFORM did not parse")
                };
                assert!(!create.or_replace);
                assert!(create.from_sql.is_none());
                assert!(create.to_sql.is_some());

                let Some(Stmt::DropTransform(drop)) = parser.next_stmt().unwrap() else {
                    panic!("DROP TRANSFORM did not parse")
                };
                assert!(drop.if_exists);
                assert!(drop.cascade);
                assert_eq!(drop.type_name, "public.payload");
                assert_eq!(drop.language, "plpgsql");
                assert!(parser.next_stmt().unwrap().is_none());
            },
        );
        with_parser(
            "CREATE TRANSFORM FOR payload LANGUAGE plpgsql (FROM SQL WITHOUT FUNCTION)",
            |parser| {
                assert_eq!(
                    parser.next_stmt().unwrap_err().sqlstate,
                    sqlstate::SYNTAX_ERROR
                );
            },
        );
    }

    #[test]
    fn security_labels_use_their_own_closed_object_grammar() {
        with_parser(
            "SECURITY LABEL FOR selinux ON PROCEDURAL LANGUAGE plpgsql IS 'label'; \
             SECURITY LABEL ON COLUMN public.items.label IS NULL; \
             SECURITY LABEL ON AGGREGATE public.percentile(IN fraction float8 ORDER BY value float8) IS 'label'",
            |parser| {
                let Some(Stmt::SecurityLabel {
                    provider,
                    target,
                    label,
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("procedural language security label did not parse")
                };
                assert_eq!(provider, Some("selinux"));
                assert_eq!(label, Some("label"));
                assert_eq!(
                    target,
                    crate::sql::ast::SecurityLabelTarget::ProceduralLanguage("plpgsql")
                );
                let Some(Stmt::SecurityLabel { target, label, .. }) = parser.next_stmt().unwrap()
                else {
                    panic!("column security label did not parse")
                };
                assert_eq!(label, None);
                assert_eq!(
                    target,
                    crate::sql::ast::SecurityLabelTarget::Column {
                        relation: QualName {
                            schema: Some("public"),
                            name: "items",
                        },
                        column: "label",
                    }
                );
                let Some(Stmt::SecurityLabel {
                    target: crate::sql::ast::SecurityLabelTarget::Aggregate(identity),
                    ..
                }) = parser.next_stmt().unwrap()
                else {
                    panic!("ordered aggregate security label did not parse")
                };
                assert_eq!(identity.name.name, "percentile");
                assert_eq!(identity.argument_types.len(), 2);
                assert!(
                    identity
                        .argument_types
                        .iter()
                        .all(|argument| argument.name == "float8" && argument.written == "float8")
                );
            },
        );
        with_parser("SECURITY LABEL ON INDEX items_index IS 'label'", |parser| {
            assert_eq!(
                parser.next_stmt().unwrap_err().sqlstate,
                sqlstate::SYNTAX_ERROR
            );
        });
    }

    #[test]
    fn comment_catalog_identities_are_closed_parse_states() {
        with_parser(
            "COMMENT ON CAST (public.mood AS text) IS 'cast'; \
             COMMENT ON OPERATOR public.=== (integer, integer) IS 'operator'; \
             COMMENT ON OPERATOR FAMILY public.int_family USING btree IS 'family'; \
             COMMENT ON OPERATOR CLASS public.int_class USING btree IS 'class'; \
             COMMENT ON CONSTRAINT int_checked ON public.items IS 'constraint'; \
             COMMENT ON FOREIGN DATA WRAPPER app_fdw IS 'wrapper'; \
             COMMENT ON ACCESS METHOD btree IS 'access method'; \
             COMMENT ON PROCEDURAL LANGUAGE plpgsql IS 'language'",
            |parser| {
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::Comment {
                        target: crate::sql::ast::CommentTarget::Cast { .. },
                        ..
                    })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::Comment {
                        target: crate::sql::ast::CommentTarget::Operator(_),
                        ..
                    })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::Comment {
                        target: crate::sql::ast::CommentTarget::OperatorFamily { .. },
                        ..
                    })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::Comment {
                        target: crate::sql::ast::CommentTarget::OperatorClass { .. },
                        ..
                    })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::Comment {
                        target: crate::sql::ast::CommentTarget::Constraint { .. },
                        ..
                    })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::Comment {
                        target: crate::sql::ast::CommentTarget::ForeignDataWrapper("app_fdw"),
                        ..
                    })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::Comment {
                        target: crate::sql::ast::CommentTarget::AccessMethod("btree"),
                        ..
                    })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::Comment {
                        target: crate::sql::ast::CommentTarget::ProceduralLanguage("plpgsql"),
                        ..
                    })
                ));
            },
        );
        with_parser(
            "COMMENT ON OPERATOR FAMILY public.int_family USING hash IS 'family'",
            |parser| assert!(parser.next_stmt().is_err()),
        );
    }

    #[test]
    fn syntax_errors_carry_position() {
        for (sql, position) in [("SELECT FROM", 11), ("SELECT , 1", 7)] {
            with_parser(sql, |parser| {
                let error = parser.next_stmt().unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::SYNTAX_ERROR);
                assert_eq!(error.at, position, "{sql}");
                assert_eq!(error.cursor, Some(position), "{sql}");
            });
        }
    }

    #[test]
    fn syntax_errors_echo_client_spelling_at_the_postgresql_cursor() {
        // Messages and cursors (as byte offsets) from PostgreSQL 18.6.
        for (sql, message, cursor) in [
            (
                "SELECT 1 FROM WHERE",
                "syntax error at or near \"WHERE\"",
                14,
            ),
            ("SELECT 1 + FROM x", "syntax error at or near \"FROM\"", 11),
            (
                "CREATE TABLE public.tt (a int NULL CONSTRAINT x NOT NULL)",
                "conflicting NULL/NOT NULL declarations for column \"a\" of table \"tt\"",
                35,
            ),
            (
                "ALTER TABLE tt ADD COLUMN b int NOT NULL NULL",
                "conflicting NULL/NOT NULL declarations for column \"b\" of table \"tt\"",
                41,
            ),
            (
                "CREATE DOMAIN dd AS int NOT NULL NULL",
                "conflicting NULL/NOT NULL constraints",
                33,
            ),
            ("SELECT 1 +   ", "syntax error at end of input", 13),
            ("SELECT 1 + /* c */", "syntax error at end of input", 18),
            (
                "CREATE TABLE Select (a int)",
                "syntax error at or near \"Select\"",
                13,
            ),
            ("SELECT (1,2).f1", "syntax error at or near \".\"", 12),
            (
                "FETCH 99999999999 FROM c",
                "syntax error at or near \"99999999999\"",
                6,
            ),
            (
                "SELECT 'abc",
                "unterminated quoted string at or near \"'abc\"",
                7,
            ),
            (
                "SELECT 1 /* open",
                "unterminated /* comment at or near \"/* open\"",
                9,
            ),
        ] {
            with_parser(sql, |parser| {
                let error = loop {
                    match parser.next_stmt() {
                        Ok(Some(_)) => {}
                        Ok(None) => panic!("{sql} parsed"),
                        Err(error) => break error,
                    }
                };
                assert_eq!(error.message.as_str(), message, "{sql}");
                assert_eq!(error.cursor, Some(cursor), "{sql}");
            });
        }
        // Type modifiers fail in parse analysis, pointing at the type name.
        with_parser("SELECT 1::numeric(2000)", |parser| {
            let error = parser.next_stmt().unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
            assert_eq!(
                error.message.as_str(),
                "NUMERIC precision 2000 must be between 1 and 1000"
            );
            assert_eq!(error.cursor, Some(10));
        });
    }

    #[test]
    fn subscripts_follow_only_postgresql_indirection_forms() {
        // PostgreSQL 18.6 rejects each of these at the `[` or `..`.
        for (sql, message) in [
            ("SELECT ARRAY[1,2][1]", "syntax error at or near \"[\""),
            ("SELECT f(x)[1]", "syntax error at or near \"[\""),
            ("SELECT '{1,2}'[1]", "syntax error at or near \"[\""),
            ("SELECT (1,2)[1]", "syntax error at or near \"[\""),
            ("SELECT x::int[][1] + 1[1]", "syntax error at or near \"[\""),
            ("SELECT 1..2", "syntax error at or near \"..\""),
        ] {
            with_parser(sql, |parser| {
                assert_eq!(
                    parser.next_stmt().unwrap_err().message.as_str(),
                    message,
                    "{sql}"
                );
            });
        }
        for sql in [
            "SELECT a[1], t.a[1:2], $1[1], (f(x))[1], ((1,2)).f1, (SELECT ARRAY[1])[1], a[1][2]",
            "SELECT -9223372036854775808, -0x8000_0000_0000_0000",
        ] {
            with_parser(sql, |parser| {
                assert!(parser.next_stmt().unwrap().is_some(), "{sql}");
            });
        }
        with_parser("SELECT -9223372036854775808", |parser| {
            let Some(Stmt::Select(select)) = parser.next_stmt().unwrap() else {
                panic!("not a SELECT");
            };
            assert!(matches!(
                select.items,
                [SelectItem::Expr {
                    expression: Expr::Int(i64::MIN),
                    ..
                }]
            ));
        });
    }

    #[test]
    fn statement_lists_cross_the_former_fixed_width_and_exhaustion_is_a_program_limit() {
        // Well past the former 64-item boundary: only the statement arena
        // bounds list width now.
        let wide = (0..=4 * FORMER_LIST_LIMIT)
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(",");
        with_parser(&format!("SELECT 1 IN ({wide})"), |parser| {
            assert!(matches!(parser.next_stmt().unwrap(), Some(Stmt::Select(_))));
        });
        with_parser(&format!("SELECT {wide}"), |parser| {
            assert!(matches!(parser.next_stmt().unwrap(), Some(Stmt::Select(_))));
        });
        // Exhausting the statement arena is a resource limit, not a syntax
        // error.
        let huge = (0..100_000)
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(",");
        with_parser(&format!("SELECT 1 IN ({huge})"), |parser| {
            let error = parser.next_stmt().unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(error.message.as_str().contains("SQL arena"));
        });
    }

    #[test]
    fn statement_widths_cross_the_former_fixed_boundaries_and_never_drop_warnings() {
        let warned_columns = (0..FORMER_LIST_LIMIT)
            .map(|index| format!("column_{index} timestamp(7)"))
            .collect::<Vec<_>>()
            .join(",");
        with_parser(
            &format!("CREATE TABLE warning_width ({warned_columns})"),
            |parser| {
                assert!(matches!(
                    parser.next_stmt().unwrap(),
                    Some(Stmt::CreateTable(_))
                ));
                let warnings = parser.take_warnings();
                assert_eq!(warnings.len(), 2 * FORMER_LIST_LIMIT);
                assert!(
                    warnings
                        .iter()
                        .all(|warning| warning.message.contains("precision reduced"))
                );
            },
        );

        // Every one of these lists crossed its former 64-item boundary.
        let width = FORMER_LIST_LIMIT + 1;
        let ctes = (0..width)
            .map(|index| format!("cte_{index} AS (SELECT {index})"))
            .collect::<Vec<_>>()
            .join(",");
        let lock_clauses = std::iter::repeat_n("FOR SHARE", width)
            .collect::<Vec<_>>()
            .join(" ");
        let windows = (0..width)
            .map(|index| format!("window_{index} AS ()"))
            .collect::<Vec<_>>()
            .join(",");
        let actions = (0..width)
            .map(|index| format!("ADD COLUMN column_{index} integer"))
            .collect::<Vec<_>>()
            .join(",");
        for (label, sql) in [
            ("WITH list", format!("WITH {ctes} SELECT 1")),
            ("locking clauses", format!("SELECT 1 {lock_clauses}")),
            (
                "WINDOW definitions",
                format!("SELECT row_number() OVER window_0 WINDOW {windows}"),
            ),
            (
                "ALTER TABLE actions",
                format!("ALTER TABLE target {actions}"),
            ),
        ] {
            with_parser(&sql, |parser| {
                assert!(
                    parser.next_stmt().unwrap().is_some(),
                    "{label} must parse past the former fixed boundary"
                );
            });
        }
    }

    #[test]
    fn statistics_grammar_produces_only_valid_typed_states() {
        with_parser(
            "CREATE STATISTICS s (ndistinct, dependencies, mcv) ON a, (lower(b)) FROM app.t; \
             ALTER STATISTICS s SET STATISTICS DEFAULT; \
             ALTER STATISTICS s SET STATISTICS 10000; \
             DROP STATISTICS IF EXISTS s CASCADE",
            |parser| {
                let Stmt::CreateStatistics(create) = parser.next_stmt().unwrap().unwrap() else {
                    panic!("expected CREATE STATISTICS")
                };
                let StatisticsKeys::Multivariate { kinds, keys } = create.keys else {
                    panic!("expected multivariate statistics")
                };
                assert!(kinds.ndistinct() && kinds.dependencies() && kinds.mcv());
                assert_eq!(keys.len(), 2);
                assert!(matches!(keys[1], StatisticsKey::Expression(_)));
                assert!(matches!(
                    parser.next_stmt().unwrap().unwrap(),
                    Stmt::AlterStatistics {
                        action: AlterStatisticsAction::SetTarget(StatisticsTarget::Default),
                        ..
                    }
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap().unwrap(),
                    Stmt::AlterStatistics {
                        action: AlterStatisticsAction::SetTarget(StatisticsTarget::Value(10000)),
                        ..
                    }
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap().unwrap(),
                    Stmt::DropStatistics {
                        if_exists: true,
                        cascade: true,
                        ..
                    }
                ));
            },
        );
        for sql in [
            "CREATE STATISTICS ON a FROM t",
            "CREATE STATISTICS s (ndistinct) ON (lower(a)) FROM t",
            "CREATE STATISTICS s (mcv, mcv) ON a, b FROM t",
            "CREATE STATISTICS IF NOT EXISTS ON a, b FROM t",
            "ALTER STATISTICS s SET STATISTICS 10001",
        ] {
            with_parser(sql, |parser| {
                assert!(parser.next_stmt().is_err(), "accepted {sql}")
            });
        }
    }

    #[test]
    fn table_sources_parse_into_closed_inheritance_and_sampling_states() {
        with_parser(
            "TABLE ONLY (app.events) UNION ALL TABLE archived_events *; \
             SELECT event_id FROM app.events AS sampled(event_id, payload) \
               TABLESAMPLE BERNOULLI ($1) REPEATABLE ($2)",
            |parser| {
                let Stmt::SetQuery(query) = parser.next_stmt().unwrap().unwrap() else {
                    panic!("expected TABLE set query")
                };
                let SetTree::Op { left, right, .. } = query.body else {
                    panic!("expected UNION ALL tree")
                };
                let SetTree::Select(left) = left else {
                    panic!()
                };
                assert_eq!(
                    left.from.unwrap().base.inheritance,
                    RelationInheritance::Only
                );
                let SetTree::Select(right) = right else {
                    panic!()
                };
                assert_eq!(
                    right.from.unwrap().base.inheritance,
                    RelationInheritance::Descendants
                );

                let Stmt::Select(select) = parser.next_stmt().unwrap().unwrap() else {
                    panic!("expected sampled SELECT")
                };
                let table = select.from.unwrap().base;
                assert_eq!(table.col_alias.unwrap(), ["event_id", "payload"]);
                let sample = table.sample.expect("typed TABLESAMPLE");
                assert_eq!(sample.method, TableSampleMethod::Bernoulli);
                assert!(matches!(sample.percentage, Expr::Param(1)));
                assert!(matches!(sample.repeatable, Some(Expr::Param(2))));
            },
        );

        for (sql, state) in [
            (
                "SELECT * FROM events TABLESAMPLE missing (10)",
                sqlstate::UNDEFINED_OBJECT,
            ),
            (
                "SELECT * FROM events TABLESAMPLE system (1, 2)",
                sqlstate::INVALID_TABLESAMPLE_ARGUMENT,
            ),
            ("SELECT * FROM LATERAL events", sqlstate::SYNTAX_ERROR),
            ("TABLE ONLY events *", sqlstate::SYNTAX_ERROR),
        ] {
            with_parser(sql, |parser| {
                let error = parser.next_stmt().unwrap_err();
                assert_eq!(error.sqlstate, state, "{sql}");
            });
        }
    }

    #[test]
    fn locale_ddl_parses_encoding_and_provider_into_closed_types() {
        with_parser(
            "CREATE DEFAULT CONVERSION app.c FOR 'latin-1' TO 'utf_8' FROM pg_catalog.iso8859_1_to_utf8; \
             CREATE COLLATION app.c_locale (provider = LIBC, locale = 'C')",
            |parser| {
                let Stmt::CreateConversion(conversion) = parser.next_stmt().unwrap().unwrap()
                else {
                    panic!("expected CREATE CONVERSION")
                };
                assert_eq!(
                    conversion.source_encoding,
                    crate::storage::PgEncoding::LATIN1
                );
                assert_eq!(
                    conversion.destination_encoding,
                    crate::storage::PgEncoding::UTF8
                );
                let Stmt::CreateCollation(collation) = parser.next_stmt().unwrap().unwrap() else {
                    panic!("expected CREATE COLLATION")
                };
                assert!(matches!(
                    collation.definition,
                    crate::sql::ast::CreateCollationDefinition::Options {
                        provider: Some(crate::sql::ast::ParsedCollationProvider::Libc),
                        ..
                    }
                ));
            },
        );
        for sql in [
            "CREATE CONVERSION c FOR 'not-an-encoding' TO 'UTF8' FROM f",
            "CREATE COLLATION c (provider = pretend, locale = 'C')",
        ] {
            with_parser(sql, |parser| {
                assert!(parser.next_stmt().is_err(), "accepted {sql}");
            });
        }
    }

    #[test]
    fn rewrite_rules_parse_into_closed_event_mode_and_action_states() {
        with_parser(
            "CREATE RULE audit_update AS ON UPDATE TO app.accounts \
               WHERE NEW.balance <> OLD.balance DO ALSO ( \
                 INSERT INTO audit VALUES (OLD.id, NEW.balance); \
                 NOTIFY account_changes, 'updated'; \
               ); \
             CREATE OR REPLACE RULE suppress_delete AS ON DELETE TO accounts \
               DO INSTEAD NOTHING; \
             ALTER TABLE accounts ENABLE REPLICA RULE audit_update; \
             ALTER TABLE accounts DISABLE RULE audit_update; \
             ALTER RULE audit_update ON app.accounts RENAME TO audit_balance; \
             DROP RULE IF EXISTS audit_balance ON app.accounts CASCADE",
            |parser| {
                let Stmt::CreateRule(created) = parser.next_stmt().unwrap().unwrap() else {
                    panic!("expected CREATE RULE")
                };
                assert_eq!(created.event, RuleEvent::Update);
                assert_eq!(created.mode, RuleMode::Also);
                assert_eq!(created.condition_sql, Some("NEW.balance <> OLD.balance"));
                assert_eq!(created.actions.len(), 2);
                assert!(matches!(created.actions[0].statement, Stmt::Insert(_)));
                assert!(matches!(created.actions[1].statement, Stmt::Notify { .. }));

                let Stmt::CreateRule(replaced) = parser.next_stmt().unwrap().unwrap() else {
                    panic!("expected CREATE OR REPLACE RULE")
                };
                assert!(replaced.or_replace);
                assert_eq!(replaced.event, RuleEvent::Delete);
                assert_eq!(replaced.mode, RuleMode::Instead);
                assert!(replaced.actions.is_empty());

                assert!(matches!(
                    parser.next_stmt().unwrap().unwrap(),
                    Stmt::AlterTable(AlterTable {
                        actions: [AlterAction::SetRuleEnabled {
                            name: "audit_update",
                            enabled: TriggerEnableMode::Replica,
                        }],
                        ..
                    })
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap().unwrap(),
                    Stmt::AlterTable(AlterTable {
                        actions: [AlterAction::SetRuleEnabled {
                            name: "audit_update",
                            enabled: TriggerEnableMode::Disabled,
                        }],
                        ..
                    })
                ));

                assert!(matches!(
                    parser.next_stmt().unwrap().unwrap(),
                    Stmt::AlterRule {
                        name: "audit_update",
                        new_name: "audit_balance",
                        ..
                    }
                ));
                assert!(matches!(
                    parser.next_stmt().unwrap().unwrap(),
                    Stmt::DropRule(crate::sql::ast::DropRule {
                        name: "audit_balance",
                        if_exists: true,
                        cascade: true,
                        ..
                    })
                ));
            },
        );

        for sql in [
            "CREATE RULE bad AS ON TRUNCATE TO t DO NOTHING",
            "CREATE RULE bad AS ON INSERT TO t DO VACUUM t",
            "CREATE RULE bad AS ON INSERT TO t DO (INSERT INTO x VALUES (1) SELECT 1)",
            "ALTER TABLE t DISABLE RULE ALL",
            "ALTER TABLE t ENABLE RULE USER",
        ] {
            with_parser(sql, |parser| {
                assert!(parser.next_stmt().is_err(), "accepted {sql}");
            });
        }
    }

    #[test]
    fn database_connection_limit_requires_the_complete_option_before_its_value() {
        for sql in [
            "CREATE DATABASE sample CONNECTION LIMIT 9",
            "CREATE DATABASE sample WITH CONNECTION LIMIT = 9",
            "ALTER DATABASE sample CONNECTION LIMIT 9",
            "ALTER DATABASE sample WITH CONNECTION LIMIT = 9",
        ] {
            with_parser(sql, |parser| {
                match parser.next_stmt().unwrap().unwrap() {
                    Stmt::CreateDatabase { options, .. } => {
                        assert_eq!(options.connection_limit, Some(9));
                    }
                    Stmt::AlterDatabase {
                        action:
                            crate::sql::ast::AlterDatabaseAction::Options {
                                connection_limit, ..
                            },
                        ..
                    } => assert_eq!(connection_limit, Some(9)),
                    _ => panic!("unexpected database statement"),
                }
                assert!(parser.next_stmt().unwrap().is_none());
            });
        }
        for sql in [
            "CREATE DATABASE sample CONNECTION = LIMIT 9",
            "CREATE DATABASE sample CONNECTION LIMIT = = 9",
            "CREATE DATABASE sample CONNECTION_LIMIT = = 9",
            "ALTER DATABASE sample CONNECTION = LIMIT 9",
            "ALTER DATABASE sample WITH CONNECTION LIMIT = = 9",
            "ALTER DATABASE sample WITH CONNECTION_LIMIT = = 9",
        ] {
            with_parser(sql, |parser| {
                assert!(parser.next_stmt().is_err(), "accepted {sql}");
            });
        }
    }

    #[test]
    fn sql_constructor_and_temporal_calls_keep_their_syntax_identity() {
        for (sql, expected) in [
            (
                "SELECT json_scalar(1)",
                crate::sql::ast::CallKind::SqlSyntax,
            ),
            (
                "SELECT xmlconcat('<a/>'::xml)",
                crate::sql::ast::CallKind::SqlSyntax,
            ),
            (
                "SELECT merge_action()",
                crate::sql::ast::CallKind::SqlSyntax,
            ),
            (
                "SELECT current_time(0)",
                crate::sql::ast::CallKind::SqlSyntax,
            ),
            ("SELECT localtime(3)", crate::sql::ast::CallKind::SqlSyntax),
            (
                "SELECT current_timestamp(0)",
                crate::sql::ast::CallKind::SqlSyntax,
            ),
            (
                "SELECT localtimestamp(0)",
                crate::sql::ast::CallKind::SqlSyntax,
            ),
            (
                "SELECT \"json_scalar\"(1)",
                crate::sql::ast::CallKind::Routine,
            ),
            (
                "SELECT pg_catalog.xmlconcat('<a/>'::xml)",
                crate::sql::ast::CallKind::Routine,
            ),
            (
                "SELECT \"merge_action\"()",
                crate::sql::ast::CallKind::Routine,
            ),
            (
                "SELECT pg_catalog.current_time(0)",
                crate::sql::ast::CallKind::Routine,
            ),
        ] {
            with_parser(sql, |parser| {
                let Stmt::Select(select) = parser.next_stmt().unwrap().unwrap() else {
                    panic!("expected SELECT");
                };
                let [
                    SelectItem::Expr {
                        expression: Expr::Call { kind, .. },
                        ..
                    },
                ] = select.items
                else {
                    panic!("expected call");
                };
                assert_eq!(*kind, expected, "{sql}");
            });
        }
        for sql in [
            "SELECT json_scalar()",
            "SELECT xmlconcat()",
            "SELECT merge_action(1)",
            "SELECT current_time(1+1)",
            "SELECT localtime(NULL)",
            "SELECT json_scalar(DISTINCT 1)",
            "SELECT xmlconcat('<a/>'::xml) OVER ()",
        ] {
            with_parser(sql, |parser| {
                assert_eq!(
                    parser.next_stmt().unwrap_err().sqlstate,
                    sqlstate::SYNTAX_ERROR,
                    "{sql}"
                );
            });
        }
    }

    #[test]
    fn sql_expression_modifiers_fail_at_the_original_token() {
        for (sql, token) in [
            ("SELECT COALESCE(DISTINCT 1,2)", "DISTINCT"),
            ("SELECT ROW(VARIADIC ARRAY[1])", "VARIADIC"),
            ("SELECT NULLIF(1,2) FILTER (WHERE true)", "FILTER"),
            ("SELECT GREATEST(a=>1,b=>2)", "=>"),
            ("SELECT JSON_ARRAY(DISTINCT 1)", "DISTINCT"),
            ("SELECT JSON_ARRAY(VARIADIC ARRAY[1])", "VARIADIC"),
        ] {
            with_parser(sql, |parser| {
                let error = parser.next_stmt().unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::SYNTAX_ERROR);
                assert_eq!(
                    error.message,
                    stack_format!(PARSE_MESSAGE_BYTES, "syntax error at or near \"{}\"", token)
                );
                assert_eq!(&sql[error.at..error.at + token.len()], token);
            });
        }
        for sql in [
            "SELECT \"coalesce\"(DISTINCT 1,2)",
            "SELECT unnest(DISTINCT ARRAY[1])",
            "SELECT unnest(VARIADIC ARRAY[1])",
        ] {
            with_parser(sql, |parser| {
                let Stmt::Select(select) = parser.next_stmt().unwrap().unwrap() else {
                    panic!()
                };
                assert!(matches!(
                    select.items,
                    [SelectItem::Expr {
                        expression: Expr::Call {
                            kind: CallKind::Routine,
                            ..
                        },
                        ..
                    }]
                ));
            });
        }
    }

    #[test]
    fn ordinary_call_closing_errors_keep_the_unexpected_token() {
        for (sql, message) in [
            (
                "SELECT pg_catalog.json_object('x':1)",
                "syntax error at or near \":\"",
            ),
            (
                "SELECT \"pg_catalog\".json_object('x':1)",
                "syntax error at or near \":\"",
            ),
            ("SELECT arbitrary(1:2)", "syntax error at or near \":\""),
            ("SELECT arbitrary(1", "syntax error at end of input"),
            ("SELECT count(*", "syntax error at end of input"),
        ] {
            with_parser(sql, |parser| {
                let error = parser.next_stmt().unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::SYNTAX_ERROR);
                assert_eq!(error.message.as_str(), message);
            });
        }
    }

    /// Wording, SQLSTATE and echoed token captured from PostgreSQL 18.6.
    #[test]
    fn grammar_and_scanner_failures_use_postgresql_messages() {
        for (sql, state, message) in [
            ("SELECT 1 +", "42601", "syntax error at end of input"),
            ("SELECT (1", "42601", "syntax error at end of input"),
            ("SELEC 1", "42601", "syntax error at or near \"SELEC\""),
            (
                "CREATE TABLE (a int)",
                "42601",
                "syntax error at or near \"(\"",
            ),
            ("SELECT * FROM", "42601", "syntax error at end of input"),
            (
                "INSERT INTO x VALUES 1",
                "42601",
                "syntax error at or near \"1\"",
            ),
            ("SELECT x NOT 1", "42601", "syntax error at or near \"1\""),
            (
                "SELECT CASE END",
                "42601",
                "syntax error at or near \"END\"",
            ),
            (
                "ALTER TABLE t FOO",
                "42601",
                "syntax error at or near \"FOO\"",
            ),
            ("SELECT 1 2", "42601", "syntax error at or near \"2\""),
            (
                "BEGIN ISOLATION LEVEL FOO",
                "42601",
                "syntax error at or near \"FOO\"",
            ),
            (
                "CREATE TABLE t (a int, )",
                "42601",
                "syntax error at or near \")\"",
            ),
            ("SELECT f(a := )", "42601", "syntax error at or near \")\""),
            ("SELECT 1 `", "42601", "syntax error at end of input"),
            ("SELECT $", "42601", "syntax error at or near \"$\""),
            (
                "SELECT interval '1' day to year",
                "42601",
                "syntax error at or near \"year\"",
            ),
            (
                "SELECT interval '1' month to day",
                "42601",
                "syntax error at or near \"to\"",
            ),
            (
                "CREATE FUNCTION f() RETURNS int LANGUAGE sql STABLE VOLATILE AS 'x'",
                "42601",
                "conflicting or redundant options",
            ),
            (
                "CREATE FUNCTION f() LANGUAGE sql AS 'select 1'",
                "42P13",
                "function result type must be specified",
            ),
            (
                "CREATE FUNCTION f() RETURNS int LANGUAGE sql",
                "42P13",
                "no function body specified",
            ),
            (
                "CREATE FUNCTION f() RETURNS int AS 'select 1'",
                "42P13",
                "no language specified",
            ),
            (
                "SELECT 'abc",
                "42601",
                "unterminated quoted string at or near \"'abc\"",
            ),
            (
                "SELECT E'abc",
                "42601",
                "unterminated quoted string at or near \"E'abc\"",
            ),
            (
                "SELECT \"abc",
                "42601",
                "unterminated quoted identifier at or near \"\"abc\"",
            ),
            (
                "SELECT $$abc",
                "42601",
                "unterminated dollar-quoted string at or near \"$$abc\"",
            ),
            (
                "SELECT B'10",
                "42601",
                "unterminated bit string literal at or near \"B'10\"",
            ),
            (
                "SELECT X'1F",
                "42601",
                "unterminated hexadecimal string literal at or near \"X'1F\"",
            ),
            (
                "SELECT /* abc",
                "42601",
                "unterminated /* comment at or near \"/* abc\"",
            ),
            (
                "SELECT \"\"",
                "42601",
                "zero-length delimited identifier at or near \"\"\"\"",
            ),
            (
                "SELECT 1a",
                "42601",
                "trailing junk after numeric literal at or near \"1a\"",
            ),
            (
                "SELECT 1ex",
                "42601",
                "trailing junk after numeric literal at or near \"1ex\"",
            ),
            (
                "SELECT 0x1G",
                "42601",
                "trailing junk after numeric literal at or near \"0x1G\"",
            ),
            (
                "SELECT $1abc",
                "42601",
                "trailing junk after parameter at or near \"$1abc\"",
            ),
        ] {
            let mut budget = Budget::new(1 << 20);
            let arena = Arena::new(&mut budget, "postgresql parse errors", 1 << 16).unwrap();
            let error = match Parser::new(sql, &arena) {
                Ok(mut parser) => parser.next_stmt().unwrap_err(),
                Err(error) => error,
            };
            assert_eq!(
                (error.sqlstate, error.message.as_str()),
                (state, message),
                "{sql}"
            );
        }
    }

    #[test]
    fn array_brackets_build_nested_array_expressions() {
        with_parser("SELECT ARRAY[[1,NULL],[3,4]]", |parser| {
            let Stmt::Select(select) = parser.next_stmt().unwrap().unwrap() else {
                panic!()
            };
            let [
                SelectItem::Expr {
                    expression: Expr::Array(rows),
                    ..
                },
            ] = select.items
            else {
                panic!()
            };
            assert!(matches!(
                rows,
                [
                    Expr::Array([Expr::Int(1), Expr::Null]),
                    Expr::Array([Expr::Int(3), Expr::Int(4)])
                ]
            ));
        });
        with_parser(
            "SELECT CASE WHEN false THEN ARRAY[[[[[[[1]]]]]]] ELSE ARRAY[1] END",
            |parser| {
                assert!(parser.next_stmt().unwrap().is_some());
            },
        );
        with_parser("SELECT ARRAY[[1],ARRAY[2]]", |parser| {
            assert_eq!(
                parser.next_stmt().unwrap_err().message.as_str(),
                "syntax error at or near \"ARRAY\""
            );
        });
        let deep = format!("SELECT ARRAY{}1{}", "[".repeat(65), "]".repeat(65));
        with_parser(&deep, |parser| {
            assert_eq!(
                parser.next_stmt().unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
        });
    }
}

#[cfg(test)]
mod source_position_tests {
    use super::*;

    #[test]
    fn source_positions_preserve_occurrence_identity_without_affecting_expression_equality() {
        let mut budget = crate::mem::budget::Budget::new(64 << 10);
        let arena = Arena::new(&mut budget, "expression source metadata", 64 << 10).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let first = parse_expr("f(1)+g(2)::integer", &arena).unwrap();
            let second = parse_expr(" f(1)+g(2)::integer", &arena).unwrap();
            assert_eq!(first, second, "nested provenance is not SQL semantics");
            assert_eq!(first.source_position().get(), Some(4));
            assert_eq!(second.source_position().get(), Some(5));
            let Expr::Binary { left, right, .. } = first else {
                panic!("binary expression")
            };
            assert_eq!(left.source_position().get(), Some(0));
            assert_eq!(right.source_position().get(), Some(9));
            let copied = crate::sql::ast::duplicate_expression(first, &arena, &[]).unwrap();
            assert_eq!(copied, first);
            assert_eq!(copied.source_position().get(), Some(4));
            let Expr::Binary { left, right, .. } = copied else {
                panic!("copied binary")
            };
            assert_eq!(left.source_position().get(), Some(0));
            assert_eq!(right.source_position().get(), Some(9));
            let query =
                parse_query("SELECT * FROM first_fn() JOIN second_fn() ON true", &arena).unwrap();
            let from = query.from.unwrap();
            assert_eq!(from.base.func_source_position.get(), Some(14));
            assert_eq!(from.joins[0].table.func_source_position.get(), Some(30));
        });
    }
}
