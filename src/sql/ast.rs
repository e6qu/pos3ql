//! Arena-allocated AST. Every node is `Copy`; child links are arena
//! references, so an entire statement tree lives exactly as long as the
//! per-statement arena and costs nothing to drop.

/// A possibly schema-qualified relation name, as written. `schema: None`
/// means the statement spelled a bare name that resolves through the session
/// search path; carrying the pair everywhere makes losing a qualifier
/// impossible by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualName<'a> {
    pub schema: Option<&'a str>,
    pub name: &'a str,
}

impl<'a> QualName<'a> {
    pub fn bare(name: &'a str) -> Self {
        QualName { schema: None, name }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stmt<'a> {
    Select(Select<'a>),
    CreateTable(CreateTable<'a>),
    Insert(Insert<'a>),
    Update(Update<'a>),
    Delete(Delete<'a>),
    Merge(Merge<'a>),
    Begin,
    Commit,
    Rollback,
    /// SAVEPOINT name.
    Savepoint(&'a str),
    /// RELEASE [SAVEPOINT] name.
    ReleaseSavepoint(&'a str),
    /// ROLLBACK TO [SAVEPOINT] name.
    RollbackToSavepoint(&'a str),
    DropTable(DropTable<'a>),
    /// TRUNCATE [TABLE] name [, ...] [RESTART IDENTITY | CONTINUE IDENTITY]
    /// [CASCADE | RESTRICT].
    Truncate { tables: &'a [QualName<'a>], restart_identity: bool, cascade: bool },
    /// CREATE [OR REPLACE] VIEW name AS <select>. `sql` is the raw SELECT text,
    /// stored and re-expanded as a derived table at query time.
    CreateView { name: QualName<'a>, or_replace: bool, sql: &'a str },
    /// DROP VIEW [IF EXISTS] name.
    DropView { names: &'a [QualName<'a>], if_exists: bool },
    /// `CREATE TABLE [IF NOT EXISTS] name [(cols)] AS <select> [WITH [NO] DATA]`
    /// and, with `materialized`, `CREATE MATERIALIZED VIEW`. `sql` is the raw
    /// SELECT text, run once to populate the new (backing) table; `columns`
    /// optionally renames the query's output columns.
    CreateTableAs {
        name: QualName<'a>,
        columns: &'a [&'a str],
        sql: &'a str,
        with_data: bool,
        if_not_exists: bool,
        materialized: bool,
    },
    /// REFRESH MATERIALIZED VIEW name — re-run the stored query, replacing rows.
    RefreshMaterializedView { name: QualName<'a> },
    /// DROP MATERIALIZED VIEW [IF EXISTS] name.
    DropMaterializedView { names: &'a [QualName<'a>], if_exists: bool },
    /// CREATE SEQUENCE [IF NOT EXISTS] name [options].
    CreateSequence {
        name: QualName<'a>,
        if_not_exists: bool,
        options: SeqOptions<'a>,
    },
    /// ALTER SEQUENCE [IF EXISTS] name [options] [RESTART [WITH n]].
    AlterSequence {
        name: QualName<'a>,
        if_exists: bool,
        options: SeqOptions<'a>,
    },
    /// DROP SEQUENCE [IF EXISTS] name [, ...].
    DropSequence { names: &'a [QualName<'a>], if_exists: bool },
    /// CREATE DOMAIN name [AS] basetype [ constraint ... ].
    CreateDomain(CreateDomain<'a>),
    /// ALTER DOMAIN name <action>.
    AlterDomain { name: QualName<'a>, action: AlterDomainAction<'a> },
    /// DROP DOMAIN [IF EXISTS] name [, ...] [CASCADE|RESTRICT].
    DropDomain { names: &'a [QualName<'a>], if_exists: bool, cascade: bool },
    /// CREATE [UNIQUE] INDEX name ON table (col, ...).
    CreateIndex {
        name: &'a str,
        table: QualName<'a>,
        columns: &'a [&'a str],
        unique: bool,
    },
    /// DROP INDEX [IF EXISTS] name.
    DropIndex { names: &'a [QualName<'a>], if_exists: bool },
    /// SET name {=|TO} value. `value` is the raw source text of the value
    /// (quotes included); the session GUC store validates and applies it.
    Set { name: &'a str, value: &'a str },
    /// SET TRANSACTION ... / SET SESSION CHARACTERISTICS AS TRANSACTION ...:
    /// the engine provides one isolation level, so the clause is acknowledged.
    SetTransaction,
    Show(&'a str),
    /// SHOW ALL: every readable setting as (name, setting, description).
    ShowAll,
    /// Snapshot to object storage now.
    Checkpoint,
    AlterTable(AlterTable<'a>),
    /// SQL-level PREPARE name [(types)] AS <statement>; `sql` is the raw
    /// statement text and `param_types` the declared `$n` type names (empty if
    /// none were declared).
    Prepare { name: &'a str, sql: &'a str, param_types: &'a [&'a str] },
    /// SQL-level EXECUTE name(args).
    ExecutePrepared { name: &'a str, args: &'a [&'a Expr<'a>] },
    /// DEALLOCATE name | ALL (None = ALL).
    Deallocate(Option<&'a str>),
    /// COPY table [(columns)] FROM STDIN / TO STDOUT — the bulk-data
    /// subprotocol, text format.
    Copy(CopyStmt<'a>),
    /// A set-operation query (UNION / INTERSECT / EXCEPT). A lone SELECT stays
    /// `Select` above; this variant appears only when a set operator is present.
    SetQuery(SetQuery<'a>),
    /// CREATE SCHEMA [IF NOT EXISTS] name [AUTHORIZATION role] [element ...].
    /// Elements are the embedded CREATE statements, executed with the new
    /// schema as their creation target.
    CreateSchema {
        name: &'a str,
        if_not_exists: bool,
        elements: &'a [&'a Stmt<'a>],
    },
    /// DROP SCHEMA [IF EXISTS] name [, ...] [CASCADE | RESTRICT].
    DropSchema { names: &'a [&'a str], if_exists: bool, cascade: bool },
    /// DECLARE name [SCROLL|NO SCROLL] CURSOR [WITH|WITHOUT HOLD] FOR select.
    /// `sql` is the raw SELECT text, materialized at DECLARE.
    DeclareCursor { name: &'a str, scroll: bool, hold: bool, sql: &'a str },
    /// FETCH/MOVE direction [FROM|IN] cursor. MOVE positions without rows.
    FetchCursor {
        name: &'a str,
        motion: crate::sql::cursor::FetchMotion,
        move_only: bool,
    },
    /// CLOSE cursor | CLOSE ALL (None).
    CloseCursor(Option<&'a str>),
    /// VACUUM [options] [table [(columns)] [, ...]] — drives a checkpoint
    /// (this engine's space reclamation); options and targets are parsed and
    /// the whole store is compacted.
    Vacuum,
    /// ANALYZE [options] [table [(columns)] [, ...]] — accepted; the planner
    /// works from live table state, not collected statistics, so there is no
    /// statistics artifact to build and none is client-observable.
    Analyze,
    /// LISTEN channel — register interest; delivered notifications arrive as
    /// asynchronous NotificationResponse messages.
    Listen(&'a str),
    /// UNLISTEN channel, or UNLISTEN * to drop every registration.
    Unlisten(Option<&'a str>),
    /// NOTIFY channel [, payload] — raise a notification (delivered at commit).
    Notify { channel: &'a str, payload: Option<&'a str> },
    /// COMMENT ON <object> IS { 'text' | NULL }. `text: None` removes it.
    Comment { target: CommentTarget<'a>, text: Option<&'a str> },
}

/// Which kind of relation a `COMMENT ON` names — PostgreSQL rejects a comment
/// whose keyword does not match the object's actual kind (42809).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentRelKind {
    Table,
    View,
    MaterializedView,
    Index,
    Sequence,
}

impl CommentRelKind {
    /// The noun PostgreSQL uses in `"x" is not a <noun>`.
    pub fn noun(self) -> &'static str {
        match self {
            CommentRelKind::Table => "table",
            CommentRelKind::View => "view",
            CommentRelKind::MaterializedView => "materialized view",
            CommentRelKind::Index => "index",
            CommentRelKind::Sequence => "sequence",
        }
    }
}

/// The object a COMMENT applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentTarget<'a> {
    /// TABLE / VIEW / MATERIALIZED VIEW / INDEX / SEQUENCE name.
    Relation { kind: CommentRelKind, name: QualName<'a> },
    /// COLUMN table.column.
    Column { relation: QualName<'a>, column: &'a str },
    /// SCHEMA name.
    Schema(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    Union,
    Intersect,
    Except,
}

/// A tree of set operations over SELECT leaves (INTERSECT binds tighter than
/// UNION/EXCEPT; UNION and EXCEPT are left-associative).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SetTree<'a> {
    Select(&'a Select<'a>),
    Op { operator: SetOp, all: bool, left: &'a SetTree<'a>, right: &'a SetTree<'a> },
}

/// A set-operation query plus the trailing ORDER BY / LIMIT / OFFSET that apply
/// to the whole combined result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SetQuery<'a> {
    /// WITH CTEs prefixed to the whole set operation.
    pub with: &'a [Cte<'a>],
    pub body: &'a SetTree<'a>,
    pub order_by: &'a [OrderBy<'a>],
    pub limit: Option<&'a Expr<'a>>,
    pub offset: Option<&'a Expr<'a>>,
    /// `FETCH FIRST n ROWS WITH TIES`: after the limit, also keep rows tying
    /// with the last one on the `ORDER BY` keys.
    pub with_ties: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Select<'a> {
    pub items: &'a [SelectItem<'a>],
    pub distinct: bool,
    /// `DISTINCT ON (exprs)`: keep the first row per distinct value of these
    /// expressions (in ORDER BY order). Empty = plain DISTINCT or none.
    pub distinct_on: &'a [&'a Expr<'a>],
    pub from: Option<FromClause<'a>>,
    pub where_clause: Option<&'a Expr<'a>>,
    pub group_by: &'a [&'a Expr<'a>],
    /// Grouping sets for `ROLLUP`/`CUBE`/`GROUPING SETS`. Each element is a
    /// bitmask over `group_by` indices selecting the columns that group in that
    /// set (bit *i* set = `group_by[i]` participates; a cleared bit means that
    /// column is NULL in the set's output rows). Empty means a plain
    /// `GROUP BY`: a single implicit set of all `group_by` columns.
    pub grouping_sets: &'a [u64],
    pub having: Option<&'a Expr<'a>>,
    pub order_by: &'a [OrderBy<'a>],
    pub limit: Option<&'a Expr<'a>>,
    pub offset: Option<&'a Expr<'a>>,
    /// `FETCH FIRST n ROWS WITH TIES`: after the limit, also keep rows tying
    /// with the last one on the `ORDER BY` keys.
    pub with_ties: bool,
    /// Non-recursive `WITH` common table expressions. Expanded into derived
    /// tables before execution; empty after expansion.
    pub with: &'a [Cte<'a>],
    /// When present, this "select" is actually a set-operation query (used in
    /// subquery position): its rows come from `set_body`, and only `order_by`
    /// / `limit` / `offset` above apply. `items`/`from`/etc. are unused.
    pub set_body: Option<&'a SetTree<'a>>,
}

/// One `WITH name [(col, ...)] AS (SELECT ...)` common table expression.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cte<'a> {
    pub name: &'a str,
    /// Optional output-column rename list (`WITH t(n) AS ...`); empty = none.
    pub columns: &'a [&'a str],
    /// The WITH clause carried the RECURSIVE keyword (a self-referencing body
    /// is executed by fixpoint iteration rather than inline expansion).
    pub recursive: bool,
    pub query: &'a Select<'a>,
}

/// The materialized rows of a recursive CTE, bound during CTE expansion so a
/// `FROM cte_name` reference resolves to a pre-computed row set instead of an
/// inline subquery. Rows are projected-encoded; column types are carried as
/// `(type oid, typlen)` pairs so this stays free of storage-layer types.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MaterializedCte<'a> {
    pub column_names: &'a [&'a str],
    pub column_types: &'a [(i32, i16)],
    pub rows: &'a [&'a [u8]],
}

