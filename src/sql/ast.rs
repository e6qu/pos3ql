//! Arena-allocated AST. Every node is `Copy`; child links are arena
//! references, so an entire statement tree lives exactly as long as the
//! per-statement arena and costs nothing to drop.

use crate::sql::types::ColType;
use crate::util::StackStr;

/// A PostgreSQL two-phase transaction identifier. PostgreSQL requires a
/// quoted value shorter than 200 bytes; constructing the closed value at the
/// parse boundary keeps overlong or truncated identifiers out of execution,
/// catalogs, and durability records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedTransactionId(StackStr<199>);

impl PreparedTransactionId {
    pub const EMPTY: Self = Self(StackStr::new());

    pub fn parse(value: &str) -> Option<Self> {
        let value = StackStr::from_str(value);
        (!value.is_truncated()).then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// A possibly schema-qualified relation name, as written. `schema: None`
/// means the statement spelled a bare name that resolves through the session
/// search path; carrying the pair everywhere makes losing a qualifier
/// impossible by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualName<'a> {
    pub schema: Option<&'a str>,
    pub name: &'a str,
}

/// A syntactically valid collation reference. Catalog lookup happens at the
/// statement's transaction-visible binding boundary, never in the lexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedCollation<'a> {
    Builtin(Collation),
    Named(&'a QualName<'a>),
}

impl ParsedCollation<'_> {
    pub const DEFAULT: Self = Self::Builtin(Collation::Default);

    pub const fn builtin(self) -> Option<Collation> {
        match self {
            Self::Builtin(collation) => Some(collation),
            Self::Named(_) => None,
        }
    }
}

/// The privilege identity used while expanding a view body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewSecurity {
    Definer,
    Invoker,
}

/// PostgreSQL's three view options.  Parsing them as a closed state keeps a
/// later executor from treating a misspelled option as inert catalog text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewOption {
    CheckOption(ViewCheckOption),
    SecurityBarrier(bool),
    SecurityInvoker(bool),
}

/// One option name accepted by `ALTER VIEW ... RESET (...)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewOptionName {
    CheckOption,
    SecurityBarrier,
    SecurityInvoker,
}

/// `check_option` is neither a boolean nor an arbitrary string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewCheckOption {
    Local,
    Cascaded,
}

/// One `ALTER VIEW` action.  Options and exposed-column operations remain
/// separate from stored-query replacement, preserving PostgreSQL's object
/// identity and dependency semantics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AlterViewAction<'a> {
    SetDefault {
        column: &'a str,
        expression: &'a str,
    },
    DropDefault {
        column: &'a str,
    },
    RenameColumn {
        from: &'a str,
        to: &'a str,
    },
    RenameTo(&'a str),
    SetSchema(&'a str),
    SetOptions(&'a [ViewOption]),
    ResetOptions(&'a [ViewOptionName]),
}

/// `ALTER MATERIALIZED VIEW` deliberately exposes only operations whose
/// backing-relation effects have one durable implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterMaterializedViewAction<'a> {
    RenameTo(&'a str),
    SetSchema(&'a str),
    SetTablespace(&'a str),
}

/// One explicitly named relation in a publication.  An empty `columns` slice
/// means the PostgreSQL default: publish every column.  Keeping the selected
/// names beside their relation prevents later execution from losing a column
/// list while resolving a batch of targets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PublicationTarget<'a> {
    pub relation: QualName<'a>,
    pub columns: &'a [&'a str],
    pub filter: Option<&'a Expr<'a>>,
    pub filter_text: Option<&'a str>,
}

/// A resolved built-in collation identity.  The parser resolves the spelling
/// once, so execution cannot accept a `COLLATE` clause and accidentally lose
/// its semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collation {
    None,
    Default,
    C,
    Posix,
    UcsBasic,
    /// A database-local catalog slot. The slot, rather than a spelling, is the
    /// durable identity across rename and schema moves.
    Catalog(u8),
}

impl Collation {
    /// Every built-in catalog collation. `None` is an attribute state, not a
    /// catalog object, so it deliberately cannot appear in this list.
    pub const BUILTIN: [Self; 4] = [Self::Default, Self::C, Self::Posix, Self::UcsBasic];

    pub const fn oid(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Default => 100,
            Self::C => 950,
            Self::Posix => 951,
            Self::UcsBasic => 962,
            Self::Catalog(slot) => 20_000 + slot as i32,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Default => "default",
            Self::C => "C",
            Self::Posix => "POSIX",
            Self::UcsBasic => "ucs_basic",
            Self::Catalog(_) => "<catalog collation>",
        }
    }

    pub const fn provider(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Default => "d",
            Self::C | Self::Posix => "c",
            Self::UcsBasic => "b",
            Self::Catalog(_) => "",
        }
    }

    pub const fn encoding(self) -> i32 {
        match self {
            Self::UcsBasic => 6,
            Self::None | Self::Default | Self::C | Self::Posix => -1,
            Self::Catalog(_) => -1,
        }
    }

    pub const fn libc_locale(self) -> &'static str {
        match self {
            Self::C => "C",
            Self::Posix => "POSIX",
            Self::None | Self::Default | Self::UcsBasic => "",
            Self::Catalog(_) => "",
        }
    }

    pub const fn code(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::C => 1,
            Self::Posix => 2,
            Self::UcsBasic => 3,
            Self::None => 4,
            Self::Catalog(slot) => 5 + slot,
        }
    }

    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Default),
            1 => Some(Self::C),
            2 => Some(Self::Posix),
            3 => Some(Self::UcsBasic),
            4 => Some(Self::None),
            5..=132 => Some(Self::Catalog(code - 5)),
            _ => None,
        }
    }
}

/// A statement PostgreSQL permits inside CREATE SCHEMA. Keeping this distinct
/// from [`Stmt`] makes the parser's grammar guarantee available to execution.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CreateSchemaElement<'a> {
    Table(CreateTable<'a>),
    View {
        name: QualName<'a>,
        or_replace: bool,
        security: ViewSecurity,
        sql: &'a str,
    },
    Index {
        name: Option<&'a str>,
        table: QualName<'a>,
        build: IndexBuildMode,
        scope: IndexTargetScope,
        if_not_exists: bool,
        columns: &'a [IndexColumn<'a>],
        include_columns: &'a [&'a str],
        nulls_not_distinct: bool,
        predicate: Option<&'a Expr<'a>>,
        predicate_text: Option<&'a str>,
        options: IndexStorageOptions,
        tablespace: Option<&'a str>,
        unique: bool,
    },
    Sequence {
        name: QualName<'a>,
        if_not_exists: bool,
        options: SeqOptions<'a>,
    },
    Domain(CreateDomain<'a>),
    Enum {
        name: QualName<'a>,
        labels: &'a [&'a str],
    },
    Composite {
        name: QualName<'a>,
        fields: &'a [CompositeField<'a>],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceTarget<'a> {
    pub table: QualName<'a>,
    pub columns: &'a [&'a str],
}

/// The VACUUM modes this object-native engine can execute. Other PostgreSQL
/// options are rejected at parsing rather than accepted without an effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VacuumOptions {
    pub full: bool,
    pub analyze: bool,
}

impl VacuumOptions {
    pub const DEFAULT: Self = Self {
        full: false,
        analyze: false,
    };
}

/// EXPLAIN's output representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainFormat {
    Text,
    Json,
    Xml,
    Yaml,
}

/// Whether EXPLAIN ANALYZE measures result serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainSerialize {
    None,
    Text,
    Binary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionIsolation {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

impl TransactionIsolation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadUncommitted => "read uncommitted",
            Self::ReadCommitted => "read committed",
            Self::RepeatableRead => "repeatable read",
            Self::Serializable => "serializable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionCharacteristics {
    pub isolation: Option<TransactionIsolation>,
    pub read_only: Option<bool>,
    pub deferrable: Option<bool>,
}

