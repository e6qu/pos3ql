//! Arena-allocated AST. Every node is `Copy`; child links are arena
//! references, so an entire statement tree lives exactly as long as the
//! per-statement arena and costs nothing to drop.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stmt<'a> {
    Select(Select<'a>),
    CreateTable(CreateTable<'a>),
    Insert(Insert<'a>),
    Update(Update<'a>),
    Delete(Delete<'a>),
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
    /// CREATE [OR REPLACE] VIEW name AS <select>. `sql` is the raw SELECT text,
    /// stored and re-expanded as a derived table at query time.
    CreateView { name: &'a str, or_replace: bool, sql: &'a str },
    /// DROP VIEW [IF EXISTS] name.
    DropView { name: &'a str, if_exists: bool },
    /// CREATE [UNIQUE] INDEX name ON table (col, ...).
    CreateIndex {
        name: &'a str,
        table: &'a str,
        columns: &'a [&'a str],
        unique: bool,
    },
    /// DROP INDEX [IF EXISTS] name.
    DropIndex { name: &'a str, if_exists: bool },
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
    /// A set-operation query (UNION / INTERSECT / EXCEPT). A lone SELECT stays
    /// `Select` above; this variant appears only when a set operator is present.
    SetQuery(SetQuery<'a>),
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
    pub body: &'a SetTree<'a>,
    pub order_by: &'a [OrderBy<'a>],
    pub limit: Option<&'a Expr<'a>>,
    pub offset: Option<&'a Expr<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Select<'a> {
    pub items: &'a [SelectItem<'a>],
    pub distinct: bool,
    pub from: Option<FromClause<'a>>,
    pub where_clause: Option<&'a Expr<'a>>,
    pub group_by: &'a [&'a Expr<'a>],
    pub having: Option<&'a Expr<'a>>,
    pub order_by: &'a [OrderBy<'a>],
    pub limit: Option<&'a Expr<'a>>,
    pub offset: Option<&'a Expr<'a>>,
    /// Non-recursive `WITH` common table expressions. Expanded into derived
    /// tables before execution; empty after expansion.
    pub with: &'a [Cte<'a>],
    /// When present, this "select" is actually a set-operation query (used in
    /// subquery position): its rows come from `set_body`, and only `order_by`
    /// / `limit` / `offset` above apply. `items`/`from`/etc. are unused.
    pub set_body: Option<&'a SetTree<'a>>,
}

/// One `WITH name AS (SELECT ...)` common table expression.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cte<'a> {
    pub name: &'a str,
    pub query: &'a Select<'a>,
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
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Join<'a> {
    pub table: TableRef<'a>,
    pub kind: JoinKind,
    /// ON condition; None for CROSS JOIN.
    pub on: Option<&'a Expr<'a>>,
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
    Expr { expression: &'a Expr<'a>, alias: Option<&'a str> },
}

/// A window function's `OVER (PARTITION BY ... ORDER BY ...)` clause. Only the
/// default frame is supported; an explicit ROWS/RANGE frame is rejected.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowSpec<'a> {
    pub partition_by: &'a [&'a Expr<'a>],
    pub order_by: &'a [OrderBy<'a>],
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
    pub name: &'a str,
    pub columns: &'a [ColumnDef<'a>],
    /// Table-level constraints (multi-column PK/UNIQUE, CHECK, FOREIGN KEY),
    /// plus column-level CHECK/REFERENCES desugared into this list.
    pub constraints: &'a [TableConstraint<'a>],
    pub if_not_exists: bool,
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
        parent: &'a str,
        /// Referenced columns; empty means "the parent's primary key".
        parent_cols: &'a [&'a str],
        on_delete: FkAction,
        on_update: FkAction,
    },
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
    pub name: &'a str,
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
    /// DEFAULT expression (constants only are accepted at execution).
    pub default: Option<&'a Expr<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Insert<'a> {
    pub table: &'a str,
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
    pub table: &'a str,
    pub assignments: &'a [(&'a str, &'a Expr<'a>)],
    /// Extra tables joined for the assignment/WHERE (`UPDATE t SET ... FROM e`).
    pub from: Option<&'a FromClause<'a>>,
    pub where_clause: Option<&'a Expr<'a>>,
    pub returning: &'a [SelectItem<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Delete<'a> {
    pub table: &'a str,
    /// Extra tables joined for the WHERE (`DELETE FROM t USING e`).
    pub using: Option<&'a FromClause<'a>>,
    pub where_clause: Option<&'a Expr<'a>>,
    pub returning: &'a [SelectItem<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlterTable<'a> {
    pub table: &'a str,
    pub action: AlterAction<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AlterAction<'a> {
    RenameTable(&'a str),
    RenameColumn { from: &'a str, to: &'a str },
    AddColumn(ColumnDef<'a>),
    DropColumn(&'a str),
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
                if matches!(*name, "count" | "sum" | "avg" | "min" | "max" | "bool_and" | "bool_or" | "every" | "string_agg")
        )
    }

    /// True when the expression is a compile-time constant: only literals
    /// and pure operations over them, with no column/parameter/subquery/
    /// aggregate reference. PostgreSQL evaluates these at plan time, so
    /// their errors (division by zero, overflow) surface eagerly.
    pub fn is_constant(&self) -> bool {
        /// Set-returning functions expand to multiple rows and are never a
        /// foldable constant.
        fn is_set_returning(name: &str) -> bool {
            name.eq_ignore_ascii_case("unnest")
                || name.eq_ignore_ascii_case("generate_series")
                || name.eq_ignore_ascii_case("_pg_expandarray")
        }
        match self {
            Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_)
            | Expr::NumericLit(_) | Expr::Str(_) => true,
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
            Expr::Case { operand, whens, otherwise } => {
                operand.map(|o| o.is_constant()).unwrap_or(true)
                    && whens.iter().all(|(c, r)| c.is_constant() && r.is_constant())
                    && otherwise.map(|e| e.is_constant()).unwrap_or(true)
            }
            // Aggregates, window functions, and set-returning functions are
            // never constant; other calls are constant when their arguments are.
            Expr::Call { name, args, over, .. } => {
                over.is_none()
                    && !self.is_aggregate()
                    && !is_set_returning(name)
                    && args.iter().all(|a| a.is_constant())
            }
            Expr::Array(items) => items.iter().all(|e| e.is_constant()),
            Expr::Subscript { base, index } => base.is_constant() && index.is_constant(),
            Expr::Field { base, .. } => base.is_constant(),
            Expr::AnyAll { operand, array, .. } => operand.is_constant() && array.is_constant(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
    /// `~` bitwise NOT (integers).
    BitNot,
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
            Self::JsonGet | Self::JsonGetText => 9,
        }
    }
}