/// A base table plus a chain of joins (nested-loop order).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FromClause<'a> {
    /// (table name, optional alias).
    pub base: TableRef<'a>,
    pub joins: &'a [Join<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TableRef<'a> {
    /// Optional schema qualifier (pg_catalog / information_schema / public).
    pub schema: Option<&'a str>,
    pub table: &'a str,
    pub alias: Option<&'a str>,
    /// Derived table: `FROM (SELECT ...) alias`. When set, `table` is empty and
    /// `alias` is the (required) correlation name.
    pub subquery: Option<&'a Select<'a>>,
    /// Table function: `FROM func(args) alias`. When set, `table` is the
    /// function name and these are its argument expressions.
    pub func_args: Option<&'a [&'a Expr<'a>]>,
    /// Column-alias list (`alias(c1, c2, ...)`): renames the output columns of a
    /// derived table or a table function. A table function has a single output
    /// column, so it accepts exactly one name.
    pub col_alias: Option<&'a [&'a str]>,
    /// Materialized recursive-CTE reference: when set, this FROM item reads the
    /// pre-computed row set instead of a table or subquery.
    pub cte: Option<&'a MaterializedCte<'a>>,
    /// `func(args) WITH ORDINALITY`: append a 1-based `bigint` ordinality column
    /// to a table function's output. Only valid on a table-function FROM item.
    pub with_ordinality: bool,
}