impl TransactionCharacteristics {
    pub const EMPTY: Self = Self {
        isolation: None,
        read_only: None,
        deferrable: None,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionTarget {
    Current,
    SessionDefaults,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingSyntax {
    Generic,
    FromCurrent,
    TimeZone,
    TimeZoneInterval(i32),
}

/// PostgreSQL EXPLAIN options. Keeping the complete option state in the AST
/// prevents accepted syntax from being forgotten between parse and execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExplainOptions {
    pub analyze: bool,
    pub verbose: bool,
    pub costs: bool,
    pub settings: bool,
    pub buffers: bool,
    pub wal: bool,
    pub timing: bool,
    pub summary: bool,
    pub memory: bool,
    pub generic_plan: bool,
    pub serialize: ExplainSerialize,
    pub format: ExplainFormat,
}

impl ExplainOptions {
    pub const DEFAULT: Self = Self {
        analyze: false,
        verbose: false,
        costs: true,
        settings: false,
        buffers: false,
        wal: false,
        timing: true,
        summary: false,
        memory: false,
        generic_plan: false,
        serialize: ExplainSerialize::None,
        format: ExplainFormat::Text,
    };
}

impl<'a> QualName<'a> {
    pub fn bare(name: &'a str) -> Self {
        QualName { schema: None, name }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stmt<'a> {
    Select(Select<'a>),
    /// EXPLAIN [(options)] statement. The referenced statement is arena-owned
    /// so the recursive AST remains finite and allocation-free at execution.
    Explain {
        options: ExplainOptions,
        statement: &'a Stmt<'a>,
    },
    /// A `WITH` clause whose main statement modifies data. Query main
    /// statements carry their CTEs directly on [`Select`] / [`SetQuery`];
    /// keeping DML in this wrapper avoids four duplicate `with` fields and
    /// makes it impossible for execution to accept and then lose the clause.
    With {
        ctes: &'a [Cte<'a>],
        statement: &'a Stmt<'a>,
    },
    CreateTable(CreateTable<'a>),
    Insert(Insert<'a>),
    Update(Update<'a>),
    Delete(Delete<'a>),
    Merge(Merge<'a>),
    Begin(TransactionCharacteristics),
    Commit,
    Rollback,
    PrepareTransaction(PreparedTransactionId),
    CommitPrepared(PreparedTransactionId),
    RollbackPrepared(PreparedTransactionId),
    /// SAVEPOINT name.
    Savepoint(&'a str),
    /// RELEASE [SAVEPOINT] name.
    ReleaseSavepoint(&'a str),
    /// ROLLBACK TO [SAVEPOINT] name.
    RollbackToSavepoint(&'a str),
    /// Change the transaction-local timing of one or more deferrable
    /// constraints. Names remain structured so search-path resolution happens
    /// once against catalog identities at execution.
    SetConstraints {
        targets: ConstraintTargets<'a>,
        mode: ConstraintMode,
    },
    /// LOCK [TABLE] relation [, ...] [IN lockmode MODE] [NOWAIT].
    LockTable {
        tables: &'a [QualName<'a>],
        mode: TableLockMode,
        nowait: bool,
    },
    DropTable(DropTable<'a>),
    /// TRUNCATE [TABLE] name [, ...] [RESTART IDENTITY | CONTINUE IDENTITY]
    /// [CASCADE | RESTRICT].
    Truncate {
        tables: &'a [QualName<'a>],
        restart_identity: bool,
        cascade: bool,
    },
    /// CREATE [OR REPLACE] VIEW name AS <select>. `sql` is the raw SELECT text,
    /// stored and re-expanded as a derived table at query time.
    CreateView {
        name: QualName<'a>,
        or_replace: bool,
        security: ViewSecurity,
        sql: &'a str,
    },
    /// `ALTER VIEW` retains a closed action rather than sharing the broader
    /// table-action grammar, whose accepted states are deliberately different.
    AlterView {
        name: QualName<'a>,
        if_exists: bool,
        action: AlterViewAction<'a>,
    },
    AlterMaterializedView {
        name: QualName<'a>,
        if_exists: bool,
        action: AlterMaterializedViewAction<'a>,
    },
    /// A rewrite rule is parsed into its closed event/mode states and complete
    /// action statements. Source slices are retained for durable catalog text.
    CreateRule(CreateRule<'a>),
    AlterRule {
        name: &'a str,
        table: QualName<'a>,
        new_name: &'a str,
    },
    DropRule(DropRule<'a>),
    /// A SQL routine retains its parsed invocation contract and body spelling.
    /// The executor resolves every type before the definition reaches storage.
    CreateRoutine(CreateRoutine<'a>),
    /// A user-defined aggregate keeps its invocation shape separate from its
    /// support-function contract. Ordered-set direct arguments are therefore
    /// never accidentally fed to the transition function.
    CreateAggregate(CreateAggregate<'a>),
    CreateCast(CreateCast<'a>),
    DropCast(DropCast<'a>),
    CreateOperator(CreateOperator<'a>),
    AlterOperator {
        identity: OperatorIdentity<'a>,
        action: AlterOperatorAction<'a>,
    },
    DropOperator {
        identities: &'a [OperatorIdentity<'a>],
        if_exists: bool,
        cascade: bool,
    },
    CreateOperatorFamily {
        name: QualName<'a>,
        method: IndexAccessMethod,
    },
    AlterOperatorFamily {
        name: QualName<'a>,
        method: IndexAccessMethod,
        action: AlterOperatorFamilyAction<'a>,
    },
    DropOperatorFamily {
        names: &'a [QualName<'a>],
        method: IndexAccessMethod,
        if_exists: bool,
        cascade: bool,
    },
    CreateOperatorClass(CreateOperatorClass<'a>),
    AlterOperatorClass {
        name: QualName<'a>,
        method: IndexAccessMethod,
        action: AlterOperatorClassAction<'a>,
    },
    DropOperatorClass {
        names: &'a [QualName<'a>],
        method: IndexAccessMethod,
        if_exists: bool,
        cascade: bool,
    },
    /// `CALL procedure(args)`: unlike a scalar expression call, this can run a
    /// complete SQL statement body and therefore has its own statement node.
    Call {
        name: QualName<'a>,
        arguments: &'a [&'a Expr<'a>],
        argument_names: &'a [Option<&'a str>],
        variadic: bool,
    },
    /// An anonymous PL/pgSQL block. Unsupported languages are rejected before
    /// this node is constructed.
    Do {
        body: &'a str,
    },
    CreateLanguage(CreateLanguage<'a>),
    AlterLanguage {
        name: &'a str,
        action: AlterLanguageAction<'a>,
    },
    DropLanguage {
        names: &'a [&'a str],
        if_exists: bool,
        cascade: bool,
    },
    DropFunction {
        functions: &'a [RoutineIdentity<'a>],
        if_exists: bool,
        cascade: bool,
    },
    DropProcedure {
        procedures: &'a [RoutineIdentity<'a>],
        if_exists: bool,
        cascade: bool,
    },
    DropRoutine {
        routines: &'a [RoutineIdentity<'a>],
        if_exists: bool,
        cascade: bool,
    },
    DropAggregate {
        aggregates: &'a [AggregateIdentity<'a>],
        if_exists: bool,
        cascade: bool,
    },
    /// The shared routine forms retain the written target kind; that makes a
    /// function/procedure mismatch an explicit error rather than a name-only
    /// lookup.
    AlterRoutine {
        kind: RoutineTargetKind,
        routine: RoutineIdentity<'a>,
        actions: &'a [AlterRoutineAction<'a>],
    },
    AlterAggregate {
        aggregate: AggregateIdentity<'a>,
        action: AlterRoutineAction<'a>,
    },
    CreateExtension {
        name: &'a str,
        if_not_exists: bool,
        schema: Option<&'a str>,
        version: Option<&'a str>,
        cascade: bool,
    },
    AlterExtension {
        name: &'a str,
        action: AlterExtensionAction<'a>,
    },
    AlterMaterializedViewExtensionDependency {
        name: QualName<'a>,
        extension: &'a str,
        enabled: bool,
    },
    DropExtension {
        names: &'a [&'a str],
        if_exists: bool,
        cascade: bool,
    },
    /// DROP VIEW [IF EXISTS] name.
    DropView {
        names: &'a [QualName<'a>],
        if_exists: bool,
        cascade: bool,
    },
    CreateCollation(CreateCollation<'a>),
    AlterCollation {
        name: QualName<'a>,
        action: AlterCollationAction<'a>,
    },
    DropCollation {
        name: QualName<'a>,
        if_exists: bool,
        cascade: bool,
    },
    CreateConversion(CreateConversion<'a>),
    AlterConversion {
        name: QualName<'a>,
        action: AlterConversionAction<'a>,
    },
    DropConversion {
        name: QualName<'a>,
        if_exists: bool,
        cascade: bool,
    },
    CreateForeignDataWrapper(CreateForeignDataWrapper<'a>),
    AlterForeignDataWrapper {
        name: &'a str,
        action: AlterForeignDataWrapperAction<'a>,
    },
    DropForeignDataWrapper {
        names: &'a [&'a str],
        if_exists: bool,
        cascade: bool,
    },
    CreateForeignServer(CreateForeignServer<'a>),
    AlterForeignServer {
        name: &'a str,
        action: AlterForeignServerAction<'a>,
    },
    DropForeignServer {
        names: &'a [&'a str],
        if_exists: bool,
        cascade: bool,
    },
    CreateUserMapping(CreateUserMapping<'a>),
    AlterUserMapping(AlterUserMapping<'a>),
    DropUserMapping(DropUserMapping<'a>),
    CreateForeignTable(CreateForeignTable<'a>),
    AlterForeignTable(AlterTable<'a>),
    DropForeignTable(DropTable<'a>),
    ImportForeignSchema(ImportForeignSchema<'a>),
    CreateTextSearchParser(CreateTextSearchParser<'a>),
    CreateTextSearchTemplate(CreateTextSearchTemplate<'a>),
    CreateTextSearchDictionary(CreateTextSearchDictionary<'a>),
    CreateTextSearchConfiguration(CreateTextSearchConfiguration<'a>),
    AlterTextSearch {
        kind: TextSearchObjectKind,
        name: QualName<'a>,
        action: AlterTextSearchAction<'a>,
    },
    DropTextSearch {
        kind: TextSearchObjectKind,
        name: QualName<'a>,
        if_exists: bool,
        cascade: bool,
    },
    /// CREATE PUBLICATION name FOR { ALL TABLES | TABLE table [(column [, ...])] [, ...] }
    /// [WITH (publish = 'insert, update, delete, truncate')].
    CreatePublication {
        name: &'a str,
        all_tables: bool,
        tables: &'a [PublicationTarget<'a>],
        schemas: &'a [&'a str],
        publish: PublicationOperations,
        publish_via_partition_root: bool,
        publish_generated_columns: PublishGeneratedColumns,
    },
    /// ALTER PUBLICATION name SET (publish = ...) or change its explicit
    /// relation membership.
    AlterPublication {
        name: &'a str,
        action: AlterPublicationAction<'a>,
    },
    /// DROP PUBLICATION [IF EXISTS] name [, ...].
    DropPublication {
        names: &'a [&'a str],
        if_exists: bool,
    },
    /// CREATE SUBSCRIPTION name CONNECTION 'conninfo' PUBLICATION name [, ...].
    /// Option states are carried explicitly so execution cannot conflate a
    /// PostgreSQL default with an option the client supplied.
    CreateSubscription {
        name: &'a str,
        connection: &'a str,
        publications: &'a [&'a str],
        options: SubscriptionOptions<'a>,
    },
    /// ALTER SUBSCRIPTION lifecycle operation.
    AlterSubscription {
        name: &'a str,
        action: AlterSubscriptionAction<'a>,
    },
    /// DROP SUBSCRIPTION [IF EXISTS] name [, ...].
    DropSubscription {
        names: &'a [&'a str],
        if_exists: bool,
    },
    /// A row trigger definition.  Its timing, event set, target relation and
    /// function identity are separate typed fields so execution cannot
    /// reinterpret a trigger as a different DML event.
    CreateTrigger(CreateTrigger<'a>),
    CreateEventTrigger(CreateEventTrigger<'a>),
    AlterEventTrigger {
        name: &'a str,
        action: AlterEventTriggerAction<'a>,
    },
    DropEventTrigger {
        name: &'a str,
        if_exists: bool,
        cascade: bool,
    },
    /// ALTER TRIGGER has only identity/enabled-state operations; the typed
    /// action keeps those distinct from a definition replacement.
    AlterTrigger {
        trigger: TriggerIdentity<'a>,
        action: AlterTriggerAction<'a>,
    },
    /// DROP TRIGGER [IF EXISTS] name ON table.
    DropTrigger {
        trigger: TriggerIdentity<'a>,
        if_exists: bool,
        cascade: bool,
    },
    /// A row-security policy whose command-specific expression shape was
    /// validated while parsing. In particular, INSERT cannot carry USING and
    /// SELECT/DELETE cannot carry WITH CHECK.
    CreatePolicy(CreatePolicy<'a>),
    AlterPolicy(AlterPolicy<'a>),
    DropPolicy {
        policy: PolicyIdentity<'a>,
        if_exists: bool,
        cascade: bool,
    },
    /// A named extended-statistics definition. The key shape distinguishes
    /// PostgreSQL's one-expression form from multivariate statistics before
    /// execution can observe an invalid kind/key combination.
    CreateStatistics(CreateStatistics<'a>),
    AlterStatistics {
        name: QualName<'a>,
        action: AlterStatisticsAction<'a>,
    },
    DropStatistics {
        names: &'a [QualName<'a>],
        if_exists: bool,
        cascade: bool,
    },
    /// Table-producing DDL retains its written command kind because PostgreSQL
    /// exposes distinct event-trigger tags for SELECT INTO and CREATE TABLE AS.
    CreateTableAs {
        name: QualName<'a>,
        columns: &'a [&'a str],
        sql: &'a str,
        with_data: bool,
        if_not_exists: bool,
        kind: CreateTableAsKind,
    },
    /// REFRESH MATERIALIZED VIEW name — re-run the stored query, replacing rows.
    RefreshMaterializedView {
        name: QualName<'a>,
    },
    /// DROP MATERIALIZED VIEW [IF EXISTS] name.
    DropMaterializedView {
        names: &'a [QualName<'a>],
        if_exists: bool,
        cascade: bool,
    },
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
        set_schema: Option<&'a str>,
    },
    /// DROP SEQUENCE [IF EXISTS] name [, ...].
    DropSequence {
        names: &'a [QualName<'a>],
        if_exists: bool,
        cascade: bool,
    },
    /// CREATE DOMAIN name [AS] basetype [ constraint ... ].
    CreateDomain(CreateDomain<'a>),
    /// ALTER DOMAIN name <action>.
    AlterDomain {
        name: QualName<'a>,
        action: AlterDomainAction<'a>,
    },
    /// DROP DOMAIN [IF EXISTS] name [, ...] [CASCADE|RESTRICT].
    DropDomain {
        names: &'a [QualName<'a>],
        if_exists: bool,
        cascade: bool,
    },
    /// CREATE TYPE name AS ENUM ('label', ...).
    CreateEnum {
        name: QualName<'a>,
        labels: &'a [&'a str],
    },
    /// CREATE TYPE name AS (field type [, ...]).
    CreateComposite {
        name: QualName<'a>,
        fields: &'a [CompositeField<'a>],
    },
    /// ALTER TYPE name <action> (enum ADD VALUE / RENAME).
    AlterType {
        name: QualName<'a>,
        action: AlterTypeAction<'a>,
    },
    /// DROP TYPE [IF EXISTS] name [, ...] [CASCADE|RESTRICT].
    DropType {
        names: &'a [QualName<'a>],
        if_exists: bool,
        cascade: bool,
    },
    /// CREATE INDEX with every syntax default resolved at the parse boundary.
    CreateIndex {
        name: Option<&'a str>,
        table: QualName<'a>,
        build: IndexBuildMode,
        scope: IndexTargetScope,
        if_not_exists: bool,
        columns: &'a [IndexColumn<'a>],
        /// Non-key covering columns. A distinct AST field makes it impossible
        /// for execution to accidentally use them for ordering or uniqueness.
        include_columns: &'a [&'a str],
        /// Whether NULL key values participate in uniqueness equality.
        nulls_not_distinct: bool,
        /// The parsed `WHERE` membership predicate. Keeping this separate
        /// from the durable spelling makes an absent predicate impossible to
        /// confuse with an always-true one.
        predicate: Option<&'a Expr<'a>>,
        /// Exact source retained for WAL and checkpoints; it is parsed again
        /// only at the catalog boundary that evaluates a row.
        predicate_text: Option<&'a str>,
        options: IndexStorageOptions,
        tablespace: Option<&'a str>,
        unique: bool,
    },
    /// ALTER INDEX [IF EXISTS] name RENAME TO new_name.
    AlterIndex {
        name: QualName<'a>,
        if_exists: bool,
        action: AlterIndexAction<'a>,
    },
    /// ALTER INDEX ALL IN TABLESPACE ... SET TABLESPACE ... .
    AlterIndexesTablespace {
        source: &'a str,
        owners: &'a [&'a str],
        target: &'a str,
        nowait: bool,
    },
    /// DROP INDEX [CONCURRENTLY] [IF EXISTS] name [, ...] [CASCADE|RESTRICT].
    DropIndex {
        names: &'a [QualName<'a>],
        if_exists: bool,
        build: IndexBuildMode,
        cascade: bool,
    },
    /// REINDEX. Target class and options are complete typed parse states.
    Reindex {
        target: ReindexTarget,
        name: Option<QualName<'a>>,
        options: ReindexOptions<'a>,
    },
    /// CLUSTER either reuses every relation's recorded clustering index or
    /// selects one index for a single table. `All` cannot accidentally carry
    /// a stray relation or index name.
    Cluster {
        target: ClusterTarget<'a>,
        verbose: bool,
    },
    /// SET [LOCAL] name {=|TO} value. `value` is the raw source text of the
    /// value (quotes included); the session GUC store validates and applies it.
    Set {
        name: &'a str,
        value: &'a str,
        local: bool,
        syntax: SettingSyntax,
    },
    SetCatalog(&'a str),
    /// RESET name / RESET ALL restores one or every settable GUC to default.
    Reset(Option<&'a str>),
    AlterSystem {
        name: Option<&'a str>,
        value: Option<&'a str>,
    },
    SetTransaction {
        target: TransactionTarget,
        characteristics: TransactionCharacteristics,
    },
    /// SET TRANSACTION SNAPSHOT 'snapshot_id'.
    SetTransactionSnapshot(&'a str),
    /// SET ROLE role | NONE and RESET ROLE.
    SetRole {
        role: Option<&'a str>,
        local: bool,
        reset: bool,
    },
    /// SET [LOCAL] SESSION AUTHORIZATION role | DEFAULT and
    /// RESET SESSION AUTHORIZATION. Unlike SET ROLE, this changes both
    /// session_user and current_user while the authenticated identity remains
    /// the authority used to permit the change.
    SetSessionAuthorization {
        role: Option<&'a str>,
        local: bool,
        reset: bool,
    },
    Show(&'a str),
    /// SHOW ALL: every readable setting as (name, setting, description).
    ShowAll,
    Discard(DiscardTarget),
    /// Snapshot to object storage now.
    Checkpoint,
    AlterTable(AlterTable<'a>),
    /// SQL-level PREPARE name [(types)] AS <statement>; `sql` is the raw
    /// statement text and `param_types` the declared `$n` type names (empty if
    /// none were declared).
    Prepare {
        name: &'a str,
        sql: &'a str,
        param_types: &'a [&'a str],
    },
    /// SQL-level EXECUTE name(args).
    ExecutePrepared {
        name: &'a str,
        args: &'a [&'a Expr<'a>],
    },
    /// DEALLOCATE name | ALL (None = ALL).
    Deallocate(Option<&'a str>),
    /// COPY table [(columns)] FROM STDIN / TO STDOUT — the bulk-data
    /// subprotocol, text format.
    Copy(CopyStmt<'a>),
    /// A set-operation query (UNION / INTERSECT / EXCEPT). A lone SELECT stays
    /// `Select` above; this variant appears only when a set operator is present.
    SetQuery(SetQuery<'a>),
    /// CREATE SCHEMA [IF NOT EXISTS] name [AUTHORIZATION role] [element ...].
    /// Elements are the grammar-permitted CREATE forms, executed with the new
    /// schema as their creation target.
    CreateSchema {
        name: &'a str,
        authorization: Option<&'a str>,
        if_not_exists: bool,
        elements: &'a [&'a CreateSchemaElement<'a>],
    },
    /// DROP SCHEMA [IF EXISTS] name [, ...] [CASCADE | RESTRICT].
    DropSchema {
        names: &'a [&'a str],
        if_exists: bool,
        cascade: bool,
    },
    CreateDatabase {
        name: &'a str,
        options: CreateDatabaseOptions<'a>,
    },
    AlterDatabase {
        name: &'a str,
        action: AlterDatabaseAction<'a>,
    },
    DropDatabase {
        name: &'a str,
        if_exists: bool,
        force: bool,
    },
    CreateTablespace {
        name: &'a str,
        owner: Option<&'a str>,
        location: &'a str,
        options: TablespaceOptions,
    },
    AlterTablespace {
        name: &'a str,
        action: AlterTablespaceAction<'a>,
    },
    DropTablespace {
        name: &'a str,
        if_exists: bool,
    },
    /// DECLARE name [BINARY] [SCROLL|NO SCROLL] CURSOR [WITH|WITHOUT HOLD] FOR select.
    /// `sql` is the raw SELECT text, materialized at DECLARE.
    DeclareCursor {
        name: &'a str,
        binary: bool,
        scroll: crate::sql::cursor::CursorScroll,
        hold: bool,
        sql: &'a str,
    },
    /// FETCH/MOVE direction [FROM|IN] cursor. MOVE positions without rows.
    FetchCursor {
        name: &'a str,
        motion: crate::sql::cursor::FetchMotion,
        move_only: bool,
    },
    /// CLOSE cursor | CLOSE ALL (None).
    CloseCursor(Option<&'a str>),
    /// VACUUM [options] [table [(columns)] [, ...]] — drives this engine's
    /// checkpoint/compaction and optionally refreshes exact live statistics.
    Vacuum {
        targets: &'a [MaintenanceTarget<'a>],
        options: VacuumOptions,
    },
    /// ANALYZE [options] [table [(columns)] [, ...]] — validates the requested
    /// relations and walks the exact live row state used by the planner.
    Analyze(&'a [MaintenanceTarget<'a>]),
    /// LISTEN channel — register interest; delivered notifications arrive as
    /// asynchronous NotificationResponse messages.
    Listen(&'a str),
    /// UNLISTEN channel, or UNLISTEN * to drop every registration.
    Unlisten(Option<&'a str>),
    /// NOTIFY channel [, payload] — raise a notification (delivered at commit).
    Notify {
        channel: &'a str,
        payload: Option<&'a str>,
    },
    /// COMMENT ON <object> IS { 'text' | NULL }. `text: None` removes it.
    Comment {
        target: CommentTarget<'a>,
        text: Option<&'a str>,
    },
    /// ALTER <supported object> name OWNER TO role. Every catalog object is
    /// owned by the one modeled role, but the target and requested role are
    /// still validated exactly.
    AlterOwner {
        kind: AlterOwnerKind,
        name: QualName<'a>,
        role: &'a str,
        if_exists: bool,
    },
    AlterLargeObjectOwner {
        oid: LargeObjectId,
        role: &'a str,
    },
    /// CREATE ROLE / USER / GROUP. USER differs only in its default LOGIN
    /// attribute; GROUP is PostgreSQL's compatibility spelling for ROLE.
    CreateRole {
        name: &'a str,
        options: RoleOptions<'a>,
        memberships: RoleMembershipClauses<'a>,
    },
    /// ALTER ROLE / USER / GROUP name [WITH] role-option ...
    AlterRole {
        name: &'a str,
        options: RoleOptions<'a>,
    },
    AlterRoleRename {
        name: &'a str,
        new_name: &'a str,
    },
    AlterRoleSetting {
        role: Option<&'a str>,
        database: Option<&'a str>,
        action: RoleSettingAction<'a>,
    },
    /// DROP ROLE / USER / GROUP [IF EXISTS] name [, ...].
    DropRole {
        names: &'a [&'a str],
        if_exists: bool,
    },
    GrantRole {
        roles: &'a [&'a str],
        members: &'a [&'a str],
        options: RoleMembershipPatch,
        grantor: Option<&'a str>,
    },
    RevokeRole {
        roles: &'a [&'a str],
        members: &'a [&'a str],
        option: Option<RoleMembershipOption>,
        grantor: Option<&'a str>,
        cascade: bool,
    },
    GrantPrivileges {
        privileges: &'a [PrivilegeSpec<'a>],
        target: PrivilegeTarget<'a>,
        grantees: &'a [&'a str],
        grant_option: bool,
        grantor: Option<&'a str>,
    },
    RevokePrivileges {
        grant_option_only: bool,
        privileges: &'a [PrivilegeSpec<'a>],
        target: PrivilegeTarget<'a>,
        grantees: &'a [&'a str],
        grantor: Option<&'a str>,
        cascade: bool,
    },
    AlterDefaultPrivileges {
        roles: &'a [&'a str],
        schemas: &'a [&'a str],
        action: DefaultPrivilegeAction<'a>,
    },
    ReassignOwned {
        roles: &'a [&'a str],
        new_owner: &'a str,
    },
    DropOwned {
        roles: &'a [&'a str],
        cascade: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleEvent {
    Select,
    Insert,
    Update,
    Delete,
}

impl RuleEvent {
    pub const fn catalog_code(self) -> u8 {
        match self {
            Self::Select => b'1',
            Self::Update => b'2',
            Self::Insert => b'3',
            Self::Delete => b'4',
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleMode {
    Also,
    Instead,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RuleAction<'a> {
    pub statement: &'a Stmt<'a>,
    pub sql: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreateRule<'a> {
    pub name: &'a str,
    pub or_replace: bool,
    pub event: RuleEvent,
    pub table: QualName<'a>,
    pub condition: Option<&'a Expr<'a>>,
    pub condition_sql: Option<&'a str>,
    pub mode: RuleMode,
    /// Empty is the typed representation of `DO ... NOTHING`.
    pub actions: &'a [RuleAction<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropRule<'a> {
    pub name: &'a str,
    pub table: QualName<'a>,
    pub if_exists: bool,
    pub cascade: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateTableAsKind {
    Table,
    MaterializedView,
    SelectInto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateCollationDefinition<'a> {
    From(QualName<'a>),
    Options {
        locale: Option<&'a str>,
        lc_collate: Option<&'a str>,
        lc_ctype: Option<&'a str>,
        provider: Option<ParsedCollationProvider>,
        deterministic: Option<bool>,
        rules: Option<&'a str>,
        version: Option<&'a str>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedCollationProvider {
    Builtin,
    Libc,
    Icu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateCollation<'a> {
    pub name: QualName<'a>,
    pub if_not_exists: bool,
    pub definition: CreateCollationDefinition<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterCollationAction<'a> {
    RefreshVersion,
    Rename(&'a str),
    Owner(&'a str),
    SetSchema(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateConversion<'a> {
    pub default: bool,
    pub name: QualName<'a>,
    pub source_encoding: crate::storage::PgEncoding,
    pub destination_encoding: crate::storage::PgEncoding,
    pub function: QualName<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterConversionAction<'a> {
    Rename(&'a str),
    Owner(&'a str),
    SetSchema(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForeignOption<'a> {
    pub name: &'a str,
    pub value: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignOptionAction<'a> {
    Add(ForeignOption<'a>),
    Set(ForeignOption<'a>),
    Drop(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignDataHandler<'a> {
    None,
    Function(QualName<'a>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignDataValidator<'a> {
    None,
    Function(QualName<'a>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateForeignDataWrapper<'a> {
    pub name: &'a str,
    pub handler: ForeignDataHandler<'a>,
    pub validator: ForeignDataValidator<'a>,
    pub options: &'a [ForeignOption<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterForeignDataWrapperAction<'a> {
    Definition {
        handler: Option<ForeignDataHandler<'a>>,
        validator: Option<ForeignDataValidator<'a>>,
        options: &'a [ForeignOptionAction<'a>],
    },
    Owner(&'a str),
    Rename(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateForeignServer<'a> {
    pub name: &'a str,
    pub if_not_exists: bool,
    pub server_type: Option<&'a str>,
    pub version: Option<&'a str>,
    pub wrapper: &'a str,
    pub options: &'a [ForeignOption<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterForeignServerAction<'a> {
    Definition {
        /// Outer `None` means VERSION was not mentioned; inner `None` is
        /// PostgreSQL's `VERSION NULL` and removes the version.
        version: Option<Option<&'a str>>,
        options: &'a [ForeignOptionAction<'a>],
    },
    Owner(&'a str),
    Rename(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignUser<'a> {
    Named(&'a str),
    CurrentRole,
    CurrentUser,
    User,
    Public,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateUserMapping<'a> {
    pub user: ForeignUser<'a>,
    pub server: &'a str,
    pub if_not_exists: bool,
    pub options: &'a [ForeignOption<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlterUserMapping<'a> {
    pub user: ForeignUser<'a>,
    pub server: &'a str,
    pub options: &'a [ForeignOptionAction<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropUserMapping<'a> {
    pub user: ForeignUser<'a>,
    pub server: &'a str,
    pub if_exists: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreateForeignTable<'a> {
    pub relation: CreateTable<'a>,
    pub server: &'a str,
    pub options: &'a [ForeignOption<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignSchemaSelection<'a> {
    All,
    LimitTo(&'a [&'a str]),
    Except(&'a [&'a str]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportForeignSchema<'a> {
    pub remote_schema: &'a str,
    pub selection: ForeignSchemaSelection<'a>,
    pub server: &'a str,
    pub local_schema: &'a str,
    pub options: &'a [ForeignOption<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextSearchObjectKind {
    Parser,
    Template,
    Dictionary,
    Configuration,
}

impl TextSearchObjectKind {
    pub const fn noun(self) -> &'static str {
        match self {
            Self::Parser => "text search parser",
            Self::Template => "text search template",
            Self::Dictionary => "text search dictionary",
            Self::Configuration => "text search configuration",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateTextSearchParser<'a> {
    pub name: QualName<'a>,
    pub start: QualName<'a>,
    pub gettoken: QualName<'a>,
    pub end: QualName<'a>,
    pub headline: Option<QualName<'a>>,
    pub lextypes: QualName<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateTextSearchTemplate<'a> {
    pub name: QualName<'a>,
    pub init: Option<QualName<'a>>,
    pub lexize: QualName<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSearchOption<'a> {
    pub name: &'a str,
    pub value: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateTextSearchDictionary<'a> {
    pub name: QualName<'a>,
    pub template: QualName<'a>,
    pub options: &'a [TextSearchOption<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextSearchConfigurationSource<'a> {
    Parser(QualName<'a>),
    Copy(QualName<'a>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateTextSearchConfiguration<'a> {
    pub name: QualName<'a>,
    pub source: TextSearchConfigurationSource<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextSearchMappingAction<'a> {
    Set {
        replace_existing: bool,
        token_types: &'a [&'a str],
        dictionaries: &'a [QualName<'a>],
    },
    Replace {
        token_types: Option<&'a [&'a str]>,
        old: QualName<'a>,
        new: QualName<'a>,
    },
    Drop {
        if_exists: bool,
        token_types: &'a [&'a str],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterTextSearchAction<'a> {
    Rename(&'a str),
    Owner(&'a str),
    SetSchema(&'a str),
    DictionaryOptions(&'a [TextSearchOption<'a>]),
    ConfigurationMapping(TextSearchMappingAction<'a>),
}

/// A parsed named-composite attribute. Keeping the field name and type spelling
/// together prevents the executor from accepting a name-only half-definition.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompositeField<'a> {
    pub name: &'a str,
    pub type_name: &'a str,
    pub type_mod: i32,
    pub collation: ParsedCollation<'a>,
}

/// A parsed ALTER INDEX operation. Keeping the supported operation typed
/// prevents execution from accepting an index alteration it cannot enact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterIndexAction<'a> {
    Rename(&'a str),
    SetTablespace(&'a str),
    SetOptions(IndexStorageOptions),
    ResetOptions(IndexStorageOptionNames),
    SetStatistics { column: u16, target: i16 },
    AttachPartition(QualName<'a>),
    ExtensionDependency { extension: &'a str, enabled: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TablespaceCost(u64);

impl TablespaceCost {
    pub fn new(value: f64) -> Option<Self> {
        (value.is_finite() && value >= 0.0).then_some(Self(value.to_bits()))
    }

    pub fn from_bits(bits: u64) -> Option<Self> {
        Self::new(f64::from_bits(bits))
    }

    pub fn bits(self) -> u64 {
        self.0
    }

    pub fn value(self) -> f64 {
        f64::from_bits(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TablespaceOptions {
    pub random_page_cost: Option<TablespaceCost>,
    pub seq_page_cost: Option<TablespaceCost>,
    pub effective_io_concurrency: Option<i32>,
    pub maintenance_io_concurrency: Option<i32>,
}

impl TablespaceOptions {
    pub const DEFAULT: Self = Self {
        random_page_cost: None,
        seq_page_cost: None,
        effective_io_concurrency: None,
        maintenance_io_concurrency: None,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TablespaceOptionNames {
    pub random_page_cost: bool,
    pub seq_page_cost: bool,
    pub effective_io_concurrency: bool,
    pub maintenance_io_concurrency: bool,
}

impl TablespaceOptionNames {
    pub const EMPTY: Self = Self {
        random_page_cost: false,
        seq_page_cost: false,
        effective_io_concurrency: false,
        maintenance_io_concurrency: false,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterTablespaceAction<'a> {
    Rename(&'a str),
    SetOwner(&'a str),
    SetOptions(TablespaceOptions),
    ResetOptions(TablespaceOptionNames),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseStrategy {
    WalLog,
    FileCopy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseLocaleProvider {
    Builtin,
    Libc,
    Icu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateDatabaseOptions<'a> {
    pub owner: Option<&'a str>,
    pub template: Option<&'a str>,
    pub encoding: Option<&'a str>,
    pub strategy: Option<DatabaseStrategy>,
    pub locale_provider: Option<DatabaseLocaleProvider>,
    pub collate: Option<&'a str>,
    pub ctype: Option<&'a str>,
    pub locale: Option<&'a str>,
    pub collation_version: Option<&'a str>,
    pub tablespace: Option<&'a str>,
    pub allow_connections: Option<bool>,
    pub connection_limit: Option<i32>,
    pub is_template: Option<bool>,
    pub oid: Option<i32>,
}

impl CreateDatabaseOptions<'_> {
    pub const EMPTY: Self = Self {
        owner: None,
        template: None,
        encoding: None,
        strategy: None,
        locale_provider: None,
        collate: None,
        ctype: None,
        locale: None,
        collation_version: None,
        tablespace: None,
        allow_connections: None,
        connection_limit: None,
        is_template: None,
        oid: None,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterDatabaseAction<'a> {
    Options {
        allow_connections: Option<bool>,
        connection_limit: Option<i32>,
        is_template: Option<bool>,
    },
    Rename(&'a str),
    SetOwner(&'a str),
    SetTablespace(&'a str),
    RefreshCollationVersion,
    Set {
        name: &'a str,
        value: RoutineConfigValue<'a>,
    },
    Reset(Option<&'a str>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardTarget {
    All,
    Plans,
    Sequences,
    Temporary,
}

/// PostgreSQL's blocking and concurrent index lifecycle paths have different
/// transaction and lock contracts. They cannot be inferred from an optional
/// keyword after parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexBuildMode {
    Blocking,
    Concurrent,
}

/// Whether CREATE INDEX recursively creates indexes for partitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexTargetScope {
    Recurse,
    Only,
}

/// B-tree relation options supported by every accepted index definition.
/// `None` means PostgreSQL's default, not an executor-selected fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStorageOptions {
    pub fillfactor: Option<u8>,
    pub deduplicate_items: Option<bool>,
}

impl IndexStorageOptions {
    pub const DEFAULT: Self = Self {
        fillfactor: None,
        deduplicate_items: None,
    };
}

/// ALTER INDEX RESET names. A bit is present only when the option was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStorageOptionNames {
    pub fillfactor: bool,
    pub deduplicate_items: bool,
}

impl IndexStorageOptionNames {
    pub const EMPTY: Self = Self {
        fillfactor: false,
        deduplicate_items: false,
    };
}

/// Row-change operations a publication emits through logical replication.
/// PostgreSQL enables all four when the WITH clause is omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicationOperations {
    pub insert: bool,
    pub update: bool,
    pub delete: bool,
    pub truncate: bool,
}

/// The creation options that determine whether a subscriber must make a
/// remote connection or create a publisher slot. The defaults are PostgreSQL
/// syntax defaults, captured before execution rather than inferred later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionOptions<'a> {
    pub connect: SubscriptionConnect,
    pub enabled: bool,
    pub copy_data: bool,
    pub slot: SubscriptionSlotPlan<'a>,
    pub behavior: SubscriptionBehavior,
}

/// PostgreSQL-visible behavior retained by a subscription after creation.
/// Defaults are resolved by the parser, so the catalog and worker receive one
/// complete state rather than a bag of optional strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionBehavior {
    pub binary: bool,
    pub streaming: SubscriptionStreaming,
    pub synchronous_commit: SubscriptionSynchronousCommit,
    pub two_phase: bool,
    pub disable_on_error: bool,
    pub password_required: bool,
    pub run_as_owner: bool,
    pub origin: SubscriptionOrigin,
    pub failover: bool,
}

impl SubscriptionBehavior {
    pub const POSTGRESQL_18_DEFAULT: Self = Self {
        binary: false,
        streaming: SubscriptionStreaming::Parallel,
        synchronous_commit: SubscriptionSynchronousCommit::Off,
        two_phase: false,
        disable_on_error: false,
        password_required: true,
        run_as_owner: false,
        origin: SubscriptionOrigin::Any,
        failover: false,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionStreaming {
    Off,
    On,
    Parallel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionSynchronousCommit {
    Off,
    Local,
    RemoteWrite,
    On,
    RemoteApply,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionOrigin {
    None,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionSlotSetting<'a> {
    Named(&'a str),
    Absent,
}

/// Transactional `ALTER SUBSCRIPTION SET` patch. Each field is typed before
/// execution; `None` means the SQL did not name that setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionSettingsPatch<'a> {
    pub slot: Option<SubscriptionSlotSetting<'a>>,
    pub binary: Option<bool>,
    pub streaming: Option<SubscriptionStreaming>,
    pub synchronous_commit: Option<SubscriptionSynchronousCommit>,
    pub two_phase: Option<bool>,
    pub disable_on_error: Option<bool>,
    pub password_required: Option<bool>,
    pub run_as_owner: Option<bool>,
    pub origin: Option<SubscriptionOrigin>,
    pub failover: Option<bool>,
}

/// Whether CREATE SUBSCRIPTION may contact the publisher. `Deferred` is the
/// complete `connect = false` state; it cannot carry an enabled or copy plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionConnect {
    Now,
    Deferred,
}

/// A slot name has already been resolved as PostgreSQL's default or an
/// explicit identifier, but does not yet borrow the subscription name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionSlotName<'a> {
    Default,
    Named(&'a str),
}

/// Remote-slot ownership is decided at the parse boundary. This prevents a
/// later worker from treating an external slot as one it may drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionSlotPlan<'a> {
    Managed(SubscriptionSlotName<'a>),
    External(SubscriptionSlotName<'a>),
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterSubscriptionAction<'a> {
    Enable,
    Disable,
    SetConnection(&'a str),
    SetPublications {
        publications: &'a [&'a str],
        refresh: SubscriptionPublicationRefresh,
    },
    AddPublications {
        publications: &'a [&'a str],
        refresh: SubscriptionPublicationRefresh,
    },
    DropPublications {
        publications: &'a [&'a str],
        refresh: SubscriptionPublicationRefresh,
    },
    RefreshPublications {
        copy_data: bool,
    },
    SetOptions(SubscriptionSettingsPatch<'a>),
    Skip {
        lsn: Option<u64>,
    },
    SetOwner(&'a str),
    Rename(&'a str),
}

/// The instant at which a trigger observes a DML change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerTiming {
    Before,
    After,
    InsteadOf,
}

impl TriggerTiming {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Before => 0,
            Self::After => 1,
            Self::InsteadOf => 2,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Before),
            1 => Some(Self::After),
            2 => Some(Self::InsteadOf),
            _ => None,
        }
    }
}

/// PostgreSQL's trigger granularity. The parser records the SQL default
/// explicitly, so execution never infers row behavior from omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerLevel {
    Row,
    Statement,
}

impl TriggerLevel {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Row => 0,
            Self::Statement => 1,
        }
    }

    pub(crate) const fn from_code(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Row),
            1 => Some(Self::Statement),
            _ => None,
        }
    }
}

/// A non-empty, known trigger event set. Only the durable decoder constructs
/// this from bytes; catalog state cannot carry an unknown event bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerEvents(u8);

impl TriggerEvents {
    pub(crate) const INSERT: u8 = 1;
    pub(crate) const UPDATE: u8 = 2;
    pub(crate) const DELETE: u8 = 4;
    pub(crate) const TRUNCATE: u8 = 8;
    const ALL: u8 = Self::INSERT | Self::UPDATE | Self::DELETE | Self::TRUNCATE;

    pub(crate) const fn from_bits(bits: u8) -> Option<Self> {
        if bits != 0 && bits & !Self::ALL == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    pub(crate) const fn bits(self) -> u8 {
        self.0
    }

    pub(crate) const fn contains(self, event: u8) -> bool {
        self.0 & event != 0
    }

    pub(crate) const fn has_truncate(self) -> bool {
        self.contains(Self::TRUNCATE)
    }
}

/// One DML operation a trigger can observe.  The parser collects one or more
/// of these into a non-empty fixed list, rather than exposing a boolean soup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerEvent {
    Insert,
    Update,
    Delete,
    Truncate,
}

/// The table-qualified identity PostgreSQL uses for ALTER/DROP TRIGGER.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerIdentity<'a> {
    pub name: &'a str,
    pub table: QualName<'a>,
}

/// The named relations exposed by an AFTER statement trigger.  This is an
/// algebraic state rather than two independently optional names, so a durable
/// trigger cannot carry duplicate aliases or an empty transition declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerTransitionTables<'a> {
    None,
    Old(&'a str),
    New(&'a str),
    OldNew { old: &'a str, new: &'a str },
}

impl<'a> TriggerTransitionTables<'a> {
    pub(crate) const fn old(self) -> Option<&'a str> {
        match self {
            Self::Old(old) | Self::OldNew { old, .. } => Some(old),
            Self::None | Self::New(_) => None,
        }
    }

    pub(crate) const fn new_table(self) -> Option<&'a str> {
        match self {
            Self::New(new) | Self::OldNew { new, .. } => Some(new),
            Self::None | Self::Old(_) => None,
        }
    }
}

/// A parsed trigger definition whose ordinary and constraint forms retain
/// their distinct legal states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateTrigger<'a> {
    pub or_replace: bool,
    pub name: &'a str,
    pub kind: TriggerKind<'a>,
    pub timing: TriggerTiming,
    pub level: TriggerLevel,
    pub events: &'a [TriggerEvent],
    /// Empty means every UPDATE; otherwise this trigger fires only for an
    /// UPDATE whose SET list names at least one listed column.
    pub update_columns: &'a [&'a str],
    pub table: QualName<'a>,
    pub transition_tables: TriggerTransitionTables<'a>,
    /// Parser-validated source retained for the bounded durable catalog form.
    pub when: Option<&'a str>,
    pub function: QualName<'a>,
    pub arguments: &'a [&'a str],
}

/// Ordinary and constraint triggers have different legal timing and
/// transaction behavior. Keeping the forms distinct prevents durable ordinary
/// triggers from accidentally acquiring deferrability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind<'a> {
    Ordinary,
    Constraint {
        referenced_table: Option<QualName<'a>>,
        timing: ConstraintTiming,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterTriggerAction<'a> {
    Rename(&'a str),
    DependsOnExtension { extension: &'a str, enabled: bool },
}

/// PostgreSQL's SET PUBLICATION refresh choice. A fresh copy is distinct from
/// a stream-definition change and is never inferred from an omitted option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionPublicationRefresh {
    Refresh { copy_data: bool },
    NoRefresh,
}

/// The validated state change requested by `ALTER PUBLICATION`.  Membership
/// actions carry relations, never a textual clause that execution could
/// partially reinterpret.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AlterPublicationAction<'a> {
    SetOptions {
        publish: Option<PublicationOperations>,
        publish_via_partition_root: Option<bool>,
        publish_generated_columns: Option<PublishGeneratedColumns>,
    },
    SetOwner(&'a str),
    Rename(&'a str),
    SetTargets {
        tables: &'a [PublicationTarget<'a>],
        schemas: &'a [&'a str],
    },
    AddTargets {
        tables: &'a [PublicationTarget<'a>],
        schemas: &'a [&'a str],
    },
    DropTargets {
        tables: &'a [PublicationTarget<'a>],
        schemas: &'a [&'a str],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishGeneratedColumns {
    None,
    Stored,
}

impl PublicationOperations {
    pub const ALL: Self = Self {
        insert: true,
        update: true,
        delete: true,
        truncate: true,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    All,
    Select,
    Insert,
    Update,
    Delete,
    Truncate,
    References,
    Trigger,
    Usage,
    Create,
    Execute,
    Maintain,
    Connect,
    Temporary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrivilegeSpec<'a> {
    pub privilege: Privilege,
    /// A non-empty list is a column privilege and can only target a relation.
    pub columns: &'a [&'a str],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultPrivilegeObjectKind {
    Tables,
    Sequences,
    Functions,
    Types,
    Schemas,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultPrivilegeAction<'a> {
    Grant {
        privileges: &'a [Privilege],
        kind: DefaultPrivilegeObjectKind,
        grantees: &'a [&'a str],
        grant_option: bool,
    },
    Revoke {
        grant_option_only: bool,
        privileges: &'a [Privilege],
        kind: DefaultPrivilegeObjectKind,
        grantees: &'a [&'a str],
        cascade: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegeObjectKind {
    Table,
    Sequence,
    Schema,
    Tablespace,
    Database,
    Type,
    ForeignDataWrapper,
    ForeignServer,
    AllTablesInSchema,
    AllSequencesInSchema,
    AllFunctionsInSchema,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegeTarget<'a> {
    Objects {
        kind: PrivilegeObjectKind,
        names: &'a [QualName<'a>],
    },
    /// Routine targets include argument types and the syntactic kind. A name
    /// alone cannot identify an overload or distinguish a procedure.
    Routines {
        kind: RoutineTargetKind,
        identities: &'a [RoutineIdentity<'a>],
    },
    LargeObjects(&'a [LargeObjectId]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LargeObjectId(u32);

impl LargeObjectId {
    pub const fn parse(value: u32) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineTargetKind {
    Function,
    Procedure,
    Aggregate,
    Either,
}

impl RoutineTargetKind {
    pub const fn noun(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Procedure => "procedure",
            Self::Aggregate => "aggregate",
            Self::Either => "routine",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleMembershipOption {
    Admin,
    Inherit,
    Set,
}

/// Membership option changes are patches. PostgreSQL preserves an existing
/// option when it is omitted; defaults are applied only to a new edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleMembershipPatch {
    pub admin: Option<bool>,
    pub inherit: Option<bool>,
    pub set: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleMembershipClauses<'a> {
    /// Parent roles granted to the new role by `IN ROLE` / `IN GROUP`.
    pub in_roles: &'a [&'a str],
    /// Existing roles made members of the new role by `ROLE`.
    pub role_members: &'a [&'a str],
    /// Existing roles made members of the new role with admin option.
    pub admin_members: &'a [&'a str],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleSettingAction<'a> {
    Set {
        name: &'a str,
        value: RoutineConfigValue<'a>,
    },
    Reset(Option<&'a str>),
}

impl RoleMembershipPatch {
    pub const EMPTY: Self = Self {
        admin: None,
        inherit: None,
        set: None,
    };
}

/// Attribute changes shared by CREATE ROLE and ALTER ROLE. `None` means the
/// option was not written (CREATE applies PostgreSQL's defaults; ALTER keeps
/// the current value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleOptions<'a> {
    pub superuser: Option<bool>,
    pub inherit: Option<bool>,
    pub create_role: Option<bool>,
    pub create_database: Option<bool>,
    pub can_login: Option<bool>,
    pub replication: Option<bool>,
    pub bypass_row_level_security: Option<bool>,
    pub connection_limit: Option<i32>,
    /// `Some(None)` is PASSWORD NULL; `None` means no PASSWORD clause.
    pub password: Option<Option<&'a str>>,
    /// Canonical source text of VALID UNTIL, or NULL for infinity.
    pub valid_until: Option<Option<&'a str>>,
}

impl RoleOptions<'_> {
    pub const EMPTY: Self = Self {
        superuser: None,
        inherit: None,
        create_role: None,
        create_database: None,
        can_login: None,
        replication: None,
        bypass_row_level_security: None,
        connection_limit: None,
        password: None,
        valid_until: None,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastContext {
    Explicit,
    Assignment,
    Implicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastMethod<'a> {
    Function {
        name: QualName<'a>,
        argument_types: &'a [&'a str],
    },
    Binary,
    InOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateCast<'a> {
    pub source_type: &'a str,
    pub target_type: &'a str,
    pub method: CastMethod<'a>,
    pub context: CastContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropCast<'a> {
    pub source_type: &'a str,
    pub target_type: &'a str,
    pub if_exists: bool,
    pub cascade: bool,
}

/// PostgreSQL exposes only the btree access method in pos3ql's modeled index
/// runtime. Parsing produces this closed value before catalog mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexAccessMethod {
    Btree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorOperands<'a> {
    Prefix(&'a str),
    Binary { left: &'a str, right: &'a str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorIdentity<'a> {
    pub name: QualName<'a>,
    pub operands: OperatorOperands<'a>,
}

pub(crate) const CATALOG_OPERATOR_CALL_PREFIX: &str = "\u{1}operator\u{1f}";

pub(crate) fn catalog_operator_call(name: &str) -> Option<(Option<&str>, &str)> {
    let encoded = name.strip_prefix(CATALOG_OPERATOR_CALL_PREFIX)?;
    let (schema, operator) = encoded.split_once('\u{1f}')?;
    Some(((!schema.is_empty()).then_some(schema), operator))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateOperator<'a> {
    pub name: QualName<'a>,
    pub function: QualName<'a>,
    pub operands: OperatorOperands<'a>,
    pub commutator: Option<QualName<'a>>,
    pub negator: Option<QualName<'a>>,
    pub hashes: bool,
    pub merges: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterOperatorAction<'a> {
    Owner(&'a str),
    SetSchema(&'a str),
    Set {
        commutator: Option<QualName<'a>>,
        negator: Option<QualName<'a>>,
        hashes: bool,
        merges: bool,
    },
}

/// Btree operator strategies are a closed 1..=5 domain. A raw integer can
/// therefore never enter durable operator-family state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtreeStrategy {
    Less = 1,
    LessEqual,
    Equal,
    GreaterEqual,
    Greater,
}

impl BtreeStrategy {
    pub const fn from_number(number: u32) -> Option<Self> {
        match number {
            1 => Some(Self::Less),
            2 => Some(Self::LessEqual),
            3 => Some(Self::Equal),
            4 => Some(Self::GreaterEqual),
            5 => Some(Self::Greater),
            _ => None,
        }
    }

    pub const fn number(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorFamilyMember<'a> {
    Operator {
        strategy: BtreeStrategy,
        operator: OperatorIdentity<'a>,
    },
    CompareFunction {
        left_type: &'a str,
        right_type: &'a str,
        function: QualName<'a>,
        argument_types: &'a [&'a str],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterOperatorFamilyAction<'a> {
    Add(&'a [OperatorFamilyMember<'a>]),
    Drop(&'a [OperatorFamilyMemberIdentity<'a>]),
    Rename(&'a str),
    Owner(&'a str),
    SetSchema(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorFamilyMemberIdentity<'a> {
    Operator {
        strategy: BtreeStrategy,
        left_type: &'a str,
        right_type: &'a str,
    },
    CompareFunction {
        left_type: &'a str,
        right_type: &'a str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorClassMember<'a> {
    Operator {
        strategy: BtreeStrategy,
        operator: QualName<'a>,
        operand_types: Option<(&'a str, &'a str)>,
    },
    CompareFunction {
        operand_types: Option<(&'a str, &'a str)>,
        function: QualName<'a>,
        argument_types: &'a [&'a str],
    },
    Storage(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateOperatorClass<'a> {
    pub name: QualName<'a>,
    pub default: bool,
    pub input_type: &'a str,
    pub method: IndexAccessMethod,
    pub family: Option<QualName<'a>>,
    pub members: &'a [OperatorClassMember<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterOperatorClassAction<'a> {
    Rename(&'a str),
    Owner(&'a str),
    SetSchema(&'a str),
}

/// One btree index key. A plain column retains its resolved name; every other
/// key is an expression with durable canonical source. PostgreSQL's defaults
/// depend on direction: ascending keys put NULLs last, descending keys first.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IndexColumn<'a> {
    pub column: Option<&'a str>,
    pub expression: &'a Expr<'a>,
    pub expression_text: &'a str,
    pub collation: Option<ParsedCollation<'a>>,
    pub operator_class: Option<QualName<'a>>,
    pub descending: bool,
    pub nulls_first: bool,
}

/// The relation class selected by REINDEX. Other PostgreSQL forms require
/// global/catalog state this engine does not expose as a user relation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReindexTarget {
    Index,
    Table,
    Schema,
    Database,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReindexOptions<'a> {
    pub build: IndexBuildMode,
    pub tablespace: Option<&'a str>,
    pub verbose: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterTarget<'a> {
    All,
    Table {
        table: QualName<'a>,
        index: Option<QualName<'a>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterOwnerKind {
    Schema,
    Type,
    Domain,
    Table,
    ForeignTable,
    View,
    MaterializedView,
    Sequence,
    Statistics,
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
    Relation {
        kind: CommentRelKind,
        name: QualName<'a>,
    },
    /// COLUMN table.column.
    Column {
        relation: QualName<'a>,
        column: &'a str,
    },
    /// SCHEMA name.
    Schema(&'a str),
    /// TABLESPACE name.
    Tablespace(&'a str),
    Database(&'a str),
    LargeObject(LargeObjectId),
    Collation(QualName<'a>),
    Conversion(QualName<'a>),
    TextSearch {
        kind: TextSearchObjectKind,
        name: QualName<'a>,
    },
    EventTrigger(&'a str),
    /// EXTENSION name.
    Extension(&'a str),
    /// TRIGGER name ON relation; trigger names are relation-local.
    Trigger(TriggerIdentity<'a>),
    /// RULE name ON relation; rewrite-rule names are relation-local.
    Rule(TriggerIdentity<'a>),
    /// TYPE name, or DOMAIN name when `domain_only` requires that kind.
    Type {
        name: &'a str,
        domain_only: bool,
    },
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
    Op {
        operator: SetOp,
        all: bool,
        left: &'a SetTree<'a>,
        right: &'a SetTree<'a>,
    },
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
    /// Trailing `FOR UPDATE`/… clauses. A set operation can never be locked, so
    /// a non-empty list here is rejected at execution; carried only to raise
    /// PostgreSQL's exact error.
    pub locking: &'a [LockClause<'a>],
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
    /// Whether duplicate grouping sets are retained (`ALL`, PostgreSQL's
    /// default) or collapsed before aggregation (`DISTINCT`).
    pub grouping_set_quantifier: GroupingSetQuantifier,
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
    /// `FOR UPDATE`/`FOR SHARE`/… row-locking clauses, in written order. Empty
    /// when the query carries none.
    pub locking: &'a [LockClause<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupingSetQuantifier {
    All,
    Distinct,
}

/// The strength of a `FOR ...` row-locking clause, strongest first (this order
/// is how PostgreSQL combines two clauses naming the same table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStrength {
    Update,
    NoKeyUpdate,
    Share,
    KeyShare,
}

impl LockStrength {
    /// The clause keyword as PostgreSQL spells it in errors (`FOR UPDATE`, …).
    pub fn keyword(self) -> &'static str {
        match self {
            LockStrength::Update => "FOR UPDATE",
            LockStrength::NoKeyUpdate => "FOR NO KEY UPDATE",
            LockStrength::Share => "FOR SHARE",
            LockStrength::KeyShare => "FOR KEY SHARE",
        }
    }
}

/// What a row-locking clause does when a target row is already locked by another
/// transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockWait {
    /// Block until the lock is released (PostgreSQL's default).
    Wait,
    /// Raise `55P03` rather than wait.
    NoWait,
    /// Omit the locked row from the result.
    SkipLocked,
}

/// PostgreSQL's table-level lock modes, weakest to strongest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableLockMode {
    AccessShare,
    RowShare,
    RowExclusive,
    ShareUpdateExclusive,
    Share,
    ShareRowExclusive,
    Exclusive,
    AccessExclusive,
}

/// A single `FOR { UPDATE | NO KEY UPDATE | SHARE | KEY SHARE } [OF t, …]
/// [NOWAIT | SKIP LOCKED]` row-locking clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockClause<'a> {
    pub strength: LockStrength,
    /// Tables named in `OF`; empty means every base table in the FROM clause.
    pub of: &'a [&'a str],
    pub wait: LockWait,
}

/// PostgreSQL's materialization directive on one common table expression.
/// Keeping the three grammar states distinct lets execution apply the
/// evaluate-once contract without inferring it from missing syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CteMaterialization {
    Default,
    Materialized,
    NotMaterialized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CteSearchOrder {
    BreadthFirst,
    DepthFirst,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CteSearch<'a> {
    pub order: CteSearchOrder,
    pub columns: &'a [&'a str],
    pub sequence_column: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CteCycle<'a> {
    pub columns: &'a [&'a str],
    pub mark_column: &'a str,
    pub mark: CteCycleMark<'a>,
    pub path_column: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CteCycleMark<'a> {
    Boolean,
    Custom {
        value: &'a Expr<'a>,
        default: &'a Expr<'a>,
    },
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
    pub materialization: CteMaterialization,
    pub search: Option<CteSearch<'a>>,
    pub cycle: Option<CteCycle<'a>>,
    /// The CTE body as a query. For a data-modifying CTE (`dml` is `Some`) this
    /// is a placeholder and unused.
    pub query: &'a Select<'a>,
    /// A data-modifying CTE body (`WITH x AS (INSERT/UPDATE/DELETE ... RETURNING
    /// ...)`): the statement runs exactly once and its RETURNING rows become the
    /// CTE relation. `None` for an ordinary query CTE.
    pub dml: Option<&'a Stmt<'a>>,
}

/// The materialized rows of a recursive CTE, bound during CTE expansion so a
/// `FROM cte_name` reference resolves to a pre-computed row set instead of an
/// inline subquery. Rows are projected-encoded; type, typmod, and collation
/// metadata retain the exact derived-relation state without a storage-layer
/// dependency.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MaterializedCte<'a> {
    pub column_names: &'a [&'a str],
    pub column_types: &'a [(i32, i16, i32)],
    pub column_collations: &'a [Collation],
    pub(crate) source: MaterializedCteSource<'a>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum MaterializedCteSource<'a> {
    Inline(&'a [&'a [u8]]),
    External(Option<crate::sql::external::ExternalRun>),
    RecursiveInline(&'a core::sync::atomic::AtomicUsize),
    RecursiveExternal(&'a core::sync::atomic::AtomicUsize),
}

#[derive(Clone, Copy)]
pub(crate) struct MaterializedCteInlineSource {
    pub address: usize,
    pub length: usize,
}

impl PartialEq for MaterializedCteSource<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (*self, *other) {
            (Self::Inline(left), Self::Inline(right)) => left == right,
            (Self::External(left), Self::External(right)) => left == right,
            (Self::RecursiveInline(left), Self::RecursiveInline(right)) => {
                core::ptr::eq(left, right)
            }
            (Self::RecursiveExternal(left), Self::RecursiveExternal(right)) => {
                core::ptr::eq(left, right)
            }
            _ => false,
        }
    }
}

impl<'a> MaterializedCte<'a> {
    pub(crate) fn rows(&self) -> &'a [&'a [u8]] {
        match self.source {
            MaterializedCteSource::Inline(rows) => rows,
            MaterializedCteSource::RecursiveInline(source) => {
                let source = source.load(core::sync::atomic::Ordering::Relaxed);
                if source == 0 {
                    return &[];
                }
                // The selector names one immutable arena-owned descriptor, so
                // an update cannot expose a pointer/length from different rows.
                let source = unsafe { &*(source as *const MaterializedCteInlineSource) };
                unsafe {
                    core::slice::from_raw_parts(source.address as *const &'a [u8], source.length)
                }
            }
            MaterializedCteSource::External(_) | MaterializedCteSource::RecursiveExternal(_) => &[],
        }
    }

    pub(crate) fn external_run(&self) -> Option<crate::sql::external::ExternalRun> {
        match self.source {
            MaterializedCteSource::External(run) => run,
            MaterializedCteSource::RecursiveExternal(address) => {
                let address = address.load(core::sync::atomic::Ordering::Relaxed);
                if address == 0 {
                    None
                } else {
                    // Each address names an immutable arena-owned run value.
                    Some(unsafe { *(address as *const crate::sql::external::ExternalRun) })
                }
            }
            MaterializedCteSource::Inline(_) | MaterializedCteSource::RecursiveInline(_) => None,
        }
    }
}

/// A base table plus a chain of joins (nested-loop order).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FromClause<'a> {
    /// (table name, optional alias).
    pub base: TableRef<'a>,
    pub joins: &'a [Join<'a>],
}

/// Whether a relation source includes its inheritance/partition descendants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationInheritance {
    Descendants,
    Only,
}

/// The built-in PostgreSQL sampling methods pos3ql can execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableSampleMethod {
    System,
    Bernoulli,
}

/// A relation's typed `TABLESAMPLE` clause.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TableSample<'a> {
    pub method: TableSampleMethod,
    pub percentage: &'a Expr<'a>,
    pub repeatable: Option<&'a Expr<'a>>,
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
    /// Empty for positional function sources; otherwise parallel to
    /// `func_args`. The parser constructs both together.
    pub func_argument_names: &'a [Option<&'a str>],
    /// The last function-source argument is an already-packed array.
    pub func_variadic: bool,
    /// `ROWS FROM (f(...), g(...))`: each entry is a function-only table
    /// reference. The outer source owns the shared alias and ordinality, so a
    /// parsed table source is either one function or one non-empty function
    /// group, never both.
    pub rows_from: Option<&'a [TableRef<'a>]>,
    /// Column-alias list (`alias(c1, c2, ...)`): renames leading output columns.
    pub col_alias: Option<&'a [&'a str]>,
    /// Default inheritance traversal or explicit `ONLY` selection.
    pub inheritance: RelationInheritance,
    /// Sampling applies only to a physical table or materialized view.
    pub sample: Option<TableSample<'a>>,
    /// Materialized recursive-CTE reference: when set, this FROM item reads the
    /// pre-computed row set instead of a table or subquery.
    pub cte: Option<&'a MaterializedCte<'a>>,
    /// `func(args) WITH ORDINALITY`: append a 1-based `bigint` ordinality column
    /// to a table function's output. Only valid on a table-function FROM item.
    pub with_ordinality: bool,
    /// `LATERAL (subquery)` / `LATERAL func(...)`: the FROM item may reference
    /// columns of the FROM items to its left, and is re-evaluated per outer row.
    pub lateral: bool,
    /// Role slot whose privileges apply to this physical relation reference.
    /// Set only by stored-view expansion; ordinary parsed references use the
    /// current effective role.
    pub authorization_role: Option<u16>,
}

impl TableRef<'_> {
    pub const fn is_function_source(&self) -> bool {
        self.func_args.is_some() || self.rows_from.is_some()
    }
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
    /// Typed `USING` clause. Keeping its optional alias inside the clause
    /// makes an alias without merged columns unrepresentable.
    pub using: Option<JoinUsing<'a>>,
    /// NATURAL join: the using-column list is every common column name,
    /// resolved at plan time.
    pub natural: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JoinUsing<'a> {
    /// Each name resolves once on the left join tree and once on the right.
    pub columns: &'a [&'a str],
    /// Qualifies only the merged columns and shares the table-alias namespace.
    pub alias: Option<&'a str>,
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
    Expr {
        expression: &'a Expr<'a>,
        alias: Option<&'a str>,
    },
}

/// A window function's resolved `OVER` clause.
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
    /// The table's partitioning role.  This is structured at parse time so
    /// execution never needs to reinterpret a SQL fragment as a bound.
    pub partition: PartitionClause<'a>,
    /// PostgreSQL table inheritance or typed-table membership. The executor
    /// rejects these closed states until it can preserve their scan and
    /// dependency semantics through durable storage.
    pub membership: TableMembership<'a>,
    /// Relation persistence requested by the client. Only permanent tables
    /// can enter object-native durable storage.
    pub persistence: RelationPersistence,
    /// The PostgreSQL table access method, resolved before storage mutation.
    pub access_method: TableAccessMethod<'a>,
    /// An explicit relation tablespace. `None` selects the database default.
    pub tablespace: Option<&'a str>,
    /// Heap storage options. They are parsed as a closed state rather than
    /// accepted as inert catalog text.
    pub storage_options: RelationStorageOptions,
    pub if_not_exists: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TableMembership<'a> {
    None,
    Inherits(&'a [QualName<'a>]),
    OfType(QualName<'a>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelationStorageOptions {
    pub fillfactor: Option<u8>,
}

impl RelationStorageOptions {
    pub const DEFAULT: Self = Self { fillfactor: None };

    pub const fn is_empty(self) -> bool {
        self.fillfactor.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelationStorageOptionNames {
    pub fillfactor: bool,
}

impl RelationStorageOptionNames {
    pub const EMPTY: Self = Self { fillfactor: false };
}

/// Parsed access-method spelling; only an executable variant can reach a
/// durable relation definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAccessMethod<'a> {
    Heap,
    /// A syntactically valid access-method name whose catalog resolution is
    /// deferred to execution. It cannot reach storage unless it resolves to
    /// one of the executable variants above.
    Named(&'a str),
}

/// PostgreSQL's table-persistence grammar, parsed before execution decides
/// whether the object-native durability contract can realize it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationPersistence {
    Permanent,
    Unlogged,
    Temporary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnStorage {
    Plain,
    External,
    Extended,
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnCompression {
    Default,
    Pglz,
    Lz4,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PartitionClause<'a> {
    None,
    By {
        strategy: PartitionStrategy,
        columns: &'a [&'a str],
    },
    Of {
        parent: QualName<'a>,
        bound: PartitionBound<'a>,
        subpartition: Option<PartitionBy<'a>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PartitionBy<'a> {
    pub strategy: PartitionStrategy,
    pub columns: &'a [&'a str],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionStrategy {
    Range,
    List,
    Hash,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PartitionBound<'a> {
    Default,
    Range {
        from: &'a [&'a Expr<'a>],
        to: &'a [&'a Expr<'a>],
    },
    List {
        values: &'a [&'a Expr<'a>],
    },
    Hash {
        modulus: &'a Expr<'a>,
        remainder: &'a Expr<'a>,
    },
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
        timing: ConstraintTiming,
    },
    Unique {
        name: Option<&'a str>,
        columns: &'a [&'a str],
        timing: ConstraintTiming,
    },
    Check {
        name: Option<&'a str>,
        expression: &'a Expr<'a>,
        /// Source text of the predicate, stored durably and re-parsed at
        /// enforcement time.
        text: &'a str,
        validation: ConstraintValidation,
    },
    ForeignKey {
        name: Option<&'a str>,
        columns: &'a [&'a str],
        parent: QualName<'a>,
        /// Referenced columns; empty means "the parent's primary key".
        parent_cols: &'a [&'a str],
        on_delete: FkAction,
        on_update: FkAction,
        timing: ConstraintTiming,
        validation: ConstraintValidation,
    },
    Exclusion {
        name: Option<&'a str>,
        columns: &'a [&'a str],
        operators: &'a [ExclusionOperator],
        predicate: Option<&'a Expr<'a>>,
        predicate_text: Option<&'a str>,
        timing: ConstraintTiming,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusionOperator {
    Equal,
    Overlaps,
    Adjacent,
}

/// The transaction-local check mode selected by a constraint definition or
/// `SET CONSTRAINTS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintMode {
    Immediate,
    Deferred,
}

/// A constraint is either permanently immediate or has an explicit initial
/// mode. This prevents `INITIALLY DEFERRED` from existing independently of
/// `DEFERRABLE` after parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintTiming {
    NotDeferrable,
    Deferrable(ConstraintMode),
}

impl ConstraintTiming {
    pub const fn is_deferrable(self) -> bool {
        matches!(self, Self::Deferrable(_))
    }

    pub const fn initially_deferred(self) -> bool {
        matches!(self, Self::Deferrable(ConstraintMode::Deferred))
    }
}

/// Validation and enforcement cannot form the contradictory state "validated
/// but not enforced".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintValidation {
    EnforcedValidated,
    EnforcedNotValid,
    NotEnforced,
}

impl ConstraintValidation {
    pub const fn enforced(self) -> bool {
        !matches!(self, Self::NotEnforced)
    }

    pub const fn validated(self) -> bool {
        matches!(self, Self::EnforcedValidated)
    }
}

/// `ALL` is a semantic selector, not a constraint named `all`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintTargets<'a> {
    All,
    Named(&'a [QualName<'a>]),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// None = omitted; Some(None) = OWNED BY NONE; Some(Some(owner)) assigns
    /// the sequence to a table column.
    pub owned_by: Option<Option<SeqOwner<'a>>>,
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
        owned_by: None,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeqOwner<'a> {
    pub table: QualName<'a>,
    pub column: &'a str,
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

/// An input-capable routine parameter either requires a call argument or owns
/// a parsed default. Keeping `VARIADIC` separate prevents an INOUT/OUT
/// parameter from accidentally acquiring variadic call semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineArgumentMode<'a> {
    In { default_text: Option<&'a str> },
    Out,
    InOut { default_text: Option<&'a str> },
    Variadic { default_text: Option<&'a str> },
}

impl<'a> RoutineArgumentMode<'a> {
    pub const fn is_input(self) -> bool {
        !matches!(self, Self::Out)
    }

    pub const fn is_output(self) -> bool {
        matches!(self, Self::Out | Self::InOut { .. })
    }

    pub const fn default_text(self) -> Option<&'a str> {
        match self {
            Self::In { default_text }
            | Self::InOut { default_text }
            | Self::Variadic { default_text } => default_text,
            Self::Out => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutineArgument<'a> {
    pub mode: RoutineArgumentMode<'a>,
    /// PostgreSQL permits unnamed input and output parameters. Absence is a
    /// distinct state rather than an empty identifier accepted by accident.
    pub name: Option<&'a str>,
    pub type_name: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutineResultColumn<'a> {
    pub name: &'a str,
    pub type_name: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregateArgument<'a> {
    pub name: &'a str,
    pub type_name: &'a str,
    pub variadic: bool,
}

/// The syntactic aggregate kind determines which arguments are accumulated.
/// Keeping the alternatives distinct prevents ordinary aggregates from
/// acquiring direct arguments or hypothetical semantics later in execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateArguments<'a> {
    Normal(&'a [AggregateArgument<'a>]),
    OrderedSet {
        direct: &'a [AggregateArgument<'a>],
        aggregated: &'a [AggregateArgument<'a>],
        hypothetical: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFinalModify {
    ReadOnly,
    Shareable,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineParallel {
    Safe,
    Restricted,
    Unsafe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregateFinal<'a> {
    pub function: QualName<'a>,
    pub extra: bool,
    pub modify: AggregateFinalModify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregateMoving<'a> {
    pub transition: QualName<'a>,
    pub inverse: QualName<'a>,
    pub state_type: &'a str,
    pub state_space: Option<u32>,
    pub final_function: Option<AggregateFinal<'a>>,
    pub initial_condition: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregatePartial<'a> {
    pub combine: QualName<'a>,
    pub serial: Option<QualName<'a>>,
    pub deserial: Option<QualName<'a>>,
}

/// A parsed aggregate definition. Required options are non-optional and option
/// families that must be complete are represented by their own value types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregateDefinition<'a> {
    pub transition: QualName<'a>,
    pub state_type: &'a str,
    pub state_space: Option<u32>,
    pub final_function: Option<AggregateFinal<'a>>,
    pub partial: Option<AggregatePartial<'a>>,
    pub moving: Option<AggregateMoving<'a>>,
    pub initial_condition: Option<&'a str>,
    pub sort_operator: Option<&'a str>,
    pub parallel: RoutineParallel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateAggregate<'a> {
    pub name: QualName<'a>,
    pub or_replace: bool,
    pub arguments: AggregateArguments<'a>,
    pub definition: AggregateDefinition<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregateIdentity<'a> {
    pub name: QualName<'a>,
    pub direct_argument_types: &'a [&'a str],
    pub aggregated_argument_types: &'a [&'a str],
    pub ordered_set: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateRoutine<'a> {
    pub name: QualName<'a>,
    pub or_replace: bool,
    pub arguments: &'a [RoutineArgument<'a>],
    pub kind: RoutineCreateKind<'a>,
    pub language: RoutineLanguage,
    pub attributes: RoutineAttributes,
    pub configs: &'a [RoutineConfigClause<'a>],
    pub body: RoutineBody<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutineConfigClause<'a> {
    pub name: &'a str,
    pub value: RoutineConfigValue<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineConfigValue<'a> {
    Value(&'a str),
    Current,
}

/// The three PostgreSQL SQL-routine body forms have different binding and
/// catalog semantics, so the body spelling cannot safely stand in for its
/// form. `Return` stores the expression text; `Atomic` stores the statements
/// between BEGIN ATOMIC and END.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineBody<'a> {
    String(&'a str),
    Return(&'a str),
    Atomic(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutineEstimate(u64);

impl RoutineEstimate {
    pub fn new(value: f64) -> Option<Self> {
        (value.is_finite() && value > 0.0).then_some(Self(value.to_bits()))
    }

    pub fn get(self) -> f64 {
        f64::from_bits(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutineAttributes {
    pub strict: bool,
    pub volatility: RoutineVolatility,
    pub parallel: RoutineParallel,
    pub security_definer: bool,
    pub leakproof: bool,
    pub cost: Option<RoutineEstimate>,
    pub rows: Option<RoutineEstimate>,
}

impl Default for RoutineAttributes {
    fn default() -> Self {
        Self {
            strict: false,
            volatility: RoutineVolatility::Volatile,
            parallel: RoutineParallel::Unsafe,
            security_definer: false,
            leakproof: false,
            cost: None,
            rows: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineVolatility {
    Immutable,
    Stable,
    Volatile,
}

/// The parser accepts only languages whose execution contract is represented
/// by the routine kind; callers never receive an unchecked language string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineLanguage {
    Sql,
    PlPgSql,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateLanguage<'a> {
    pub name: &'a str,
    pub or_replace: bool,
    pub trusted: bool,
    pub handler: Option<QualName<'a>>,
    pub inline: Option<QualName<'a>>,
    pub validator: Option<QualName<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterLanguageAction<'a> {
    Rename(&'a str),
    SetOwner(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineCreateKind<'a> {
    Function {
        result_type: &'a str,
        set_returning: bool,
    },
    /// OUT/INOUT parameters define the result contract. An optional RETURNS
    /// clause is retained only so execution can resolve and prove that it
    /// agrees with the implied scalar/record type.
    OutputFunction {
        declared_result_type: Option<&'a str>,
        set_returning: bool,
    },
    TableFunction {
        columns: &'a [RoutineResultColumn<'a>],
    },
    Trigger,
    EventTrigger,
    Procedure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterRoutineAction<'a> {
    SetOwner(&'a str),
    Rename(&'a str),
    SetSchema(&'a str),
    ExtensionDependency {
        extension: &'a str,
        enabled: bool,
    },
    SetStrict(bool),
    SetVolatility(RoutineVolatility),
    SetLeakproof(bool),
    SetSecurityDefiner(bool),
    SetParallel(RoutineParallel),
    SetCost(RoutineEstimate),
    SetRows(RoutineEstimate),
    SetConfig {
        name: &'a str,
        value: RoutineConfigValue<'a>,
    },
    ResetConfig(Option<&'a str>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionRelationKind {
    Table,
    View,
    MaterializedView,
    Sequence,
}

/// A closed extension-member identity prevents unsupported object kinds from
/// reaching the durable dependency graph as unchecked names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionMemberIdentity<'a> {
    Aggregate(AggregateIdentity<'a>),
    Routine {
        kind: RoutineTargetKind,
        identity: RoutineIdentity<'a>,
    },
    Relation {
        kind: ExtensionRelationKind,
        name: QualName<'a>,
    },
    Schema(&'a str),
    Domain(QualName<'a>),
    Type(QualName<'a>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterExtensionAction<'a> {
    Update {
        version: Option<&'a str>,
    },
    SetSchema(&'a str),
    Member {
        add: bool,
        object: ExtensionMemberIdentity<'a>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutineIdentity<'a> {
    pub name: QualName<'a>,
    pub argument_types: &'a [&'a str],
    /// `false` only for the PostgreSQL shorthand `DROP FUNCTION name`.
    pub signature_is_explicit: bool,
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
    Rename(&'a str),
    SetSchema(&'a str),
}

/// One `ALTER TYPE` action. Enum and named-composite actions are distinct
/// variants, so execution cannot apply an enum mutation to an attribute layout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AlterTypeAction<'a> {
    /// ADD VALUE [IF NOT EXISTS] 'label' [ {BEFORE|AFTER} 'existing' ].
    AddValue {
        label: &'a str,
        if_not_exists: bool,
        before: Option<&'a str>,
        after: Option<&'a str>,
    },
    /// RENAME TO new_name (renames the type itself, not a value).
    RenameTo(&'a str),
    /// SET SCHEMA new_schema. The durable type identity remains its catalog slot.
    SetSchema(&'a str),
    /// RENAME VALUE 'old' TO 'new' — rejected (values are stored inline).
    RenameValue { from: &'a str, to: &'a str },
    /// ADD ATTRIBUTE name type.
    AddAttribute(CompositeField<'a>),
    /// DROP ATTRIBUTE [IF EXISTS] name.
    DropAttribute { name: &'a str, if_exists: bool },
    /// RENAME ATTRIBUTE old TO new.
    RenameAttribute { from: &'a str, to: &'a str },
    /// ALTER ATTRIBUTE name [SET DATA] TYPE type.
    AlterAttributeType {
        name: &'a str,
        type_name: &'a str,
        type_mod: i32,
        collation: ParsedCollation<'a>,
    },
    /// ALTER ATTRIBUTE name SET NOT NULL.
    SetAttributeNotNull(&'a str),
    /// ALTER ATTRIBUTE name DROP NOT NULL.
    DropAttributeNotNull(&'a str),
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
    pub cascade: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColumnDef<'a> {
    pub name: &'a str,
    pub type_name: &'a str,
    /// PostgreSQL atttypmod for the declared type: -1 when no `(...)` modifier.
    /// varchar(n)/char(n) encode `n + 4`; numeric(p,s) encodes `((p<<16)|s)+4`.
    pub type_mod: i32,
    /// The collation selected by `COLLATE` or the database default.
    pub collation: ParsedCollation<'a>,
    /// Foreign-column options are parsed only by CREATE FOREIGN TABLE. An
    /// ordinary table can therefore reach execution only with an empty list.
    pub foreign_options: &'a [ForeignOption<'a>],
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
    pub identity: Option<IdentitySpec<'a>>,
}

/// A `GENERATED ... AS IDENTITY` specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentitySpec<'a> {
    /// `ALWAYS` (reject explicit inserts) vs `BY DEFAULT` (explicit allowed).
    pub always: bool,
    /// Optional PostgreSQL-generated or user-selected backing sequence name.
    pub sequence_name: Option<QualName<'a>>,
    /// The backing sequence's full parameter set.
    pub options: SeqOptions<'a>,
}

/// The result of parsing a `GENERATED` column clause.
pub enum ColGen<'a> {
    /// `ALWAYS AS (expr) STORED`.
    Generated(&'a str),
    /// `{ ALWAYS | BY DEFAULT } AS IDENTITY [(options)]`.
    Identity(IdentitySpec<'a>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CopyStmt<'a> {
    pub table: QualName<'a>,
    /// Empty means "all columns in table order".
    pub columns: &'a [&'a str],
    /// `COPY (query) TO STDOUT`: the parenthesized query's raw text, re-parsed at
    /// execution. When set, `table`/`columns` are unused and only `TO` is legal.
    pub query: Option<&'a str>,
    /// `TO STDOUT` when true; `FROM STDIN` otherwise.
    pub to: bool,
    pub options: CopyOptions<'a>,
    /// `COPY FROM ... WHERE` predicate, parsed with the statement so invalid
    /// syntax cannot enter the streaming protocol.
    pub where_clause: Option<&'a Expr<'a>>,
    /// The parsed predicate's exact source. COPY input outlives the statement
    /// arena, so execution owns this bounded source in its setup.
    pub where_text: Option<&'a str>,
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

/// The header contract for text or CSV COPY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyHeader {
    None,
    Skip,
    Match,
}

/// COPY FROM's conversion-error policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyErrorAction {
    Stop,
    Ignore,
}

/// Notices emitted for rows discarded by `ON_ERROR ignore`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyLogVerbosity {
    Default,
    Verbose,
    Silent,
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
    /// CSV/text header handling. `MATCH` validates names during COPY FROM.
    pub header: CopyHeader,
    /// Text/CSV COPY FROM conversion-error handling.
    pub on_error: CopyErrorAction,
    /// Maximum discarded conversion errors when `on_error` is `Ignore`.
    pub reject_limit: Option<u64>,
    /// Reporting policy for discarded conversion errors.
    pub log_verbosity: CopyLogVerbosity,
    /// COPY FROM text/CSV sentinel that selects a column default.
    pub default_string: Option<&'a str>,
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
        header: CopyHeader::None,
        on_error: CopyErrorAction::Stop,
        reject_limit: None,
        log_verbosity: CopyLogVerbosity::Default,
        default_string: None,
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
    /// Conflict-target keys (`ON CONFLICT (a, lower(b))`); empty means no
    /// column-inference target was given (either a named `constraint`, or —
    /// with neither — any unique constraint for DO NOTHING).
    pub target: &'a [OnConflictTarget<'a>],
    /// A named arbiter constraint (`ON CONFLICT ON CONSTRAINT name`); mutually
    /// exclusive with a column `target`.
    pub constraint: Option<&'a str>,
    /// `None` = DO NOTHING; `Some` = DO UPDATE SET .... Assignments may
    /// reference the target row's columns and `excluded.<col>` (the proposed
    /// row).
    pub update: Option<&'a [(&'a str, &'a Expr<'a>)]>,
    /// Optional WHERE on DO UPDATE.
    pub update_where: Option<&'a Expr<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OnConflictTarget<'a> {
    pub column: Option<&'a str>,
    pub expression: &'a Expr<'a>,
    pub expression_text: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Update<'a> {
    pub table: QualName<'a>,
    pub alias: Option<&'a str>,
    pub assignments: &'a [(&'a str, &'a Expr<'a>)],
    /// Extra tables joined for the assignment/WHERE (`UPDATE t SET ... FROM e`).
    pub from: Option<&'a FromClause<'a>>,
    pub where_clause: Option<&'a Expr<'a>>,
    pub returning: &'a [SelectItem<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Delete<'a> {
    pub table: QualName<'a>,
    pub alias: Option<&'a str>,
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
    Insert {
        columns: &'a [&'a str],
        values: &'a [&'a Expr<'a>],
        default_values: bool,
    },
    /// `DO NOTHING`.
    DoNothing,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlterTable<'a> {
    pub table: QualName<'a>,
    /// ALTER TABLE IF EXISTS: a missing target emits a notice and succeeds.
    pub if_exists: bool,
    /// `ONLY` suppresses partition recursion.
    pub only: bool,
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
    SetForeignOptions(&'a [ForeignOptionAction<'a>]),
    SetColumnForeignOptions {
        column: &'a str,
        options: &'a [ForeignOptionAction<'a>],
    },
    RenameColumn {
        from: &'a str,
        to: &'a str,
    },
    AddColumn(ColumnDef<'a>),
    DropColumn {
        name: &'a str,
        if_exists: bool,
        cascade: bool,
    },
    /// ALTER [COLUMN] col SET DEFAULT expr.
    SetDefault {
        column: &'a str,
        value: &'a Expr<'a>,
        value_text: &'a str,
    },
    /// ALTER [COLUMN] col DROP DEFAULT.
    DropDefault {
        column: &'a str,
    },
    /// ALTER [COLUMN] col ADD GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY.
    AddIdentity {
        column: &'a str,
        spec: IdentitySpec<'a>,
    },
    /// ALTER [COLUMN] col DROP IDENTITY [IF EXISTS].
    DropIdentity {
        column: &'a str,
        if_exists: bool,
    },
    /// ALTER [COLUMN] col SET GENERATED { ALWAYS | BY DEFAULT }.
    SetIdentityMode {
        column: &'a str,
        always: bool,
    },
    /// ALTER [COLUMN] col SET EXPRESSION AS (expr).
    SetGeneratedExpression {
        column: &'a str,
        expression_text: &'a str,
    },
    /// ALTER [COLUMN] col SET identity-sequence options.
    AlterIdentitySequence {
        column: &'a str,
        options: SeqOptions<'a>,
    },
    /// ALTER [COLUMN] col SET NOT NULL — validated against existing rows.
    SetNotNull {
        column: &'a str,
    },
    /// ALTER [COLUMN] col DROP NOT NULL.
    DropNotNull {
        column: &'a str,
    },
    /// ALTER [COLUMN] col SET STATISTICS target. `-1` is PostgreSQL's
    /// `DEFAULT` target and remains a valid stored value.
    SetStatistics {
        column: &'a str,
        target: i16,
    },
    SetStorage {
        column: &'a str,
        storage: ColumnStorage,
    },
    SetCompression {
        column: &'a str,
        compression: ColumnCompression,
    },
    /// ALTER [COLUMN] col [SET DATA] TYPE newtype [USING expr]. Without `using`
    /// the stored value is cast through the assignment cast; with it, `using`
    /// is evaluated per row (the old columns in scope) and cast to the type.
    AlterColumnType {
        column: &'a str,
        type_name: &'a str,
        type_mod: i32,
        collation: Option<ParsedCollation<'a>>,
        using: Option<&'a Expr<'a>>,
    },
    /// ALTER TABLE ... ADD [CONSTRAINT name] <table constraint>. Existing rows
    /// are validated against the new constraint before it is attached.
    AddConstraint(TableConstraint<'a>),
    /// ALTER TABLE ... ADD [CONSTRAINT name] { UNIQUE | PRIMARY KEY } USING
    /// INDEX. The index and the constraint become one durable dependency.
    AttachIndexConstraint {
        name: Option<&'a str>,
        index: QualName<'a>,
        primary: bool,
        timing: ConstraintTiming,
    },
    /// ALTER TABLE ... DROP CONSTRAINT [IF EXISTS] name.
    DropConstraint {
        name: &'a str,
        if_exists: bool,
        cascade: bool,
    },
    /// ALTER TABLE ... RENAME CONSTRAINT old TO new.
    RenameConstraint {
        from: &'a str,
        to: &'a str,
    },
    AlterConstraint {
        name: &'a str,
        alteration: ConstraintAlteration,
    },
    ValidateConstraint(&'a str),
    /// ALTER TABLE trigger-state command with a parser-classified target.
    SetTriggerEnabled {
        target: TriggerEnableTarget<'a>,
        enabled: TriggerEnableMode,
    },
    SetRowLevelSecurity(RowLevelSecurityAlteration),
    SetPersistence(RelationPersistence),
    SetStorageOptions(RelationStorageOptions),
    ResetStorageOptions(RelationStorageOptionNames),
    SetInheritance {
        parent: QualName<'a>,
        inherit: bool,
    },
    /// ALTER TABLE ... SET TABLESPACE name.
    SetTablespace(&'a str),
    /// ALTER TABLE ... SET ACCESS METHOD heap.
    SetAccessMethod(TableAccessMethod<'a>),
    /// ALTER TABLE ... REPLICA IDENTITY controls the old tuple emitted for
    /// logical UPDATE and DELETE messages.
    SetReplicaIdentity(ReplicaIdentityTarget<'a>),
    AttachPartition {
        child: QualName<'a>,
        bound: PartitionBound<'a>,
    },
    DetachPartition {
        child: QualName<'a>,
        mode: PartitionDetachMode,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionDetachMode {
    Immediate,
    Concurrent,
    Finalize,
}

/// The closed SQL forms of `ALTER TABLE ... REPLICA IDENTITY`. An index target
/// remains a qualified relation name until catalog resolution proves it is a
/// usable index of the altered table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaIdentityTarget<'a> {
    Default,
    Full,
    Nothing,
    Index(QualName<'a>),
}

/// The independently optional attributes of `ALTER CONSTRAINT`. Execution
/// combines them with the existing typed definition before accepting the new
/// state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConstraintAlteration {
    pub deferrable: Option<bool>,
    pub initially: Option<ConstraintMode>,
    pub enforced: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerEnableMode {
    Origin,
    Replica,
    Always,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventTriggerEvent {
    Login,
    DdlCommandStart,
    DdlCommandEnd,
    SqlDrop,
    TableRewrite,
}

impl EventTriggerEvent {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::DdlCommandStart => "ddl_command_start",
            Self::DdlCommandEnd => "ddl_command_end",
            Self::SqlDrop => "sql_drop",
            Self::TableRewrite => "table_rewrite",
        }
    }

    pub const fn supports_tag_filter(self) -> bool {
        matches!(
            self,
            Self::DdlCommandStart | Self::DdlCommandEnd | Self::SqlDrop
        )
    }

    pub const fn code(self) -> u8 {
        match self {
            Self::Login => 0,
            Self::DdlCommandStart => 1,
            Self::DdlCommandEnd => 2,
            Self::SqlDrop => 3,
            Self::TableRewrite => 4,
        }
    }

    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Login),
            1 => Some(Self::DdlCommandStart),
            2 => Some(Self::DdlCommandEnd),
            3 => Some(Self::SqlDrop),
            4 => Some(Self::TableRewrite),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateEventTrigger<'a> {
    pub name: &'a str,
    pub event: EventTriggerEvent,
    pub tags: &'a [&'a str],
    pub function: QualName<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterEventTriggerAction<'a> {
    SetEnabled(TriggerEnableMode),
    SetOwner(&'a str),
    Rename(&'a str),
}

/// `ALL` and `USER` are command selectors, not trigger names. Keeping them
/// distinct preserves quoted names such as `"all"` at the parse boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerEnableTarget<'a> {
    Name(&'a str),
    All,
    User,
}

/// ALTER TABLE's independently durable row-security flags. ENABLE controls
/// whether policies apply; FORCE controls whether the table owner is subject
/// to them. Keeping the four commands distinct prevents one flag from being
/// inferred from the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowLevelSecurityAlteration {
    Enable,
    Disable,
    Force,
    NoForce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyPermissiveness {
    Permissive,
    Restrictive,
}

/// PostgreSQL role specifications accepted by a policy TO clause. PUBLIC is
/// a real all-roles state, not a role name; the three dynamic spellings are
/// resolved to catalog identities when the DDL executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyRole<'a> {
    Public,
    CurrentRole,
    CurrentUser,
    SessionUser,
    Named(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PolicyExpression<'a> {
    pub expression: &'a Expr<'a>,
    pub source: &'a str,
}

/// The legal expression shape for each CREATE POLICY command. This enum is
/// the parse boundary: invalid USING/WITH CHECK combinations cannot reach the
/// catalog as a bag of optional fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PolicyCommand<'a> {
    All {
        using: Option<PolicyExpression<'a>>,
        with_check: Option<PolicyExpression<'a>>,
    },
    Select {
        using: Option<PolicyExpression<'a>>,
    },
    Insert {
        with_check: Option<PolicyExpression<'a>>,
    },
    Update {
        using: Option<PolicyExpression<'a>>,
        with_check: Option<PolicyExpression<'a>>,
    },
    Delete {
        using: Option<PolicyExpression<'a>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreatePolicy<'a> {
    pub name: &'a str,
    pub table: QualName<'a>,
    pub permissiveness: PolicyPermissiveness,
    pub roles: &'a [PolicyRole<'a>],
    pub command: PolicyCommand<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyIdentity<'a> {
    pub name: &'a str,
    pub table: QualName<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlterPolicy<'a> {
    pub identity: PolicyIdentity<'a>,
    /// None means retain the role list; Some(empty) is impossible because the
    /// parser requires at least one role after TO.
    pub roles: Option<&'a [PolicyRole<'a>]>,
    pub using: Option<PolicyExpression<'a>>,
    pub with_check: Option<PolicyExpression<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatisticsName<'a> {
    Generated,
    Explicit {
        name: QualName<'a>,
        if_not_exists: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatisticsKinds(u8);

impl StatisticsKinds {
    const NDISTINCT: u8 = 1;
    const DEPENDENCIES: u8 = 2;
    const MCV: u8 = 4;

    pub const ALL: Self = Self(Self::NDISTINCT | Self::DEPENDENCIES | Self::MCV);
    pub const EXPRESSION: Self = Self(0);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub fn insert_ndistinct(&mut self) -> bool {
        self.insert(Self::NDISTINCT)
    }

    pub fn insert_dependencies(&mut self) -> bool {
        self.insert(Self::DEPENDENCIES)
    }

    pub fn insert_mcv(&mut self) -> bool {
        self.insert(Self::MCV)
    }

    fn insert(&mut self, kind: u8) -> bool {
        let fresh = self.0 & kind == 0;
        self.0 |= kind;
        fresh
    }

    pub const fn ndistinct(self) -> bool {
        self.0 & Self::NDISTINCT != 0
    }

    pub const fn dependencies(self) -> bool {
        self.0 & Self::DEPENDENCIES != 0
    }

    pub const fn mcv(self) -> bool {
        self.0 & Self::MCV != 0
    }

    pub const fn code(self) -> u8 {
        self.0
    }

    pub const fn from_code(code: u8) -> Option<Self> {
        if code != 0 && code & !Self::ALL.0 == 0 {
            Some(Self(code))
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StatisticsExpression<'a> {
    pub expression: &'a Expr<'a>,
    pub source: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StatisticsKey<'a> {
    Column(&'a str),
    Expression(StatisticsExpression<'a>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StatisticsKeys<'a> {
    Expression(StatisticsExpression<'a>),
    Multivariate {
        kinds: StatisticsKinds,
        keys: &'a [StatisticsKey<'a>],
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreateStatistics<'a> {
    pub name: StatisticsName<'a>,
    pub keys: StatisticsKeys<'a>,
    pub table: QualName<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatisticsTarget {
    Default,
    Value(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterStatisticsAction<'a> {
    Owner(&'a str),
    Rename(&'a str),
    SetSchema(&'a str),
    SetTarget(StatisticsTarget),
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
    /// A named SQL-routine argument. Column lookup has precedence; `index`
    /// supplies the positional value only when this spelling binds no column.
    RoutineParam {
        qualifier: Option<&'a str>,
        name: &'a str,
        index: u32,
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
    /// An explicit collation that remains part of expression identity until
    /// the comparison, ordering, or key path consumes it.
    Collate {
        operand: &'a Expr<'a>,
        collation: ParsedCollation<'a>,
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
        /// Empty for positional calls; otherwise parallel to `args` and
        /// populated only for named notation parsed after positional inputs.
        argument_names: &'a [Option<&'a str>],
        /// The final argument was introduced by the `VARIADIC` keyword and is
        /// already the declared array, rather than an element to pack.
        variadic: bool,
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
    /// `operand operator ANY/ALL (SELECT ...)` for operators other than the
    /// `IN`/`NOT IN` spellings represented above.
    QuantifiedSubquery {
        operand: &'a Expr<'a>,
        operator: BinaryOp,
        select: &'a Select<'a>,
        all: bool,
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
    Subscript {
        base: &'a Expr<'a>,
        index: &'a Expr<'a>,
    },
    /// Array slice `base[lower:upper]`; either bound may be omitted (`base[:2]`,
    /// `base[2:]`, `base[:]`), defaulting to the array's first / last element.
    Slice {
        base: &'a Expr<'a>,
        lower: Option<&'a Expr<'a>>,
        upper: Option<&'a Expr<'a>>,
    },
    /// `(base).field` composite field access. Used by driver introspection with
    /// the `_pg_expandarray` set function, whose result exposes `.x` (element)
    /// and `.n` (1-based ordinal).
    Field {
        base: &'a Expr<'a>,
        field: &'a str,
    },
    /// `t.*` in an expression position: the table's typed composite row.
    WholeRow(&'a str),
    /// A three-part column reference `schema.table.column`: the qualifier
    /// pair must match an unaliased FROM entry that really is that schema's
    /// table (PostgreSQL's rule), then resolves like `table.column`.
    SchemaColumn {
        schema: &'a str,
        table: &'a str,
        name: &'a str,
    },
    /// Typed executor state carried after a recursive CTE row's visible
    /// columns. The SQL grammar cannot construct this node; SEARCH/CYCLE
    /// rewriting uses it to retain parent paths without exposing hidden
    /// columns through `*`.
    RecursiveState {
        qualifier: &'a str,
        index: u8,
        ctype: ColType,
    },
    /// `operand operator ANY/ALL (array)` — quantified comparison.
    AnyAll {
        operand: &'a Expr<'a>,
        operator: BinaryOp,
        array: &'a Expr<'a>,
        all: bool,
    },
}

fn is_volatile_function(name: &str) -> bool {
    const NAMES: &[&str] = &[
        "clock_timestamp",
        "timeofday",
        "random",
        "random_normal",
        "setseed",
        "nextval",
        "currval",
        "lastval",
        "setval",
        "gen_random_uuid",
        "uuid_generate_v1",
        "uuid_generate_v4",
        "txid_current",
        "pg_current_xact_id",
        "pg_is_in_recovery",
        "pg_reload_conf",
        "set_config",
    ];
    NAMES
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
        || matches!(
            name,
            "lo_create"
                | "lo_import"
                | "lo_export"
                | "lo_open"
                | "lo_close"
                | "loread"
                | "lowrite"
                | "lo_lseek"
                | "lo_creat"
                | "lo_tell"
                | "lo_unlink"
                | "lo_truncate"
                | "lo_lseek64"
                | "lo_tell64"
                | "lo_truncate64"
                | "lo_from_bytea"
                | "lo_get"
                | "lo_put"
        )
}

fn is_nonimmutable_function(name: &str) -> bool {
    const STABLE_NAMES: &[&str] = &[
        "now",
        "current_timestamp",
        "current_date",
        "current_time",
        "localtime",
        "localtimestamp",
        "statement_timestamp",
        "transaction_timestamp",
        "current_user",
        "session_user",
        "user",
        "current_role",
        "current_schema",
        "current_database",
        "current_catalog",
        "pg_backend_pid",
        "current_setting",
    ];
    is_volatile_function(name)
        || STABLE_NAMES
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
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
        fn is_set_returning(name: &str) -> bool {
            super::query::is_builtin_set_routine(name)
        }
        match self {
            Expr::Null
            | Expr::Bool(_)
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::NumericLit(_)
            | Expr::Str(_)
            | Expr::BitLit(_) => true,
            Expr::WholeRow(_) | Expr::SchemaColumn { .. } => false,
            Expr::Column { .. }
            | Expr::RecursiveState { .. }
            | Expr::RoutineParam { .. }
            | Expr::Param(_)
            | Expr::Subquery(_)
            | Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery { .. }
            | Expr::Exists(_)
            | Expr::ArraySubquery(_)
            | Expr::DefaultMarker => false,
            // Catalog-defined casts need the query catalog for both their
            // validation and their runtime representation. They are not
            // foldable through the catalog-free evaluator.
            Expr::Cast { type_name, .. }
                if type_name.eq_ignore_ascii_case("regclass")
                    || type_name.eq_ignore_ascii_case("regtype")
                    || crate::sql::types::ColType::from_sql_name(type_name).is_none() =>
            {
                false
            }
            Expr::Unary { operand, .. }
            | Expr::Cast { operand, .. }
            | Expr::Collate { operand, .. }
            | Expr::IsNull { operand, .. } => operand.is_constant(),
            Expr::Binary { left, right, .. } => left.is_constant() && right.is_constant(),
            Expr::InList { operand, list, .. } => {
                operand.is_constant() && list.iter().all(|e| e.is_constant())
            }
            Expr::Between {
                operand, low, high, ..
            } => operand.is_constant() && low.is_constant() && high.is_constant(),
            Expr::Like {
                operand, pattern, ..
            }
            | Expr::Match {
                operand, pattern, ..
            } => operand.is_constant() && pattern.is_constant(),
            Expr::Case {
                operand,
                whens,
                otherwise,
                ..
            } => {
                operand.map(|o| o.is_constant()).unwrap_or(true)
                    && whens
                        .iter()
                        .all(|(c, r)| c.is_constant() && r.is_constant())
                    && otherwise.map(|e| e.is_constant()).unwrap_or(true)
            }
            // Aggregates, windows, set-returning functions, and non-immutable
            // calls are never constants. In particular, probing a volatile
            // call for plan-time errors would itself change session state.
            Expr::Call {
                name, args, over, ..
            } => {
                over.is_none()
                    && !self.is_aggregate()
                    && !is_set_returning(name)
                    && !is_nonimmutable_function(name)
                    && args.iter().all(|a| a.is_constant())
            }
            Expr::Array(items) => items.iter().all(|e| e.is_constant()),
            Expr::Subscript { base, index } => base.is_constant() && index.is_constant(),
            Expr::Slice { base, lower, upper } => {
                base.is_constant()
                    && lower.is_none_or(|e| e.is_constant())
                    && upper.is_none_or(|e| e.is_constant())
            }
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
            | Expr::RecursiveState { .. }
            | Expr::Param(_)
            | Expr::DefaultMarker => false,
            Expr::Unary { operand, .. }
            | Expr::Cast { operand, .. }
            | Expr::Collate { operand, .. }
            | Expr::IsNull { operand, .. }
            | Expr::Field { base: operand, .. } => operand.contains_call(),
            Expr::Slice { base, lower, upper } => {
                base.contains_call()
                    || lower.is_some_and(|e| e.contains_call())
                    || upper.is_some_and(|e| e.contains_call())
            }
            Expr::Binary { left, right, .. }
            | Expr::Subscript {
                base: left,
                index: right,
            }
            | Expr::AnyAll {
                operand: left,
                array: right,
                ..
            } => left.contains_call() || right.contains_call(),
            Expr::InList { operand, list, .. } => {
                operand.contains_call() || list.iter().any(|e| e.contains_call())
            }
            Expr::Between {
                operand, low, high, ..
            } => operand.contains_call() || low.contains_call() || high.contains_call(),
            Expr::Like {
                operand, pattern, ..
            }
            | Expr::Match {
                operand, pattern, ..
            } => operand.contains_call() || pattern.contains_call(),
            Expr::Case {
                operand,
                whens,
                otherwise,
                ..
            } => {
                operand.map(|o| o.contains_call()).unwrap_or(false)
                    || whens
                        .iter()
                        .any(|(c, r)| c.contains_call() || r.contains_call())
                    || otherwise.map(|o| o.contains_call()).unwrap_or(false)
            }
            Expr::Array(items) => items.iter().any(|e| e.contains_call()),
            // A subquery-bearing default is rejected elsewhere; treat it as
            // non-foldable to be safe.
            Expr::Subquery(_)
            | Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery { .. }
            | Expr::Exists(_)
            | Expr::ArraySubquery(_) => true,
        }
    }

    /// Whether the expression tree contains a subquery — disallowed in a column
    /// generation expression (0A000).
    pub fn contains_subquery(&self) -> bool {
        match self {
            Expr::Subquery(_)
            | Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery { .. }
            | Expr::Exists(_)
            | Expr::ArraySubquery(_) => true,
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
            | Expr::RecursiveState { .. }
            | Expr::Param(_)
            | Expr::DefaultMarker => false,
            Expr::Unary { operand, .. }
            | Expr::Cast { operand, .. }
            | Expr::Collate { operand, .. }
            | Expr::IsNull { operand, .. }
            | Expr::Field { base: operand, .. } => operand.contains_subquery(),
            Expr::Slice { base, lower, upper } => {
                base.contains_subquery()
                    || lower.is_some_and(|e| e.contains_subquery())
                    || upper.is_some_and(|e| e.contains_subquery())
            }
            Expr::Binary { left, right, .. }
            | Expr::Subscript {
                base: left,
                index: right,
            }
            | Expr::AnyAll {
                operand: left,
                array: right,
                ..
            } => left.contains_subquery() || right.contains_subquery(),
            Expr::Call { args, .. } => args.iter().any(|a| a.contains_subquery()),
            Expr::InList { operand, list, .. } => {
                operand.contains_subquery() || list.iter().any(|e| e.contains_subquery())
            }
            Expr::Between {
                operand, low, high, ..
            } => operand.contains_subquery() || low.contains_subquery() || high.contains_subquery(),
            Expr::Like {
                operand, pattern, ..
            }
            | Expr::Match {
                operand, pattern, ..
            } => operand.contains_subquery() || pattern.contains_subquery(),
            Expr::Case {
                operand,
                whens,
                otherwise,
                ..
            } => {
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
        self.find_function(is_nonimmutable_function)
    }

    /// The volatile subset relevant to PostgreSQL's CTE inlining rule.
    pub fn contains_volatile_function(&self) -> Option<&str> {
        self.find_function(is_volatile_function)
    }

    fn find_function(&self, matches: fn(&str) -> bool) -> Option<&str> {
        match self {
            Expr::Call { name, args, .. } => {
                if matches(name) {
                    return Some(name);
                }
                args.iter().find_map(|a| a.find_function(matches))
            }
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
            | Expr::RecursiveState { .. }
            | Expr::Param(_)
            | Expr::DefaultMarker
            | Expr::Subquery(_)
            | Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery { .. }
            | Expr::Exists(_)
            | Expr::ArraySubquery(_) => None,
            Expr::Unary { operand, .. }
            | Expr::Cast { operand, .. }
            | Expr::Collate { operand, .. }
            | Expr::IsNull { operand, .. }
            | Expr::Field { base: operand, .. } => operand.find_function(matches),
            Expr::Slice { base, lower, upper } => base
                .find_function(matches)
                .or_else(|| lower.and_then(|e| e.find_function(matches)))
                .or_else(|| upper.and_then(|e| e.find_function(matches))),
            Expr::Binary { left, right, .. }
            | Expr::Subscript {
                base: left,
                index: right,
            }
            | Expr::AnyAll {
                operand: left,
                array: right,
                ..
            } => left
                .find_function(matches)
                .or_else(|| right.find_function(matches)),
            Expr::InList { operand, list, .. } => operand
                .find_function(matches)
                .or_else(|| list.iter().find_map(|e| e.find_function(matches))),
            Expr::Between {
                operand, low, high, ..
            } => operand
                .find_function(matches)
                .or_else(|| low.find_function(matches))
                .or_else(|| high.find_function(matches)),
            Expr::Like {
                operand, pattern, ..
            }
            | Expr::Match {
                operand, pattern, ..
            } => operand
                .find_function(matches)
                .or_else(|| pattern.find_function(matches)),
            Expr::Case {
                operand,
                whens,
                otherwise,
                ..
            } => operand
                .and_then(|o| o.find_function(matches))
                .or_else(|| {
                    whens.iter().find_map(|(c, r)| {
                        c.find_function(matches)
                            .or_else(|| r.find_function(matches))
                    })
                })
                .or_else(|| otherwise.and_then(|o| o.find_function(matches))),
            Expr::Array(items) => items.iter().find_map(|e| e.find_function(matches)),
        }
    }

    /// Visits every column reference in the tree (by unqualified name), for
    /// validating a generation expression's dependencies.
    pub fn for_each_column(&self, f: &mut dyn FnMut(&str)) {
        match self {
            Expr::Column { name, .. } | Expr::RoutineParam { name, .. } => f(name),
            Expr::Null
            | Expr::Bool(_)
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::NumericLit(_)
            | Expr::Str(_)
            | Expr::BitLit(_)
            | Expr::WholeRow(_)
            | Expr::SchemaColumn { .. }
            | Expr::RecursiveState { .. }
            | Expr::Param(_)
            | Expr::DefaultMarker
            | Expr::Subquery(_)
            | Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery { .. }
            | Expr::Exists(_)
            | Expr::ArraySubquery(_) => {}
            Expr::Unary { operand, .. }
            | Expr::Cast { operand, .. }
            | Expr::Collate { operand, .. }
            | Expr::IsNull { operand, .. }
            | Expr::Field { base: operand, .. } => operand.for_each_column(f),
            Expr::Slice { base, lower, upper } => {
                base.for_each_column(f);
                if let Some(e) = lower {
                    e.for_each_column(f);
                }
                if let Some(e) = upper {
                    e.for_each_column(f);
                }
            }
            Expr::Binary { left, right, .. }
            | Expr::Subscript {
                base: left,
                index: right,
            }
            | Expr::AnyAll {
                operand: left,
                array: right,
                ..
            } => {
                left.for_each_column(f);
                right.for_each_column(f);
            }
            Expr::Call { args, .. } => args.iter().for_each(|a| a.for_each_column(f)),
            Expr::InList { operand, list, .. } => {
                operand.for_each_column(f);
                list.iter().for_each(|e| e.for_each_column(f));
            }
            Expr::Between {
                operand, low, high, ..
            } => {
                operand.for_each_column(f);
                low.for_each_column(f);
                high.for_each_column(f);
            }
            Expr::Like {
                operand, pattern, ..
            }
            | Expr::Match {
                operand, pattern, ..
            } => {
                operand.for_each_column(f);
                pattern.for_each_column(f);
            }
            Expr::Case {
                operand,
                whens,
                otherwise,
                ..
            } => {
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

    /// Visits column references while preserving an optional table qualifier.
    /// Subqueries own their bindings and are visited by their enclosing query.
    pub fn for_each_column_reference(&self, f: &mut dyn FnMut(Option<&str>, &str)) {
        match self {
            Expr::Column { qualifier, name } => f(*qualifier, name),
            Expr::Null
            | Expr::Bool(_)
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::NumericLit(_)
            | Expr::Str(_)
            | Expr::BitLit(_)
            | Expr::WholeRow(_)
            | Expr::SchemaColumn { .. }
            | Expr::RecursiveState { .. }
            | Expr::Param(_)
            | Expr::RoutineParam { .. }
            | Expr::DefaultMarker
            | Expr::Subquery(_)
            | Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery { .. }
            | Expr::Exists(_)
            | Expr::ArraySubquery(_) => {}
            Expr::Unary { operand, .. }
            | Expr::Cast { operand, .. }
            | Expr::Collate { operand, .. }
            | Expr::IsNull { operand, .. }
            | Expr::Field { base: operand, .. } => operand.for_each_column_reference(f),
            Expr::Slice { base, lower, upper } => {
                base.for_each_column_reference(f);
                if let Some(expression) = lower {
                    expression.for_each_column_reference(f);
                }
                if let Some(expression) = upper {
                    expression.for_each_column_reference(f);
                }
            }
            Expr::Binary { left, right, .. }
            | Expr::Subscript {
                base: left,
                index: right,
            }
            | Expr::AnyAll {
                operand: left,
                array: right,
                ..
            } => {
                left.for_each_column_reference(f);
                right.for_each_column_reference(f);
            }
            Expr::Call { args, .. } => args.iter().for_each(|argument| {
                argument.for_each_column_reference(f);
            }),
            Expr::InList { operand, list, .. } => {
                operand.for_each_column_reference(f);
                list.iter()
                    .for_each(|expression| expression.for_each_column_reference(f));
            }
            Expr::Between {
                operand, low, high, ..
            } => {
                operand.for_each_column_reference(f);
                low.for_each_column_reference(f);
                high.for_each_column_reference(f);
            }
            Expr::Like {
                operand, pattern, ..
            }
            | Expr::Match {
                operand, pattern, ..
            } => {
                operand.for_each_column_reference(f);
                pattern.for_each_column_reference(f);
            }
            Expr::Case {
                operand,
                whens,
                otherwise,
                ..
            } => {
                if let Some(expression) = operand {
                    expression.for_each_column_reference(f);
                }
                for (condition, result) in *whens {
                    condition.for_each_column_reference(f);
                    result.for_each_column_reference(f);
                }
                if let Some(expression) = otherwise {
                    expression.for_each_column_reference(f);
                }
            }
            Expr::Array(items) => items.iter().for_each(|expression| {
                expression.for_each_column_reference(f);
            }),
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
    /// `!! tsquery`, the text-search NOT operator.
    TextSearchNot,
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
    /// `@@` and its PostgreSQL-compatible `@@@` alias.
    TextSearchMatch,
    /// `<->` between two tsquery values.
    TextSearchPhrase,
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
    pub(crate) const fn from_operator_name(name: &str) -> Option<Self> {
        Some(match name.as_bytes() {
            b"+" => Self::Add,
            b"-" => Self::Sub,
            b"*" => Self::Mul,
            b"/" => Self::Div,
            b"%" => Self::Mod,
            b"=" => Self::Eq,
            b"<>" | b"!=" => Self::NotEq,
            b"<" => Self::Lt,
            b"<=" => Self::LtEq,
            b">" => Self::Gt,
            b">=" => Self::GtEq,
            b"||" => Self::Concat,
            b"@@" | b"@@@" => Self::TextSearchMatch,
            b"<->" => Self::TextSearchPhrase,
            b"->" => Self::JsonGet,
            b"->>" => Self::JsonGetText,
            b"#>" => Self::JsonPath,
            b"#>>" => Self::JsonPathText,
            b"#-" => Self::JsonDeletePath,
            b"?" => Self::JsonExists,
            b"?|" => Self::JsonExistsAny,
            b"?&" => Self::JsonExistsAll,
            b"&" => Self::BitAnd,
            b"|" => Self::BitOr,
            b"#" => Self::BitXor,
            b"<<" => Self::Shl,
            b">>" => Self::Shr,
            b"^" => Self::Pow,
            b"@>" => Self::Contains,
            b"<@" => Self::ContainedBy,
            b"&&" => Self::Overlaps,
            b"&<" => Self::NotRightOf,
            b"&>" => Self::NotLeftOf,
            b"-|-" => Self::Adjacent,
            b"<<=" => Self::NetContainedEq,
            b">>=" => Self::NetContainsEq,
            _ => return None,
        })
    }

    pub(crate) const fn operator_name(self) -> Option<&'static str> {
        Some(match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Mod => "%",
            Self::Eq => "=",
            Self::NotEq => "<>",
            Self::Lt => "<",
            Self::LtEq => "<=",
            Self::Gt => ">",
            Self::GtEq => ">=",
            Self::Concat => "||",
            Self::TextSearchMatch => "@@",
            Self::TextSearchPhrase => "<->",
            Self::JsonGet => "->",
            Self::JsonGetText => "->>",
            Self::JsonPath => "#>",
            Self::JsonPathText => "#>>",
            Self::JsonDeletePath => "#-",
            Self::JsonExists => "?",
            Self::JsonExistsAny => "?|",
            Self::JsonExistsAll => "?&",
            Self::BitAnd => "&",
            Self::BitOr => "|",
            Self::BitXor => "#",
            Self::Shl => "<<",
            Self::Shr => ">>",
            Self::Pow => "^",
            Self::Contains => "@>",
            Self::ContainedBy => "<@",
            Self::Overlaps => "&&",
            Self::NotRightOf => "&<",
            Self::NotLeftOf => "&>",
            Self::Adjacent => concat!("-", "|", "-"),
            Self::NetContainedEq => "<<=",
            Self::NetContainsEq => ">>=",
            Self::And | Self::Or | Self::Like | Self::ILike => return None,
        })
    }

    /// Binding power for the Pratt parser; higher binds tighter.
    /// Mirrors PostgreSQL's operator precedence table.
    pub fn precedence(self) -> u8 {
        match self {
            Self::Or => 1,
            Self::And => 2,
            Self::Eq | Self::NotEq | Self::Lt | Self::LtEq | Self::Gt | Self::GtEq => 4,
            // Containment/overlap/adjacency operators bind like comparisons.
            Self::Contains | Self::ContainedBy | Self::Overlaps => 4,
            Self::TextSearchMatch => 4,
            Self::NotRightOf | Self::NotLeftOf | Self::Adjacent => 4,
            Self::NetContainedEq | Self::NetContainsEq => 4,
            Self::Like | Self::ILike => 4,
            Self::JsonExists | Self::JsonExistsAny | Self::JsonExistsAll => 4,
            Self::Concat => 5,
            Self::TextSearchPhrase => 5,
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
            Self::JsonGet
            | Self::JsonGetText
            | Self::JsonPath
            | Self::JsonPathText
            | Self::JsonDeletePath => 9,
        }
    }
}
