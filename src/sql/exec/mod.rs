//! Statement execution against table storage.
//!
//! Scans decode rows from the memtable heap into stack arrays; ORDER BY
//! materializes sort keys into the per-statement arena (bounded by the
//! arena size, loudly). No allocation anywhere.

use super::txn::TxnState;
use crate::mem::arena::Arena;
use crate::mem::fixed_vec::FixedVec;
use crate::pg::respond::Responder;
use crate::pg::wire::WireFull;
use crate::sql_err;
use crate::stack_format;
use crate::storage::rowenc;
use crate::storage::{
    ColumnMeta, MAX_COLUMNS, MAX_ROUTINE_ARGUMENTS, ROUTINE_SQL_MAX, RoutineArgumentDef,
    RoutineIdentity, RoutineSpec, RowHome, SeqSpec, SeqType, SqlName, Storage, TableDef,
};
use crate::util::StackStr;
use crate::wal::{Wal, WalOp};

use super::ast::{
    AlterAction, AlterTable, CreateRoutine, CreateTable, Delete, DropTable, Expr, Insert,
    LikeClause, Overriding, QualName, SelectItem, Update,
};
use super::eval::{
    ColumnLookup, EvalHooks, NO_HOOKS, NoColumns, SqlError, cast_to, compare_datums, eval,
    eval_full, sqlstate,
};
use super::types::{ColDesc, ColType, Datum, TypeMod};

/// Wildcard expansion can double the select list.
pub const MAX_PROJ: usize = MAX_COLUMNS * 2;

/// Column resolution over one decoded row. The datum lifetime `'v` (heap /
/// arena bytes) is independent of the borrow of the value slice itself, so
/// looked-up datums may outlive the decode buffer.
pub struct RowCtx<'s, 'v, 'd> {
    pub def: &'d TableDef,
    pub values: &'s [Datum<'v>],
    pub alias: Option<&'d str>,
}

impl<'v> ColumnLookup<'v> for RowCtx<'_, 'v, '_> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'v>, SqlError> {
        if let Some(q) = qualifier
            && !crate::sql::eval::qualifier_answers_target(self.def, self.alias, q)
        {
            return Err(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "missing FROM-clause entry for table \"{}\"",
                q
            ));
        }
        match self.def.column_index(name) {
            Some(i) => Ok(self.values[i]),
            None => Err(sql_err!(
                sqlstate::UNDEFINED_COLUMN,
                "column \"{}\" does not exist",
                name
            )),
        }
    }

    fn whole_row_fields(
        &self,
        table: &str,
        arena: &'v Arena,
    ) -> Result<Option<&'v [super::types::RecordField<'v>]>, SqlError> {
        if !crate::sql::eval::qualifier_answers_target(self.def, self.alias, table) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "missing FROM-clause entry for table \"{}\"",
                table
            ));
        }
        let cols = self.def.columns();
        let mut fields = [super::types::RecordField {
            name: "",
            type_oid: 0,
            value: Datum::Null,
        }; MAX_COLUMNS];
        let too_large = || sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "record exceeds the arena");
        for (i, field) in fields.iter_mut().enumerate().take(self.def.n_columns) {
            field.name = arena
                .alloc_str(cols[i].name.as_str())
                .map_err(|_| too_large())?;
            field.type_oid = cols[i].ctype.oid();
            field.value = self.values.get(i).copied().unwrap_or(Datum::Null);
        }
        let out = arena
            .alloc_slice_copy(&fields[..self.def.n_columns])
            .map_err(|_| too_large())?;
        Ok(Some(&*out))
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        if let Some(q) = qualifier
            && !crate::sql::eval::qualifier_answers_target(self.def, self.alias, q)
        {
            return None;
        }
        self.def
            .column_index(name)
            .map(|i| self.def.columns()[i].ctype)
    }

    fn column_domain(&self, qualifier: Option<&str>, name: &str) -> Option<SqlName> {
        if let Some(q) = qualifier
            && !crate::sql::eval::qualifier_answers_target(self.def, self.alias, q)
        {
            return None;
        }
        self.def.column_index(name).and_then(|i| {
            self.def.columns()[i]
                .user_type
                .map(|identity| identity.name)
        })
    }
}

type Outcome = Result<Result<(), SqlError>, WireFull>;

fn sql_ok() -> Outcome {
    Ok(Ok(()))
}

fn sql_fail(e: SqlError) -> Outcome {
    Ok(Err(e))
}

mod describe;
pub(crate) use describe::AliasedDefCols;
pub(crate) use describe::enter_routine_parameter_types;
pub use describe::{
    ColTypeResolver, DefCols, NoCols, RECORD_FIELD_NAMES, check_row_field_types,
    could_not_identify, derived_name, describe_items, expr_record_handle as expr_record_handle_pub,
    infer_type_pub, infer_type_res, init_record_shapes, not_composite, record_field_type,
    record_shape, register_shape_for, reset_record_shapes, typeof_static, typeof_static_coltype,
    typeof_static_oid, visit_record_shape as visit_record_shape_pub,
};
pub(crate) use describe::{coltype_of_oid, json_each_value_type_pub, unify_numeric_tower};

mod projected;
pub(crate) use projected::{
    compare_projected_prefix, encode_projected_by, encode_projected_by_into, encode_projected_into,
    projected_row_len_by, projected_row_width,
};
pub use projected::{
    decode_projected_col_record, decode_projected_pub, decode_projected_value,
    encode_projected_pub, projected_prefix_len, projected_value_len, sort_dedup_projected,
};

mod ddl;
pub(crate) use ddl::check_referenced_columns;
use ddl::{add_unique_key, attach_constraints, auto_key_name, build_column, build_def};

mod constraints;
pub(crate) use constraints::coerce_domain_value;
use constraints::{
    MAX_FK_CASCADE_DEPTH, ParsedChecks, apply_fk_parent_actions, check_index_tuple_size,
    enforce_row_constraints, parse_checks, parse_defaults, parse_generated, referenced_key_changed,
    table_is_referenced,
};
pub use constraints::{check_all_unique, check_unique, check_unique_indexes};

#[derive(Clone, Copy)]
struct OwnedSequencePlan {
    schema: SqlName,
    name: SqlName,
    spec: SeqSpec,
    owner: crate::storage::SequenceOwner,
}

fn default_owned_sequence_name(table: &str, column: &str) -> Result<SqlName, SqlError> {
    use core::fmt::Write as _;
    let mut name = crate::util::StackStr::<64>::new();
    let _ = write!(name, "{}_{}_seq", table, column);
    if name.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "generated sequence name is too long"
        ));
    }
    SqlName::parse(name.as_str())
}

fn owned_sequence_plan(
    def: &TableDef,
    column_index: usize,
    identity: Option<crate::sql::ast::IdentitySpec<'_>>,
) -> Result<OwnedSequencePlan, SqlError> {
    let column = def.columns()[column_index];
    let mut options = identity.map_or(crate::sql::ast::SeqOptions::EMPTY, |i| i.options);
    options.data_type = Some(match column.ctype {
        ColType::Int2 => "smallint",
        ColType::Int8 => "bigint",
        _ => "integer",
    });
    options.owned_by = None;
    let (spec, _) = resolve_seq_spec(&options, None)?;
    let (schema, name) = match identity.and_then(|i| i.sequence_name) {
        Some(name) => {
            if let Some(schema) = name.schema
                && schema != def.schema.as_str()
            {
                return Err(sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "sequence must be in same schema as table it is linked to"
                ));
            }
            (def.schema, SqlName::parse(name.name)?)
        }
        None => (
            def.schema,
            default_owned_sequence_name(def.name.as_str(), column.name.as_str())?,
        ),
    };
    Ok(OwnedSequencePlan {
        schema,
        name,
        spec,
        owner: crate::storage::SequenceOwner {
            table_schema: def.schema,
            table: def.name,
            column: column.name,
        },
    })
}

fn create_owned_sequence(
    storage: &mut Storage,
    wal: &mut Wal,
    plan: OwnedSequencePlan,
    txid: u32,
) -> Result<usize, SqlError> {
    if storage.relation_name_taken(plan.schema.as_str(), plan.name.as_str(), txid) {
        return Err(sql_err!(
            sqlstate::DUPLICATE_TABLE,
            "relation \"{}\" already exists",
            plan.name.as_str()
        ));
    }
    let slot = storage.create_sequence(
        plan.schema,
        plan.name,
        plan.spec,
        Some(plan.owner),
        Some(plan.owner),
        txid,
    )?;
    let lsn = storage.bump_lsn();
    if let Err(error) = wal.stage(
        txid,
        lsn,
        &WalOp::CreateSequence {
            schema: plan.schema.as_str(),
            name: plan.name.as_str(),
            data_type: plan.spec.data_type.to_u8(),
            increment: plan.spec.increment,
            min_value: plan.spec.min_value,
            max_value: plan.spec.max_value,
            start_value: plan.spec.start_value,
            cache: plan.spec.cache,
            cycle: plan.spec.cycle,
            owner: Some(plan.owner),
            generator_for: Some(plan.owner),
        },
    ) {
        storage.rollback_sequence_create(slot);
        return Err(error);
    }
    Ok(slot)
}

pub fn create_table(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    statement: &CreateTable,
    arena: &Arena,
    responder: &mut Responder,
) -> Outcome {
    let mut def = match build_def_with_likes(storage, statement, txn.txid, arena) {
        Ok(d) => d,
        Err(e) => return sql_fail(e),
    };
    def.schema = match storage.creation_schema(statement.name.schema, statement.name.name, txn.txid)
    {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    // A copied constraint lands before the explicitly written ones, so a
    // duplicate primary key is caught with PostgreSQL's own message.
    if let Err(e) = copy_like_constraints(storage, &mut def, statement, txn.txid) {
        return sql_fail(e);
    }
    if let Err(e) = reject_multiple_primary(&def) {
        return sql_fail(e);
    }
    if let Err(e) = attach_constraints(storage, &mut def, statement.constraints, txn.txid, arena) {
        return sql_fail(e);
    }
    match storage.create_table_in(def, txn.txid) {
        Ok(slot) => {
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.stage(txn.txid, lsn, &WalOp::CreateTable(def)) {
                // Nothing reached the journal; undo the in-memory apply.
                storage.rollback_create(slot);
                return sql_fail(e);
            }
            if let Err(e) = txn.record_ddl(super::txn::DdlUndo::Created(slot as u32)) {
                storage.rollback_create(slot);
                return sql_fail(e);
            }
            if let Err(error) = apply_default_privileges_to_new_object(
                storage,
                txn,
                crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Table,
                    slot: slot as u16,
                },
            ) {
                return sql_fail(error);
            }
            for c in statement.columns {
                let Some(index) = def.column_index(c.name) else {
                    continue;
                };
                if !def.columns()[index].auto_increment {
                    continue;
                }
                let plan = match owned_sequence_plan(&def, index, c.identity) {
                    Ok(plan) => plan,
                    Err(error) => return sql_fail(error),
                };
                let sequence_slot = match create_owned_sequence(storage, wal, plan, txn.txid) {
                    Ok(sequence_slot) => sequence_slot,
                    Err(error) => return sql_fail(error),
                };
                if let Err(error) =
                    txn.record_ddl(super::txn::DdlUndo::SequenceCreated(sequence_slot as u32))
                {
                    storage.rollback_sequence_create(sequence_slot);
                    return sql_fail(error);
                }
                if let Err(error) = apply_default_privileges_to_new_object(
                    storage,
                    txn,
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::Sequence,
                        slot: sequence_slot as u16,
                    },
                ) {
                    return sql_fail(error);
                }
            }
        }
        Err(e) if e.sqlstate == sqlstate::DUPLICATE_TABLE && statement.if_not_exists => {
            responder.notice(
                crate::sql::eval::sqlstate::DUPLICATE_TABLE,
                stack_format!(
                    128,
                    "relation \"{}\" already exists, skipping",
                    statement.name.name
                )
                .as_str(),
            )?;
        }
        Err(e) => return sql_fail(e),
    }
    if let Err(e) = copy_like_indexes(storage, wal, txn, statement, &def) {
        return sql_fail(e);
    }
    responder.command_complete("CREATE TABLE")?;
    sql_ok()
}

/// A table gets one primary key. A column-level `PRIMARY KEY` sets the column's
/// flag directly and never reaches [`attach_constraints`], so two of them — or
/// one alongside a key copied by `LIKE ... INCLUDING INDEXES` — is only caught
/// by counting the assembled definition.
fn reject_multiple_primary(def: &TableDef) -> Result<(), SqlError> {
    let declared = def.columns().iter().filter(|c| c.primary).count()
        + def.uniques[..def.n_uniques]
            .iter()
            .filter(|k| k.is_primary)
            .count();
    if declared > 1 {
        return Err(sql_err!(
            crate::sql::eval::sqlstate::INVALID_TABLE_DEFINITION,
            "multiple primary keys for table \"{}\" are not allowed",
            def.name.as_str()
        ));
    }
    Ok(())
}

/// The source table of a `LIKE`, or PostgreSQL's undefined-table error.
fn like_source<'s>(
    storage: &'s Storage,
    like: &LikeClause,
    txid: u32,
) -> Result<&'s TableDef, SqlError> {
    match resolve_dml_table(storage, &like.source, txid) {
        Ok(i) => Ok(storage.table_def(i, txid)),
        Err(e) => Err(e),
    }
}

/// [`build_def`] with each `LIKE source` element's columns spliced in where it
/// was written. A copied column always keeps its name, type and NOT NULL; the
/// rest of its properties follow the element's `INCLUDING` flags.
fn build_def_with_likes(
    storage: &Storage,
    statement: &CreateTable,
    txid: u32,
    arena: &Arena,
) -> Result<TableDef, SqlError> {
    if statement.likes.is_empty() {
        return build_def(statement.name.name, statement.columns, storage, txid, arena);
    }
    let mut def = TableDef {
        name: SqlName::parse(statement.name.name)?,
        ..TableDef::empty()
    };
    let mut n = 0usize;
    for position in 0..=statement.columns.len() {
        for like in statement.likes.iter().filter(|l| l.at == position) {
            let source = like_source(storage, like, txid)?;
            for column in source.columns() {
                let mut copied = *column;
                if !like.defaults && !copied.default.is_generated() {
                    copied.default = crate::storage::ColumnDefault::NONE;
                }
                if !like.indexes {
                    copied.unique = false;
                    copied.primary = false;
                }
                if !like.identity {
                    copied.auto_increment = false;
                }
                // Without INCLUDING GENERATED a generated column is copied as a
                // plain column (its generation expression is dropped).
                if !like.generated && copied.default.is_generated() {
                    copied.default = crate::storage::ColumnDefault::NONE;
                }
                push_column(&mut def, &mut n, copied)?;
            }
        }
        if let Some(column) = statement.columns.get(position) {
            push_column(
                &mut def,
                &mut n,
                build_column(column, storage, txid, arena)?,
            )?;
        }
    }
    def.n_columns = n;
    Ok(def)
}

/// Appends one column, rejecting a name already taken.
fn push_column(def: &mut TableDef, n: &mut usize, column: ColumnMeta) -> Result<(), SqlError> {
    if *n == MAX_COLUMNS {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "tables can have at most {} columns",
            MAX_COLUMNS
        ));
    }
    if def.columns[..*n]
        .iter()
        .any(|prev| prev.name == column.name)
    {
        return Err(sql_err!(
            sqlstate::DUPLICATE_COLUMN,
            "column \"{}\" specified more than once",
            column.name.as_str()
        ));
    }
    def.columns[*n] = column;
    *n += 1;
    Ok(())
}

/// Copies the CHECK constraints and multi-column keys of each `LIKE` source
/// that asked for them. Foreign keys are never copied, as in PostgreSQL.
fn copy_like_constraints(
    storage: &Storage,
    def: &mut TableDef,
    statement: &CreateTable,
    txid: u32,
) -> Result<(), SqlError> {
    for like in statement.likes {
        let source = like_source(storage, like, txid)?;
        if like.constraints {
            for check in &source.checks[..source.n_checks] {
                if def.n_checks == crate::storage::MAX_CHECKS {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "a table can have at most {} CHECK constraints",
                        crate::storage::MAX_CHECKS
                    ));
                }
                let mut copied = *check;
                // The name is regenerated from the new table, as PostgreSQL
                // does, so the two tables' constraints stay distinguishable.
                copied.name = auto_key_name(def, &[], "check", false)?;
                def.checks[def.n_checks] = copied;
                def.n_checks += 1;
            }
        }
        if like.indexes {
            for key in &source.uniques[..source.n_uniques] {
                let columns = remap_columns(def, source, &key.columns[..key.n_cols])?;
                add_unique_key(
                    def,
                    None,
                    if key.is_primary { "pkey" } else { "key" },
                    &columns,
                    key.n_cols,
                    key.is_primary,
                )?;
            }
        }
    }
    Ok(())
}

/// Maps column positions in `source` onto the new table, which may have shifted
/// them by preceding columns.
fn remap_columns(
    def: &TableDef,
    source: &TableDef,
    columns: &[u16],
) -> Result<[u16; crate::storage::MAX_INDEX_COLS], SqlError> {
    let mut out = [0u16; crate::storage::MAX_INDEX_COLS];
    for (slot, &c) in out.iter_mut().zip(columns) {
        let name = source.columns()[c as usize].name.as_str();
        match def.column_index(name) {
            Some(i) => *slot = i as u16,
            None => {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    name
                ));
            }
        }
    }
    Ok(out)
}

/// One source index, captured before the mutable borrow that creates its copy.
#[derive(Clone, Copy)]
struct CopiedIndex {
    columns: [u16; crate::storage::MAX_INDEX_COLS],
    expressions: [Option<crate::util::StackStr<{ crate::storage::INDEX_EXPRESSION_MAX }>>;
        crate::storage::MAX_INDEX_COLS],
    include_columns: [u16; crate::storage::MAX_INDEX_COLS],
    descending: [bool; crate::storage::MAX_INDEX_COLS],
    nulls_first: [bool; crate::storage::MAX_INDEX_COLS],
    n_cols: usize,
    n_include_cols: usize,
    nulls_not_distinct: bool,
    predicate: Option<crate::util::StackStr<{ crate::storage::INDEX_PREDICATE_MAX }>>,
    unique: bool,
}

/// Recreates each `LIKE` source's secondary indexes on the new table. It has no
/// rows yet, so the uniqueness scan [`create_index`] performs is unnecessary.
fn copy_like_indexes(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    statement: &CreateTable,
    def: &TableDef,
) -> Result<(), SqlError> {
    use crate::storage::IndexDef;
    for like in statement.likes.iter().filter(|l| l.indexes) {
        // Collected up front: creating one needs `storage` mutably.
        let mut copied = [CopiedIndex {
            columns: [0; crate::storage::MAX_INDEX_COLS],
            expressions: [None; crate::storage::MAX_INDEX_COLS],
            include_columns: [0; crate::storage::MAX_INDEX_COLS],
            descending: [false; crate::storage::MAX_INDEX_COLS],
            nulls_first: [false; crate::storage::MAX_INDEX_COLS],
            n_cols: 0,
            n_include_cols: 0,
            nulls_not_distinct: false,
            predicate: None,
            unique: false,
        }; MAX_LIKE_INDEXES];
        let mut n_copied = 0;
        let source_def = *storage.table_def(
            resolve_dml_table(storage, &like.source, txn.txid)?,
            txn.txid,
        );
        for index in storage.indexes_for(
            source_def.schema.as_str(),
            source_def.name.as_str(),
            txn.txid,
        ) {
            if n_copied == MAX_LIKE_INDEXES {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "cannot copy more than {} indexes",
                    MAX_LIKE_INDEXES
                ));
            }
            copied[n_copied] = CopiedIndex {
                columns: index.columns,
                expressions: index.expressions,
                include_columns: index.include_columns,
                descending: index.descending,
                nulls_first: index.nulls_first,
                n_cols: index.n_cols,
                n_include_cols: index.n_include_cols,
                nulls_not_distinct: index.nulls_not_distinct,
                predicate: index.predicate,
                unique: index.unique,
            };
            n_copied += 1;
        }
        let source = source_def;
        for index in &copied[..n_copied] {
            let columns = remap_columns(def, &source, &index.columns[..index.n_cols])?;
            let include_columns =
                remap_columns(def, &source, &index.include_columns[..index.n_include_cols])?;
            let name = auto_key_name(def, &columns[..index.n_cols], "idx", true)?;
            let slot = storage.create_index(
                IndexDef {
                    schema: def.schema,
                    name,
                    pending_name: None,
                    table: def.name,
                    ownership: crate::storage::Ownership::BOOTSTRAP,
                    columns,
                    expressions: index.expressions,
                    include_columns,
                    descending: index.descending,
                    nulls_first: index.nulls_first,
                    n_cols: index.n_cols,
                    n_include_cols: index.n_include_cols,
                    nulls_not_distinct: index.nulls_not_distinct,
                    predicate: index.predicate,
                    unique: index.unique,
                    ddl_state: crate::storage::CatalogDdlState::Present,
                },
                txn.txid,
            )?;
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.stage(
                txn.txid,
                lsn,
                &WalOp::CreateIndex {
                    schema: def.schema.as_str(),
                    name: name.as_str(),
                    table: def.name.as_str(),
                    columns,
                    expressions: index
                        .expressions
                        .each_ref()
                        .map(|expression| expression.as_ref().map(|text| text.as_str())),
                    include_columns,
                    descending: index.descending,
                    nulls_first: index.nulls_first,
                    n_cols: index.n_cols,
                    n_include_cols: index.n_include_cols,
                    nulls_not_distinct: index.nulls_not_distinct,
                    predicate: index.predicate.as_ref().map(|text| text.as_str()),
                    unique: index.unique,
                },
            ) {
                storage.rollback_index_create(slot);
                return Err(e);
            }
            txn.record_ddl(super::txn::DdlUndo::IndexCreated(slot as u32))?;
        }
    }
    Ok(())
}

/// Upper bound on the secondary indexes one `LIKE ... INCLUDING INDEXES` copies.
const MAX_LIKE_INDEXES: usize = 8;

/// The next value of a serial/identity column: a real sequence, as PostgreSQL
/// has it. Explicit inserts do not advance it, deletes and TRUNCATE do not
/// rewind it, and the advance survives a rollback (a consumed number stays
/// consumed). The counter is journaled at commit and floored against the
/// stored rows at startup.
fn next_auto_value<'x>(
    storage: &mut Storage,
    table_index: usize,
    col: usize,
    ctype: ColType,
    seq_session: &crate::sql::guc::SeqSession,
    txid: u32,
) -> Result<Datum<'x>, SqlError> {
    let def = *storage.table_def(table_index, txid);
    let column = def.columns()[col];
    if let Some(slot) = storage.generated_sequence_slot(
        def.schema.as_str(),
        def.name.as_str(),
        column.name.as_str(),
        txid,
    ) {
        let role = storage.current_role_slot(txid).ok_or_else(|| {
            sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "current role is not present in the role catalog"
            )
        })?;
        let object = crate::storage::AccessObject {
            class: crate::storage::AccessClass::Sequence,
            slot: slot as u16,
        };
        if !storage.has_object_privilege(object, role, crate::storage::PrivilegeSet::USAGE, txid)
            && !storage.has_object_privilege(
                object,
                role,
                crate::storage::PrivilegeSet::UPDATE,
                txid,
            )
        {
            return Err(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied for sequence {}",
                storage.sequence_for(slot, txid).name.as_str()
            ));
        }
        let sequence = storage.sequence_for(slot, txid);
        let next = storage.next_sequence_value(slot, txid)?;
        seq_session.record_nextval(slot, sequence.created_at, next);
        return match ctype {
            ColType::Int8 => Ok(Datum::Int8(next)),
            ColType::Int2 => i16::try_from(next)
                .map(Datum::Int2)
                .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "smallint out of range")),
            _ => i32::try_from(next)
                .map(Datum::Int4)
                .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "integer out of range")),
        };
    }
    let step = def.columns()[col].auto_increment_step;
    let table = storage.table_mut(table_index);
    let next = table.serial_last[col] + step;
    let bound_error =
        |what: &'static str| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "{} out of range", what);
    let out = match ctype {
        ColType::Int8 => Datum::Int8(next),
        ColType::Int2 => Datum::Int2(i16::try_from(next).map_err(|_| bound_error("smallint"))?),
        _ => Datum::Int4(i32::try_from(next).map_err(|_| bound_error("integer"))?),
    };
    table.serial_last[col] = next;
    table.serial_dirty = true;
    Ok(out)
}

/// The unique constraint or index an `ON CONFLICT` clause arbitrates on.
/// `Any` (no target, DO NOTHING) treats a violation of *any* unique constraint
/// as the conflict; `Columns` restricts the conflict to rows equal on exactly
/// this column set, so a violation of a *different* unique falls through to a
/// normal 23505 — matching PostgreSQL, which uses the arbiter index alone.
enum Arbiter<'a> {
    Any,
    Keys {
        columns: [u16; crate::storage::MAX_INDEX_COLS],
        expressions: [Option<&'a Expr<'a>>; crate::storage::MAX_INDEX_COLS],
        n_columns: usize,
        nulls_not_distinct: bool,
    },
}

fn same_index_expression(stored: &str, target: &str) -> bool {
    let target = target.trim();
    stored == target
        || (target.starts_with('(')
            && target.ends_with(')')
            && stored == target[1..target.len() - 1].trim())
}

#[derive(Clone, Copy)]
struct PartialArbiter<'a> {
    columns: [u16; crate::storage::MAX_INDEX_COLS],
    expressions: [Option<&'a Expr<'a>>; crate::storage::MAX_INDEX_COLS],
    n_columns: usize,
    nulls_not_distinct: bool,
    predicate: Option<&'a Expr<'a>>,
}

/// Does some unique constraint/index on `def` cover exactly `want` (as a set,
/// order-independent)? Validates an `ON CONFLICT (columns)` inference target.
fn unique_arbiter_matches(
    storage: &Storage,
    def: &TableDef,
    want: &[u16],
    txid: u32,
) -> Option<bool> {
    let same = |cols: &[u16]| cols.len() == want.len() && want.iter().all(|w| cols.contains(w));
    for (i, c) in def.columns().iter().enumerate() {
        if c.unique && same(&[i as u16]) {
            return Some(false);
        }
    }
    for uk in def.uniques() {
        if same(uk.columns()) {
            return Some(false);
        }
    }
    let mut matched = false;
    for index in storage.unique_indexes_for(def.schema.as_str(), def.name.as_str(), txid) {
        if index.predicate.is_none() && same(&index.columns[..index.n_cols]) {
            matched = true;
            if index.nulls_not_distinct {
                return Some(true);
            }
        }
    }
    matched.then_some(false)
}

/// Resolves `ON CONSTRAINT name` to the named arbiter's column set: a named
/// UNIQUE/PRIMARY KEY, a unique index, or a single-column key's synthesized
/// name (`<table>_pkey` / `<table>_<col>_key`).
fn arbiter_by_name(
    storage: &Storage,
    def: &TableDef,
    name: &str,
    txid: u32,
) -> Option<([u16; crate::storage::MAX_INDEX_COLS], usize, bool)> {
    let mut cols = [0u16; crate::storage::MAX_INDEX_COLS];
    for uk in def.uniques() {
        if uk.name.as_str() == name {
            let n = uk.n_cols;
            cols[..n].copy_from_slice(uk.columns());
            return Some((cols, n, false));
        }
    }
    for index in storage.unique_indexes_for(def.schema.as_str(), def.name.as_str(), txid) {
        if index.predicate.is_none() && index.name_for(txid).as_str() == name {
            let n = index.n_cols;
            cols[..n].copy_from_slice(&index.columns[..n]);
            return Some((cols, n, index.nulls_not_distinct));
        }
    }
    for (i, c) in def.columns().iter().enumerate() {
        let synth = if c.primary {
            ddl::auto_key_name(def, &[i as u16], "pkey", false)
        } else if c.unique {
            ddl::auto_key_name(def, &[i as u16], "key", true)
        } else {
            continue;
        };
        if synth.map(|nm| nm.as_str() == name).unwrap_or(false) {
            cols[0] = i as u16;
            return Some((cols, 1, false));
        }
    }
    None
}

/// Resolves an `ON CONFLICT` clause's arbiter, raising PostgreSQL's analysis
/// errors (a data-independent step: it runs even when no row conflicts).
fn resolve_arbiter<'a>(
    storage: &Storage,
    def: &TableDef,
    oc: &super::ast::OnConflict<'a>,
    txid: u32,
) -> Result<Arbiter<'a>, SqlError> {
    if !oc.target.is_empty() {
        if oc.target.iter().any(|target| target.column.is_none()) {
            for index in storage.unique_indexes_for(def.schema.as_str(), def.name.as_str(), txid) {
                if index.predicate.is_some() || index.n_cols != oc.target.len() {
                    continue;
                }
                let mut expressions = [None; crate::storage::MAX_INDEX_COLS];
                let matches = oc.target.iter().enumerate().all(|(position, target)| {
                    expressions[position] = target.column.map_or(Some(target.expression), |_| None);
                    match (target.column, index.expressions[position]) {
                        (Some(column), None) => {
                            def.column_index(column) == Some(index.columns[position] as usize)
                        }
                        (None, Some(source)) => {
                            same_index_expression(source.as_str(), target.expression_text)
                        }
                        _ => false,
                    }
                });
                if matches {
                    return Ok(Arbiter::Keys {
                        columns: index.columns,
                        expressions,
                        n_columns: index.n_cols,
                        nulls_not_distinct: index.nulls_not_distinct,
                    });
                }
            }
            return Err(sql_err!(
                sqlstate::INVALID_COLUMN_REFERENCE,
                "there is no unique or exclusion constraint matching the ON CONFLICT specification"
            ));
        }
        let mut want = [0u16; crate::storage::MAX_INDEX_COLS];
        let mut n = 0;
        let mut expressions = [None; crate::storage::MAX_INDEX_COLS];
        for target in oc.target {
            let Some(name) = target.column else {
                return Err(sql_err!(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    "there is no unique or exclusion constraint matching the ON CONFLICT specification"
                ));
            };
            let Some(index) = def.column_index(name) else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    name
                ));
            };
            if n == crate::storage::MAX_INDEX_COLS {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "ON CONFLICT target has too many columns"
                ));
            }
            want[n] = index as u16;
            expressions[n] = None;
            n += 1;
        }
        if let Some(nulls_not_distinct) = unique_arbiter_matches(storage, def, &want[..n], txid) {
            return Ok(Arbiter::Keys {
                columns: want,
                expressions,
                n_columns: n,
                nulls_not_distinct,
            });
        }
        return Err(sql_err!(
            sqlstate::INVALID_COLUMN_REFERENCE,
            "there is no unique or exclusion constraint matching the ON CONFLICT specification"
        ));
    }
    if let Some(cname) = oc.constraint {
        if let Some((cols, n, nulls_not_distinct)) = arbiter_by_name(storage, def, cname, txid) {
            return Ok(Arbiter::Keys {
                columns: cols,
                expressions: [None; crate::storage::MAX_INDEX_COLS],
                n_columns: n,
                nulls_not_distinct,
            });
        }
        return Err(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "constraint \"{}\" for table \"{}\" does not exist",
            cname,
            def.name.as_str()
        ));
    }
    if oc.update.is_some() {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "ON CONFLICT DO UPDATE requires inference specification or constraint name"
        ));
    }
    Ok(Arbiter::Any)
}

/// Finds an existing visible row that conflicts with the candidate on the
/// resolved [`Arbiter`] — the row `ON CONFLICT` acts on. NULLs are distinct, so
/// a candidate with a NULL key column never conflicts.
#[allow(clippy::too_many_arguments)]
fn find_conflict<'a>(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    arbiter: &Arbiter<'a>,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<u64>, SqlError> {
    let partial_count = storage
        .unique_indexes_for(def.schema.as_str(), def.name.as_str(), txid)
        .filter(|index| {
            index.predicate.is_some()
                || index.expressions[..index.n_cols]
                    .iter()
                    .any(Option::is_some)
        })
        .count();
    let partial = arena
        .alloc_slice_with(partial_count, |_| None::<PartialArbiter<'a>>)
        .map_err(|_| crate::sql::eval::arena_full())?;
    let mut next_partial = 0usize;
    for index in storage.unique_indexes_for(def.schema.as_str(), def.name.as_str(), txid) {
        if index.predicate.is_none()
            && index.expressions[..index.n_cols]
                .iter()
                .all(Option::is_none)
        {
            continue;
        }
        let mut columns = [0u16; crate::storage::MAX_INDEX_COLS];
        columns[..index.n_cols].copy_from_slice(&index.columns[..index.n_cols]);
        let mut expressions = [None; crate::storage::MAX_INDEX_COLS];
        for (position, source) in index.expressions.iter().enumerate().take(index.n_cols) {
            if let Some(source) = source {
                let source = arena
                    .alloc_str(source.as_str())
                    .map_err(|_| crate::sql::eval::arena_full())?;
                expressions[position] = Some(crate::sql::parser::parse_expr(source, arena)?);
            }
        }
        let predicate = match index.predicate {
            Some(source) => {
                let source = arena
                    .alloc_str(source.as_str())
                    .map_err(|_| crate::sql::eval::arena_full())?;
                Some(crate::sql::parser::parse_expr(source, arena)?)
            }
            None => None,
        };
        partial[next_partial] = Some(PartialArbiter {
            columns,
            expressions,
            n_columns: index.n_cols,
            nulls_not_distinct: index.nulls_not_distinct,
            predicate,
        });
        next_partial += 1;
    }
    let mut found: Option<u64> = None;
    storage.for_each_row_state(table_index, &mut |rowid, state| {
        use core::ops::ControlFlow;
        let Some(home) = storage.visible_row_home(table_index, rowid, state, txid)? else {
            return Ok(ControlFlow::Continue(()));
        };
        let hit = storage.with_row_bytes(table_index, rowid, home, |bytes| {
            let mut other = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(bytes, schema, &mut other)?;
            let eq = |a: &Datum, b: &Datum| {
                if a.is_null() || b.is_null() {
                    Ok(false)
                } else {
                    compare_datums(a, b).map(|ordering| ordering.is_eq())
                }
            };
            let key_hit = |cols: &[u16], nulls_not_distinct: bool| {
                for &column in cols {
                    if !eq(&values[column as usize], &other[column as usize])? {
                        if nulls_not_distinct
                            && values[column as usize].is_null()
                            && other[column as usize].is_null()
                        {
                            continue;
                        }
                        return Ok(false);
                    }
                }
                Ok(true)
            };
            match arbiter {
                // A named/inferred arbiter conflicts on its own columns only.
                Arbiter::Keys {
                    columns,
                    expressions,
                    n_columns,
                    nulls_not_distinct,
                } => {
                    if expressions[..*n_columns].iter().all(Option::is_none) {
                        key_hit(&columns[..*n_columns], *nulls_not_distinct)
                    } else {
                        let keys = crate::sql::exec::constraints::index_key_values(
                            def,
                            values,
                            &columns[..*n_columns],
                            &expressions[..*n_columns],
                            arena,
                        )?;
                        let other_keys = crate::sql::exec::constraints::index_key_values(
                            def,
                            &other,
                            &columns[..*n_columns],
                            &expressions[..*n_columns],
                            arena,
                        )?;
                        crate::sql::exec::constraints::key_values_equal(
                            &keys[..*n_columns],
                            &other_keys[..*n_columns],
                            *nulls_not_distinct,
                        )
                    }
                }
                // No target (DO NOTHING): any unique violation is a conflict.
                Arbiter::Any => {
                    for (i, c) in def.columns().iter().enumerate() {
                        if c.unique && eq(&values[i], &other[i])? {
                            return Ok(true);
                        }
                    }
                    for uk in def.uniques() {
                        if key_hit(uk.columns(), false)? {
                            return Ok(true);
                        }
                    }
                    for index in
                        storage.unique_indexes_for(def.schema.as_str(), def.name.as_str(), txid)
                    {
                        if index.predicate.is_none()
                            && index.expressions[..index.n_cols]
                                .iter()
                                .all(Option::is_none)
                            && key_hit(&index.columns[..index.n_cols], index.nulls_not_distinct)?
                        {
                            return Ok(true);
                        }
                    }
                    for partial_index in partial.iter().flatten() {
                        let candidate_member =
                            partial_index.predicate.map_or(Ok(true), |predicate| {
                                crate::sql::exec::constraints::index_predicate_matches(
                                    def, values, predicate, arena,
                                )
                            })?;
                        let other_member =
                            partial_index.predicate.map_or(Ok(true), |predicate| {
                                crate::sql::exec::constraints::index_predicate_matches(
                                    def, &other, predicate, arena,
                                )
                            })?;
                        let candidate_keys = crate::sql::exec::constraints::index_key_values(
                            def,
                            values,
                            &partial_index.columns[..partial_index.n_columns],
                            &partial_index.expressions[..partial_index.n_columns],
                            arena,
                        )?;
                        let other_keys = crate::sql::exec::constraints::index_key_values(
                            def,
                            &other,
                            &partial_index.columns[..partial_index.n_columns],
                            &partial_index.expressions[..partial_index.n_columns],
                            arena,
                        )?;
                        if candidate_member
                            && other_member
                            && crate::sql::exec::constraints::key_values_equal(
                                &candidate_keys[..partial_index.n_columns],
                                &other_keys[..partial_index.n_columns],
                                partial_index.nulls_not_distinct,
                            )?
                        {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }
            }
        })?;
        if hit {
            found = Some(rowid);
            return Ok(ControlFlow::Break(()));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(found)
}

/// Column lookup for ON CONFLICT DO UPDATE: `excluded.<col>` resolves to the
/// row proposed by INSERT; every other reference resolves to the existing
/// (conflicting) row.
struct ExcludedCtx<'s, 'v, 'd> {
    def: &'d TableDef,
    existing: &'s [Datum<'v>],
    excluded: &'s [Datum<'v>],
}

impl<'v> ColumnLookup<'v> for ExcludedCtx<'_, 'v, '_> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'v>, SqlError> {
        let src = if qualifier == Some("excluded") {
            self.excluded
        } else {
            if let Some(q) = qualifier
                && !crate::sql::eval::qualifier_answers_single(self.def, q)
            {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "missing FROM-clause entry for table \"{}\"",
                    q
                ));
            }
            self.existing
        };
        match self.def.column_index(name) {
            Some(i) => Ok(src[i]),
            None => Err(sql_err!(
                sqlstate::UNDEFINED_COLUMN,
                "column \"{}\" does not exist",
                name
            )),
        }
    }

    fn col_type(&self, _qualifier: Option<&str>, name: &str) -> Option<ColType> {
        self.def
            .column_index(name)
            .map(|i| self.def.columns()[i].ctype)
    }
}

enum ConflictOutcome<'a> {
    Store,
    Skip,
    /// DO UPDATE applied; carries the arena-encoded updated row so RETURNING can
    /// project the post-update values (PostgreSQL returns the updated row).
    Updated(&'a [u8]),
}

/// Applies an ON CONFLICT clause to one candidate row, against the arbiter
/// already resolved once for the statement.
#[allow(clippy::too_many_arguments)]
fn handle_conflict<'a>(
    storage: &mut Storage,
    txn: &mut TxnState,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    on_conflict: &Option<super::ast::OnConflict>,
    arbiter: &Arbiter,
    checks: &ParsedChecks,
    arena: &'a Arena,
    params: &[Datum],
) -> Result<ConflictOutcome<'a>, SqlError> {
    let Some(oc) = on_conflict else {
        return Ok(ConflictOutcome::Store);
    };
    let Some(rowid) = find_conflict(
        storage,
        table_index,
        def,
        schema,
        values,
        arbiter,
        txn.txid,
        arena,
    )?
    else {
        return Ok(ConflictOutcome::Store);
    };
    let Some(assigns) = oc.update else {
        return Ok(ConflictOutcome::Skip); // DO NOTHING
    };
    // DO UPDATE: recompute the conflicting row, `excluded` = the proposed row.
    let new_bytes = {
        let mut existing = [Datum::Null; MAX_COLUMNS];
        let state = *storage
            .table(table_index)
            .rows
            .get(&rowid)
            .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "conflict row vanished"))?;
        let home = storage
            .visible_row_home(table_index, rowid, state, txn.txid)?
            .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "conflict row vanished"))?;
        let bytes = storage.row_bytes(table_index, rowid, home, arena)?;
        rowenc::decode(bytes, schema, &mut existing)?;
        let context = ExcludedCtx {
            def,
            existing: &existing[..def.n_columns],
            excluded: values,
        };
        let mut subquery_expressions: [Option<&Expr>; MAX_PROJ] = [None; MAX_PROJ];
        let mut subquery_expression_count = 0usize;
        if let Some(condition) = oc.update_where {
            subquery_expressions[subquery_expression_count] = Some(condition);
            subquery_expression_count += 1;
        }
        for (_, expression) in assigns {
            subquery_expressions[subquery_expression_count] = Some(expression);
            subquery_expression_count += 1;
        }
        let subqueries = super::query::subquery_hooks(
            &subquery_expressions[..subquery_expression_count],
            storage,
            txn.txid,
            arena,
            params,
        )?;
        let hooks = EvalHooks {
            subs: Some(&subqueries),
            ..NO_HOOKS
        };
        if let Some(cond) = oc.update_where
            && !matches!(
                eval_full(cond, arena, params, &context, &hooks)?,
                Datum::Bool(true)
            )
        {
            return Ok(ConflictOutcome::Skip); // WHERE excluded this row
        }
        let mut new_values = [Datum::Null; MAX_COLUMNS];
        new_values[..def.n_columns].copy_from_slice(&existing[..def.n_columns]);
        for (name, expression) in assigns {
            let Some(target) = def.column_index(name) else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" of relation \"{}\" does not exist",
                    name,
                    def.name.as_str()
                ));
            };
            let v = eval_full(expression, arena, params, &context, &hooks)?;
            new_values[target] = coerce(v, &def.columns()[target], storage, txn.txid, arena)?;
        }
        check_not_null(def, &new_values)?;
        enforce_row_constraints(
            storage,
            table_index,
            def,
            schema,
            &new_values[..def.n_columns],
            Some(rowid),
            txn.txid,
            checks,
            arena,
            params,
        )?;
        let len = rowenc::encoded_len(&new_values[..def.n_columns]);
        let out = arena.alloc_slice_with(len, |_| 0u8).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "updated row exceeds the arena"
            )
        })?;
        rowenc::encode(&new_values[..def.n_columns], out);
        &*out
    };
    let (new_loc, slice) = storage.heap.append(new_bytes.len())?;
    slice.copy_from_slice(new_bytes);
    let prior = storage.write_pending(
        table_index,
        rowid,
        txn.txid,
        txn.command_id(),
        Some(new_loc),
    )?;
    if let Err(e) = txn.touch(table_index as u32, rowid, prior) {
        storage.restore_pending(table_index, rowid, txn.txid, prior);
        return Err(e);
    }
    Ok(ConflictOutcome::Updated(new_bytes))
}

/// Assigns each omitted/defaulted auto-increment column its next value. A
/// column the statement set *explicitly* — even to NULL — is left alone, so
/// an explicit NULL falls through to the not-null check exactly as PostgreSQL
/// rejects it, and an explicit value never advances the sequence.
fn fill_auto_increment(
    storage: &mut Storage,
    table_index: usize,
    def: &TableDef,
    values: &mut [Datum],
    explicit: &[bool; MAX_COLUMNS],
    seq_session: &crate::sql::guc::SeqSession,
    txid: u32,
) -> Result<(), SqlError> {
    if !def.columns().iter().any(|c| c.auto_increment) {
        return Ok(());
    }
    for i in 0..def.n_columns {
        let col = &def.columns()[i];
        if col.auto_increment && !explicit[i] && values[i].is_null() {
            values[i] = next_auto_value(storage, table_index, i, col.ctype, seq_session, txid)?;
        }
    }
    Ok(())
}

/// PostgreSQL names the kind of object a DROP could not find — `table "x" does
/// not exist`, not `relation` — while every other lookup says relation.
fn undefined_kind(kind: &str, name: &str) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_TABLE,
        "{} \"{}\" does not exist",
        kind,
        name
    )
}

pub fn drop_table(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    statement: &DropTable,
    responder: &mut Responder,
) -> Outcome {
    let mut selected_tables = [usize::MAX; 16];
    let mut selected_table_count = 0usize;
    for name in statement.names {
        if let Some(crate::storage::ResolvedRelation::Table(slot)) =
            storage.resolve_relation(name.schema, name.name, txn.txid)
            && storage
                .matview_slot(
                    storage.table_def(slot, txn.txid).schema.as_str(),
                    storage.table_def(slot, txn.txid).name.as_str(),
                    txn.txid,
                )
                .is_none()
            && !selected_tables[..selected_table_count].contains(&slot)
        {
            if selected_table_count == selected_tables.len() {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many tables in one DROP statement"
                ));
            }
            selected_tables[selected_table_count] = slot;
            selected_table_count += 1;
        }
    }
    for name in statement.names {
        // A DROP whose qualifier names no schema is PostgreSQL's 3F000 (a
        // SELECT of the same spelling is 42P01 — the codes really differ).
        if let Some(schema) = name.schema
            && storage.find_schema_visible(schema, txn.txid).is_none()
            && !statement.if_exists
        {
            return sql_fail(sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "schema \"{}\" does not exist",
                schema
            ));
        }
        // A sequence is a relation, but not a table: DROP TABLE on it is a type
        // error (42809), which IF EXISTS does not suppress.
        if storage
            .sequence_on_path(name.schema, name.name, txn.txid)
            .is_some()
        {
            return sql_fail(sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "\"{}\" is not a table",
                name.name
            ));
        }
        match storage.resolve_relation(name.schema, name.name, txn.txid) {
            Some(crate::storage::ResolvedRelation::Table(index))
                if storage
                    .matview_slot(
                        storage.table_def(index, txn.txid).schema.as_str(),
                        storage.table_def(index, txn.txid).name.as_str(),
                        txn.txid,
                    )
                    .is_some() =>
            {
                // The backing table of a materialized view is not an ordinary
                // table; PostgreSQL refuses DROP TABLE on it (42809), directing
                // the user to DROP MATERIALIZED VIEW.
                return sql_fail(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "\"{}\" is not a table",
                    name.name
                ));
            }
            Some(crate::storage::ResolvedRelation::Table(index)) => {
                if let Err(error) = storage.require_owner(
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::Table,
                        slot: index as u16,
                    },
                    txn.txid,
                    "table",
                ) {
                    return sql_fail(error);
                }
                if let Err(error) = storage.lock_table(
                    txn.txid,
                    index,
                    crate::sql::ast::TableLockMode::AccessExclusive,
                    false,
                ) {
                    return sql_fail(error);
                }
                if let Some(other) = storage.table(index).ddl_locked_by_other(txn.txid) {
                    if let Err(error) = storage.wait_for_transaction(txn.txid, other) {
                        return sql_fail(error);
                    }
                    return sql_fail(sql_err!(
                        crate::sql::eval::sqlstate::INTERNAL_LOCK_WAIT,
                        "statement is waiting for concurrent DDL on \"{}\"",
                        name.name
                    ));
                }
                let def = *storage.table_def(index, txn.txid);
                let root = |dependency: &crate::storage::StoredQueryDependency| {
                    dependency.class == crate::storage::DependencyClass::Table
                        && dependency.slot as usize == index
                };
                let closure = stored_query_dependent_closure(storage, txn.txid, root);
                let (dependent_views, dependent_matviews) = match closure {
                    Ok(closure) => closure,
                    Err(error) => return sql_fail(error),
                };
                let has_dependents = dependent_views.iter().any(|selected| *selected)
                    || dependent_matviews.iter().any(|selected| *selected);
                if has_dependents && !statement.cascade {
                    if let Err(error) = report_stored_query_dependents(
                        storage,
                        txn.txid,
                        StoredQueryRoot {
                            class: crate::storage::DependencyClass::Table,
                            slot: index,
                            kind: "table",
                            schema: def.schema,
                            name: def.name,
                        },
                        StoredQuerySelection {
                            views: &dependent_views,
                            matviews: &dependent_matviews,
                        },
                        false,
                        responder,
                    ) {
                        return sql_fail(error);
                    }
                    return sql_fail(sql_err!(
                        sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                        "cannot drop table {} because other objects depend on it",
                        def.name.as_str()
                    ));
                }
                if statement.cascade {
                    if let Err(error) = report_stored_query_dependents(
                        storage,
                        txn.txid,
                        StoredQueryRoot {
                            class: crate::storage::DependencyClass::Table,
                            slot: index,
                            kind: "table",
                            schema: def.schema,
                            name: def.name,
                        },
                        StoredQuerySelection {
                            views: &dependent_views,
                            matviews: &dependent_matviews,
                        },
                        true,
                        responder,
                    ) {
                        return sql_fail(error);
                    }
                    if let Err(error) = drop_selected_stored_queries(
                        storage,
                        wal,
                        txn,
                        &dependent_views,
                        &dependent_matviews,
                    ) {
                        return sql_fail(error);
                    }
                }
                loop {
                    let inbound = (0..storage.table_count()).find_map(|child| {
                        if !storage.table(child).visible_to(txn.txid)
                            || selected_tables[..selected_table_count].contains(&child)
                        {
                            return None;
                        }
                        storage
                            .table_def(child, txn.txid)
                            .fkeys()
                            .iter()
                            .position(|foreign_key| {
                                foreign_key.parent_schema == def.schema
                                    && foreign_key.parent == def.name
                            })
                            .map(|foreign_key| (child, foreign_key))
                    });
                    let Some((child, foreign_key)) = inbound else {
                        break;
                    };
                    let child_definition = *storage.table_def(child, txn.txid);
                    let constraint = child_definition.fkeys()[foreign_key].name;
                    if !statement.cascade {
                        return sql_fail(sql_err!(
                            sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                            "cannot drop table {} because other objects depend on it",
                            def.name.as_str()
                        ));
                    }
                    let lsn = storage.bump_lsn();
                    if let Err(error) = wal.stage(
                        txn.txid,
                        lsn,
                        &WalOp::DropTableFk {
                            schema: child_definition.schema.as_str(),
                            table: child_definition.name.as_str(),
                            fk_name: constraint.as_str(),
                        },
                    ) {
                        return sql_fail(error);
                    }
                    let mut updated = child_definition;
                    if !drop_named_constraint(&mut updated, constraint.as_str()) {
                        return sql_fail(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "inbound foreign key vanished during DROP TABLE"
                        ));
                    }
                    let mut identity_mapping = [None; MAX_COLUMNS];
                    for (column, target) in identity_mapping
                        .iter_mut()
                        .enumerate()
                        .take(child_definition.n_columns)
                    {
                        *target = Some(child_definition.columns()[column].name);
                    }
                    if let Err(error) =
                        storage.write_table_def(child, txn.txid, updated, &identity_mapping, false)
                    {
                        return sql_fail(error);
                    }
                    if let Err(error) =
                        txn.record_ddl(super::txn::DdlUndo::TableAltered(child as u32))
                    {
                        storage.rollback_table_def(child, txn.txid);
                        return sql_fail(error);
                    }
                }
                // Owned serial/identity sequences are internal dependencies:
                // dropping their table drops them in the same transaction.
                for sequence_index in 0..storage.sequence_count() {
                    let sequence = storage.sequence_for(sequence_index, txn.txid);
                    if !sequence.visible_to(txn.txid)
                        || !matches!(
                            sequence.owner,
                            Some(owner)
                                if owner.table_schema == def.schema && owner.table == def.name
                        )
                    {
                        continue;
                    }
                    let (sequence_schema, sequence_name) = (sequence.schema, sequence.name);
                    let lsn = storage.bump_lsn();
                    if let Err(error) = wal.stage(
                        txn.txid,
                        lsn,
                        &WalOp::DropSequence {
                            schema: sequence_schema.as_str(),
                            name: sequence_name.as_str(),
                        },
                    ) {
                        return sql_fail(error);
                    }
                    match storage.drop_sequence(
                        sequence_schema.as_str(),
                        sequence_name.as_str(),
                        txn.txid,
                    ) {
                        Ok(Some(slot)) => {
                            if let Err(error) =
                                txn.record_ddl(super::txn::DdlUndo::SequenceDropped(slot as u32))
                            {
                                return sql_fail(error);
                            }
                        }
                        Ok(None) => {}
                        Err(error) => return sql_fail(error),
                    }
                }
                let lsn = storage.bump_lsn();
                if let Err(e) = wal.stage(
                    txn.txid,
                    lsn,
                    &WalOp::DropTable {
                        schema: def.schema.as_str(),
                        name: def.name.as_str(),
                    },
                ) {
                    return sql_fail(e);
                }
                if let Err(e) = txn.record_ddl(super::txn::DdlUndo::Dropped(index as u32)) {
                    return sql_fail(e);
                }
                storage.drop_table_in(index, txn.txid);
                // A table's indexes are dropped with it (no separate journal
                // record; DropTable replay re-applies this).
                storage.drop_indexes_for(def.schema.as_str(), def.name.as_str(), txn.txid);
            }
            _ if statement.if_exists => {
                // PostgreSQL's skip notice carries SQLSTATE 00000.
                responder.notice(
                    crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(128, "table \"{}\" does not exist, skipping", name.name).as_str(),
                )?;
            }
            _ => return sql_fail(undefined_kind("table", name.name)),
        }
    }
    responder.command_complete("DROP TABLE")?;
    sql_ok()
}

/// CREATE SCHEMA [IF NOT EXISTS]: registers a schema in the catalog,
/// journaled and transactional like table DDL.
pub fn create_schema(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &str,
    authorization: Option<&str>,
    if_not_exists: bool,
    responder: &mut Responder,
) -> Outcome {
    let authorized_owner = if let Some(written) = authorization {
        let role = resolve_role_name(written);
        let Some(owner) = storage.find_role_visible(role.as_str(), txn.txid) else {
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "role \"{}\" does not exist",
                role.as_str()
            ));
        };
        let current = storage.current_role_slot(txn.txid).unwrap_or(0);
        if !storage.role(current).attributes_to(txn.txid).superuser
            && current != owner
            && !storage.role_can_set(current, owner, txn.txid)
        {
            return sql_fail(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "must be able to SET ROLE \"{}\"",
                role.as_str()
            ));
        }
        Some(owner)
    } else {
        None
    };
    if name.starts_with("pg_") {
        let mut detail =
            crate::util::StackStr::<{ crate::sql::eval::MAX_DIAGNOSTIC_DETAIL_BYTES }>::new();
        let _ = core::fmt::Write::write_str(
            &mut detail,
            "The prefix \"pg_\" is reserved for system schemas.",
        );
        crate::sql::eval::stash_diagnostic(detail, None);
        return sql_fail(sql_err!(
            crate::sql::eval::sqlstate::RESERVED_NAME,
            "unacceptable schema name \"{}\"",
            name
        ));
    }
    let taken_by_system = name == "information_schema";
    let created = if taken_by_system {
        Err(sql_err!(
            crate::sql::eval::sqlstate::DUPLICATE_SCHEMA,
            "schema \"{}\" already exists",
            name
        ))
    } else {
        let sqlname = match SqlName::parse(name) {
            Ok(n) => n,
            Err(e) => return sql_fail(e),
        };
        storage.create_schema_in(sqlname, txn.txid)
    };
    match created {
        Ok(slot) => {
            if let Some(owner) = authorized_owner {
                storage.set_object_owner(
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::Schema,
                        slot: slot as u16,
                    },
                    owner,
                    txn.txid,
                );
            }
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.stage(txn.txid, lsn, &WalOp::CreateSchema(name)) {
                storage.rollback_schema_create(slot);
                return sql_fail(e);
            }
            if let Err(e) = txn.record_ddl(super::txn::DdlUndo::SchemaCreated(slot as u32)) {
                storage.rollback_schema_create(slot);
                return sql_fail(e);
            }
            if let Err(error) = apply_default_privileges_to_new_object(
                storage,
                txn,
                crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Schema,
                    slot: slot as u16,
                },
            ) {
                return sql_fail(error);
            }
        }
        Err(e) if e.sqlstate == crate::sql::eval::sqlstate::DUPLICATE_SCHEMA && if_not_exists => {
            responder.notice(
                crate::sql::eval::sqlstate::DUPLICATE_SCHEMA,
                stack_format!(128, "schema \"{}\" already exists, skipping", name).as_str(),
            )?;
        }
        Err(e) => return sql_fail(e),
    }
    responder.command_complete("CREATE SCHEMA")?;
    sql_ok()
}

fn rewrite_object_acl_owner(
    storage: &mut Storage,
    txn: &mut TxnState,
    object: crate::storage::AccessObject,
    old_owner: u16,
    new_owner: u16,
) -> Result<(), SqlError> {
    if old_owner == new_owner {
        return Ok(());
    }
    let acl_count = storage.acl_entries().count();
    for slot in 0..acl_count {
        let entry = *storage.acl_entry(slot);
        if entry.object != object || entry.object.slot == u16::MAX {
            continue;
        }
        let (grantee, grantor) = storage.acl_identity(slot, txn.txid);
        if grantee != old_owner && grantor != old_owner {
            continue;
        }
        let prior = storage.change_acl_identity(
            slot,
            if grantee == old_owner {
                new_owner
            } else {
                grantee
            },
            if grantor == old_owner {
                new_owner
            } else {
                grantor
            },
            txn.txid,
        );
        if let Err(error) = txn.record_ddl(super::txn::DdlUndo::ObjectAclChanged {
            slot: slot as u32,
            prior,
        }) {
            storage.restore_acl_pending(slot, prior);
            return Err(error);
        }
    }
    Ok(())
}

fn preserve_object_acl(
    storage: &mut Storage,
    txn: &mut TxnState,
    old_object: crate::storage::AccessObject,
    new_object: crate::storage::AccessObject,
) -> Result<(), SqlError> {
    let acl_count = storage.acl_entries().count();
    for acl_slot in 0..acl_count {
        let entry = *storage.acl_entry(acl_slot);
        if entry.object != old_object {
            continue;
        }
        let (grantee, grantor) = storage.acl_identity(acl_slot, txn.txid);
        let (privileges, grant_options) = storage.acl_state(acl_slot, txn.txid);
        let (changed, prior) = storage.change_acl(
            new_object,
            grantee,
            grantor,
            privileges,
            grant_options,
            txn.txid,
        )?;
        if let Err(error) = txn.record_ddl(super::txn::DdlUndo::ObjectAclChanged {
            slot: changed as u32,
            prior,
        }) {
            storage.restore_acl_pending(changed, prior);
            return Err(error);
        }
    }
    Ok(())
}

pub fn alter_owner(
    storage: &mut Storage,
    txn: &mut TxnState,
    kind: crate::sql::ast::AlterOwnerKind,
    name: &QualName,
    role: &str,
    if_exists: bool,
    responder: &mut Responder,
) -> Outcome {
    use crate::sql::ast::AlterOwnerKind;
    use crate::storage::{AccessClass, AccessObject};
    let (noun, tag) = match kind {
        AlterOwnerKind::Schema => ("schema", "ALTER SCHEMA"),
        AlterOwnerKind::Type => ("type", "ALTER TYPE"),
        AlterOwnerKind::Domain => ("domain", "ALTER DOMAIN"),
        AlterOwnerKind::Table => ("table", "ALTER TABLE"),
        AlterOwnerKind::View => ("view", "ALTER VIEW"),
        AlterOwnerKind::MaterializedView => ("materialized view", "ALTER MATERIALIZED VIEW"),
        AlterOwnerKind::Sequence => ("sequence", "ALTER SEQUENCE"),
    };
    let relation = || storage.resolve_relation(name.schema, name.name, txn.txid);
    let object = match kind {
        AlterOwnerKind::Schema => storage
            .find_schema_visible(name.name, txn.txid)
            .map(|slot| AccessObject {
                class: AccessClass::Schema,
                slot: slot as u16,
            }),
        AlterOwnerKind::Type => (match name.schema {
            Some(schema) => storage.enum_slot(schema, name.name, txn.txid),
            None => storage.resolve_enum_slot(name.name, txn.txid),
        })
        .map(|slot| AccessObject {
            class: AccessClass::Enum,
            slot: slot as u16,
        }),
        AlterOwnerKind::Domain => (match name.schema {
            Some(schema) => storage.domain_slot(schema, name.name, txn.txid),
            None => storage.resolve_domain_slot(name.name, txn.txid),
        })
        .map(|slot| AccessObject {
            class: AccessClass::Domain,
            slot: slot as u16,
        }),
        AlterOwnerKind::Table => match relation() {
            Some(crate::storage::ResolvedRelation::Table(slot)) => {
                let definition = storage.table_def(slot, txn.txid);
                storage
                    .matview_slot(
                        definition.schema.as_str(),
                        definition.name.as_str(),
                        txn.txid,
                    )
                    .map_or(
                        Some(AccessObject {
                            class: AccessClass::Table,
                            slot: slot as u16,
                        }),
                        |matview| {
                            Some(AccessObject {
                                class: AccessClass::MaterializedView,
                                slot: matview as u16,
                            })
                        },
                    )
            }
            Some(crate::storage::ResolvedRelation::View(slot)) => Some(AccessObject {
                class: AccessClass::View,
                slot: slot as u16,
            }),
            Some(_) => {
                return sql_fail(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "\"{}\" is not a table",
                    name.name
                ));
            }
            None => match resolve_sequence(storage, name, txn.txid) {
                Ok(Some(slot)) => Some(AccessObject {
                    class: AccessClass::Sequence,
                    slot: slot as u16,
                }),
                Ok(None) => None,
                Err(error) => return sql_fail(error),
            },
        },
        AlterOwnerKind::View => match relation() {
            Some(crate::storage::ResolvedRelation::View(slot)) => Some(AccessObject {
                class: AccessClass::View,
                slot: slot as u16,
            }),
            Some(_) => {
                return sql_fail(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "\"{}\" is not a view",
                    name.name
                ));
            }
            None => None,
        },
        AlterOwnerKind::MaterializedView => match relation() {
            Some(crate::storage::ResolvedRelation::Table(table)) => {
                let def = *storage.table_def(table, txn.txid);
                let Some(slot) =
                    storage.matview_slot(def.schema.as_str(), def.name.as_str(), txn.txid)
                else {
                    return sql_fail(sql_err!(
                        sqlstate::WRONG_OBJECT_TYPE,
                        "\"{}\" is not a materialized view",
                        name.name
                    ));
                };
                Some(AccessObject {
                    class: AccessClass::MaterializedView,
                    slot: slot as u16,
                })
            }
            Some(_) => {
                return sql_fail(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "\"{}\" is not a materialized view",
                    name.name
                ));
            }
            None => None,
        },
        AlterOwnerKind::Sequence => match resolve_sequence(storage, name, txn.txid) {
            Ok(Some(slot)) => Some(AccessObject {
                class: AccessClass::Sequence,
                slot: slot as u16,
            }),
            Ok(None) => None,
            Err(error) => return sql_fail(error),
        },
    };
    let Some(object) = object else {
        if if_exists {
            responder.notice(
                sqlstate::SUCCESSFUL_COMPLETION,
                stack_format!(160, "{} \"{}\" does not exist, skipping", noun, name.name).as_str(),
            )?;
            responder.command_complete(tag)?;
            return sql_ok();
        }
        return sql_fail(match kind {
            AlterOwnerKind::Schema => sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "schema \"{}\" does not exist",
                name.name
            ),
            AlterOwnerKind::Type | AlterOwnerKind::Domain => {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "type \"{}\" does not exist",
                    name.name
                )
            }
            AlterOwnerKind::Sequence => undefined_kind("sequence", name.name),
            _ => undefined_qual(name),
        });
    };
    let role = resolve_role_name(role);
    let Some(new_owner) = storage.find_role_visible(role.as_str(), txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "role \"{}\" does not exist",
            role.as_str()
        ));
    };
    let current = super::eval::funcs::system::current_user_owned();
    let Some(current_role) = storage.find_role_visible(current.as_str(), txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "role \"{}\" does not exist",
            current.as_str()
        ));
    };
    let superuser = storage.role(current_role).attributes_to(txn.txid).superuser;
    if !superuser && storage.object_owner(object, txn.txid) != current_role {
        return sql_fail(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "must be owner of {} {}",
            noun,
            name.name
        ));
    }
    if !superuser
        && current_role != new_owner
        && !storage.role_can_set(current_role, new_owner, txn.txid)
    {
        return sql_fail(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "must be able to SET ROLE \"{}\"",
            role.as_str()
        ));
    }
    if !superuser && object.class != AccessClass::Schema {
        let (schema, _) = storage.access_object_name(object);
        if let Err(error) = storage.require_schema_create_as(schema.as_str(), new_owner, txn.txid) {
            return sql_fail(error);
        }
    }
    let old_owner = storage.object_owner(object, txn.txid) as u16;
    let prior = storage.set_object_owner(object, new_owner, txn.txid);
    if let Err(error) = txn.record_ddl(super::txn::DdlUndo::ObjectOwnerChanged { object, prior }) {
        storage.restore_object_owner(object, prior);
        return sql_fail(error);
    }
    if let Err(error) = rewrite_object_acl_owner(storage, txn, object, old_owner, new_owner as u16)
    {
        return sql_fail(error);
    }
    responder.command_complete(tag)?;
    sql_ok()
}

fn resolve_role_name(written: &str) -> crate::util::StackStr<64> {
    match written {
        "current_role" | "current_user" => super::eval::funcs::system::current_user_owned(),
        "session_user" => super::eval::funcs::system::session_user_owned(),
        _ => crate::util::StackStr::from_str(written),
    }
}

fn require_create_role(storage: &Storage, txid: u32) -> Result<(), SqlError> {
    let current = super::eval::funcs::system::current_user_owned();
    let allowed = storage
        .find_role_visible(current.as_str(), txid)
        .is_some_and(|slot| {
            let attributes = storage.role(slot).attributes_to(txid);
            attributes.superuser || attributes.create_role
        });
    if allowed {
        Ok(())
    } else {
        Err(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "permission denied to create role"
        ))
    }
}

fn require_role_attribute_authority(
    storage: &Storage,
    txid: u32,
    options: &crate::sql::ast::RoleOptions<'_>,
) -> Result<(), SqlError> {
    let current = super::eval::funcs::system::current_user_owned();
    let slot = storage
        .find_role_visible(current.as_str(), txid)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "role \"{}\" does not exist",
                current.as_str()
            )
        })?;
    let attributes = storage.role(slot).attributes_to(txid);
    if !attributes.superuser
        && (options.superuser == Some(true)
            || options.replication == Some(true)
            || options.bypass_row_level_security == Some(true))
    {
        return Err(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "must be superuser to alter superuser, replication, or bypassrls attributes"
        ));
    }
    if !attributes.superuser && options.create_database == Some(true) && !attributes.create_database
    {
        return Err(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "permission denied to grant CREATEDB"
        ));
    }
    Ok(())
}

fn apply_role_options(
    mut attributes: crate::storage::RoleAttributes,
    options: &crate::sql::ast::RoleOptions<'_>,
) -> Result<crate::storage::RoleAttributes, SqlError> {
    if let Some(value) = options.superuser {
        attributes.superuser = value;
    }
    if let Some(value) = options.inherit {
        attributes.inherit = value;
    }
    if let Some(value) = options.create_role {
        attributes.create_role = value;
    }
    if let Some(value) = options.create_database {
        attributes.create_database = value;
    }
    if let Some(value) = options.can_login {
        attributes.can_login = value;
    }
    if let Some(value) = options.replication {
        attributes.replication = value;
    }
    if let Some(value) = options.bypass_row_level_security {
        attributes.bypass_row_level_security = value;
    }
    if let Some(value) = options.connection_limit {
        if value < -1 {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid connection limit: {}",
                value
            ));
        }
        attributes.connection_limit = value;
    }
    if let Some(password) = options.password {
        attributes.password = crate::storage::RolePassword::EMPTY;
        attributes.has_password = password.is_some();
        if let Some(password) = password {
            if password.len() > crate::storage::ROLE_PASSWORD_MAX {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "role password exceeds {} bytes",
                    crate::storage::ROLE_PASSWORD_MAX
                ));
            }
            let mut salt = [0u8; 16];
            if unsafe { libc::getentropy(salt.as_mut_ptr().cast(), salt.len()) } != 0 {
                return Err(sql_err!(
                    sqlstate::IO_ERROR,
                    "could not generate role password salt"
                ));
            }
            let verifier = crate::pg::auth::ScramServer::derive(
                password,
                salt,
                crate::pg::auth::SCRAM_ITERATIONS,
            );
            attributes.password = crate::storage::RolePassword {
                salt: verifier.salt,
                stored_key: verifier.stored_key,
                server_key: verifier.server_key,
                iterations: verifier.iterations,
            };
        }
    }
    if let Some(valid_until) = options.valid_until {
        attributes.valid_until = crate::util::StackStr::new();
        attributes.has_valid_until = valid_until.is_some();
        if let Some(valid_until) = valid_until {
            if !valid_until.eq_ignore_ascii_case("infinity") {
                crate::sql::datetime::parse_timestamp(valid_until, true)?;
            }
            attributes.valid_until = crate::util::StackStr::from_str(valid_until);
            if attributes.valid_until.is_truncated() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "VALID UNTIL value exceeds {} bytes",
                    crate::storage::ROLE_VALID_UNTIL_MAX
                ));
            }
        }
    }
    Ok(attributes)
}

pub fn create_role(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &str,
    options: &crate::sql::ast::RoleOptions<'_>,
    memberships: &crate::sql::ast::RoleMembershipClauses<'_>,
    responder: &mut Responder,
) -> Outcome {
    if let Err(error) = require_create_role(storage, txn.txid) {
        return sql_fail(error);
    }
    if let Err(error) = require_role_attribute_authority(storage, txn.txid, options) {
        return sql_fail(error);
    }
    let name = match SqlName::parse(name) {
        Ok(name) => name,
        Err(error) => return sql_fail(error),
    };
    let attributes = match apply_role_options(crate::storage::RoleAttributes::ORDINARY, options) {
        Ok(attributes) => attributes,
        Err(error) => return sql_fail(error),
    };
    let (slot, prior) = match storage.create_role(name, attributes, txn.txid) {
        Ok(result) => result,
        Err(error) => return sql_fail(error),
    };
    let lsn = storage.bump_lsn();
    if let Err(error) = wal.stage(
        txn.txid,
        lsn,
        &WalOp::UpsertRole {
            name: name.as_str(),
            attributes,
        },
    ) {
        storage.rollback_role_change(slot, prior);
        return sql_fail(error);
    }
    if let Err(error) = txn.record_ddl(super::txn::DdlUndo::RoleChanged {
        slot: slot as u32,
        prior,
    }) {
        storage.rollback_role_change(slot, prior);
        return sql_fail(error);
    }
    let membership_count = memberships
        .in_roles
        .len()
        .saturating_add(memberships.role_members.len())
        .saturating_add(memberships.admin_members.len());
    if membership_count >= super::txn::MAX_TXN_DDL {
        return sql_fail(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "too many role memberships in one statement"
        ));
    }
    let grantor = storage.current_role_slot(txn.txid).unwrap_or(0);
    for written in memberships.in_roles {
        let resolved = resolve_role_name(written);
        let Some(parent) = storage.find_role_visible(resolved.as_str(), txn.txid) else {
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "role \"{}\" does not exist",
                resolved.as_str()
            ));
        };
        if !storage.role(grantor).attributes_to(txn.txid).superuser
            && !storage.role_can_admin(grantor, parent, txn.txid)
        {
            return sql_fail(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "admin option for role \"{}\" is required",
                resolved.as_str()
            ));
        }
        if let Err(error) = stage_role_membership(
            storage,
            wal,
            txn,
            parent,
            slot,
            grantor,
            crate::storage::RoleMembershipOptions::DEFAULT,
        ) {
            return sql_fail(error);
        }
    }
    for (members, admin) in [
        (memberships.role_members, false),
        (memberships.admin_members, true),
    ] {
        for written in members {
            let resolved = resolve_role_name(written);
            let Some(member) = storage.find_role_visible(resolved.as_str(), txn.txid) else {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "role \"{}\" does not exist",
                    resolved.as_str()
                ));
            };
            if let Err(error) = stage_role_membership(
                storage,
                wal,
                txn,
                slot,
                member,
                grantor,
                crate::storage::RoleMembershipOptions {
                    admin,
                    ..crate::storage::RoleMembershipOptions::DEFAULT
                },
            ) {
                return sql_fail(error);
            }
        }
    }
    responder.command_complete("CREATE ROLE")?;
    sql_ok()
}

fn stage_role_membership(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    role: usize,
    member: usize,
    grantor: usize,
    options: crate::storage::RoleMembershipOptions,
) -> Result<(), SqlError> {
    if role == member || storage.role_is_member_of(role, member, txn.txid) {
        return Err(sql_err!(
            sqlstate::INVALID_GRANT_OPERATION,
            "role \"{}\" is a member of role \"{}\"",
            storage.role_name(role, txn.txid).as_str(),
            storage.role_name(member, txn.txid).as_str()
        ));
    }
    let (membership, prior) =
        storage.change_role_membership(role, member, grantor, options, true, txn.txid)?;
    let role_name = storage.role_name(role, txn.txid);
    let member_name = storage.role_name(member, txn.txid);
    let grantor_name = storage.role_name(grantor, txn.txid);
    let lsn = storage.bump_lsn();
    if let Err(error) = wal.stage(
        txn.txid,
        lsn,
        &WalOp::UpsertRoleMembership {
            role: role_name.as_str(),
            member: member_name.as_str(),
            grantor: grantor_name.as_str(),
            options,
        },
    ) {
        storage.rollback_role_membership_change(membership, prior);
        return Err(error);
    }
    if let Err(error) = txn.record_ddl(super::txn::DdlUndo::RoleMembershipChanged {
        slot: membership as u32,
        prior,
    }) {
        storage.rollback_role_membership_change(membership, prior);
        return Err(error);
    }
    Ok(())
}

pub fn alter_role(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &str,
    options: &crate::sql::ast::RoleOptions<'_>,
    responder: &mut Responder,
) -> Outcome {
    let resolved = resolve_role_name(name);
    let Some(slot) = storage.find_role_visible(resolved.as_str(), txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "role \"{}\" does not exist",
            resolved.as_str()
        ));
    };
    let current = super::eval::funcs::system::current_user_owned();
    let current_slot = storage
        .find_role_visible(current.as_str(), txn.txid)
        .expect("CREATE ROLE privilege check resolved current role");
    let current_attributes = storage.role(current_slot).attributes_to(txn.txid);
    let target_attributes = storage.role(slot).attributes_to(txn.txid);
    if !current_attributes.superuser {
        if current_slot != slot
            && (!current_attributes.create_role
                || target_attributes.superuser
                || !storage.role_can_admin(current_slot, slot, txn.txid))
        {
            return sql_fail(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied to alter role \"{}\"",
                resolved.as_str()
            ));
        }
        if current_slot == slot
            && (options.superuser.is_some()
                || options.inherit.is_some()
                || options.create_role.is_some()
                || options.create_database.is_some()
                || options.can_login.is_some()
                || options.replication.is_some()
                || options.bypass_row_level_security.is_some()
                || options.connection_limit.is_some())
        {
            return sql_fail(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied to alter role attributes"
            ));
        }
    }
    if let Err(error) = require_role_attribute_authority(storage, txn.txid, options) {
        return sql_fail(error);
    }
    let attributes = match apply_role_options(storage.role(slot).attributes_to(txn.txid), options) {
        Ok(attributes) => attributes,
        Err(error) => return sql_fail(error),
    };
    let prior = match storage.alter_role(slot, attributes, txn.txid) {
        Ok(prior) => prior,
        Err(error) => return sql_fail(error),
    };
    let lsn = storage.bump_lsn();
    if let Err(error) = wal.stage(
        txn.txid,
        lsn,
        &WalOp::UpsertRole {
            name: storage.role_name(slot, txn.txid).as_str(),
            attributes,
        },
    ) {
        storage.rollback_role_change(slot, prior);
        return sql_fail(error);
    }
    if let Err(error) = txn.record_ddl(super::txn::DdlUndo::RoleChanged {
        slot: slot as u32,
        prior,
    }) {
        storage.rollback_role_change(slot, prior);
        return sql_fail(error);
    }
    responder.command_complete("ALTER ROLE")?;
    sql_ok()
}

pub fn rename_role(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &str,
    new_name: &str,
    responder: &mut Responder,
) -> Outcome {
    if let Err(error) = require_create_role(storage, txn.txid) {
        return sql_fail(error);
    }
    let resolved = resolve_role_name(name);
    let Some(slot) = storage.find_role_visible(resolved.as_str(), txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "role \"{}\" does not exist",
            resolved.as_str()
        ));
    };
    if resolved.as_str() == super::eval::funcs::system::current_user_owned().as_str() {
        return sql_fail(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "current user cannot be renamed"
        ));
    }
    let new_name = match SqlName::parse(new_name) {
        Ok(name) => name,
        Err(error) => return sql_fail(error),
    };
    let old_name = storage.role(slot).name_to(txn.txid);
    let attributes = storage.role(slot).attributes_to(txn.txid);
    let prior = match storage.rename_role(slot, new_name, txn.txid) {
        Ok(prior) => prior,
        Err(error) => return sql_fail(error),
    };
    let first_lsn = storage.bump_lsn();
    if let Err(error) = wal.stage(
        txn.txid,
        first_lsn,
        &WalOp::DropRole {
            name: old_name.as_str(),
        },
    ) {
        storage.rollback_role_change(slot, prior);
        return sql_fail(error);
    }
    let second_lsn = storage.bump_lsn();
    if let Err(error) = wal.stage(
        txn.txid,
        second_lsn,
        &WalOp::UpsertRole {
            name: new_name.as_str(),
            attributes,
        },
    ) {
        storage.rollback_role_change(slot, prior);
        return sql_fail(error);
    }
    if let Err(error) = txn.record_ddl(super::txn::DdlUndo::RoleChanged {
        slot: slot as u32,
        prior,
    }) {
        storage.rollback_role_change(slot, prior);
        return sql_fail(error);
    }
    responder.command_complete("ALTER ROLE")?;
    sql_ok()
}

pub fn drop_role(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    names: &[&str],
    if_exists: bool,
    responder: &mut Responder,
) -> Outcome {
    if let Err(error) = require_create_role(storage, txn.txid) {
        return sql_fail(error);
    }
    let current = super::eval::funcs::system::session_user_owned();
    for written in names {
        let resolved = resolve_role_name(written);
        let Some(slot) = storage.find_role_visible(resolved.as_str(), txn.txid) else {
            if if_exists {
                responder.notice(
                    sqlstate::UNDEFINED_OBJECT,
                    stack_format!(
                        128,
                        "role \"{}\" does not exist, skipping",
                        resolved.as_str()
                    )
                    .as_str(),
                )?;
                continue;
            }
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "role \"{}\" does not exist",
                resolved.as_str()
            ));
        };
        if resolved.as_str() == current.as_str() {
            return sql_fail(sql_err!(
                sqlstate::OBJECT_IN_USE,
                "current user cannot be dropped"
            ));
        }
        if storage.role_has_object_dependents(slot, txn.txid) {
            return sql_fail(sql_err!(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                "role \"{}\" cannot be dropped because some objects depend on it",
                resolved.as_str()
            ));
        }
        let membership_count = (0..storage.role_membership_count())
            .filter(|membership_slot| {
                let membership = storage.role_membership(*membership_slot);
                membership.visible_to(txn.txid)
                    && (membership.role as usize == slot || membership.member as usize == slot)
            })
            .count();
        if membership_count + 1 > super::txn::MAX_TXN_DDL {
            return sql_fail(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "dropping role \"{}\" changes too many role memberships",
                resolved.as_str()
            ));
        }
        // PostgreSQL removes memberships that name a dropped role.  Make each
        // removal a normal transactional catalog transition so WAL/recovery
        // and savepoint rollback cannot retain a dangling role slot.
        for membership_slot in 0..storage.role_membership_count() {
            let membership = *storage.role_membership(membership_slot);
            if !membership.visible_to(txn.txid)
                || (membership.role as usize != slot && membership.member as usize != slot)
            {
                continue;
            }
            let role_name = storage.role_name(membership.role as usize, txn.txid);
            let member_name = storage.role_name(membership.member as usize, txn.txid);
            let (changed_slot, prior) = match storage.change_role_membership(
                membership.role as usize,
                membership.member as usize,
                membership.grantor as usize,
                membership.options_to(txn.txid),
                false,
                txn.txid,
            ) {
                Ok(changed) => changed,
                Err(error) => return sql_fail(error),
            };
            let lsn = storage.bump_lsn();
            if let Err(error) = wal.stage(
                txn.txid,
                lsn,
                &WalOp::DropRoleMembership {
                    role: role_name.as_str(),
                    member: member_name.as_str(),
                },
            ) {
                storage.rollback_role_membership_change(changed_slot, prior);
                return sql_fail(error);
            }
            if let Err(error) = txn.record_ddl(super::txn::DdlUndo::RoleMembershipChanged {
                slot: changed_slot as u32,
                prior,
            }) {
                storage.rollback_role_membership_change(changed_slot, prior);
                return sql_fail(error);
            }
        }
        let name = storage.role_name(slot, txn.txid);
        let prior = match storage.drop_role_in(slot, txn.txid) {
            Ok(prior) => prior,
            Err(error) => return sql_fail(error),
        };
        let lsn = storage.bump_lsn();
        if let Err(error) = wal.stage(
            txn.txid,
            lsn,
            &WalOp::DropRole {
                name: name.as_str(),
            },
        ) {
            storage.rollback_role_change(slot, prior);
            return sql_fail(error);
        }
        if let Err(error) = txn.record_ddl(super::txn::DdlUndo::RoleChanged {
            slot: slot as u32,
            prior,
        }) {
            storage.rollback_role_change(slot, prior);
            return sql_fail(error);
        }
    }
    responder.command_complete("DROP ROLE")?;
    sql_ok()
}

pub fn set_role(
    storage: &Storage,
    txn: &TxnState,
    guc: &crate::sql::guc::GucState,
    role: Option<&str>,
    local: bool,
    reset: bool,
    responder: &mut Responder,
) -> Outcome {
    let Some(written) = role else {
        guc.reset_role(local);
        responder.command_complete(if reset { "RESET" } else { "SET" })?;
        return sql_ok();
    };
    let resolved = resolve_role_name(written);
    let Some(target) = storage.find_role_visible(resolved.as_str(), txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "role \"{}\" does not exist",
            resolved.as_str()
        ));
    };
    let current = super::eval::funcs::system::current_user_owned();
    let current_superuser = storage
        .find_role_visible(current.as_str(), txn.txid)
        .is_some_and(|slot| storage.role(slot).attributes_to(txn.txid).superuser);
    let current_slot = storage.find_role_visible(current.as_str(), txn.txid);
    if !current_superuser
        && !current_slot.is_some_and(|member| storage.role_can_set(member, target, txn.txid))
    {
        return sql_fail(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "permission denied to set role \"{}\"",
            resolved.as_str()
        ));
    }
    guc.set_role(storage.role_name(target, txn.txid).as_str(), local);
    responder.command_complete("SET")?;
    sql_ok()
}

pub fn set_session_authorization(
    storage: &Storage,
    txn: &TxnState,
    guc: &crate::sql::guc::GucState,
    role: Option<&str>,
    local: bool,
    reset: bool,
    responder: &mut Responder,
) -> Outcome {
    if role.is_none() {
        guc.reset_session_authorization(local);
        responder.command_complete(if reset { "RESET" } else { "SET" })?;
        return sql_ok();
    }
    let authenticated = guc.authenticated_user();
    let Some(authenticated_slot) = storage.find_role_visible(authenticated, txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "authenticated role \"{}\" no longer exists",
            authenticated
        ));
    };
    let authenticated_superuser = storage
        .role(authenticated_slot)
        .attributes_to(txn.txid)
        .superuser;

    let target_name = role.expect("the reset/default case returned above");
    let Some(target) = storage.find_role_visible(target_name, txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "role \"{}\" does not exist",
            target_name
        ));
    };
    if !authenticated_superuser && target != authenticated_slot {
        return sql_fail(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "permission denied to set session authorization \"{}\"",
            target_name
        ));
    }

    if reset {
        guc.reset_session_authorization(local);
    } else {
        let canonical = storage.role_name(target, txn.txid);
        guc.set_session_authorization(canonical.as_str(), local);
    }
    responder.command_complete(if reset { "RESET" } else { "SET" })?;
    sql_ok()
}

pub fn grant_role(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    roles: &[&str],
    members: &[&str],
    options: crate::sql::ast::RoleGrantOptions,
    responder: &mut Responder,
) -> Outcome {
    if roles.len().saturating_mul(members.len()) > super::txn::MAX_TXN_DDL {
        return sql_fail(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "too many role memberships in one statement (limit {})",
            super::txn::MAX_TXN_DDL
        ));
    }
    let current = super::eval::funcs::system::current_user_owned();
    let Some(grantor) = storage.find_role_visible(current.as_str(), txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "role \"{}\" does not exist",
            current.as_str()
        ));
    };
    let grantor_superuser = storage.role(grantor).attributes_to(txn.txid).superuser;
    let mut role_slots = [0usize; crate::storage::MAX_ROLES];
    let mut member_slots = [0usize; crate::storage::MAX_ROLES];
    for (index, written) in roles.iter().enumerate() {
        let resolved = resolve_role_name(written);
        let Some(slot) = storage.find_role_visible(resolved.as_str(), txn.txid) else {
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "role \"{}\" does not exist",
                resolved.as_str()
            ));
        };
        if !grantor_superuser && !storage.role_can_admin(grantor, slot, txn.txid) {
            return sql_fail(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "admin option for role \"{}\" is required",
                resolved.as_str()
            ));
        }
        role_slots[index] = slot;
    }
    for (index, written) in members.iter().enumerate() {
        let resolved = resolve_role_name(written);
        let Some(slot) = storage.find_role_visible(resolved.as_str(), txn.txid) else {
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "role \"{}\" does not exist",
                resolved.as_str()
            ));
        };
        member_slots[index] = slot;
    }
    for &role in &role_slots[..roles.len()] {
        for &member in &member_slots[..members.len()] {
            if role == member || storage.role_is_member_of(role, member, txn.txid) {
                return sql_fail(sql_err!(
                    sqlstate::INVALID_GRANT_OPERATION,
                    "role \"{}\" is a member of role \"{}\"",
                    storage.role_name(role, txn.txid).as_str(),
                    storage.role_name(member, txn.txid).as_str()
                ));
            }
            let membership_options = crate::storage::RoleMembershipOptions {
                admin: options.admin,
                inherit: options.inherit,
                set: options.set,
            };
            let (slot, prior) = match storage.change_role_membership(
                role,
                member,
                grantor,
                membership_options,
                true,
                txn.txid,
            ) {
                Ok(changed) => changed,
                Err(error) => return sql_fail(error),
            };
            let lsn = storage.bump_lsn();
            let role_name = storage.role_name(role, txn.txid);
            let member_name = storage.role_name(member, txn.txid);
            let grantor_name = storage.role_name(grantor, txn.txid);
            let operation = WalOp::UpsertRoleMembership {
                role: role_name.as_str(),
                member: member_name.as_str(),
                grantor: grantor_name.as_str(),
                options: membership_options,
            };
            if let Err(error) = wal.stage(txn.txid, lsn, &operation) {
                storage.rollback_role_membership_change(slot, prior);
                return sql_fail(error);
            }
            if let Err(error) = txn.record_ddl(super::txn::DdlUndo::RoleMembershipChanged {
                slot: slot as u32,
                prior,
            }) {
                storage.rollback_role_membership_change(slot, prior);
                return sql_fail(error);
            }
        }
    }
    responder.command_complete("GRANT ROLE")?;
    sql_ok()
}

pub fn revoke_role(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    roles: &[&str],
    members: &[&str],
    admin_option_only: bool,
    responder: &mut Responder,
) -> Outcome {
    if roles.len().saturating_mul(members.len()) > super::txn::MAX_TXN_DDL {
        return sql_fail(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "too many role memberships in one statement (limit {})",
            super::txn::MAX_TXN_DDL
        ));
    }
    let current = super::eval::funcs::system::current_user_owned();
    let Some(grantor) = storage.find_role_visible(current.as_str(), txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "role \"{}\" does not exist",
            current.as_str()
        ));
    };
    let grantor_superuser = storage.role(grantor).attributes_to(txn.txid).superuser;
    for role_name in roles {
        let role_name = resolve_role_name(role_name);
        let Some(role) = storage.find_role_visible(role_name.as_str(), txn.txid) else {
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "role \"{}\" does not exist",
                role_name.as_str()
            ));
        };
        if !grantor_superuser && !storage.role_can_admin(grantor, role, txn.txid) {
            return sql_fail(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "admin option for role \"{}\" is required",
                role_name.as_str()
            ));
        }
        for member_name in members {
            let member_name = resolve_role_name(member_name);
            let Some(member) = storage.find_role_visible(member_name.as_str(), txn.txid) else {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "role \"{}\" does not exist",
                    member_name.as_str()
                ));
            };
            let Some(existing) = storage.find_role_membership_visible(role, member, txn.txid)
            else {
                responder.notice(
                    sqlstate::WARNING_PRIVILEGE_NOT_GRANTED,
                    stack_format!(
                        160,
                        "role \"{}\" is not a member of role \"{}\"",
                        member_name.as_str(),
                        role_name.as_str()
                    )
                    .as_str(),
                )?;
                continue;
            };
            let old_options = storage.role_membership(existing).options_to(txn.txid);
            let (exists, options) = if admin_option_only {
                (
                    true,
                    crate::storage::RoleMembershipOptions {
                        admin: false,
                        ..old_options
                    },
                )
            } else {
                (false, old_options)
            };
            let (slot, prior) = match storage
                .change_role_membership(role, member, grantor, options, exists, txn.txid)
            {
                Ok(changed) => changed,
                Err(error) => return sql_fail(error),
            };
            let lsn = storage.bump_lsn();
            let role_name = storage.role_name(role, txn.txid);
            let member_name = storage.role_name(member, txn.txid);
            let grantor_name = storage.role_name(grantor, txn.txid);
            let operation = if exists {
                WalOp::UpsertRoleMembership {
                    role: role_name.as_str(),
                    member: member_name.as_str(),
                    grantor: grantor_name.as_str(),
                    options,
                }
            } else {
                WalOp::DropRoleMembership {
                    role: role_name.as_str(),
                    member: member_name.as_str(),
                }
            };
            if let Err(error) = wal.stage(txn.txid, lsn, &operation) {
                storage.rollback_role_membership_change(slot, prior);
                return sql_fail(error);
            }
            if let Err(error) = txn.record_ddl(super::txn::DdlUndo::RoleMembershipChanged {
                slot: slot as u32,
                prior,
            }) {
                storage.rollback_role_membership_change(slot, prior);
                return sql_fail(error);
            }
        }
    }
    responder.command_complete("REVOKE ROLE")?;
    sql_ok()
}

fn privilege_mask(
    privileges: &[crate::sql::ast::Privilege],
    class: crate::storage::AccessClass,
) -> Result<crate::storage::PrivilegeSet, SqlError> {
    use crate::sql::ast::Privilege;
    use crate::storage::{AccessClass, PrivilegeSet};
    let allowed = match class {
        AccessClass::Table | AccessClass::View | AccessClass::MaterializedView => {
            PrivilegeSet::TABLE_ALL
        }
        AccessClass::Sequence => PrivilegeSet::SEQUENCE_ALL,
        AccessClass::Schema => PrivilegeSet::SCHEMA_ALL,
        AccessClass::Domain | AccessClass::Enum => PrivilegeSet::TYPE_ALL,
        AccessClass::Index => PrivilegeSet::NONE,
        AccessClass::Routine => PrivilegeSet::FUNCTION_ALL,
    };
    let mut result = PrivilegeSet::NONE;
    for privilege in privileges {
        let bit = match privilege {
            Privilege::All => allowed,
            Privilege::Select => PrivilegeSet::SELECT,
            Privilege::Insert => PrivilegeSet::INSERT,
            Privilege::Update => PrivilegeSet::UPDATE,
            Privilege::Delete => PrivilegeSet::DELETE,
            Privilege::Truncate => PrivilegeSet::TRUNCATE,
            Privilege::References => PrivilegeSet::REFERENCES,
            Privilege::Trigger => PrivilegeSet::TRIGGER,
            Privilege::Usage => PrivilegeSet::USAGE,
            Privilege::Create => PrivilegeSet::CREATE,
            Privilege::Execute => PrivilegeSet::EXECUTE,
            Privilege::Maintain => PrivilegeSet::MAINTAIN,
        };
        if !allowed.contains(bit) {
            return Err(sql_err!(
                sqlstate::INVALID_GRANT_OPERATION,
                "invalid privilege type for this object"
            ));
        }
        result = result.union(bit);
    }
    Ok(result)
}

fn default_privilege_mask(
    privileges: &[crate::sql::ast::Privilege],
    class: crate::storage::DefaultPrivilegeClass,
) -> Result<crate::storage::PrivilegeSet, SqlError> {
    use crate::sql::ast::Privilege;
    use crate::storage::PrivilegeSet;
    let allowed = class.all_privileges();
    let mut result = PrivilegeSet::NONE;
    for privilege in privileges {
        let bit = match privilege {
            Privilege::All => allowed,
            Privilege::Select => PrivilegeSet::SELECT,
            Privilege::Insert => PrivilegeSet::INSERT,
            Privilege::Update => PrivilegeSet::UPDATE,
            Privilege::Delete => PrivilegeSet::DELETE,
            Privilege::Truncate => PrivilegeSet::TRUNCATE,
            Privilege::References => PrivilegeSet::REFERENCES,
            Privilege::Trigger => PrivilegeSet::TRIGGER,
            Privilege::Usage => PrivilegeSet::USAGE,
            Privilege::Create => PrivilegeSet::CREATE,
            Privilege::Execute => PrivilegeSet::EXECUTE,
            Privilege::Maintain => PrivilegeSet::MAINTAIN,
        };
        if !allowed.contains(bit) {
            return Err(sql_err!(
                sqlstate::INVALID_GRANT_OPERATION,
                "invalid privilege type {} for this object",
                match privilege {
                    Privilege::Select => "SELECT",
                    Privilege::Insert => "INSERT",
                    Privilege::Update => "UPDATE",
                    Privilege::Delete => "DELETE",
                    Privilege::Truncate => "TRUNCATE",
                    Privilege::References => "REFERENCES",
                    Privilege::Trigger => "TRIGGER",
                    Privilege::Usage => "USAGE",
                    Privilege::Create => "CREATE",
                    Privilege::Execute => "EXECUTE",
                    Privilege::Maintain => "MAINTAIN",
                    Privilege::All => "ALL",
                }
            ));
        }
        result = result.union(bit);
    }
    Ok(result)
}

pub fn alter_default_privileges(
    storage: &mut Storage,
    txn: &mut TxnState,
    roles: &[&str],
    schemas: &[&str],
    action: crate::sql::ast::DefaultPrivilegeAction<'_>,
    responder: &mut Responder,
) -> Outcome {
    use crate::sql::ast::{DefaultPrivilegeAction, DefaultPrivilegeObjectKind};
    use crate::storage::{DEFAULT_ACL_ALL_SCHEMAS, DefaultPrivilegeClass, MAX_ROLES, PUBLIC_ROLE};

    let (privileges, kind, grantees, grant, grant_option_only, grant_option) = match action {
        DefaultPrivilegeAction::Grant {
            privileges,
            kind,
            grantees,
            grant_option,
        } => (privileges, kind, grantees, true, false, grant_option),
        DefaultPrivilegeAction::Revoke {
            grant_option_only,
            privileges,
            kind,
            grantees,
            cascade: _,
        } => (privileges, kind, grantees, false, grant_option_only, false),
    };
    let class = match kind {
        DefaultPrivilegeObjectKind::Tables => DefaultPrivilegeClass::Table,
        DefaultPrivilegeObjectKind::Sequences => DefaultPrivilegeClass::Sequence,
        DefaultPrivilegeObjectKind::Functions => DefaultPrivilegeClass::Function,
        DefaultPrivilegeObjectKind::Types => DefaultPrivilegeClass::Type,
        DefaultPrivilegeObjectKind::Schemas => DefaultPrivilegeClass::Schema,
    };
    let requested = match default_privilege_mask(privileges, class) {
        Ok(mask) => mask,
        Err(error) => return sql_fail(error),
    };

    let current_name = super::eval::funcs::system::current_user_owned();
    let Some(current) = storage.find_role_visible(current_name.as_str(), txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "role \"{}\" does not exist",
            current_name.as_str()
        ));
    };
    let current_superuser = storage.role(current).attributes_to(txn.txid).superuser;

    let mut owner_slots = [0u16; MAX_ROLES];
    let owner_count = if roles.is_empty() {
        owner_slots[0] = current as u16;
        1
    } else {
        for (index, written) in roles.iter().enumerate() {
            let resolved = resolve_role_name(written);
            let Some(owner) = storage.find_role_visible(resolved.as_str(), txn.txid) else {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "role \"{}\" does not exist",
                    resolved.as_str()
                ));
            };
            if !current_superuser && !storage.role_can_set(current, owner, txn.txid) {
                return sql_fail(sql_err!(
                    sqlstate::INSUFFICIENT_PRIVILEGE,
                    "permission denied to change default privileges"
                ));
            }
            owner_slots[index] = owner as u16;
        }
        roles.len()
    };

    let mut schema_slots = [DEFAULT_ACL_ALL_SCHEMAS; crate::storage::MAX_SCHEMAS];
    let schema_count = if schemas.is_empty() {
        1
    } else {
        for (index, name) in schemas.iter().enumerate() {
            let Some(slot) = storage.find_schema_visible(name, txn.txid) else {
                return sql_fail(sql_err!(
                    sqlstate::INVALID_SCHEMA_NAME,
                    "schema \"{}\" does not exist",
                    name
                ));
            };
            schema_slots[index] = slot as u16;
        }
        schemas.len()
    };
    if owner_count
        .saturating_mul(schema_count)
        .saturating_mul(grantees.len())
        > super::txn::MAX_TXN_DDL
    {
        return sql_fail(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "too many default privilege changes in one statement"
        ));
    }

    for owner in &owner_slots[..owner_count] {
        for schema in &schema_slots[..schema_count] {
            for written in grantees {
                let grantee = if written.eq_ignore_ascii_case("public") {
                    PUBLIC_ROLE
                } else {
                    let resolved = resolve_role_name(written);
                    let Some(slot) = storage.find_role_visible(resolved.as_str(), txn.txid) else {
                        return sql_fail(sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "role \"{}\" does not exist",
                            resolved.as_str()
                        ));
                    };
                    slot as u16
                };
                let (old_privileges, old_options) =
                    storage.default_acl_effective(*owner, *schema, class, grantee, txn.txid);
                let (new_privileges, new_options) = if grant {
                    (
                        old_privileges.union(requested),
                        if grant_option {
                            old_options.union(requested)
                        } else {
                            old_options
                        },
                    )
                } else if grant_option_only {
                    (old_privileges, old_options.without(requested))
                } else {
                    (
                        old_privileges.without(requested),
                        old_options.without(requested),
                    )
                };
                let baseline = Storage::default_acl_baseline(*owner, *schema, class, grantee);
                let defined = (new_privileges, new_options) != baseline;
                let (slot, prior) = match storage.change_default_acl(
                    crate::storage::DefaultAclKey {
                        owner: *owner,
                        schema: *schema,
                        class,
                        grantee,
                    },
                    defined,
                    new_privileges,
                    new_options,
                    txn.txid,
                ) {
                    Ok(change) => change,
                    Err(error) => return sql_fail(error),
                };
                if let Err(error) = txn.record_ddl(super::txn::DdlUndo::DefaultAclChanged {
                    slot: slot as u32,
                    prior,
                }) {
                    storage.restore_default_acl_pending(slot, prior);
                    return sql_fail(error);
                }
            }
        }
    }
    responder.command_complete("ALTER DEFAULT PRIVILEGES")?;
    sql_ok()
}

fn apply_default_privileges_to_new_object(
    storage: &mut Storage,
    txn: &mut TxnState,
    object: crate::storage::AccessObject,
) -> Result<(), SqlError> {
    use crate::storage::{
        AccessClass, DEFAULT_ACL_ALL_SCHEMAS, DefaultPrivilegeClass, MAX_ROLES, PUBLIC_ROLE,
        PrivilegeSet,
    };
    let class = match object.class {
        AccessClass::Table | AccessClass::View | AccessClass::MaterializedView => {
            DefaultPrivilegeClass::Table
        }
        AccessClass::Sequence => DefaultPrivilegeClass::Sequence,
        AccessClass::Schema => DefaultPrivilegeClass::Schema,
        AccessClass::Domain | AccessClass::Enum => DefaultPrivilegeClass::Type,
        AccessClass::Index => return Ok(()),
        AccessClass::Routine => DefaultPrivilegeClass::Function,
    };
    let owner = storage.object_owner(object, txn.txid) as u16;
    let schema = if object.class == AccessClass::Schema {
        DEFAULT_ACL_ALL_SCHEMAS
    } else {
        let (schema, _) = storage.access_object_name_to(object, txn.txid);
        storage
            .find_schema_visible(schema.as_str(), txn.txid)
            .map_or(DEFAULT_ACL_ALL_SCHEMAS, |slot| slot as u16)
    };
    let customized = storage.default_acl_entries().any(|(_, entry)| {
        entry.owner == owner
            && entry.class == class
            && (entry.schema == DEFAULT_ACL_ALL_SCHEMAS || entry.schema == schema)
            && storage
                .default_acl_state(
                    entry.owner,
                    entry.schema,
                    entry.class,
                    entry.grantee,
                    txn.txid,
                )
                .0
    });

    for role_index in 0..=MAX_ROLES {
        let grantee = if role_index == MAX_ROLES {
            PUBLIC_ROLE
        } else {
            if !storage.role(role_index).visible_to(txn.txid) {
                continue;
            }
            role_index as u16
        };
        let (global_defined, _, _) =
            storage.default_acl_state(owner, DEFAULT_ACL_ALL_SCHEMAS, class, grantee, txn.txid);
        let (global_privileges, global_options) =
            storage.default_acl_effective(owner, DEFAULT_ACL_ALL_SCHEMAS, class, grantee, txn.txid);
        let (schema_defined, schema_privileges, schema_options) =
            if schema == DEFAULT_ACL_ALL_SCHEMAS {
                (false, PrivilegeSet::NONE, PrivilegeSet::NONE)
            } else {
                storage.default_acl_state(owner, schema, class, grantee, txn.txid)
            };
        let privileges = global_privileges.union(schema_privileges);
        let grant_options = global_options.union(schema_options);
        let built_in_without_row = grantee == owner
            || (grantee == PUBLIC_ROLE
                && class.default_public_privileges().0 != PrivilegeSet::NONE.0);
        if !global_defined
            && !schema_defined
            && (privileges.0 == 0 || (built_in_without_row && !customized))
        {
            continue;
        }
        let (slot, prior) =
            storage.change_acl(object, grantee, owner, privileges, grant_options, txn.txid)?;
        if let Err(error) = txn.record_ddl(super::txn::DdlUndo::ObjectAclChanged {
            slot: slot as u32,
            prior,
        }) {
            storage.restore_acl_pending(slot, prior);
            return Err(error);
        }
    }
    Ok(())
}

fn resolve_owned_roles(
    storage: &Storage,
    txid: u32,
    names: &[&str],
    output: &mut [u16; crate::storage::MAX_ROLES],
) -> Result<usize, SqlError> {
    let current_name = super::eval::funcs::system::current_user_owned();
    let current = storage
        .find_role_visible(current_name.as_str(), txid)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "role \"{}\" does not exist",
                current_name.as_str()
            )
        })?;
    let superuser = storage.role(current).attributes_to(txid).superuser;
    let mut count = 0usize;
    for written in names {
        let resolved = resolve_role_name(written);
        let role = storage
            .find_role_visible(resolved.as_str(), txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "role \"{}\" does not exist",
                    resolved.as_str()
                )
            })?;
        if !superuser && !storage.role_can_set(current, role, txid) {
            return Err(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied to drop objects"
            ));
        }
        let role = role as u16;
        if !output[..count].contains(&role) {
            output[count] = role;
            count += 1;
        }
    }
    Ok(count)
}

pub fn reassign_owned(
    storage: &mut Storage,
    txn: &mut TxnState,
    roles: &[&str],
    new_owner: &str,
    responder: &mut Responder,
) -> Outcome {
    use crate::storage::{AccessClass, AccessObject, MAX_ROLES};
    let mut source_roles = [0u16; MAX_ROLES];
    let source_count = match resolve_owned_roles(storage, txn.txid, roles, &mut source_roles) {
        Ok(count) => count,
        Err(error) => return sql_fail(error),
    };
    let resolved_owner = resolve_role_name(new_owner);
    let Some(target) = storage.find_role_visible(resolved_owner.as_str(), txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "role \"{}\" does not exist",
            resolved_owner.as_str()
        ));
    };
    let current_name = super::eval::funcs::system::current_user_owned();
    let current = storage
        .find_role_visible(current_name.as_str(), txn.txid)
        .expect("current role was resolved with the source roles");
    if !storage.role(current).attributes_to(txn.txid).superuser
        && !storage.role_can_set(current, target, txn.txid)
    {
        return sql_fail(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "permission denied to reassign objects"
        ));
    }

    let classes = [
        AccessClass::Table,
        AccessClass::View,
        AccessClass::MaterializedView,
        AccessClass::Sequence,
        AccessClass::Schema,
        AccessClass::Domain,
        AccessClass::Enum,
        AccessClass::Index,
        AccessClass::Routine,
    ];
    let mut changes = 0usize;
    for class in classes {
        for slot in 0..storage.access_class_slots(class) {
            let object = AccessObject {
                class,
                slot: slot as u16,
            };
            if storage.access_object_visible_to(object, txn.txid)
                && source_roles[..source_count]
                    .contains(&(storage.object_owner(object, txn.txid) as u16))
            {
                changes += 1;
            }
        }
    }
    if changes > super::txn::MAX_TXN_DDL.saturating_sub(txn.ddl().len()) {
        return sql_fail(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "too many objects to reassign in one transaction"
        ));
    }
    for class in classes {
        for slot in 0..storage.access_class_slots(class) {
            let object = AccessObject {
                class,
                slot: slot as u16,
            };
            if !storage.access_object_visible_to(object, txn.txid)
                || !source_roles[..source_count]
                    .contains(&(storage.object_owner(object, txn.txid) as u16))
            {
                continue;
            }
            let old_owner = storage.object_owner(object, txn.txid) as u16;
            let prior = storage.set_object_owner(object, target, txn.txid);
            if let Err(error) =
                txn.record_ddl(super::txn::DdlUndo::ObjectOwnerChanged { object, prior })
            {
                storage.restore_object_owner(object, prior);
                return sql_fail(error);
            }
            if let Err(error) =
                rewrite_object_acl_owner(storage, txn, object, old_owner, target as u16)
            {
                return sql_fail(error);
            }
        }
    }
    responder.command_complete("REASSIGN OWNED")?;
    sql_ok()
}

fn run_as_role<T>(role: SqlName, operation: impl FnOnce() -> T) -> T {
    let prior = super::eval::funcs::system::current_user_owned();
    super::eval::funcs::system::set_current_user(role.as_str());
    let result = operation();
    super::eval::funcs::system::set_current_user(prior.as_str());
    result
}

fn record_acl_removal(
    storage: &mut Storage,
    txn: &mut TxnState,
    slot: usize,
) -> Result<crate::storage::PrivilegeSet, SqlError> {
    let entry = *storage.acl_entry(slot);
    let (grantee, grantor) = storage.acl_identity(slot, txn.txid);
    let (_, grant_options) = storage.acl_state(slot, txn.txid);
    let (changed, prior) = storage.change_acl(
        entry.object,
        grantee,
        grantor,
        crate::storage::PrivilegeSet::NONE,
        crate::storage::PrivilegeSet::NONE,
        txn.txid,
    )?;
    if let Err(error) = txn.record_ddl(super::txn::DdlUndo::ObjectAclChanged {
        slot: changed as u32,
        prior,
    }) {
        storage.restore_acl_pending(changed, prior);
        return Err(error);
    }
    Ok(grant_options)
}

fn drop_owned_privileges(
    storage: &mut Storage,
    txn: &mut TxnState,
    roles: &[u16],
) -> Result<(), SqlError> {
    use crate::storage::{MAX_ACL_ENTRIES, PUBLIC_ROLE, PrivilegeSet};
    let mut queue_objects = [crate::storage::AccessObject {
        class: crate::storage::AccessClass::Table,
        slot: 0,
    }; MAX_ACL_ENTRIES];
    let mut queue_roles = [0u16; MAX_ACL_ENTRIES];
    let mut queue_privileges = [PrivilegeSet::NONE; MAX_ACL_ENTRIES];
    let mut queue_count = 0usize;

    for slot in 0..storage.acl_entries().count() {
        let entry = *storage.acl_entry(slot);
        let (grantee, grantor) = storage.acl_identity(slot, txn.txid);
        let (privileges, _) = storage.acl_state(slot, txn.txid);
        if privileges.0 == 0
            || !storage.access_object_visible_to(entry.object, txn.txid)
            || (!roles.contains(&grantee) && !roles.contains(&grantor))
        {
            continue;
        }
        let lost_options = record_acl_removal(storage, txn, slot)?;
        if grantee != PUBLIC_ROLE && lost_options.0 != 0 {
            if queue_count == MAX_ACL_ENTRIES {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "privilege dependency graph exceeds {} entries",
                    MAX_ACL_ENTRIES
                ));
            }
            queue_objects[queue_count] = entry.object;
            queue_roles[queue_count] = grantee;
            queue_privileges[queue_count] = lost_options;
            queue_count += 1;
        }
    }
    let mut at = 0usize;
    let mut dependent = [0usize; MAX_ACL_ENTRIES];
    while at < queue_count {
        let object = queue_objects[at];
        let grantor = queue_roles[at];
        let lost = queue_privileges[at];
        at += 1;
        let dependent_count =
            storage.dependent_acl_slots(object, grantor, lost, txn.txid, &mut dependent);
        for slot in dependent[..dependent_count].iter().copied() {
            let entry = *storage.acl_entry(slot);
            let (grantee, _) = storage.acl_identity(slot, txn.txid);
            let (privileges, _) = storage.acl_state(slot, txn.txid);
            if privileges.0 == 0 {
                continue;
            }
            let recursively_lost = record_acl_removal(storage, txn, slot)?;
            if grantee != PUBLIC_ROLE && recursively_lost.0 != 0 {
                if queue_count == MAX_ACL_ENTRIES {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "privilege dependency graph exceeds {} entries",
                        MAX_ACL_ENTRIES
                    ));
                }
                queue_objects[queue_count] = entry.object;
                queue_roles[queue_count] = grantee;
                queue_privileges[queue_count] = recursively_lost;
                queue_count += 1;
            }
        }
    }

    let default_count = storage.default_acl_entries().count();
    for slot in 0..default_count {
        let entry = *storage.default_acl_entry(slot);
        let (defined, _, _) = storage.default_acl_state(
            entry.owner,
            entry.schema,
            entry.class,
            entry.grantee,
            txn.txid,
        );
        if !defined || (!roles.contains(&entry.owner) && !roles.contains(&entry.grantee)) {
            continue;
        }
        let (changed, prior) = storage.change_default_acl(
            crate::storage::DefaultAclKey {
                owner: entry.owner,
                schema: entry.schema,
                class: entry.class,
                grantee: entry.grantee,
            },
            false,
            PrivilegeSet::NONE,
            PrivilegeSet::NONE,
            txn.txid,
        )?;
        if let Err(error) = txn.record_ddl(super::txn::DdlUndo::DefaultAclChanged {
            slot: changed as u32,
            prior,
        }) {
            storage.restore_default_acl_pending(changed, prior);
            return Err(error);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn drop_owned(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    scratch: &mut FixedVec<(u64, RowHome)>,
    roles: &[&str],
    cascade: bool,
    arena: &Arena,
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
) -> Outcome {
    use crate::storage::{
        AccessClass, AccessObject, DependencyClass, MAX_DOMAINS, MAX_ENUMS, MAX_ROLES, MAX_SCHEMAS,
        MAX_SEQUENCES,
    };
    let mut owned_roles = [0u16; MAX_ROLES];
    let owned_role_count = match resolve_owned_roles(storage, txn.txid, roles, &mut owned_roles) {
        Ok(count) => count,
        Err(error) => return sql_fail(error),
    };
    let owned_roles = &owned_roles[..owned_role_count];
    if let Err(error) = drop_owned_privileges(storage, txn, owned_roles) {
        return sql_fail(error);
    }

    if storage.table_count() > MAX_DEPENDENT_STORED_QUERIES
        || storage.view_count() > MAX_DEPENDENT_STORED_QUERIES
        || storage.matview_count() > MAX_DEPENDENT_STORED_QUERIES
    {
        return sql_fail(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "DROP OWNED dependency plan exceeds {} relation slots",
            MAX_DEPENDENT_STORED_QUERIES
        ));
    }
    let mut tables = [false; MAX_DEPENDENT_STORED_QUERIES];
    let mut views = [false; MAX_DEPENDENT_STORED_QUERIES];
    let mut matviews = [false; MAX_DEPENDENT_STORED_QUERIES];
    let mut sequences = [false; MAX_SEQUENCES];
    let mut domains = [false; MAX_DOMAINS];
    let mut enums = [false; MAX_ENUMS];
    let mut schemas = [false; MAX_SCHEMAS];
    for (class, selected) in [
        (AccessClass::Table, &mut tables[..]),
        (AccessClass::View, &mut views[..]),
        (AccessClass::MaterializedView, &mut matviews[..]),
        (AccessClass::Sequence, &mut sequences[..]),
        (AccessClass::Domain, &mut domains[..]),
        (AccessClass::Enum, &mut enums[..]),
        (AccessClass::Schema, &mut schemas[..]),
    ] {
        for (slot, selected) in selected
            .iter_mut()
            .enumerate()
            .take(storage.access_class_slots(class))
        {
            let object = AccessObject {
                class,
                slot: slot as u16,
            };
            *selected = storage.access_object_visible_to(object, txn.txid)
                && owned_roles.contains(&(storage.object_owner(object, txn.txid) as u16));
        }
    }

    let root = |dependency: &crate::storage::StoredQueryDependency| match dependency.class {
        DependencyClass::Table => tables
            .get(dependency.slot as usize)
            .copied()
            .unwrap_or(false),
        DependencyClass::View => views
            .get(dependency.slot as usize)
            .copied()
            .unwrap_or(false),
        DependencyClass::Domain => domains
            .get(dependency.slot as usize)
            .copied()
            .unwrap_or(false),
        DependencyClass::Enum => enums
            .get(dependency.slot as usize)
            .copied()
            .unwrap_or(false),
        DependencyClass::Sequence => sequences
            .get(dependency.slot as usize)
            .copied()
            .unwrap_or(false),
    };
    let (dependent_views, dependent_matviews) =
        match stored_query_dependent_closure(storage, txn.txid, root) {
            Ok(selection) => selection,
            Err(error) => return sql_fail(error),
        };
    if !cascade
        && (dependent_views
            .iter()
            .zip(views)
            .any(|(dependent, owned)| *dependent && !owned)
            || dependent_matviews
                .iter()
                .zip(matviews)
                .any(|(dependent, owned)| *dependent && !owned))
    {
        return sql_fail(sql_err!(
            sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
            "cannot drop owned objects because other objects depend on them"
        ));
    }
    if cascade {
        for slot in 0..views.len() {
            views[slot] |= dependent_views[slot];
            matviews[slot] |= dependent_matviews[slot];
        }
    }
    if let Err(error) = drop_selected_stored_queries(storage, wal, txn, &views, &matviews) {
        return sql_fail(error);
    }

    // A CASCADE of an owned schema removes every contained object, including
    // objects owned by another role. RESTRICT postpones schemas until their
    // owned contents have been removed and lets the ordinary dependency
    // preflight reject any survivors.
    if cascade {
        for (slot, selected) in schemas
            .iter()
            .copied()
            .enumerate()
            .take(storage.access_class_slots(AccessClass::Schema))
        {
            if !selected
                || !storage.access_object_visible_to(
                    AccessObject {
                        class: AccessClass::Schema,
                        slot: slot as u16,
                    },
                    txn.txid,
                )
            {
                continue;
            }
            let schema = storage.schema_def(slot).name;
            let owner = storage.role_name(
                storage.object_owner(
                    AccessObject {
                        class: AccessClass::Schema,
                        slot: slot as u16,
                    },
                    txn.txid,
                ),
                txn.txid,
            );
            let outcome = run_as_role(owner, || {
                responder.without_command_complete(|responder| {
                    drop_schema(
                        storage,
                        wal,
                        txn,
                        scratch,
                        &[schema.as_str()],
                        false,
                        true,
                        arena,
                        seq_session,
                        responder,
                    )
                })
            });
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return sql_fail(error),
                Err(error) => return Err(error),
            }
        }
    }

    let mut table_schemas = [SqlName::EMPTY; MAX_DEPENDENT_STORED_QUERIES];
    let mut table_names = [SqlName::EMPTY; MAX_DEPENDENT_STORED_QUERIES];
    let mut table_name_count = 0usize;
    for (slot, selected) in tables
        .iter()
        .copied()
        .enumerate()
        .take(storage.table_count())
    {
        if !selected || !storage.table(slot).visible_to(txn.txid) {
            continue;
        }
        let definition = *storage.table_def(slot, txn.txid);
        if storage
            .matview_slot(
                definition.schema.as_str(),
                definition.name.as_str(),
                txn.txid,
            )
            .is_none()
        {
            table_schemas[table_name_count] = definition.schema;
            table_names[table_name_count] = definition.name;
            table_name_count += 1;
        }
    }
    if table_name_count != 0 {
        let mut qualified = [QualName::bare(""); MAX_DEPENDENT_STORED_QUERIES];
        for index in 0..table_name_count {
            qualified[index] = QualName {
                schema: Some(table_schemas[index].as_str()),
                name: table_names[index].as_str(),
            };
        }
        let statement = DropTable {
            names: &qualified[..table_name_count],
            if_exists: false,
            cascade,
        };
        let outcome = responder.without_command_complete(|responder| {
            drop_table(storage, wal, txn, &statement, responder)
        });
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return sql_fail(error),
            Err(error) => return Err(error),
        }
    }

    for (class, selected) in [
        (AccessClass::Sequence, &sequences[..]),
        (AccessClass::Domain, &domains[..]),
        (AccessClass::Enum, &enums[..]),
    ] {
        for (slot, selected) in selected
            .iter()
            .copied()
            .enumerate()
            .take(storage.access_class_slots(class))
            .rev()
        {
            let object = AccessObject {
                class,
                slot: slot as u16,
            };
            if !selected || !storage.access_object_visible_to(object, txn.txid) {
                continue;
            }
            let (schema, name) = storage.access_object_name_to(object, txn.txid);
            let owner = storage.role_name(storage.object_owner(object, txn.txid), txn.txid);
            let qualified = QualName {
                schema: Some(schema.as_str()),
                name: name.as_str(),
            };
            let outcome = run_as_role(owner, || {
                responder.without_command_complete(|responder| match class {
                    AccessClass::Sequence => drop_sequence(
                        storage,
                        wal,
                        txn,
                        core::slice::from_ref(&qualified),
                        false,
                        cascade,
                        responder,
                    ),
                    AccessClass::Domain => drop_domain(
                        storage,
                        wal,
                        txn,
                        scratch,
                        core::slice::from_ref(&qualified),
                        false,
                        cascade,
                        arena,
                        seq_session,
                        responder,
                    ),
                    AccessClass::Enum => drop_enum(
                        storage,
                        wal,
                        txn,
                        scratch,
                        core::slice::from_ref(&qualified),
                        false,
                        cascade,
                        arena,
                        seq_session,
                        responder,
                    ),
                    _ => unreachable!(),
                })
            });
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return sql_fail(error),
                Err(error) => return Err(error),
            }
        }
    }

    // Standalone indexes can have a different owner from their table.
    for slot in (0..storage.access_class_slots(AccessClass::Index)).rev() {
        let object = AccessObject {
            class: AccessClass::Index,
            slot: slot as u16,
        };
        if !storage.access_object_visible_to(object, txn.txid)
            || !owned_roles.contains(&(storage.object_owner(object, txn.txid) as u16))
        {
            continue;
        }
        let (schema, name) = storage.access_object_name_to(object, txn.txid);
        let owner = storage.role_name(storage.object_owner(object, txn.txid), txn.txid);
        let qualified = QualName {
            schema: Some(schema.as_str()),
            name: name.as_str(),
        };
        let outcome = run_as_role(owner, || {
            responder.without_command_complete(|responder| {
                drop_index(
                    storage,
                    wal,
                    txn,
                    core::slice::from_ref(&qualified),
                    false,
                    responder,
                )
            })
        });
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return sql_fail(error),
            Err(error) => return Err(error),
        }
    }

    for slot in (0..storage.access_class_slots(AccessClass::Routine)).rev() {
        let object = Storage::routine_access_object(slot);
        if !storage.access_object_visible_to(object, txn.txid)
            || !owned_roles.contains(&(storage.object_owner(object, txn.txid) as u16))
        {
            continue;
        }
        let routine = *storage.routine(slot);
        let mut type_codes = [0_u8; MAX_ROUTINE_ARGUMENTS];
        for (index, argument) in routine.arguments().iter().enumerate() {
            type_codes[index] = argument.ctype.code();
        }
        let lsn = storage.bump_lsn();
        if let Err(error) = wal.stage(
            txn.txid,
            lsn,
            &WalOp::DropRoutine {
                schema: routine.schema_for(txn.txid).as_str(),
                name: routine.name_for(txn.txid).as_str(),
                argument_type_codes: &type_codes[..routine.argument_count],
            },
        ) {
            return sql_fail(error);
        }
        storage.drop_routine(slot, txn.txid);
        if let Err(error) = txn.record_ddl(super::txn::DdlUndo::RoutineDropped(slot as u32)) {
            storage.rollback_routine_drop(slot, txn.txid);
            return sql_fail(error);
        }
    }

    if !cascade {
        for (slot, selected) in schemas
            .iter()
            .copied()
            .enumerate()
            .take(storage.access_class_slots(AccessClass::Schema))
            .rev()
        {
            let object = AccessObject {
                class: AccessClass::Schema,
                slot: slot as u16,
            };
            if !selected || !storage.access_object_visible_to(object, txn.txid) {
                continue;
            }
            let schema = storage.schema_def(slot).name;
            let owner = storage.role_name(storage.object_owner(object, txn.txid), txn.txid);
            let outcome = run_as_role(owner, || {
                responder.without_command_complete(|responder| {
                    drop_schema(
                        storage,
                        wal,
                        txn,
                        scratch,
                        &[schema.as_str()],
                        false,
                        false,
                        arena,
                        seq_session,
                        responder,
                    )
                })
            });
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return sql_fail(error),
                Err(error) => return Err(error),
            }
        }
    }
    responder.command_complete("DROP OWNED")?;
    sql_ok()
}

fn add_privilege_object(
    objects: &mut [crate::storage::AccessObject; crate::storage::MAX_ACL_ENTRIES],
    count: &mut usize,
    object: crate::storage::AccessObject,
) -> Result<(), SqlError> {
    if objects[..*count].contains(&object) {
        return Ok(());
    }
    if *count == objects.len() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "too many objects in one privilege statement (limit {})",
            objects.len()
        ));
    }
    objects[*count] = object;
    *count += 1;
    Ok(())
}

fn resolve_privilege_objects(
    storage: &Storage,
    target: crate::sql::ast::PrivilegeTarget<'_>,
    txid: u32,
    objects: &mut [crate::storage::AccessObject; crate::storage::MAX_ACL_ENTRIES],
) -> Result<usize, SqlError> {
    use crate::sql::ast::{PrivilegeObjectKind, PrivilegeTarget, RoutineTargetKind};
    use crate::storage::{AccessClass, AccessObject};
    let mut count = 0usize;
    match target {
        PrivilegeTarget::Routines { kind, identities } => {
            for identity in identities {
                let schema = identity.name.schema.unwrap_or("public");
                let mut argument_types = [ColType::Text; MAX_ROUTINE_ARGUMENTS];
                for (index, type_name) in identity.argument_types.iter().enumerate() {
                    let Some(ctype) = ColType::from_sql_name(type_name) else {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "type \"{}\" does not exist",
                            type_name
                        ));
                    };
                    argument_types[index] = ctype;
                }
                let Some(slot) = storage.routine_slot_by_signature(
                    schema,
                    identity.name.name,
                    &argument_types[..identity.argument_types.len()],
                    txid,
                ) else {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function \"{}\" does not exist",
                        identity.name.name
                    ));
                };
                let actual = storage.routine(slot).kind;
                let accepted = match kind {
                    RoutineTargetKind::Function => actual.function_result().is_some(),
                    RoutineTargetKind::Procedure => {
                        matches!(actual, crate::storage::RoutineKind::Procedure)
                    }
                    RoutineTargetKind::Either => true,
                };
                if !accepted {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "{} \"{}\" does not exist",
                        match kind {
                            RoutineTargetKind::Function => "function",
                            RoutineTargetKind::Procedure => "procedure",
                            RoutineTargetKind::Either => "routine",
                        },
                        identity.name.name
                    ));
                }
                add_privilege_object(objects, &mut count, Storage::routine_access_object(slot))?;
            }
        }
        PrivilegeTarget::Objects { kind, names } => {
            for name in names {
                match kind {
                    PrivilegeObjectKind::Table => {
                        let object = match storage.resolve_relation(name.schema, name.name, txid) {
                            Some(crate::storage::ResolvedRelation::Table(slot)) => {
                                let definition = storage.table_def(slot, txid);
                                match storage.matview_slot(
                                    definition.schema.as_str(),
                                    definition.name.as_str(),
                                    txid,
                                ) {
                                    Some(matview) => AccessObject {
                                        class: AccessClass::MaterializedView,
                                        slot: matview as u16,
                                    },
                                    None => AccessObject {
                                        class: AccessClass::Table,
                                        slot: slot as u16,
                                    },
                                }
                            }
                            Some(crate::storage::ResolvedRelation::View(slot)) => AccessObject {
                                class: AccessClass::View,
                                slot: slot as u16,
                            },
                            _ => return Err(undefined_qual(name)),
                        };
                        add_privilege_object(objects, &mut count, object)?;
                    }
                    PrivilegeObjectKind::Sequence => {
                        let Some(slot) = resolve_sequence(storage, name, txid)? else {
                            return Err(undefined_kind("sequence", name.name));
                        };
                        add_privilege_object(
                            objects,
                            &mut count,
                            AccessObject {
                                class: AccessClass::Sequence,
                                slot: slot as u16,
                            },
                        )?;
                    }
                    PrivilegeObjectKind::Schema => {
                        let Some(slot) = storage.find_schema_visible(name.name, txid) else {
                            return Err(sql_err!(
                                sqlstate::INVALID_SCHEMA_NAME,
                                "schema \"{}\" does not exist",
                                name.name
                            ));
                        };
                        add_privilege_object(
                            objects,
                            &mut count,
                            AccessObject {
                                class: AccessClass::Schema,
                                slot: slot as u16,
                            },
                        )?;
                    }
                    PrivilegeObjectKind::Type => {
                        let domain = match name.schema {
                            Some(schema) => storage.domain_slot(schema, name.name, txid),
                            None => storage.resolve_domain_slot(name.name, txid),
                        };
                        let enumeration = match name.schema {
                            Some(schema) => storage.enum_slot(schema, name.name, txid),
                            None => storage.resolve_enum_slot(name.name, txid),
                        };
                        let object = domain
                            .map(|slot| AccessObject {
                                class: AccessClass::Domain,
                                slot: slot as u16,
                            })
                            .or_else(|| {
                                enumeration.map(|slot| AccessObject {
                                    class: AccessClass::Enum,
                                    slot: slot as u16,
                                })
                            })
                            .ok_or_else(|| {
                                sql_err!(
                                    sqlstate::UNDEFINED_OBJECT,
                                    "type \"{}\" does not exist",
                                    name.name
                                )
                            })?;
                        add_privilege_object(objects, &mut count, object)?;
                    }
                    PrivilegeObjectKind::AllTablesInSchema => {
                        let schema = name.name;
                        if storage.find_schema_visible(schema, txid).is_none() {
                            return Err(sql_err!(
                                sqlstate::INVALID_SCHEMA_NAME,
                                "schema \"{}\" does not exist",
                                schema
                            ));
                        }
                        for slot in 0..storage.table_count() {
                            if !storage.table(slot).visible_to(txid) {
                                continue;
                            }
                            let definition = storage.table_def(slot, txid);
                            if definition.schema.as_str() != schema {
                                continue;
                            }
                            let object = storage
                                .matview_slot(schema, definition.name.as_str(), txid)
                                .map_or(
                                    AccessObject {
                                        class: AccessClass::Table,
                                        slot: slot as u16,
                                    },
                                    |matview| AccessObject {
                                        class: AccessClass::MaterializedView,
                                        slot: matview as u16,
                                    },
                                );
                            add_privilege_object(objects, &mut count, object)?;
                        }
                        for (slot, view) in storage.views_with_slots() {
                            if view.visible_to(txid) && view.schema.as_str() == schema {
                                add_privilege_object(
                                    objects,
                                    &mut count,
                                    AccessObject {
                                        class: AccessClass::View,
                                        slot: slot as u16,
                                    },
                                )?;
                            }
                        }
                    }
                    PrivilegeObjectKind::AllSequencesInSchema => {
                        let schema = name.name;
                        if storage.find_schema_visible(schema, txid).is_none() {
                            return Err(sql_err!(
                                sqlstate::INVALID_SCHEMA_NAME,
                                "schema \"{}\" does not exist",
                                schema
                            ));
                        }
                        for slot in 0..storage.sequence_count() {
                            let sequence = storage.sequence_for(slot, txid);
                            if sequence.visible_to(txid) && sequence.schema.as_str() == schema {
                                add_privilege_object(
                                    objects,
                                    &mut count,
                                    AccessObject {
                                        class: AccessClass::Sequence,
                                        slot: slot as u16,
                                    },
                                )?;
                            }
                        }
                    }
                    PrivilegeObjectKind::AllFunctionsInSchema => {
                        let schema = name.name;
                        if storage.find_schema_visible(schema, txid).is_none() {
                            return Err(sql_err!(
                                sqlstate::INVALID_SCHEMA_NAME,
                                "schema \"{}\" does not exist",
                                schema
                            ));
                        }
                        for slot in 0..storage.routine_count() {
                            let routine = storage.routine(slot);
                            if routine.visible_to(txid)
                                && routine.schema_for(txid).as_str() == schema
                            {
                                add_privilege_object(
                                    objects,
                                    &mut count,
                                    Storage::routine_access_object(slot),
                                )?;
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(count)
}

#[allow(clippy::too_many_arguments)]
pub fn grant_privileges(
    storage: &mut Storage,
    txn: &mut TxnState,
    privileges: &[crate::sql::ast::Privilege],
    target: crate::sql::ast::PrivilegeTarget<'_>,
    grantees: &[&str],
    grant_option: bool,
    responder: &mut Responder,
) -> Outcome {
    use crate::storage::{AccessClass, AccessObject, PUBLIC_ROLE};
    let mut objects = [AccessObject {
        class: AccessClass::Table,
        slot: 0,
    }; crate::storage::MAX_ACL_ENTRIES];
    let object_count = match resolve_privilege_objects(storage, target, txn.txid, &mut objects) {
        Ok(count) => count,
        Err(error) => return sql_fail(error),
    };
    let current = super::eval::funcs::system::current_user_owned();
    let Some(grantor) = storage.find_role_visible(current.as_str(), txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "role \"{}\" does not exist",
            current.as_str()
        ));
    };
    if object_count.saturating_mul(grantees.len()) > super::txn::MAX_TXN_DDL {
        return sql_fail(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "too many privilege changes in one statement"
        ));
    }
    for object in &objects[..object_count] {
        let requested = match privilege_mask(privileges, object.class) {
            Ok(mask) => mask,
            Err(error) => return sql_fail(error),
        };
        if !storage.has_object_grant_option(*object, grantor, requested, txn.txid) {
            let (_, name) = storage.access_object_name(*object);
            return sql_fail(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied for relation {}",
                name.as_str()
            ));
        }
        let acl_grantor = acl_grantor(storage, *object, grantor, txn.txid);
        for name in grantees {
            let grantee = if name.eq_ignore_ascii_case("public") {
                PUBLIC_ROLE
            } else {
                let Some(slot) = storage.find_role_visible(name, txn.txid) else {
                    return sql_fail(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "role \"{}\" does not exist",
                        name
                    ));
                };
                slot as u16
            };
            let (old_privileges, old_options) =
                storage.acl_from(*object, grantee, acl_grantor as u16, txn.txid);
            let new_options = if grant_option {
                old_options.union(requested)
            } else {
                old_options
            };
            let (slot, prior) = match storage.change_acl(
                *object,
                grantee,
                acl_grantor as u16,
                old_privileges.union(requested),
                new_options,
                txn.txid,
            ) {
                Ok(change) => change,
                Err(error) => return sql_fail(error),
            };
            if let Err(error) = txn.record_ddl(super::txn::DdlUndo::ObjectAclChanged {
                slot: slot as u32,
                prior,
            }) {
                storage.restore_acl_pending(slot, prior);
                return sql_fail(error);
            }
        }
    }
    responder.command_complete("GRANT")?;
    sql_ok()
}

#[allow(clippy::too_many_arguments)]
pub fn revoke_privileges(
    storage: &mut Storage,
    txn: &mut TxnState,
    grant_option_only: bool,
    privileges: &[crate::sql::ast::Privilege],
    target: crate::sql::ast::PrivilegeTarget<'_>,
    grantees: &[&str],
    cascade: bool,
    responder: &mut Responder,
) -> Outcome {
    use crate::storage::{AccessClass, AccessObject, PUBLIC_ROLE};
    let mut objects = [AccessObject {
        class: AccessClass::Table,
        slot: 0,
    }; crate::storage::MAX_ACL_ENTRIES];
    let object_count = match resolve_privilege_objects(storage, target, txn.txid, &mut objects) {
        Ok(count) => count,
        Err(error) => return sql_fail(error),
    };
    let current = super::eval::funcs::system::current_user_owned();
    let Some(grantor) = storage.find_role_visible(current.as_str(), txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "role \"{}\" does not exist",
            current.as_str()
        ));
    };
    for object in &objects[..object_count] {
        let requested = match privilege_mask(privileges, object.class) {
            Ok(mask) => mask,
            Err(error) => return sql_fail(error),
        };
        if !storage.has_object_grant_option(*object, grantor, requested, txn.txid) {
            let (_, name) = storage.access_object_name(*object);
            return sql_fail(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied for relation {}",
                name.as_str()
            ));
        }
        let acl_grantor = acl_grantor(storage, *object, grantor, txn.txid);
        for name in grantees {
            let grantee = if name.eq_ignore_ascii_case("public") {
                PUBLIC_ROLE
            } else {
                let Some(slot) = storage.find_role_visible(name, txn.txid) else {
                    return sql_fail(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "role \"{}\" does not exist",
                        name
                    ));
                };
                slot as u16
            };
            let (old_privileges, old_options) =
                storage.acl_from(*object, grantee, acl_grantor as u16, txn.txid);
            let removed_options = crate::storage::PrivilegeSet(old_options.0 & requested.0);
            if grantee != PUBLIC_ROLE && removed_options.0 != 0 {
                let mut dependent = [0usize; crate::storage::MAX_ACL_ENTRIES];
                let dependent_count = storage.dependent_acl_slots(
                    *object,
                    grantee,
                    removed_options,
                    txn.txid,
                    &mut dependent,
                );
                if dependent_count != 0 && !cascade {
                    return sql_fail(sql_err!(
                        sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                        "dependent privileges exist"
                    ));
                }
                if cascade {
                    let mut queue_roles = [0u16; crate::storage::MAX_ACL_ENTRIES];
                    let mut queue_privileges =
                        [crate::storage::PrivilegeSet::NONE; crate::storage::MAX_ACL_ENTRIES];
                    queue_roles[0] = grantee;
                    queue_privileges[0] = removed_options;
                    let mut queue_len = 1usize;
                    let mut queue_at = 0usize;
                    while queue_at < queue_len {
                        let downstream_grantor = queue_roles[queue_at];
                        let lost_options = queue_privileges[queue_at];
                        queue_at += 1;
                        let dependent_count = storage.dependent_acl_slots(
                            *object,
                            downstream_grantor,
                            lost_options,
                            txn.txid,
                            &mut dependent,
                        );
                        for dependent_slot in &dependent[..dependent_count] {
                            let entry = *storage.acl_entry(*dependent_slot);
                            let (dependent_grantee, dependent_grantor) =
                                storage.acl_identity(*dependent_slot, txn.txid);
                            let (dependent_privileges, dependent_options) =
                                storage.acl_state(*dependent_slot, txn.txid);
                            let recursively_lost =
                                crate::storage::PrivilegeSet(dependent_options.0 & lost_options.0);
                            let (slot, prior) = match storage.change_acl(
                                entry.object,
                                dependent_grantee,
                                dependent_grantor,
                                dependent_privileges.without(lost_options),
                                dependent_options.without(lost_options),
                                txn.txid,
                            ) {
                                Ok(change) => change,
                                Err(error) => return sql_fail(error),
                            };
                            if let Err(error) =
                                txn.record_ddl(super::txn::DdlUndo::ObjectAclChanged {
                                    slot: slot as u32,
                                    prior,
                                })
                            {
                                storage.restore_acl_pending(slot, prior);
                                return sql_fail(error);
                            }
                            if dependent_grantee != PUBLIC_ROLE && recursively_lost.0 != 0 {
                                if queue_len == queue_roles.len() {
                                    return sql_fail(sql_err!(
                                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                        "privilege dependency graph exceeds {} entries",
                                        queue_roles.len()
                                    ));
                                }
                                queue_roles[queue_len] = dependent_grantee;
                                queue_privileges[queue_len] = recursively_lost;
                                queue_len += 1;
                            }
                        }
                    }
                }
            }
            let new_privileges = if grant_option_only {
                old_privileges
            } else {
                old_privileges.without(requested)
            };
            let (slot, prior) = match storage.change_acl(
                *object,
                grantee,
                acl_grantor as u16,
                new_privileges,
                old_options.without(requested),
                txn.txid,
            ) {
                Ok(change) => change,
                Err(error) => return sql_fail(error),
            };
            if let Err(error) = txn.record_ddl(super::txn::DdlUndo::ObjectAclChanged {
                slot: slot as u32,
                prior,
            }) {
                storage.restore_acl_pending(slot, prior);
                return sql_fail(error);
            }
        }
    }
    responder.command_complete("REVOKE")?;
    sql_ok()
}

fn acl_grantor(
    storage: &Storage,
    object: crate::storage::AccessObject,
    current: usize,
    txid: u32,
) -> usize {
    if storage.role(current).attributes_to(txid).superuser {
        storage.object_owner(object, txid)
    } else {
        current
    }
}

/// One object a DROP SCHEMA sweeps up, for dependency reports and the
/// cascaded drops.
#[derive(Clone, Copy)]
enum SchemaObject {
    Table(usize),
    View(usize),
    Matview {
        table: usize,
        catalog: usize,
    },
    Sequence(usize),
    Domain(usize),
    Enum(usize),
    /// An inbound foreign key on a table that itself survives.
    InboundFk {
        table: usize,
        fk_index: usize,
    },
}

/// DROP SCHEMA [IF EXISTS] name [, ...] [CASCADE | RESTRICT]: RESTRICT (the
/// default) refuses a non-empty schema with PostgreSQL's dependency report;
/// CASCADE drops every contained catalog object and severs inbound foreign
/// keys from surviving tables.
#[allow(clippy::too_many_arguments)]
pub fn drop_schema(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    scratch: &mut FixedVec<(u64, RowHome)>,
    names: &[&str],
    if_exists: bool,
    cascade: bool,
    arena: &Arena,
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
) -> Outcome {
    use core::fmt::Write as _;
    const MAX_DROP_SCHEMAS: usize = 16;
    let mut slots: [usize; MAX_DROP_SCHEMAS] = [0; MAX_DROP_SCHEMAS];
    let mut n_slots = 0usize;
    for name in names {
        if *name == "pg_catalog" || *name == "information_schema" {
            return sql_fail(sql_err!(
                crate::sql::eval::sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop schema {} because it is required by the database system",
                name
            ));
        }
        match storage.find_schema_visible(name, txn.txid) {
            Some(slot) => {
                if let Err(error) = storage.require_owner(
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::Schema,
                        slot: slot as u16,
                    },
                    txn.txid,
                    "schema",
                ) {
                    return sql_fail(error);
                }
                if !slots[..n_slots].contains(&slot) {
                    if n_slots == MAX_DROP_SCHEMAS {
                        return sql_fail(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "too many schemas in one DROP SCHEMA"
                        ));
                    }
                    slots[n_slots] = slot;
                    n_slots += 1;
                }
            }
            None if if_exists => {
                responder.notice(
                    crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(128, "schema \"{}\" does not exist, skipping", name).as_str(),
                )?;
            }
            None => {
                return sql_fail(sql_err!(
                    sqlstate::INVALID_SCHEMA_NAME,
                    "schema \"{}\" does not exist",
                    name
                ));
            }
        }
    }
    // Everything the drop sweeps up, across all listed schemas.
    let in_listed = |storage: &Storage, schema: &str| {
        slots[..n_slots]
            .iter()
            .any(|&slot| storage.schema_def(slot).name.as_str() == schema)
    };
    let mut objects: [Option<SchemaObject>; 64] = [const { None }; 64];
    let mut n_objects = 0usize;
    let mut push = |o: SchemaObject, n_objects: &mut usize| -> Result<(), SqlError> {
        if *n_objects == objects.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "DROP SCHEMA sweeps up more than {} objects",
                64
            ));
        }
        objects[*n_objects] = Some(o);
        *n_objects += 1;
        Ok(())
    };
    for t in 0..storage.table_count() {
        let def = storage.table_def(t, txn.txid);
        if !storage.table(t).visible_to(txn.txid) || !in_listed(storage, def.schema.as_str()) {
            continue;
        }
        let def = *def;
        let object = match storage.matview_slot(def.schema.as_str(), def.name.as_str(), txn.txid) {
            Some(catalog) => SchemaObject::Matview { table: t, catalog },
            None => SchemaObject::Table(t),
        };
        if let Err(e) = push(object, &mut n_objects) {
            return sql_fail(e);
        }
    }
    for v in 0..storage.view_count() {
        if storage.view(v).visible_to(txn.txid)
            && in_listed(storage, storage.view(v).schema.as_str())
            && let Err(e) = push(SchemaObject::View(v), &mut n_objects)
        {
            return sql_fail(e);
        }
    }
    for sequence in 0..storage.sequence_count() {
        let sequence_definition = storage.sequence_for(sequence, txn.txid);
        if sequence_definition.visible_to(txn.txid)
            && in_listed(storage, sequence_definition.schema.as_str())
            && sequence_definition.owner.is_none()
            && let Err(e) = push(SchemaObject::Sequence(sequence), &mut n_objects)
        {
            return sql_fail(e);
        }
    }
    let mut schema_domains = [false; crate::storage::MAX_DOMAINS];
    for (domain, selected) in schema_domains
        .iter_mut()
        .enumerate()
        .take(storage.domain_count())
    {
        if storage.domain(domain).visible_to(txn.txid)
            && in_listed(storage, storage.domain(domain).schema.as_str())
        {
            *selected = true;
            if let Err(e) = push(SchemaObject::Domain(domain), &mut n_objects) {
                return sql_fail(e);
            }
        }
    }
    for enumeration in 0..storage.enum_count() {
        if storage.enum_for(enumeration, txn.txid).visible_to(txn.txid)
            && in_listed(
                storage,
                storage.enum_for(enumeration, txn.txid).schema.as_str(),
            )
            && let Err(e) = push(SchemaObject::Enum(enumeration), &mut n_objects)
        {
            return sql_fail(e);
        }
    }
    if cascade {
        // Domains outside the schema can depend on a domain or enum inside it.
        // They are catalog-only dependents, so include their entire bounded
        // closure rather than leaving a dangling base type.
        for domain_slot in 0..storage.domain_count() {
            let domain = storage.domain(domain_slot);
            if !domain.visible_to(txn.txid) || schema_domains[domain_slot] {
                continue;
            }
            let parent_in_schema = (0..storage.domain_count()).any(|parent| {
                schema_domains[parent] && domain_depends_on(storage, domain_slot, parent, txn.txid)
            });
            let enum_in_schema = match domain.base {
                ColType::Enum(slot) | ColType::Array(super::types::ArrElem::Enum(slot)) => {
                    in_listed(
                        storage,
                        storage.enum_for(slot as usize, txn.txid).schema.as_str(),
                    )
                }
                _ => false,
            };
            if (parent_in_schema || enum_in_schema)
                && let Err(error) = push(SchemaObject::Domain(domain_slot), &mut n_objects)
            {
                return sql_fail(error);
            }
        }
        loop {
            let mut dependent = None;
            for table in 0..storage.table_count() {
                if !storage.table(table).visible_to(txn.txid) {
                    continue;
                }
                let def = storage.table_def(table, txn.txid);
                if in_listed(storage, def.schema.as_str()) {
                    continue;
                }
                if !def.columns().iter().any(|column| {
                    if column
                        .user_type
                        .is_some_and(|identity| in_listed(storage, identity.schema.as_str()))
                    {
                        return true;
                    }
                    let Some(identity) = column.user_type else {
                        return false;
                    };
                    let Some(domain_slot) = storage.domain_slot(
                        identity.schema.as_str(),
                        identity.name.as_str(),
                        txn.txid,
                    ) else {
                        return false;
                    };
                    let parent_domain_in_schema = (0..storage.domain_count()).any(|parent| {
                        schema_domains[parent]
                            && domain_depends_on(storage, domain_slot, parent, txn.txid)
                    });
                    let base_enum_in_schema = match storage.domain(domain_slot).base {
                        ColType::Enum(slot) | ColType::Array(super::types::ArrElem::Enum(slot)) => {
                            in_listed(
                                storage,
                                storage.enum_for(slot as usize, txn.txid).schema.as_str(),
                            )
                        }
                        _ => false,
                    };
                    parent_domain_in_schema || base_enum_in_schema
                }) {
                    continue;
                }
                dependent = Some(table);
                break;
            }
            let Some(table_slot) = dependent else {
                break;
            };
            let def = storage.table_def(table_slot, txn.txid);
            let (table_schema, table) = (def.schema, def.name);
            let mut columns = [SqlName::EMPTY; MAX_COLUMNS];
            let mut column_count = 0;
            for column in def.columns() {
                let directly_in_schema = column
                    .user_type
                    .is_some_and(|identity| in_listed(storage, identity.schema.as_str()));
                let through_domain = column
                    .user_type
                    .and_then(|identity| {
                        storage
                            .domain_slot(identity.schema.as_str(), identity.name.as_str(), txn.txid)
                            .map(|domain_slot| {
                                let parent_in_schema = (0..storage.domain_count()).any(|parent| {
                                    schema_domains[parent]
                                        && domain_depends_on(storage, domain_slot, parent, txn.txid)
                                });
                                let enum_in_schema = match storage.domain(domain_slot).base {
                                    ColType::Enum(slot)
                                    | ColType::Array(super::types::ArrElem::Enum(slot)) => {
                                        in_listed(
                                            storage,
                                            storage
                                                .enum_for(slot as usize, txn.txid)
                                                .schema
                                                .as_str(),
                                        )
                                    }
                                    _ => false,
                                };
                                parent_in_schema || enum_in_schema
                            })
                    })
                    .unwrap_or(false);
                if directly_in_schema || through_domain {
                    columns[column_count] = column.name;
                    column_count += 1;
                }
            }
            if let Err(error) = cascade_drop_type_column(
                storage,
                wal,
                txn,
                scratch,
                table_schema,
                table,
                &columns[..column_count],
                arena,
                seq_session,
                responder,
            ) {
                return sql_fail(error);
            }
        }
        // Stored queries outside the dropped schemas follow their resolved
        // dependencies transitively, exactly as pg_depend drives CASCADE.
        if n_objects > 0 {
            let closure = stored_query_dependent_closure(storage, txn.txid, |dependency| {
                in_listed(storage, dependency.schema.as_str())
            });
            let (dependent_views, dependent_matviews) = match closure {
                Ok(closure) => closure,
                Err(error) => return sql_fail(error),
            };
            for (view_slot, &is_dependent) in dependent_views
                .iter()
                .enumerate()
                .take(storage.view_count())
            {
                let view = storage.view(view_slot);
                if is_dependent
                    && view.visible_to(txn.txid)
                    && !in_listed(storage, view.schema.as_str())
                    && let Err(error) = push(SchemaObject::View(view_slot), &mut n_objects)
                {
                    return sql_fail(error);
                }
            }
            for (matview_slot, &is_dependent) in dependent_matviews
                .iter()
                .enumerate()
                .take(storage.matview_count())
            {
                let matview = storage.matview(matview_slot);
                if !is_dependent
                    || !matview.visible_to(txn.txid)
                    || in_listed(storage, matview.schema.as_str())
                {
                    continue;
                }
                let Some(table) =
                    storage.find_visible(matview.schema.as_str(), matview.name.as_str(), txn.txid)
                else {
                    return sql_fail(undefined_kind("materialized view", matview.name.as_str()));
                };
                if let Err(error) = push(
                    SchemaObject::Matview {
                        table,
                        catalog: matview_slot,
                    },
                    &mut n_objects,
                ) {
                    return sql_fail(error);
                }
            }
        }
    }
    // Inbound foreign keys: a surviving table referencing a dropped one loses
    // the constraint (PostgreSQL drops the constraint, not the table).
    for t in 0..storage.table_count() {
        let def = storage.table_def(t, txn.txid);
        if !storage.table(t).visible_to(txn.txid) || in_listed(storage, def.schema.as_str()) {
            continue;
        }
        for f in 0..def.n_fkeys {
            if in_listed(storage, def.fkeys[f].parent_schema.as_str())
                && let Err(e) = push(
                    SchemaObject::InboundFk {
                        table: t,
                        fk_index: f,
                    },
                    &mut n_objects,
                )
            {
                return sql_fail(e);
            }
        }
    }
    // PostgreSQL walks the listed schemas in reverse, and reports each
    // schema's dependents in OID (creation) order; a severed constraint
    // sorts with its child table, after it.
    let schema_rank = |storage: &Storage, schema: &str| -> usize {
        slots[..n_slots]
            .iter()
            .position(|&slot| storage.schema_def(slot).name.as_str() == schema)
            .map(|p| n_slots - p)
            .unwrap_or(0)
    };
    let sort_key = |storage: &Storage, o: &SchemaObject| -> (usize, u64, u8) {
        match o {
            SchemaObject::Table(t) => {
                let table = storage.table(*t);
                let def = storage.table_def(*t, txn.txid);
                (
                    schema_rank(storage, def.schema.as_str()),
                    table.created_at,
                    0,
                )
            }
            SchemaObject::View(v) => {
                let view = storage.view(*v);
                (
                    schema_rank(storage, view.schema.as_str()),
                    view.created_at,
                    0,
                )
            }
            SchemaObject::Matview { table, .. } => {
                let table_state = storage.table(*table);
                let def = storage.table_def(*table, txn.txid);
                (
                    schema_rank(storage, def.schema.as_str()),
                    table_state.created_at,
                    0,
                )
            }
            SchemaObject::Sequence(sequence) => {
                let sequence = storage.sequence_for(*sequence, txn.txid);
                (
                    schema_rank(storage, sequence.schema.as_str()),
                    sequence.created_at,
                    0,
                )
            }
            SchemaObject::Domain(domain) => {
                let domain = storage.domain(*domain);
                (
                    schema_rank(storage, domain.schema.as_str()),
                    domain.created_at,
                    0,
                )
            }
            SchemaObject::Enum(enumeration) => {
                let enumeration = storage.enum_for(*enumeration, txn.txid);
                (
                    schema_rank(storage, enumeration.schema.as_str()),
                    enumeration.created_at,
                    0,
                )
            }
            SchemaObject::InboundFk { table, fk_index } => {
                let child = storage.table(*table);
                let def = storage.table_def(*table, txn.txid);
                (
                    schema_rank(storage, def.fkeys[*fk_index].parent_schema.as_str()),
                    child.created_at,
                    1,
                )
            }
        }
    };
    {
        let slice = &mut objects[..n_objects];
        slice.sort_unstable_by_key(|o| sort_key(storage, o.as_ref().expect("filled")));
    }
    // Renders one swept object the way PostgreSQL's dependency report does:
    // qualified only when its schema is not on the current search path.
    let in_path = |storage: &Storage, schema: &str| {
        storage.path().entries().iter().any(|e| match e {
            crate::storage::PathEntry::Schema(slot) => {
                storage.schema_def(*slot as usize).name.as_str() == schema
            }
            crate::storage::PathEntry::Catalog => schema == "pg_catalog",
        })
    };
    let describe = |storage: &Storage, o: &SchemaObject, out: &mut crate::util::StackStr<192>| {
        let write_rel = |out: &mut crate::util::StackStr<192>, schema: &SqlName, name: &SqlName| {
            if in_path(storage, schema.as_str()) {
                let _ = write!(out, "{}", name.as_str());
            } else {
                let _ = write!(out, "{}.{}", schema.as_str(), name.as_str());
            }
        };
        match o {
            SchemaObject::Table(t) => {
                let def = storage.table_def(*t, txn.txid);
                let _ = write!(out, "table ");
                write_rel(out, &def.schema, &def.name);
            }
            SchemaObject::View(v) => {
                let view = storage.view(*v);
                let _ = write!(out, "view ");
                write_rel(out, &view.schema, &view.name);
            }
            SchemaObject::Matview { table, .. } => {
                let def = storage.table_def(*table, txn.txid);
                let _ = write!(out, "materialized view ");
                write_rel(out, &def.schema, &def.name);
            }
            SchemaObject::Sequence(sequence) => {
                let sequence = storage.sequence_for(*sequence, txn.txid);
                let _ = write!(out, "sequence ");
                write_rel(out, &sequence.schema, &sequence.name);
            }
            SchemaObject::Domain(domain) => {
                let domain = storage.domain(*domain);
                let _ = write!(out, "type ");
                write_rel(out, &domain.schema, &domain.name);
            }
            SchemaObject::Enum(enumeration) => {
                let enumeration = storage.enum_for(*enumeration, txn.txid);
                let _ = write!(out, "type ");
                write_rel(out, &enumeration.schema, &enumeration.name);
            }
            SchemaObject::InboundFk { table, fk_index } => {
                let def = storage.table_def(*table, txn.txid);
                let _ = write!(
                    out,
                    "constraint {} on table ",
                    def.fkeys[*fk_index].name.as_str()
                );
                write_rel(out, &def.schema, &def.name);
            }
        }
    };
    if n_objects > 0 && !cascade {
        let first = slots[0];
        let mut detail =
            crate::util::StackStr::<{ crate::sql::eval::MAX_DIAGNOSTIC_DETAIL_BYTES }>::new();
        for (i, o) in objects[..n_objects].iter().flatten().enumerate() {
            let mut line = crate::util::StackStr::<192>::new();
            describe(storage, o, &mut line);
            let schema = match o {
                SchemaObject::Table(t) => storage.table_def(*t, txn.txid).schema,
                SchemaObject::View(v) => storage.view(*v).schema,
                SchemaObject::Matview { table, .. } => storage.table_def(*table, txn.txid).schema,
                SchemaObject::Sequence(sequence) => {
                    storage.sequence_for(*sequence, txn.txid).schema
                }
                SchemaObject::Domain(domain) => storage.domain(*domain).schema,
                SchemaObject::Enum(enumeration) => storage.enum_for(*enumeration, txn.txid).schema,
                SchemaObject::InboundFk { table, fk_index } => {
                    storage.table_def(*table, txn.txid).fkeys[*fk_index].parent_schema
                }
            };
            let _ = write!(
                detail,
                "{}{} depends on schema {}",
                if i > 0 { "\n" } else { "" },
                line.as_str(),
                schema.as_str(),
            );
        }
        let mut hint = crate::util::StackStr::<128>::new();
        let _ = write!(
            hint,
            "Use DROP ... CASCADE to drop the dependent objects too."
        );
        crate::sql::eval::stash_diagnostic(detail, Some(hint));
        return sql_fail(sql_err!(
            crate::sql::eval::sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
            "cannot drop schema {} because other objects depend on it",
            storage.schema_def(first).name.as_str()
        ));
    }
    if n_objects == 1 {
        let mut line = crate::util::StackStr::<192>::new();
        describe(storage, objects[0].as_ref().expect("counted"), &mut line);
        responder.notice(
            crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
            stack_format!(224, "drop cascades to {}", line.as_str()).as_str(),
        )?;
    } else if n_objects > 1 {
        let mut detail =
            crate::util::StackStr::<{ crate::sql::eval::MAX_DIAGNOSTIC_DETAIL_BYTES }>::new();
        for (i, o) in objects[..n_objects].iter().flatten().enumerate() {
            let mut line = crate::util::StackStr::<192>::new();
            describe(storage, o, &mut line);
            let _ = write!(
                detail,
                "{}drop cascades to {}",
                if i > 0 { "\n" } else { "" },
                line.as_str()
            );
        }
        crate::sql::eval::stash_diagnostic(detail, None);
        responder.notice(
            crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
            stack_format!(128, "drop cascades to {} other objects", n_objects).as_str(),
        )?;
    }
    let owned_sequence_undo = objects[..n_objects]
        .iter()
        .flatten()
        .filter_map(|object| match object {
            SchemaObject::Table(table) => Some(*storage.table_def(*table, txn.txid)),
            _ => None,
        })
        .map(|table| {
            (0..storage.sequence_count())
                .filter(|sequence_slot| {
                    let sequence = storage.sequence_for(*sequence_slot, txn.txid);
                    sequence.visible_to(txn.txid)
                        && matches!(
                            sequence.owner,
                            Some(owner)
                                if owner.table_schema == table.schema
                                    && owner.table == table.name
                        )
                })
                .count()
        })
        .sum::<usize>();
    let undo_needed = n_objects
        + objects[..n_objects]
            .iter()
            .flatten()
            .filter(|object| matches!(object, SchemaObject::Matview { .. }))
            .count()
        + owned_sequence_undo
        + n_slots;
    if txn.ddl().len() + undo_needed > super::txn::MAX_TXN_DDL {
        return sql_fail(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "DROP SCHEMA needs {} DDL undo entries but only {} remain",
            undo_needed,
            super::txn::MAX_TXN_DDL - txn.ddl().len()
        ));
    }
    // Apply the preflighted catalog plan in its deterministic creation order;
    // every mutation is journaled before the containing schemas are removed.
    for o in objects[..n_objects].iter().flatten() {
        match o {
            SchemaObject::InboundFk { table, fk_index } => {
                let def = *storage.table_def(*table, txn.txid);
                let fk_name = def.fkeys[*fk_index].name;
                let (schema, tname) = (def.schema, def.name);
                let lsn = storage.bump_lsn();
                if let Err(e) = wal.stage(
                    txn.txid,
                    lsn,
                    &WalOp::DropTableFk {
                        schema: schema.as_str(),
                        table: tname.as_str(),
                        fk_name: fk_name.as_str(),
                    },
                ) {
                    return sql_fail(e);
                }
                let mut updated = def;
                if !drop_named_constraint(&mut updated, fk_name.as_str()) {
                    continue;
                }
                let mut identity_mapping = [None; MAX_COLUMNS];
                for (column, target) in identity_mapping.iter_mut().enumerate().take(def.n_columns)
                {
                    *target = Some(def.columns()[column].name);
                }
                if let Err(error) =
                    storage.write_table_def(*table, txn.txid, updated, &identity_mapping, false)
                {
                    return sql_fail(error);
                }
                if let Err(error) = txn.record_ddl(super::txn::DdlUndo::TableAltered(*table as u32))
                {
                    storage.rollback_table_def(*table, txn.txid);
                    return sql_fail(error);
                }
            }
            SchemaObject::View(v) => {
                let (schema, vname) = {
                    let view = storage.view(*v);
                    (view.schema, view.name)
                };
                let lsn = storage.bump_lsn();
                if let Err(e) = wal.stage(
                    txn.txid,
                    lsn,
                    &WalOp::DropView {
                        schema: schema.as_str(),
                        name: vname.as_str(),
                    },
                ) {
                    return sql_fail(e);
                }
                let dropped = match storage.drop_view(schema.as_str(), vname.as_str(), txn.txid) {
                    Ok(d) => d,
                    Err(e) => return sql_fail(e),
                };
                if let Some(slot) = dropped
                    && let Err(e) = txn.record_ddl(super::txn::DdlUndo::ViewDropped(slot as u32))
                {
                    return sql_fail(e);
                }
            }
            SchemaObject::Matview { table, catalog } => {
                let def = *storage.table_def(*table, txn.txid);
                let lsn = storage.bump_lsn();
                if let Err(error) = wal.stage(
                    txn.txid,
                    lsn,
                    &WalOp::DropTable {
                        schema: def.schema.as_str(),
                        name: def.name.as_str(),
                    },
                ) {
                    return sql_fail(error);
                }
                if let Err(error) = txn.record_ddl(super::txn::DdlUndo::Dropped(*table as u32)) {
                    return sql_fail(error);
                }
                storage.drop_table_in(*table, txn.txid);
                storage.drop_indexes_for(def.schema.as_str(), def.name.as_str(), txn.txid);
                let lsn = storage.bump_lsn();
                if let Err(error) = wal.stage(
                    txn.txid,
                    lsn,
                    &WalOp::DropMatview {
                        schema: def.schema.as_str(),
                        name: def.name.as_str(),
                    },
                ) {
                    return sql_fail(error);
                }
                match storage.drop_matview(def.schema.as_str(), def.name.as_str(), txn.txid) {
                    Ok(Some(slot)) => {
                        debug_assert_eq!(slot, *catalog);
                        if let Err(error) =
                            txn.record_ddl(super::txn::DdlUndo::MatviewDropped(slot as u32))
                        {
                            return sql_fail(error);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => return sql_fail(error),
                }
            }
            SchemaObject::Sequence(sequence) => {
                let sequence = storage.sequence_for(*sequence, txn.txid);
                let (schema, name) = (sequence.schema, sequence.name);
                let lsn = storage.bump_lsn();
                if let Err(error) = wal.stage(
                    txn.txid,
                    lsn,
                    &WalOp::DropSequence {
                        schema: schema.as_str(),
                        name: name.as_str(),
                    },
                ) {
                    return sql_fail(error);
                }
                match storage.drop_sequence(schema.as_str(), name.as_str(), txn.txid) {
                    Ok(Some(slot)) => {
                        if let Err(error) =
                            txn.record_ddl(super::txn::DdlUndo::SequenceDropped(slot as u32))
                        {
                            return sql_fail(error);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => return sql_fail(error),
                }
            }
            SchemaObject::Domain(domain) => {
                let domain = storage.domain(*domain);
                let (schema, name) = (domain.schema, domain.name);
                let lsn = storage.bump_lsn();
                if let Err(error) = wal.stage(
                    txn.txid,
                    lsn,
                    &WalOp::DropDomain {
                        schema: schema.as_str(),
                        name: name.as_str(),
                    },
                ) {
                    return sql_fail(error);
                }
                match storage.drop_domain(schema.as_str(), name.as_str(), txn.txid) {
                    Ok(Some(slot)) => {
                        if let Err(error) =
                            txn.record_ddl(super::txn::DdlUndo::DomainDropped(slot as u32))
                        {
                            return sql_fail(error);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => return sql_fail(error),
                }
            }
            SchemaObject::Enum(enumeration) => {
                let enumeration = storage.enum_for(*enumeration, txn.txid);
                let (schema, name) = (enumeration.schema, enumeration.name);
                let lsn = storage.bump_lsn();
                if let Err(error) = wal.stage(
                    txn.txid,
                    lsn,
                    &WalOp::DropEnum {
                        schema: schema.as_str(),
                        name: name.as_str(),
                    },
                ) {
                    return sql_fail(error);
                }
                match storage.drop_enum(schema.as_str(), name.as_str(), txn.txid) {
                    Ok(Some(slot)) => {
                        if let Err(error) =
                            txn.record_ddl(super::txn::DdlUndo::EnumDropped(slot as u32))
                        {
                            return sql_fail(error);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => return sql_fail(error),
                }
            }
            SchemaObject::Table(t) => {
                if let Some(other) = storage.table(*t).ddl_locked_by_other(txn.txid) {
                    if let Err(error) = storage.wait_for_transaction(txn.txid, other) {
                        return sql_fail(error);
                    }
                    return sql_fail(sql_err!(
                        crate::sql::eval::sqlstate::INTERNAL_LOCK_WAIT,
                        "statement is waiting for concurrent DDL on \"{}\"",
                        storage.table_def(*t, txn.txid).name.as_str()
                    ));
                }
                let def = *storage.table_def(*t, txn.txid);
                for sequence_slot in 0..storage.sequence_count() {
                    let sequence = storage.sequence_for(sequence_slot, txn.txid);
                    if !sequence.visible_to(txn.txid)
                        || !matches!(
                            sequence.owner,
                            Some(owner)
                                if owner.table_schema == def.schema
                                    && owner.table == def.name
                        )
                    {
                        continue;
                    }
                    let (schema, name) = (sequence.schema, sequence.name);
                    let lsn = storage.bump_lsn();
                    if let Err(error) = wal.stage(
                        txn.txid,
                        lsn,
                        &WalOp::DropSequence {
                            schema: schema.as_str(),
                            name: name.as_str(),
                        },
                    ) {
                        return sql_fail(error);
                    }
                    match storage.drop_sequence(schema.as_str(), name.as_str(), txn.txid) {
                        Ok(Some(slot)) => {
                            if let Err(error) =
                                txn.record_ddl(super::txn::DdlUndo::SequenceDropped(slot as u32))
                            {
                                return sql_fail(error);
                            }
                        }
                        Ok(None) => {}
                        Err(error) => return sql_fail(error),
                    }
                }
                let lsn = storage.bump_lsn();
                if let Err(e) = wal.stage(
                    txn.txid,
                    lsn,
                    &WalOp::DropTable {
                        schema: def.schema.as_str(),
                        name: def.name.as_str(),
                    },
                ) {
                    return sql_fail(e);
                }
                if let Err(e) = txn.record_ddl(super::txn::DdlUndo::Dropped(*t as u32)) {
                    return sql_fail(e);
                }
                storage.drop_table_in(*t, txn.txid);
                storage.drop_indexes_for(def.schema.as_str(), def.name.as_str(), txn.txid);
            }
        }
    }
    for &slot in &slots[..n_slots] {
        if let Err(error) = remove_schema_from_publications(storage, wal, txn, slot as u8) {
            return sql_fail(error);
        }
        let name = storage.schema_def(slot).name;
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.stage(txn.txid, lsn, &WalOp::DropSchema(name.as_str())) {
            return sql_fail(e);
        }
        if let Err(e) = txn.record_ddl(super::txn::DdlUndo::SchemaDropped(slot as u32)) {
            return sql_fail(e);
        }
        storage.drop_schema_in(slot, txn.txid);
    }
    responder.command_complete("DROP SCHEMA")?;
    sql_ok()
}

fn remove_schema_from_publications(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    schema_slot: u8,
) -> Result<(), SqlError> {
    while let Some((name, mut definition)) =
        storage.publication_selecting_schema(schema_slot, txn.txid)
    {
        let index = definition.schemas[..definition.schema_count]
            .iter()
            .position(|member| *member == schema_slot)
            .expect("selected schema");
        definition
            .schemas
            .copy_within(index + 1..definition.schema_count, index);
        definition.schema_count -= 1;
        definition.schemas[definition.schema_count] = u8::MAX;
        let (slot, prior) = storage.alter_publication(name.as_str(), definition, txn.txid)?;
        let lsn = storage.bump_lsn();
        if let Err(error) = wal.stage(
            txn.txid,
            lsn,
            &WalOp::AlterPublication {
                name: name.as_str(),
                all_tables: definition.all_tables,
                tables: definition.tables,
                table_count: definition.table_count,
                schemas: definition.schemas,
                schema_count: definition.schema_count,
                publish_insert: definition.publish_insert,
                publish_update: definition.publish_update,
                publish_delete: definition.publish_delete,
                publish_truncate: definition.publish_truncate,
            },
        ) {
            storage.rollback_publication_alter(slot, prior);
            return Err(error);
        }
        if let Err(error) = txn.record_ddl(super::txn::DdlUndo::PublicationAltered {
            slot: slot as u32,
            prior,
        }) {
            storage.rollback_publication_alter(slot, prior);
            return Err(error);
        }
    }
    Ok(())
}

/// CREATE PUBLICATION records a transaction-owned catalog entry and its
/// complete initial selection before the commit record makes it visible.
#[allow(clippy::too_many_arguments)]
pub fn create_publication(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut super::txn::TxnState,
    name: &str,
    all_tables: bool,
    tables: &[QualName],
    schemas: &[&str],
    publish: crate::sql::ast::PublicationOperations,
    responder: &mut Responder,
) -> Outcome {
    if all_tables && (!tables.is_empty() || !schemas.is_empty()) {
        return sql_fail(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "FOR ALL TABLES cannot name tables"
        ));
    }
    let mut members = [u16::MAX; crate::storage::MAX_PUBLICATION_TABLES];
    for (index, table) in tables.iter().enumerate() {
        let Some(crate::storage::ResolvedRelation::Table(slot)) =
            storage.resolve_relation(table.schema, table.name, txn.txid)
        else {
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "relation \"{}\" does not exist",
                table.name
            ));
        };
        if let Err(error) = storage.require_owner(
            storage.table_access_object(slot, txn.txid),
            txn.txid,
            "table",
        ) {
            return sql_fail(error);
        }
        if members[..index].contains(&(slot as u16)) {
            return sql_fail(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "relation \"{}\" is already in publication",
                table.name
            ));
        }
        members[index] = slot as u16;
    }
    let schema_members = match publication_schemas(storage, txn.txid, schemas) {
        Ok(schemas) => schemas,
        Err(error) => return sql_fail(error),
    };
    let name = match SqlName::parse(name) {
        Ok(name) => name,
        Err(error) => return sql_fail(error),
    };
    match storage.create_publication(
        crate::storage::PublicationSpec {
            name,
            all_tables,
            tables: &members[..tables.len()],
            schemas: &schema_members[..schemas.len()],
            publish_insert: publish.insert,
            publish_update: publish.update,
            publish_delete: publish.delete,
            publish_truncate: publish.truncate,
        },
        txn.txid,
    ) {
        Ok(slot) => {
            let owner = storage.publication_owner(slot, txn.txid);
            let lsn = storage.bump_lsn();
            if let Err(error) = wal.stage(
                txn.txid,
                lsn,
                &WalOp::CreatePublication {
                    name: name.as_str(),
                    owner,
                    all_tables,
                    tables: members,
                    table_count: tables.len(),
                    schemas: schema_members,
                    schema_count: schemas.len(),
                    publish_insert: publish.insert,
                    publish_update: publish.update,
                    publish_delete: publish.delete,
                    publish_truncate: publish.truncate,
                },
            ) {
                storage.rollback_publication_create(slot);
                return sql_fail(error);
            }
            match txn.record_ddl(super::txn::DdlUndo::PublicationCreated(slot as u32)) {
                Ok(()) => Ok(Ok(responder.command_complete("CREATE PUBLICATION")?)),
                Err(error) => {
                    storage.rollback_publication_create(slot);
                    sql_fail(error)
                }
            }
        }
        Err(error) => sql_fail(error),
    }
}

fn publication_schemas(
    storage: &Storage,
    txid: u32,
    schemas: &[&str],
) -> Result<[u8; crate::storage::MAX_SCHEMAS], SqlError> {
    let mut members = [u8::MAX; crate::storage::MAX_SCHEMAS];
    if schemas.is_empty() {
        return Ok(members);
    }
    let role = storage.current_role_slot(txid).ok_or_else(|| {
        sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "current role is not present in the role catalog"
        )
    })?;
    if txid != 0 && !storage.role(role).attributes_to(txid).superuser {
        return Err(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "must be superuser to publish schemas"
        ));
    }
    for (index, name) in schemas.iter().enumerate() {
        let Some(slot) = storage.find_schema_visible(name, txid) else {
            return Err(sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "schema \"{}\" does not exist",
                name
            ));
        };
        if members[..index].contains(&(slot as u8)) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "schema \"{}\" is listed more than once",
                name
            ));
        }
        members[index] = slot as u8;
    }
    Ok(members)
}

pub fn drop_publication(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut super::txn::TxnState,
    names: &[&str],
    if_exists: bool,
    responder: &mut Responder,
) -> Outcome {
    for name in names {
        let Some((slot, _)) = storage.publication_definition(name, txn.txid) else {
            if if_exists {
                responder.notice(
                    sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(128, "publication \"{}\" does not exist, skipping", name)
                        .as_str(),
                )?;
                continue;
            }
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "publication \"{}\" does not exist",
                name
            ));
        };
        if let Err(error) = storage.require_publication_owner(slot, txn.txid) {
            return sql_fail(error);
        }
        match storage.drop_publication(name, txn.txid) {
            Ok(Some(slot)) => {
                let lsn = storage.bump_lsn();
                if let Err(error) = wal.stage(txn.txid, lsn, &WalOp::DropPublication { name }) {
                    storage.rollback_publication_drop(slot, txn.txid);
                    return sql_fail(error);
                }
                if let Err(error) =
                    txn.record_ddl(super::txn::DdlUndo::PublicationDropped(slot as u32))
                {
                    storage.rollback_publication_drop(slot, txn.txid);
                    return sql_fail(error);
                }
            }
            Ok(None) => {
                return sql_fail(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "publication disappeared before DROP PUBLICATION completed"
                ));
            }
            Err(error) => return sql_fail(error),
        }
    }
    Ok(Ok(responder.command_complete("DROP PUBLICATION")?))
}

/// Applies one fully parsed publication definition change.  The storage layer
/// stages it by transaction id; WAL carries the complete resulting definition
/// so recovery has no dependence on the prior catalog image.
pub fn alter_publication(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut super::txn::TxnState,
    name: &str,
    action: crate::sql::ast::AlterPublicationAction<'_>,
    responder: &mut Responder,
) -> Outcome {
    let Some((slot, current)) = storage.publication_definition(name, txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "publication \"{}\" does not exist",
            name
        ));
    };
    if let Err(error) = storage.require_publication_owner(slot, txn.txid) {
        return sql_fail(error);
    }
    let mut definition = current;
    match action {
        crate::sql::ast::AlterPublicationAction::Rename(new_name) => {
            let new_name = match SqlName::parse(new_name) {
                Ok(name) => name,
                Err(error) => return sql_fail(error),
            };
            let prior = match storage.rename_publication(slot, new_name, txn.txid) {
                Ok(prior) => prior,
                Err(error) => return sql_fail(error),
            };
            let lsn = storage.bump_lsn();
            if let Err(error) = wal.stage(
                txn.txid,
                lsn,
                &WalOp::RenamePublication {
                    name,
                    new_name: new_name.as_str(),
                },
            ) {
                storage.rollback_publication_rename(slot, prior);
                return sql_fail(error);
            }
            if let Err(error) = txn.record_ddl(super::txn::DdlUndo::PublicationRenamed {
                slot: slot as u32,
                prior,
            }) {
                storage.rollback_publication_rename(slot, prior);
                return sql_fail(error);
            }
            return Ok(Ok(responder.command_complete("ALTER PUBLICATION")?));
        }
        crate::sql::ast::AlterPublicationAction::SetOwner(role_name) => {
            let role_name = resolve_role_name(role_name);
            let Some(new_owner) = storage.find_role_visible(role_name.as_str(), txn.txid) else {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "role \"{}\" does not exist",
                    role_name.as_str()
                ));
            };
            let Some(current_role) = storage.current_role_slot(txn.txid) else {
                return sql_fail(sql_err!(
                    sqlstate::INSUFFICIENT_PRIVILEGE,
                    "current role is not present in the role catalog"
                ));
            };
            let superuser = storage.role(current_role).attributes_to(txn.txid).superuser;
            if !superuser
                && current_role != new_owner
                && !storage.role_can_set(current_role, new_owner, txn.txid)
            {
                return sql_fail(sql_err!(
                    sqlstate::INSUFFICIENT_PRIVILEGE,
                    "must be able to SET ROLE \"{}\"",
                    role_name.as_str()
                ));
            }
            if !superuser
                && (current.all_tables || current.schema_count != 0)
                && !storage.role(new_owner).attributes_to(txn.txid).superuser
            {
                return sql_fail(sql_err!(
                    sqlstate::INSUFFICIENT_PRIVILEGE,
                    "new owner of a schema or all-tables publication must be superuser"
                ));
            }
            let prior = match storage.set_publication_owner(slot, new_owner, txn.txid) {
                Ok(prior) => prior,
                Err(error) => return sql_fail(error),
            };
            let lsn = storage.bump_lsn();
            if let Err(error) = wal.stage(
                txn.txid,
                lsn,
                &WalOp::SetPublicationOwner {
                    name,
                    owner: new_owner as u16,
                },
            ) {
                storage.restore_publication_owner_pending(slot, prior);
                return sql_fail(error);
            }
            if let Err(error) = txn.record_ddl(super::txn::DdlUndo::PublicationOwnerChanged {
                slot: slot as u32,
                prior,
            }) {
                storage.restore_publication_owner_pending(slot, prior);
                return sql_fail(error);
            }
            return Ok(Ok(responder.command_complete("ALTER PUBLICATION")?));
        }
        crate::sql::ast::AlterPublicationAction::SetOperations(operations) => {
            definition.publish_insert = operations.insert;
            definition.publish_update = operations.update;
            definition.publish_delete = operations.delete;
            definition.publish_truncate = operations.truncate;
        }
        crate::sql::ast::AlterPublicationAction::SetTargets { tables, schemas } => {
            if current.all_tables {
                return sql_fail(sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "publication \"{}\" is defined as FOR ALL TABLES",
                    name
                ));
            }
            let (members, count) = match publication_members(storage, txn.txid, tables) {
                Ok(members) => members,
                Err(error) => return sql_fail(error),
            };
            definition.tables = members;
            definition.table_count = count;
            let schema_members = match publication_schemas(storage, txn.txid, schemas) {
                Ok(schemas) => schemas,
                Err(error) => return sql_fail(error),
            };
            definition.schemas = schema_members;
            definition.schema_count = schemas.len();
        }
        crate::sql::ast::AlterPublicationAction::AddTargets { tables, schemas } => {
            if current.all_tables {
                return sql_fail(sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "publication \"{}\" is defined as FOR ALL TABLES",
                    name
                ));
            }
            let (members, count) = match publication_members(storage, txn.txid, tables) {
                Ok(members) => members,
                Err(error) => return sql_fail(error),
            };
            if definition.table_count + count > crate::storage::MAX_PUBLICATION_TABLES {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many tables in publication (limit {})",
                    crate::storage::MAX_PUBLICATION_TABLES
                ));
            }
            for table in &members[..count] {
                if definition.tables[..definition.table_count].contains(table) {
                    return sql_fail(sql_err!(
                        sqlstate::DUPLICATE_OBJECT,
                        "relation is already in publication \"{}\"",
                        name
                    ));
                }
                definition.tables[definition.table_count] = *table;
                definition.table_count += 1;
            }
            let schema_members = match publication_schemas(storage, txn.txid, schemas) {
                Ok(schemas) => schemas,
                Err(error) => return sql_fail(error),
            };
            if definition.schema_count + schemas.len() > crate::storage::MAX_SCHEMAS {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many schemas in publication"
                ));
            }
            for schema in &schema_members[..schemas.len()] {
                if definition.schemas[..definition.schema_count].contains(schema) {
                    return sql_fail(sql_err!(
                        sqlstate::DUPLICATE_OBJECT,
                        "schema is already in publication \"{}\"",
                        name
                    ));
                }
                definition.schemas[definition.schema_count] = *schema;
                definition.schema_count += 1;
            }
        }
        crate::sql::ast::AlterPublicationAction::DropTargets { tables, schemas } => {
            if current.all_tables {
                return sql_fail(sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "publication \"{}\" is defined as FOR ALL TABLES",
                    name
                ));
            }
            let (members, count) = match publication_members(storage, txn.txid, tables) {
                Ok(members) => members,
                Err(error) => return sql_fail(error),
            };
            for table in &members[..count] {
                let Some(index) = definition.tables[..definition.table_count]
                    .iter()
                    .position(|member| member == table)
                else {
                    return sql_fail(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "relation is not part of publication \"{}\"",
                        name
                    ));
                };
                definition
                    .tables
                    .copy_within(index + 1..definition.table_count, index);
                definition.table_count -= 1;
                definition.tables[definition.table_count] = u16::MAX;
            }
            let schema_members = match publication_schemas(storage, txn.txid, schemas) {
                Ok(schemas) => schemas,
                Err(error) => return sql_fail(error),
            };
            for schema in &schema_members[..schemas.len()] {
                let Some(index) = definition.schemas[..definition.schema_count]
                    .iter()
                    .position(|member| member == schema)
                else {
                    return sql_fail(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "schema is not part of publication \"{}\"",
                        name
                    ));
                };
                definition
                    .schemas
                    .copy_within(index + 1..definition.schema_count, index);
                definition.schema_count -= 1;
                definition.schemas[definition.schema_count] = u8::MAX;
            }
        }
    }
    let (slot, prior) = match storage.alter_publication(name, definition, txn.txid) {
        Ok(result) => result,
        Err(error) => return sql_fail(error),
    };
    let lsn = storage.bump_lsn();
    if let Err(error) = wal.stage(
        txn.txid,
        lsn,
        &WalOp::AlterPublication {
            name,
            all_tables: definition.all_tables,
            tables: definition.tables,
            table_count: definition.table_count,
            schemas: definition.schemas,
            schema_count: definition.schema_count,
            publish_insert: definition.publish_insert,
            publish_update: definition.publish_update,
            publish_delete: definition.publish_delete,
            publish_truncate: definition.publish_truncate,
        },
    ) {
        storage.rollback_publication_alter(slot, prior);
        return sql_fail(error);
    }
    if let Err(error) = txn.record_ddl(super::txn::DdlUndo::PublicationAltered {
        slot: slot as u32,
        prior,
    }) {
        storage.rollback_publication_alter(slot, prior);
        return sql_fail(error);
    }
    Ok(Ok(responder.command_complete("ALTER PUBLICATION")?))
}

fn publication_members(
    storage: &Storage,
    txid: u32,
    tables: &[QualName],
) -> Result<([u16; crate::storage::MAX_PUBLICATION_TABLES], usize), SqlError> {
    let mut members = [u16::MAX; crate::storage::MAX_PUBLICATION_TABLES];
    for (index, table) in tables.iter().enumerate() {
        let Some(crate::storage::ResolvedRelation::Table(slot)) =
            storage.resolve_relation(table.schema, table.name, txid)
        else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "relation \"{}\" does not exist",
                table.name
            ));
        };
        storage.require_owner(storage.table_access_object(slot, txid), txid, "table")?;
        if members[..index].contains(&(slot as u16)) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "relation \"{}\" is listed more than once",
                table.name
            ));
        }
        members[index] = slot as u16;
    }
    Ok((members, tables.len()))
}

/// The user-supplied portion of CREATE VIEW, grouped to keep execution's
/// transaction and response dependencies distinct from statement input.
pub struct CreateViewCommand<'a> {
    pub name: &'a QualName<'a>,
    pub or_replace: bool,
    pub sql: &'a str,
    pub raw_path: &'a str,
}

pub fn create_routine(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    routine: &CreateRoutine<'_>,
    arena: &Arena,
    responder: &mut Responder,
) -> Outcome {
    let schema = match storage.creation_schema(routine.name.schema, routine.name.name, txn.txid) {
        Ok(schema) => schema,
        Err(error) => return sql_fail(error),
    };
    let name = match SqlName::parse(routine.name.name) {
        Ok(name) => name,
        Err(error) => return sql_fail(error),
    };
    let mut result_columns = [RoutineArgumentDef::EMPTY; MAX_ROUTINE_ARGUMENTS];
    let mut result_column_count = 0;
    let kind = match routine.kind {
        super::ast::RoutineCreateKind::Function {
            result_type,
            set_returning,
        } => {
            let Some(result) = ColType::from_sql_name(result_type) else {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "type \"{}\" does not exist",
                    result_type
                ));
            };
            if set_returning {
                crate::storage::RoutineKind::SetFunction { result }
            } else {
                crate::storage::RoutineKind::Function { result }
            }
        }
        super::ast::RoutineCreateKind::TableFunction { columns } => {
            let mut output = [RoutineArgumentDef::EMPTY; MAX_ROUTINE_ARGUMENTS];
            for (slot, column) in columns.iter().enumerate() {
                let name = match SqlName::parse(column.name) {
                    Ok(name) => name,
                    Err(error) => return sql_fail(error),
                };
                let Some(ctype) = ColType::from_sql_name(column.type_name) else {
                    return sql_fail(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "type \"{}\" does not exist",
                        column.type_name
                    ));
                };
                if ctype.is_pseudo() {
                    return sql_fail(sql_err!(
                        sqlstate::INVALID_FUNCTION_DEFINITION,
                        "TABLE function column \"{}\" has pseudo-type {}",
                        column.name,
                        column.type_name
                    ));
                }
                output[slot] = RoutineArgumentDef { name, ctype };
            }
            result_columns = output;
            result_column_count = columns.len();
            crate::storage::RoutineKind::TableFunction
        }
        super::ast::RoutineCreateKind::Procedure => crate::storage::RoutineKind::Procedure,
    };
    let mut arguments = [RoutineArgumentDef::EMPTY; MAX_ROUTINE_ARGUMENTS];
    for (slot, argument) in routine.arguments.iter().enumerate() {
        let argument_name = match SqlName::parse(argument.name) {
            Ok(name) => name,
            Err(error) => return sql_fail(error),
        };
        let ctype = match ColType::from_sql_name(argument.type_name) {
            Some(ctype) => ctype,
            None => {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "type \"{}\" does not exist",
                    argument.type_name
                ));
            }
        };
        if ctype.is_pseudo() {
            return sql_fail(sql_err!(
                sqlstate::INVALID_FUNCTION_DEFINITION,
                "function argument \"{}\" has pseudo-type {}",
                argument.name,
                argument.type_name
            ));
        }
        arguments[slot] = RoutineArgumentDef {
            name: argument_name,
            ctype,
        };
    }
    let mut argument_types = [ColType::Text; MAX_ROUTINE_ARGUMENTS];
    for (slot, argument) in arguments[..routine.arguments.len()].iter().enumerate() {
        argument_types[slot] = argument.ctype;
    }
    let replaced = storage.routine_slot_by_signature(
        schema.as_str(),
        name.as_str(),
        &argument_types[..routine.arguments.len()],
        txn.txid,
    );
    if let Some(replaced_slot) = replaced {
        if !routine.or_replace {
            return sql_fail(sql_err!(
                sqlstate::DUPLICATE_FUNCTION,
                "function \"{}\" already exists with same argument types",
                name.as_str()
            ));
        }
        if let Err(error) = storage.require_routine_owner(replaced_slot, txn.txid) {
            return sql_fail(error);
        }
        let prior = storage.routine(replaced_slot);
        let prior_kind = prior.kind;
        let same_result_contract = prior_kind == kind
            && (!matches!(kind, crate::storage::RoutineKind::TableFunction)
                || (prior.result_column_count == result_column_count
                    && prior.result_columns[..result_column_count]
                        == result_columns[..result_column_count]));
        if !same_result_contract {
            let message =
                if prior_kind.function_result().is_some() && kind.function_result().is_some() {
                    "cannot change return type of existing function"
                } else {
                    "cannot change routine kind of existing routine"
                };
            return sql_fail(sql_err!(
                sqlstate::INVALID_FUNCTION_DEFINITION,
                "{}",
                message
            ));
        }
        storage.drop_routine(replaced_slot, txn.txid);
    }
    match kind {
        crate::storage::RoutineKind::Function { .. } => {
            let returns_void = matches!(
                kind,
                crate::storage::RoutineKind::Function {
                    result: ColType::Void
                }
            );
            if let Err(error) =
                super::query::parse_routine_function_program(routine.body, arena, returns_void)
            {
                return sql_fail(error);
            }
        }
        crate::storage::RoutineKind::SetFunction { .. }
        | crate::storage::RoutineKind::TableFunction => {
            let returns_void = matches!(
                kind,
                crate::storage::RoutineKind::SetFunction {
                    result: ColType::Void
                }
            );
            if let Err(error) =
                super::query::parse_routine_function_program(routine.body, arena, returns_void)
            {
                return sql_fail(error);
            }
        }
        crate::storage::RoutineKind::Procedure => {
            let mut parser = match super::parser::Parser::new(routine.body, arena) {
                Ok(parser) => parser,
                Err(error) => return sql_fail(super::parse_error_to_sql(&error)),
            };
            let mut statements = 0usize;
            loop {
                match parser.next_stmt() {
                    Ok(Some(_)) => statements += 1,
                    Ok(None) => break,
                    Err(error) => return sql_fail(super::parse_error_to_sql(&error)),
                }
            }
            if statements == 0 {
                return sql_fail(sql_err!(sqlstate::SYNTAX_ERROR, "procedure body is empty"));
            }
        }
    }
    let body = StackStr::<ROUTINE_SQL_MAX>::from_str(routine.body);
    if body.is_truncated() {
        return sql_fail(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "routine definition exceeds {} bytes",
            ROUTINE_SQL_MAX
        ));
    }
    let slot = match storage.create_routine(
        RoutineSpec {
            identity: replaced
                .map(|slot| RoutineIdentity::Preserve {
                    created_at: storage.routine(slot).created_at,
                    ownership: storage.routine(slot).ownership,
                })
                .unwrap_or(RoutineIdentity::Allocate),
            schema,
            name,
            arguments,
            argument_count: routine.arguments.len(),
            kind,
            result_columns,
            result_column_count,
            body,
        },
        txn.txid,
    ) {
        Ok(slot) => slot,
        Err(error) => return sql_fail(error),
    };
    let lsn = storage.bump_lsn();
    if let Err(error) = wal.stage(txn.txid, lsn, &WalOp::CreateRoutine(*storage.routine(slot))) {
        storage.rollback_routine_create(slot);
        if let Some(replaced_slot) = replaced {
            storage.rollback_routine_drop(replaced_slot, txn.txid);
        }
        return sql_fail(error);
    }
    if let Err(error) = txn.record_ddl(super::txn::DdlUndo::RoutineCreated(slot as u32)) {
        storage.rollback_routine_create(slot);
        if let Some(replaced_slot) = replaced {
            storage.rollback_routine_drop(replaced_slot, txn.txid);
        }
        return sql_fail(error);
    }
    if let Some(replaced_slot) = replaced
        && let Err(error) =
            txn.record_ddl(super::txn::DdlUndo::RoutineDropped(replaced_slot as u32))
    {
        storage.rollback_routine_create(slot);
        storage.rollback_routine_drop(replaced_slot, txn.txid);
        return sql_fail(error);
    }
    let new_object = Storage::routine_access_object(slot);
    if let Some(replaced_slot) = replaced {
        let old_object = Storage::routine_access_object(replaced_slot);
        if let Err(error) = preserve_object_acl(storage, txn, old_object, new_object) {
            return sql_fail(error);
        }
    } else if let Err(error) = apply_default_privileges_to_new_object(storage, txn, new_object) {
        return sql_fail(error);
    }
    responder.command_complete(match kind {
        crate::storage::RoutineKind::Function { .. }
        | crate::storage::RoutineKind::SetFunction { .. }
        | crate::storage::RoutineKind::TableFunction => "CREATE FUNCTION",
        crate::storage::RoutineKind::Procedure => "CREATE PROCEDURE",
    })?;
    sql_ok()
}

pub struct DropRoutineCommand<'a> {
    pub routines: &'a [super::ast::RoutineIdentity<'a>],
    pub if_exists: bool,
    pub cascade: bool,
    pub kind: crate::sql::ast::RoutineTargetKind,
}

pub fn alter_routine(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    kind: crate::sql::ast::RoutineTargetKind,
    identity: &super::ast::RoutineIdentity<'_>,
    action: crate::sql::ast::AlterRoutineAction<'_>,
    responder: &mut Responder,
) -> Outcome {
    let schema = identity.name.schema.unwrap_or("public");
    let mut argument_types = [ColType::Text; MAX_ROUTINE_ARGUMENTS];
    for (index, type_name) in identity.argument_types.iter().enumerate() {
        let Some(ctype) = ColType::from_sql_name(type_name) else {
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "type \"{}\" does not exist",
                type_name
            ));
        };
        argument_types[index] = ctype;
    }
    let Some(slot) = storage.routine_slot_by_signature(
        schema,
        identity.name.name,
        &argument_types[..identity.argument_types.len()],
        txn.txid,
    ) else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "routine \"{}\" does not exist",
            identity.name.name
        ));
    };
    let routine = *storage.routine(slot);
    let actual_kind = if matches!(routine.kind, crate::storage::RoutineKind::Procedure) {
        crate::sql::ast::RoutineTargetKind::Procedure
    } else {
        crate::sql::ast::RoutineTargetKind::Function
    };
    if kind != crate::sql::ast::RoutineTargetKind::Either && kind != actual_kind {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "{} \"{}\" does not exist",
            kind.noun(),
            identity.name.name
        ));
    }
    if let Err(error) = storage.require_routine_owner(slot, txn.txid) {
        return sql_fail(error);
    }
    if let crate::sql::ast::AlterRoutineAction::SetOwner(role) = action {
        let role = resolve_role_name(role);
        let Some(new_owner) = storage.find_role_visible(role.as_str(), txn.txid) else {
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "role \"{}\" does not exist",
                role.as_str()
            ));
        };
        let object = Storage::routine_access_object(slot);
        let current = super::eval::funcs::system::current_user_owned();
        let Some(current_role) = storage.find_role_visible(current.as_str(), txn.txid) else {
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "role \"{}\" does not exist",
                current.as_str()
            ));
        };
        let superuser = storage.role(current_role).attributes_to(txn.txid).superuser;
        if !superuser
            && current_role != new_owner
            && !storage.role_can_set(current_role, new_owner, txn.txid)
        {
            return sql_fail(sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "must be able to SET ROLE \"{}\"",
                role.as_str()
            ));
        }
        if !superuser
            && let Err(error) = storage.require_schema_create_as(
                routine.schema_for(txn.txid).as_str(),
                new_owner,
                txn.txid,
            )
        {
            return sql_fail(error);
        }
        let old_owner = storage.object_owner(object, txn.txid) as u16;
        let prior = storage.set_object_owner(object, new_owner, txn.txid);
        if let Err(error) =
            txn.record_ddl(super::txn::DdlUndo::ObjectOwnerChanged { object, prior })
        {
            storage.restore_object_owner(object, prior);
            return sql_fail(error);
        }
        if let Err(error) =
            rewrite_object_acl_owner(storage, txn, object, old_owner, new_owner as u16)
        {
            return sql_fail(error);
        }
        responder.command_complete(match kind {
            crate::sql::ast::RoutineTargetKind::Function => "ALTER FUNCTION",
            crate::sql::ast::RoutineTargetKind::Procedure => "ALTER PROCEDURE",
            crate::sql::ast::RoutineTargetKind::Either => "ALTER ROUTINE",
        })?;
        return sql_ok();
    }
    let (new_schema, new_name) = match action {
        crate::sql::ast::AlterRoutineAction::Rename(name) => {
            let name = match SqlName::parse(name) {
                Ok(name) => name,
                Err(error) => return sql_fail(error),
            };
            (routine.schema_for(txn.txid), name)
        }
        crate::sql::ast::AlterRoutineAction::SetSchema(schema) => {
            let Some(_) = storage.find_schema_visible(schema, txn.txid) else {
                return sql_fail(sql_err!(
                    sqlstate::INVALID_SCHEMA_NAME,
                    "schema \"{}\" does not exist",
                    schema
                ));
            };
            if let Err(error) = storage.require_schema_create(schema, txn.txid) {
                return sql_fail(error);
            }
            let schema = match SqlName::parse(schema) {
                Ok(schema) => schema,
                Err(error) => return sql_fail(error),
            };
            (schema, routine.name_for(txn.txid))
        }
        crate::sql::ast::AlterRoutineAction::SetOwner(_) => unreachable!(),
    };
    let old_schema = routine.schema_for(txn.txid);
    let old_name = routine.name_for(txn.txid);
    if old_schema == new_schema && old_name == new_name {
        return sql_fail(sql_err!(
            sqlstate::DUPLICATE_FUNCTION,
            "routine \"{}\" already exists",
            new_name.as_str()
        ));
    }
    let mut type_codes = [0_u8; MAX_ROUTINE_ARGUMENTS];
    for (index, argument) in routine.arguments().iter().enumerate() {
        type_codes[index] = argument.ctype.code();
    }
    let prior = match storage.alter_routine_identity(slot, new_schema, new_name, txn.txid) {
        Ok(prior) => prior,
        Err(error) => return sql_fail(error),
    };
    let lsn = storage.bump_lsn();
    if let Err(error) = wal.stage(
        txn.txid,
        lsn,
        &WalOp::AlterRoutineIdentity {
            schema: old_schema.as_str(),
            name: old_name.as_str(),
            argument_type_codes: &type_codes[..routine.argument_count],
            new_schema: new_schema.as_str(),
            new_name: new_name.as_str(),
        },
    ) {
        storage.restore_routine_identity(slot, prior);
        return sql_fail(error);
    }
    if let Err(error) = txn.record_ddl(super::txn::DdlUndo::RoutineIdentityAltered {
        slot: slot as u32,
        prior,
    }) {
        storage.restore_routine_identity(slot, prior);
        return sql_fail(error);
    }
    responder.command_complete(match kind {
        crate::sql::ast::RoutineTargetKind::Function => "ALTER FUNCTION",
        crate::sql::ast::RoutineTargetKind::Procedure => "ALTER PROCEDURE",
        crate::sql::ast::RoutineTargetKind::Either => "ALTER ROUTINE",
    })?;
    sql_ok()
}

pub fn drop_routine(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    command: DropRoutineCommand<'_>,
    responder: &mut Responder,
) -> Outcome {
    let DropRoutineCommand {
        routines,
        if_exists,
        cascade: _,
        kind,
    } = command;
    for identity in routines {
        let schema = identity.name.schema.unwrap_or("public");
        let mut argument_types = [ColType::Text; MAX_ROUTINE_ARGUMENTS];
        if identity.argument_types.len() > argument_types.len() {
            return sql_fail(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many function arguments"
            ));
        }
        for (index, type_name) in identity.argument_types.iter().enumerate() {
            let Some(ctype) = ColType::from_sql_name(type_name) else {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "type \"{}\" does not exist",
                    type_name
                ));
            };
            argument_types[index] = ctype;
        }
        let Some(slot) = storage.routine_slot_by_signature(
            schema,
            identity.name.name,
            &argument_types[..identity.argument_types.len()],
            txn.txid,
        ) else {
            if if_exists {
                continue;
            }
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function \"{}\" does not exist",
                identity.name.name
            ));
        };
        let actual_kind = if matches!(
            storage.routine(slot).kind,
            crate::storage::RoutineKind::Procedure
        ) {
            crate::sql::ast::RoutineTargetKind::Procedure
        } else {
            crate::sql::ast::RoutineTargetKind::Function
        };
        if kind != crate::sql::ast::RoutineTargetKind::Either && kind != actual_kind {
            if if_exists {
                continue;
            }
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "{} \"{}\" does not exist",
                kind.noun(),
                identity.name.name
            ));
        }
        if let Err(error) = storage.require_routine_owner(slot, txn.txid) {
            return sql_fail(error);
        }
        let routine = *storage.routine(slot);
        let lsn = storage.bump_lsn();
        let mut type_codes = [0_u8; MAX_ROUTINE_ARGUMENTS];
        for (index, argument) in routine.arguments().iter().enumerate() {
            type_codes[index] = argument.ctype.code();
        }
        if let Err(error) = wal.stage(
            txn.txid,
            lsn,
            &WalOp::DropRoutine {
                schema: routine.schema_for(txn.txid).as_str(),
                name: routine.name_for(txn.txid).as_str(),
                argument_type_codes: &type_codes[..routine.argument_count],
            },
        ) {
            return sql_fail(error);
        }
        storage.drop_routine(slot, txn.txid);
        if let Err(error) = txn.record_ddl(super::txn::DdlUndo::RoutineDropped(slot as u32)) {
            storage.rollback_routine_drop(slot, txn.txid);
            return sql_fail(error);
        }
    }
    responder.command_complete(match kind {
        crate::sql::ast::RoutineTargetKind::Function => "DROP FUNCTION",
        crate::sql::ast::RoutineTargetKind::Procedure => "DROP PROCEDURE",
        crate::sql::ast::RoutineTargetKind::Either => "DROP ROUTINE",
    })?;
    sql_ok()
}

pub fn create_view(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut super::txn::TxnState,
    command: CreateViewCommand<'_>,
    arena: &Arena,
    responder: &mut Responder,
) -> Outcome {
    let CreateViewCommand {
        name,
        or_replace,
        sql,
        raw_path,
    } = command;
    use core::fmt::Write;
    let mut buffer = crate::util::StackStr::<{ crate::storage::VIEW_SQL_MAX }>::new();
    let _ = write!(buffer, "{sql}");
    if buffer.is_truncated() {
        return sql_fail(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "view definition exceeds {} bytes",
            crate::storage::VIEW_SQL_MAX
        ));
    }
    // Validate the definition now (tables/views exist, columns resolve), as
    // PostgreSQL does at CREATE VIEW time.
    if let Err(e) = super::query::validate_view(buffer.as_str(), storage, txn.txid, arena) {
        return sql_fail(e);
    }
    let user = super::eval::funcs::system::session_user_owned();
    let path = storage.compute_path(raw_path, user.as_str(), txn.txid);
    let dependencies = match super::query::stored_query_dependencies(
        buffer.as_str(),
        storage,
        txn.txid,
        path,
        arena,
    ) {
        Ok(dependencies) => dependencies,
        Err(error) => return sql_fail(error),
    };
    let schema = match storage.creation_schema(name.schema, name.name, txn.txid) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let sqlname = match SqlName::parse(name.name) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let mut creation_path = crate::util::StackStr::<128>::new();
    let _ = core::fmt::Write::write_str(&mut creation_path, raw_path);
    if creation_path.is_truncated() {
        return sql_fail(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "search_path is too long to store with a view"
        ));
    }
    match storage.create_view(
        schema,
        sqlname,
        crate::storage::StoredQueryDefinition {
            sql: buffer,
            creation_path,
            dependencies,
        },
        or_replace,
        txn.txid,
    ) {
        Ok((new_slot, old_slot)) => {
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.stage(
                txn.txid,
                lsn,
                &WalOp::CreateView {
                    schema: schema.as_str(),
                    name: name.name,
                    sql,
                    path: raw_path,
                    dependencies: crate::wal::WalStoredQueryDependencies::Captured(&dependencies),
                },
            ) {
                // The journal rejected the record; undo the in-memory apply.
                storage.rollback_view_create(new_slot);
                if let Some(o) = old_slot {
                    storage.rollback_view_drop(o, txn.txid);
                }
                return sql_fail(e);
            }
            // Rollback undo: drop the new view; revive any superseded one.
            if let Err(e) = txn.record_ddl(super::txn::DdlUndo::ViewCreated(new_slot as u32)) {
                return sql_fail(e);
            }
            let new_object = crate::storage::AccessObject {
                class: crate::storage::AccessClass::View,
                slot: new_slot as u16,
            };
            if let Some(old_slot) = old_slot {
                let old_object = crate::storage::AccessObject {
                    class: crate::storage::AccessClass::View,
                    slot: old_slot as u16,
                };
                let owner = storage.object_owner(old_object, txn.txid);
                storage.set_object_owner(new_object, owner, txn.txid);
                if let Err(error) = preserve_object_acl(storage, txn, old_object, new_object) {
                    return sql_fail(error);
                }
            } else if let Err(error) =
                apply_default_privileges_to_new_object(storage, txn, new_object)
            {
                return sql_fail(error);
            }
            if let Some(o) = old_slot
                && let Err(e) = txn.record_ddl(super::txn::DdlUndo::ViewDropped(o as u32))
            {
                return sql_fail(e);
            }
        }
        Err(e) => return sql_fail(e),
    }
    responder.command_complete("CREATE VIEW")?;
    sql_ok()
}

/// `COMMENT ON <object> IS { 'text' | NULL }`. Resolves and kind-checks the
/// object, applies the comment as this transaction's uncommitted overlay, and
/// journals it (promoted on commit, discarded on rollback — like other DDL).
pub fn comment(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    target: &super::ast::CommentTarget,
    text: Option<&str>,
    arena: &Arena,
    responder: &mut Responder,
) -> Outcome {
    use super::ast::CommentTarget;
    use crate::storage::{CommentClass, StoredRelKind};

    let txid = txn.txid;
    // Resolve the target to a comment key `(class, schema, name, subid)`,
    // matching PostgreSQL's resolution and its error wording.
    let (class, schema, name, subid) = match *target {
        CommentTarget::Relation { kind, name: rel } => {
            let Some((schema, actual)) = storage.classify_relation(rel.schema, rel.name, txid)
            else {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "relation \"{}\" does not exist",
                    rel.name
                ));
            };
            let ok = matches!(
                (kind, actual),
                (super::ast::CommentRelKind::Table, StoredRelKind::Table)
                    | (super::ast::CommentRelKind::View, StoredRelKind::View)
                    | (
                        super::ast::CommentRelKind::MaterializedView,
                        StoredRelKind::Matview
                    )
                    | (super::ast::CommentRelKind::Index, StoredRelKind::Index)
                    | (
                        super::ast::CommentRelKind::Sequence,
                        StoredRelKind::Sequence
                    )
            );
            if !ok {
                return sql_fail(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "\"{}\" is not a{} {}",
                    rel.name,
                    if kind.noun().starts_with(['a', 'e', 'i', 'o', 'u']) {
                        "n"
                    } else {
                        ""
                    },
                    kind.noun()
                ));
            }
            let stored = match SqlName::parse(rel.name) {
                Ok(n) => n,
                Err(e) => return sql_fail(e),
            };
            (CommentClass::Relation, schema, stored, 0u32)
        }
        CommentTarget::Column { relation, column } => {
            let Some((schema, actual)) =
                storage.classify_relation(relation.schema, relation.name, txid)
            else {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "relation \"{}\" does not exist",
                    relation.name
                ));
            };
            if matches!(actual, StoredRelKind::Sequence | StoredRelKind::Index) {
                return sql_fail(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "cannot set comment on relation \"{}\"",
                    relation.name
                ));
            }
            let attnum = if actual == StoredRelKind::View {
                let Some(crate::storage::ResolvedRelation::View(slot)) =
                    storage.resolve_relation(Some(schema.as_str()), relation.name, txid)
                else {
                    return sql_fail(sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "relation \"{}\" does not exist",
                        relation.name
                    ));
                };
                let view = storage.view(slot).clone();
                let user = super::eval::funcs::system::session_user_owned();
                let view_path =
                    storage.compute_path(view.creation_path.as_str(), user.as_str(), txid);
                let view_sql = match arena.alloc_str(view.sql.as_str()) {
                    Ok(sql) => sql,
                    Err(_) => return sql_fail(super::query::arena_full_pub()),
                };
                let mut columns = [crate::sql::types::ColDesc::new("", 0, 0); MAX_PROJ];
                let described = super::query::describe_stored_query(
                    view_sql,
                    storage,
                    txid,
                    view_path,
                    storage.view_dependencies(slot),
                    arena,
                    &mut columns,
                );
                let column_count = match described {
                    Ok(count) => count,
                    Err(error) => return sql_fail(error),
                };
                let Some(index) = columns[..column_count]
                    .iter()
                    .position(|description| description.name == column)
                else {
                    return sql_fail(sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column \"{}\" of relation \"{}\" does not exist",
                        column,
                        relation.name
                    ));
                };
                index as u32 + 1
            } else {
                let slot = storage
                    .find_visible(schema.as_str(), relation.name, txid)
                    .expect("classified relation resolves to a table slot");
                let Some(attnum) = storage.column_number(slot, column) else {
                    return sql_fail(sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column \"{}\" of relation \"{}\" does not exist",
                        column,
                        relation.name
                    ));
                };
                attnum
            };
            let stored = match SqlName::parse(relation.name) {
                Ok(n) => n,
                Err(e) => return sql_fail(e),
            };
            (CommentClass::Relation, schema, stored, attnum)
        }
        CommentTarget::Schema(schema_name) => {
            if storage.find_schema_visible(schema_name, txid).is_none() {
                return sql_fail(sql_err!(
                    sqlstate::INVALID_SCHEMA_NAME,
                    "schema \"{}\" does not exist",
                    schema_name
                ));
            }
            let stored = match SqlName::parse(schema_name) {
                Ok(n) => n,
                Err(e) => return sql_fail(e),
            };
            (CommentClass::Schema, SqlName::EMPTY, stored, 0u32)
        }
        CommentTarget::Type {
            name: type_name,
            domain_only,
        } => {
            let domain_slot = storage.resolve_domain_slot(type_name, txid);
            let enum_slot = storage.resolve_enum_slot(type_name, txid);
            let (qualifier, bare_name) = type_name
                .split_once('.')
                .map_or((None, type_name), |(schema, name)| (Some(schema), name));
            let builtin = (qualifier.is_none() || qualifier == Some("pg_catalog"))
                .then(|| super::catalog::builtin_type_identity(bare_name, qualifier.is_none()))
                .flatten();
            let composite = storage
                .classify_relation(qualifier, bare_name, txid)
                .filter(|(_, kind)| {
                    matches!(
                        kind,
                        StoredRelKind::Table | StoredRelKind::View | StoredRelKind::Matview
                    )
                });
            let (schema, name) = if let Some(slot) = domain_slot {
                let definition = storage.domain(slot);
                (definition.schema, definition.name)
            } else if domain_only {
                if let Some((catalog_name, _)) = builtin {
                    return sql_fail(sql_err!(
                        sqlstate::WRONG_OBJECT_TYPE,
                        "\"pg_catalog.{}\" is not a domain",
                        catalog_name
                    ));
                }
                if enum_slot.is_some() {
                    return sql_fail(sql_err!(
                        sqlstate::WRONG_OBJECT_TYPE,
                        "\"{}\" is not a domain",
                        type_name
                    ));
                }
                if composite.is_some() {
                    return sql_fail(sql_err!(
                        sqlstate::WRONG_OBJECT_TYPE,
                        "\"{}\" is not a domain",
                        type_name
                    ));
                }
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "type \"{}\" does not exist",
                    type_name
                ));
            } else if let Some(slot) = enum_slot {
                let definition = storage.enum_for(slot, txn.txid);
                (definition.schema, definition.name)
            } else if let Some((schema, _)) = composite {
                let stored = match SqlName::parse(bare_name) {
                    Ok(name) => name,
                    Err(error) => return sql_fail(error),
                };
                (schema, stored)
            } else {
                let Some((catalog_name, _)) = builtin else {
                    return sql_fail(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "type \"{}\" does not exist",
                        type_name
                    ));
                };
                let stored = match SqlName::parse(catalog_name) {
                    Ok(name) => name,
                    Err(error) => return sql_fail(error),
                };
                (
                    match SqlName::parse("pg_catalog") {
                        Ok(schema) => schema,
                        Err(error) => return sql_fail(error),
                    },
                    stored,
                )
            };
            (CommentClass::Type, schema, name, 0u32)
        }
    };

    let stored_text = match text {
        Some(t) => match crate::storage::comment_stackstr(t) {
            Ok(s) => Some(s),
            Err(e) => return sql_fail(e),
        },
        None => None,
    };

    let (slot, prior) = match storage.set_comment(class, schema, name, subid, stored_text, txid) {
        Ok(v) => v,
        Err(e) => return sql_fail(e),
    };
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.stage(
        txn.txid,
        lsn,
        &WalOp::Comment {
            class: class.to_u8(),
            schema: schema.as_str(),
            name: name.as_str(),
            subid,
            text,
        },
    ) {
        // The journal rejected the record; undo the in-memory overlay.
        storage.restore_comment_pending(slot, prior);
        return sql_fail(e);
    }
    if let Err(e) = txn.record_ddl(super::txn::DdlUndo::CommentSet {
        slot: slot as u32,
        prior,
    }) {
        storage.restore_comment_pending(slot, prior);
        return sql_fail(e);
    }
    responder.command_complete("COMMENT")?;
    sql_ok()
}

/// CREATE TABLE ... AS <query> [WITH [NO] DATA]: build a table from the query's
/// output schema, then populate it by running the query once.
#[allow(clippy::too_many_arguments)]
pub fn create_table_as(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &crate::sql::ast::QualName,
    rename: &[&str],
    sql: &str,
    with_data: bool,
    if_not_exists: bool,
    materialized: bool,
    raw_path: &str,
    arena: &Arena,
    params: &[Datum],
    responder: &mut Responder,
) -> Outcome {
    use crate::storage::{ColumnMeta, SqlName, TableDef};
    // Resolve the query's output columns without running it.
    let mut columns = [crate::sql::types::ColDesc::new("", 0, 0); MAX_PROJ];
    let n_cols = match super::query::describe_query(sql, storage, txn.txid, arena, &mut columns) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let dependencies = if materialized {
        let user = super::eval::funcs::system::session_user_owned();
        let path = storage.compute_path(raw_path, user.as_str(), txn.txid);
        match super::query::stored_query_dependencies(sql, storage, txn.txid, path, arena) {
            Ok(dependencies) => dependencies,
            Err(error) => return sql_fail(error),
        }
    } else {
        crate::storage::StoredQueryDependencies::EMPTY
    };
    if !rename.is_empty() && rename.len() != n_cols {
        return sql_fail(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "CREATE TABLE AS specifies too {} column names",
            if rename.len() > n_cols { "many" } else { "few" }
        ));
    }
    // Build the backing table's definition from those columns.
    let mut def = TableDef::empty();
    def.schema = match storage.creation_schema(name.schema, name.name, txn.txid) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    def.name = match SqlName::parse(name.name) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    def.n_columns = n_cols;
    for i in 0..n_cols {
        let Some((ctype, user_type)) = catalog_column_type(storage, txn.txid, columns[i].type_oid)
        else {
            return sql_fail(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "CREATE TABLE AS cannot materialize column {} (type oid {})",
                i + 1,
                columns[i].type_oid
            ));
        };
        let col_name = if rename.is_empty() {
            columns[i].name
        } else {
            rename[i]
        };
        let parsed = match SqlName::parse(col_name) {
            Ok(n) => n,
            Err(e) => return sql_fail(e),
        };
        def.columns[i] = ColumnMeta {
            name: parsed,
            ctype,
            type_mod: columns[i].type_mod,
            not_null: false,
            unique: false,
            primary: false,
            auto_increment: false,
            default: crate::storage::ColumnDefault::NONE,
            is_identity: false,
            identity_always: false,
            auto_increment_step: 1,
            user_type,
        };
    }
    // Create the empty table, journaled — exactly as CREATE TABLE does.
    let table_index = match storage.create_table_in(def, txn.txid) {
        Ok(slot) => {
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.stage(txn.txid, lsn, &WalOp::CreateTable(def)) {
                storage.rollback_create(slot);
                return sql_fail(e);
            }
            if let Err(e) = txn.record_ddl(super::txn::DdlUndo::Created(slot as u32)) {
                storage.rollback_create(slot);
                return sql_fail(e);
            }
            if !materialized
                && let Err(error) = apply_default_privileges_to_new_object(
                    storage,
                    txn,
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::Table,
                        slot: slot as u16,
                    },
                )
            {
                return sql_fail(error);
            }
            slot
        }
        Err(e) if e.sqlstate == sqlstate::DUPLICATE_TABLE && if_not_exists => {
            responder.notice(
                sqlstate::DUPLICATE_TABLE,
                stack_format!(128, "relation \"{}\" already exists, skipping", name.name).as_str(),
            )?;
            responder.command_complete("CREATE TABLE AS")?;
            return sql_ok();
        }
        Err(e) => return sql_fail(e),
    };
    // Populate, unless WITH NO DATA. Two passes: materialize the query's rows
    // into the arena (reading storage immutably), then store them (mutably) —
    // the source could reference another table, so the phases must not overlap.
    let mut count = 0u64;
    if with_data {
        let sel = match crate::sql::parser::parse_query(sql, arena) {
            Ok(s) => s,
            Err(e) => return sql_fail(e),
        };
        let mut rows = 0usize;
        if let Err(e) = super::query::select_into_rows(
            storage,
            txn.txid,
            sel,
            arena,
            params,
            None,
            None,
            &mut |_| {
                rows += 1;
                Ok(())
            },
        ) {
            return sql_fail(e);
        }
        let empty: &[u8] = &[];
        let rows_bytes: &mut [&[u8]] = match arena.alloc_slice_with(rows, |_| empty) {
            Ok(r) => r,
            Err(_) => {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "CREATE TABLE AS result exceeds the statement arena"
                ));
            }
        };
        let mut at = 0usize;
        if let Err(e) = super::query::select_into_rows(
            storage,
            txn.txid,
            sel,
            arena,
            params,
            None,
            None,
            &mut |vals| {
                rows_bytes[at] = encode_projected_pub(vals, arena)?;
                at += 1;
                Ok(())
            },
        ) {
            return sql_fail(e);
        }
        for bytes in rows_bytes.iter() {
            let mut values = [Datum::Null; MAX_COLUMNS];
            for (i, slot) in values.iter_mut().enumerate().take(n_cols) {
                let v = decode_projected_pub(bytes, i);
                match coerce(v, &def.columns()[i], storage, txn.txid, arena) {
                    Ok(v) => *slot = v,
                    Err(e) => return sql_fail(e),
                }
            }
            if let Err(e) = store_row(storage, txn, table_index, None, &values[..n_cols]) {
                return sql_fail(e);
            }
            count += 1;
        }
    }
    // A materialized view additionally records its defining query (for REFRESH)
    // and its populated state in the parallel matview catalog. Its rows are the
    // backing table's, which we just filled.
    if materialized {
        use core::fmt::Write;
        let mut buffer = crate::util::StackStr::<{ crate::storage::VIEW_SQL_MAX }>::new();
        let _ = write!(buffer, "{sql}");
        if buffer.is_truncated() {
            return sql_fail(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "materialized view query exceeds {} bytes",
                crate::storage::VIEW_SQL_MAX
            ));
        }
        let mut cpath = crate::util::StackStr::<128>::new();
        let _ = write!(cpath, "{raw_path}");
        match storage.create_matview(
            def.schema,
            def.name,
            crate::storage::StoredQueryDefinition {
                sql: buffer,
                creation_path: cpath,
                dependencies,
            },
            with_data,
            txn.txid,
        ) {
            Ok(slot) => {
                let lsn = storage.bump_lsn();
                if let Err(e) = wal.stage(
                    txn.txid,
                    lsn,
                    &WalOp::CreateMatview {
                        schema: def.schema.as_str(),
                        name: name.name,
                        sql,
                        path: raw_path,
                        dependencies: crate::wal::WalStoredQueryDependencies::Captured(
                            &dependencies,
                        ),
                        populated: with_data,
                    },
                ) {
                    storage.rollback_matview_create(slot);
                    return sql_fail(e);
                }
                if let Err(e) = txn.record_ddl(super::txn::DdlUndo::MatviewCreated(slot as u32)) {
                    storage.rollback_matview_create(slot);
                    return sql_fail(e);
                }
                if let Err(error) = apply_default_privileges_to_new_object(
                    storage,
                    txn,
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::MaterializedView,
                        slot: slot as u16,
                    },
                ) {
                    return sql_fail(error);
                }
            }
            Err(e) => return sql_fail(e),
        }
    }
    responder.command_complete(stack_format!(32, "SELECT {count}").as_str())?;
    sql_ok()
}

/// Resolves a described query column back to persistent table metadata.
/// User-defined OID bands carry both their storage representation and their
/// schema-qualified type identity, so CTAS and materialized views do not
/// flatten enums/domains or lose their automatically-created array types.
pub(crate) fn catalog_column_type(
    storage: &Storage,
    txid: u32,
    type_oid: i32,
) -> Option<(ColType, Option<crate::storage::UserTypeName>)> {
    use crate::sql::types::{ArrElem, oid};
    if (oid::FIRST_DOMAIN..oid::FIRST_DOMAIN + crate::storage::MAX_DOMAINS as i32)
        .contains(&type_oid)
    {
        let domain = storage.domain((type_oid - oid::FIRST_DOMAIN) as usize);
        return domain.visible_to(txid).then_some((
            domain.base,
            Some(crate::storage::UserTypeName {
                schema: domain.schema,
                name: domain.name,
            }),
        ));
    }
    if (oid::FIRST_DOMAIN_ARRAY..oid::FIRST_DOMAIN_ARRAY + crate::storage::MAX_DOMAINS as i32)
        .contains(&type_oid)
    {
        let slot = (type_oid - oid::FIRST_DOMAIN_ARRAY) as usize;
        let domain = storage.domain(slot);
        let element = ArrElem::domain(slot as u16, domain.base)?;
        return domain.visible_to(txid).then_some((
            ColType::Array(element),
            Some(crate::storage::UserTypeName {
                schema: domain.schema,
                name: domain.name,
            }),
        ));
    }
    let ctype = coltype_of_oid(type_oid)?;
    let user_type = match ctype {
        ColType::Enum(slot) | ColType::Array(ArrElem::Enum(slot)) => {
            let enumeration = storage.enum_for(slot as usize, txid);
            if !enumeration.visible_to(txid) {
                return None;
            }
            Some(crate::storage::UserTypeName {
                schema: enumeration.schema,
                name: enumeration.name,
            })
        }
        _ => None,
    };
    Some((ctype, user_type))
}

/// REFRESH MATERIALIZED VIEW: re-run the stored query, replacing every row of
/// the backing table.
pub fn refresh_materialized_view(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &crate::sql::ast::QualName,
    arena: &Arena,
    params: &[Datum],
    responder: &mut Responder,
) -> Outcome {
    let table_index = match storage.resolve_relation(name.schema, name.name, txn.txid) {
        Some(crate::storage::ResolvedRelation::Table(idx)) => idx,
        _ => return sql_fail(undefined_kind("relation", name.name)),
    };
    let def = *storage.table_def(table_index, txn.txid);
    let Some(slot) = storage.matview_slot(def.schema.as_str(), def.name.as_str(), txn.txid) else {
        return sql_fail(sql_err!(
            sqlstate::WRONG_OBJECT_TYPE,
            "\"{}\" is not a materialized view",
            name.name
        ));
    };
    // Copy the stored query out before mutating storage.
    let matview = storage.matview(slot).clone();
    let sql = match arena.alloc_str(matview.sql.as_str()) {
        Ok(s) => s,
        Err(_) => {
            return sql_fail(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "materialized view query exceeds the statement arena"
            ));
        }
    };
    let user = super::eval::funcs::system::session_user_owned();
    let path = storage.compute_path(matview.creation_path.as_str(), user.as_str(), txn.txid);
    let select = match crate::sql::parser::parse_query(sql, arena) {
        Ok(select) => select,
        Err(error) => return sql_fail(error),
    };
    let select = match super::query::expand_stored_query(
        select,
        storage,
        txn.txid,
        path,
        storage.matview_dependencies(slot),
        arena,
    ) {
        Ok(select) => select,
        Err(error) => return sql_fail(error),
    };
    // Remove every visible row, transactionally (a matview has no constraints).
    let mut rowids: [u64; 4096] = [0; 4096];
    loop {
        let mut count = 0usize;
        if let Err(error) = storage.for_each_row_state(table_index, &mut |rowid, state| {
            use core::ops::ControlFlow;
            if storage
                .visible_row_home(table_index, rowid, state, txn.txid)?
                .is_none()
            {
                return Ok(ControlFlow::Continue(()));
            }
            if count == rowids.len() {
                return Ok(ControlFlow::Break(()));
            }
            rowids[count] = rowid;
            count += 1;
            Ok(ControlFlow::Continue(()))
        }) {
            return sql_fail(error);
        }
        if count == 0 {
            break;
        }
        for &rowid in &rowids[..count] {
            match storage.write_pending(table_index, rowid, txn.txid, txn.command_id(), None) {
                Ok(prior) => {
                    if let Err(e) = txn.touch(table_index as u32, rowid, prior) {
                        storage.restore_pending(table_index, rowid, txn.txid, prior);
                        return sql_fail(e);
                    }
                }
                Err(e) => return sql_fail(e),
            }
        }
    }
    // Re-run the query and store its rows into the backing table (two-pass, so
    // the source may read another table without overlapping the write).
    let mut rows = 0usize;
    if let Err(e) = super::query::select_into_rows(
        storage,
        txn.txid,
        select,
        arena,
        params,
        None,
        None,
        &mut |_| {
            rows += 1;
            Ok(())
        },
    ) {
        return sql_fail(e);
    }
    let empty: &[u8] = &[];
    let rows_bytes: &mut [&[u8]] = match arena.alloc_slice_with(rows, |_| empty) {
        Ok(r) => r,
        Err(_) => {
            return sql_fail(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "REFRESH result exceeds the statement arena"
            ));
        }
    };
    let mut at = 0usize;
    if let Err(e) = super::query::select_into_rows(
        storage,
        txn.txid,
        select,
        arena,
        params,
        None,
        None,
        &mut |values| {
            rows_bytes[at] = encode_projected_pub(values, arena)?;
            at += 1;
            Ok(())
        },
    ) {
        return sql_fail(e);
    }
    let n_cols = def.n_columns;
    for bytes in rows_bytes.iter() {
        let mut values = [Datum::Null; MAX_COLUMNS];
        for (i, slot_v) in values.iter_mut().enumerate().take(n_cols) {
            let v = decode_projected_pub(bytes, i);
            match coerce(v, &def.columns()[i], storage, txn.txid, arena) {
                Ok(v) => *slot_v = v,
                Err(e) => return sql_fail(e),
            }
        }
        if let Err(e) = store_row(storage, txn, table_index, None, &values[..n_cols]) {
            return sql_fail(e);
        }
    }
    // Mark it populated (durably).
    storage.set_matview_populated(slot, true);
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.stage(
        txn.txid,
        lsn,
        &WalOp::SetMatviewPopulated {
            schema: def.schema.as_str(),
            name: def.name.as_str(),
            populated: true,
        },
    ) {
        return sql_fail(e);
    }
    responder.command_complete("REFRESH MATERIALIZED VIEW")?;
    sql_ok()
}

/// DROP MATERIALIZED VIEW [IF EXISTS]: drops the backing table and its matview
/// catalog entry.
pub fn drop_materialized_view(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    names: &[crate::sql::ast::QualName],
    if_exists: bool,
    cascade: bool,
    responder: &mut Responder,
) -> Outcome {
    for name in names {
        let idx = match storage.resolve_relation(name.schema, name.name, txn.txid) {
            Some(crate::storage::ResolvedRelation::Table(idx))
                if storage
                    .matview_slot(
                        storage.table_def(idx, txn.txid).schema.as_str(),
                        storage.table_def(idx, txn.txid).name.as_str(),
                        txn.txid,
                    )
                    .is_some() =>
            {
                idx
            }
            // A relation that exists but is not a materialized view is a type
            // error (42809), which IF EXISTS does not suppress.
            Some(_) => {
                return sql_fail(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "\"{}\" is not a materialized view",
                    name.name
                ));
            }
            None if if_exists => {
                responder.notice(
                    crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(
                        128,
                        "materialized view \"{}\" does not exist, skipping",
                        name.name
                    )
                    .as_str(),
                )?;
                continue;
            }
            None => return sql_fail(undefined_kind("materialized view", name.name)),
        };
        let def = *storage.table_def(idx, txn.txid);
        let matview_slot = storage
            .matview_slot(def.schema.as_str(), def.name.as_str(), txn.txid)
            .expect("resolved materialized view has a catalog entry");
        if let Err(error) = storage.require_owner(
            crate::storage::AccessObject {
                class: crate::storage::AccessClass::MaterializedView,
                slot: matview_slot as u16,
            },
            txn.txid,
            "materialized view",
        ) {
            return sql_fail(error);
        }
        let closure = stored_query_dependent_closure(storage, txn.txid, |dependency| {
            dependency.class == crate::storage::DependencyClass::Table
                && dependency.slot as usize == idx
        });
        let (dependent_views, dependent_matviews) = match closure {
            Ok(closure) => closure,
            Err(error) => return sql_fail(error),
        };
        let has_dependents = dependent_views.iter().any(|selected| *selected)
            || dependent_matviews.iter().any(|selected| *selected);
        if has_dependents && !cascade {
            return sql_fail(sql_err!(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop materialized view {} because other objects depend on it",
                def.name.as_str()
            ));
        }
        if cascade
            && let Err(error) = drop_selected_stored_queries(
                storage,
                wal,
                txn,
                &dependent_views,
                &dependent_matviews,
            )
        {
            return sql_fail(error);
        }
        // Drop the backing table.
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.stage(
            txn.txid,
            lsn,
            &WalOp::DropTable {
                schema: def.schema.as_str(),
                name: def.name.as_str(),
            },
        ) {
            return sql_fail(e);
        }
        if let Err(e) = txn.record_ddl(super::txn::DdlUndo::Dropped(idx as u32)) {
            return sql_fail(e);
        }
        storage.drop_table_in(idx, txn.txid);
        storage.drop_indexes_for(def.schema.as_str(), def.name.as_str(), txn.txid);
        // Drop the matview catalog entry.
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.stage(
            txn.txid,
            lsn,
            &WalOp::DropMatview {
                schema: def.schema.as_str(),
                name: def.name.as_str(),
            },
        ) {
            return sql_fail(e);
        }
        match storage.drop_matview(def.schema.as_str(), def.name.as_str(), txn.txid) {
            Ok(Some(slot)) => {
                if let Err(e) = txn.record_ddl(super::txn::DdlUndo::MatviewDropped(slot as u32)) {
                    return sql_fail(e);
                }
            }
            Ok(None) => {}
            Err(e) => return sql_fail(e),
        }
    }
    responder.command_complete("DROP MATERIALIZED VIEW")?;
    sql_ok()
}

/// Maps an `AS <type>` name to a [`SeqType`]; unknown types are rejected exactly
/// as PostgreSQL rejects them.
fn seq_type_of(name: &str) -> Result<SeqType, SqlError> {
    match name {
        "smallint" | "int2" => Ok(SeqType::Smallint),
        "integer" | "int" | "int4" => Ok(SeqType::Integer),
        "bigint" | "int8" => Ok(SeqType::Bigint),
        _ => Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "sequence type must be smallint, integer, or bigint"
        )),
    }
}

/// Computes and validates a [`SeqSpec`] from parsed options, against real
/// PostgreSQL's rules and SQLSTATEs. `base` is the current spec for ALTER (an
/// omitted option keeps its current value); `None` for CREATE (omitted options
/// take their defaults). Returns the spec and any RESTART target.
fn resolve_seq_spec(
    options: &crate::sql::ast::SeqOptions,
    base: Option<SeqSpec>,
) -> Result<(SeqSpec, Option<i64>), SqlError> {
    use crate::sql::ast::SeqBound;
    let data_type = match options.data_type {
        Some(n) => seq_type_of(n)?,
        None => base.map(|b| b.data_type).unwrap_or(SeqType::Bigint),
    };
    let increment = options.increment.or(base.map(|b| b.increment)).unwrap_or(1);
    if increment == 0 {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "INCREMENT must not be zero"
        ));
    }
    let ascending = increment > 0;
    let (type_min, type_max) = data_type.bounds();
    let default_min = if ascending { 1 } else { type_min };
    let default_max = if ascending { type_max } else { -1 };
    let min_value = match options.min_value {
        SeqBound::Value(v) => {
            if v < type_min || v > type_max {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "MINVALUE ({}) is out of range for sequence data type {}",
                    v,
                    data_type.sql_name()
                ));
            }
            v
        }
        SeqBound::NoBound => default_min,
        SeqBound::Unset => base.map(|b| b.min_value).unwrap_or(default_min),
    };
    let max_value = match options.max_value {
        SeqBound::Value(v) => {
            if v < type_min || v > type_max {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "MAXVALUE ({}) is out of range for sequence data type {}",
                    v,
                    data_type.sql_name()
                ));
            }
            v
        }
        SeqBound::NoBound => default_max,
        SeqBound::Unset => base.map(|b| b.max_value).unwrap_or(default_max),
    };
    if min_value >= max_value {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "MINVALUE ({}) must be less than MAXVALUE ({})",
            min_value,
            max_value
        ));
    }
    let default_start = if ascending { min_value } else { max_value };
    let start_value = options
        .start
        .or(base.map(|b| b.start_value))
        .unwrap_or(default_start);
    if start_value < min_value {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "START value ({}) cannot be less than MINVALUE ({})",
            start_value,
            min_value
        ));
    }
    if start_value > max_value {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "START value ({}) cannot be greater than MAXVALUE ({})",
            start_value,
            max_value
        ));
    }
    let cache = options.cache.or(base.map(|b| b.cache)).unwrap_or(1);
    if cache < 1 {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "CACHE ({}) must be greater than zero",
            cache
        ));
    }
    let cycle = options.cycle.or(base.map(|b| b.cycle)).unwrap_or(false);
    // RESTART [WITH n]: n defaults to the (new) start value; validate it is in
    // range, matching PostgreSQL.
    let restart = match options.restart {
        None => None,
        Some(explicit) => {
            let target = explicit.unwrap_or(start_value);
            if target < min_value || target > max_value {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "RESTART value ({}) cannot be less than MINVALUE ({})",
                    target,
                    min_value
                ));
            }
            Some(target)
        }
    };
    Ok((
        SeqSpec {
            data_type,
            increment,
            min_value,
            max_value,
            start_value,
            cache,
            cycle,
        },
        restart,
    ))
}

fn resolve_sequence_owner(
    storage: &Storage,
    owner: crate::sql::ast::SeqOwner<'_>,
    sequence_schema: &str,
    txid: u32,
) -> Result<crate::storage::SequenceOwner, SqlError> {
    let table_index = match storage.resolve_relation(owner.table.schema, owner.table.name, txid) {
        Some(crate::storage::ResolvedRelation::Table(index)) => index,
        _ => return Err(undefined_qual(&owner.table)),
    };
    let table = storage.table_def(table_index, txid);
    if table.schema.as_str() != sequence_schema {
        return Err(sql_err!(
            sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "sequence must be in same schema as table it is linked to"
        ));
    }
    let Some(column_index) = table.column_index(owner.column) else {
        return Err(undefined_column(owner.column));
    };
    Ok(crate::storage::SequenceOwner {
        table_schema: table.schema,
        table: table.name,
        column: table.columns()[column_index].name,
    })
}

/// CREATE SEQUENCE [IF NOT EXISTS].
pub fn create_sequence(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &QualName,
    if_not_exists: bool,
    options: &crate::sql::ast::SeqOptions,
    responder: &mut Responder,
) -> Outcome {
    let schema = match storage.creation_schema(name.schema, name.name, txn.txid) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    if storage.relation_name_taken(schema.as_str(), name.name, txn.txid) {
        if if_not_exists {
            responder.notice(
                sqlstate::DUPLICATE_TABLE,
                stack_format!(128, "relation \"{}\" already exists, skipping", name.name).as_str(),
            )?;
            responder.command_complete("CREATE SEQUENCE")?;
            return sql_ok();
        }
        return sql_fail(sql_err!(
            sqlstate::DUPLICATE_TABLE,
            "relation \"{}\" already exists",
            name.name
        ));
    }
    let (spec, _) = match resolve_seq_spec(options, None) {
        Ok(v) => v,
        Err(e) => return sql_fail(e),
    };
    let owner = match options.owned_by {
        Some(Some(owner)) => {
            match resolve_sequence_owner(storage, owner, schema.as_str(), txn.txid) {
                Ok(owner) => Some(owner),
                Err(error) => return sql_fail(error),
            }
        }
        _ => None,
    };
    let sqlname = match SqlName::parse(name.name) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let slot = match storage.create_sequence(schema, sqlname, spec, owner, None, txn.txid) {
        Ok(slot) => slot,
        Err(e) => return sql_fail(e),
    };
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.stage(
        txn.txid,
        lsn,
        &WalOp::CreateSequence {
            schema: schema.as_str(),
            name: name.name,
            data_type: spec.data_type.to_u8(),
            increment: spec.increment,
            min_value: spec.min_value,
            max_value: spec.max_value,
            start_value: spec.start_value,
            cache: spec.cache,
            cycle: spec.cycle,
            owner,
            generator_for: None,
        },
    ) {
        storage.rollback_sequence_create(slot);
        return sql_fail(e);
    }
    if let Err(e) = txn.record_ddl(super::txn::DdlUndo::SequenceCreated(slot as u32)) {
        return sql_fail(e);
    }
    if let Err(error) = apply_default_privileges_to_new_object(
        storage,
        txn,
        crate::storage::AccessObject {
            class: crate::storage::AccessClass::Sequence,
            slot: slot as u16,
        },
    ) {
        return sql_fail(error);
    }
    responder.command_complete("CREATE SEQUENCE")?;
    sql_ok()
}

/// ALTER SEQUENCE [IF EXISTS] — redefine parameters (and optionally RESTART).
pub fn alter_sequence(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &QualName,
    if_exists: bool,
    options: &crate::sql::ast::SeqOptions,
    responder: &mut Responder,
) -> Outcome {
    let slot = match resolve_sequence(storage, name, txn.txid) {
        Ok(Some(slot)) => slot,
        Ok(None) if if_exists => {
            responder.notice(
                crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                stack_format!(128, "sequence \"{}\" does not exist, skipping", name.name).as_str(),
            )?;
            responder.command_complete("ALTER SEQUENCE")?;
            return sql_ok();
        }
        Ok(None) => return sql_fail(undefined_kind("sequence", name.name)),
        Err(e) => return sql_fail(e),
    };
    if let Err(error) = storage.require_owner(
        crate::storage::AccessObject {
            class: crate::storage::AccessClass::Sequence,
            slot: slot as u16,
        },
        txn.txid,
        "sequence",
    ) {
        return sql_fail(error);
    }
    let prior = storage.sequence_for(slot, txn.txid);
    let base = SeqSpec {
        data_type: prior.data_type,
        increment: prior.increment,
        min_value: prior.min_value,
        max_value: prior.max_value,
        start_value: prior.start_value,
        cache: prior.cache,
        cycle: prior.cycle,
    };
    let (spec, restart) = match resolve_seq_spec(options, Some(base)) {
        Ok(v) => v,
        Err(e) => return sql_fail(e),
    };
    if options.owned_by.is_some()
        && let Some(generator) = prior.generator_for
        && let Some(table_slot) = storage.find_visible(
            generator.table_schema.as_str(),
            generator.table.as_str(),
            txn.txid,
        )
        && let Some(column) = storage
            .table_def(table_slot, txn.txid)
            .column_index(generator.column.as_str())
        && storage.table_def(table_slot, txn.txid).columns()[column].is_identity
    {
        return sql_fail(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "cannot change ownership of identity sequence"
        ));
    }
    let owner = match options.owned_by {
        None => prior.owner,
        Some(None) => None,
        Some(Some(requested)) => {
            let sequence_schema = prior.schema;
            match resolve_sequence_owner(storage, requested, sequence_schema.as_str(), txn.txid) {
                Ok(owner) => Some(owner),
                Err(error) => return sql_fail(error),
            }
        }
    };
    let generator_for = prior.generator_for;
    let prior_definition =
        match storage.stage_sequence_alter(slot, spec, owner, generator_for, restart, txn.txid) {
            Ok(prior) => prior,
            Err(error) => return sql_fail(error),
        };
    let (schema, sname) = {
        let s = storage.sequence_for(slot, txn.txid);
        (s.schema, s.name)
    };
    // The redefinition journals as a CreateSequence (absolute parameters).
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.stage(
        txn.txid,
        lsn,
        &WalOp::CreateSequence {
            schema: schema.as_str(),
            name: sname.as_str(),
            data_type: spec.data_type.to_u8(),
            increment: spec.increment,
            min_value: spec.min_value,
            max_value: spec.max_value,
            start_value: spec.start_value,
            cache: spec.cache,
            cycle: spec.cycle,
            owner,
            generator_for,
        },
    ) {
        storage.rollback_sequence_alter(slot, prior_definition);
        return sql_fail(e);
    }
    if let Err(error) = txn.record_ddl(super::txn::DdlUndo::SequenceAltered {
        slot: slot as u32,
        prior: prior_definition,
    }) {
        storage.rollback_sequence_alter(slot, prior_definition);
        return sql_fail(error);
    }
    responder.command_complete("ALTER SEQUENCE")?;
    sql_ok()
}

/// DROP SEQUENCE [IF EXISTS].
pub fn drop_sequence(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    names: &[crate::sql::ast::QualName],
    if_exists: bool,
    cascade: bool,
    responder: &mut Responder,
) -> Outcome {
    for name in names {
        let slot = match resolve_sequence(storage, name, txn.txid) {
            Ok(Some(slot)) => slot,
            Ok(None) if if_exists => {
                responder.notice(
                    crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(128, "sequence \"{}\" does not exist, skipping", name.name)
                        .as_str(),
                )?;
                continue;
            }
            Ok(None) => return sql_fail(undefined_kind("sequence", name.name)),
            Err(e) => return sql_fail(e),
        };
        if let Err(error) = storage.require_owner(
            crate::storage::AccessObject {
                class: crate::storage::AccessClass::Sequence,
                slot: slot as u16,
            },
            txn.txid,
            "sequence",
        ) {
            return sql_fail(error);
        }
        let (schema, sname) = {
            let s = storage.sequence_for(slot, txn.txid);
            if let Some(owner) = s.owner {
                return sql_fail(sql_err!(
                    sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                    "cannot drop sequence \"{}\" because column {}.{}.{} requires it",
                    s.name.as_str(),
                    owner.table_schema.as_str(),
                    owner.table.as_str(),
                    owner.column.as_str()
                ));
            }
            (s.schema, s.name)
        };
        let closure = stored_query_dependent_closure(storage, txn.txid, |dependency| {
            dependency.class == crate::storage::DependencyClass::Sequence
                && dependency.slot as usize == slot
        });
        let (dependent_views, dependent_matviews) = match closure {
            Ok(closure) => closure,
            Err(error) => return sql_fail(error),
        };
        let has_dependents = dependent_views.iter().any(|selected| *selected)
            || dependent_matviews.iter().any(|selected| *selected);
        if has_dependents && !cascade {
            if let Err(error) = report_stored_query_dependents(
                storage,
                txn.txid,
                StoredQueryRoot {
                    class: crate::storage::DependencyClass::Sequence,
                    slot,
                    kind: "sequence",
                    schema,
                    name: sname,
                },
                StoredQuerySelection {
                    views: &dependent_views,
                    matviews: &dependent_matviews,
                },
                false,
                responder,
            ) {
                return sql_fail(error);
            }
            return sql_fail(sql_err!(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop sequence {} because other objects depend on it",
                sname.as_str()
            ));
        }
        if cascade {
            if let Err(error) = report_stored_query_dependents(
                storage,
                txn.txid,
                StoredQueryRoot {
                    class: crate::storage::DependencyClass::Sequence,
                    slot,
                    kind: "sequence",
                    schema,
                    name: sname,
                },
                StoredQuerySelection {
                    views: &dependent_views,
                    matviews: &dependent_matviews,
                },
                true,
                responder,
            ) {
                return sql_fail(error);
            }
            if let Err(error) = drop_selected_stored_queries(
                storage,
                wal,
                txn,
                &dependent_views,
                &dependent_matviews,
            ) {
                return sql_fail(error);
            }
        }
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.stage(
            txn.txid,
            lsn,
            &WalOp::DropSequence {
                schema: schema.as_str(),
                name: sname.as_str(),
            },
        ) {
            return sql_fail(e);
        }
        match storage.drop_sequence(schema.as_str(), sname.as_str(), txn.txid) {
            Ok(Some(slot)) => {
                if let Err(e) = txn.record_ddl(super::txn::DdlUndo::SequenceDropped(slot as u32)) {
                    return sql_fail(e);
                }
            }
            Ok(None) => {}
            Err(e) => return sql_fail(e),
        }
    }
    responder.command_complete("DROP SEQUENCE")?;
    sql_ok()
}

// --- CREATE / ALTER / DROP DOMAIN --------------------------------------------

/// Copies domain text (a DEFAULT or CHECK source) into a fixed buffer, or a loud
/// `PROGRAM_LIMIT_EXCEEDED` if it is longer than `N`.
fn domain_text<const N: usize>(text: &str) -> Result<StackStr<N>, SqlError> {
    use core::fmt::Write as _;
    let mut out = StackStr::<N>::new();
    let _ = write!(out, "{text}");
    if out.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "domain constraint or default exceeds {} bytes",
            N
        ));
    }
    Ok(out)
}

/// Validates that a domain CHECK/DEFAULT expression parses and references no
/// column other than `VALUE` (PostgreSQL's placeholder for the input value).
fn validate_domain_expr(text: &str, allow_value: bool, arena: &Arena) -> Result<(), SqlError> {
    let expr = crate::sql::parser::parse_expr(text, arena)?;
    let mut bad: Option<SqlError> = None;
    expr.for_each_column(&mut |name| {
        if bad.is_some() {
            return;
        }
        if !(allow_value && name.eq_ignore_ascii_case("value")) {
            bad = Some(sql_err!(
                sqlstate::UNDEFINED_COLUMN,
                "column \"{}\" does not exist",
                name
            ));
        }
    });
    match bad {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Builds the validated [`DomainSpec`] from a base type, its typmod, and the
/// domain's constraints; unnamed CHECKs get PostgreSQL-style `<domain>_check`
/// names.
#[allow(clippy::too_many_arguments)]
fn build_domain_spec(
    storage: &Storage,
    txid: u32,
    domain: &str,
    base_type: &str,
    base_type_mod: i32,
    not_null: bool,
    default_text: Option<&str>,
    ast_checks: &[crate::sql::ast::DomainCheck],
    arena: &Arena,
) -> Result<crate::storage::DomainSpec, SqlError> {
    if matches!(base_type, "record" | "record[]") {
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "\"{}\" is not a valid base type for a domain",
            base_type
        ));
    }
    let (base_domain, base, inherited_default) =
        if let Some(base) = ColType::from_sql_name(base_type) {
            (None, base, None)
        } else if let Some(parent) = storage.find_domain(base_type, txid) {
            (
                Some(crate::storage::UserTypeName {
                    schema: parent.schema,
                    name: parent.name,
                }),
                parent.base,
                parent.default_expr,
            )
        } else if let Some(enum_slot) = storage.resolve_enum_slot(base_type, txid) {
            (None, ColType::Enum(enum_slot as u16), None)
        } else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "type \"{}\" does not exist",
                base_type
            ));
        };
    if ColType::from_sql_name(base_type).is_none() {
        if let Some(parent) = storage.find_domain(base_type, txid) {
            storage.require_type_usage(parent.schema.as_str(), parent.name.as_str(), txid)?;
        } else if let Some(enum_slot) = storage.resolve_enum_slot(base_type, txid) {
            let enumeration = storage.enum_for(enum_slot, txid);
            storage.require_type_usage(
                enumeration.schema.as_str(),
                enumeration.name.as_str(),
                txid,
            )?;
        }
    }
    let default_expr = match default_text {
        Some(t) => {
            validate_domain_expr(t, false, arena)?;
            Some(domain_text::<{ crate::storage::DEFAULT_EXPR_MAX }>(t)?)
        }
        // PostgreSQL copies a parent domain's default at CREATE time: a later
        // ALTER of the parent default does not change the child.
        None => inherited_default,
    };
    let mut checks = [crate::storage::CheckConstraint::EMPTY; crate::storage::MAX_DOMAIN_CHECKS];
    let mut n = 0;
    for c in ast_checks {
        if n == crate::storage::MAX_DOMAIN_CHECKS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "a domain may have at most {} CHECK constraints",
                crate::storage::MAX_DOMAIN_CHECKS
            ));
        }
        validate_domain_expr(c.expression, true, arena)?;
        let name = match c.name {
            Some(nm) => SqlName::parse(nm)?,
            None => generate_check_name(domain, &checks[..n])?,
        };
        checks[n] = crate::storage::CheckConstraint {
            name,
            expression: domain_text::<{ crate::storage::CHECK_SQL_MAX }>(c.expression)?,
        };
        n += 1;
    }
    Ok(crate::storage::DomainSpec {
        base_domain,
        base,
        base_type_mod,
        not_null,
        default_expr,
        checks,
        n_checks: n,
    })
}

/// PostgreSQL's unnamed-constraint naming for a domain CHECK: `<domain>_check`,
/// then `<domain>_check1`, `<domain>_check2`, … avoiding names already used.
fn generate_check_name(
    domain: &str,
    existing: &[crate::storage::CheckConstraint],
) -> Result<SqlName, SqlError> {
    for suffix in 0..1000usize {
        let candidate = if suffix == 0 {
            stack_format!(128, "{}_check", domain)
        } else {
            stack_format!(128, "{}_check{}", domain, suffix)
        };
        if !existing
            .iter()
            .any(|c| c.name.as_str() == candidate.as_str())
        {
            return SqlName::parse(candidate.as_str());
        }
    }
    Err(sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "cannot name domain CHECK constraint"
    ))
}

pub fn create_domain(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    d: &crate::sql::ast::CreateDomain,
    arena: &Arena,
    responder: &mut Responder,
) -> Outcome {
    let schema = match storage.creation_schema(d.name.schema, d.name.name, txn.txid) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    if storage
        .domain_slot(schema.as_str(), d.name.name, txn.txid)
        .is_some()
        || storage
            .enum_slot(schema.as_str(), d.name.name, txn.txid)
            .is_some()
    {
        return sql_fail(sql_err!(
            sqlstate::DUPLICATE_OBJECT,
            "type \"{}\" already exists",
            d.name.name
        ));
    }
    let spec = match build_domain_spec(
        storage,
        txn.txid,
        d.name.name,
        d.base_type,
        d.base_type_mod,
        d.not_null,
        d.default_text,
        d.checks,
        arena,
    ) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let sqlname = match SqlName::parse(d.name.name) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let slot = match storage.create_domain(schema, sqlname, spec, txn.txid) {
        Ok(slot) => slot,
        Err(e) => return sql_fail(e),
    };
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.stage(txn.txid, lsn, &WalOp::CreateDomain(*storage.domain(slot))) {
        storage.rollback_domain_create(slot);
        return sql_fail(e);
    }
    if let Err(e) = txn.record_ddl(super::txn::DdlUndo::DomainCreated(slot as u32)) {
        return sql_fail(e);
    }
    if let Err(error) = apply_default_privileges_to_new_object(
        storage,
        txn,
        crate::storage::AccessObject {
            class: crate::storage::AccessClass::Domain,
            slot: slot as u16,
        },
    ) {
        return sql_fail(error);
    }
    responder.command_complete("CREATE DOMAIN")?;
    sql_ok()
}

#[allow(clippy::too_many_arguments)]
pub fn drop_domain(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    scratch: &mut FixedVec<(u64, RowHome)>,
    names: &[QualName],
    if_exists: bool,
    cascade: bool,
    arena: &Arena,
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
) -> Outcome {
    let mut selected = [false; crate::storage::MAX_DOMAINS];
    for name in names {
        let slot = match name.schema {
            Some(schema) => storage.domain_slot(schema, name.name, txn.txid),
            None => storage.resolve_domain_slot(name.name, txn.txid),
        };
        let Some(slot) = slot else {
            if if_exists {
                responder.notice(
                    sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(128, "type \"{}\" does not exist, skipping", name.name).as_str(),
                )?;
                continue;
            }
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "type \"{}\" does not exist",
                name.name
            ));
        };
        if let Err(error) = storage.require_owner(
            crate::storage::AccessObject {
                class: crate::storage::AccessClass::Domain,
                slot: slot as u16,
            },
            txn.txid,
            "type",
        ) {
            return sql_fail(error);
        }
        selected[slot] = true;
    }

    // Domains form a bounded parent chain. Resolve the whole closure before
    // mutating any catalog entry, so CASCADE drops descendant domains too and
    // a failure cannot leave a dangling base-domain reference.
    for candidate in 0..storage.domain_count() {
        if !storage.domain(candidate).visible_to(txn.txid) || selected[candidate] {
            continue;
        }
        let depends_on_selected = (0..storage.domain_count()).any(|target| {
            selected[target] && domain_depends_on(storage, candidate, target, txn.txid)
        });
        if !depends_on_selected {
            continue;
        }
        if !cascade {
            let target = (0..storage.domain_count())
                .find(|target| {
                    selected[*target] && domain_depends_on(storage, candidate, *target, txn.txid)
                })
                .expect("dependency target");
            return sql_fail(sql_err!(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop type {} because other objects depend on it",
                storage.domain(target).name.as_str()
            ));
        }
        selected[candidate] = true;
    }

    if let Err(error) = drop_domain_selection(
        storage,
        wal,
        txn,
        scratch,
        &selected,
        None,
        cascade,
        0,
        arena,
        seq_session,
        responder,
    ) {
        return sql_fail(error);
    }
    responder.command_complete("DROP DOMAIN")?;
    sql_ok()
}

#[allow(clippy::too_many_arguments)]
fn drop_domain_selection(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    scratch: &mut FixedVec<(u64, RowHome)>,
    selected: &[bool; crate::storage::MAX_DOMAINS],
    selected_enum: Option<usize>,
    cascade: bool,
    reserved_undo: usize,
    arena: &Arena,
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
) -> Result<(), SqlError> {
    let selected_count = selected.iter().filter(|&&yes| yes).count();
    if selected_count > 0 || selected_enum.is_some() {
        apply_type_drop_to_stored_queries(storage, wal, txn, selected, selected_enum, cascade)?;
    }
    for (slot, is_selected) in selected.iter().enumerate().take(storage.domain_count()) {
        if !*is_selected {
            continue;
        }
        while let Some((table_schema, table, _)) = domain_column_in_use(storage, slot, txn.txid) {
            if cascade {
                let domain = storage.domain(slot);
                let (domain_schema, domain_name) = (domain.schema, domain.name);
                let table_slot = storage
                    .find_visible(table_schema.as_str(), table.as_str(), txn.txid)
                    .expect("dependent table remains visible");
                let def = storage.table_def(table_slot, txn.txid);
                let mut columns = [SqlName::EMPTY; MAX_COLUMNS];
                let mut column_count = 0;
                for column in def.columns() {
                    if column.user_type
                        == Some(crate::storage::UserTypeName {
                            schema: domain_schema,
                            name: domain_name,
                        })
                    {
                        columns[column_count] = column.name;
                        column_count += 1;
                    }
                }
                cascade_drop_type_column(
                    storage,
                    wal,
                    txn,
                    scratch,
                    table_schema,
                    table,
                    &columns[..column_count],
                    arena,
                    seq_session,
                    responder,
                )?;
                continue;
            }
            return Err(sql_err!(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop type {} because other objects depend on it",
                storage.domain(slot).name.as_str()
            ));
        }
    }

    let undo_needed = selected_count + reserved_undo;
    if txn.ddl().len() + undo_needed > super::txn::MAX_TXN_DDL {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "type drop needs {} DDL undo entries but only {} remain",
            undo_needed,
            super::txn::MAX_TXN_DDL - txn.ddl().len()
        ));
    }

    // Produce child-before-parent order while every definition is visible.
    let mut ordered = [usize::MAX; crate::storage::MAX_DOMAINS];
    let mut ordered_count = 0;
    while ordered_count < selected_count {
        let Some(leaf) = (0..storage.domain_count()).find(|candidate| {
            selected[*candidate]
                && !ordered[..ordered_count].contains(candidate)
                && !(0..storage.domain_count()).any(|child| {
                    child != *candidate
                        && selected[child]
                        && !ordered[..ordered_count].contains(&child)
                        && domain_depends_on(storage, child, *candidate, txn.txid)
                })
        }) else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "domain dependency cycle detected"
            ));
        };
        ordered[ordered_count] = leaf;
        ordered_count += 1;
    }

    for slot in ordered[..ordered_count].iter().copied() {
        let (schema, dname) = {
            let domain = storage.domain(slot);
            (domain.schema, domain.name)
        };
        let lsn = storage.bump_lsn();
        wal.stage(
            txn.txid,
            lsn,
            &WalOp::DropDomain {
                schema: schema.as_str(),
                name: dname.as_str(),
            },
        )?;
        match storage.drop_domain(schema.as_str(), dname.as_str(), txn.txid) {
            Ok(Some(slot)) => {
                txn.record_ddl(super::txn::DdlUndo::DomainDropped(slot as u32))?;
            }
            Ok(None) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn domain_column_in_use(
    storage: &Storage,
    domain_slot: usize,
    txid: u32,
) -> Option<(SqlName, SqlName, SqlName)> {
    let domain = storage.domain(domain_slot);
    for table_index in 0..storage.table_count() {
        let table = storage.table(table_index);
        if !table.visible_to(txid) {
            continue;
        }
        let def = storage.table_def(table_index, txid);
        for column in def.columns() {
            if column.user_type
                == Some(crate::storage::UserTypeName {
                    schema: domain.schema,
                    name: domain.name,
                })
            {
                return Some((def.schema, def.name, column.name));
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn cascade_drop_type_column(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    scratch: &mut FixedVec<(u64, RowHome)>,
    table_schema: SqlName,
    table_name: SqlName,
    column_names: &[SqlName],
    arena: &Arena,
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
) -> Result<(), SqlError> {
    let mut actions = [AlterAction::DropColumn(""); MAX_COLUMNS];
    for (index, column) in column_names.iter().enumerate() {
        actions[index] = AlterAction::DropColumn(column.as_str());
    }
    let statement = AlterTable {
        table: QualName {
            schema: Some(table_schema.as_str()),
            name: table_name.as_str(),
        },
        if_exists: false,
        actions: &actions[..column_names.len()],
    };
    match alter_table_inner(
        storage,
        wal,
        txn,
        scratch,
        &statement,
        arena,
        seq_session,
        responder,
        false,
    ) {
        Ok(result) => result,
        Err(_) => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "response buffer exhausted during internal type cascade"
        )),
    }
}

pub fn alter_domain(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &QualName,
    action: &crate::sql::ast::AlterDomainAction,
    arena: &Arena,
    responder: &mut Responder,
) -> Outcome {
    use crate::sql::ast::AlterDomainAction as A;
    let slot = match name.schema {
        Some(schema) => storage.domain_slot(schema, name.name, txn.txid),
        None => storage.resolve_domain_slot(name.name, txn.txid),
    };
    let Some(slot) = slot else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "type \"{}\" does not exist",
            name.name
        ));
    };
    if let Err(error) = storage.require_owner(
        crate::storage::AccessObject {
            class: crate::storage::AccessClass::Domain,
            slot: slot as u16,
        },
        txn.txid,
        "type",
    ) {
        return sql_fail(error);
    }
    if matches!(action, A::Rename(_) | A::SetSchema(_)) {
        let current = storage.domain_for(slot, txn.txid);
        let (schema, domain_name) = match action {
            A::Rename(domain_name) => {
                let domain_name = match SqlName::parse(domain_name) {
                    Ok(domain_name) => domain_name,
                    Err(error) => return sql_fail(error),
                };
                (current.schema, domain_name)
            }
            A::SetSchema(schema) => {
                if storage.find_schema_visible(schema, txn.txid).is_none() {
                    return sql_fail(sql_err!(
                        sqlstate::INVALID_SCHEMA_NAME,
                        "schema \"{}\" does not exist",
                        schema
                    ));
                }
                if let Err(error) = storage.require_schema_create(schema, txn.txid) {
                    return sql_fail(error);
                }
                let schema = match SqlName::parse(schema) {
                    Ok(schema) => schema,
                    Err(error) => return sql_fail(error),
                };
                (schema, current.name)
            }
            _ => unreachable!(),
        };
        if current.schema == schema && current.name == domain_name {
            return sql_fail(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "type \"{}\" already exists",
                domain_name.as_str()
            ));
        }
        let prior = match storage.stage_domain_identity(slot, schema, domain_name, txn.txid) {
            Ok(prior) => prior,
            Err(error) => return sql_fail(error),
        };
        let lsn = storage.bump_lsn();
        if let Err(error) = wal.stage(
            txn.txid,
            lsn,
            &WalOp::AlterDomainIdentity {
                schema: current.schema.as_str(),
                name: current.name.as_str(),
                new_schema: schema.as_str(),
                new_name: domain_name.as_str(),
            },
        ) {
            storage.rollback_domain_alter(slot, prior);
            return sql_fail(error);
        }
        if let Err(error) = txn.record_ddl(super::txn::DdlUndo::DomainAltered {
            slot: slot as u32,
            prior,
        }) {
            storage.rollback_domain_alter(slot, prior);
            return sql_fail(error);
        }
        responder.command_complete("ALTER DOMAIN")?;
        return sql_ok();
    }
    // Start from the current definition and apply the action.
    let current = storage.domain_for(slot, txn.txid);
    let mut spec = crate::storage::DomainSpec {
        base_domain: current.base_domain,
        base: current.base,
        base_type_mod: current.base_type_mod,
        not_null: current.not_null,
        default_expr: current.default_expr,
        checks: current.checks,
        n_checks: current.n_checks,
    };
    let revalidate;
    match action {
        A::SetNotNull => {
            spec.not_null = true;
            revalidate = true;
        }
        A::DropNotNull => {
            spec.not_null = false;
            revalidate = false;
        }
        A::SetDefault(text) => {
            revalidate = false;
            if let Err(e) = validate_domain_expr(text, false, arena) {
                return sql_fail(e);
            }
            match domain_text::<{ crate::storage::DEFAULT_EXPR_MAX }>(text) {
                Ok(t) => spec.default_expr = Some(t),
                Err(e) => return sql_fail(e),
            }
        }
        A::DropDefault => {
            revalidate = false;
            spec.default_expr = None;
        }
        A::AddCheck(check) => {
            revalidate = true;
            if spec.n_checks == crate::storage::MAX_DOMAIN_CHECKS {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "a domain may have at most {} CHECK constraints",
                    crate::storage::MAX_DOMAIN_CHECKS
                ));
            }
            if let Err(e) = validate_domain_expr(check.expression, true, arena) {
                return sql_fail(e);
            }
            let cname = match check.name {
                Some(nm) => match SqlName::parse(nm) {
                    Ok(n) => n,
                    Err(e) => return sql_fail(e),
                },
                None => {
                    match generate_check_name(current.name.as_str(), &spec.checks[..spec.n_checks])
                    {
                        Ok(n) => n,
                        Err(e) => return sql_fail(e),
                    }
                }
            };
            let expression =
                match domain_text::<{ crate::storage::CHECK_SQL_MAX }>(check.expression) {
                    Ok(t) => t,
                    Err(e) => return sql_fail(e),
                };
            spec.checks[spec.n_checks] = crate::storage::CheckConstraint {
                name: cname,
                expression,
            };
            spec.n_checks += 1;
        }
        A::DropConstraint {
            name: cname,
            if_exists,
        } => {
            revalidate = false;
            let Some(pos) = spec.checks[..spec.n_checks]
                .iter()
                .position(|c| c.name.as_str() == *cname)
            else {
                if *if_exists {
                    responder.notice(
                        sqlstate::SUCCESSFUL_COMPLETION,
                        stack_format!(
                            128,
                            "constraint \"{}\" of domain \"{}\" does not exist, skipping",
                            cname,
                            name.name
                        )
                        .as_str(),
                    )?;
                    responder.command_complete("ALTER DOMAIN")?;
                    return sql_ok();
                }
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "constraint \"{}\" of domain \"{}\" does not exist",
                    cname,
                    name.name
                ));
            };
            for i in pos..spec.n_checks - 1 {
                spec.checks[i] = spec.checks[i + 1];
            }
            spec.n_checks -= 1;
            spec.checks[spec.n_checks] = crate::storage::CheckConstraint::EMPTY;
        }
        A::Rename(_) | A::SetSchema(_) => unreachable!(),
    }
    let prior = match storage.stage_domain_alter(slot, spec, txn.txid) {
        Ok(prior) => prior,
        Err(error) => return sql_fail(error),
    };
    if revalidate && let Err(e) = validate_domain_rows(storage, slot, txn.txid, arena) {
        storage.rollback_domain_alter(slot, prior);
        return sql_fail(e);
    }
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.stage(
        txn.txid,
        lsn,
        &WalOp::CreateDomain(storage.domain_for(slot, txn.txid)),
    ) {
        storage.rollback_domain_alter(slot, prior);
        return sql_fail(e);
    }
    if let Err(e) = txn.record_ddl(super::txn::DdlUndo::DomainAltered {
        slot: slot as u32,
        prior,
    }) {
        storage.rollback_domain_alter(slot, prior);
        return sql_fail(e);
    }
    responder.command_complete("ALTER DOMAIN")?;
    sql_ok()
}

/// Re-validates every stored scalar value whose declared domain is `target` or
/// a descendant of it. PostgreSQL refuses to ALTER a domain while an array of
/// that domain exists; scalar columns are scanned before the catalog change is
/// made visible.
fn validate_domain_rows(
    storage: &Storage,
    target: usize,
    txid: u32,
    arena: &Arena,
) -> Result<(), SqlError> {
    let target_name = storage.domain(target).name;
    for (table_index, table) in storage.live_tables() {
        let def = table.def;
        let mut affected = [false; MAX_COLUMNS];
        let mut any = false;
        for (column_index, column) in def.columns().iter().enumerate() {
            let Some(identity) = column.user_type else {
                continue;
            };
            let Some(domain_slot) =
                storage.domain_slot(identity.schema.as_str(), identity.name.as_str(), txid)
            else {
                continue;
            };
            if !domain_depends_on(storage, domain_slot, target, txid) {
                continue;
            }
            if matches!(
                column.ctype,
                ColType::Array(crate::sql::types::ArrElem::Domain { .. })
            ) {
                return Err(sql_err!(
                    sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                    "cannot alter type \"{}\" because column \"{}.{}\" uses it",
                    target_name.as_str(),
                    def.name.as_str(),
                    column.name.as_str()
                ));
            }
            affected[column_index] = true;
            any = true;
        }
        if !any {
            continue;
        }
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        def.schema(&mut schema);
        storage.for_each_row_state(table_index, &mut |rowid, state| {
            use core::ops::ControlFlow;
            let Some(home) = storage.visible_row_home(table_index, rowid, state, txid)? else {
                return Ok(ControlFlow::Continue(()));
            };
            let bytes = storage.row_bytes(table_index, rowid, home, arena)?;
            let mut values = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(bytes, &schema[..def.n_columns], &mut values)?;
            for column_index in 0..def.n_columns {
                if !affected[column_index] {
                    continue;
                }
                let identity = def.columns()[column_index]
                    .user_type
                    .expect("affected domain");
                let leaf = storage
                    .domain_slot(identity.schema.as_str(), identity.name.as_str(), txid)
                    .expect("affected domain remains visible");
                let _ = coerce_domain_value(
                    storage,
                    leaf,
                    values[column_index],
                    txid,
                    arena,
                    crate::sql::eval::NO_PARAMS,
                )?;
            }
            Ok(ControlFlow::Continue(()))
        })?;
    }
    Ok(())
}

fn domain_depends_on(storage: &Storage, mut slot: usize, target: usize, txid: u32) -> bool {
    for _ in 0..crate::storage::MAX_DOMAINS {
        if slot == target {
            return true;
        }
        let Some(parent) = storage.domain(slot).base_domain else {
            return false;
        };
        let Some(parent_slot) =
            storage.domain_slot(parent.schema.as_str(), parent.name.as_str(), txid)
        else {
            return false;
        };
        slot = parent_slot;
    }
    false
}

/// Builds an [`EnumSpec`] from a `CREATE TYPE ... AS ENUM` label list: rejects
/// duplicates (42710) and over-long labels, and assigns each member a 1-based
/// sort key (PostgreSQL's `enumsortorder`).
fn build_enum_spec(labels: &[&str]) -> Result<crate::storage::EnumSpec, SqlError> {
    if labels.len() > crate::storage::MAX_ENUM_LABELS {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "an enum type may have at most {} labels",
            crate::storage::MAX_ENUM_LABELS
        ));
    }
    let mut members = [crate::storage::EnumMember::EMPTY; crate::storage::MAX_ENUM_LABELS];
    for (i, &label) in labels.iter().enumerate() {
        if labels[..i].contains(&label) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "enum label \"{}\" specified more than once",
                label
            ));
        }
        members[i] = crate::storage::EnumMember {
            label: SqlName::parse(label)?,
            sort: (i + 1) as f64,
        };
    }
    Ok(crate::storage::EnumSpec {
        members,
        n_members: labels.len(),
    })
}

pub fn create_enum(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &QualName,
    labels: &[&str],
    responder: &mut Responder,
) -> Outcome {
    let schema = match storage.creation_schema(name.schema, name.name, txn.txid) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    // Types share a namespace: reject a name already taken by a domain or enum.
    if storage
        .enum_slot(schema.as_str(), name.name, txn.txid)
        .is_some()
        || storage
            .domain_slot(schema.as_str(), name.name, txn.txid)
            .is_some()
    {
        return sql_fail(sql_err!(
            sqlstate::DUPLICATE_OBJECT,
            "type \"{}\" already exists",
            name.name
        ));
    }
    let spec = match build_enum_spec(labels) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let sqlname = match SqlName::parse(name.name) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let slot = match storage.create_enum(schema, sqlname, spec, txn.txid) {
        Ok(slot) => slot,
        Err(e) => return sql_fail(e),
    };
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.stage(
        txn.txid,
        lsn,
        &WalOp::CreateEnum(storage.enum_for(slot, txn.txid)),
    ) {
        storage.rollback_enum_create(slot);
        return sql_fail(e);
    }
    if let Err(e) = txn.record_ddl(super::txn::DdlUndo::EnumCreated(slot as u32)) {
        return sql_fail(e);
    }
    if let Err(error) = apply_default_privileges_to_new_object(
        storage,
        txn,
        crate::storage::AccessObject {
            class: crate::storage::AccessClass::Enum,
            slot: slot as u16,
        },
    ) {
        return sql_fail(error);
    }
    responder.command_complete("CREATE TYPE")?;
    sql_ok()
}

#[allow(clippy::too_many_arguments)]
pub fn drop_enum(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    scratch: &mut FixedVec<(u64, RowHome)>,
    names: &[QualName],
    if_exists: bool,
    cascade: bool,
    arena: &Arena,
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
) -> Outcome {
    for name in names {
        let slot = match name.schema {
            Some(schema) => storage.enum_slot(schema, name.name, txn.txid),
            None => storage.resolve_enum_slot(name.name, txn.txid),
        };
        let Some(slot) = slot else {
            if if_exists {
                responder.notice(
                    sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(128, "type \"{}\" does not exist, skipping", name.name).as_str(),
                )?;
                continue;
            }
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "type \"{}\" does not exist",
                name.name
            ));
        };
        if let Err(error) = storage.require_owner(
            crate::storage::AccessObject {
                class: crate::storage::AccessClass::Enum,
                slot: slot as u16,
            },
            txn.txid,
            "type",
        ) {
            return sql_fail(error);
        }
        let (schema, ename) = {
            let e = storage.enum_for(slot, txn.txid);
            (e.schema, e.name)
        };
        while let Some((table_schema, table, _)) = enum_column_in_use(storage, slot, txn.txid) {
            if cascade {
                let table_slot = storage
                    .find_visible(table_schema.as_str(), table.as_str(), txn.txid)
                    .expect("dependent table remains visible");
                let def = storage.table_def(table_slot, txn.txid);
                let mut columns = [SqlName::EMPTY; MAX_COLUMNS];
                let mut column_count = 0;
                for column in def.columns() {
                    if matches!(
                        column.ctype,
                        ColType::Enum(enum_slot)
                            | ColType::Array(super::types::ArrElem::Enum(enum_slot))
                            if enum_slot as usize == slot
                    ) {
                        columns[column_count] = column.name;
                        column_count += 1;
                    }
                }
                if let Err(error) = cascade_drop_type_column(
                    storage,
                    wal,
                    txn,
                    scratch,
                    table_schema,
                    table,
                    &columns[..column_count],
                    arena,
                    seq_session,
                    responder,
                ) {
                    return sql_fail(error);
                }
                continue;
            }
            return sql_fail(sql_err!(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop type {} because other objects depend on it",
                ename.as_str()
            ));
        }
        let mut dependent_domains = [false; crate::storage::MAX_DOMAINS];
        for (domain_slot, is_dependent) in dependent_domains
            .iter_mut()
            .enumerate()
            .take(storage.domain_count())
        {
            let domain = storage.domain(domain_slot);
            if domain.visible_to(txn.txid)
                && matches!(
                    domain.base,
                    ColType::Enum(enum_slot)
                        | ColType::Array(super::types::ArrElem::Enum(enum_slot))
                        if enum_slot as usize == slot
                )
            {
                *is_dependent = true;
            }
        }
        let dependent_count = dependent_domains.iter().filter(|&&yes| yes).count();
        if dependent_count > 0 && !cascade {
            return sql_fail(sql_err!(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop type {} because other objects depend on it",
                ename.as_str()
            ));
        }
        if let Err(error) = drop_domain_selection(
            storage,
            wal,
            txn,
            scratch,
            &dependent_domains,
            Some(slot),
            cascade,
            1,
            arena,
            seq_session,
            responder,
        ) {
            return sql_fail(error);
        }
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.stage(
            txn.txid,
            lsn,
            &WalOp::DropEnum {
                schema: schema.as_str(),
                name: ename.as_str(),
            },
        ) {
            return sql_fail(e);
        }
        match storage.drop_enum(schema.as_str(), ename.as_str(), txn.txid) {
            Ok(Some(slot)) => {
                if let Err(e) = txn.record_ddl(super::txn::DdlUndo::EnumDropped(slot as u32)) {
                    return sql_fail(e);
                }
            }
            Ok(None) => {}
            Err(e) => return sql_fail(e),
        }
    }
    responder.command_complete("DROP TYPE")?;
    sql_ok()
}

fn apply_type_drop_to_stored_queries(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    selected_domains: &[bool; crate::storage::MAX_DOMAINS],
    selected_enum: Option<usize>,
    cascade: bool,
) -> Result<(), SqlError> {
    use crate::storage::DependencyClass;
    let root = |dependency: &crate::storage::StoredQueryDependency| match dependency.class {
        DependencyClass::Domain => selected_domains
            .get(dependency.slot as usize)
            .copied()
            .unwrap_or(false),
        DependencyClass::Enum => selected_enum == Some(dependency.slot as usize),
        _ => false,
    };
    let (views, matviews) = stored_query_dependent_closure(storage, txn.txid, root)?;
    if !views.iter().any(|selected| *selected) && !matviews.iter().any(|selected| *selected) {
        return Ok(());
    }
    if !cascade {
        return Err(sql_err!(
            sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
            "cannot drop type because a stored query depends on it"
        ));
    }
    drop_selected_stored_queries(storage, wal, txn, &views, &matviews)
}

const MAX_DEPENDENT_STORED_QUERIES: usize = 64;

fn stored_query_dependent_closure(
    storage: &Storage,
    txid: u32,
    root: impl Fn(&crate::storage::StoredQueryDependency) -> bool,
) -> Result<
    (
        [bool; MAX_DEPENDENT_STORED_QUERIES],
        [bool; MAX_DEPENDENT_STORED_QUERIES],
    ),
    SqlError,
> {
    use crate::storage::DependencyClass;
    if storage.view_count() > MAX_DEPENDENT_STORED_QUERIES
        || storage.matview_count() > MAX_DEPENDENT_STORED_QUERIES
        || storage.table_count() > MAX_DEPENDENT_STORED_QUERIES
    {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "stored-query dependency closure exceeds {} catalog slots",
            MAX_DEPENDENT_STORED_QUERIES
        ));
    }
    let mut views = [false; MAX_DEPENDENT_STORED_QUERIES];
    let mut matviews = [false; MAX_DEPENDENT_STORED_QUERIES];
    let mut matview_tables = [false; MAX_DEPENDENT_STORED_QUERIES];
    loop {
        let mut changed = false;
        for slot in 0..storage.view_count() {
            let view = storage.view(slot);
            if views[slot] || !view.visible_to(txid) {
                continue;
            }
            let hit = storage
                .view_dependencies(slot)
                .entries()
                .iter()
                .any(|dependency| {
                    root(dependency)
                        || (dependency.class == DependencyClass::View
                            && views[dependency.slot as usize])
                        || (dependency.class == DependencyClass::Table
                            && matview_tables[dependency.slot as usize])
                });
            if hit {
                views[slot] = true;
                changed = true;
            }
        }
        let mut slot = 0;
        while slot < storage.matview_count() {
            let matview = storage.matview(slot);
            if matviews[slot] || !matview.visible_to(txid) {
                slot += 1;
                continue;
            }
            let hit = storage
                .matview_dependencies(slot)
                .entries()
                .iter()
                .any(|dependency| {
                    root(dependency)
                        || (dependency.class == DependencyClass::View
                            && views[dependency.slot as usize])
                        || (dependency.class == DependencyClass::Table
                            && matview_tables[dependency.slot as usize])
                });
            if hit {
                matviews[slot] = true;
                let table = storage
                    .find_visible(matview.schema.as_str(), matview.name.as_str(), txid)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_TABLE,
                            "materialized view \"{}\" has no backing table",
                            matview.name.as_str()
                        )
                    })?;
                matview_tables[table] = true;
                changed = true;
            }
            slot += 1;
        }
        if !changed {
            break;
        }
    }
    Ok((views, matviews))
}

#[derive(Clone, Copy)]
struct StoredQueryRoot {
    class: crate::storage::DependencyClass,
    slot: usize,
    kind: &'static str,
    schema: SqlName,
    name: SqlName,
}

#[derive(Clone, Copy)]
struct StoredQuerySelection<'a> {
    views: &'a [bool; MAX_DEPENDENT_STORED_QUERIES],
    matviews: &'a [bool; MAX_DEPENDENT_STORED_QUERIES],
}

fn report_stored_query_dependents(
    storage: &Storage,
    txid: u32,
    root: StoredQueryRoot,
    selection: StoredQuerySelection<'_>,
    cascade: bool,
    responder: &mut Responder,
) -> Result<(), SqlError> {
    use crate::storage::DependencyClass;
    use core::fmt::Write as _;

    let views = selection.views;
    let matviews = selection.matviews;
    let count = views.iter().filter(|selected| **selected).count()
        + matviews.iter().filter(|selected| **selected).count();
    if count == 0 {
        return Ok(());
    }

    // A dependency report is parent-before-child. Derive a bounded depth from
    // the same graph used for selection so a table→view→matview→view chain is
    // rendered in PostgreSQL's order.
    let mut view_depth = [0u8; MAX_DEPENDENT_STORED_QUERIES];
    let mut matview_depth = [0u8; MAX_DEPENDENT_STORED_QUERIES];
    loop {
        let mut changed = false;
        for slot in 0..storage.view_count() {
            if !views[slot] || view_depth[slot] != 0 {
                continue;
            }
            let mut depth = 0u8;
            for dependency in storage.view_dependencies(slot).entries() {
                let parent_depth = match dependency.class {
                    class if class == root.class && dependency.slot as usize == root.slot => 1,
                    DependencyClass::View => view_depth[dependency.slot as usize]
                        .checked_add(1)
                        .filter(|_| view_depth[dependency.slot as usize] != 0)
                        .unwrap_or(0),
                    DependencyClass::Table => {
                        let mut found = 0;
                        for matview_slot in 0..storage.matview_count() {
                            if !matviews[matview_slot] || matview_depth[matview_slot] == 0 {
                                continue;
                            }
                            let matview = storage.matview(matview_slot);
                            if storage.find_visible(
                                matview.schema.as_str(),
                                matview.name.as_str(),
                                txid,
                            ) == Some(dependency.slot as usize)
                            {
                                found = matview_depth[matview_slot].saturating_add(1);
                                break;
                            }
                        }
                        found
                    }
                    _ => 0,
                };
                if parent_depth != 0 {
                    depth = parent_depth;
                    break;
                }
            }
            if depth != 0 {
                view_depth[slot] = depth;
                changed = true;
            }
        }
        for slot in 0..storage.matview_count() {
            if !matviews[slot] || matview_depth[slot] != 0 {
                continue;
            }
            let mut depth = 0u8;
            for dependency in storage.matview_dependencies(slot).entries() {
                let parent_depth = match dependency.class {
                    class if class == root.class && dependency.slot as usize == root.slot => 1,
                    DependencyClass::View if view_depth[dependency.slot as usize] != 0 => {
                        view_depth[dependency.slot as usize].saturating_add(1)
                    }
                    _ => 0,
                };
                if parent_depth != 0 {
                    depth = parent_depth;
                    break;
                }
            }
            if depth != 0 {
                matview_depth[slot] = depth;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let in_path = |schema: &str| {
        storage.path().entries().iter().any(|entry| match entry {
            crate::storage::PathEntry::Schema(slot) => {
                storage.schema_def(*slot as usize).name.as_str() == schema
            }
            crate::storage::PathEntry::Catalog => schema == "pg_catalog",
        })
    };
    let write_name = |out: &mut crate::util::StackStr<192>, schema: &SqlName, name: &SqlName| {
        if in_path(schema.as_str()) {
            let _ = write!(out, "{}", name.as_str());
        } else {
            let _ = write!(out, "{}.{}", schema.as_str(), name.as_str());
        }
    };
    let describe_view = |slot: usize, out: &mut crate::util::StackStr<192>| {
        let view = storage.view(slot);
        let _ = write!(out, "view ");
        write_name(out, &view.schema, &view.name);
    };
    let describe_matview = |slot: usize, out: &mut crate::util::StackStr<192>| {
        let matview = storage.matview(slot);
        let _ = write!(out, "materialized view ");
        write_name(out, &matview.schema, &matview.name);
    };
    let mut detail =
        crate::util::StackStr::<{ crate::sql::eval::MAX_DIAGNOSTIC_DETAIL_BYTES }>::new();
    let mut written = 0usize;
    for depth in 1..=MAX_DEPENDENT_STORED_QUERIES as u8 {
        for slot in 0..storage.view_count() {
            if views[slot] && view_depth[slot] == depth {
                let mut object = crate::util::StackStr::<192>::new();
                describe_view(slot, &mut object);
                let _ = write!(
                    detail,
                    "{}{}{}",
                    if written == 0 { "" } else { "\n" },
                    if cascade { "drop cascades to " } else { "" },
                    object.as_str()
                );
                if !cascade {
                    let mut parent = crate::util::StackStr::<192>::new();
                    if depth == 1 {
                        let _ = write!(parent, "{} ", root.kind);
                        write_name(&mut parent, &root.schema, &root.name);
                    } else {
                        for dependency in storage.view_dependencies(slot).entries() {
                            if dependency.class == DependencyClass::View
                                && view_depth[dependency.slot as usize] == depth - 1
                            {
                                describe_view(dependency.slot as usize, &mut parent);
                                break;
                            }
                            if dependency.class == DependencyClass::Table {
                                for (matview_slot, &parent_depth) in matview_depth
                                    .iter()
                                    .enumerate()
                                    .take(storage.matview_count())
                                {
                                    let matview = storage.matview(matview_slot);
                                    if parent_depth == depth - 1
                                        && storage.find_visible(
                                            matview.schema.as_str(),
                                            matview.name.as_str(),
                                            txid,
                                        ) == Some(dependency.slot as usize)
                                    {
                                        describe_matview(matview_slot, &mut parent);
                                        break;
                                    }
                                }
                            }
                            if !parent.as_str().is_empty() {
                                break;
                            }
                        }
                    }
                    let _ = write!(detail, " depends on {}", parent.as_str());
                }
                written += 1;
            }
        }
        for slot in 0..storage.matview_count() {
            if matviews[slot] && matview_depth[slot] == depth {
                let mut object = crate::util::StackStr::<192>::new();
                describe_matview(slot, &mut object);
                let _ = write!(
                    detail,
                    "{}{}{}",
                    if written == 0 { "" } else { "\n" },
                    if cascade { "drop cascades to " } else { "" },
                    object.as_str()
                );
                if !cascade {
                    let mut parent = crate::util::StackStr::<192>::new();
                    if depth == 1 {
                        let _ = write!(parent, "{} ", root.kind);
                        write_name(&mut parent, &root.schema, &root.name);
                    } else if let Some(dependency) = storage
                        .matview_dependencies(slot)
                        .entries()
                        .iter()
                        .find(|dependency| {
                            dependency.class == DependencyClass::View
                                && view_depth[dependency.slot as usize] == depth - 1
                        })
                    {
                        describe_view(dependency.slot as usize, &mut parent);
                    }
                    let _ = write!(detail, " depends on {}", parent.as_str());
                }
                written += 1;
            }
        }
    }
    if written != count || detail.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "stored-query dependency report exceeds its fixed buffer"
        ));
    }

    if cascade {
        if count == 1 {
            responder
                .notice(sqlstate::SUCCESSFUL_COMPLETION, detail.as_str())
                .map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "response buffer full"))?;
        } else {
            crate::sql::eval::stash_diagnostic(detail, None);
            responder
                .notice(
                    sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(128, "drop cascades to {} other objects", count).as_str(),
                )
                .map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "response buffer full"))?;
        }
    } else {
        let mut hint = crate::util::StackStr::<128>::new();
        let _ = write!(
            hint,
            "Use DROP ... CASCADE to drop the dependent objects too."
        );
        crate::sql::eval::stash_diagnostic(detail, Some(hint));
    }
    Ok(())
}

fn drop_selected_stored_queries(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    views: &[bool; MAX_DEPENDENT_STORED_QUERIES],
    matviews: &[bool; MAX_DEPENDENT_STORED_QUERIES],
) -> Result<(), SqlError> {
    // Dependents are already closed transitively. Reverse slot order avoids
    // immediately reusing a just-freed low slot while this plan is executing.
    for slot in (0..storage.view_count()).rev() {
        if views[slot] && storage.view(slot).visible_to(txn.txid) {
            drop_view_slot(storage, wal, txn, slot)?;
        }
    }
    for slot in (0..storage.matview_count()).rev() {
        if matviews[slot] && storage.matview(slot).visible_to(txn.txid) {
            drop_matview_slot(storage, wal, txn, slot)?;
        }
    }
    Ok(())
}

fn drop_view_slot(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    slot: usize,
) -> Result<(), SqlError> {
    let (schema, name) = {
        let view = storage.view(slot);
        (view.schema, view.name)
    };
    let lsn = storage.bump_lsn();
    wal.stage(
        txn.txid,
        lsn,
        &WalOp::DropView {
            schema: schema.as_str(),
            name: name.as_str(),
        },
    )?;
    if let Some(slot) = storage.drop_view(schema.as_str(), name.as_str(), txn.txid)? {
        txn.record_ddl(super::txn::DdlUndo::ViewDropped(slot as u32))?;
    }
    Ok(())
}

fn drop_matview_slot(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    slot: usize,
) -> Result<(), SqlError> {
    let (schema, name) = {
        let matview = storage.matview(slot);
        (matview.schema, matview.name)
    };
    let table = storage
        .find_visible(schema.as_str(), name.as_str(), txn.txid)
        .ok_or_else(|| undefined_kind("materialized view", name.as_str()))?;
    let lsn = storage.bump_lsn();
    wal.stage(
        txn.txid,
        lsn,
        &WalOp::DropTable {
            schema: schema.as_str(),
            name: name.as_str(),
        },
    )?;
    txn.record_ddl(super::txn::DdlUndo::Dropped(table as u32))?;
    storage.drop_table_in(table, txn.txid);
    storage.drop_indexes_for(schema.as_str(), name.as_str(), txn.txid);
    let lsn = storage.bump_lsn();
    wal.stage(
        txn.txid,
        lsn,
        &WalOp::DropMatview {
            schema: schema.as_str(),
            name: name.as_str(),
        },
    )?;
    if let Some(slot) = storage.drop_matview(schema.as_str(), name.as_str(), txn.txid)? {
        txn.record_ddl(super::txn::DdlUndo::MatviewDropped(slot as u32))?;
    }
    Ok(())
}

fn enum_column_in_use(
    storage: &Storage,
    enum_slot: usize,
    txid: u32,
) -> Option<(SqlName, SqlName, SqlName)> {
    for table_index in 0..storage.table_count() {
        let table = storage.table(table_index);
        if !table.visible_to(txid) {
            continue;
        }
        let def = storage.table_def(table_index, txid);
        for column in def.columns() {
            if matches!(column.ctype, ColType::Enum(slot) if slot as usize == enum_slot)
                || matches!(
                    column.ctype,
                    ColType::Array(super::types::ArrElem::Enum(slot))
                        if slot as usize == enum_slot
                )
            {
                return Some((def.schema, def.name, column.name));
            }
        }
    }
    None
}

pub fn alter_type(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    name: &QualName,
    action: &crate::sql::ast::AlterTypeAction,
    arena: &Arena,
    responder: &mut Responder,
) -> Outcome {
    use crate::sql::ast::AlterTypeAction as A;
    let slot = match name.schema {
        Some(schema) => storage.enum_slot(schema, name.name, txn.txid),
        None => storage.resolve_enum_slot(name.name, txn.txid),
    };
    let Some(slot) = slot else {
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "type \"{}\" does not exist",
            name.name
        ));
    };
    if let Err(error) = storage.require_owner(
        crate::storage::AccessObject {
            class: crate::storage::AccessClass::Enum,
            slot: slot as u16,
        },
        txn.txid,
        "type",
    ) {
        return sql_fail(error);
    }
    match action {
        A::AddValue {
            label,
            if_not_exists,
            before,
            after,
        } => {
            let current = storage.enum_for(slot, txn.txid);
            if current.sort_of(label).is_some() {
                if *if_not_exists {
                    responder.notice(
                        sqlstate::DUPLICATE_OBJECT,
                        stack_format!(128, "enum label \"{}\" already exists, skipping", label)
                            .as_str(),
                    )?;
                    responder.command_complete("ALTER TYPE")?;
                    return sql_ok();
                }
                return sql_fail(sql_err!(
                    sqlstate::DUPLICATE_OBJECT,
                    "enum label \"{}\" already exists",
                    label
                ));
            }
            if current.n_members >= crate::storage::MAX_ENUM_LABELS {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "an enum type may have at most {} labels",
                    crate::storage::MAX_ENUM_LABELS
                ));
            }
            let sort = match compute_add_value_sort(&current, before.as_deref(), after.as_deref()) {
                Ok(s) => s,
                Err(e) => return sql_fail(e),
            };
            let new_label = match SqlName::parse(label) {
                Ok(n) => n,
                Err(e) => return sql_fail(e),
            };
            let mut altered = current;
            altered.members[altered.n_members] = crate::storage::EnumMember {
                label: new_label,
                sort,
            };
            altered.n_members += 1;
            let prior = match storage.stage_enum_alter(slot, altered, txn.txid) {
                Ok(prior) => prior,
                Err(error) => return sql_fail(error),
            };
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.stage(
                txn.txid,
                lsn,
                &WalOp::CreateEnum(storage.enum_for(slot, txn.txid)),
            ) {
                storage.rollback_enum_alter(slot, prior);
                return sql_fail(e);
            }
            if let Err(e) = txn.record_ddl(super::txn::DdlUndo::EnumAltered {
                slot: slot as u32,
                prior,
            }) {
                storage.rollback_enum_alter(slot, prior);
                return sql_fail(e);
            }
        }
        A::RenameTo(new_name) => {
            let current = storage.enum_for(slot, txn.txid);
            if current.name.as_str() == *new_name {
                responder.command_complete("ALTER TYPE")?;
                return sql_ok();
            }
            if storage
                .enum_slot(current.schema.as_str(), new_name, txn.txid)
                .is_some()
                || storage
                    .domain_slot(current.schema.as_str(), new_name, txn.txid)
                    .is_some()
            {
                return sql_fail(sql_err!(
                    sqlstate::DUPLICATE_OBJECT,
                    "type \"{}\" already exists",
                    new_name
                ));
            }
            let renamed = match SqlName::parse(new_name) {
                Ok(name) => name,
                Err(e) => return sql_fail(e),
            };
            let mut altered = current;
            altered.name = renamed;
            let prior = match storage.stage_enum_alter(slot, altered, txn.txid) {
                Ok(prior) => prior,
                Err(error) => return sql_fail(error),
            };
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.stage(
                txn.txid,
                lsn,
                &WalOp::RenameEnum {
                    schema: current.schema.as_str(),
                    old_name: current.name.as_str(),
                    new_name,
                },
            ) {
                storage.rollback_enum_alter(slot, prior);
                return sql_fail(e);
            }
            if let Err(e) = txn.record_ddl(super::txn::DdlUndo::EnumAltered {
                slot: slot as u32,
                prior,
            }) {
                storage.rollback_enum_alter(slot, prior);
                return sql_fail(e);
            }
        }
        A::RenameValue { from, to } => {
            let current = storage.enum_for(slot, txn.txid);
            let Some(member_index) = current
                .members()
                .iter()
                .position(|member| member.label.as_str() == *from)
            else {
                return sql_fail(sql_err!(
                    sqlstate::INVALID_TEXT_REPRESENTATION,
                    "\"{}\" is not an existing enum label",
                    from
                ));
            };
            if current.sort_of(to).is_some() {
                return sql_fail(sql_err!(
                    sqlstate::DUPLICATE_OBJECT,
                    "enum label \"{}\" already exists",
                    to
                ));
            }
            let renamed = match SqlName::parse(to) {
                Ok(label) => label,
                Err(e) => return sql_fail(e),
            };
            let mut altered = current;
            altered.members[member_index].label = renamed;
            let prior = match storage.stage_enum_alter(slot, altered, txn.txid) {
                Ok(prior) => prior,
                Err(error) => return sql_fail(error),
            };
            if let Err(e) =
                rewrite_enum_label(storage, txn, slot as u16, from, renamed.as_str(), arena)
            {
                storage.rollback_enum_alter(slot, prior);
                return sql_fail(e);
            }
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.stage(
                txn.txid,
                lsn,
                &WalOp::CreateEnum(storage.enum_for(slot, txn.txid)),
            ) {
                storage.rollback_enum_alter(slot, prior);
                return sql_fail(e);
            }
            if let Err(e) = txn.record_ddl(super::txn::DdlUndo::EnumAltered {
                slot: slot as u32,
                prior,
            }) {
                storage.rollback_enum_alter(slot, prior);
                return sql_fail(e);
            }
        }
    }
    responder.command_complete("ALTER TYPE")?;
    sql_ok()
}

/// Rewrites the inline label carried by every stored value of one enum. The
/// sort key and type slot are stable, so comparisons and indexes keep their
/// identity; scalar enum columns, enum arrays, and domain arrays over the enum
/// all pass through this one walk. Writes are ordinary transaction-pending row
/// changes, hence rollback/savepoint semantics come for free.
fn rewrite_enum_label(
    storage: &mut Storage,
    txn: &mut TxnState,
    enum_slot: u16,
    from: &str,
    to: &str,
    arena: &Arena,
) -> Result<(), SqlError> {
    for table_index in 0..storage.table_count() {
        if !storage.table(table_index).visible_to(txn.txid) {
            continue;
        }
        let def = *storage.table_def(table_index, txn.txid);
        let mut affected = [false; MAX_COLUMNS];
        let mut any = false;
        for (column_index, column) in def.columns().iter().enumerate() {
            let hit = matches!(column.ctype, ColType::Enum(slot) if slot == enum_slot)
                || matches!(
                    column.ctype,
                    ColType::Array(element)
                        if matches!(element.to_coltype(), ColType::Enum(slot) if slot == enum_slot)
                );
            affected[column_index] = hit;
            any |= hit;
        }
        if !any {
            continue;
        }
        let count = storage.visible_row_count(table_index, txn.txid)?;
        let rows = arena
            .alloc_slice_with(count, |_| None::<(u64, RowHome)>)
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "enum rename exceeds the statement arena"
                )
            })?;
        let mut n = 0;
        storage.for_each_row_state(table_index, &mut |rowid, state| {
            if let Some(home) = storage.visible_row_home(table_index, rowid, state, txn.txid)? {
                rows[n] = Some((rowid, home));
                n += 1;
            }
            Ok(core::ops::ControlFlow::Continue(()))
        })?;
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        def.schema(&mut schema);
        for entry in rows[..n].iter().copied().flatten() {
            let (rowid, home) = entry;
            let source = storage.row_bytes(table_index, rowid, home, arena)?;
            let bytes = arena.alloc_slice_copy(source).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "enum rename exceeds the statement arena"
                )
            })?;
            let mut values = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(bytes, &schema[..def.n_columns], &mut values)?;
            let mut changed = false;
            for column_index in 0..def.n_columns {
                if !affected[column_index] {
                    continue;
                }
                match values[column_index] {
                    Datum::Enum { slot, sort, label } if slot == enum_slot && label == from => {
                        values[column_index] = Datum::Enum {
                            slot,
                            sort,
                            label: arena.alloc_str(to).map_err(|_| {
                                sql_err!(
                                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                    "enum rename exceeds the statement arena"
                                )
                            })?,
                        };
                        changed = true;
                    }
                    Datum::Array { element, raw } => {
                        let shape = crate::sql::array::shape(raw).expect("array datum invariant");
                        let count = crate::sql::array::len(raw);
                        let mut items = [Datum::Null; crate::sql::array::MAX_ELEMENTS];
                        let mut array_changed = false;
                        for (index, item) in items.iter_mut().take(count).enumerate() {
                            *item =
                                crate::sql::array::get(raw, element, index).unwrap_or(Datum::Null);
                            if let Datum::Enum { slot, sort, label } = *item
                                && slot == enum_slot
                                && label == from
                            {
                                *item = Datum::Enum {
                                    slot,
                                    sort,
                                    label: arena.alloc_str(to).map_err(|_| {
                                        sql_err!(
                                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                            "enum rename exceeds the statement arena"
                                        )
                                    })?,
                                };
                                array_changed = true;
                            }
                        }
                        if array_changed {
                            values[column_index] = Datum::Array {
                                element,
                                raw: crate::sql::array::build_shaped(
                                    &items[..count],
                                    shape,
                                    arena,
                                )?,
                            };
                            changed = true;
                        }
                    }
                    _ => {}
                }
            }
            if changed {
                store_row(
                    storage,
                    txn,
                    table_index,
                    Some(rowid),
                    &values[..def.n_columns],
                )?;
            }
        }
    }
    Ok(())
}

/// The sort key for a new enum member: appended past the current maximum, or —
/// with BEFORE/AFTER — midway between the named neighbour and its adjacent
/// member (fractional, so existing members and stored rows are undisturbed).
fn compute_add_value_sort(
    def: &crate::storage::EnumDef,
    before: Option<&str>,
    after: Option<&str>,
) -> Result<f64, SqlError> {
    let members = def.members();
    let neighbour = before.or(after);
    let Some(pivot) = neighbour else {
        // Append: one past the current maximum sort (or 1.0 for an empty enum).
        let max = members
            .iter()
            .map(|m| m.sort)
            .fold(f64::NEG_INFINITY, f64::max);
        return Ok(if members.is_empty() { 1.0 } else { max + 1.0 });
    };
    let Some(pivot_sort) = def.sort_of(pivot) else {
        return Err(sql_err!(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            "\"{}\" is not an existing enum label",
            pivot
        ));
    };
    // Sorted neighbours around the pivot bound the fractional insertion.
    let mut sorts: [f64; crate::storage::MAX_ENUM_LABELS] = [0.0; crate::storage::MAX_ENUM_LABELS];
    for (i, m) in members.iter().enumerate() {
        sorts[i] = m.sort;
    }
    sorts[..members.len()].sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pos = sorts[..members.len()]
        .iter()
        .position(|&s| s == pivot_sort)
        .unwrap();
    let new_sort = if before.is_some() {
        let lower = if pos == 0 {
            pivot_sort - 1.0
        } else {
            sorts[pos - 1]
        };
        (lower + pivot_sort) / 2.0
    } else {
        let upper = if pos + 1 == members.len() {
            pivot_sort + 1.0
        } else {
            sorts[pos + 1]
        };
        (pivot_sort + upper) / 2.0
    };
    Ok(new_sort)
}

/// Resolves a name to a live sequence slot. A relation that exists but is not a
/// sequence is a type error (42809); a missing relation is `Ok(None)` so the
/// caller can apply IF EXISTS.
fn resolve_sequence(
    storage: &Storage,
    name: &QualName,
    txid: u32,
) -> Result<Option<usize>, SqlError> {
    // A qualifier names the schema directly; otherwise search the path.
    if let Some(slot) = storage.sequence_on_path(name.schema, name.name, txid) {
        return Ok(Some(slot));
    }
    // Not a sequence: distinguish a wrong-type relation (42809) from absence.
    if storage
        .resolve_relation(name.schema, name.name, txid)
        .is_some()
    {
        return Err(sql_err!(
            sqlstate::WRONG_OBJECT_TYPE,
            "\"{}\" is not a sequence",
            name.name
        ));
    }
    Ok(None)
}

/// DROP VIEW [IF EXISTS].
pub fn drop_view(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut super::txn::TxnState,
    names: &[QualName],
    if_exists: bool,
    cascade: bool,
    responder: &mut Responder,
) -> Outcome {
    for name in names {
        if let Some(schema) = name.schema
            && storage.find_schema_visible(schema, txn.txid).is_none()
            && !if_exists
        {
            return sql_fail(sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "schema \"{}\" does not exist",
                schema
            ));
        }
        let found = match storage.resolve_relation(name.schema, name.name, txn.txid) {
            Some(crate::storage::ResolvedRelation::View(slot)) => Some(slot),
            _ => None,
        };
        if let Some(slot) = found {
            if let Err(error) = storage.require_owner(
                crate::storage::AccessObject {
                    class: crate::storage::AccessClass::View,
                    slot: slot as u16,
                },
                txn.txid,
                "view",
            ) {
                return sql_fail(error);
            }
            let (schema, view_name) = {
                let v = storage.view(slot);
                (v.schema, v.name)
            };
            let closure = stored_query_dependent_closure(storage, txn.txid, |dependency| {
                dependency.class == crate::storage::DependencyClass::View
                    && dependency.slot as usize == slot
            });
            let (dependent_views, dependent_matviews) = match closure {
                Ok(closure) => closure,
                Err(error) => return sql_fail(error),
            };
            let has_dependents = dependent_views.iter().any(|selected| *selected)
                || dependent_matviews.iter().any(|selected| *selected);
            if has_dependents && !cascade {
                return sql_fail(sql_err!(
                    sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                    "cannot drop view {} because other objects depend on it",
                    view_name.as_str()
                ));
            }
            if cascade
                && let Err(error) = drop_selected_stored_queries(
                    storage,
                    wal,
                    txn,
                    &dependent_views,
                    &dependent_matviews,
                )
            {
                return sql_fail(error);
            }
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.stage(
                txn.txid,
                lsn,
                &WalOp::DropView {
                    schema: schema.as_str(),
                    name: view_name.as_str(),
                },
            ) {
                return sql_fail(e);
            }
            let dropped = match storage.drop_view(schema.as_str(), view_name.as_str(), txn.txid) {
                Ok(d) => d,
                Err(e) => return sql_fail(e),
            };
            if let Some(slot) = dropped
                && let Err(e) = txn.record_ddl(super::txn::DdlUndo::ViewDropped(slot as u32))
            {
                return sql_fail(e);
            }
        } else if if_exists {
            responder.notice(
                crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                stack_format!(128, "view \"{}\" does not exist, skipping", name.name).as_str(),
            )?;
        } else {
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "view \"{}\" does not exist",
                name.name
            ));
        }
    }
    responder.command_complete("DROP VIEW")?;
    sql_ok()
}

/// CREATE [UNIQUE] INDEX: registers a durable btree index over a table's
/// columns. A UNIQUE index validates the existing image before publication.
#[allow(clippy::too_many_arguments)]
pub fn create_index(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut super::txn::TxnState,
    name: &str,
    table: &QualName,
    index_columns: &[crate::sql::ast::IndexColumn<'_>],
    include_column_names: &[&str],
    nulls_not_distinct: bool,
    predicate_expression: Option<&Expr<'_>>,
    predicate_text: Option<&str>,
    arena: &Arena,
    unique: bool,
    responder: &mut Responder,
) -> Outcome {
    use crate::storage::{IndexDef, MAX_INDEX_COLS};
    let table_index = match resolve_dml_table(storage, table, txn.txid) {
        Ok(i) => i,
        Err(e) => return sql_fail(e),
    };
    if let Err(error) = storage.require_owner(
        storage.table_access_object(table_index, txn.txid),
        txn.txid,
        "table",
    ) {
        return sql_fail(error);
    }
    if let Err(error) = storage.lock_table(
        txn.txid,
        table_index,
        crate::sql::ast::TableLockMode::Share,
        false,
    ) {
        return sql_fail(error);
    }
    let tdef = *storage.table_def(table_index, txn.txid);
    if let Some(predicate_expression) = predicate_expression {
        let (type_oid, _) = match infer_type_pub(predicate_expression, Some(&tdef)) {
            Ok(value) => value,
            Err(error) => return sql_fail(error),
        };
        if type_oid != crate::sql::types::oid::BOOL {
            return sql_fail(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "argument of WHERE must be type boolean"
            ));
        }
    }
    if index_columns.is_empty() || index_columns.len() > MAX_INDEX_COLS {
        return sql_fail(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "an index must have 1..={} columns",
            MAX_INDEX_COLS
        ));
    }
    let mut columns = [0u16; MAX_INDEX_COLS];
    let mut expressions = [None; MAX_INDEX_COLS];
    let mut include_columns = [0u16; MAX_INDEX_COLS];
    let mut descending = [false; MAX_INDEX_COLS];
    let mut nulls_first = [false; MAX_INDEX_COLS];
    for (i, index_column) in index_columns.iter().enumerate() {
        if let Some(column_name) = index_column.column {
            let Some(column_index) = tdef.column_index(column_name) else {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    column_name
                ));
            };
            columns[i] = column_index as u16;
        } else {
            if let Err(error) = infer_type_pub(index_column.expression, Some(&tdef)) {
                return sql_fail(error);
            }
            expressions[i] =
                match crate::storage::index_expression_stackstr(index_column.expression_text) {
                    Ok(expression) => Some(expression),
                    Err(error) => return sql_fail(error),
                };
        }
        descending[i] = index_column.descending;
        nulls_first[i] = index_column.nulls_first;
    }
    if include_column_names.len() > MAX_INDEX_COLS {
        return sql_fail(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "an index may include at most {} columns",
            MAX_INDEX_COLS
        ));
    }
    for (i, name) in include_column_names.iter().enumerate() {
        let Some(column_index) = tdef.column_index(name) else {
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_COLUMN,
                "column \"{}\" does not exist",
                name
            ));
        };
        if columns[..index_columns.len()]
            .iter()
            .enumerate()
            .any(|(key, column)| expressions[key].is_none() && *column == column_index as u16)
            || include_columns[..i].contains(&(column_index as u16))
        {
            return sql_fail(sql_err!(
                sqlstate::DUPLICATE_COLUMN,
                "column \"{}\" appears more than once in index definition",
                name
            ));
        }
        include_columns[i] = column_index as u16;
    }
    // The written column list's length — not the fixed array's, whose
    // padding would quietly widen the index's tuple (a UNIQUE index on (b)
    // must not enforce uniqueness of (b, first-column) instead).
    let n_cols = index_columns.len();
    let sqlname = match SqlName::parse(name) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let predicate = match predicate_text {
        Some(text) => match crate::storage::index_predicate_stackstr(text) {
            Ok(predicate) => Some(predicate),
            Err(error) => return sql_fail(error),
        },
        None => None,
    };
    let def = IndexDef {
        schema: tdef.schema,
        name: sqlname,
        pending_name: None,
        table: tdef.name,
        ownership: crate::storage::Ownership::BOOTSTRAP,
        columns,
        expressions,
        include_columns,
        descending,
        nulls_first,
        n_cols,
        n_include_cols: include_column_names.len(),
        nulls_not_distinct,
        predicate,
        unique,
        ddl_state: crate::storage::CatalogDdlState::Present,
    };
    // Register first so the UNIQUE validation below sees this index; on any
    // failure the registration is rolled back.
    let slot = match storage.create_index(def, txn.txid) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    tdef.schema(&mut schema);
    // Validate every existing tuple against the physical generation limit.
    // UNIQUE additionally checks authoritative rows for duplicate keys. A
    // conflict is deferred so rollback can remove the pending catalog entry
    // after the shared row walk releases its borrows.
    let validation = storage.for_each_row_state(table_index, &mut |rowid, state| {
        use core::ops::ControlFlow;
        let Some(home) = state.committed else {
            return Ok(ControlFlow::Continue(()));
        };
        storage.with_row_bytes(table_index, rowid, home, |bytes| {
            let mut values = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(bytes, &schema[..tdef.n_columns], &mut values)?;
            if predicate_expression.is_none() && expressions[..n_cols].iter().all(Option::is_none) {
                check_index_tuple_size(&columns[..n_cols], &values[..tdef.n_columns])?;
            }
            if unique {
                check_unique_indexes(
                    storage,
                    table_index,
                    &tdef,
                    &schema[..tdef.n_columns],
                    &values[..tdef.n_columns],
                    Some(rowid),
                    // The just-registered index is an uncommitted CREATE owned
                    // by this transaction; validation must see it.
                    txn.txid,
                    arena,
                )?;
            }
            Ok(())
        })?;
        Ok(ControlFlow::Continue(()))
    });
    if let Err(error) = validation {
        storage.rollback_index_create(slot);
        return sql_fail(error);
    }
    if let Err(error) = storage.prepare_index_enforcers(table_index, txn.txid) {
        storage.rollback_index_create(slot);
        storage
            .refresh_enforcers(table_index)
            .expect("rolling back a pending index restores the prior cache shape");
        return sql_fail(error);
    }
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.stage(
        txn.txid,
        lsn,
        &WalOp::CreateIndex {
            schema: tdef.schema.as_str(),
            name,
            table: tdef.name.as_str(),
            columns,
            expressions: expressions
                .each_ref()
                .map(|expression| expression.as_ref().map(|text| text.as_str())),
            include_columns,
            descending,
            nulls_first,
            n_cols,
            n_include_cols: include_column_names.len(),
            nulls_not_distinct,
            predicate: predicate.as_ref().map(|text| text.as_str()),
            unique,
        },
    ) {
        storage.rollback_index_create(slot);
        storage
            .refresh_enforcers(table_index)
            .expect("failed index WAL restores the prior cache shape");
        return sql_fail(e);
    }
    if let Err(e) = txn.record_ddl(super::txn::DdlUndo::IndexCreated(slot as u32)) {
        storage.rollback_index_create(slot);
        storage
            .refresh_enforcers(table_index)
            .expect("failed DDL undo reservation restores the prior cache shape");
        return sql_fail(e);
    }
    responder.command_complete("CREATE INDEX")?;
    sql_ok()
}

pub fn alter_index(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut super::txn::TxnState,
    name: &QualName,
    if_exists: bool,
    action: crate::sql::ast::AlterIndexAction,
    responder: &mut Responder,
) -> Outcome {
    let schema = match name.schema {
        Some(schema) => match storage.find_schema_visible(schema, txn.txid) {
            Some(slot) => storage
                .index_slot(schema, name.name, txn.txid)
                .map(|_| storage.schema_def(slot).name),
            None if if_exists => None,
            None => {
                return sql_fail(sql_err!(
                    sqlstate::INVALID_SCHEMA_NAME,
                    "schema \"{}\" does not exist",
                    schema
                ));
            }
        },
        None => storage
            .path()
            .entries()
            .iter()
            .find_map(|entry| match entry {
                crate::storage::PathEntry::Schema(slot) => {
                    let schema = storage.schema_def(*slot as usize).name;
                    storage
                        .index_slot(schema.as_str(), name.name, txn.txid)
                        .map(|_| schema)
                }
                crate::storage::PathEntry::Catalog => None,
            }),
    };
    let Some(schema) = schema else {
        if if_exists {
            responder.notice(
                crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                stack_format!(128, "index \"{}\" does not exist, skipping", name.name).as_str(),
            )?;
            return Ok(Ok(responder.command_complete("ALTER INDEX")?));
        }
        return sql_fail(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "index \"{}\" does not exist",
            name.name
        ));
    };
    let slot = storage
        .index_slot(schema.as_str(), name.name, txn.txid)
        .expect("resolved index exists");
    let object = crate::storage::AccessObject {
        class: crate::storage::AccessClass::Index,
        slot: slot as u16,
    };
    if let Err(error) = storage.require_owner(object, txn.txid, "index") {
        return sql_fail(error);
    }
    let Some(table) = storage.index_table_slot(slot) else {
        return sql_fail(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "index \"{}\" has no table",
            name.name
        ));
    };
    if let Err(error) = storage.lock_table(
        txn.txid,
        table,
        crate::sql::ast::TableLockMode::ShareUpdateExclusive,
        false,
    ) {
        return sql_fail(error);
    }
    match action {
        crate::sql::ast::AlterIndexAction::Rename(new_name) => {
            let new_name = match SqlName::parse(new_name) {
                Ok(name) => name,
                Err(error) => return sql_fail(error),
            };
            let prior = match storage.rename_index(slot, new_name, txn.txid) {
                Ok(prior) => prior,
                Err(error) => return sql_fail(error),
            };
            let lsn = storage.bump_lsn();
            if let Err(error) = wal.stage(
                txn.txid,
                lsn,
                &WalOp::RenameIndex {
                    schema: schema.as_str(),
                    name: name.name,
                    new_name: new_name.as_str(),
                },
            ) {
                storage.rollback_index_rename(slot, prior);
                return sql_fail(error);
            }
            if let Err(error) = txn.record_ddl(super::txn::DdlUndo::IndexRenamed {
                slot: slot as u32,
                prior,
            }) {
                storage.rollback_index_rename(slot, prior);
                return sql_fail(error);
            }
        }
    }
    Ok(Ok(responder.command_complete("ALTER INDEX")?))
}

/// DROP INDEX [IF EXISTS].
pub fn drop_index(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut super::txn::TxnState,
    names: &[QualName],
    if_exists: bool,
    responder: &mut Responder,
) -> Outcome {
    for name in names {
        if let Some(schema) = name.schema
            && storage.find_schema_visible(schema, txn.txid).is_none()
            && !if_exists
        {
            return sql_fail(sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "schema \"{}\" does not exist",
                schema
            ));
        }
        // A bare index name resolves through the search path; a qualified one
        // looks only in its schema.
        let found: Option<SqlName> = match name.schema {
            Some(schema) => storage
                .index_exists(schema, name.name, txn.txid)
                .then(|| SqlName::parse(schema).ok())
                .flatten(),
            None => storage.path().entries().iter().find_map(|e| match e {
                crate::storage::PathEntry::Schema(slot) => {
                    let schema = storage.schema_def(*slot as usize).name;
                    storage
                        .index_exists(schema.as_str(), name.name, txn.txid)
                        .then_some(schema)
                }
                crate::storage::PathEntry::Catalog => None,
            }),
        };
        if let Some(schema) = found {
            let object = storage
                .resolve_access_object(
                    crate::storage::AccessClass::Index,
                    schema.as_str(),
                    name.name,
                    txn.txid,
                )
                .expect("resolved index exists");
            if let Err(error) = storage.require_owner(object, txn.txid, "index") {
                return sql_fail(error);
            }
            let lsn = storage.bump_lsn();
            if let Err(e) = wal.stage(
                txn.txid,
                lsn,
                &WalOp::DropIndex {
                    schema: schema.as_str(),
                    name: name.name,
                },
            ) {
                return sql_fail(e);
            }
            let dropped = match storage.drop_index(schema.as_str(), name.name, txn.txid) {
                Ok(d) => d,
                Err(e) => return sql_fail(e),
            };
            if let Some(slot) = dropped
                && let Err(e) = txn.record_ddl(super::txn::DdlUndo::IndexDropped(slot as u32))
            {
                return sql_fail(e);
            }
        } else if if_exists {
            responder.notice(
                crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                stack_format!(128, "index \"{}\" does not exist, skipping", name.name).as_str(),
            )?;
        } else {
            // An index is an object, not a relation, to PostgreSQL's error
            // codes: a missing one is 42704, where a missing table is 42P01.
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "index \"{}\" does not exist",
                name.name
            ));
        }
    }
    responder.command_complete("DROP INDEX")?;
    sql_ok()
}

/// REINDEX rebuilds the selected table's bounded value-index cache from the
/// authoritative committed rows. The cache is disposable by design, so its
/// reconstruction is not journaled; restart uses the same reconstruction path.
pub fn reindex(
    storage: &mut Storage,
    txn: &mut super::txn::TxnState,
    target: crate::sql::ast::ReindexTarget,
    name: &QualName,
    concurrently: bool,
    responder: &mut Responder,
) -> Outcome {
    if concurrently {
        return sql_fail(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "REINDEX CONCURRENTLY is not implemented"
        ));
    }
    let mut tables = [usize::MAX; crate::storage::MAX_SCHEMAS * crate::storage::MAX_COLUMNS];
    let mut table_count = 0usize;
    match target {
        crate::sql::ast::ReindexTarget::Table => match resolve_dml_table(storage, name, txn.txid) {
            Ok(table) => tables[0] = table,
            Err(error) => return sql_fail(error),
        },
        crate::sql::ast::ReindexTarget::Index => {
            let index = match name.schema {
                Some(schema) => storage.index_definition(schema, name.name, txn.txid),
                None => storage
                    .path()
                    .entries()
                    .iter()
                    .find_map(|entry| match entry {
                        crate::storage::PathEntry::Schema(slot) => storage.index_definition(
                            storage.schema_def(*slot as usize).name.as_str(),
                            name.name,
                            txn.txid,
                        ),
                        crate::storage::PathEntry::Catalog => None,
                    }),
            };
            let Some(index) = index else {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "index \"{}\" does not exist",
                    name.name
                ));
            };
            let Some(table) =
                storage.find_visible(index.schema.as_str(), index.table.as_str(), txn.txid)
            else {
                return sql_fail(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "index \"{}\" has no table",
                    index.name.as_str()
                ));
            };
            tables[0] = table;
        }
        crate::sql::ast::ReindexTarget::Schema => {
            let Some(schema) = storage.find_schema_visible(name.name, txn.txid) else {
                return sql_fail(sql_err!(
                    sqlstate::INVALID_SCHEMA_NAME,
                    "schema \"{}\" does not exist",
                    name.name
                ));
            };
            let object = crate::storage::AccessObject {
                class: crate::storage::AccessClass::Schema,
                slot: schema as u16,
            };
            if let Err(error) = storage.require_owner(object, txn.txid, "schema") {
                return sql_fail(error);
            }
            for table in 0..storage.table_count() {
                let definition = storage.table_def(table, txn.txid);
                if storage.table(table).visible_to(txn.txid)
                    && definition.schema.as_str() == name.name
                {
                    if table_count == tables.len() {
                        return sql_fail(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "REINDEX schema has too many tables"
                        ));
                    }
                    tables[table_count] = table;
                    table_count += 1;
                }
            }
        }
    }
    if target != crate::sql::ast::ReindexTarget::Schema {
        table_count = 1;
    }
    for &table in &tables[..table_count] {
        if let Err(error) = require_table_privilege(
            storage,
            table,
            crate::storage::PrivilegeSet::MAINTAIN,
            txn.txid,
        ) {
            return sql_fail(error);
        }
        if let Err(error) = storage.lock_table(
            txn.txid,
            table,
            crate::sql::ast::TableLockMode::Share,
            false,
        ) {
            return sql_fail(error);
        }
    }
    for &table in &tables[..table_count] {
        if let Err(error) = storage.refresh_enforcers(table) {
            return sql_fail(error);
        }
    }
    responder.command_complete("REINDEX")?;
    sql_ok()
}

/// COPY's per-statement setup: the resolved table and target columns, in
/// COPY's column order. Held by the connection across CopyData messages.
#[derive(Clone, Copy)]
pub struct CopySetup {
    pub table_index: usize,
    pub targets: [usize; MAX_COLUMNS],
    pub n_targets: usize,
    pub fmt: CopyFmt,
}

/// The resolved, owned form of a COPY's format options — owned because a COPY
/// FROM STDIN's [`CopySetup`] outlives the statement arena. The `force_*` fields
/// are bitmasks over table column indices.
#[derive(Clone, Copy)]
pub struct CopyFmt {
    pub csv: bool,
    pub binary: bool,
    pub delimiter: u8,
    pub quote: u8,
    pub escape: u8,
    pub header: bool,
    pub null: StackStr<64>,
    pub force_quote_all: bool,
    pub force_quote: u64,
    pub force_not_null: u64,
    pub force_null: u64,
}

impl CopyFmt {
    fn resolve(
        def: &TableDef,
        table_name: &str,
        options: &crate::sql::ast::CopyOptions,
    ) -> Result<CopyFmt, SqlError> {
        use core::fmt::Write as _;
        let mut null = StackStr::<64>::new();
        let _ = null.write_str(options.null_str());
        if null.is_truncated() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "COPY NULL string is too long"
            ));
        }
        // Resolve a FORCE column list into a bitmask over table columns.
        let mask = |names: &[&str]| -> Result<u64, SqlError> {
            let mut bits = 0u64;
            for name in names {
                let Some(index) = def.column_index(name) else {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column \"{}\" of relation \"{}\" does not exist",
                        name,
                        table_name
                    ));
                };
                bits |= 1u64 << index;
            }
            Ok(bits)
        };
        let delimiter = options.delimiter_byte();
        let quote = options.quote_byte();
        if options.is_csv() && delimiter == quote {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "COPY delimiter and quote must be different"
            ));
        }
        Ok(CopyFmt {
            csv: options.is_csv(),
            binary: options.is_binary(),
            delimiter,
            quote,
            escape: options.escape_byte(),
            header: options.header,
            null,
            force_quote_all: options.force_quote_all,
            force_quote: mask(options.force_quote)?,
            force_not_null: mask(options.force_not_null)?,
            force_null: mask(options.force_null)?,
        })
    }

    fn forced(mask: u64, column: usize) -> bool {
        column < 64 && mask & (1u64 << column) != 0
    }
}

/// Resolves a COPY statement's table, column list, and format options.
pub fn copy_begin(
    storage: &Storage,
    statement: &crate::sql::ast::CopyStmt,
    txid: u32,
) -> Result<CopySetup, SqlError> {
    let table_index = resolve_dml_table(storage, &statement.table, txid)?;
    require_table_privilege(
        storage,
        table_index,
        if statement.to {
            crate::storage::PrivilegeSet::SELECT
        } else {
            crate::storage::PrivilegeSet::INSERT
        },
        txid,
    )?;
    let def = storage.table_def(table_index, txid);
    let mut targets = [0usize; MAX_COLUMNS];
    let n_targets = if statement.columns.is_empty() {
        for (i, t) in targets.iter_mut().enumerate().take(def.n_columns) {
            *t = i;
        }
        def.n_columns
    } else {
        for (i, name) in statement.columns.iter().enumerate() {
            let Some(col) = def.column_index(name) else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" of relation \"{}\" does not exist",
                    name,
                    statement.table.name
                ));
            };
            targets[i] = col;
        }
        statement.columns.len()
    };
    // Direction-only options are refused as PostgreSQL does.
    let opts = &statement.options;
    if !statement.to && (opts.force_quote_all || !opts.force_quote.is_empty()) {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "COPY FORCE_QUOTE cannot be used with COPY FROM"
        ));
    }
    if statement.to && !opts.force_not_null.is_empty() {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "COPY FORCE_NOT_NULL cannot be used with COPY TO"
        ));
    }
    if statement.to && !opts.force_null.is_empty() {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "COPY FORCE_NULL cannot be used with COPY TO"
        ));
    }
    let fmt = CopyFmt::resolve(def, statement.table.name, &statement.options)?;
    if !statement.to {
        for &target in &targets[..n_targets] {
            if def.columns()[target].default.is_generated() {
                return Err(sql_err!(
                    sqlstate::GENERATED_ALWAYS,
                    "cannot insert a non-DEFAULT value into column \"{}\"",
                    def.columns()[target].name.as_str()
                ));
            }
        }
    }
    Ok(CopySetup {
        table_index,
        targets,
        n_targets,
        fmt,
    })
}

/// One COPY FROM data line: text fields decode, coerce through each column's
/// input semantics, and store through the same row core INSERT uses —
/// defaults, sequences, NOT NULL, uniqueness, CHECK and foreign keys all
/// enforced identically.
pub fn copy_row(
    storage: &mut Storage,
    txn: &mut TxnState,
    seq_session: &crate::sql::guc::SeqSession,
    setup: &CopySetup,
    line: &[u8],
    arena: &Arena,
) -> Result<(), SqlError> {
    let def = *storage.table_def(setup.table_index, txn.txid);
    let mut fields: [Option<&str>; MAX_COLUMNS] = [None; MAX_COLUMNS];
    let fmt = &setup.fmt;
    let n_fields = if fmt.csv {
        crate::sql::copy::decode_row_csv(
            line,
            arena,
            &mut fields[..setup.n_targets],
            fmt.delimiter,
            fmt.quote,
            fmt.escape,
            fmt.null.as_str(),
            &|i| CopyFmt::forced(fmt.force_not_null, setup.targets[i]),
            &|i| CopyFmt::forced(fmt.force_null, setup.targets[i]),
        )?
    } else {
        crate::sql::copy::decode_row(line, arena, &mut fields[..setup.n_targets])?
    };
    if n_fields < setup.n_targets {
        return Err(sql_err!(
            sqlstate::BAD_COPY_FILE_FORMAT,
            "missing data for column \"{}\"",
            def.columns()[setup.targets[n_fields]].name.as_str()
        ));
    }
    let checks = parse_checks(&def, arena)?;
    let default_exprs = parse_defaults(&def, arena)?;
    let generated_exprs = parse_generated(&def, arena)?;
    let mut values = [Datum::Null; MAX_COLUMNS];
    let mut explicit = [false; MAX_COLUMNS];
    for (i, field) in fields.iter().enumerate().take(setup.n_targets) {
        let col_index = setup.targets[i];
        let col = &def.columns()[col_index];
        values[col_index] = match field {
            None => Datum::Null,
            Some(text) => coerce(Datum::Text(text), col, storage, txn.txid, arena)?,
        };
        explicit[col_index] = true;
    }
    fill_omitted_defaults(
        storage,
        txn.txid,
        seq_session,
        &def,
        &default_exprs,
        &mut values,
        &explicit,
        arena,
    )?;
    fill_auto_increment(
        storage,
        setup.table_index,
        &def,
        &mut values,
        &explicit,
        seq_session,
        txn.txid,
    )?;
    compute_generated(
        &def,
        &generated_exprs,
        &mut values,
        storage,
        txn.txid,
        arena,
    )?;
    check_not_null(&def, &values)?;
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut schema);
    enforce_row_constraints(
        storage,
        setup.table_index,
        &def,
        &schema[..def.n_columns],
        &values[..def.n_columns],
        None,
        txn.txid,
        &checks,
        arena,
        &[],
    )?;
    store_row(
        storage,
        txn,
        setup.table_index,
        None,
        &values[..def.n_columns],
    )
}

/// One COPY FROM binary row: `row` is the int16 field count followed by each
/// field's int32 length (or -1 for NULL) and its binary bytes. Fields decode
/// through each column's binary input, then store through the same row core as
/// INSERT — defaults, sequences, NOT NULL, uniqueness, CHECK and foreign keys.
pub fn copy_row_binary(
    storage: &mut Storage,
    txn: &mut TxnState,
    seq_session: &crate::sql::guc::SeqSession,
    setup: &CopySetup,
    row: &[u8],
    arena: &Arena,
) -> Result<(), SqlError> {
    let def = *storage.table_def(setup.table_index, txn.txid);
    let malformed = || sql_err!(sqlstate::BAD_COPY_FILE_FORMAT, "invalid COPY binary row");
    let mut reader = crate::pg::wire::MsgIn::new(row);
    let count = reader.i16().map_err(|_| malformed())?;
    if count < 0 || count as usize != setup.n_targets {
        return Err(sql_err!(
            sqlstate::BAD_COPY_FILE_FORMAT,
            "COPY binary row has {} fields, expected {}",
            count,
            setup.n_targets
        ));
    }
    let checks = parse_checks(&def, arena)?;
    let default_exprs = parse_defaults(&def, arena)?;
    let generated_exprs = parse_generated(&def, arena)?;
    let mut values = [Datum::Null; MAX_COLUMNS];
    let mut explicit = [false; MAX_COLUMNS];
    for i in 0..setup.n_targets {
        let field_len = reader.i32().map_err(|_| malformed())?;
        if field_len < -1 {
            return Err(malformed());
        }
        let col_index = setup.targets[i];
        let col = def.columns()[col_index];
        values[col_index] = if field_len == -1 {
            Datum::Null
        } else {
            let field = reader.take(field_len as usize).map_err(|_| malformed())?;
            let decoded =
                decode_binary_field_with_catalog(col.ctype, field, arena, storage, txn.txid)?;
            coerce(decoded, &col, storage, txn.txid, arena)?
        };
        explicit[col_index] = true;
    }
    if !reader.done() {
        return Err(malformed());
    }
    fill_omitted_defaults(
        storage,
        txn.txid,
        seq_session,
        &def,
        &default_exprs,
        &mut values,
        &explicit,
        arena,
    )?;
    fill_auto_increment(
        storage,
        setup.table_index,
        &def,
        &mut values,
        &explicit,
        seq_session,
        txn.txid,
    )?;
    compute_generated(
        &def,
        &generated_exprs,
        &mut values,
        storage,
        txn.txid,
        arena,
    )?;
    check_not_null(&def, &values)?;
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut schema);
    enforce_row_constraints(
        storage,
        setup.table_index,
        &def,
        &schema[..def.n_columns],
        &values[..def.n_columns],
        None,
        txn.txid,
        &checks,
        arena,
        &[],
    )?;
    store_row(
        storage,
        txn,
        setup.table_index,
        None,
        &values[..def.n_columns],
    )
}

/// Decodes one COPY-binary field into a datum of `ctype`, per PostgreSQL's
/// per-type binary receive format.
pub(crate) fn decode_binary_field<'a>(
    ctype: ColType,
    bytes: &'a [u8],
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    decode_binary_field_with_context(ctype, bytes, arena, BinaryDecodeContext::Plain)
}

/// Decodes binary input whose declared type was resolved through the active
/// catalog. `txid` makes domain constraints observe the same catalog view as
/// the statement that supplied the parameter.
pub(crate) fn decode_binary_field_with_catalog<'a>(
    ctype: ColType,
    bytes: &'a [u8],
    arena: &'a Arena,
    storage: &Storage,
    txid: u32,
) -> Result<Datum<'a>, SqlError> {
    decode_binary_field_with_context(
        ctype,
        bytes,
        arena,
        BinaryDecodeContext::Catalog { storage, txid },
    )
}

/// A catalog-resolved parameter type. Both text and binary Bind values pass
/// through this boundary, so catalog types cannot be accepted by one format
/// while silently bypassing their input rules in the other.
#[derive(Clone, Copy)]
pub(crate) enum ParameterInputType {
    Builtin(ColType),
    Domain { slot: usize, base: ColType },
    DomainArray(crate::sql::types::ArrElem),
}

pub(crate) fn resolve_parameter_input_type(
    storage: &Storage,
    oid: i32,
    txid: u32,
) -> Result<ParameterInputType, SqlError> {
    use crate::sql::types::oid as oids;

    let unknown_type = || {
        sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "parameter type OID {} does not exist",
            oid
        )
    };
    if oid == 0 || oid == oids::UNKNOWN {
        return Ok(ParameterInputType::Builtin(ColType::Text));
    }
    let domain_slot = (oid - oids::FIRST_DOMAIN) as usize;
    if (oids::FIRST_DOMAIN..oids::FIRST_DOMAIN + crate::storage::MAX_DOMAINS as i32).contains(&oid)
    {
        let domain = storage.domain(domain_slot);
        if !domain.visible_to(txid) {
            return Err(unknown_type());
        }
        return Ok(ParameterInputType::Domain {
            slot: domain_slot,
            base: domain.base,
        });
    }
    let domain_array_slot = (oid - oids::FIRST_DOMAIN_ARRAY) as usize;
    if (oids::FIRST_DOMAIN_ARRAY..oids::FIRST_DOMAIN_ARRAY + crate::storage::MAX_DOMAINS as i32)
        .contains(&oid)
    {
        let domain = storage.domain(domain_array_slot);
        if !domain.visible_to(txid) {
            return Err(unknown_type());
        }
        let element = crate::sql::types::ArrElem::domain(domain_array_slot as u16, domain.base)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "binary input of a record-valued domain is not supported"
                )
            })?;
        return Ok(ParameterInputType::DomainArray(element));
    }
    let ctype = ColType::from_oid(oid).ok_or_else(|| {
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "binary format for parameter type OID {} is not implemented",
            oid
        )
    })?;
    match ctype {
        ColType::Enum(slot) | ColType::Array(crate::sql::types::ArrElem::Enum(slot))
            if slot as usize >= storage.enum_count()
                || !storage.enum_for(slot as usize, txid).visible_to(txid) =>
        {
            Err(unknown_type())
        }
        _ => Ok(ParameterInputType::Builtin(ctype)),
    }
}

pub(crate) fn decode_binary_input<'a>(
    storage: &Storage,
    oid: i32,
    bytes: &'a [u8],
    arena: &'a Arena,
    txid: u32,
) -> Result<Datum<'a>, SqlError> {
    if oid == crate::sql::types::oid::REGTYPE {
        let bytes: [u8; 4] = bytes.try_into().map_err(|_| {
            sql_err!(
                sqlstate::INVALID_BINARY_REPRESENTATION,
                "invalid binary representation for parameter type OID {}",
                oid
            )
        })?;
        return crate::sql::eval::regtype_of_oid(i64::from(i32::from_be_bytes(bytes)), arena);
    }
    let decoded = match resolve_parameter_input_type(storage, oid, txid)? {
        ParameterInputType::Builtin(ctype) => {
            decode_binary_field_with_catalog(ctype, bytes, arena, storage, txid)
        }
        ParameterInputType::Domain { slot, base } => {
            let value = decode_binary_field_with_catalog(base, bytes, arena, storage, txid)?;
            coerce_domain_value(
                storage,
                slot,
                value,
                txid,
                arena,
                crate::sql::eval::NO_PARAMS,
            )
        }
        ParameterInputType::DomainArray(element) => {
            decode_binary_field_with_catalog(ColType::Array(element), bytes, arena, storage, txid)
        }
    };
    // COPY framing errors are 22P04, but a Bind value is an individual type's
    // binary input and PostgreSQL reports malformed bytes as 22P03. Keep the
    // codec shared while preserving the protocol boundary in its error type.
    decoded.map_err(|error| {
        if error.sqlstate == sqlstate::BAD_COPY_FILE_FORMAT {
            sql_err!(
                sqlstate::INVALID_BINARY_REPRESENTATION,
                "invalid binary representation for parameter type OID {}",
                oid
            )
        } else {
            error
        }
    })
}

/// Whether a catalog-resolved result type has a PostgreSQL binary send form.
/// This uses the same typed OID boundary as Bind, so query COPY cannot reject
/// domains and user arrays merely because their identity is not a built-in
/// [`ColType`].
pub(crate) fn binary_output_type_supported(storage: &Storage, oid: i32, txid: u32) -> bool {
    resolve_parameter_input_type(storage, oid, txid).is_ok()
}

/// Decodes a UTF-8 text Bind value according to its declared PostgreSQL type.
///
/// The wire format is text, not an untyped SQL literal: resolving it here
/// gives text and binary Bind identical domain, enum, array, and typmod
/// boundaries before the executor observes the parameter.
pub(crate) fn decode_text_input<'a>(
    storage: &Storage,
    oid: i32,
    bytes: &'a [u8],
    arena: &'a Arena,
    txid: u32,
) -> Result<Datum<'a>, SqlError> {
    let text = core::str::from_utf8(bytes).map_err(|_| {
        sql_err!(
            sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
            "invalid byte sequence for encoding \"UTF8\""
        )
    })?;
    if oid == crate::sql::types::oid::REGTYPE {
        return crate::sql::eval::regtype_of_name(text);
    }
    let decode_builtin = |ctype| match ctype {
        target if target.is_reg_object() => {
            let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
            crate::sql::eval::regobject_cast(Datum::Text(text), target, Some(&catalog), arena)
        }
        ColType::Enum(slot) => coerce_enum_value(Datum::Text(text), slot, storage, txid, arena),
        ColType::Array(
            element @ (crate::sql::types::ArrElem::Enum(_)
            | crate::sql::types::ArrElem::Domain { .. }),
        ) => coerce_user_type_array(Datum::Text(text), element, storage, txid, arena),
        _ => crate::sql::eval::cast_to(Datum::Text(text), ctype, arena),
    };
    match resolve_parameter_input_type(storage, oid, txid)? {
        ParameterInputType::Builtin(ctype) => decode_builtin(ctype),
        ParameterInputType::Domain { slot, base } => coerce_domain_value(
            storage,
            slot,
            decode_builtin(base)?,
            txid,
            arena,
            crate::sql::eval::NO_PARAMS,
        ),
        ParameterInputType::DomainArray(element) => {
            coerce_user_type_array(Datum::Text(text), element, storage, txid, arena)
        }
    }
}

pub(crate) fn coerce_binary_input_null<'a>(
    storage: &Storage,
    oid: i32,
    arena: &'a Arena,
    txid: u32,
) -> Result<Datum<'a>, SqlError> {
    if oid == 0 {
        return Ok(Datum::Null);
    }
    match resolve_parameter_input_type(storage, oid, txid)? {
        ParameterInputType::Domain { slot, .. } => coerce_domain_value(
            storage,
            slot,
            Datum::Null,
            txid,
            arena,
            crate::sql::eval::NO_PARAMS,
        ),
        ParameterInputType::Builtin(_) | ParameterInputType::DomainArray(_) => Ok(Datum::Null),
    }
}

#[derive(Clone, Copy)]
enum BinaryDecodeContext<'a> {
    Plain,
    Catalog { storage: &'a Storage, txid: u32 },
}

fn decode_binary_field_with_context<'a>(
    ctype: ColType,
    bytes: &'a [u8],
    arena: &'a Arena,
    context: BinaryDecodeContext<'_>,
) -> Result<Datum<'a>, SqlError> {
    use crate::sql::types::oid as oids;
    let bad = || {
        sql_err!(
            sqlstate::BAD_COPY_FILE_FORMAT,
            "invalid binary field for type {}",
            ctype.name()
        )
    };
    let via = |oid| crate::pg::conn::decode_binary_param(oid, bytes, arena).map_err(|_| bad());
    match ctype {
        ColType::Void => Err(bad()),
        ColType::Bool => via(oids::BOOL),
        ColType::Int2 => {
            let b: [u8; 2] = bytes.try_into().map_err(|_| bad())?;
            Ok(Datum::Int2(i16::from_be_bytes(b)))
        }
        ColType::Int2Vector => decode_binary_int2vector(bytes, arena),
        ColType::Int4 | ColType::Oid => via(oids::INT4),
        ColType::Regtype => {
            let bytes: [u8; 4] = bytes.try_into().map_err(|_| bad())?;
            crate::sql::eval::regtype_of_oid(i64::from(i32::from_be_bytes(bytes)), arena)
                .map_err(|_| bad())
        }
        target @ (ColType::Regproc
        | ColType::Regprocedure
        | ColType::Regoper
        | ColType::Regoperator
        | ColType::Regclass
        | ColType::Regnamespace
        | ColType::Regrole) => {
            let bytes: [u8; 4] = bytes.try_into().map_err(|_| bad())?;
            let referenced_oid = i32::from_be_bytes(bytes);
            match context {
                BinaryDecodeContext::Plain => {
                    let name = arena.alloc_str_display(referenced_oid).map_err(|_| bad())?;
                    Ok(Datum::RegObject {
                        type_oid: target.oid(),
                        referenced_oid,
                        name,
                    })
                }
                BinaryDecodeContext::Catalog { storage, txid } => {
                    let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
                    crate::sql::eval::regobject_cast(
                        Datum::Int4(referenced_oid),
                        target,
                        Some(&catalog),
                        arena,
                    )
                    .map_err(|_| bad())
                }
            }
        }
        ColType::Int8 => via(oids::INT8),
        ColType::Float4 => via(oids::FLOAT4),
        ColType::Float8 => via(oids::FLOAT8),
        ColType::Text | ColType::Varchar | ColType::Bpchar | ColType::Name => {
            core::str::from_utf8(bytes)
                .map(Datum::Text)
                .map_err(|_| bad())
        }
        ColType::Date => via(oids::DATE),
        ColType::Timestamp => via(oids::TIMESTAMP),
        ColType::Timestamptz => via(oids::TIMESTAMPTZ),
        ColType::Time => via(oids::TIME),
        ColType::Timetz => {
            // 8-byte time then a 4-byte zone counted west of UTC; the stored
            // offset is the eastward negation, as the send side flips it.
            let b: [u8; 12] = bytes.try_into().map_err(|_| bad())?;
            let time = i64::from_be_bytes(b[0..8].try_into().unwrap());
            let zone = i32::from_be_bytes(b[8..12].try_into().unwrap());
            Ok(Datum::Timetz(time, -zone))
        }
        ColType::Interval => via(oids::INTERVAL),
        // The wire representation contains UTF-8 JSON text, but valid UTF-8
        // is not necessarily valid JSON. Reuse the type input functions here
        // so binary Bind, nested fields, and binary COPY cannot create an
        // invalid JSON datum.
        ColType::Json => crate::sql::eval::cast_to(via(oids::JSON)?, ColType::Json, arena),
        ColType::Jsonb => crate::sql::eval::cast_to(via(oids::JSONB)?, ColType::Jsonb, arena),
        ColType::Uuid => via(oids::UUID),
        ColType::Bytea => via(oids::BYTEA),
        ColType::Numeric => via(oids::NUMERIC),
        ColType::Inet => via(oids::INET),
        ColType::Cidr => via(oids::CIDR),
        ColType::Macaddr => via(oids::MACADDR),
        ColType::Macaddr8 => via(oids::MACADDR8),
        ColType::Array(element) => decode_binary_array(element, bytes, arena, context),
        ColType::Range(kind) => decode_binary_range(kind, bytes, arena),
        ColType::Multirange(kind) => decode_binary_multirange(kind, bytes, arena),
        ColType::Bit { varying } => decode_binary_bit(varying, bytes, arena),
        ColType::Record => decode_binary_record(bytes, arena, context),
        ColType::Enum(slot) => {
            let label = core::str::from_utf8(bytes).map_err(|_| bad())?;
            let BinaryDecodeContext::Catalog { storage, txid } = context else {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "binary input of a user-defined enum requires catalog context"
                ));
            };
            coerce_enum_value(Datum::Text(label), slot, storage, txid, arena)
        }
    }
}

fn decode_binary_record<'a>(
    bytes: &'a [u8],
    arena: &'a Arena,
    context: BinaryDecodeContext<'_>,
) -> Result<Datum<'a>, SqlError> {
    let bad = || sql_err!(sqlstate::BAD_COPY_FILE_FORMAT, "invalid binary record");
    let mut reader = crate::pg::wire::MsgIn::new(bytes);
    let field_count = reader.i32().map_err(|_| bad())?;
    if field_count < 0 || field_count as usize > RECORD_FIELD_NAMES.len() {
        return Err(bad());
    }
    let fields = arena
        .alloc_slice_with(field_count as usize, |_| super::types::RecordField {
            name: "",
            type_oid: 0,
            value: Datum::Null,
        })
        .map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "record exceeds the statement arena"
            )
        })?;
    for (index, field) in fields.iter_mut().enumerate() {
        let type_oid = reader.i32().map_err(|_| bad())?;
        let length = reader.i32().map_err(|_| bad())?;
        if length < -1 {
            return Err(bad());
        }
        field.name = RECORD_FIELD_NAMES[index];
        field.type_oid = type_oid;
        field.value = if length == -1 {
            Datum::Null
        } else {
            let bytes = reader.take(length as usize).map_err(|_| bad())?;
            decode_binary_field_by_oid(type_oid, bytes, arena, context)?
        };
    }
    if !reader.done() {
        return Err(bad());
    }
    Ok(Datum::Record(fields))
}

/// Decodes a record field whose PostgreSQL OID is carried in the record's
/// binary body. Catalog-defined domain identities need their constraints
/// applied here; treating their underlying type as the field type would make
/// a malformed binary Bind bypass the domain boundary.
fn decode_binary_field_by_oid<'a>(
    oid: i32,
    bytes: &'a [u8],
    arena: &'a Arena,
    context: BinaryDecodeContext<'_>,
) -> Result<Datum<'a>, SqlError> {
    let bad = || sql_err!(sqlstate::BAD_COPY_FILE_FORMAT, "invalid binary record");
    if let BinaryDecodeContext::Catalog { storage, txid } = context {
        return decode_binary_input(storage, oid, bytes, arena, txid);
    }
    let ctype = ColType::from_oid(oid).ok_or_else(bad)?;
    decode_binary_field_with_context(ctype, bytes, arena, context)
}

/// Decodes PostgreSQL's `int2vector` send format. It is an `int2` array on
/// the wire, while the catalog representation stores its values packed in
/// native little-endian order.
fn decode_binary_int2vector<'a>(bytes: &'a [u8], arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    use crate::sql::types::oid;

    let bad = || sql_err!(sqlstate::BAD_COPY_FILE_FORMAT, "invalid binary int2vector");
    let mut reader = crate::pg::wire::MsgIn::new(bytes);
    let ndim = reader.i32().map_err(|_| bad())?;
    let has_null = reader.i32().map_err(|_| bad())?;
    let element_oid = reader.i32().map_err(|_| bad())?;
    if has_null != 0 || element_oid != oid::INT2 {
        return Err(bad());
    }
    if ndim == 0 {
        if !reader.done() {
            return Err(bad());
        }
        return Ok(Datum::Int2Vector(&[]));
    }
    if ndim != 1 {
        return Err(bad());
    }
    let count = reader.i32().map_err(|_| bad())?;
    let _lower_bound = reader.i32().map_err(|_| bad())?;
    if !(0..=crate::sql::array::MAX_ELEMENTS as i32).contains(&count) {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "int2vector value too large"
        ));
    }
    let raw = arena
        .alloc_slice_with(count as usize * 2, |_| 0u8)
        .map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "int2vector exceeds the statement arena"
            )
        })?;
    for slot in raw.chunks_exact_mut(2) {
        let len = reader.i32().map_err(|_| bad())?;
        if len != 2 {
            return Err(bad());
        }
        let value = reader.take(2).map_err(|_| bad())?;
        slot.copy_from_slice(&i16::from_be_bytes([value[0], value[1]]).to_le_bytes());
    }
    if !reader.done() {
        return Err(bad());
    }
    Ok(Datum::Int2Vector(raw))
}

/// Decodes PostgreSQL's binary array receive format.
fn decode_binary_array<'a>(
    element: crate::sql::types::ArrElem,
    bytes: &'a [u8],
    arena: &'a Arena,
    context: BinaryDecodeContext<'_>,
) -> Result<Datum<'a>, SqlError> {
    let bad = || sql_err!(sqlstate::BAD_COPY_FILE_FORMAT, "invalid binary array");
    let mut reader = crate::pg::wire::MsgIn::new(bytes);
    let ndim = reader.i32().map_err(|_| bad())?;
    let has_null = reader.i32().map_err(|_| bad())?;
    let element_oid = reader.i32().map_err(|_| bad())?;
    if !(0..=1).contains(&has_null) || element_oid != element.element_oid() {
        return Err(bad());
    }
    if ndim == 0 {
        if !reader.done() {
            return Err(bad());
        }
        return Ok(Datum::Array {
            element,
            raw: crate::sql::array::build(&[], arena)?,
        });
    }
    if ndim < 0 || ndim as usize > crate::sql::array::MAX_DIMENSIONS {
        return Err(bad());
    }
    let mut dimensions = [0usize; crate::sql::array::MAX_DIMENSIONS];
    let mut lower_bounds = [0i32; crate::sql::array::MAX_DIMENSIONS];
    for index in 0..ndim as usize {
        let dimension = reader.i32().map_err(|_| bad())?;
        let lower_bound = reader.i32().map_err(|_| bad())?;
        if dimension <= 0 {
            return Err(bad());
        }
        dimensions[index] = dimension as usize;
        lower_bounds[index] = lower_bound;
    }
    let shape = crate::sql::array::Shape::new(
        &dimensions[..ndim as usize],
        &lower_bounds[..ndim as usize],
    )?;
    let count = shape.element_count();
    let element_type = element.to_coltype();
    let mut items = [Datum::Null; crate::sql::array::MAX_ELEMENTS];
    let mut saw_null = false;
    for slot in items.iter_mut().take(count) {
        let len = reader.i32().map_err(|_| bad())?;
        if len == -1 {
            *slot = Datum::Null;
            saw_null = true;
            continue;
        }
        if len < -1 {
            return Err(bad());
        }
        let field = reader.take(len as usize).map_err(|_| bad())?;
        let value = decode_binary_field_with_context(element_type, field, arena, context)?;
        *slot = match element {
            crate::sql::types::ArrElem::Domain { slot, .. } => {
                let BinaryDecodeContext::Catalog { storage, txid } = context else {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "binary input of a domain array requires catalog context"
                    ));
                };
                coerce_domain_value(
                    storage,
                    slot as usize,
                    value,
                    txid,
                    arena,
                    crate::sql::eval::NO_PARAMS,
                )?
            }
            _ => value,
        };
    }
    if !reader.done() {
        return Err(bad());
    }
    if (has_null != 0) != saw_null {
        return Err(bad());
    }
    Ok(Datum::Array {
        element,
        raw: crate::sql::array::build_shaped(&items[..count], shape, arena)?,
    })
}

/// Decodes one PostgreSQL binary range body (flags byte + finite bounds) into a
/// canonical `Datum::Range`. Shared by range and multirange decoding.
fn decode_binary_range<'a>(
    kind: crate::sql::types::RangeKind,
    bytes: &'a [u8],
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let mut reader = crate::pg::wire::MsgIn::new(bytes);
    let text = decode_range_body(kind, &mut reader, arena)?;
    if !reader.done() {
        return Err(sql_err!(
            sqlstate::BAD_COPY_FILE_FORMAT,
            "invalid binary range"
        ));
    }
    Ok(Datum::Range { text, kind })
}

fn decode_range_body<'a>(
    kind: crate::sql::types::RangeKind,
    reader: &mut crate::pg::wire::MsgIn<'a>,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let bad = || sql_err!(sqlstate::BAD_COPY_FILE_FORMAT, "invalid binary range");
    // PostgreSQL masks reserved range flag bits before interpreting the
    // message. Keep that wire rule here; rejecting them would make a client
    // payload PostgreSQL accepts fail only against pos3ql.
    let flags = reader.u8().map_err(|_| bad())? & 0x1f;
    let mut parsed = crate::sql::range::Parsed {
        empty: flags & 0x01 != 0,
        lower: None,
        upper: None,
        lower_inc: flags & 0x02 != 0,
        upper_inc: flags & 0x04 != 0,
    };
    if parsed.empty {
        return crate::sql::range::canonical(&parsed, kind, arena);
    }
    let element_type = kind.elem_type();
    let read_bound =
        |reader: &mut crate::pg::wire::MsgIn<'a>| -> Result<Option<&'a str>, SqlError> {
            let len = reader.i32().map_err(|_| bad())?;
            if len < 0 {
                return Err(bad());
            }
            let field = reader.take(len as usize).map_err(|_| bad())?;
            let datum = decode_binary_field(element_type, field, arena)?;
            Ok(Some(arena.alloc_str_display(datum).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "range bound exceeds the statement arena"
                )
            })?))
        };
    if flags & 0x08 == 0 {
        parsed.lower = read_bound(reader)?;
    }
    if flags & 0x10 == 0 {
        parsed.upper = read_bound(reader)?;
    }
    crate::sql::range::canonical(&parsed, kind, arena)
}

/// Decodes the PostgreSQL binary multirange format: int32 range count, then each
/// range as int32 length + its range binary.
fn decode_binary_multirange<'a>(
    kind: crate::sql::types::RangeKind,
    bytes: &'a [u8],
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let bad = || sql_err!(sqlstate::BAD_COPY_FILE_FORMAT, "invalid binary multirange");
    let mut reader = crate::pg::wire::MsgIn::new(bytes);
    let count = reader.i32().map_err(|_| bad())?;
    if !(0..=crate::sql::range::MAX_MULTIRANGE as i32).contains(&count) {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "multirange has too many ranges"
        ));
    }
    let mut ranges = [""; crate::sql::range::MAX_MULTIRANGE];
    for slot in ranges.iter_mut().take(count as usize) {
        let len = reader.i32().map_err(|_| bad())?;
        let field = reader.take(len as usize).map_err(|_| bad())?;
        let mut inner = crate::pg::wire::MsgIn::new(field);
        *slot = decode_range_body(kind, &mut inner, arena)?;
        if !inner.done() {
            return Err(bad());
        }
    }
    if !reader.done() {
        return Err(bad());
    }
    let text =
        crate::sql::range::canonicalize_multirange(&mut ranges[..count as usize], kind, arena)?;
    Ok(Datum::Multirange { text, kind })
}

/// Decodes the PostgreSQL binary bit-string format: int32 bit length, then
/// ceil(len/8) bytes packed MSB-first, into a `0`/`1` string.
fn decode_binary_bit<'a>(
    varying: bool,
    bytes: &'a [u8],
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let bad = || sql_err!(sqlstate::BAD_COPY_FILE_FORMAT, "invalid binary bit string");
    let mut reader = crate::pg::wire::MsgIn::new(bytes);
    let bit_len = reader.i32().map_err(|_| bad())?;
    if bit_len < 0 {
        return Err(bad());
    }
    let bit_len = bit_len as usize;
    let packed = reader.take(bit_len.div_ceil(8)).map_err(|_| bad())?;
    if !reader.done() {
        return Err(bad());
    }
    let bits = arena
        .alloc_slice_with(bit_len, |i| {
            if packed[i / 8] & (0x80 >> (i % 8)) != 0 {
                b'1'
            } else {
                b'0'
            }
        })
        .map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "bit string exceeds the statement arena"
            )
        })?;
    let bits = core::str::from_utf8(bits).map_err(|_| bad())?;
    Ok(Datum::Bit { bits, varying })
}

/// COPY TO STDOUT: every visible row, targets in COPY order, each value in
/// its wire text form with COPY's escapes. Returns the row count for the
/// command tag.
pub fn copy_out(
    storage: &Storage,
    txid: u32,
    setup: &CopySetup,
    arena: &Arena,
    responder: &mut Responder,
) -> Result<u64, SqlError> {
    let def = *storage.table_def(setup.table_index, txid);
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut schema);
    let fmt = &setup.fmt;
    responder
        .copy_out_response(setup.n_targets, fmt.binary)
        .map_err(wire_to_sql)?;
    if fmt.binary {
        responder.copy_binary_header().map_err(wire_to_sql)?;
    }
    // A header line of column names, in the same field format as the data.
    if fmt.header {
        responder
            .copy_data_row(&|out| {
                for i in 0..setup.n_targets {
                    if i > 0 {
                        out(&[fmt.delimiter]);
                    }
                    let name = def.columns()[setup.targets[i]].name.as_str();
                    if fmt.csv {
                        crate::sql::copy::encode_field_csv(
                            out,
                            Some(name),
                            fmt.null.as_str(),
                            fmt.delimiter,
                            fmt.quote,
                            fmt.escape,
                            false,
                        );
                    } else {
                        crate::sql::copy::encode_field(out, Some(name));
                    }
                }
            })
            .map_err(wire_to_sql)?;
    }
    // Rowid order is insertion order (rowids are monotonic), which is the
    // order PostgreSQL's COPY TO emits for a freshly-loaded table. Snapshot
    // the visible tokens first, sort, then stream.
    let mut visible = 0usize;
    storage.for_each_row_state(setup.table_index, &mut |rowid, state| {
        if storage
            .visible_row_home(setup.table_index, rowid, state, txid)?
            .is_some()
        {
            visible += 1;
        }
        Ok(core::ops::ControlFlow::Continue(()))
    })?;
    let tokens = arena
        .alloc_slice_with(visible, |_| {
            (
                0u64,
                crate::storage::RowHome::Heap(crate::storage::RowLoc { offset: 0, len: 0 }),
            )
        })
        .map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "COPY TO snapshot exceeds the statement arena"
            )
        })?;
    let mut fill = 0usize;
    storage.for_each_row_state(setup.table_index, &mut |rowid, state| {
        if let Some(home) = storage.visible_row_home(setup.table_index, rowid, state, txid)? {
            tokens[fill] = (rowid, home);
            fill += 1;
        }
        Ok(core::ops::ControlFlow::Continue(()))
    })?;
    tokens.sort_unstable_by_key(|(rowid, _)| *rowid);
    let mut count = 0u64;
    for &(rowid, home) in tokens.iter() {
        storage.with_row_bytes(setup.table_index, rowid, home, |bytes| {
            let mut values = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(bytes, &schema[..def.n_columns], &mut values)?;
            if fmt.binary {
                // Each field is its value's binary form (int32 length + bytes,
                // or -1 for NULL). Ranges and multiranges are pre-parsed into
                // arena datums here (fallible, once); every other type encodes
                // arena-free. The emission closure then runs deterministically
                // (it may run twice on the flush-and-retry path).
                let mut plans = [BinaryFieldPlan::Direct; MAX_COLUMNS];
                for (i, plan) in plans.iter_mut().enumerate().take(setup.n_targets) {
                    *plan = binary_field_plan(&values[setup.targets[i]], arena)?;
                }
                responder
                    .copy_binary_row(setup.n_targets, &|m| {
                        for i in 0..setup.n_targets {
                            match plans[i] {
                                BinaryFieldPlan::Direct => {
                                    Responder::encode_value_binary(m, &values[setup.targets[i]]);
                                }
                                BinaryFieldPlan::Range(f, l, u) => {
                                    m.field(|m| encode_range_binary(m, f, l, u));
                                }
                                BinaryFieldPlan::Multirange(ranges) => {
                                    m.field(|m| {
                                        m.i32(ranges.len() as i32);
                                        for &(f, l, u) in ranges {
                                            m.field(|m| encode_range_binary(m, f, l, u));
                                        }
                                    });
                                }
                            }
                        }
                    })
                    .map_err(wire_to_sql)?;
                return Ok(());
            }
            // Render each target into the arena first (fallible), so the
            // wire write below is a deterministic, retry-safe emission.
            let render = responder.render_context();
            let mut texts: [Option<&str>; MAX_COLUMNS] = [None; MAX_COLUMNS];
            for (i, texts_slot) in texts.iter_mut().enumerate().take(setup.n_targets) {
                // The wire-text output function, exactly as a SELECT would
                // render it — styled timestamps, GUC-honoring bytea, `t`
                // for booleans — then COPY's escapes on top below.
                *texts_slot = Responder::datum_wire_text(&values[setup.targets[i]], render, arena)?;
            }
            responder
                .copy_data_row(&|out| {
                    for (i, text) in texts.iter().enumerate().take(setup.n_targets) {
                        if i > 0 {
                            out(&[fmt.delimiter]);
                        }
                        if fmt.csv {
                            let force = fmt.force_quote_all
                                || CopyFmt::forced(fmt.force_quote, setup.targets[i]);
                            crate::sql::copy::encode_field_csv(
                                out,
                                *text,
                                fmt.null.as_str(),
                                fmt.delimiter,
                                fmt.quote,
                                fmt.escape,
                                force,
                            );
                        } else if let Some(value) = text {
                            crate::sql::copy::encode_field(out, Some(value));
                        } else {
                            out(fmt.null.as_str().as_bytes());
                        }
                    }
                })
                .map_err(wire_to_sql)?;
            Ok(())
        })?;
        count += 1;
    }
    if fmt.binary {
        responder.copy_binary_trailer().map_err(wire_to_sql)?;
    }
    responder.copy_done().map_err(wire_to_sql)?;
    Ok(count)
}

fn wire_to_sql(_: crate::pg::wire::WireFull) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "COPY output exceeds the send buffer"
    )
}

/// Resolves COPY format options for a `COPY (query) TO STDOUT`, whose columns
/// are the query's output columns (named for `FORCE_QUOTE` and the header),
/// with no backing table. Mirrors [`CopyFmt::resolve`] but resolves the
/// `force_*` column lists against the output column names.
fn copy_fmt_for_columns(
    names: &[&str],
    options: &crate::sql::ast::CopyOptions,
) -> Result<CopyFmt, SqlError> {
    use core::fmt::Write as _;
    // FORCE_NOT_NULL / FORCE_NULL are COPY FROM-only, as for a table source.
    if !options.force_not_null.is_empty() || !options.force_null.is_empty() {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "COPY FORCE_NOT_NULL/FORCE_NULL cannot be used with COPY TO"
        ));
    }
    let mut null = StackStr::<64>::new();
    let _ = null.write_str(options.null_str());
    if null.is_truncated() {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "COPY NULL string is too long"
        ));
    }
    let mask = |cols: &[&str]| -> Result<u64, SqlError> {
        let mut bits = 0u64;
        for name in cols {
            let Some(index) = names.iter().position(|n| n.eq_ignore_ascii_case(name)) else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    name
                ));
            };
            bits |= 1u64 << index;
        }
        Ok(bits)
    };
    let delimiter = options.delimiter_byte();
    let quote = options.quote_byte();
    if options.is_csv() && delimiter == quote {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "COPY delimiter and quote must be different"
        ));
    }
    Ok(CopyFmt {
        csv: options.is_csv(),
        binary: options.is_binary(),
        delimiter,
        quote,
        escape: options.escape_byte(),
        header: options.header,
        null,
        force_quote_all: options.force_quote_all,
        force_quote: mask(options.force_quote)?,
        force_not_null: 0,
        force_null: 0,
    })
}

/// Streams the rows of `COPY (query) TO STDOUT`: describe the query's output
/// columns, then run it and format each result row exactly as a table COPY TO
/// would (text / CSV / binary), so the output is byte-identical to PostgreSQL.
#[allow(clippy::too_many_arguments)]
pub fn copy_out_query(
    storage: &Storage,
    txid: u32,
    sql: &str,
    options: &crate::sql::ast::CopyOptions,
    seq: Option<&dyn crate::sql::eval::SequenceAccess>,
    arena: &Arena,
    params: &[Datum],
    responder: &mut Responder,
) -> Result<u64, SqlError> {
    let mut columns = [crate::sql::types::ColDesc::new("", 0, 0); MAX_PROJ];
    let n = super::query::describe_query(sql, storage, txid, arena, &mut columns)?;
    let mut names = [""; MAX_PROJ];
    for (i, c) in columns[..n].iter().enumerate() {
        names[i] = c.name;
    }
    let fmt = copy_fmt_for_columns(&names[..n], options)?;
    if fmt.binary {
        for c in &columns[..n] {
            if !binary_output_type_supported(storage, c.type_oid, txid) {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "COPY BINARY cannot send a column of type oid {}",
                    c.type_oid
                ));
            }
        }
    }
    responder
        .copy_out_response(n, fmt.binary)
        .map_err(wire_to_sql)?;
    if fmt.binary {
        responder.copy_binary_header().map_err(wire_to_sql)?;
    }
    if fmt.header {
        responder
            .copy_data_row(&|out| {
                for (i, c) in columns[..n].iter().enumerate() {
                    if i > 0 {
                        out(&[fmt.delimiter]);
                    }
                    if fmt.csv {
                        crate::sql::copy::encode_field_csv(
                            out,
                            Some(c.name),
                            fmt.null.as_str(),
                            fmt.delimiter,
                            fmt.quote,
                            fmt.escape,
                            false,
                        );
                    } else {
                        crate::sql::copy::encode_field(out, Some(c.name));
                    }
                }
            })
            .map_err(wire_to_sql)?;
    }
    let sel = crate::sql::parser::parse_query(sql, arena)?;
    let render = responder.render_context();
    let fmt = &fmt;
    let mut count = 0u64;
    super::query::select_into_rows(storage, txid, sel, arena, params, None, seq, &mut |vals| {
        if fmt.binary {
            let mut plans = [BinaryFieldPlan::Direct; MAX_COLUMNS];
            for (i, plan) in plans.iter_mut().enumerate().take(n) {
                *plan = binary_field_plan(&vals[i], arena)?;
            }
            responder
                .copy_binary_row(n, &|m| {
                    for (i, plan) in plans.iter().enumerate().take(n) {
                        match *plan {
                            BinaryFieldPlan::Direct => {
                                Responder::encode_value_binary(m, &vals[i]);
                            }
                            BinaryFieldPlan::Range(f, l, u) => {
                                m.field(|m| encode_range_binary(m, f, l, u));
                            }
                            BinaryFieldPlan::Multirange(ranges) => {
                                m.field(|m| {
                                    m.i32(ranges.len() as i32);
                                    for &(f, l, u) in ranges {
                                        m.field(|m| encode_range_binary(m, f, l, u));
                                    }
                                });
                            }
                        }
                    }
                })
                .map_err(wire_to_sql)?;
        } else {
            let mut texts: [Option<&str>; MAX_COLUMNS] = [None; MAX_COLUMNS];
            for (i, slot) in texts.iter_mut().enumerate().take(n) {
                *slot = Responder::datum_wire_text(&vals[i], render, arena)?;
            }
            responder
                .copy_data_row(&|out| {
                    for (i, text) in texts.iter().enumerate().take(n) {
                        if i > 0 {
                            out(&[fmt.delimiter]);
                        }
                        if fmt.csv {
                            let force = fmt.force_quote_all || CopyFmt::forced(fmt.force_quote, i);
                            crate::sql::copy::encode_field_csv(
                                out,
                                *text,
                                fmt.null.as_str(),
                                fmt.delimiter,
                                fmt.quote,
                                fmt.escape,
                                force,
                            );
                        } else if let Some(value) = text {
                            crate::sql::copy::encode_field(out, Some(value));
                        } else {
                            out(fmt.null.as_str().as_bytes());
                        }
                    }
                })
                .map_err(wire_to_sql)?;
        }
        count += 1;
        Ok(())
    })?;
    if fmt.binary {
        responder.copy_binary_trailer().map_err(wire_to_sql)?;
    }
    responder.copy_done().map_err(wire_to_sql)?;
    Ok(count)
}

/// A range's binary parts: the flags byte, and its two bound datums already
/// parsed from the canonical text (`None` = infinite bound, or both `None` for
/// an empty range).
type RangeBinaryParts<'a> = (u8, Option<Datum<'a>>, Option<Datum<'a>>);

/// Per-field plan for binary COPY output. Scalars, arrays and bit strings
/// encode arena-free (`Direct`); ranges and multiranges are pre-parsed into
/// arena datums up front so the retry-safe emission closure never allocates
/// or fails.
#[derive(Clone, Copy)]
enum BinaryFieldPlan<'a> {
    Direct,
    Range(u8, Option<Datum<'a>>, Option<Datum<'a>>),
    Multirange(&'a [RangeBinaryParts<'a>]),
}

fn binary_field_plan<'a>(v: &Datum<'a>, arena: &'a Arena) -> Result<BinaryFieldPlan<'a>, SqlError> {
    match v {
        Datum::Range { text, kind } => {
            let (flags, lower, upper) = parse_range_bounds(text, *kind, arena)?;
            Ok(BinaryFieldPlan::Range(flags, lower, upper))
        }
        Datum::Multirange { text, kind } => {
            let mut components = [""; crate::sql::range::MAX_MULTIRANGE];
            let n = crate::sql::range::split_components(text, &mut components)?;
            let mut parts: [RangeBinaryParts; crate::sql::range::MAX_MULTIRANGE] =
                [(0u8, None, None); crate::sql::range::MAX_MULTIRANGE];
            for (slot, &component) in parts.iter_mut().zip(components.iter()).take(n) {
                *slot = parse_range_bounds(component, *kind, arena)?;
            }
            let stored = arena.alloc_slice_copy(&parts[..n]).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "COPY BINARY multirange exceeds the statement arena"
                )
            })?;
            Ok(BinaryFieldPlan::Multirange(stored))
        }
        _ => Ok(BinaryFieldPlan::Direct),
    }
}

/// Parses a canonical range text into its PostgreSQL binary flags byte plus the
/// two bound datums. Flags: `0x01` empty, `0x02` lower-inclusive, `0x04`
/// upper-inclusive, `0x08` lower-infinite, `0x10` upper-infinite.
fn parse_range_bounds<'a>(
    text: &str,
    kind: crate::sql::types::RangeKind,
    arena: &'a Arena,
) -> Result<RangeBinaryParts<'a>, SqlError> {
    let parsed = crate::sql::range::parse(text)?;
    if parsed.empty {
        return Ok((0x01, None, None));
    }
    let mut flags = 0u8;
    if parsed.lower_inc {
        flags |= 0x02;
    }
    if parsed.upper_inc {
        flags |= 0x04;
    }
    if parsed.lower.is_none() {
        flags |= 0x08;
    }
    if parsed.upper.is_none() {
        flags |= 0x10;
    }
    let lower = match parsed.lower {
        Some(_) => Some(crate::sql::range::lower_datum(text, kind, arena)?),
        None => None,
    };
    let upper = match parsed.upper {
        Some(_) => Some(crate::sql::range::upper_datum(text, kind, arena)?),
        None => None,
    };
    Ok((flags, lower, upper))
}

/// Writes a range's body (after the outer int32 length): the flags byte, then
/// each finite bound as int32 length + binary via `encode_value_binary`.
fn encode_range_binary(
    m: &mut crate::pg::wire::MsgOut,
    flags: u8,
    lower: Option<Datum>,
    upper: Option<Datum>,
) {
    m.u8(flags);
    if let Some(datum) = lower {
        Responder::encode_value_binary(m, &datum);
    }
    if let Some(datum) = upper {
        Responder::encode_value_binary(m, &datum);
    }
}

/// Computes every `GENERATED ALWAYS AS (expr) STORED` column from the row's
/// already-filled other columns, writing the result into `values`. A generated
/// column never references another generated column (enforced at CREATE), so a
/// snapshot of the row before this pass supplies every dependency.
fn compute_generated<'a>(
    def: &TableDef,
    generated: &constraints::ParsedDefaults<'a>,
    values: &mut [Datum<'a>; MAX_COLUMNS],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    if !generated.iter().any(|g| g.is_some()) {
        return Ok(());
    }
    let snapshot: [Datum<'a>; MAX_COLUMNS] = *values;
    let context = RowCtx {
        def,
        values: &snapshot[..def.n_columns],
        alias: None,
    };
    for (i, g) in generated.iter().enumerate() {
        if let Some(expr) = g {
            let v = eval(expr, arena, crate::sql::eval::NO_PARAMS, &context)?;
            values[i] = coerce(v, &def.columns()[i], storage, txid, arena)?;
        }
    }
    Ok(())
}

/// Fills every column the input row omitted from its folded or expression
/// default. Expression defaults run once per row with live catalog and
/// sequence access, matching INSERT (including enum/domain casts and nextval).
#[expect(clippy::too_many_arguments, reason = "row-default execution context")]
fn fill_omitted_defaults<'values, 'arena>(
    storage: &Storage,
    txid: u32,
    seq_session: &crate::sql::guc::SeqSession,
    def: &'values TableDef,
    defaults: &constraints::ParsedDefaults<'arena>,
    values: &mut [Datum<'values>; MAX_COLUMNS],
    explicit: &[bool; MAX_COLUMNS],
    arena: &'arena Arena,
) -> Result<(), SqlError>
where
    'arena: 'values,
{
    let sequence = crate::sql::sequence::SeqEval::new(storage, seq_session, txid);
    let catalog = super::query::storage_catalog(storage, arena, txid);
    let hooks = super::eval::EvalHooks {
        catalog: Some(&catalog),
        sequences: Some(&sequence),
        ..super::eval::NO_HOOKS
    };
    for (index, column) in def.columns().iter().enumerate() {
        if explicit[index] {
            continue;
        }
        values[index] =
            column_default_value(storage, txid, column, defaults[index], arena, &hooks)?;
    }
    Ok(())
}

fn column_default_value<'values, 'arena>(
    storage: &Storage,
    txid: u32,
    column: &'values crate::storage::ColumnMeta,
    expression: Option<&'arena Expr<'arena>>,
    arena: &'arena Arena,
    hooks: &super::eval::EvalHooks<'_, 'arena>,
) -> Result<Datum<'values>, SqlError>
where
    'arena: 'values,
{
    if let Some(default) = column.default.constant() {
        return Ok(default.as_datum());
    }
    let Some(expression) = expression else {
        return Ok(Datum::Null);
    };
    let value = super::eval::eval_full(
        expression,
        arena,
        crate::sql::eval::NO_PARAMS,
        &NoColumns,
        hooks,
    )?;
    coerce(value, column, storage, txid, arena)
}

/// What to do with an explicitly supplied value for a column, given the
/// statement's `OVERRIDING` mode.
#[derive(PartialEq)]
enum IdentityAction {
    /// Use the supplied value (a normal column, or an allowed identity write).
    Accept,
    /// Reject it (428C9) — a `GENERATED ALWAYS` identity without `OVERRIDING
    /// SYSTEM VALUE`.
    Reject,
    /// Ignore the supplied value and use the identity sequence — `OVERRIDING
    /// USER VALUE` on a `GENERATED BY DEFAULT` identity.
    UseSequence,
}

/// Decides how a supplied value for `column` is treated under `overriding`.
fn identity_action(def: &TableDef, column: usize, overriding: Overriding) -> IdentityAction {
    let col = &def.columns()[column];
    if !col.is_identity {
        return IdentityAction::Accept;
    }
    if col.identity_always {
        if overriding == Overriding::System {
            IdentityAction::Accept
        } else {
            IdentityAction::Reject
        }
    } else if overriding == Overriding::User {
        IdentityAction::UseSequence
    } else {
        IdentityAction::Accept
    }
}

/// The 428C9 error PostgreSQL raises for an explicit write to a `GENERATED
/// ALWAYS` identity column without `OVERRIDING SYSTEM VALUE`.
fn reject_identity_write(def: &TableDef, column: usize) -> SqlError {
    sql_err!(
        sqlstate::GENERATED_ALWAYS,
        "cannot insert a non-DEFAULT value into column \"{}\"",
        def.columns()[column].name.as_str()
    )
}

/// Rejects an explicit non-`DEFAULT` value written to a generated column (428C9),
/// the error PostgreSQL raises for `INSERT`/`UPDATE` on such a column.
fn reject_generated_write(def: &TableDef, column: usize) -> Result<(), SqlError> {
    if def.columns()[column].default.is_generated() {
        return Err(sql_err!(
            sqlstate::GENERATED_ALWAYS,
            "cannot insert a non-DEFAULT value into column \"{}\"",
            def.columns()[column].name.as_str()
        ));
    }
    Ok(())
}

/// A two-table column lookup for MERGE's ON / WHEN-condition / SET expressions:
/// a qualified name resolves to the target or source half; an unqualified name
/// searches both (ambiguous if in both, 42702).
struct MergeLookup<'d, 'v> {
    target_def: &'d TableDef,
    target_alias: &'d str,
    target: &'v [Datum<'v>],
    source_def: &'d TableDef,
    source_alias: &'d str,
    source: &'v [Datum<'v>],
}

impl<'v> ColumnLookup<'v> for MergeLookup<'_, 'v> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'v>, SqlError> {
        match qualifier {
            Some(q) if q == self.target_alias => self
                .target_def
                .column_index(name)
                .map(|i| self.target[i])
                .ok_or_else(|| undefined_column(name)),
            Some(q) if q == self.source_alias => self
                .source_def
                .column_index(name)
                .map(|i| self.source[i])
                .ok_or_else(|| undefined_column(name)),
            Some(q) => Err(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "missing FROM-clause entry for table \"{}\"",
                q
            )),
            None => match (
                self.target_def.column_index(name),
                self.source_def.column_index(name),
            ) {
                (Some(_), Some(_)) => Err(sql_err!(
                    sqlstate::AMBIGUOUS_COLUMN,
                    "column reference \"{}\" is ambiguous",
                    name
                )),
                (Some(i), None) => Ok(self.target[i]),
                (None, Some(i)) => Ok(self.source[i]),
                (None, None) => Err(undefined_column(name)),
            },
        }
    }
}

/// `MERGE INTO target USING source ON cond WHEN ...`. Source-driven: each source
/// row is matched against the target on `cond`; a match applies the first
/// satisfied WHEN MATCHED clause, a miss the first WHEN NOT MATCHED clause. A
/// target row affected twice is a cardinality error (21000).
#[allow(clippy::too_many_arguments)]
pub fn merge(
    storage: &mut Storage,
    txn: &mut TxnState,
    _scratch: &mut FixedVec<(u64, RowHome)>,
    statement: &crate::sql::ast::Merge,
    arena: &Arena,
    params: &[Datum],
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
) -> Outcome {
    use crate::sql::ast::MergeAction;
    let target_qual = crate::sql::ast::QualName {
        schema: statement.target.schema,
        name: statement.target.name,
    };
    let table_index = match resolve_dml_table(storage, &target_qual, txn.txid) {
        Ok(i) => i,
        Err(e) => return sql_fail(e),
    };
    let mut required = crate::storage::PrivilegeSet::NONE;
    for clause in statement.whens {
        required = required.union(match clause.action {
            MergeAction::Insert { .. } => crate::storage::PrivilegeSet::INSERT,
            MergeAction::Update(_) => crate::storage::PrivilegeSet::UPDATE,
            MergeAction::Delete => crate::storage::PrivilegeSet::DELETE,
            MergeAction::DoNothing => crate::storage::PrivilegeSet::NONE,
        });
    }
    if let Err(error) = require_table_privilege(storage, table_index, required, txn.txid) {
        return sql_fail(error);
    }
    if let Err(error) = storage.lock_table(
        txn.txid,
        table_index,
        crate::sql::ast::TableLockMode::RowExclusive,
        false,
    ) {
        return sql_fail(error);
    }
    let def = *storage.table_def(table_index, txn.txid);
    let target_alias = statement.target_alias.unwrap_or(statement.target.name);
    let source_alias = statement
        .source
        .alias
        .unwrap_or(if statement.source.table.is_empty() {
            ""
        } else {
            statement.source.table
        });

    // Materialize the source as `SELECT * FROM <source>`: its column set (a
    // synthesized def) and its rows.
    let source_from = crate::sql::ast::FromClause {
        base: statement.source,
        joins: &[],
    };
    let star = match arena.alloc_slice_copy(&[SelectItem::Wildcard]) {
        Ok(s) => &*s,
        Err(_) => return sql_fail(super::query::arena_full_pub()),
    };
    let source_select = crate::sql::ast::Select {
        items: star,
        distinct: false,
        distinct_on: &[],
        from: Some(source_from),
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
    };
    // Copy the synthesized def out of the borrow (it is tied to `storage`),
    // so the write path below can borrow storage mutably.
    let source_def = match super::query::synth_derived_def(
        storage,
        &source_select,
        source_alias,
        statement.source.col_alias,
        txn.txid,
        arena,
    ) {
        Ok(d) => *d,
        Err(e) => return sql_fail(e),
    };
    let source_def = &source_def;
    // Pass 1: count source rows. Pass 2: encode each to arena bytes.
    let mut n_source = 0usize;
    if let Err(e) = super::query::select_into_rows(
        storage,
        txn.txid,
        &source_select,
        arena,
        params,
        None,
        None,
        &mut |_| {
            n_source += 1;
            Ok(())
        },
    ) {
        return sql_fail(e);
    }
    let empty: &[u8] = &[];
    let source_rows: &mut [&[u8]] = match arena.alloc_slice_with(n_source, |_| empty) {
        Ok(r) => r,
        Err(_) => return sql_fail(super::query::arena_full_pub()),
    };
    {
        let mut at = 0usize;
        if let Err(e) = super::query::select_into_rows(
            storage,
            txn.txid,
            &source_select,
            arena,
            params,
            None,
            None,
            &mut |vals| {
                source_rows[at] = encode_projected_pub(vals, arena)?;
                at += 1;
                Ok(())
            },
        ) {
            return sql_fail(e);
        }
    }

    // Collect the target rows once (rowid + decoded values), plus an affected
    // flag per row for the cardinality check.
    let mut target_schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut target_schema);
    let target_schema = &target_schema[..def.n_columns];
    let n_target = match storage.visible_row_count(table_index, txn.txid) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let target_ids: &mut [u64] = match arena.alloc_slice_with(n_target, |_| 0u64) {
        Ok(s) => s,
        Err(_) => return sql_fail(super::query::arena_full_pub()),
    };
    let target_vals: &mut [&[Datum]] = match arena.alloc_slice_with(n_target, |_| &[][..]) {
        Ok(s) => s,
        Err(_) => return sql_fail(super::query::arena_full_pub()),
    };
    let affected: &mut [bool] = match arena.alloc_slice_with(n_target, |_| false) {
        Ok(s) => s,
        Err(_) => return sql_fail(super::query::arena_full_pub()),
    };
    {
        use core::ops::ControlFlow;
        // Snapshot rowid + home first (the closure cannot borrow the arena while
        // `storage` is borrowed), then decode.
        let placeholder = crate::storage::RowHome::Spilled {
            len: 0,
            sst: 0,
            commit_lsn: 0,
        };
        let ids: &mut [u64] = target_ids;
        let hms: &mut [crate::storage::RowHome] =
            match arena.alloc_slice_with(n_target, |_| placeholder) {
                Ok(s) => s,
                Err(_) => return sql_fail(super::query::arena_full_pub()),
            };
        let mut k = 0usize;
        if let Err(e) = storage.for_each_row_state(table_index, &mut |rowid, state| {
            if let Some(home) = storage.visible_row_home(table_index, rowid, state, txn.txid)?
                && k < ids.len()
            {
                ids[k] = rowid;
                hms[k] = home;
                k += 1;
            }
            Ok(ControlFlow::Continue(()))
        }) {
            return sql_fail(e);
        }
        for j in 0..k {
            let fetched = match storage.row_bytes(table_index, ids[j], hms[j], arena) {
                Ok(b) => b,
                Err(e) => return sql_fail(e),
            };
            // Copy into the arena so the decoded datums do not borrow storage
            // (the write path below borrows it mutably).
            let bytes = match arena.alloc_slice_copy(fetched) {
                Ok(b) => &*b,
                Err(_) => return sql_fail(super::query::arena_full_pub()),
            };
            let mut vals = [Datum::Null; MAX_COLUMNS];
            if let Err(e) = rowenc::decode(bytes, target_schema, &mut vals) {
                return sql_fail(e);
            }
            let owned = match arena.alloc_slice_copy(&vals[..def.n_columns]) {
                Ok(v) => &*v,
                Err(_) => return sql_fail(super::query::arena_full_pub()),
            };
            target_vals[j] = owned;
        }
    }

    let checks = match parse_checks(&def, arena) {
        Ok(c) => c,
        Err(e) => return sql_fail(e),
    };
    let generated = match parse_generated(&def, arena) {
        Ok(g) => g,
        Err(e) => return sql_fail(e),
    };
    let defaults = match parse_defaults(&def, arena) {
        Ok(d) => d,
        Err(e) => return sql_fail(e),
    };

    let mut affected_count = 0u64;
    for sbytes in source_rows.iter() {
        let n_src_cols = projected_row_width(sbytes);
        let mut sv = [Datum::Null; MAX_COLUMNS];
        for (c, slot) in sv.iter_mut().enumerate().take(n_src_cols) {
            *slot = decode_projected_pub(sbytes, c);
        }
        let sv = &sv[..source_def.n_columns.min(n_src_cols)];
        let mut matched = false;
        for j in 0..n_target {
            let lookup = MergeLookup {
                target_def: &def,
                target_alias,
                target: target_vals[j],
                source_def,
                source_alias,
                source: sv,
            };
            match eval(statement.on, arena, params, &lookup) {
                Ok(Datum::Bool(true)) => {}
                Ok(_) => continue,
                Err(e) => return sql_fail(e),
            }
            matched = true;
            // First satisfied WHEN MATCHED clause.
            for when in statement.whens.iter().filter(|w| w.matched) {
                if let Some(cond) = when.cond {
                    match eval(cond, arena, params, &lookup) {
                        Ok(Datum::Bool(true)) => {}
                        Ok(_) => continue,
                        Err(e) => return sql_fail(e),
                    }
                }
                match &when.action {
                    MergeAction::DoNothing => {}
                    MergeAction::Delete => {
                        if affected[j] {
                            return sql_fail(merge_cardinality());
                        }
                        affected[j] = true;
                        match storage.write_pending(
                            table_index,
                            target_ids[j],
                            txn.txid,
                            txn.command_id(),
                            None,
                        ) {
                            Ok(prior) => {
                                if let Err(e) = txn.touch(table_index as u32, target_ids[j], prior)
                                {
                                    return sql_fail(e);
                                }
                            }
                            Err(e) => return sql_fail(e),
                        }
                        affected_count += 1;
                    }
                    MergeAction::Update(assignments) => {
                        if affected[j] {
                            return sql_fail(merge_cardinality());
                        }
                        let mut new_values = [Datum::Null; MAX_COLUMNS];
                        new_values[..def.n_columns].copy_from_slice(target_vals[j]);
                        for (name, expression) in assignments.iter() {
                            let Some(ci) = def.column_index(name) else {
                                return sql_fail(undefined_column(name));
                            };
                            let v = match eval(expression, arena, params, &lookup) {
                                Ok(v) => v,
                                Err(e) => return sql_fail(e),
                            };
                            match coerce(v, &def.columns()[ci], storage, txn.txid, arena) {
                                Ok(v) => new_values[ci] = v,
                                Err(e) => return sql_fail(e),
                            }
                        }
                        if let Err(e) = compute_generated(
                            &def,
                            &generated,
                            &mut new_values,
                            storage,
                            txn.txid,
                            arena,
                        ) {
                            return sql_fail(e);
                        }
                        if let Err(e) = check_not_null(&def, &new_values) {
                            return sql_fail(e);
                        }
                        if let Err(e) = enforce_row_constraints(
                            storage,
                            table_index,
                            &def,
                            target_schema,
                            &new_values[..def.n_columns],
                            Some(target_ids[j]),
                            txn.txid,
                            &checks,
                            arena,
                            params,
                        ) {
                            return sql_fail(e);
                        }
                        let len = rowenc::encoded_len(&new_values[..def.n_columns]);
                        let out = match arena.alloc_slice_with(len, |_| 0u8) {
                            Ok(o) => o,
                            Err(_) => return sql_fail(super::query::arena_full_pub()),
                        };
                        rowenc::encode(&new_values[..def.n_columns], out);
                        let (loc, slice) = match storage.heap.append(out.len()) {
                            Ok(x) => x,
                            Err(e) => return sql_fail(e),
                        };
                        slice.copy_from_slice(out);
                        match storage.write_pending(
                            table_index,
                            target_ids[j],
                            txn.txid,
                            txn.command_id(),
                            Some(loc),
                        ) {
                            Ok(prior) => {
                                if let Err(e) = txn.touch(table_index as u32, target_ids[j], prior)
                                {
                                    storage.restore_pending(
                                        table_index,
                                        target_ids[j],
                                        txn.txid,
                                        prior,
                                    );
                                    return sql_fail(e);
                                }
                            }
                            Err(e) => return sql_fail(e),
                        }
                        affected[j] = true;
                        affected_count += 1;
                    }
                    MergeAction::Insert { .. } => {
                        return sql_fail(sql_err!(
                            sqlstate::SYNTAX_ERROR,
                            "INSERT is not allowed in a WHEN MATCHED clause"
                        ));
                    }
                }
                break;
            }
        }
        if !matched {
            // First satisfied WHEN NOT MATCHED clause (source columns only).
            let source_ctx = RowCtx {
                def: source_def,
                values: sv,
                alias: None,
            };
            for when in statement.whens.iter().filter(|w| !w.matched) {
                if let Some(cond) = when.cond {
                    match eval(cond, arena, params, &source_ctx) {
                        Ok(Datum::Bool(true)) => {}
                        Ok(_) => continue,
                        Err(e) => return sql_fail(e),
                    }
                }
                match &when.action {
                    MergeAction::DoNothing => {}
                    MergeAction::Insert {
                        columns,
                        values,
                        default_values,
                    } => {
                        if let Err(e) = merge_insert(
                            storage,
                            txn,
                            table_index,
                            &def,
                            columns,
                            values,
                            *default_values,
                            &source_ctx,
                            &generated,
                            &defaults,
                            seq_session,
                            arena,
                            params,
                            &checks,
                        ) {
                            return sql_fail(e);
                        }
                        affected_count += 1;
                    }
                    _ => {
                        return sql_fail(sql_err!(
                            sqlstate::SYNTAX_ERROR,
                            "only INSERT / DO NOTHING is allowed in a WHEN NOT MATCHED clause"
                        ));
                    }
                }
                break;
            }
        }
    }
    responder.command_complete(stack_format!(32, "MERGE {}", affected_count).as_str())?;
    sql_ok()
}

/// The 21000 error PostgreSQL raises when a MERGE would affect a target row a
/// second time.
fn merge_cardinality() -> SqlError {
    sql_err!(
        sqlstate::CARDINALITY_VIOLATION,
        "MERGE command cannot affect row a second time"
    )
}

/// Applies a MERGE `WHEN NOT MATCHED THEN INSERT`: builds the row from the
/// clause (values evaluated with the source row in scope), fills defaults and
/// generated columns, and stores it.
#[allow(clippy::too_many_arguments)]
fn merge_insert(
    storage: &mut Storage,
    txn: &mut TxnState,
    table_index: usize,
    def: &TableDef,
    columns: &[&str],
    values: &[&Expr],
    default_values: bool,
    source_ctx: &RowCtx,
    generated: &constraints::ParsedDefaults,
    defaults: &constraints::ParsedDefaults,
    seq_session: &crate::sql::guc::SeqSession,
    arena: &Arena,
    params: &[Datum],
    checks: &ParsedChecks,
) -> Result<(), SqlError> {
    // Target columns for the supplied values: the named list, or all columns.
    let mut targets = [0usize; MAX_COLUMNS];
    let n_targets = if columns.is_empty() {
        for (i, t) in targets.iter_mut().enumerate().take(def.n_columns) {
            *t = i;
        }
        def.n_columns
    } else {
        for (i, name) in columns.iter().enumerate() {
            let Some(ci) = def.column_index(name) else {
                return Err(undefined_column(name));
            };
            targets[i] = ci;
        }
        columns.len()
    };
    let mut row = [Datum::Null; MAX_COLUMNS];
    let mut explicit = [false; MAX_COLUMNS];
    if !default_values {
        if values.len() != n_targets {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "MERGE INSERT has {} expressions but {} target columns",
                values.len(),
                n_targets
            ));
        }
        let seq = crate::sql::sequence::SeqEval::new(storage, seq_session, txn.txid);
        let catalog = super::query::storage_catalog(storage, arena, txn.txid);
        let hooks = super::eval::EvalHooks {
            catalog: Some(&catalog),
            sequences: Some(&seq),
            ..super::eval::NO_HOOKS
        };
        for (i, expression) in values.iter().enumerate() {
            reject_generated_write(def, targets[i])?;
            let v = super::eval::eval_full(expression, arena, params, source_ctx, &hooks)?;
            row[targets[i]] = coerce(v, &def.columns()[targets[i]], storage, txn.txid, arena)?;
            explicit[targets[i]] = true;
        }
    }
    // Defaults + auto-increment + generated for the unset columns.
    {
        let seq = crate::sql::sequence::SeqEval::new(storage, seq_session, txn.txid);
        let catalog = super::query::storage_catalog(storage, arena, txn.txid);
        let hooks = super::eval::EvalHooks {
            catalog: Some(&catalog),
            sequences: Some(&seq),
            ..super::eval::NO_HOOKS
        };
        for (i, col) in def.columns().iter().enumerate() {
            if explicit[i] {
                continue;
            }
            if let Some(d) = col.default.constant() {
                row[i] = d.as_datum();
            } else if let Some(expr) = defaults[i] {
                let v = super::eval::eval_full(
                    expr,
                    arena,
                    crate::sql::eval::NO_PARAMS,
                    &NoColumns,
                    &hooks,
                )?;
                row[i] = coerce(v, col, storage, txn.txid, arena)?;
            }
        }
    }
    fill_auto_increment(
        storage,
        table_index,
        def,
        &mut row,
        &explicit,
        seq_session,
        txn.txid,
    )?;
    let mut row_arr = row;
    compute_generated(def, generated, &mut row_arr, storage, txn.txid, arena)?;
    check_not_null(def, &row_arr)?;
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut schema);
    enforce_row_constraints(
        storage,
        table_index,
        def,
        &schema[..def.n_columns],
        &row_arr[..def.n_columns],
        None,
        txn.txid,
        checks,
        arena,
        params,
    )?;
    store_row(storage, txn, table_index, None, &row_arr[..def.n_columns])
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn insert(
    storage: &mut Storage,
    txn: &mut TxnState,
    statement: &Insert,
    arena: &Arena,
    params: &[Datum],
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
    mut capture: Option<&mut dyn FnMut(&[Datum]) -> Result<(), SqlError>>,
) -> Outcome {
    let capturing = capture.is_some();
    let table_index = match resolve_dml_table(storage, &statement.table, txn.txid) {
        Ok(i) => i,
        Err(e) => return sql_fail(e),
    };
    if let Err(error) = require_table_privilege(
        storage,
        table_index,
        crate::storage::PrivilegeSet::INSERT,
        txn.txid,
    ) {
        return sql_fail(error);
    }
    if let Err(error) = storage.lock_table(
        txn.txid,
        table_index,
        crate::sql::ast::TableLockMode::RowExclusive,
        false,
    ) {
        return sql_fail(error);
    }
    let def = *storage.table_def(table_index, txn.txid);
    let checks = match parse_checks(&def, arena) {
        Ok(c) => c,
        Err(e) => return sql_fail(e),
    };

    // Resolve the ON CONFLICT arbiter once: PostgreSQL raises its inference
    // errors up front, independent of whether any row actually conflicts.
    let arbiter = match &statement.on_conflict {
        Some(oc) => match resolve_arbiter(storage, &def, oc, txn.txid) {
            Ok(a) => a,
            Err(e) => return sql_fail(e),
        },
        None => Arbiter::Any,
    };

    // Column list → target indices.
    let mut targets = [0usize; MAX_COLUMNS];
    let n_targets = if statement.columns.is_empty() {
        for (i, t) in targets.iter_mut().enumerate().take(def.n_columns) {
            *t = i;
        }
        def.n_columns
    } else {
        for (i, name) in statement.columns.iter().enumerate() {
            let Some(col) = def.column_index(name) else {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" of relation \"{}\" does not exist",
                    name,
                    statement.table.name
                ));
            };
            targets[i] = col;
        }
        statement.columns.len()
    };

    // RETURNING sends its RowDescription before any rows — unless the rows are
    // being captured for a data-modifying CTE, which describes them itself.
    if !statement.returning.is_empty() && !capturing {
        let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
        match super::query::describe_catalog_items(
            statement.returning,
            Some(&def),
            storage,
            txn.txid,
            &mut columns,
        ) {
            Ok(n) => responder.row_description(&columns[..n])?,
            Err(e) => return sql_fail(e),
        }
    }

    // INSERT ... SELECT: materialize the source rows into the arena first
    // (reading storage immutably), then insert them (mutably) — the source may
    // read the very table being written, so the two phases must not overlap.
    if let Some(sel) = statement.select {
        // Pass 1: count. A "dry" sequence evaluator resolves names (so errors
        // still surface) but does not advance any generator — the real advance
        // happens once, in the encoding pass.
        let mut count = 0usize;
        {
            let dry = crate::sql::sequence::SeqEval::dry(storage, seq_session, txn.txid);
            if let Err(e) = super::query::select_into_rows_recycling(
                storage,
                txn.txid,
                sel,
                arena,
                params,
                None,
                Some(&dry),
                &mut |_| {
                    count += 1;
                    Ok(())
                },
            ) {
                return sql_fail(e);
            }
        }
        // Pass 2: encode each projected row to self-describing arena bytes.
        let empty: &[u8] = &[];
        let rows_bytes: &mut [&[u8]] = match arena.alloc_slice_with(count, |_| empty) {
            Ok(r) => r,
            Err(_) => {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "INSERT ... SELECT result exceeds the statement arena"
                ));
            }
        };
        let mut at = 0usize;
        let mut fill = |vals: &[Datum]| -> Result<(), SqlError> {
            rows_bytes[at] = encode_projected_pub(vals, arena)?;
            at += 1;
            Ok(())
        };
        {
            let live = crate::sql::sequence::SeqEval::new(storage, seq_session, txn.txid);
            if let Err(e) = super::query::select_into_rows(
                storage,
                txn.txid,
                sel,
                arena,
                params,
                None,
                Some(&live),
                &mut fill,
            ) {
                return sql_fail(e);
            }
        }

        let default_exprs = match parse_defaults(&def, arena) {
            Ok(d) => d,
            Err(e) => return sql_fail(e),
        };
        let generated_exprs = match parse_generated(&def, arena) {
            Ok(g) => g,
            Err(e) => return sql_fail(e),
        };
        let mut inserted = 0u64;
        for bytes in rows_bytes.iter() {
            let n_src = projected_row_width(bytes);
            if n_src != n_targets {
                let msg = if n_src > n_targets {
                    "INSERT has more expressions than target columns"
                } else {
                    "INSERT has more target columns than expressions"
                };
                return sql_fail(sql_err!(sqlstate::SYNTAX_ERROR, "{}", msg));
            }
            let mut values = [Datum::Null; MAX_COLUMNS];
            let mut explicit = [false; MAX_COLUMNS];
            for i in 0..n_src {
                // A generated column cannot be a target of INSERT ... SELECT.
                if let Err(e) = reject_generated_write(&def, targets[i]) {
                    return sql_fail(e);
                }
                match identity_action(&def, targets[i], statement.overriding) {
                    IdentityAction::Reject => {
                        return sql_fail(reject_identity_write(&def, targets[i]));
                    }
                    // OVERRIDING USER VALUE: skip the query's value, use identity.
                    IdentityAction::UseSequence => continue,
                    IdentityAction::Accept => {}
                }
                let v = decode_projected_pub(bytes, i);
                let col = &def.columns()[targets[i]];
                match coerce(v, col, storage, txn.txid, arena) {
                    Ok(v) => values[targets[i]] = v,
                    Err(e) => return sql_fail(e),
                }
                explicit[targets[i]] = true;
            }
            // Defaults for columns the query does not supply: a folded constant,
            // or a per-row DEFAULT expression (evaluated under a scoped sequence
            // evaluator, so a `nextval` default advances once per inserted row).
            {
                let seq = crate::sql::sequence::SeqEval::new(storage, seq_session, txn.txid);
                let catalog = super::query::storage_catalog(storage, arena, txn.txid);
                let hooks = super::eval::EvalHooks {
                    catalog: Some(&catalog),
                    sequences: Some(&seq),
                    ..super::eval::NO_HOOKS
                };
                for (i, col) in def.columns().iter().enumerate() {
                    if explicit[i] {
                        continue;
                    }
                    if let Some(d) = col.default.constant() {
                        values[i] = d.as_datum();
                    } else if let Some(expr) = default_exprs[i] {
                        let v = match super::eval::eval_full(
                            expr,
                            arena,
                            crate::sql::eval::NO_PARAMS,
                            &NoColumns,
                            &hooks,
                        ) {
                            Ok(v) => v,
                            Err(e) => return sql_fail(e),
                        };
                        match coerce(v, col, storage, txn.txid, arena) {
                            Ok(v) => values[i] = v,
                            Err(e) => return sql_fail(e),
                        }
                    }
                }
            }
            if let Err(e) = fill_auto_increment(
                storage,
                table_index,
                &def,
                &mut values,
                &explicit,
                seq_session,
                txn.txid,
            ) {
                return sql_fail(e);
            }
            if let Err(e) = compute_generated(
                &def,
                &generated_exprs,
                &mut values,
                storage,
                txn.txid,
                arena,
            ) {
                return sql_fail(e);
            }
            if let Err(e) = check_not_null(&def, &values) {
                return sql_fail(e);
            }
            {
                let mut sch = [ColType::Bool; MAX_COLUMNS];
                def.schema(&mut sch);
                match handle_conflict(
                    storage,
                    txn,
                    table_index,
                    &def,
                    &sch[..def.n_columns],
                    &values[..def.n_columns],
                    &statement.on_conflict,
                    &arbiter,
                    &checks,
                    arena,
                    params,
                ) {
                    Ok(ConflictOutcome::Store) => {}
                    Ok(ConflictOutcome::Skip) => continue,
                    Ok(ConflictOutcome::Updated(row_bytes)) => {
                        inserted += 1;
                        if !statement.returning.is_empty()
                            && let Err(e) = emit_conflict_returning(
                                storage,
                                txn.txid,
                                &def,
                                row_bytes,
                                statement.returning,
                                arena,
                                params,
                                responder,
                                &mut capture,
                            )?
                        {
                            return sql_fail(e);
                        }
                        continue;
                    }
                    Err(e) => return sql_fail(e),
                }
            }
            let mut schema_buf = [ColType::Bool; MAX_COLUMNS];
            def.schema(&mut schema_buf);
            if let Err(e) = enforce_row_constraints(
                storage,
                table_index,
                &def,
                &schema_buf[..def.n_columns],
                &values[..def.n_columns],
                None,
                txn.txid,
                &checks,
                arena,
                params,
            ) {
                return sql_fail(e);
            }
            if let Err(e) = store_row(storage, txn, table_index, None, &values[..def.n_columns]) {
                return sql_fail(e);
            }
            if !statement.returning.is_empty()
                && let Err(e) = emit_projected(
                    storage,
                    txn.txid,
                    &def,
                    None,
                    &values[..def.n_columns],
                    statement.returning,
                    arena,
                    params,
                    responder,
                    &mut capture,
                )?
            {
                return sql_fail(e);
            }
            inserted += 1;
        }
        let tag = stack_format!(48, "INSERT 0 {}", inserted);
        if !capturing {
            responder.command_complete(tag.as_str())?;
        }
        return sql_ok();
    }

    // Non-constant DEFAULT expressions (now(), nextval(...), …) and GENERATED
    // expressions, re-parsed once and evaluated per row below.
    let default_exprs = match parse_defaults(&def, arena) {
        Ok(d) => d,
        Err(e) => return sql_fail(e),
    };
    let generated_exprs = match parse_generated(&def, arena) {
        Ok(g) => g,
        Err(e) => return sql_fail(e),
    };
    let mut inserted = 0u64;
    for row_exprs in statement.rows {
        if row_exprs.len() > n_targets {
            return sql_fail(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "INSERT has more expressions than target columns"
            ));
        }
        // A column is "explicit" when the row supplies a non-DEFAULT value for
        // it; only the others take their default (so a supplied value does not
        // waste a `nextval`). The datums borrow `def`, which outlives the row.
        let mut values = [Datum::Null; MAX_COLUMNS];
        let mut explicit = [false; MAX_COLUMNS];
        // `ignore[i]` marks a supplied value that OVERRIDING USER VALUE discards
        // in favor of the identity sequence.
        let mut ignore = [false; MAX_COLUMNS];
        for (i, expression) in row_exprs.iter().enumerate() {
            if !matches!(expression, Expr::DefaultMarker) {
                // A generated column rejects any explicit non-DEFAULT value.
                if let Err(e) = reject_generated_write(&def, targets[i]) {
                    return sql_fail(e);
                }
                match identity_action(&def, targets[i], statement.overriding) {
                    IdentityAction::Reject => {
                        return sql_fail(reject_identity_write(&def, targets[i]));
                    }
                    IdentityAction::UseSequence => ignore[targets[i]] = true,
                    IdentityAction::Accept => explicit[targets[i]] = true,
                }
            }
        }
        {
            // A per-row sequence evaluator (`nextval`/`setval` in a VALUES item
            // or a DEFAULT expression advance once per row). Scoped so its shared
            // `&storage` borrow ends before the row is written mutably below.
            let mut subquery_expressions: [Option<&Expr>; MAX_COLUMNS] = [None; MAX_COLUMNS];
            for (index, expression) in row_exprs.iter().enumerate() {
                subquery_expressions[index] = Some(expression);
            }
            let subqueries = match super::query::subquery_hooks(
                &subquery_expressions[..row_exprs.len()],
                storage,
                txn.txid,
                arena,
                params,
            ) {
                Ok(subqueries) => subqueries,
                Err(error) => return sql_fail(error),
            };
            let seq = crate::sql::sequence::SeqEval::new(storage, seq_session, txn.txid);
            let catalog = super::query::storage_catalog(storage, arena, txn.txid);
            let hooks = super::eval::EvalHooks {
                catalog: Some(&catalog),
                sequences: Some(&seq),
                subs: Some(&subqueries),
                ..super::eval::NO_HOOKS
            };
            for (i, expression) in row_exprs.iter().enumerate() {
                if matches!(expression, Expr::DefaultMarker) || ignore[targets[i]] {
                    continue; // filled from the default / identity below
                }
                let v = match super::eval::eval_full(expression, arena, params, &NoColumns, &hooks)
                {
                    Ok(v) => v,
                    Err(e) => return sql_fail(e),
                };
                let col = &def.columns()[targets[i]];
                match coerce(v, col, storage, txn.txid, arena) {
                    Ok(v) => values[targets[i]] = v,
                    Err(e) => return sql_fail(e),
                }
            }
            // Defaults for the columns the row did not set explicitly.
            for (i, col) in def.columns().iter().enumerate() {
                if explicit[i] {
                    continue;
                }
                if let Some(d) = col.default.constant() {
                    values[i] = d.as_datum();
                } else if let Some(expr) = default_exprs[i] {
                    let v = match super::eval::eval_full(
                        expr,
                        arena,
                        crate::sql::eval::NO_PARAMS,
                        &NoColumns,
                        &hooks,
                    ) {
                        Ok(v) => v,
                        Err(e) => return sql_fail(e),
                    };
                    match coerce(v, col, storage, txn.txid, arena) {
                        Ok(v) => values[i] = v,
                        Err(e) => return sql_fail(e),
                    }
                }
            }
        }
        if let Err(e) = fill_auto_increment(
            storage,
            table_index,
            &def,
            &mut values,
            &explicit,
            seq_session,
            txn.txid,
        ) {
            return sql_fail(e);
        }
        // Generated columns are computed last, from the now-filled row.
        if let Err(e) = compute_generated(
            &def,
            &generated_exprs,
            &mut values,
            storage,
            txn.txid,
            arena,
        ) {
            return sql_fail(e);
        }
        if let Err(e) = check_not_null(&def, &values) {
            return sql_fail(e);
        }
        {
            let mut sch = [ColType::Bool; MAX_COLUMNS];
            def.schema(&mut sch);
            match handle_conflict(
                storage,
                txn,
                table_index,
                &def,
                &sch[..def.n_columns],
                &values[..def.n_columns],
                &statement.on_conflict,
                &arbiter,
                &checks,
                arena,
                params,
            ) {
                Ok(ConflictOutcome::Store) => {}
                Ok(ConflictOutcome::Skip) => continue,
                Ok(ConflictOutcome::Updated(row_bytes)) => {
                    inserted += 1;
                    if !statement.returning.is_empty()
                        && let Err(e) = emit_conflict_returning(
                            storage,
                            txn.txid,
                            &def,
                            row_bytes,
                            statement.returning,
                            arena,
                            params,
                            responder,
                            &mut capture,
                        )?
                    {
                        return sql_fail(e);
                    }
                    continue;
                }
                Err(e) => return sql_fail(e),
            }
        }
        let mut schema_buf = [ColType::Bool; MAX_COLUMNS];
        def.schema(&mut schema_buf);
        if let Err(e) = enforce_row_constraints(
            storage,
            table_index,
            &def,
            &schema_buf[..def.n_columns],
            &values[..def.n_columns],
            None,
            txn.txid,
            &checks,
            arena,
            params,
        ) {
            return sql_fail(e);
        }
        if let Err(e) = store_row(storage, txn, table_index, None, &values[..def.n_columns]) {
            return sql_fail(e);
        }
        if !statement.returning.is_empty()
            && let Err(e) = emit_projected(
                storage,
                txn.txid,
                &def,
                None,
                &values[..def.n_columns],
                statement.returning,
                arena,
                params,
                responder,
                &mut capture,
            )?
        {
            return sql_fail(e);
        }
        inserted += 1;
    }
    let tag = stack_format!(48, "INSERT 0 {}", inserted);
    if !capturing {
        responder.command_complete(tag.as_str())?;
    }
    sql_ok()
}

/// Projects `values` through `items` and emits one DataRow.
/// Emits a `RETURNING` row for the row an `ON CONFLICT DO UPDATE` just wrote,
/// decoding the arena-encoded updated bytes so the projection sees the
/// post-update values (matching PostgreSQL, which returns the updated row).
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn emit_conflict_returning(
    storage: &Storage,
    txid: u32,
    def: &TableDef,
    row_bytes: &[u8],
    items: &[SelectItem],
    arena: &Arena,
    params: &[Datum],
    responder: &mut Responder,
    capture: &mut Option<&mut dyn FnMut(&[Datum]) -> Result<(), SqlError>>,
) -> Result<Result<(), SqlError>, WireFull> {
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut schema);
    let mut updated = [Datum::Null; MAX_COLUMNS];
    if let Err(e) = rowenc::decode(row_bytes, &schema[..def.n_columns], &mut updated) {
        return Ok(Err(e));
    }
    emit_projected(
        storage,
        txid,
        def,
        None,
        &updated[..def.n_columns],
        items,
        arena,
        params,
        responder,
        capture,
    )
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn emit_projected(
    storage: &Storage,
    txid: u32,
    def: &TableDef,
    alias: Option<&str>,
    values: &[Datum],
    items: &[SelectItem],
    arena: &Arena,
    params: &[Datum],
    responder: &mut Responder,
    capture: &mut Option<&mut dyn FnMut(&[Datum]) -> Result<(), SqlError>>,
) -> Result<Result<(), SqlError>, WireFull> {
    let context = RowCtx { def, values, alias };
    let mut expressions: [Option<&Expr>; MAX_PROJ] = [None; MAX_PROJ];
    let mut expression_count = 0usize;
    for item in items {
        let expression = match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                Some(*expression)
            }
            _ => None,
        };
        if let Some(expression) = expression {
            expressions[expression_count] = Some(expression);
            expression_count += 1;
        }
    }
    let subqueries = match super::query::subquery_hooks(
        &expressions[..expression_count],
        storage,
        txid,
        arena,
        params,
    ) {
        Ok(subqueries) => subqueries,
        Err(error) => return Ok(Err(error)),
    };
    let hooks = EvalHooks {
        subs: Some(&subqueries),
        ..NO_HOOKS
    };
    let mut projected = [Datum::Null; MAX_PROJ];
    let mut n = 0;
    for item in items {
        match item {
            SelectItem::Wildcard => {
                for v in context.values {
                    projected[n] = *v;
                    n += 1;
                }
            }
            SelectItem::TableWildcard(q) => {
                if !crate::sql::eval::qualifier_answers_target(def, alias, q) {
                    return Ok(Err(sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "missing FROM-clause entry for table \"{}\"",
                        q
                    )));
                }
                for v in context.values {
                    projected[n] = *v;
                    n += 1;
                }
            }
            SelectItem::RecordStar(base) => {
                match super::eval::record_star_expand(base, arena, params, &context, &hooks) {
                    Ok(fields) => {
                        for f in fields {
                            projected[n] = f.value;
                            n += 1;
                        }
                    }
                    Err(e) => return Ok(Err(e)),
                }
            }
            SelectItem::Expr { expression, .. } => {
                match eval_full(expression, arena, params, &context, &hooks) {
                    Ok(v) => {
                        projected[n] = v;
                        n += 1;
                    }
                    Err(e) => return Ok(Err(e)),
                }
            }
        }
    }
    // A data-modifying CTE captures its RETURNING rows in memory instead of
    // streaming them to the client.
    if let Some(sink) = capture.as_deref_mut() {
        if let Err(e) = sink(&projected[..n]) {
            return Ok(Err(e));
        }
    } else {
        responder.data_row(&projected[..n])?;
    }
    Ok(Ok(()))
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn update(
    storage: &mut Storage,
    txn: &mut TxnState,
    scratch: &mut FixedVec<(u64, RowHome)>,
    statement: &Update,
    arena: &Arena,
    params: &[Datum],
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
    mut capture: Option<&mut dyn FnMut(&[Datum]) -> Result<(), SqlError>>,
) -> Outcome {
    let capturing = capture.is_some();
    let table_index = match resolve_dml_table(storage, &statement.table, txn.txid) {
        Ok(i) => i,
        Err(e) => return sql_fail(e),
    };
    if let Err(error) = require_table_privilege(
        storage,
        table_index,
        crate::storage::PrivilegeSet::UPDATE,
        txn.txid,
    ) {
        return sql_fail(error);
    }
    if let Err(error) = storage.lock_table(
        txn.txid,
        table_index,
        crate::sql::ast::TableLockMode::RowExclusive,
        false,
    ) {
        return sql_fail(error);
    }
    let def = *storage.table_def(table_index, txn.txid);
    let checks = match parse_checks(&def, arena) {
        Ok(c) => c,
        Err(e) => return sql_fail(e),
    };
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut schema);
    let schema = &schema[..def.n_columns];

    // Resolve assignment targets once.
    let mut targets = [0usize; MAX_COLUMNS];
    for (i, (name, _)) in statement.assignments.iter().enumerate() {
        let Some(col) = def.column_index(name) else {
            return sql_fail(sql_err!(
                sqlstate::UNDEFINED_COLUMN,
                "column \"{}\" of relation \"{}\" does not exist",
                name,
                statement.table.name
            ));
        };
        targets[i] = col;
    }
    // A generated column can only be updated to DEFAULT (which recomputes it).
    for (a, (_, expression)) in statement.assignments.iter().enumerate() {
        if def.columns()[targets[a]].default.is_generated()
            && !matches!(expression, Expr::DefaultMarker)
        {
            return sql_fail(sql_err!(
                sqlstate::GENERATED_ALWAYS,
                "column \"{}\" can only be updated to DEFAULT",
                def.columns()[targets[a]].name.as_str()
            ));
        }
    }
    let generated_exprs = match parse_generated(&def, arena) {
        Ok(g) => g,
        Err(e) => return sql_fail(e),
    };
    let defaults = match parse_defaults(&def, arena) {
        Ok(d) => d,
        Err(e) => return sql_fail(e),
    };

    let subs = match super::query::subquery_hooks(
        &[statement.where_clause],
        storage,
        txn.txid,
        arena,
        params,
    ) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let catalog = super::query::storage_catalog(storage, arena, txn.txid);
    let hooks = super::eval::EvalHooks {
        group: None,
        aggs: None,
        subs: Some(&subs),
        windows: None,
        catalog: Some(&catalog),
        srf_index: None,
        sequences: None,
    };
    let collect = if let Some(from) = statement.from {
        collect_join_matches(
            storage,
            table_index,
            &def,
            statement.alias,
            schema,
            from,
            statement.where_clause,
            arena,
            params,
            txn.txid,
            scratch,
        )
    } else {
        collect_matches(
            storage,
            table_index,
            statement.alias,
            txn.txid,
            schema,
            statement.where_clause,
            arena,
            params,
            &hooks,
            scratch,
        )
    };
    if let Err(e) = collect {
        return sql_fail(e);
    }

    // PostgreSQL obtains a row lock before it computes or publishes any new
    // image. Collecting all target rowids first makes a default wait
    // suspendable without partially executing the statement.
    let changes_key = targets[..statement.assignments.len()]
        .iter()
        .any(|&column| {
            def.columns()[column].primary
                || def.columns()[column].unique
                || def
                    .uniques()
                    .iter()
                    .any(|constraint| constraint.columns().contains(&(column as u16)))
                || storage
                    .unique_indexes_for(def.schema.as_str(), def.name.as_str(), txn.txid)
                    .any(|index| index.columns[..index.n_cols].contains(&(column as u16)))
        });
    let lock_strength = if changes_key {
        crate::sql::ast::LockStrength::Update
    } else {
        crate::sql::ast::LockStrength::NoKeyUpdate
    };
    for &(rowid, _) in scratch.iter() {
        match storage.acquire_row_lock(
            table_index,
            rowid,
            txn.txid,
            lock_strength,
            crate::sql::ast::LockWait::Wait,
        ) {
            Ok(crate::sql::lock::LockDecision::Acquired) => {}
            Ok(crate::sql::lock::LockDecision::Waiting) => {
                return sql_fail(sql_err!(
                    sqlstate::INTERNAL_LOCK_WAIT,
                    "statement is waiting for a row lock"
                ));
            }
            Ok(crate::sql::lock::LockDecision::Skipped) => {
                unreachable!("data modification does not request SKIP LOCKED")
            }
            Err(error) => return sql_fail(error),
        }
    }

    if !statement.returning.is_empty() && !capturing {
        let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
        match super::query::describe_catalog_items_as(
            statement.returning,
            Some(&def),
            statement.alias,
            storage,
            txn.txid,
            &mut columns,
        ) {
            Ok(n) => responder.row_description(&columns[..n])?,
            Err(e) => return sql_fail(e),
        }
    }

    let mut updated = 0u64;
    for i in 0..scratch.len() {
        let (rowid, home) = scratch[i];
        // Build the new row image in the statement arena so the heap
        // borrow ends before the heap is appended to.
        // An arena-owned copy of the old row bytes: the referential-action
        // pass below needs the old values after storage mutates.
        let fetched = match storage.row_bytes(table_index, rowid, home, arena) {
            Ok(b) => b,
            Err(e) => return sql_fail(e),
        };
        let row_bytes = match arena.alloc_slice_copy(fetched) {
            Ok(b) => &*b,
            Err(_) => {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "updated rows exceed the statement arena"
                ));
            }
        };
        let new_bytes = {
            let mut values = [Datum::Null; MAX_COLUMNS];
            if let Err(e) = rowenc::decode(row_bytes, schema, &mut values) {
                return sql_fail(e);
            }
            let mut new_values = [Datum::Null; MAX_COLUMNS];
            new_values[..def.n_columns].copy_from_slice(&values[..def.n_columns]);
            let context = RowCtx {
                def: &def,
                values: &values[..def.n_columns],
                alias: statement.alias,
            };
            if let Some(from) = statement.from {
                // UPDATE ... FROM: evaluate the assignments against the target
                // row joined with the first matching FROM row.
                let mut observed = [&Expr::Null; MAX_COLUMNS];
                for (index, (_, expression)) in statement.assignments.iter().enumerate() {
                    observed[index] = expression;
                }
                let sequences = crate::sql::sequence::SeqEval::new(storage, seq_session, txn.txid);
                let catalog = super::query::storage_catalog(storage, arena, txn.txid);
                let hooks = super::eval::EvalHooks {
                    catalog: Some(&catalog),
                    sequences: Some(&sequences),
                    ..super::eval::NO_HOOKS
                };
                let mut set_err: Option<SqlError> = None;
                let r = super::query::first_from_match(
                    storage,
                    from,
                    txn.txid,
                    statement.where_clause,
                    &observed[..statement.assignments.len()],
                    arena,
                    params,
                    &context,
                    &mut |combined| {
                        for (a, (_, expression)) in statement.assignments.iter().enumerate() {
                            // A generated target's `= DEFAULT` is a no-op here; it
                            // is recomputed from the finished row below.
                            if def.columns()[targets[a]].default.is_generated() {
                                continue;
                            }
                            if matches!(expression, Expr::DefaultMarker) {
                                new_values[targets[a]] = column_default_value(
                                    storage,
                                    txn.txid,
                                    &def.columns()[targets[a]],
                                    defaults[targets[a]],
                                    arena,
                                    &hooks,
                                )?;
                                continue;
                            }
                            let v = eval_full(expression, arena, params, &combined, &hooks)?;
                            new_values[targets[a]] =
                                coerce(v, &def.columns()[targets[a]], storage, txn.txid, arena)?;
                        }
                        Ok(())
                    },
                );
                match r {
                    Ok(_) => {}
                    Err(e) => set_err = Some(e),
                }
                if let Some(e) = set_err {
                    return sql_fail(e);
                }
            } else {
                // `nextval`/`setval` in a SET expression advance once per updated
                // row; a scoped sequence evaluator (shared `&storage`) supplies
                // them and is dropped before the row is written back mutably.
                let seq = crate::sql::sequence::SeqEval::new(storage, seq_session, txn.txid);
                let catalog = super::query::storage_catalog(storage, arena, txn.txid);
                let hooks = super::eval::EvalHooks {
                    catalog: Some(&catalog),
                    sequences: Some(&seq),
                    ..super::eval::NO_HOOKS
                };
                for (a, (_, expression)) in statement.assignments.iter().enumerate() {
                    if def.columns()[targets[a]].default.is_generated() {
                        continue; // recomputed from the finished row below
                    }
                    if matches!(expression, Expr::DefaultMarker) {
                        new_values[targets[a]] = match column_default_value(
                            storage,
                            txn.txid,
                            &def.columns()[targets[a]],
                            defaults[targets[a]],
                            arena,
                            &hooks,
                        ) {
                            Ok(value) => value,
                            Err(e) => return sql_fail(e),
                        };
                        continue;
                    }
                    let v =
                        match super::eval::eval_full(expression, arena, params, &context, &hooks) {
                            Ok(v) => v,
                            Err(e) => return sql_fail(e),
                        };
                    let col = &def.columns()[targets[a]];
                    match coerce(v, col, storage, txn.txid, arena) {
                        Ok(v) => new_values[targets[a]] = v,
                        Err(e) => return sql_fail(e),
                    }
                }
            }
            // Every generated column is recomputed from the updated row (a change
            // to any dependency must flow through).
            if let Err(e) = compute_generated(
                &def,
                &generated_exprs,
                &mut new_values,
                storage,
                txn.txid,
                arena,
            ) {
                return sql_fail(e);
            }
            if let Err(e) = check_not_null(&def, &new_values) {
                return sql_fail(e);
            }
            if let Err(e) = enforce_row_constraints(
                storage,
                table_index,
                &def,
                schema,
                &new_values[..def.n_columns],
                Some(rowid),
                txn.txid,
                &checks,
                arena,
                params,
            ) {
                return sql_fail(e);
            }
            let len = rowenc::encoded_len(&new_values[..def.n_columns]);
            let out = match arena.alloc_slice_with(len, |_| 0u8) {
                Ok(o) => o,
                Err(_) => {
                    return sql_fail(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "updated rows exceed the statement arena"
                    ));
                }
            };
            rowenc::encode(&new_values[..def.n_columns], out);
            &*out
        };
        let (new_loc, slice) = match storage.heap.append(new_bytes.len()) {
            Ok(x) => x,
            Err(e) => return sql_fail(e),
        };
        slice.copy_from_slice(new_bytes);
        match storage.write_pending(
            table_index,
            rowid,
            txn.txid,
            txn.command_id(),
            Some(new_loc),
        ) {
            Ok(prior) => {
                if let Err(e) = txn.touch(table_index as u32, rowid, prior) {
                    storage.restore_pending(table_index, rowid, txn.txid, prior);
                    return sql_fail(e);
                }
            }
            Err(e) => return sql_fail(e),
        }
        // With the new parent row in place, apply each referencing key's
        // ON UPDATE action when a referenced column changed (NO ACTION /
        // RESTRICT block; CASCADE / SET NULL / SET DEFAULT rewrite the
        // referencing rows — their own constraints re-check against the new
        // key). Both row images are arena-owned, so the cascade may mutate
        // storage.
        {
            let mut old_row = [Datum::Null; MAX_COLUMNS];
            if let Err(e) = rowenc::decode(row_bytes, schema, &mut old_row) {
                return sql_fail(e);
            }
            let mut new_row = [Datum::Null; MAX_COLUMNS];
            if let Err(e) = rowenc::decode(new_bytes, schema, &mut new_row) {
                return sql_fail(e);
            }
            let referenced_key_changed = match referenced_key_changed(
                storage,
                def.schema.as_str(),
                def.name.as_str(),
                &old_row[..def.n_columns],
                &new_row[..def.n_columns],
                txn.txid,
            ) {
                Ok(changed) => changed,
                Err(error) => return sql_fail(error),
            };
            if referenced_key_changed
                && let Err(e) = apply_fk_parent_actions(
                    storage,
                    txn,
                    def.schema.as_str(),
                    def.name.as_str(),
                    &old_row[..def.n_columns],
                    Some(&new_row[..def.n_columns]),
                    arena,
                    params,
                    seq_session,
                    MAX_FK_CASCADE_DEPTH,
                )
            {
                return sql_fail(e);
            }
        }
        if !statement.returning.is_empty() {
            let mut new_values = [Datum::Null; MAX_COLUMNS];
            if let Err(e) = rowenc::decode(storage.heap.get(new_loc), schema, &mut new_values) {
                return sql_fail(e);
            }
            if let Err(e) = emit_projected(
                storage,
                txn.txid,
                &def,
                statement.alias,
                &new_values[..def.n_columns],
                statement.returning,
                arena,
                params,
                responder,
                &mut capture,
            )? {
                return sql_fail(e);
            }
        }
        updated += 1;
    }
    let tag = stack_format!(48, "UPDATE {}", updated);
    if !capturing {
        responder.command_complete(tag.as_str())?;
    }
    sql_ok()
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn delete(
    storage: &mut Storage,
    txn: &mut TxnState,
    scratch: &mut FixedVec<(u64, RowHome)>,
    statement: &Delete,
    arena: &Arena,
    params: &[Datum],
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
    mut capture: Option<&mut dyn FnMut(&[Datum]) -> Result<(), SqlError>>,
) -> Outcome {
    let capturing = capture.is_some();
    let table_index = match resolve_dml_table(storage, &statement.table, txn.txid) {
        Ok(i) => i,
        Err(e) => return sql_fail(e),
    };
    if let Err(error) = require_table_privilege(
        storage,
        table_index,
        crate::storage::PrivilegeSet::DELETE,
        txn.txid,
    ) {
        return sql_fail(error);
    }
    if let Err(error) = storage.lock_table(
        txn.txid,
        table_index,
        crate::sql::ast::TableLockMode::RowExclusive,
        false,
    ) {
        return sql_fail(error);
    }
    let def = *storage.table_def(table_index, txn.txid);
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut schema);
    let schema = &schema[..def.n_columns];

    let subs = match super::query::subquery_hooks(
        &[statement.where_clause],
        storage,
        txn.txid,
        arena,
        params,
    ) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let catalog = super::query::storage_catalog(storage, arena, txn.txid);
    let hooks = super::eval::EvalHooks {
        group: None,
        aggs: None,
        subs: Some(&subs),
        windows: None,
        catalog: Some(&catalog),
        srf_index: None,
        sequences: None,
    };
    let collect = if let Some(using) = statement.using {
        collect_join_matches(
            storage,
            table_index,
            &def,
            statement.alias,
            schema,
            using,
            statement.where_clause,
            arena,
            params,
            txn.txid,
            scratch,
        )
    } else {
        collect_matches(
            storage,
            table_index,
            statement.alias,
            txn.txid,
            schema,
            statement.where_clause,
            arena,
            params,
            &hooks,
            scratch,
        )
    };
    if let Err(e) = collect {
        return sql_fail(e);
    }
    for &(rowid, _) in scratch.iter() {
        match storage.acquire_row_lock(
            table_index,
            rowid,
            txn.txid,
            crate::sql::ast::LockStrength::Update,
            crate::sql::ast::LockWait::Wait,
        ) {
            Ok(crate::sql::lock::LockDecision::Acquired) => {}
            Ok(crate::sql::lock::LockDecision::Waiting) => {
                return sql_fail(sql_err!(
                    sqlstate::INTERNAL_LOCK_WAIT,
                    "statement is waiting for a row lock"
                ));
            }
            Ok(crate::sql::lock::LockDecision::Skipped) => {
                unreachable!("data modification does not request SKIP LOCKED")
            }
            Err(error) => return sql_fail(error),
        }
    }
    if !statement.returning.is_empty() && !capturing {
        let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
        match super::query::describe_catalog_items_as(
            statement.returning,
            Some(&def),
            statement.alias,
            storage,
            txn.txid,
            &mut columns,
        ) {
            Ok(n) => responder.row_description(&columns[..n])?,
            Err(e) => return sql_fail(e),
        }
    }
    let referenced = table_is_referenced(storage, def.schema.as_str(), def.name.as_str(), txn.txid);
    for i in 0..scratch.len() {
        let (rowid, old_home) = scratch[i];
        if !statement.returning.is_empty() || referenced {
            // The cascade below mutates storage, so the row image is decoded
            // from an arena-owned copy.
            let fetched = match storage.row_bytes(table_index, rowid, old_home, arena) {
                Ok(b) => b,
                Err(e) => return sql_fail(e),
            };
            let old_copy = match arena.alloc_slice_copy(fetched) {
                Ok(c) => c,
                Err(_) => {
                    return sql_fail(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "deleted rows exceed the statement arena"
                    ));
                }
            };
            let mut old_values = [Datum::Null; MAX_COLUMNS];
            if let Err(e) = rowenc::decode(old_copy, schema, &mut old_values) {
                return sql_fail(e);
            }
            // Apply each referencing key's ON DELETE action (NO ACTION /
            // RESTRICT block; CASCADE / SET NULL / SET DEFAULT rewrite the
            // referencing rows).
            if referenced
                && let Err(e) = apply_fk_parent_actions(
                    storage,
                    txn,
                    def.schema.as_str(),
                    def.name.as_str(),
                    &old_values[..def.n_columns],
                    None,
                    arena,
                    params,
                    seq_session,
                    MAX_FK_CASCADE_DEPTH,
                )
            {
                return sql_fail(e);
            }
            if !statement.returning.is_empty()
                && let Err(e) = emit_projected(
                    storage,
                    txn.txid,
                    &def,
                    statement.alias,
                    &old_values[..def.n_columns],
                    statement.returning,
                    arena,
                    params,
                    responder,
                    &mut capture,
                )?
            {
                return sql_fail(e);
            }
        }
        match storage.write_pending(table_index, rowid, txn.txid, txn.command_id(), None) {
            Ok(prior) => {
                if let Err(e) = txn.touch(table_index as u32, rowid, prior) {
                    storage.restore_pending(table_index, rowid, txn.txid, prior);
                    return sql_fail(e);
                }
            }
            Err(e) => return sql_fail(e),
        }
    }
    let tag = stack_format!(48, "DELETE {}", scratch.len());
    if !capturing {
        responder.command_complete(tag.as_str())?;
    }
    sql_ok()
}

/// TRUNCATE: removes every visible row of the listed tables through the
/// transactional delete machinery (so a rolled-back TRUNCATE restores them),
/// with PostgreSQL's structural foreign-key rule — a table referenced by a
/// table outside the list cannot be truncated; CASCADE pulls the referencing
/// tables in transitively, with a NOTICE per addition. RESTART IDENTITY
/// resets each serial column's sequence, transactionally (an undo entry
/// restores the prior position on rollback).
pub fn truncate(
    storage: &mut Storage,
    txn: &mut TxnState,
    tables: &[QualName],
    restart_identity: bool,
    cascade: bool,
    responder: &mut Responder,
) -> Outcome {
    // Resolve the listed tables (views are not truncatable).
    let mut list: [usize; MAX_TRUNCATE_TABLES] = [0; MAX_TRUNCATE_TABLES];
    let mut n = 0usize;
    for name in tables {
        let index = match storage.resolve_relation(name.schema, name.name, txn.txid) {
            Some(crate::storage::ResolvedRelation::View(_)) => {
                return sql_fail(sql_err!(
                    crate::sql::eval::sqlstate::WRONG_OBJECT_TYPE,
                    "\"{}\" is not a table",
                    name.name
                ));
            }
            Some(crate::storage::ResolvedRelation::Table(index)) => index,
            _ => return sql_fail(undefined_qual(name)),
        };
        if !list[..n].contains(&index) {
            if n == MAX_TRUNCATE_TABLES {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many tables in TRUNCATE"
                ));
            }
            list[n] = index;
            n += 1;
        }
    }
    // Foreign-key closure: a table outside the list referencing a listed one
    // blocks the truncate — or joins it under CASCADE.
    loop {
        let mut grew = false;
        for other in 0..storage.table_count() {
            if !storage.table(other).visible_to(txn.txid) || list[..n].contains(&other) {
                continue;
            }
            let other_def = storage.table_def(other, txn.txid);
            let refs_listed = other_def.fkeys().iter().any(|fk| {
                list[..n].iter().any(|&t| {
                    let tdef = storage.table_def(t, txn.txid);
                    tdef.schema.as_str() == fk.parent_schema.as_str()
                        && tdef.name.as_str() == fk.parent.as_str()
                })
            });
            if !refs_listed {
                continue;
            }
            if !cascade {
                return sql_fail(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "cannot truncate a table referenced in a foreign key constraint"
                ));
            }
            if n == MAX_TRUNCATE_TABLES {
                return sql_fail(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many tables in TRUNCATE"
                ));
            }
            let name = other_def.name;
            responder.notice(
                crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                stack_format!(160, "truncate cascades to table \"{}\"", name.as_str()).as_str(),
            )?;
            list[n] = other;
            n += 1;
            grew = true;
        }
        if !grew {
            break;
        }
    }
    // PostgreSQL acquires ACCESS EXCLUSIVE on the complete CASCADE closure
    // before changing any row. Acquire the whole set before mutation so a
    // wait can replay the statement without exposing a partial truncate.
    for &table_index in &list[..n] {
        if let Err(error) = require_table_privilege(
            storage,
            table_index,
            crate::storage::PrivilegeSet::TRUNCATE,
            txn.txid,
        ) {
            return sql_fail(error);
        }
        if let Err(error) = storage.lock_table(
            txn.txid,
            table_index,
            crate::sql::ast::TableLockMode::AccessExclusive,
            false,
        ) {
            return sql_fail(error);
        }
    }
    // Remove every visible row, transactionally.
    for &table_index in &list[..n] {
        let mut rowids: [u64; 4096] = [0; 4096];
        loop {
            let mut count = 0usize;
            if let Err(error) = storage.for_each_row_state(table_index, &mut |rowid, state| {
                use core::ops::ControlFlow;
                if storage
                    .visible_row_home(table_index, rowid, state, txn.txid)?
                    .is_none()
                {
                    return Ok(ControlFlow::Continue(()));
                }
                if count == rowids.len() {
                    return Ok(ControlFlow::Break(()));
                }
                rowids[count] = rowid;
                count += 1;
                Ok(ControlFlow::Continue(()))
            }) {
                return sql_fail(error);
            }
            if count == 0 {
                break;
            }
            for &rowid in &rowids[..count] {
                match storage.write_pending(table_index, rowid, txn.txid, txn.command_id(), None) {
                    Ok(prior) => {
                        if let Err(e) = txn.touch(table_index as u32, rowid, prior) {
                            storage.restore_pending(table_index, rowid, txn.txid, prior);
                            return sql_fail(e);
                        }
                    }
                    Err(e) => return sql_fail(e),
                }
            }
        }
        if restart_identity {
            let def = *storage.table_def(table_index, txn.txid);
            for c in 0..def.n_columns {
                if !def.columns()[c].auto_increment {
                    continue;
                }
                if let Some(sequence_slot) = storage.generated_sequence_slot(
                    def.schema.as_str(),
                    def.name.as_str(),
                    def.columns()[c].name.as_str(),
                    txn.txid,
                ) {
                    let start_value = storage.sequence_for(sequence_slot, txn.txid).start_value;
                    let prior = storage.reset_sequence_value(sequence_slot, txn.txid, start_value);
                    if let Err(error) =
                        txn.record_ddl(crate::sql::txn::DdlUndo::OwnedSequenceReset {
                            sequence: sequence_slot as u32,
                            prior,
                        })
                    {
                        storage.restore_sequence_value(sequence_slot, prior);
                        return sql_fail(error);
                    }
                    continue;
                }
                let prior = storage.table(table_index).serial_last[c];
                if let Err(e) = txn.record_ddl(crate::sql::txn::DdlUndo::SequenceReset {
                    table: table_index as u32,
                    column: c as u16,
                    prior,
                }) {
                    return sql_fail(e);
                }
                let t = storage.table_mut(table_index);
                t.serial_last[c] = 0;
                t.serial_dirty = true;
            }
        }
    }
    let mut truncated_tables = [0_u16; crate::sql::txn::MAX_TRUNCATE_TABLES];
    for (position, &table_index) in list[..n].iter().enumerate() {
        truncated_tables[position] = table_index as u16;
    }
    if let Err(error) = txn.record_truncate(crate::sql::txn::TruncateEvent {
        command_id: txn.command_id(),
        tables: truncated_tables,
        table_count: n,
        cascade,
        restart_identity,
    }) {
        return sql_fail(error);
    }
    responder.command_complete("TRUNCATE TABLE")?;
    sql_ok()
}

/// The most tables one TRUNCATE can name, its CASCADE closure included.
const MAX_TRUNCATE_TABLES: usize = crate::sql::txn::MAX_TRUNCATE_TABLES;

/// ALTER TABLE, autocommit-only: rewrites are journaled as DROP, CREATE,
/// full re-UPSERT within one WAL batch, so replay reproduces the new
/// shape atomically. Two-phase in memory: all new row images are prepared
/// first, then the definition and row map swap; a failure part-way leaves
/// the table untouched (only heap bytes leak until compaction).
/// Whether `ALTER COLUMN ... TYPE` may cast `from` to `to` without a `USING`
/// clause — i.e. PostgreSQL has an assignment (or implicit) cast for the pair.
/// Mirrors `pg_cast` (castcontext in {'a','i'}) over this engine's types, plus
/// the I/O rule: any type casts *to* a string type as an assignment, but *from*
/// a string type is explicit-only (needs USING).
fn alter_type_auto_castable(from: ColType, to: ColType) -> bool {
    use ColType::*;
    if from == to {
        return true;
    }
    let is_string = |t| matches!(t, Text | Varchar | Bpchar | Name);
    // to-string: assignment via the output function; from-string: explicit.
    if is_string(to) {
        return true;
    }
    if is_string(from) {
        return false;
    }
    let numeric = |t| matches!(t, Int2 | Int4 | Int8 | Float4 | Float8 | Numeric);
    if numeric(from) && numeric(to) {
        return true;
    }
    // The remaining assignment/implicit pairs among the date/time and json
    // families, per pg_cast.
    matches!(
        (from, to),
        (Date, Timestamp | Timestamptz)
            | (Timestamp, Date | Time | Timestamptz)
            | (Timestamptz, Date | Time | Timestamp | Timetz)
            | (Time, Timetz | Interval)
            | (Timetz, Time)
            | (Interval, Time)
            | (Json, Jsonb)
            | (Jsonb, Json)
    )
}

/// Validates every committed row against a table's whole constraint set, for
/// ALTER TABLE ADD CONSTRAINT — the just-added constraint is the only one that
/// can fail, and its violation surfaces with the same SQLSTATE the INSERT path
/// would give (23514 CHECK, 23505 UNIQUE, 23503 FK, 23502 the PK's NOT NULL).
fn validate_all_rows(
    storage: &Storage,
    table_index: usize,
    new_def: &TableDef,
    txid: u32,
    arena: &Arena,
) -> Result<(), SqlError> {
    let checks = parse_checks(new_def, arena)?;
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    new_def.schema(&mut schema);
    let schema = &schema[..new_def.n_columns];
    let mut result = Ok(());
    storage.for_each_row_state(table_index, &mut |rowid, state| {
        use core::ops::ControlFlow;
        let Some(home) = storage.visible_row_home_at(
            table_index,
            rowid,
            state,
            txid,
            crate::storage::SNAPSHOT_ALL,
            storage.commit_snapshot(),
        )?
        else {
            return Ok(ControlFlow::Continue(()));
        };
        let bytes = storage.row_bytes(table_index, rowid, home, arena)?;
        let mut values = [Datum::Null; MAX_COLUMNS];
        rowenc::decode(bytes, schema, &mut values)?;
        let values = &values[..new_def.n_columns];
        let check = check_not_null(new_def, values).and_then(|()| {
            enforce_row_constraints(
                storage,
                table_index,
                new_def,
                schema,
                values,
                Some(rowid),
                txid,
                &checks,
                arena,
                &[],
            )
        });
        if let Err(e) = check {
            result = Err(e);
            return Ok(ControlFlow::Break(()));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    result
}

/// Removes a named constraint from `def` for ALTER TABLE DROP CONSTRAINT:
/// a CHECK, a table-level UNIQUE/PRIMARY KEY, or an FK by its stored name, plus
/// the generated names of a single-column primary key (`<table>_pkey`) or
/// unique column (`<table>_<column>_key`). Returns whether one was found.
fn drop_named_constraint(def: &mut TableDef, name: &str) -> bool {
    for i in 0..def.n_checks {
        if def.checks[i].name.as_str() == name {
            for j in i..def.n_checks - 1 {
                def.checks[j] = def.checks[j + 1];
            }
            def.n_checks -= 1;
            return true;
        }
    }
    for i in 0..def.n_uniques {
        if def.uniques[i].name.as_str() == name {
            for j in i..def.n_uniques - 1 {
                def.uniques[j] = def.uniques[j + 1];
            }
            def.n_uniques -= 1;
            return true;
        }
    }
    for i in 0..def.n_fkeys {
        if def.fkeys[i].name.as_str() == name {
            for j in i..def.n_fkeys - 1 {
                def.fkeys[j] = def.fkeys[j + 1];
            }
            def.n_fkeys -= 1;
            return true;
        }
    }
    // A single-column primary key is a column flag named "<table>_pkey".
    let pkey = crate::stack_format!(96, "{}_pkey", def.name.as_str());
    if name == pkey.as_str()
        && let Some(c) = def.columns[..def.n_columns].iter_mut().find(|c| c.primary)
    {
        c.primary = false;
        c.unique = false;
        return true;
    }
    // A single-column UNIQUE is "<table>_<column>_key".
    for i in 0..def.n_columns {
        if def.columns[i].unique && !def.columns[i].primary {
            let key = crate::stack_format!(
                128,
                "{}_{}_key",
                def.name.as_str(),
                def.columns[i].name.as_str()
            );
            if name == key.as_str() {
                def.columns[i].unique = false;
                return true;
            }
        }
    }
    false
}

/// If two rewritten rows `a` and `b` collide on a uniqueness constraint of
/// `new_def` — a single-column UNIQUE/PRIMARY KEY flag or a multi-column key,
/// with every key column non-NULL and equal — returns the constraint's
/// PostgreSQL name. Used to validate a uniqueness constraint added alongside an
/// ALTER row rewrite, against the transformed images, before anything is
/// journaled (a NULL in any key column makes the rows distinct).
fn rewritten_dup_name(new_def: &TableDef, a: &[Datum], b: &[Datum]) -> Option<StackStr<128>> {
    use core::fmt::Write as _;
    let eq = |i: usize| {
        !a[i].is_null()
            && !b[i].is_null()
            && compare_datums(&a[i], &b[i])
                .map(|o| o.is_eq())
                .unwrap_or(false)
    };
    for (i, c) in new_def.columns().iter().enumerate() {
        if c.unique && eq(i) {
            let mut name = StackStr::<128>::new();
            if c.primary {
                let _ = write!(name, "{}_pkey", new_def.name.as_str());
            } else {
                let _ = write!(name, "{}_{}_key", new_def.name.as_str(), c.name.as_str());
            }
            return Some(name);
        }
    }
    for uk in new_def.uniques() {
        if uk.columns().iter().all(|&col| eq(col as usize)) {
            let mut name = StackStr::<128>::new();
            let _ = write!(name, "{}", uk.name.as_str());
            return Some(name);
        }
    }
    None
}

/// How each column of a rewritten row is produced from the old row, composed
/// across every subcommand of one ALTER TABLE so a single rewrite pass applies
/// all of them.
#[derive(Clone, Copy)]
enum ColSource<'a> {
    /// Copy the old row's column at this original index unchanged.
    Keep(usize),
    /// Cast the old row's column (or a USING expression over the old row) to a
    /// new type; `orig` is the source column's original index.
    Cast {
        orig: usize,
        target: ColType,
        type_mod: i32,
        using: Option<&'a Expr<'a>>,
    },
    /// A column added by this statement; its value is the new column's default
    /// (or NULL). The index is into the *new* definition.
    FillDefault(usize),
}

#[allow(clippy::too_many_arguments)]
pub fn alter_table(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    scratch: &mut FixedVec<(u64, RowHome)>,
    statement: &AlterTable,
    arena: &Arena,
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
) -> Outcome {
    alter_table_inner(
        storage,
        wal,
        txn,
        scratch,
        statement,
        arena,
        seq_session,
        responder,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn alter_table_inner(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    scratch: &mut FixedVec<(u64, RowHome)>,
    statement: &AlterTable,
    arena: &Arena,
    seq_session: &crate::sql::guc::SeqSession,
    responder: &mut Responder,
    emit_completion: bool,
) -> Outcome {
    let table_index =
        match storage.resolve_relation(statement.table.schema, statement.table.name, txn.txid) {
            Some(crate::storage::ResolvedRelation::Table(i)) => i,
            None if statement.if_exists => {
                responder.notice(
                    sqlstate::SUCCESSFUL_COMPLETION,
                    stack_format!(
                        160,
                        "relation \"{}\" does not exist, skipping",
                        statement.table.name
                    )
                    .as_str(),
                )?;
                if emit_completion {
                    responder.command_complete("ALTER TABLE")?;
                }
                return sql_ok();
            }
            _ => return sql_fail(undefined_qual(&statement.table)),
        };
    if let Err(error) = storage.lock_table(
        txn.txid,
        table_index,
        crate::sql::ast::TableLockMode::AccessExclusive,
        false,
    ) {
        return sql_fail(error);
    }
    let def = *storage.table_def(table_index, txn.txid);
    if emit_completion
        && let Err(error) = storage.require_owner(
            storage.table_access_object(table_index, txn.txid),
            txn.txid,
            "table",
        )
    {
        return sql_fail(error);
    }

    // The ACCESS EXCLUSIVE acquisition above waits for ordinary readers and
    // writers. This verifies the row-version invariant at the rewrite
    // boundary as a corruption guard.
    if storage
        .table(table_index)
        .rows
        .iter()
        .any(|(_, state)| state.locked_by_other(txn.txid).is_some())
    {
        return sql_fail(sql_err!(
            crate::sql::eval::sqlstate::LOCK_NOT_AVAILABLE,
            "table \"{}\" has uncommitted changes; retry when idle",
            statement.table.name
        ));
    }

    // SET SCHEMA is a definition-only move with its own journal record — no
    // row images change, and inbound foreign keys follow the table. It is a
    // standalone form (never combined), so it is the whole action list.
    if let [AlterAction::SetSchema(new_schema)] = statement.actions {
        let new_schema = *new_schema;
        let Some(_) = storage.find_schema_visible(new_schema, txn.txid) else {
            return sql_fail(sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                "schema \"{}\" does not exist",
                new_schema
            ));
        };
        if new_schema == def.schema.as_str() {
            // Already there: PostgreSQL treats this as a no-op success.
            if emit_completion {
                responder.command_complete("ALTER TABLE")?;
            }
            return sql_ok();
        }
        if let Err(error) = storage.require_schema_create(new_schema, txn.txid) {
            return sql_fail(error);
        }
        if storage.relation_name_taken(new_schema, def.name.as_str(), txn.txid) {
            return sql_fail(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "relation \"{}\" already exists in schema \"{}\"",
                def.name.as_str(),
                new_schema
            ));
        }
        for sequence_slot in 0..storage.sequence_count() {
            let sequence = storage.sequence_for(sequence_slot, txn.txid);
            if !matches!(
                sequence.owner,
                Some(owner) if owner.table_schema == def.schema && owner.table == def.name
            ) {
                continue;
            }
            if storage.relation_name_taken(new_schema, sequence.name.as_str(), txn.txid) {
                return sql_fail(sql_err!(
                    sqlstate::DUPLICATE_TABLE,
                    "relation \"{}\" already exists in schema \"{}\"",
                    sequence.name.as_str(),
                    new_schema
                ));
            }
        }
        let new_name = match SqlName::parse(new_schema) {
            Ok(n) => n,
            Err(e) => return sql_fail(e),
        };
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.stage(
            txn.txid,
            lsn,
            &WalOp::SetTableSchema {
                schema: def.schema.as_str(),
                name: def.name.as_str(),
                new_schema,
            },
        ) {
            return sql_fail(e);
        }
        let mut new_def = def;
        new_def.schema = new_name;
        let mut mapping = [None; MAX_COLUMNS];
        for (column, target) in mapping.iter_mut().enumerate().take(def.n_columns) {
            *target = Some(def.columns()[column].name);
        }
        if let Err(error) = storage.write_table_def(table_index, txn.txid, new_def, &mapping, false)
        {
            return sql_fail(error);
        }
        if let Err(error) = txn.record_ddl(super::txn::DdlUndo::TableAltered(table_index as u32)) {
            storage.rollback_table_def(table_index, txn.txid);
            return sql_fail(error);
        }
        if emit_completion {
            responder.command_complete("ALTER TABLE")?;
        }
        return sql_ok();
    }

    // Collect every committed row up front: the row count decides whether an
    // added NOT NULL column needs a fill (a spilled table has rows even when
    // the overlay map has evicted them, so `rows.is_empty()` cannot answer
    // this), and the same list drives the rewrite below.
    scratch.clear();
    {
        let mut overflow = false;
        if let Err(error) = storage.for_each_row_state(table_index, &mut |rowid, state| {
            use core::ops::ControlFlow;
            let Some(loc) = storage.visible_row_home_at(
                table_index,
                rowid,
                state,
                txn.txid,
                crate::storage::SNAPSHOT_ALL,
                storage.commit_snapshot(),
            )?
            else {
                return Ok(ControlFlow::Continue(()));
            };
            if scratch.push((rowid, loc)).is_err() {
                overflow = true;
                return Ok(ControlFlow::Break(()));
            }
            Ok(ControlFlow::Continue(()))
        }) {
            return sql_fail(error);
        }
        if overflow {
            return sql_fail(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "ALTER touches more than {} rows",
                scratch.capacity()
            ));
        }
    }
    let has_rows = !scratch.is_empty();

    // Build the new definition and the composed per-column rewrite source.
    // Names resolve against the running definition, so an ADD CONSTRAINT can
    // reference a column ADDed earlier in the pass-ordered list.
    let mut new_def = def;
    let mut source = [ColSource::Keep(0usize); MAX_COLUMNS];
    for (i, s) in source.iter_mut().enumerate().take(def.n_columns) {
        *s = ColSource::Keep(i);
    }
    let mut added_any = false;
    let mut dropped_any = false;
    let mut retyped_any = false;
    let mut has_added_unique = false;
    let mut identity_sequences: [Option<OwnedSequencePlan>; MAX_COLUMNS] = [None; MAX_COLUMNS];
    let mut n_identity_sequences = 0usize;
    let mut owned_sequences_to_drop = [usize::MAX; crate::storage::MAX_SEQUENCES];
    let mut n_owned_sequences_to_drop = 0usize;

    for action in statement.actions {
        match action {
            AlterAction::SetSchema(_) => unreachable!("SET SCHEMA is a standalone action"),
            AlterAction::RenameTable(new_name) => {
                if storage.relation_name_taken(def.schema.as_str(), new_name, txn.txid) {
                    return sql_fail(sql_err!(
                        sqlstate::DUPLICATE_TABLE,
                        "relation \"{}\" already exists",
                        new_name
                    ));
                }
                new_def.name = match SqlName::parse(new_name) {
                    Ok(n) => n,
                    Err(e) => return sql_fail(e),
                };
            }
            AlterAction::RenameColumn { from, to } => {
                let Some(i) = new_def.column_index(from) else {
                    return sql_fail(undefined_column(from));
                };
                if new_def.column_index(to).is_some() {
                    return sql_fail(sql_err!(
                        sqlstate::DUPLICATE_COLUMN,
                        "column \"{}\" already exists",
                        to
                    ));
                }
                new_def.columns[i].name = match SqlName::parse(to) {
                    Ok(n) => n,
                    Err(e) => return sql_fail(e),
                };
            }
            AlterAction::AddColumn(c) => {
                if new_def.column_index(c.name).is_some() {
                    return sql_fail(sql_err!(
                        sqlstate::DUPLICATE_COLUMN,
                        "column \"{}\" already exists",
                        c.name
                    ));
                }
                if new_def.n_columns == MAX_COLUMNS {
                    return sql_fail(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "tables can have at most {} columns",
                        MAX_COLUMNS
                    ));
                }
                let meta = match build_column(c, &*storage, txn.txid, arena) {
                    Ok(m) => m,
                    Err(e) => return sql_fail(e),
                };
                // NOT NULL without a default over a non-empty table is a
                // constraint violation, as in PostgreSQL.
                if matches!(meta.default, crate::storage::ColumnDefault::None)
                    && meta.not_null
                    && has_rows
                {
                    return sql_fail(sql_err!(
                        sqlstate::NOT_NULL_VIOLATION,
                        "column \"{}\" of relation \"{}\" contains null values",
                        c.name,
                        statement.table.name
                    ));
                }
                let index = new_def.n_columns;
                new_def.columns[index] = meta;
                new_def.n_columns += 1;
                source[index] = ColSource::FillDefault(index);
                added_any = true;
            }
            AlterAction::DropColumn(name) => {
                let Some(i) = new_def.column_index(name) else {
                    return sql_fail(undefined_column(name));
                };
                let original_column = match source[i] {
                    ColSource::Keep(original) | ColSource::Cast { orig: original, .. } => {
                        def.columns()[original].name
                    }
                    ColSource::FillDefault(_) => new_def.columns()[i].name,
                };
                for slot in 0..storage.sequence_count() {
                    if matches!(
                        storage.sequence_for(slot, txn.txid).owner,
                        Some(owner)
                            if owner.table_schema == def.schema
                                && owner.table == def.name
                                && owner.column == original_column
                    ) && !owned_sequences_to_drop[..n_owned_sequences_to_drop].contains(&slot)
                    {
                        owned_sequences_to_drop[n_owned_sequences_to_drop] = slot;
                        n_owned_sequences_to_drop += 1;
                    }
                }
                for j in i..new_def.n_columns - 1 {
                    new_def.columns[j] = new_def.columns[j + 1];
                    source[j] = source[j + 1];
                }
                new_def.n_columns -= 1;
                dropped_any = true;
            }
            AlterAction::SetDefault {
                column,
                value,
                value_text,
            } => {
                let Some(i) = new_def.column_index(column) else {
                    return sql_fail(undefined_column(column));
                };
                let ctype = new_def.columns[i].ctype;
                let type_mod = new_def.columns[i].type_mod;
                // A literal-only default folds to a constant; a call-bearing one
                // is stored as text and evaluated per row — CREATE TABLE's path.
                let default = match ddl::resolve_default(
                    Some(value),
                    Some(value_text),
                    ctype,
                    type_mod,
                    storage,
                    txn.txid,
                    arena,
                ) {
                    Ok(d) => d,
                    Err(e) => return sql_fail(e),
                };
                new_def.columns[i].default = default;
            }
            AlterAction::DropDefault { column } => {
                let Some(i) = new_def.column_index(column) else {
                    return sql_fail(undefined_column(column));
                };
                new_def.columns[i].default = crate::storage::ColumnDefault::NONE;
                // Dropping a serial column's default detaches its auto-increment.
                new_def.columns[i].auto_increment = false;
            }
            AlterAction::AddIdentity { column, spec } => {
                let Some(i) = new_def.column_index(column) else {
                    return sql_fail(undefined_column(column));
                };
                let col = &new_def.columns[i];
                if col.is_identity {
                    return sql_fail(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "column \"{}\" of relation \"{}\" is already an identity column",
                        column,
                        statement.table.name
                    ));
                }
                if !col.not_null {
                    return sql_fail(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "column \"{}\" of relation \"{}\" must be declared NOT NULL before identity can be added",
                        column,
                        statement.table.name
                    ));
                }
                let step = spec.options.increment.unwrap_or(1);
                new_def.columns[i].is_identity = true;
                new_def.columns[i].identity_always = spec.always;
                new_def.columns[i].auto_increment = true;
                new_def.columns[i].auto_increment_step = step;
                let plan = match owned_sequence_plan(&new_def, i, Some(*spec)) {
                    Ok(plan) => plan,
                    Err(error) => return sql_fail(error),
                };
                if storage.relation_name_taken(plan.schema.as_str(), plan.name.as_str(), txn.txid)
                    || identity_sequences[..n_identity_sequences]
                        .iter()
                        .flatten()
                        .any(|other| other.schema == plan.schema && other.name == plan.name)
                {
                    return sql_fail(sql_err!(
                        sqlstate::DUPLICATE_TABLE,
                        "relation \"{}\" already exists",
                        plan.name.as_str()
                    ));
                }
                identity_sequences[n_identity_sequences] = Some(plan);
                n_identity_sequences += 1;
            }
            AlterAction::DropIdentity { column, if_exists } => {
                let Some(i) = new_def.column_index(column) else {
                    return sql_fail(undefined_column(column));
                };
                if !new_def.columns[i].is_identity {
                    if *if_exists {
                        responder.notice(
                            crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                            stack_format!(
                                160,
                                "column \"{}\" of relation \"{}\" is not an identity column, skipping",
                                column,
                                statement.table.name
                            )
                            .as_str(),
                        )?;
                        continue;
                    }
                    return sql_fail(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "column \"{}\" of relation \"{}\" is not an identity column",
                        column,
                        statement.table.name
                    ));
                }
                let original_column = match source[i] {
                    ColSource::Keep(original) | ColSource::Cast { orig: original, .. } => {
                        def.columns()[original].name
                    }
                    ColSource::FillDefault(_) => new_def.columns()[i].name,
                };
                if let Some(slot) = storage.generated_sequence_slot(
                    def.schema.as_str(),
                    def.name.as_str(),
                    original_column.as_str(),
                    txn.txid,
                ) && matches!(
                    storage.sequence_for(slot, txn.txid).owner,
                    Some(owner)
                        if owner.table_schema == def.schema
                            && owner.table == def.name
                            && owner.column == original_column
                ) && !owned_sequences_to_drop[..n_owned_sequences_to_drop].contains(&slot)
                {
                    owned_sequences_to_drop[n_owned_sequences_to_drop] = slot;
                    n_owned_sequences_to_drop += 1;
                }
                new_def.columns[i].is_identity = false;
                new_def.columns[i].identity_always = false;
                new_def.columns[i].auto_increment = false;
                new_def.columns[i].auto_increment_step = 1;
            }
            AlterAction::SetNotNull { column } => {
                let Some(i) = new_def.column_index(column) else {
                    return sql_fail(undefined_column(column));
                };
                // A NULL is caught by the content validation below, against the
                // rewritten image so it composes with a type change in the same
                // statement.
                new_def.columns[i].not_null = true;
            }
            AlterAction::DropNotNull { column } => {
                let Some(i) = new_def.column_index(column) else {
                    return sql_fail(undefined_column(column));
                };
                // A primary key implies NOT NULL, whether it rides a column flag
                // or an explicitly named single-/multi-column key.
                let in_primary_key = new_def.columns[i].primary
                    || new_def.uniques[..new_def.n_uniques]
                        .iter()
                        .any(|k| k.is_primary && k.columns().contains(&(i as u16)));
                if in_primary_key {
                    return sql_fail(sql_err!(
                        sqlstate::INVALID_TABLE_DEFINITION,
                        "column \"{}\" is in a primary key",
                        column
                    ));
                }
                new_def.columns[i].not_null = false;
            }
            AlterAction::AlterColumnType {
                column,
                type_name,
                type_mod,
                using,
            } => {
                let Some(i) = new_def.column_index(column) else {
                    return sql_fail(undefined_column(column));
                };
                let Some(target) = ColType::from_sql_name(type_name) else {
                    return sql_fail(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "type \"{}\" does not exist",
                        type_name
                    ));
                };
                // Without USING, the stored value casts through the assignment
                // cast; a cast that is explicit-only (e.g. text→int) is refused
                // with PostgreSQL's 42804, telling the user to add USING.
                if using.is_none() && !alter_type_auto_castable(new_def.columns[i].ctype, target) {
                    return sql_fail(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "column \"{}\" cannot be cast automatically to type {}",
                        column,
                        target.name()
                    ));
                }
                // Compose with whatever already produces this column: a kept or
                // cast original column becomes a (re)cast of its original index;
                // a column added in this same statement re-casts its default.
                match source[i] {
                    ColSource::Keep(orig) | ColSource::Cast { orig, .. } => {
                        source[i] = ColSource::Cast {
                            orig,
                            target,
                            type_mod: *type_mod,
                            using: *using,
                        };
                        retyped_any = true;
                    }
                    ColSource::FillDefault(fi) => {
                        if let Some(od) = new_def.columns[fi].default.constant().copied() {
                            match cast_to(od.as_datum(), target, arena)
                                .and_then(|v| apply_typmod(v, target, *type_mod, arena))
                                .and_then(|v| crate::storage::OwnedDatum::from_datum(&v))
                            {
                                Ok(value) => {
                                    let expression = new_def.columns[fi]
                                        .default
                                        .expression()
                                        .copied()
                                        .expect("constant default has source");
                                    new_def.columns[fi].default =
                                        crate::storage::ColumnDefault::Constant {
                                            value,
                                            expression,
                                        };
                                }
                                Err(e) => return sql_fail(e),
                            }
                        }
                    }
                }
                new_def.columns[i].ctype = target;
                new_def.columns[i].type_mod = *type_mod;
            }
            AlterAction::AddConstraint(constraint) => {
                // Build the constraint into the new definition. CHECK/NOT NULL/FK
                // are validated per rewritten image below; an added uniqueness
                // constraint is validated across the rewritten images before
                // anything is journaled.
                if let Err(e) = crate::sql::exec::ddl::attach_constraints(
                    storage,
                    &mut new_def,
                    core::slice::from_ref(constraint),
                    txn.txid,
                    arena,
                ) {
                    return sql_fail(e);
                }
                if matches!(
                    constraint,
                    crate::sql::ast::TableConstraint::PrimaryKey { .. }
                        | crate::sql::ast::TableConstraint::Unique { .. }
                ) {
                    has_added_unique = true;
                }
            }
            AlterAction::DropConstraint { name, if_exists } => {
                if !drop_named_constraint(&mut new_def, name) {
                    if !*if_exists {
                        return sql_fail(sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "constraint \"{}\" of relation \"{}\" does not exist",
                            name,
                            def.name.as_str()
                        ));
                    }
                    // IF EXISTS: PostgreSQL emits a skip notice (SQLSTATE 00000).
                    responder.notice(
                        crate::sql::eval::sqlstate::SUCCESSFUL_COMPLETION,
                        stack_format!(
                            160,
                            "constraint \"{}\" of relation \"{}\" does not exist, skipping",
                            name,
                            def.name.as_str()
                        )
                        .as_str(),
                    )?;
                }
            }
            AlterAction::RenameConstraint { from, to } => {
                // The new name must be free among this table's constraints.
                let taken = new_def.checks[..new_def.n_checks]
                    .iter()
                    .any(|c| c.name.as_str() == *to)
                    || new_def.uniques[..new_def.n_uniques]
                        .iter()
                        .any(|k| k.name.as_str() == *to)
                    || new_def.fkeys[..new_def.n_fkeys]
                        .iter()
                        .any(|f| f.name.as_str() == *to);
                if taken {
                    return sql_fail(sql_err!(
                        sqlstate::DUPLICATE_OBJECT,
                        "constraint \"{}\" for relation \"{}\" already exists",
                        to,
                        def.name.as_str()
                    ));
                }
                let new_name = match SqlName::parse(to) {
                    Ok(n) => n,
                    Err(e) => return sql_fail(e),
                };
                let renamed = new_def.checks[..new_def.n_checks]
                    .iter_mut()
                    .find(|c| c.name.as_str() == *from)
                    .map(|c| c.name = new_name)
                    .or_else(|| {
                        new_def.uniques[..new_def.n_uniques]
                            .iter_mut()
                            .find(|k| k.name.as_str() == *from)
                            .map(|k| k.name = new_name)
                    })
                    .or_else(|| {
                        new_def.fkeys[..new_def.n_fkeys]
                            .iter_mut()
                            .find(|f| f.name.as_str() == *from)
                            .map(|f| f.name = new_name)
                    })
                    .is_some();
                // A single-column key on a column flag has a synthesized name;
                // renaming it materializes the flag into a named key.
                if !renamed {
                    match rename_flag_key(&mut new_def, from, new_name) {
                        Ok(true) => {}
                        Ok(false) => {
                            return sql_fail(sql_err!(
                                sqlstate::UNDEFINED_OBJECT,
                                "constraint \"{}\" for table \"{}\" does not exist",
                                from,
                                def.name.as_str()
                            ));
                        }
                        Err(e) => return sql_fail(e),
                    }
                }
            }
        }
    }

    let relation_moved = def.schema != new_def.schema || def.name != new_def.name;
    if relation_moved {
        for foreign_key in &mut new_def.fkeys[..new_def.n_fkeys] {
            if foreign_key.parent_schema == def.schema && foreign_key.parent == def.name {
                foreign_key.parent_schema = new_def.schema;
                foreign_key.parent = new_def.name;
            }
        }
    }

    let has_rewrite = added_any || dropped_any || retyped_any;

    let mut old_schema = [ColType::Bool; MAX_COLUMNS];
    def.schema(&mut old_schema);
    let old_schema = &old_schema[..def.n_columns];

    let mut new_schema = [ColType::Bool; MAX_COLUMNS];
    new_def.schema(&mut new_schema);
    let new_schema = &new_schema[..new_def.n_columns];

    // When no row image changes (rename / constraint / default / drop-not-null /
    // set-not-null over an unchanged column), the new definition — including its
    // uniqueness — is validated against the stored rows, before anything is
    // journaled. A rewrite validates each transformed image instead, below.
    if !has_rewrite
        && let Err(e) = validate_all_rows(storage, table_index, &new_def, txn.txid, arena)
    {
        return sql_fail(e);
    }

    // Build every rewritten image and validate its content
    // (NOT NULL / CHECK / foreign keys), all before any journaling, so a
    // failure — a cast error included — leaves the table untouched.
    let checks = match parse_checks(&new_def, arena) {
        Ok(c) => c,
        Err(e) => return sql_fail(e),
    };
    // Non-constant DEFAULTs of columns added by this ALTER: each existing row is
    // backfilled by evaluating the default, so a `nextval` default consumes a
    // value per existing row, exactly as PostgreSQL rewrites the table.
    let new_defaults = match parse_defaults(&new_def, arena) {
        Ok(d) => d,
        Err(e) => return sql_fail(e),
    };
    // Generated columns are recomputed for every rewritten row — an ADD COLUMN
    // of a generated column backfills, and a type change of a dependency reflows.
    let new_generated = match parse_generated(&new_def, arena) {
        Ok(g) => g,
        Err(e) => return sql_fail(e),
    };
    for i in 0..scratch.len() {
        let (rowid, old_home) = scratch[i];
        let new_loc = if has_rewrite {
            // Build the new image in the statement arena so the heap borrow
            // (decoded text refs) ends before the heap append.
            let new_bytes: &[u8] = {
                let old_bytes = match storage.row_bytes(table_index, rowid, old_home, arena) {
                    Ok(b) => b,
                    Err(e) => return sql_fail(e),
                };
                let mut old_values = [Datum::Null; MAX_COLUMNS];
                if let Err(e) = rowenc::decode(old_bytes, old_schema, &mut old_values) {
                    return sql_fail(e);
                }
                let mut out = [Datum::Null; MAX_COLUMNS];
                for c in 0..new_def.n_columns {
                    out[c] = match source[c] {
                        ColSource::Keep(orig) => old_values[orig],
                        ColSource::Cast {
                            orig,
                            target,
                            type_mod,
                            using,
                        } => {
                            // USING is evaluated with the old row's columns in
                            // scope; otherwise the old value is the cast source.
                            let cast_source = match using {
                                Some(expr) => {
                                    let ctx = RowCtx {
                                        def: &def,
                                        values: &old_values[..def.n_columns],
                                        alias: None,
                                    };
                                    match eval(expr, arena, crate::sql::eval::NO_PARAMS, &ctx) {
                                        Ok(v) => v,
                                        Err(e) => return sql_fail(e),
                                    }
                                }
                                None => old_values[orig],
                            };
                            match cast_to(cast_source, target, arena)
                                .and_then(|v| apply_typmod(v, target, type_mod, arena))
                            {
                                Ok(v) => v,
                                Err(e) => return sql_fail(e),
                            }
                        }
                        ColSource::FillDefault(fi) => {
                            if let Some(d) = new_def.columns[fi].default.constant() {
                                d.as_datum()
                            } else if let Some(expr) = new_defaults[fi] {
                                // Evaluate the non-constant default for this row
                                // (advancing a `nextval` default once per row),
                                // scoped so the borrow ends before the append.
                                let seq = crate::sql::sequence::SeqEval::new(
                                    storage,
                                    seq_session,
                                    txn.txid,
                                );
                                let catalog =
                                    super::query::storage_catalog(storage, arena, txn.txid);
                                let hooks = super::eval::EvalHooks {
                                    catalog: Some(&catalog),
                                    sequences: Some(&seq),
                                    ..super::eval::NO_HOOKS
                                };
                                let v = match super::eval::eval_full(
                                    expr,
                                    arena,
                                    crate::sql::eval::NO_PARAMS,
                                    &NoColumns,
                                    &hooks,
                                ) {
                                    Ok(v) => v,
                                    Err(e) => return sql_fail(e),
                                };
                                match coerce(v, &new_def.columns[fi], storage, txn.txid, arena) {
                                    Ok(v) => v,
                                    Err(e) => return sql_fail(e),
                                }
                            } else {
                                Datum::Null
                            }
                        }
                    };
                }
                if let Err(e) =
                    compute_generated(&new_def, &new_generated, &mut out, storage, txn.txid, arena)
                {
                    return sql_fail(e);
                }
                let values = &out[..new_def.n_columns];
                if let Err(e) = crate::sql::exec::constraints::check_row_content(
                    storage,
                    &new_def,
                    values,
                    &checks,
                    arena,
                    &[],
                    txn.txid,
                ) {
                    return sql_fail(e);
                }
                let len = rowenc::encoded_len(values);
                let buffer = match arena.alloc_slice_with(len, |_| 0u8) {
                    Ok(b) => b,
                    Err(_) => {
                        return sql_fail(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "ALTER rewrite exceeds the statement arena"
                        ));
                    }
                };
                rowenc::encode(values, buffer);
                &*buffer
            };
            let (loc, slice) = match storage.heap.append(new_bytes.len()) {
                Ok(x) => x,
                Err(e) => return sql_fail(e),
            };
            slice.copy_from_slice(new_bytes);
            loc
        } else {
            match old_home {
                RowHome::Heap(loc) => loc,
                RowHome::Spilled { .. } => {
                    // The ALTER journals a full re-upsert of every row, so a
                    // spilled row's bytes come back into the heap here; the
                    // next checkpoint spills them again.
                    let bytes = match storage.row_bytes(table_index, rowid, old_home, arena) {
                        Ok(b) => b,
                        Err(e) => return sql_fail(e),
                    };
                    let copied = match arena.alloc_slice_copy(bytes) {
                        Ok(c) => c,
                        Err(_) => {
                            return sql_fail(sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "ALTER rewrite exceeds the statement arena"
                            ));
                        }
                    };
                    let (loc, slice) = match storage.heap.append(copied.len()) {
                        Ok(x) => x,
                        Err(e) => return sql_fail(e),
                    };
                    slice.copy_from_slice(copied);
                    loc
                }
            }
        };
        scratch[i] = (rowid, RowHome::Heap(new_loc));
    }

    // A uniqueness constraint added alongside a rewrite is validated across the
    // transformed images now in the heap, before journaling — a cross-row check
    // that cannot run against the stored (still old) rows. NULLs are distinct.
    if has_rewrite && has_added_unique {
        for a in 0..scratch.len() {
            let RowHome::Heap(la) = scratch[a].1 else {
                unreachable!()
            };
            let abytes = storage.heap.get(la);
            let mut avals = [Datum::Null; MAX_COLUMNS];
            if let Err(e) = rowenc::decode(abytes, new_schema, &mut avals) {
                return sql_fail(e);
            }
            for b in (a + 1)..scratch.len() {
                let RowHome::Heap(lb) = scratch[b].1 else {
                    unreachable!()
                };
                let bbytes = storage.heap.get(lb);
                let mut bvals = [Datum::Null; MAX_COLUMNS];
                if let Err(e) = rowenc::decode(bbytes, new_schema, &mut bvals) {
                    return sql_fail(e);
                }
                if let Some(name) = rewritten_dup_name(
                    &new_def,
                    &avals[..new_def.n_columns],
                    &bvals[..new_def.n_columns],
                ) {
                    return sql_fail(sql_err!(
                        sqlstate::UNIQUE_VIOLATION,
                        "duplicate key value violates unique constraint \"{}\"",
                        name.as_str()
                    ));
                }
            }
        }
    }

    let mut column_mapping = [None; MAX_COLUMNS];
    let mut wal_column_mapping = [u16::MAX; MAX_COLUMNS];
    for (new_column, source) in source[..new_def.n_columns].iter().enumerate() {
        if let ColSource::Keep(old_column)
        | ColSource::Cast {
            orig: old_column, ..
        } = *source
        {
            column_mapping[old_column] = Some(new_def.columns()[new_column].name);
            wal_column_mapping[old_column] = new_column as u16;
        }
    }

    // Journal the in-place shape change and the re-homed rows. Every fallible
    // content step is already done; only WAL append can fail here, and it does
    // so before any in-memory swap.
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.stage(
        txn.txid,
        lsn,
        &WalOp::BeginTableRewrite {
            previous_schema: def.schema.as_str(),
            previous_name: def.name.as_str(),
            column_mapping: wal_column_mapping,
        },
    ) {
        return sql_fail(e);
    }
    let lsn = storage.bump_lsn();
    if let Err(e) = wal.stage(txn.txid, lsn, &WalOp::CreateTable(new_def)) {
        return sql_fail(e);
    }
    for i in 0..scratch.len() {
        let (rowid, new_home) = scratch[i];
        let RowHome::Heap(new_loc) = new_home else {
            unreachable!("the rewrite pass re-homes every row to the heap");
        };
        let lsn = storage.bump_lsn();
        if let Err(e) = wal.stage(
            txn.txid,
            lsn,
            &WalOp::Upsert {
                schema: new_def.schema.as_str(),
                table: new_def.name.as_str(),
                rowid,
                row: storage.heap.get(new_loc),
                is_update: false,
                old_row: None,
                command_id: txn.command_id(),
            },
        ) {
            return sql_fail(e);
        }
    }

    for plan in identity_sequences[..n_identity_sequences].iter().flatten() {
        let sequence_slot = match create_owned_sequence(storage, wal, *plan, txn.txid) {
            Ok(sequence_slot) => sequence_slot,
            Err(error) => return sql_fail(error),
        };
        if let Err(error) =
            txn.record_ddl(super::txn::DdlUndo::SequenceCreated(sequence_slot as u32))
        {
            storage.rollback_sequence_create(sequence_slot);
            return sql_fail(error);
        }
        if let Err(error) = apply_default_privileges_to_new_object(
            storage,
            txn,
            crate::storage::AccessObject {
                class: crate::storage::AccessClass::Sequence,
                slot: sequence_slot as u16,
            },
        ) {
            return sql_fail(error);
        }
    }
    for &sequence_slot in &owned_sequences_to_drop[..n_owned_sequences_to_drop] {
        let sequence = storage.sequence_for(sequence_slot, txn.txid);
        let (sequence_schema, sequence_name) = (sequence.schema, sequence.name);
        let lsn = storage.bump_lsn();
        if let Err(error) = wal.stage(
            txn.txid,
            lsn,
            &WalOp::DropSequence {
                schema: sequence_schema.as_str(),
                name: sequence_name.as_str(),
            },
        ) {
            return sql_fail(error);
        }
        match storage.drop_sequence(sequence_schema.as_str(), sequence_name.as_str(), txn.txid) {
            Ok(Some(slot)) => {
                if let Err(error) =
                    txn.record_ddl(super::txn::DdlUndo::SequenceDropped(slot as u32))
                {
                    storage.rollback_sequence_drop(slot, txn.txid);
                    return sql_fail(error);
                }
            }
            Ok(None) => {}
            Err(error) => return sql_fail(error),
        }
    }
    let rebind = |link: Option<crate::storage::SequenceOwner>, require_generator: bool| {
        let mut link = link?;
        if link.table_schema != def.schema || link.table != def.name {
            return Some(link);
        }
        // A sequence created for a column added by this same ALTER already
        // carries the final names and has no source-column mapping.
        let Some(old_column) = def.column_index(link.column.as_str()) else {
            return Some(link);
        };
        let target_name = column_mapping[old_column]?;
        let target_column = new_def.column_index(target_name.as_str())?;
        if require_generator && !new_def.columns()[target_column].auto_increment {
            return None;
        }
        link.table_schema = new_def.schema;
        link.table = new_def.name;
        link.column = new_def.columns()[target_column].name;
        Some(link)
    };
    // The table shape WAL record carries no sequence dependencies. Re-journal
    // every changed ownership/generator edge absolutely so replay observes the
    // same rename, dropped column, or dropped identity.
    for sequence_slot in 0..storage.sequence_count() {
        let sequence = storage.sequence_for(sequence_slot, txn.txid);
        if !sequence.visible_to(txn.txid) {
            continue;
        }
        let owner = rebind(sequence.owner, false);
        let generator_for = rebind(sequence.generator_for, true);
        if owner == sequence.owner && generator_for == sequence.generator_for {
            continue;
        }
        let spec = SeqSpec {
            data_type: sequence.data_type,
            increment: sequence.increment,
            min_value: sequence.min_value,
            max_value: sequence.max_value,
            start_value: sequence.start_value,
            cache: sequence.cache,
            cycle: sequence.cycle,
        };
        let (sequence_schema, sequence_name) = (sequence.schema, sequence.name);
        let lsn = storage.bump_lsn();
        if let Err(error) = wal.stage(
            txn.txid,
            lsn,
            &WalOp::CreateSequence {
                schema: sequence_schema.as_str(),
                name: sequence_name.as_str(),
                data_type: spec.data_type.to_u8(),
                increment: spec.increment,
                min_value: spec.min_value,
                max_value: spec.max_value,
                start_value: spec.start_value,
                cache: spec.cache,
                cycle: spec.cycle,
                owner,
                generator_for,
            },
        ) {
            return sql_fail(error);
        }
    }

    if let Err(error) =
        storage.write_table_def(table_index, txn.txid, new_def, &column_mapping, true)
    {
        return sql_fail(error);
    }
    if let Err(error) = txn.record_ddl(super::txn::DdlUndo::TableAltered(table_index as u32)) {
        storage.rollback_table_def(table_index, txn.txid);
        return sql_fail(error);
    }
    if relation_moved {
        for dependent_table in 0..storage.table_count() {
            if dependent_table == table_index
                || !storage.table(dependent_table).visible_to(txn.txid)
            {
                continue;
            }
            let current = *storage.table_def(dependent_table, txn.txid);
            let mut dependent = current;
            let mut changed = false;
            for foreign_key in &mut dependent.fkeys[..dependent.n_fkeys] {
                if foreign_key.parent_schema == def.schema && foreign_key.parent == def.name {
                    foreign_key.parent_schema = new_def.schema;
                    foreign_key.parent = new_def.name;
                    changed = true;
                }
            }
            if !changed {
                continue;
            }
            let mut identity_mapping = [None; MAX_COLUMNS];
            for (column, target) in identity_mapping
                .iter_mut()
                .enumerate()
                .take(current.n_columns)
            {
                *target = Some(current.columns()[column].name);
            }
            if let Err(error) = storage.write_table_def(
                dependent_table,
                txn.txid,
                dependent,
                &identity_mapping,
                false,
            ) {
                return sql_fail(error);
            }
            if let Err(error) =
                txn.record_ddl(super::txn::DdlUndo::TableAltered(dependent_table as u32))
            {
                storage.rollback_table_def(dependent_table, txn.txid);
                return sql_fail(error);
            }
        }
    }
    for i in 0..scratch.len() {
        let (rowid, new_home) = scratch[i];
        let RowHome::Heap(new_loc) = new_home else {
            unreachable!("the rewrite pass re-homes every row to the heap");
        };
        match storage.write_pending(
            table_index,
            rowid,
            txn.txid,
            txn.command_id(),
            Some(new_loc),
        ) {
            Ok(prior) => {
                if let Err(error) = txn.touch(table_index as u32, rowid, prior) {
                    storage.restore_pending(table_index, rowid, txn.txid, prior);
                    return sql_fail(error);
                }
            }
            Err(error) => return sql_fail(error),
        }
    }
    if emit_completion {
        responder.command_complete("ALTER TABLE")?;
    }
    sql_ok()
}

fn undefined_column(name: &str) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_COLUMN,
        "column \"{}\" does not exist",
        name
    )
}

/// Renames a single-column UNIQUE/PRIMARY KEY that rides a column flag: when
/// `from` is the flag's synthesized name (`<table>_pkey` / `<table>_<col>_key`),
/// clears the flag and adds a named key with `new_name` so the constraint keeps
/// its new name. Returns whether a flag key matched.
fn rename_flag_key(def: &mut TableDef, from: &str, new_name: SqlName) -> Result<bool, SqlError> {
    for i in 0..def.n_columns {
        let col = def.columns[i];
        if !(col.unique || col.primary) {
            continue;
        }
        let synthesized = if col.primary {
            stack_format!(128, "{}_pkey", def.name.as_str())
        } else {
            stack_format!(128, "{}_{}_key", def.name.as_str(), col.name.as_str())
        };
        if synthesized.as_str() == from {
            let was_primary = col.primary;
            def.columns[i].unique = false;
            def.columns[i].primary = false;
            let mut indices = [0u16; crate::storage::MAX_INDEX_COLS];
            indices[0] = i as u16;
            crate::sql::exec::ddl::add_unique_key(
                def,
                Some(new_name.as_str()),
                if was_primary { "pkey" } else { "key" },
                &indices,
                1,
                was_primary,
            )?;
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn eval_offset_pub(
    offset: Option<&Expr>,
    arena: &Arena,
    params: &[Datum],
) -> Result<u64, SqlError> {
    let Some(expression) = offset else {
        return Ok(0);
    };
    match eval(expression, arena, params, &NoColumns)? {
        Datum::Null => Ok(0),
        Datum::Int2(v) if v >= 0 => Ok(v as u64),
        Datum::Int4(v) if v >= 0 => Ok(v as u64),
        Datum::Int8(v) if v >= 0 => Ok(v as u64),
        Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_) => Err(sql_err!(
            sqlstate::INVALID_ROW_COUNT_IN_RESULT_OFFSET,
            "OFFSET must not be negative"
        )),
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "argument of OFFSET must be an integer"
        )),
    }
}

/// ORDER BY <n> refers to the n-th select item, as in PostgreSQL.
pub fn resolve_order_expr_pub<'a>(
    expression: &'a Expr<'a>,
    items: &'a [SelectItem<'a>],
) -> Result<&'a Expr<'a>, SqlError> {
    // An unqualified name that matches a SELECT-list output column binds to
    // that output column, as in PostgreSQL (output names win over input
    // columns for a simple ORDER BY name). Matching two or more output columns
    // is ambiguous (42702), matching PostgreSQL's findTargetlistEntrySQL92 —
    // e.g. `SELECT (CASE .. ELSE b END), b ... ORDER BY b`, where the CASE
    // inherits the name `b` from its ELSE column.
    if let Expr::Column {
        qualifier: None,
        name,
    } = expression
    {
        let mut found: Option<&'a Expr<'a>> = None;
        for item in items {
            if let SelectItem::Expr {
                expression: item_expr,
                alias,
            } = item
            {
                let out_name = alias.unwrap_or(derived_name(item_expr));
                if out_name == *name {
                    match found {
                        // Two output columns share the name but resolve to
                        // different expressions — ambiguous (`SELECT s, s` is
                        // not, both being the same column).
                        Some(f) if *f != **item_expr => {
                            return Err(sql_err!(
                                crate::sql::eval::sqlstate::AMBIGUOUS_COLUMN,
                                "ORDER BY \"{}\" is ambiguous",
                                name
                            ));
                        }
                        Some(_) => {}
                        None => found = Some(item_expr),
                    }
                }
            }
        }
        if let Some(item_expr) = found {
            return Ok(item_expr);
        }
    }
    // Ordinal positions (`ORDER BY 2`) are resolved by the caller against the
    // expanded output columns (stars count per column, as in PostgreSQL).
    Ok(expression)
}

pub fn eval_limit_pub(
    limit: Option<&Expr>,
    arena: &Arena,
    params: &[Datum],
) -> Result<u64, SqlError> {
    let Some(expression) = limit else {
        return Ok(u64::MAX);
    };
    match eval(expression, arena, params, &NoColumns)? {
        Datum::Null => Ok(u64::MAX),
        Datum::Int2(v) if v >= 0 => Ok(v as u64),
        Datum::Int4(v) if v >= 0 => Ok(v as u64),
        Datum::Int8(v) if v >= 0 => Ok(v as u64),
        Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_) => Err(sql_err!(
            crate::sql::eval::sqlstate::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE,
            "LIMIT must not be negative"
        )),
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "argument of LIMIT must be an integer"
        )),
    }
}

#[expect(clippy::too_many_arguments, reason = "row pipeline plumbing")]
fn row_matches<'a>(
    storage: &Storage,
    table_index: usize,
    rowid: u64,
    def: &TableDef,
    alias: Option<&str>,
    schema: &[ColType],
    home: RowHome,
    where_clause: Option<&Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &super::eval::EvalHooks<'_, 'a>,
) -> Result<bool, SqlError> {
    let Some(w) = where_clause else {
        return Ok(true);
    };
    // Consume-in-place: the row's bytes live only for this predicate — a
    // WHERE over thousands of spilled rows must not fill the arena with
    // rows it rejects. (A WHERE subquery's own spilled reads take the
    // arena path, not this scratch, so nesting holds.)
    storage.with_row_bytes(table_index, rowid, home, |bytes| {
        let mut values = [Datum::Null; MAX_COLUMNS];
        rowenc::decode(bytes, schema, &mut values)?;
        row_matches_values(def, alias, &values, w, arena, params, hooks)
    })
}

fn row_matches_values<'a>(
    def: &TableDef,
    alias: Option<&str>,
    values: &[Datum<'_>],
    w: &Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &super::eval::EvalHooks<'_, 'a>,
) -> Result<bool, SqlError> {
    let context = RowCtx {
        def,
        values: &values[..def.n_columns],
        alias,
    };
    match super::eval::eval_full(w, arena, params, &context, hooks)? {
        Datum::Bool(true) => Ok(true),
        Datum::Bool(false) | Datum::Null => Ok(false),
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "argument of WHERE must be type boolean"
        )),
    }
}

#[expect(clippy::too_many_arguments, reason = "row pipeline plumbing")]
fn collect_matches<'a>(
    storage: &Storage,
    table_index: usize,
    alias: Option<&str>,
    txid: u32,
    schema: &[ColType],
    where_clause: Option<&Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &super::eval::EvalHooks<'_, 'a>,
    scratch: &mut FixedVec<(u64, RowHome)>,
) -> Result<(), SqlError> {
    scratch.clear();
    storage.record_serializable_read(txid, table_index);
    let def = storage.table_def(table_index, txid);
    storage.for_each_row_state(table_index, &mut |rowid, state| {
        use core::ops::ControlFlow;
        let Some(loc) = storage.visible_row_home(table_index, rowid, state, txid)? else {
            return Ok(ControlFlow::Continue(()));
        };
        if row_matches(
            storage,
            table_index,
            rowid,
            def,
            alias,
            schema,
            loc,
            where_clause,
            arena,
            params,
            hooks,
        )? {
            scratch.push((rowid, loc)).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement touches more than {} rows",
                    scratch.capacity()
                )
            })?;
        }
        Ok(ControlFlow::Continue(()))
    })?;
    // DML RETURNING follows the target table's physical row order. The row
    // map is hash-addressed, so restore the monotonic row identity assigned
    // when the heap image was first appended before locks and writes begin.
    scratch.sort_unstable_by_key(|(rowid, _)| *rowid);
    Ok(())
}

/// Collects target rows that join at least one row of the extra `from` tables
/// satisfying `where_clause` — for `UPDATE ... FROM` / `DELETE ... USING`. The
/// target row supplies its columns as the outer scope of the FROM scan.
#[allow(clippy::too_many_arguments)]
fn collect_join_matches<'a>(
    storage: &'a Storage,
    table_index: usize,
    def: &TableDef,
    alias: Option<&str>,
    schema: &[ColType],
    from: &'a super::ast::FromClause<'a>,
    where_clause: Option<&'a Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    txid: u32,
    scratch: &mut FixedVec<(u64, RowHome)>,
) -> Result<(), SqlError> {
    scratch.clear();
    storage.record_serializable_read(txid, table_index);
    storage.for_each_row_state(table_index, &mut |rowid, state| {
        use core::ops::ControlFlow;
        let Some(loc) = storage.visible_row_home(table_index, rowid, state, txid)? else {
            return Ok(ControlFlow::Continue(()));
        };
        // Consume-in-place, as in row_matches: the joined-row probe reads
        // this row's values only while it runs.
        let found = storage.with_row_bytes(table_index, rowid, loc, |bytes| {
            let mut tv = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(bytes, schema, &mut tv)?;
            let context = RowCtx {
                def,
                values: &tv[..def.n_columns],
                alias,
            };
            super::query::first_from_match(
                storage,
                from,
                txid,
                where_clause,
                &[],
                arena,
                params,
                &context,
                &mut |_| Ok(()),
            )
        })?;
        if found {
            scratch.push((rowid, loc)).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement touches more than {} rows",
                    scratch.capacity()
                )
            })?;
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

fn store_row(
    storage: &mut Storage,
    txn: &mut TxnState,
    table_index: usize,
    rowid: Option<u64>,
    values: &[Datum],
) -> Result<(), SqlError> {
    let len = rowenc::encoded_len(values);
    // Encode straight into the heap: values may borrow the arena but not
    // the heap (they come from INSERT expressions), so this is borrow-safe.
    let (loc, slice) = storage.heap.append(len)?;
    rowenc::encode(values, slice);
    let rowid = rowid.unwrap_or_else(|| storage.next_rowid());
    let prior = storage.write_pending(table_index, rowid, txn.txid, txn.command_id(), Some(loc))?;
    if let Err(e) = txn.touch(table_index as u32, rowid, prior) {
        storage.restore_pending(table_index, rowid, txn.txid, prior);
        return Err(e);
    }
    Ok(())
}

fn coerce<'a>(
    v: Datum<'a>,
    col: &ColumnMeta,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    // An enum column resolves a text (or already-typed enum) value to a member
    // of its type, validating the label against the catalog (22P02 otherwise).
    if let ColType::Enum(slot) = col.ctype {
        return coerce_enum_value(v, slot, storage, txid, arena);
    }
    if let ColType::Array(
        element @ (crate::sql::types::ArrElem::Enum(_) | crate::sql::types::ArrElem::Domain { .. }),
    ) = col.ctype
    {
        return coerce_user_type_array(v, element, storage, txid, arena);
    }
    if col.ctype.is_reg_object() {
        let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
        let value = crate::sql::eval::regobject_cast(v, col.ctype, Some(&catalog), arena)?;
        return apply_typmod(value, col.ctype, col.type_mod, arena);
    }
    let v = cast_to(v, col.ctype, arena).map_err(|e| {
        // Data errors (out of range, bad input syntax — class 22) keep their
        // specific message; only a genuine type mismatch is rewritten with the
        // column context.
        if e.sqlstate.starts_with("22") {
            e
        } else {
            sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "column \"{}\" is of type {} but expression is of incompatible type",
                col.name.as_str(),
                col.ctype.name()
            )
        }
    })?;
    apply_typmod(v, col.ctype, col.type_mod, arena)
}

/// Coerces an array to a user-defined element type. Its raw element payloads
/// keep the ordinary scalar row encoding, while the array tag carries the enum
/// or domain identity used by comparisons, wire OIDs, and `pg_typeof`.
pub(crate) fn coerce_user_type_array<'a>(
    value: Datum<'a>,
    target: crate::sql::types::ArrElem,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let (source, raw) = match value {
        Datum::Array { element, raw } => (element, raw),
        Datum::Text(text) | Datum::Bpchar(text) => (
            crate::sql::types::ArrElem::Text,
            crate::sql::array::parse_literal(
                text.trim_end_matches(' '),
                crate::sql::types::ArrElem::Text,
                arena,
            )?,
        ),
        Datum::Null => return Ok(Datum::Null),
        other => {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "cannot cast type {} to {}",
                crate::sql::eval::type_name_of_pub(&other),
                target.array_name()
            ));
        }
    };
    let count = crate::sql::array::len(raw);
    let mut items = [Datum::Null; crate::sql::array::MAX_ELEMENTS];
    for (index, item) in items.iter_mut().take(count).enumerate() {
        let value = crate::sql::array::get(raw, source, index).unwrap_or(Datum::Null);
        *item = match target {
            crate::sql::types::ArrElem::Enum(slot) => {
                coerce_enum_value(value, slot, storage, txid, arena)?
            }
            crate::sql::types::ArrElem::Domain { slot, .. } => constraints::coerce_domain_value(
                storage,
                slot as usize,
                value,
                txid,
                arena,
                crate::sql::eval::NO_PARAMS,
            )?,
            _ => unreachable!("caller restricts user-defined array elements"),
        };
    }
    Ok(Datum::Array {
        element: target,
        raw: crate::sql::array::build(&items[..count], arena)?,
    })
}

/// Coerces a value into an enum column: a NULL passes through; a text or
/// already-typed enum value must name a member of the enum at `slot`, else
/// PostgreSQL's 22P02 `invalid input value for enum <type>: "..."`.
pub(crate) fn coerce_enum_value<'a>(
    v: Datum<'a>,
    slot: u16,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if v.is_null() {
        return Ok(Datum::Null);
    }
    let def = storage.enum_for(slot as usize, txid);
    let label = match v {
        Datum::Enum { label, .. } | Datum::Text(label) => label,
        Datum::Bpchar(s) => s.trim_end_matches(' '),
        _ => {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "column is of type {} but expression is of incompatible type",
                def.name.as_str()
            ));
        }
    };
    let Some(sort) = def.sort_of(label) else {
        return Err(sql_err!(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            "invalid input value for enum {}: \"{}\"",
            def.name.as_str(),
            label
        ));
    };
    Ok(Datum::Enum {
        slot,
        sort,
        label: arena
            .alloc_str(label)
            .map_err(|_| super::query::arena_full_pub())?,
    })
}

/// Applies a type modifier to an explicit cast result. Differs from column
/// assignment ([`apply_typmod`]) in one way that matches PostgreSQL: an
/// over-length `varchar(n)`/`char(n)` cast TRUNCATES rather than erroring.
/// Numeric precision/scale still round or overflow as in a column.
pub fn apply_cast_typmod<'a>(
    v: Datum<'a>,
    ctype: ColType,
    type_mod: i32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if let ColType::Array(element) = ctype {
        return apply_array_element_typmod(v, element, type_mod, arena, true);
    }
    // Decoded once; the arms below match on meaning, so no site here can read
    // the modifier under the wrong encoding.
    let modifier = TypeMod::decode(ctype, type_mod);
    if modifier == TypeMod::None || v.is_null() {
        return Ok(v);
    }
    match (ctype, modifier, v) {
        (ColType::Text | ColType::Varchar, TypeMod::Length(max), Datum::Text(s)) => {
            if s.chars().count() > max {
                let end = s.char_indices().nth(max).map_or(s.len(), |(i, _)| i);
                let t = arena.alloc_str(&s[..end]).map_err(|_| {
                    sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "cast result too large")
                })?;
                return Ok(Datum::Text(t));
            }
            Ok(v)
        }
        (ColType::Bpchar, TypeMod::Length(n), Datum::Text(s) | Datum::Bpchar(s)) => {
            // The padded result is a *bpchar* value: the padding is part of it
            // (output functions and LIKE see it), while comparisons and text
            // casts strip it — which is what the variant encodes. A bpchar
            // source re-fits from its stripped text (`c::char(3)`).
            match bpchar_fit(s.trim_end_matches(' '), n, true, arena)? {
                Datum::Text(padded) => Ok(Datum::Bpchar(padded)),
                other => Ok(other),
            }
        }
        (ColType::Bit { varying }, TypeMod::Length(n), Datum::Bit { bits, .. }) => {
            super::eval::fit_bits(bits, n, varying, arena)
        }
        _ => apply_typmod(v, ctype, type_mod, arena),
    }
}

/// Fits a string into `char(n)`: over-length truncates (cast) or errors
/// (column), and a short value is blank-padded to `n` characters.
fn bpchar_fit<'a>(
    s: &'a str,
    n: usize,
    truncate: bool,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let clen = s.chars().count();
    if clen > n {
        if truncate {
            let end = s.char_indices().nth(n).map_or(s.len(), |(i, _)| i);
            let t = arena
                .alloc_str(&s[..end])
                .map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "cast result too large"))?;
            return Ok(Datum::Text(t));
        }
        return Err(sql_err!(
            sqlstate::STRING_DATA_RIGHT_TRUNCATION,
            "value too long for type character({})",
            n
        ));
    }
    if clen == n {
        return Ok(Datum::Text(s));
    }
    // Blank-pad to n characters (a space is one byte).
    let total = s.len() + (n - clen);
    let buffer = arena
        .alloc_slice_with(total, |_| b' ')
        .map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "padded value too large"))?;
    buffer[..s.len()].copy_from_slice(s.as_bytes());
    Ok(Datum::Text(unsafe {
        core::str::from_utf8_unchecked(buffer)
    }))
}

/// Enforces a PostgreSQL atttypmod on an already-cast value: varchar(n) length
/// (22001) and numeric(p,s) rounding to scale + precision (22003). Values with
/// no modifier, and NULLs, pass through unchanged.
pub fn apply_typmod<'a>(
    v: Datum<'a>,
    ctype: ColType,
    type_mod: i32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if let ColType::Array(element) = ctype {
        return apply_array_element_typmod(v, element, type_mod, arena, false);
    }
    let modifier = TypeMod::decode(ctype, type_mod);
    if modifier == TypeMod::None || v.is_null() {
        return Ok(v);
    }
    match (ctype, modifier, v) {
        (ColType::Text | ColType::Varchar, TypeMod::Length(max), Datum::Text(s)) => {
            if s.chars().count() > max {
                // Excess that is entirely spaces truncates silently, the same
                // allowance PostgreSQL gives both varchar and char columns.
                let end = s.char_indices().nth(max).map_or(s.len(), |(i, _)| i);
                if s[end..].chars().all(|c| c == ' ') {
                    return Ok(Datum::Text(&s[..end]));
                }
                return Err(sql_err!(
                    sqlstate::STRING_DATA_RIGHT_TRUNCATION,
                    "value too long for type character varying({})",
                    max
                ));
            }
            Ok(v)
        }
        (_, TypeMod::Length(n), Datum::Text(s) | Datum::Bpchar(s)) => {
            match bpchar_fit(s.trim_end_matches(' '), n, false, arena)? {
                Datum::Text(padded) => Ok(Datum::Bpchar(padded)),
                other => Ok(other),
            }
        }
        (_, TypeMod::NumericPS { precision, scale }, Datum::Numeric(n)) => {
            apply_numeric_typmod(&n, precision as usize, scale as usize, arena).map(Datum::Numeric)
        }
        (ColType::Bit { varying }, TypeMod::Length(n), Datum::Bit { bits, .. }) => {
            super::eval::fit_bits(bits, n, varying, arena)
        }
        // Fractional-second precision: micros round half-away-from-zero in
        // integer arithmetic, as PostgreSQL's AdjustTimestampForTypmod.
        (_, TypeMod::TemporalPrecision(p), Datum::Timestamp(t)) => {
            Ok(Datum::Timestamp(round_micros(t, p)))
        }
        (_, TypeMod::TemporalPrecision(p), Datum::Timestamptz(t)) => {
            Ok(Datum::Timestamptz(round_micros(t, p)))
        }
        (_, TypeMod::TemporalPrecision(p), Datum::Time(t)) => Ok(Datum::Time(round_micros(t, p))),
        (_, TypeMod::TemporalPrecision(p), Datum::Timetz(t, zone)) => {
            Ok(Datum::Timetz(round_micros(t, p), zone))
        }
        // An interval range form with no precision (`interval hour to minute`)
        // rounds nothing — its `precision: None` cannot be mistaken for a
        // number, where the packed 0xFFFF once could.
        (
            _,
            TypeMod::IntervalMod {
                precision: Some(p), ..
            },
            Datum::Interval(iv),
        ) => Ok(Datum::Interval(crate::sql::types::Interval {
            months: iv.months,
            days: iv.days,
            micros: round_micros(iv.micros, p),
        })),
        _ => Ok(v),
    }
}

/// Applies an array declaration's element modifier while retaining its shape.
/// PostgreSQL stores `char(3)[]` as an array type with the bpchar modifier;
/// leaving it on the array boundary loses blank padding and makes binary COPY
/// differ from scalar `char(3)`.
fn apply_array_element_typmod<'a>(
    value: Datum<'a>,
    element: crate::sql::types::ArrElem,
    type_mod: i32,
    arena: &'a Arena,
    cast: bool,
) -> Result<Datum<'a>, SqlError> {
    if type_mod < 0 || TypeMod::decode(element.to_coltype(), type_mod) == TypeMod::None {
        return Ok(value);
    }
    let (actual_element, raw) = match value {
        Datum::Null => return Ok(Datum::Null),
        Datum::Array { element, raw } => (element, raw),
        _ => {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "array type modifier requires an array value"
            ));
        }
    };
    if actual_element != element {
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "array element type does not match its modifier"
        ));
    }
    let shape = crate::sql::array::shape(raw).expect("array datum invariant");
    let count = shape.element_count();
    let mut values = [Datum::Null; crate::sql::array::MAX_ELEMENTS];
    for (index, slot) in values.iter_mut().take(count).enumerate() {
        let member = crate::sql::array::get(raw, element, index).expect("array datum invariant");
        *slot = if cast {
            apply_cast_typmod(member, element.to_coltype(), type_mod, arena)?
        } else {
            apply_typmod(member, element.to_coltype(), type_mod, arena)?
        };
    }
    Ok(Datum::Array {
        element,
        raw: crate::sql::array::build_shaped(&values[..count], shape, arena)?,
    })
}

/// Rounds microseconds to `p` (0..=6) fractional-second digits,
/// half-away-from-zero in integer arithmetic (PostgreSQL's
/// `AdjustTimestampForTypmod`).
fn round_micros(micros: i64, p: u8) -> i64 {
    let p = u32::from(p.min(6));
    let scale = 10i64.pow(6 - p);
    if scale == 1 {
        return micros;
    }
    let offset = scale / 2;
    if micros >= 0 {
        (micros + offset) / scale * scale
    } else {
        -((-micros + offset) / scale * scale)
    }
}

/// Rounds to `scale` fractional digits (half away from zero) and checks that
/// the result fits in `precision` significant digits, as PostgreSQL does when
/// storing into numeric(precision, scale). Works on the decimal text so the
/// base-10000 carry logic lives in one place (Numeric::parse). NaN carries no
/// scale.
fn apply_numeric_typmod<'a>(
    n: &super::numeric::Numeric,
    precision: usize,
    scale: usize,
    arena: &'a Arena,
) -> Result<super::numeric::Numeric<'a>, SqlError> {
    use super::numeric::Numeric;
    if n.is_nan() {
        return Numeric::parse("NaN", arena);
    }
    const DIG: usize = 2100;
    let text = stack_format!(2100, "{}", n);
    let s = text.as_str();
    let (neg, body) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    let (int_b, frac_b) = (int_part.as_bytes(), frac_part.as_bytes());
    let int_len = int_b.len();
    if int_len + scale + 2 >= DIG {
        return Err(sql_err!(
            sqlstate::NUMERIC_OUT_OF_RANGE,
            "numeric field overflow"
        ));
    }

    // Kept digits: every integer digit, then `scale` fractional digits (padded
    // with zeros), then round based on the first dropped fractional digit.
    let mut digits = [b'0'; DIG];
    digits[..int_len].copy_from_slice(int_b);
    for i in 0..scale {
        digits[int_len + i] = *frac_b.get(i).unwrap_or(&b'0');
    }
    let mut carry = frac_b.get(scale).is_some_and(|&d| d >= b'5');
    let mut i = int_len + scale;
    while carry && i > 0 {
        i -= 1;
        if digits[i] == b'9' {
            digits[i] = b'0';
        } else {
            digits[i] += 1;
            carry = false;
        }
    }

    // Significant integer digits: a carry out of the integer part means the
    // value rolled up to 1 followed by `int_len` zeros.
    let sig_int = if carry {
        int_len + 1
    } else {
        let lead_zeros = digits[..int_len].iter().take_while(|&&d| d == b'0').count();
        int_len - lead_zeros
    };
    if sig_int > precision - scale {
        return Err(sql_err!(
            sqlstate::NUMERIC_OUT_OF_RANGE,
            "numeric field overflow"
        ));
    }

    // Reassemble and re-parse (parse sets dscale = scale, matching PostgreSQL).
    let mut out = [0u8; DIG + 8];
    let mut k = 0;
    if neg {
        out[k] = b'-';
        k += 1;
    }
    if carry {
        out[k] = b'1';
        k += 1;
    }
    out[k..k + int_len].copy_from_slice(&digits[..int_len]);
    k += int_len;
    if scale > 0 {
        out[k] = b'.';
        k += 1;
        out[k..k + scale].copy_from_slice(&digits[int_len..int_len + scale]);
        k += scale;
    }
    let rounded = core::str::from_utf8(&out[..k]).expect("ascii digits");
    Numeric::parse(rounded, arena)
}

fn check_not_null(def: &TableDef, values: &[Datum]) -> Result<(), SqlError> {
    for (i, c) in def.columns().iter().enumerate() {
        if c.not_null && values[i].is_null() {
            return Err(sql_err!(
                sqlstate::NOT_NULL_VIOLATION,
                "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                c.name.as_str(),
                def.name.as_str()
            ));
        }
    }
    Ok(())
}

fn undefined_table(name: &str) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_TABLE,
        "relation \"{}\" does not exist",
        name
    )
}

/// PostgreSQL's 42P01 echoes the spelling used: a qualified reference names
/// its qualifier, a bare one does not.
fn undefined_qual(name: &QualName) -> SqlError {
    match name.schema {
        Some(schema) => sql_err!(
            sqlstate::UNDEFINED_TABLE,
            "relation \"{}.{}\" does not exist",
            schema,
            name.name
        ),
        None => undefined_table(name.name),
    }
}

/// Resolves a DML/DDL target through the search path to a table slot.
pub(crate) fn resolve_dml_table(
    storage: &Storage,
    name: &QualName,
    txid: u32,
) -> Result<usize, SqlError> {
    match storage.resolve_relation(name.schema, name.name, txid) {
        Some(crate::storage::ResolvedRelation::Table(slot)) => Ok(slot),
        _ => Err(undefined_qual(name)),
    }
}

fn require_table_privilege(
    storage: &Storage,
    table: usize,
    privilege: crate::storage::PrivilegeSet,
    txid: u32,
) -> Result<(), SqlError> {
    storage.require_schema_usage(storage.table_def(table, txid).schema.as_str(), txid)?;
    let role = storage.current_role_slot(txid).ok_or_else(|| {
        sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "current role is not present in the role catalog"
        )
    })?;
    if storage.has_object_privilege(
        storage.table_access_object(table, txid),
        role,
        privilege,
        txid,
    ) {
        return Ok(());
    }
    Err(sql_err!(
        sqlstate::INSUFFICIENT_PRIVILEGE,
        "permission denied for table {}",
        storage.table_def(table, txid).name.as_str()
    ))
}

/// Public view of the OID-to-ColType mapping for value-level renderers
/// (`oid::regtype`).
pub fn coltype_of_oid_pub(o: i32) -> Option<crate::sql::types::ColType> {
    describe::coltype_of_oid(o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;

    #[test]
    fn binary_int2vector_uses_postgres_array_wire_format() {
        let bytes = [
            0, 0, 0, 1, // dimensions
            0, 0, 0, 0, // no nulls
            0, 0, 0, 21, // int2 element OID
            0, 0, 0, 2, // count
            0, 0, 0, 0, // int2vector lower bound
            0, 0, 0, 2, 0, 1, // first value
            0, 0, 0, 2, 0, 2, // second value
        ];
        let mut budget = Budget::new(1 << 16);
        let arena = Arena::new(&mut budget, "binary int2vector", 1 << 12).unwrap();

        let datum = decode_binary_field(ColType::Int2Vector, &bytes, &arena).unwrap();
        assert_eq!(datum, Datum::Int2Vector(&[1, 0, 2, 0]));
        assert_eq!(datum.to_string(), "1 2");
    }

    #[test]
    fn binary_int2vector_rejects_nulls_and_trailing_bytes() {
        let null_vector = [
            0, 0, 0, 0, // dimensions
            0, 0, 0, 1, // has nulls
            0, 0, 0, 21, // int2 element OID
        ];
        let mut budget = Budget::new(1 << 16);
        let arena = Arena::new(&mut budget, "binary int2vector", 1 << 12).unwrap();
        let error = decode_binary_field(ColType::Int2Vector, &null_vector, &arena).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::BAD_COPY_FILE_FORMAT);

        let trailing = [
            0, 0, 0, 0, // dimensions
            0, 0, 0, 0, // no nulls
            0, 0, 0, 21, // int2 element OID
            0,  // unexpected trailing byte
        ];
        let error = decode_binary_field(ColType::Int2Vector, &trailing, &arena).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::BAD_COPY_FILE_FORMAT);
    }

    #[test]
    fn binary_array_preserves_all_dimensions_and_bounds() {
        let bytes = [
            0, 0, 0, 2, // dimensions
            0, 0, 0, 0, // no nulls
            0, 0, 0, 23, // int4 element OID
            0, 0, 0, 2, 0, 0, 0, 2, // first dimension and lower bound
            0, 0, 0, 2, 0, 0, 0, 4, // second dimension and lower bound
            0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 4, 0, 0, 0, 2, 0, 0, 0, 4, 0, 0, 0, 3, 0, 0, 0, 4, 0,
            0, 0, 4,
        ];
        let mut budget = Budget::new(1 << 16);
        let arena = Arena::new(&mut budget, "binary array", 1 << 12).unwrap();
        let datum = decode_binary_field(
            ColType::Array(crate::sql::types::ArrElem::Int4),
            &bytes,
            &arena,
        )
        .unwrap();
        let Datum::Array { raw, .. } = datum else {
            panic!("array expected");
        };
        let shape = crate::sql::array::shape(raw).unwrap();
        assert_eq!(shape.dimension_count(), 2);
        assert_eq!(
            (shape.lower_bound(0), shape.upper_bound(1)),
            (Some(2), Some(5))
        );
        assert_eq!(datum.to_string(), "[2:3][4:5]={{1,2},{3,4}}");
    }

    #[test]
    fn binary_record_decodes_typed_fields() {
        let bytes = [
            0, 0, 0, 2, // field count
            0, 0, 0, 23, // int4 OID
            0, 0, 0, 4, 0, 0, 0, 42, 0, 0, 0, 25, // text OID
            0, 0, 0, 2, b'h', b'i',
        ];
        let mut budget = Budget::new(1 << 16);
        let arena = Arena::new(&mut budget, "binary record", 1 << 12).unwrap();
        let datum = decode_binary_field(ColType::Record, &bytes, &arena).unwrap();
        let Datum::Record(fields) = datum else {
            panic!("record expected");
        };
        assert_eq!(fields[0].value, Datum::Int4(42));
        assert_eq!(fields[1].value, Datum::Text("hi"));
        assert_eq!((fields[0].name, fields[1].name), ("f1", "f2"));
    }

    #[test]
    fn structured_binary_fields_reject_trailing_bytes() {
        let mut budget = Budget::new(1 << 16);
        let arena = Arena::new(&mut budget, "structured binary fields", 1 << 12).unwrap();
        let cases = [
            (
                ColType::Array(crate::sql::types::ArrElem::Int4),
                &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 23, 0][..],
            ),
            (
                ColType::Range(crate::sql::types::RangeKind::Int4),
                &[1, 0][..],
            ),
            (
                ColType::Multirange(crate::sql::types::RangeKind::Int4),
                &[0, 0, 0, 0, 0][..],
            ),
            (ColType::Bit { varying: false }, &[0, 0, 0, 0, 0][..]),
        ];
        for (ctype, bytes) in cases {
            let error = decode_binary_field(ctype, bytes, &arena).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::BAD_COPY_FILE_FORMAT);
        }
    }
}