/// Upper bound on `USING (c1, ...)` column-list length (and thus on merged
/// columns per join).
pub const MAX_USING_COLUMNS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Join<'a> {
    pub table: TableRef<'a>,
    pub kind: JoinKind,
    /// ON condition; None for CROSS JOIN and for USING/NATURAL joins (whose
    /// equality predicate is synthesized at plan time, where the joined
    /// tables' columns are known).
    pub on: Option<&'a Expr<'a>>,
    /// `USING (c1, ...)` column names. Each names one column of the left join
    /// tree and one of the right table; the pair is merged into a single
    /// output column.
    pub using_columns: Option<&'a [&'a str]>,
    /// NATURAL join: the using-column list is every common column name,
    /// resolved at plan time.
    pub natural: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SelectItem<'a> {
    /// `*`
    Wildcard,
    /// `t.*`: every column of the named FROM item (its own copies, even for
    /// USING/NATURAL-merged columns).
    TableWildcard(&'a str),
    /// `(expr).*`: expand a record-valued expression into its fields as
    /// separate columns (`(ROW(1,2)).*`, `(json_each(j)).*`, `(t).*`).
    RecordStar(&'a Expr<'a>),
    Expr { expression: &'a Expr<'a>, alias: Option<&'a str> },
}

/// A window function's `OVER (PARTITION BY ... ORDER BY ...)` clause. Only the
/// default frame is supported; an explicit ROWS/RANGE frame is rejected.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowSpec<'a> {
    pub partition_by: &'a [&'a Expr<'a>],
    pub order_by: &'a [OrderBy<'a>],
    /// Explicit `ROWS`/`RANGE`/`GROUPS` frame; None = the default frame
    /// (`RANGE UNBOUNDED PRECEDING AND CURRENT ROW`).
    pub frame: Option<WindowFrame<'a>>,
}

/// An explicit window frame clause.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowFrame<'a> {
    pub units: FrameUnits,
    pub start: FrameBound<'a>,
    pub end: FrameBound<'a>,
    pub exclusion: FrameExclusion,
}

/// The frame's `EXCLUDE` clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameExclusion {
    /// `EXCLUDE NO OTHERS` (the default): nothing removed.
    NoOthers,
    /// `EXCLUDE CURRENT ROW`.
    CurrentRow,
    /// `EXCLUDE GROUP`: the current row and its ORDER BY peers.
    Group,
    /// `EXCLUDE TIES`: the peers but not the current row itself.
    Ties,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameUnits {
    Rows,
    Range,
    Groups,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameBound<'a> {
    UnboundedPreceding,
    Preceding(&'a Expr<'a>),
    CurrentRow,
    Following(&'a Expr<'a>),
    UnboundedFollowing,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrderBy<'a> {
    pub expression: &'a Expr<'a>,
    pub descending: bool,
    /// NULLs sort first. PostgreSQL's default is NULLS LAST for ASC and
    /// NULLS FIRST for DESC.
    pub nulls_first: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreateTable<'a> {
    pub name: QualName<'a>,
    pub columns: &'a [ColumnDef<'a>],
    /// Table-level constraints (multi-column PK/UNIQUE, CHECK, FOREIGN KEY),
    /// plus column-level CHECK/REFERENCES desugared into this list.
    pub constraints: &'a [TableConstraint<'a>],
    /// `LIKE source [INCLUDING ...]` elements, expanded against the catalog
    /// when the statement runs.
    pub likes: &'a [LikeClause<'a>],
    pub if_not_exists: bool,
}

/// One `LIKE source [INCLUDING ...]` element of a `CREATE TABLE`. The copied
/// columns always carry their name, type and NOT NULL; each flag adds one more
/// group, exactly as PostgreSQL splits them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LikeClause<'a> {
    /// How many of `CreateTable::columns` precede this element, so
    /// `(z int, LIKE src, w text)` keeps PostgreSQL's column order.
    pub at: usize,
    pub source: QualName<'a>,
    /// `INCLUDING DEFAULTS`.
    pub defaults: bool,
    /// `INCLUDING CONSTRAINTS` — CHECK constraints. NOT NULL is not part of
    /// this group; it always copies.
    pub constraints: bool,
    /// `INCLUDING INDEXES` — PRIMARY KEY, UNIQUE, and secondary indexes.
    pub indexes: bool,
    /// `INCLUDING IDENTITY` — the auto-increment flag.
    pub identity: bool,
    /// `INCLUDING GENERATED` — the STORED generation expression; without it a
    /// generated column is copied as a plain column.
    pub generated: bool,
}

/// A table-level constraint, or a column-level CHECK/REFERENCES desugared to
/// name its single column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TableConstraint<'a> {
    PrimaryKey {
        name: Option<&'a str>,
        columns: &'a [&'a str],
    },
    Unique {
        name: Option<&'a str>,
        columns: &'a [&'a str],
    },
    Check {
        name: Option<&'a str>,
        expression: &'a Expr<'a>,
        /// Source text of the predicate, stored durably and re-parsed at
        /// enforcement time.
        text: &'a str,
    },
    ForeignKey {
        name: Option<&'a str>,
        columns: &'a [&'a str],
        parent: QualName<'a>,
        /// Referenced columns; empty means "the parent's primary key".
        parent_cols: &'a [&'a str],
        on_delete: FkAction,
        on_update: FkAction,
    },
}

/// A MIN/MAXVALUE option, three-valued so ALTER SEQUENCE can tell "left alone"
/// (`Unset`) from "reset to the type default" (`NoBound`) from an explicit value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqBound {
    Unset,
    NoBound,
    Value(i64),
}

/// Parsed CREATE/ALTER SEQUENCE options, each `None`/`Unset` when the clause was
/// omitted. The executor computes defaults and validates; for ALTER an omitted
/// option keeps the sequence's current setting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeqOptions<'a> {
    /// AS <type> — the raw type name (`smallint`/`integer`/`bigint` and aliases).
    pub data_type: Option<&'a str>,
    pub increment: Option<i64>,
    pub min_value: SeqBound,
    pub max_value: SeqBound,
    pub start: Option<i64>,
    pub cache: Option<i64>,
    /// Some(true) = CYCLE, Some(false) = NO CYCLE, None = unspecified.
    pub cycle: Option<bool>,
    /// None = no RESTART clause; Some(None) = RESTART (to start value);
    /// Some(Some(n)) = RESTART WITH n. (ALTER only.)
    pub restart: Option<Option<i64>>,
}

impl<'a> SeqOptions<'a> {
    pub const EMPTY: SeqOptions<'a> = SeqOptions {
        data_type: None,
        increment: None,
        min_value: SeqBound::Unset,
        max_value: SeqBound::Unset,
        start: None,
        cache: None,
        cycle: None,
        restart: None,
    };
}

/// A `CREATE DOMAIN name [AS] basetype[(typmod)] [constraint...]`. The base
/// type name and its typmod are raw (resolved in exec); constraint predicate
/// and default expressions are raw source text, re-parsed when enforced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreateDomain<'a> {
    pub name: QualName<'a>,
    pub base_type: &'a str,
    pub base_type_mod: i32,
    pub not_null: bool,
    pub default_text: Option<&'a str>,
    pub checks: &'a [DomainCheck<'a>],
}

/// One domain `[CONSTRAINT name] CHECK (expr)` — `name` is `None` when the
/// constraint was written unnamed (the executor generates `<domain>_check`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DomainCheck<'a> {
    pub name: Option<&'a str>,
    pub expression: &'a str,
}

/// One `ALTER DOMAIN` action.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AlterDomainAction<'a> {
    AddCheck(DomainCheck<'a>),
    DropConstraint { name: &'a str, if_exists: bool },
    SetNotNull,
    DropNotNull,
    SetDefault(&'a str),
    DropDefault,
}

/// Referential action for a foreign key's ON DELETE / ON UPDATE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FkAction {
    /// NO ACTION (the default) and RESTRICT both reject; NO ACTION is
    /// deferrable in PostgreSQL, RESTRICT is not, but we check immediately.
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DropTable<'a> {
    pub names: &'a [QualName<'a>],
    pub if_exists: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColumnDef<'a> {
    pub name: &'a str,
    pub type_name: &'a str,
    /// PostgreSQL atttypmod for the declared type: -1 when no `(...)` modifier.
    /// varchar(n)/char(n) encode `n + 4`; numeric(p,s) encodes `((p<<16)|s)+4`.
    pub type_mod: i32,
    pub not_null: bool,
    pub unique: bool,
    pub primary: bool,
    /// DEFAULT expression. A literal-only default is folded to a constant at
    /// execution; anything with a function call (`now()`, `nextval(...)`, …) is
    /// stored as `default_text` and re-evaluated per inserted row.
    pub default: Option<&'a Expr<'a>>,
    /// The raw source text of the DEFAULT expression, for storing non-constant
    /// defaults and for `pg_get_expr` / `\d` reconstruction.
    pub default_text: Option<&'a str>,
    /// `GENERATED ALWAYS AS (expr) STORED`: the raw source text of the generation
    /// expression, computed from the row's other columns at insert/update.
    pub generated_text: Option<&'a str>,
    /// `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY [(options)]`.
    pub identity: Option<IdentitySpec>,
}

/// A `GENERATED ... AS IDENTITY` specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentitySpec {
    /// `ALWAYS` (reject explicit inserts) vs `BY DEFAULT` (explicit allowed).
    pub always: bool,
    /// `START WITH n` — the first value handed out (default 1).
    pub start: Option<i64>,
    /// `INCREMENT BY n` — the step (default 1).
    pub increment: Option<i64>,
}

/// The result of parsing a `GENERATED` column clause.
pub enum ColGen<'a> {
    /// `ALWAYS AS (expr) STORED`.
    Generated(&'a str),
    /// `{ ALWAYS | BY DEFAULT } AS IDENTITY [(options)]`.
    Identity(IdentitySpec),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CopyStmt<'a> {
    pub table: QualName<'a>,
    /// Empty means "all columns in table order".
    pub columns: &'a [&'a str],
    /// `TO STDOUT` when true; `FROM STDIN` otherwise.
    pub to: bool,
    pub options: CopyOptions<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CopyFormat {
    /// Tab-delimited, backslash-escaped (the default).
    Text,
    /// Comma-separated with double-quote quoting.
    Csv,
    /// PostgreSQL's length-framed binary wire format.
    Binary,
}

/// The `WITH (...)` options of a COPY, as written. Character options are stored
/// as given; the effective values (format defaults filled in) are resolved at
/// execution. The `force_*` column lists name columns, resolved against the
/// table there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CopyOptions<'a> {
    pub format: CopyFormat,
    /// Field separator; `None` = the format default (tab for text, comma CSV).
    pub delimiter: Option<u8>,
    /// The string that stands for NULL; `None` = the format default (`\N` text,
    /// empty CSV).
    pub null_string: Option<&'a str>,
    /// CSV only: emit / expect a header line of column names.
    pub header: bool,
    /// CSV only: the quoting character (default `"`).
    pub quote: Option<u8>,
    /// CSV only: the character that escapes a quote inside a quoted field
    /// (default: the quote character itself).
    pub escape: Option<u8>,
    /// CSV output: quote every column unconditionally (`FORCE_QUOTE *`).
    pub force_quote_all: bool,
    /// CSV output: columns to always quote.
    pub force_quote: &'a [&'a str],
    /// CSV input: columns whose empty unquoted field is the empty string, not
    /// NULL, even when it matches the NULL string.
    pub force_not_null: &'a [&'a str],
    /// CSV input: columns whose quoted value matching the NULL string is NULL.
    pub force_null: &'a [&'a str],
}

impl CopyOptions<'_> {
    /// The default text-format options.
    pub const TEXT: CopyOptions<'static> = CopyOptions {
        format: CopyFormat::Text,
        delimiter: None,
        null_string: None,
        header: false,
        quote: None,
        escape: None,
        force_quote_all: false,
        force_quote: &[],
        force_not_null: &[],
        force_null: &[],
    };

    /// The effective field delimiter (text/CSV; unused for binary).
    pub fn delimiter_byte(&self) -> u8 {
        self.delimiter.unwrap_or(match self.format {
            CopyFormat::Csv => b',',
            CopyFormat::Text | CopyFormat::Binary => b'\t',
        })
    }

    /// The effective quote character (CSV).
    pub fn quote_byte(&self) -> u8 {
        self.quote.unwrap_or(b'"')
    }

    /// The effective escape character (CSV): the quote character by default.
    pub fn escape_byte(&self) -> u8 {
        self.escape.unwrap_or_else(|| self.quote_byte())
    }

    /// The effective NULL sentinel (text/CSV; unused for binary).
    pub fn null_str(&self) -> &str {
        self.null_string.unwrap_or(match self.format {
            CopyFormat::Csv => "",
            CopyFormat::Text | CopyFormat::Binary => "\\N",
        })
    }

    pub fn is_csv(&self) -> bool {
        matches!(self.format, CopyFormat::Csv)
    }

    pub fn is_binary(&self) -> bool {
        matches!(self.format, CopyFormat::Binary)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Insert<'a> {
    pub table: QualName<'a>,
    /// Empty means "all columns in table order".
    pub columns: &'a [&'a str],
    /// `VALUES` rows. Empty when the source is a `SELECT` (`select` is set).
    pub rows: &'a [&'a [&'a Expr<'a>]],
    /// `INSERT ... SELECT` source, when present. Mutually exclusive with `rows`.
    pub select: Option<&'a Select<'a>>,
    /// ON CONFLICT clause, when present.
    pub on_conflict: Option<OnConflict<'a>>,
    /// RETURNING items (empty = none).
    pub returning: &'a [SelectItem<'a>],
    /// `OVERRIDING { SYSTEM | USER } VALUE` for identity columns.
    pub overriding: Overriding,
}

/// `OVERRIDING` mode for `INSERT` into a table with identity columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overriding {
    /// No OVERRIDING clause.
    None,
    /// `OVERRIDING SYSTEM VALUE` — an explicit value into a `GENERATED ALWAYS`
    /// identity column is accepted instead of rejected.
    System,
    /// `OVERRIDING USER VALUE` — an explicit value into a `GENERATED BY DEFAULT`
    /// identity column is ignored in favor of the identity sequence.
    User,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OnConflict<'a> {
    /// Conflict-target columns (`ON CONFLICT (a, b)`); empty means any unique
    /// constraint or unique index.
    pub target: &'a [&'a str],
    /// `None` = DO NOTHING; `Some` = DO UPDATE SET .... Assignments may
    /// reference the target row's columns and `excluded.<col>` (the proposed
    /// row).
    pub update: Option<&'a [(&'a str, &'a Expr<'a>)]>,
    /// Optional WHERE on DO UPDATE.
    pub update_where: Option<&'a Expr<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Update<'a> {
    pub table: QualName<'a>,
    pub assignments: &'a [(&'a str, &'a Expr<'a>)],
    /// Extra tables joined for the assignment/WHERE (`UPDATE t SET ... FROM e`).
    pub from: Option<&'a FromClause<'a>>,
    pub where_clause: Option<&'a Expr<'a>>,
    pub returning: &'a [SelectItem<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Delete<'a> {
    pub table: QualName<'a>,
    /// Extra tables joined for the WHERE (`DELETE FROM t USING e`).
    pub using: Option<&'a FromClause<'a>>,
    pub where_clause: Option<&'a Expr<'a>>,
    pub returning: &'a [SelectItem<'a>],
}

/// `MERGE INTO target [AS alias] USING source [AS alias] ON cond WHEN ...`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Merge<'a> {
    pub target: QualName<'a>,
    /// Correlation name for the target (defaults to its table name).
    pub target_alias: Option<&'a str>,
    /// The data source: a table, subquery, or `(VALUES ...)`.
    pub source: TableRef<'a>,
    pub on: &'a Expr<'a>,
    pub whens: &'a [MergeWhen<'a>],
}

/// One `WHEN [NOT] MATCHED [AND cond] THEN action` clause.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MergeWhen<'a> {
    pub matched: bool,
    /// The `AND cond` refinement (None = always applies).
    pub cond: Option<&'a Expr<'a>>,
    pub action: MergeAction<'a>,
}

/// A MERGE clause's action.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MergeAction<'a> {
    /// `UPDATE SET col = expr, ...` (WHEN MATCHED only).
    Update(&'a [(&'a str, &'a Expr<'a>)]),
    /// `DELETE` (WHEN MATCHED only).
    Delete,
    /// `INSERT [(cols)] VALUES (exprs)` or `INSERT DEFAULT VALUES` (WHEN NOT
    /// MATCHED only). Empty `values` means DEFAULT VALUES.
    Insert { columns: &'a [&'a str], values: &'a [&'a Expr<'a>], default_values: bool },
    /// `DO NOTHING`.
    DoNothing,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlterTable<'a> {
    pub table: QualName<'a>,
    /// One or more subcommands. PostgreSQL applies a comma-separated list in a
    /// fixed pass order (drops, then type changes, then adds, then constraints,
    /// then column-attribute changes), not left to right; the parser sorts the
    /// list into that order. The standalone forms (RENAME …, SET SCHEMA) are
    /// never combined and arrive as a single-element list.
    pub actions: &'a [AlterAction<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AlterAction<'a> {
    RenameTable(&'a str),
    /// ALTER TABLE ... SET SCHEMA new_schema.
    SetSchema(&'a str),
    RenameColumn { from: &'a str, to: &'a str },
    AddColumn(ColumnDef<'a>),
    DropColumn(&'a str),
    /// ALTER [COLUMN] col SET DEFAULT expr.
    SetDefault { column: &'a str, value: &'a Expr<'a>, value_text: &'a str },
    /// ALTER [COLUMN] col DROP DEFAULT.
    DropDefault { column: &'a str },
    /// ALTER [COLUMN] col ADD GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY.
    AddIdentity { column: &'a str, spec: IdentitySpec },
    /// ALTER [COLUMN] col DROP IDENTITY [IF EXISTS].
    DropIdentity { column: &'a str, if_exists: bool },
    /// ALTER [COLUMN] col SET NOT NULL — validated against existing rows.
    SetNotNull { column: &'a str },
    /// ALTER [COLUMN] col DROP NOT NULL.
    DropNotNull { column: &'a str },
    /// ALTER [COLUMN] col [SET DATA] TYPE newtype [USING expr]. Without `using`
    /// the stored value is cast through the assignment cast; with it, `using`
    /// is evaluated per row (the old columns in scope) and cast to the type.
    AlterColumnType {
        column: &'a str,
        type_name: &'a str,
        type_mod: i32,
        using: Option<&'a Expr<'a>>,
    },
    /// ALTER TABLE ... ADD [CONSTRAINT name] <table constraint>. Existing rows
    /// are validated against the new constraint before it is attached.
    AddConstraint(TableConstraint<'a>),
    /// ALTER TABLE ... DROP CONSTRAINT [IF EXISTS] name.
    DropConstraint { name: &'a str, if_exists: bool },
    /// ALTER TABLE ... RENAME CONSTRAINT old TO new.
    RenameConstraint { from: &'a str, to: &'a str },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Expr<'a> {
    Null,
    Bool(bool),
    /// Integer literal that fit in i64.
    Int(i64),
    Float(f64),
    /// Decimal/exponent literal, kept as text; parsed to NUMERIC at eval
    /// time. PostgreSQL types these as numeric, not float8.
    NumericLit(&'a str),
    Str(&'a str),
    /// Bit-string literal (`B'1010'` / `X'1F'`): the canonical `'0'`/`'1'`
    /// characters, typed `bit(len)`.
    BitLit(&'a str),
    Column {
        /// Optional table/alias qualifier.
        qualifier: Option<&'a str>,
        name: &'a str,
    },
    Param(u32),
    Unary {
        operator: UnaryOp,
        operand: &'a Expr<'a>,
    },
    Binary {
        operator: BinaryOp,
        left: &'a Expr<'a>,
        right: &'a Expr<'a>,
    },
    Cast {
        operand: &'a Expr<'a>,
        type_name: &'a str,
        /// Encoded atttypmod for `::numeric(p,s)` / `::varchar(n)`, or -1.
        type_mod: i32,
    },
    IsNull {
        operand: &'a Expr<'a>,
        negated: bool,
    },
    /// Function call. `star` marks `count(*)`; `distinct` marks
    /// `count(DISTINCT x)`; `order_by` carries an aggregate's `ORDER BY`
    /// (e.g. `string_agg(x, ',' ORDER BY y)`), empty otherwise.
    Call {
        name: &'a str,
        args: &'a [&'a Expr<'a>],
        star: bool,
        distinct: bool,
        order_by: &'a [OrderBy<'a>],
        /// `OVER (...)` window clause, when the call is a window function.
        over: Option<&'a WindowSpec<'a>>,
        /// `FILTER (WHERE cond)` on an aggregate: rows where `cond` is not true
        /// are excluded from that aggregate.
        filter: Option<&'a Expr<'a>>,
    },
    /// `expression [NOT] IN (list)`.
    InList {
        operand: &'a Expr<'a>,
        list: &'a [&'a Expr<'a>],
        negated: bool,
    },
    /// `expression [NOT] BETWEEN low AND high`.
    Between {
        operand: &'a Expr<'a>,
        low: &'a Expr<'a>,
        high: &'a Expr<'a>,
        negated: bool,
    },
    /// `expression [NOT] LIKE/ILIKE pattern`.
    Like {
        operand: &'a Expr<'a>,
        pattern: &'a Expr<'a>,
        negated: bool,
        case_insensitive: bool,
        /// `ESCAPE c`: the character that quotes a literal `%` or `_` in the
        /// pattern. `None` is PostgreSQL's default of a backslash; an empty
        /// string disables escaping entirely.
        escape: Option<&'a Expr<'a>>,
    },
    /// POSIX regex match: `operand ~ pattern` (`!~`, `~*`, `!~*`).
    Match {
        operand: &'a Expr<'a>,
        pattern: &'a Expr<'a>,
        negated: bool,
        case_insensitive: bool,
    },
    /// `CASE [operand] WHEN .. THEN .. [ELSE ..] END`.
    Case {
        operand: Option<&'a Expr<'a>>,
        whens: &'a [(&'a Expr<'a>, &'a Expr<'a>)],
        otherwise: Option<&'a Expr<'a>>,
        /// True when this `CASE` is the desugaring of a syntactic construct that
        /// is not itself a `CASE` — `IS TRUE`, `IS DISTINCT FROM`. PostgreSQL
        /// labels those output columns `?column?`, not `case`, so the flag lets
        /// naming tell them from a `CASE` the query actually wrote.
        synthetic: bool,
    },
    /// The DEFAULT keyword inside INSERT VALUES.
    DefaultMarker,
    /// Scalar subquery: must yield one column, at most one row.
    Subquery(&'a Select<'a>),
    /// `expression [NOT] IN (SELECT ...)`.
    InSubquery {
        operand: &'a Expr<'a>,
        select: &'a Select<'a>,
        negated: bool,
    },
    /// `EXISTS (SELECT ...)`: true when the subquery yields at least one row.
    /// `NOT EXISTS` parses as `NOT` wrapping this.
    Exists(&'a Select<'a>),
    /// `ARRAY(SELECT ...)`: builds a one-dimensional array from a single-column
    /// subquery's rows, in row order.
    ArraySubquery(&'a Select<'a>),
    /// `ARRAY[e1, e2, ...]` array constructor.
    Array(&'a [&'a Expr<'a>]),
    /// `base[index]` array element access (1-based).
    Subscript { base: &'a Expr<'a>, index: &'a Expr<'a> },
    /// `(base).field` composite field access. Used by driver introspection with
    /// the `_pg_expandarray` set function, whose result exposes `.x` (element)
    /// and `.n` (1-based ordinal).
    Field { base: &'a Expr<'a>, field: &'a str },
    /// `t.*` in an expression position (a whole-row reference). Supported
    /// only as a `count()` argument; anywhere else it is rejected at type
    /// analysis (record values are not first-class here).
    WholeRow(&'a str),
    /// A three-part column reference `schema.table.column`: the qualifier
    /// pair must match an unaliased FROM entry that really is that schema's
    /// table (PostgreSQL's rule), then resolves like `table.column`.
    SchemaColumn { schema: &'a str, table: &'a str, name: &'a str },
    /// `operand operator ANY/ALL (array)` — quantified comparison.
    AnyAll {
        operand: &'a Expr<'a>,
        operator: BinaryOp,
        array: &'a Expr<'a>,
        all: bool,
    },
}

impl Expr<'_> {
    /// Whether this expression is an aggregate-function call.
    pub fn is_aggregate(&self) -> bool {
        matches!(
            self,
            Expr::Call { name, .. }
                if matches!(*name, "count" | "sum" | "avg" | "min" | "max" | "bool_and" | "bool_or" | "every" | "bit_and" | "bit_or" | "bit_xor" | "string_agg" | "array_agg" | "json_agg" | "jsonb_agg" | "json_object_agg" | "jsonb_object_agg" | "percentile_cont" | "percentile_disc" | "mode" | "var_pop" | "var_samp" | "variance" | "stddev_pop" | "stddev_samp" | "stddev" | "corr" | "covar_pop" | "covar_samp" | "regr_slope" | "regr_intercept" | "regr_r2" | "regr_count" | "regr_avgx" | "regr_avgy" | "regr_sxx" | "regr_syy" | "regr_sxy")
        )
    }

    /// True for an aggregate *use* — an aggregate call with no `OVER` clause,
    /// which is what makes a query grouped. `sum(x) OVER (...)` names an
    /// aggregate but is a window function: it groups nothing, and asking
    /// [`Self::is_aggregate`] (which only looks at the name) would say it does.
    pub fn is_aggregate_use(&self) -> bool {
        self.is_aggregate() && matches!(self, Expr::Call { over: None, .. })
    }

    /// True when the expression is a compile-time constant: only literals
    /// and pure operations over them, with no column/parameter/subquery/
    /// aggregate reference. PostgreSQL evaluates these at plan time, so
    /// their errors (division by zero, overflow) surface eagerly.
    pub fn is_constant(&self) -> bool {
        /// Set-returning functions expand to multiple rows and are never a
        /// foldable constant.
        /// Volatile sequence functions: never a foldable constant (they have
        /// side effects and must reach the sequence engine).
        fn is_sequence_function(name: &str) -> bool {
            name.eq_ignore_ascii_case("nextval")
                || name.eq_ignore_ascii_case("currval")
                || name.eq_ignore_ascii_case("lastval")
                || name.eq_ignore_ascii_case("setval")
        }
        fn is_set_returning(name: &str) -> bool {
            name.eq_ignore_ascii_case("unnest")
                || name.eq_ignore_ascii_case("generate_series")
                || name.eq_ignore_ascii_case("_pg_expandarray")
                || name.eq_ignore_ascii_case("regexp_matches")
                || name.eq_ignore_ascii_case("jsonb_object_keys")
                || name.eq_ignore_ascii_case("json_object_keys")
                || name.eq_ignore_ascii_case("jsonb_array_elements")
                || name.eq_ignore_ascii_case("json_array_elements")
                || name.eq_ignore_ascii_case("jsonb_array_elements_text")
                || name.eq_ignore_ascii_case("json_array_elements_text")
                || name.eq_ignore_ascii_case("json_each")
                || name.eq_ignore_ascii_case("jsonb_each")
                || name.eq_ignore_ascii_case("json_each_text")
                || name.eq_ignore_ascii_case("jsonb_each_text")
                || name.eq_ignore_ascii_case("regexp_split_to_table")
                || name.eq_ignore_ascii_case("string_to_table")
                || name.eq_ignore_ascii_case("generate_subscripts")
        }
        match self {
            Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_)
            | Expr::NumericLit(_) | Expr::Str(_) | Expr::BitLit(_) => true,
            Expr::WholeRow(_) | Expr::SchemaColumn { .. } => false,
            Expr::Column { .. } | Expr::Param(_) | Expr::Subquery(_)
            | Expr::InSubquery { .. } | Expr::Exists(_) | Expr::ArraySubquery(_)
            | Expr::DefaultMarker => false,
            Expr::Unary { operand, .. }
            | Expr::Cast { operand, .. }
            | Expr::IsNull { operand, .. } => operand.is_constant(),
            Expr::Binary { left, right, .. } => left.is_constant() && right.is_constant(),
            Expr::InList { operand, list, .. } => {
                operand.is_constant() && list.iter().all(|e| e.is_constant())
            }
            Expr::Between { operand, low, high, .. } => {
                operand.is_constant() && low.is_constant() && high.is_constant()
            }
            Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => {
                operand.is_constant() && pattern.is_constant()
            }
            Expr::Case { operand, whens, otherwise, .. } => {
                operand.map(|o| o.is_constant()).unwrap_or(true)
                    && whens.iter().all(|(c, r)| c.is_constant() && r.is_constant())
                    && otherwise.map(|e| e.is_constant()).unwrap_or(true)
            }
            // Aggregates, window functions, set-returning functions, and the
            // side-effecting sequence functions are never constant (the last
            // must reach the sequence engine, not be folded at plan time); other
            // calls are constant when their arguments are.
            Expr::Call { name, args, over, .. } => {
                over.is_none()
                    && !self.is_aggregate()
                    && !is_set_returning(name)
                    && !is_sequence_function(name)
                    && args.iter().all(|a| a.is_constant())
            }
            Expr::Array(items) => items.iter().all(|e| e.is_constant()),
            Expr::Subscript { base, index } => base.is_constant() && index.is_constant(),
            Expr::Field { base, .. } => base.is_constant(),
            Expr::AnyAll { operand, array, .. } => operand.is_constant() && array.is_constant(),
        }
    }

    /// Whether the expression tree contains a function call — the marker of a
    /// non-constant DEFAULT (`now()`, `nextval(...)`, `random()`, …) that must be
    /// evaluated per inserted row rather than folded once.
    pub fn contains_call(&self) -> bool {
        match self {
            Expr::Call { .. } => true,
            Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::NumericLit(_)
            | Expr::Str(_) | Expr::BitLit(_) | Expr::Column { .. } | Expr::WholeRow(_)
            | Expr::SchemaColumn { .. } | Expr::Param(_) | Expr::DefaultMarker => false,
            Expr::Unary { operand, .. } | Expr::Cast { operand, .. }
            | Expr::IsNull { operand, .. } | Expr::Field { base: operand, .. } => {
                operand.contains_call()
            }
            Expr::Binary { left, right, .. } | Expr::Subscript { base: left, index: right }
            | Expr::AnyAll { operand: left, array: right, .. } => {
                left.contains_call() || right.contains_call()
            }
            Expr::InList { operand, list, .. } => {
                operand.contains_call() || list.iter().any(|e| e.contains_call())
            }
            Expr::Between { operand, low, high, .. } => {
                operand.contains_call() || low.contains_call() || high.contains_call()
            }
            Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => {
                operand.contains_call() || pattern.contains_call()
            }
            Expr::Case { operand, whens, otherwise, .. } => {
                operand.map(|o| o.contains_call()).unwrap_or(false)
                    || whens.iter().any(|(c, r)| c.contains_call() || r.contains_call())
                    || otherwise.map(|o| o.contains_call()).unwrap_or(false)
            }
            Expr::Array(items) => items.iter().any(|e| e.contains_call()),
            // A subquery-bearing default is rejected elsewhere; treat it as
            // non-foldable to be safe.
            Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists(_)
            | Expr::ArraySubquery(_) => true,
        }
    }

    /// Whether the expression tree contains a subquery — disallowed in a column
    /// generation expression (0A000).
    pub fn contains_subquery(&self) -> bool {
        match self {
            Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists(_)
            | Expr::ArraySubquery(_) => true,
            Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::NumericLit(_)
            | Expr::Str(_) | Expr::BitLit(_) | Expr::Column { .. } | Expr::WholeRow(_)
            | Expr::SchemaColumn { .. } | Expr::Param(_) | Expr::DefaultMarker => false,
            Expr::Unary { operand, .. } | Expr::Cast { operand, .. }
            | Expr::IsNull { operand, .. } | Expr::Field { base: operand, .. } => {
                operand.contains_subquery()
            }
            Expr::Binary { left, right, .. } | Expr::Subscript { base: left, index: right }
            | Expr::AnyAll { operand: left, array: right, .. } => {
                left.contains_subquery() || right.contains_subquery()
            }
            Expr::Call { args, .. } => args.iter().any(|a| a.contains_subquery()),
            Expr::InList { operand, list, .. } => {
                operand.contains_subquery() || list.iter().any(|e| e.contains_subquery())
            }
            Expr::Between { operand, low, high, .. } => {
                operand.contains_subquery()
                    || low.contains_subquery()
                    || high.contains_subquery()
            }
            Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => {
                operand.contains_subquery() || pattern.contains_subquery()
            }
            Expr::Case { operand, whens, otherwise, .. } => {
                operand.map(|o| o.contains_subquery()).unwrap_or(false)
                    || whens
                        .iter()
                        .any(|(c, r)| c.contains_subquery() || r.contains_subquery())
                    || otherwise.map(|o| o.contains_subquery()).unwrap_or(false)
            }
            Expr::Array(items) => items.iter().any(|e| e.contains_subquery()),
        }
    }

    /// Whether the tree calls a volatile or stable function — one whose value can
    /// vary across rows or statements (`now`, `random`, `nextval`, `current_user`,
    /// …). Such a function is disallowed in a column generation expression
    /// (PostgreSQL requires immutability, 42P17). Every other function is treated
    /// as immutable.
    pub fn contains_nonimmutable_function(&self) -> Option<&str> {
        fn is_nonimmutable(name: &str) -> bool {
            const NAMES: &[&str] = &[
                "now", "current_timestamp", "current_date", "current_time", "localtime",
                "localtimestamp", "statement_timestamp", "transaction_timestamp",
                "clock_timestamp", "timeofday", "random", "random_normal", "nextval", "currval",
                "lastval", "setval", "gen_random_uuid", "uuid_generate_v1", "uuid_generate_v4",
                "current_user", "session_user", "user", "current_role", "current_schema",
                "current_database", "current_catalog", "pg_backend_pid", "txid_current",
                "pg_current_xact_id",
            ];
            NAMES.iter().any(|n| name.eq_ignore_ascii_case(n))
        }
        match self {
            Expr::Call { name, args, .. } => {
                if is_nonimmutable(name) {
                    return Some(name);
                }
                args.iter().find_map(|a| a.contains_nonimmutable_function())
            }
            Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::NumericLit(_)
            | Expr::Str(_) | Expr::BitLit(_) | Expr::Column { .. } | Expr::WholeRow(_)
            | Expr::SchemaColumn { .. } | Expr::Param(_) | Expr::DefaultMarker
            | Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists(_)
            | Expr::ArraySubquery(_) => None,
            Expr::Unary { operand, .. } | Expr::Cast { operand, .. }
            | Expr::IsNull { operand, .. } | Expr::Field { base: operand, .. } => {
                operand.contains_nonimmutable_function()
            }
            Expr::Binary { left, right, .. } | Expr::Subscript { base: left, index: right }
            | Expr::AnyAll { operand: left, array: right, .. } => left
                .contains_nonimmutable_function()
                .or_else(|| right.contains_nonimmutable_function()),
            Expr::InList { operand, list, .. } => operand
                .contains_nonimmutable_function()
                .or_else(|| list.iter().find_map(|e| e.contains_nonimmutable_function())),
            Expr::Between { operand, low, high, .. } => operand
                .contains_nonimmutable_function()
                .or_else(|| low.contains_nonimmutable_function())
                .or_else(|| high.contains_nonimmutable_function()),
            Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => operand
                .contains_nonimmutable_function()
                .or_else(|| pattern.contains_nonimmutable_function()),
            Expr::Case { operand, whens, otherwise, .. } => operand
                .and_then(|o| o.contains_nonimmutable_function())
                .or_else(|| {
                    whens.iter().find_map(|(c, r)| {
                        c.contains_nonimmutable_function()
                            .or_else(|| r.contains_nonimmutable_function())
                    })
                })
                .or_else(|| otherwise.and_then(|o| o.contains_nonimmutable_function())),
            Expr::Array(items) => {
                items.iter().find_map(|e| e.contains_nonimmutable_function())
            }
        }
    }

    /// Visits every column reference in the tree (by unqualified name), for
    /// validating a generation expression's dependencies.
    pub fn for_each_column(&self, f: &mut dyn FnMut(&str)) {
        match self {
            Expr::Column { name, .. } => f(name),
            Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::NumericLit(_)
            | Expr::Str(_) | Expr::BitLit(_) | Expr::WholeRow(_) | Expr::SchemaColumn { .. }
            | Expr::Param(_) | Expr::DefaultMarker | Expr::Subquery(_)
            | Expr::InSubquery { .. } | Expr::Exists(_) | Expr::ArraySubquery(_) => {}
            Expr::Unary { operand, .. } | Expr::Cast { operand, .. }
            | Expr::IsNull { operand, .. } | Expr::Field { base: operand, .. } => {
                operand.for_each_column(f)
            }
            Expr::Binary { left, right, .. } | Expr::Subscript { base: left, index: right }
            | Expr::AnyAll { operand: left, array: right, .. } => {
                left.for_each_column(f);
                right.for_each_column(f);
            }
            Expr::Call { args, .. } => args.iter().for_each(|a| a.for_each_column(f)),
            Expr::InList { operand, list, .. } => {
                operand.for_each_column(f);
                list.iter().for_each(|e| e.for_each_column(f));
            }
            Expr::Between { operand, low, high, .. } => {
                operand.for_each_column(f);
                low.for_each_column(f);
                high.for_each_column(f);
            }
            Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => {
                operand.for_each_column(f);
                pattern.for_each_column(f);
            }
            Expr::Case { operand, whens, otherwise, .. } => {
                if let Some(o) = operand {
                    o.for_each_column(f);
                }
                for (c, r) in *whens {
                    c.for_each_column(f);
                    r.for_each_column(f);
                }
                if let Some(o) = otherwise {
                    o.for_each_column(f);
                }
            }
            Expr::Array(items) => items.iter().for_each(|e| e.for_each_column(f)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
    /// `~` bitwise NOT (integers).
    BitNot,
    /// PostgreSQL's prefix arithmetic operators `|/`, `||/` and `@`. They are
    /// operators rather than the functions they compute — a column they produce
    /// is `?column?`, not `sqrt` — so they are their own nodes and delegate to
    /// those functions when evaluated.
    SquareRoot,
    CubeRoot,
    AbsoluteValue,
}

impl UnaryOp {
    /// The scalar function a prefix arithmetic operator computes.
    pub fn arithmetic_function(self) -> Option<&'static str> {
        match self {
            UnaryOp::SquareRoot => Some("sqrt"),
            UnaryOp::CubeRoot => Some("cbrt"),
            UnaryOp::AbsoluteValue => Some("abs"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    Concat,
    /// `json -> key/index` — returns json/jsonb.
    JsonGet,
    /// `json ->> key/index` — returns text.
    JsonGetText,
    /// `json #> path` — extract by text[] path, returns json/jsonb.
    JsonPath,
    /// `json #>> path` — extract by text[] path, returns text.
    JsonPathText,
    /// `jsonb #- path` — delete the value at a text[] path, returns jsonb.
    JsonDeletePath,
    /// `jsonb ? key` — does the object have the key (or the array the element)?
    JsonExists,
    /// `jsonb ?| array` — does it have any of the keys?
    JsonExistsAny,
    /// `jsonb ?& array` — does it have all of the keys?
    JsonExistsAll,
    /// Integer bitwise operators.
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    /// `^` exponentiation (double precision).
    Pow,
    /// `@>` contains, `<@` contained by, `&&` overlaps (ranges).
    Contains,
    ContainedBy,
    Overlaps,
    /// `&<` does not extend right, `&>` does not extend left, `-|-` adjacent
    /// (ranges). `<<`/`>>` reuse `Shl`/`Shr`; `+`/`-`/`*` reuse the arithmetic
    /// operators (dispatched on range operands).
    NotRightOf,
    NotLeftOf,
    Adjacent,
    /// `<<=` network "is contained within or equals", `>>=` network "contains
    /// or equals". (`<<`/`>>` reuse `Shl`/`Shr`, dispatched on network operands.)
    NetContainedEq,
    NetContainsEq,
    /// Pattern match, used only as the operator of a quantified `LIKE ANY/ALL`
    /// (`ILike` is the case-insensitive form). Plain `x LIKE y` uses `Expr::Like`.
    Like,
    ILike,
}

impl BinaryOp {
    /// Binding power for the Pratt parser; higher binds tighter.
    /// Mirrors PostgreSQL's operator precedence table.
    pub fn precedence(self) -> u8 {
        match self {
            Self::Or => 1,
            Self::And => 2,
            Self::Eq | Self::NotEq | Self::Lt | Self::LtEq | Self::Gt | Self::GtEq => 4,
            // Containment/overlap/adjacency operators bind like comparisons.
            Self::Contains | Self::ContainedBy | Self::Overlaps => 4,
            Self::NotRightOf | Self::NotLeftOf | Self::Adjacent => 4,
            Self::NetContainedEq | Self::NetContainsEq => 4,
            Self::Like | Self::ILike => 4,
            Self::JsonExists | Self::JsonExistsAny | Self::JsonExistsAll => 4,
            Self::Concat => 5,
            // Bitwise OR/XOR/AND and shifts sit between comparison and addition,
            // matching PostgreSQL (they are non-standard, mid-precedence).
            Self::BitOr | Self::BitXor => 5,
            Self::BitAnd => 5,
            Self::Shl | Self::Shr => 5,
            Self::Add | Self::Sub => 6,
            Self::Mul | Self::Div | Self::Mod => 7,
            // Exponentiation binds tighter than multiplication.
            Self::Pow => 8,
            // JSON accessors bind tightest among binary operators.
            Self::JsonGet | Self::JsonGetText | Self::JsonPath | Self::JsonPathText
            | Self::JsonDeletePath => 9,
        }
    }
}
