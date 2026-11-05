//! Exact index lifetimes and replay boundaries across durable SQL phases.

use super::{datetime, sequence, type_inputs};
use crate::mem::lease_pool::{MutableSlice, SharedSlice};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::storage::{IndexKind, IndexReplacement, ReindexTable, SessionTableLock, Storage};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Create,
    Drop,
    Reindex,
}

impl Kind {
    pub(crate) fn for_statement(statement: &super::ast::Stmt<'_>) -> Option<Self> {
        match statement {
            super::ast::Stmt::CreateIndex {
                build: super::ast::IndexBuildMode::Concurrent,
                ..
            } => Some(Self::Create),
            super::ast::Stmt::DropIndex {
                build: super::ast::IndexBuildMode::Concurrent,
                ..
            } => Some(Self::Drop),
            super::ast::Stmt::Reindex { options, .. }
                if options.build == super::ast::IndexBuildMode::Concurrent =>
            {
                Some(Self::Reindex)
            }
            _ => None,
        }
    }

    pub(crate) fn completion(self) -> &'static str {
        match self {
            Self::Create => "CREATE INDEX",
            Self::Drop => "DROP INDEX",
            Self::Reindex => "REINDEX",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Commit(CommitPhase),
    WaitBuild,
    Build,
    WaitValidate,
    Validate,
    FinishCreate,
    MarkDead,
    FinishDrop,
    ReindexSnapshot,
    ReindexSwap,
    ReindexDead,
    ReindexDrop,
    ReindexFinish,
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitPhase {
    Registered,
    Ready,
    Validated,
    Invalid,
    Dead,
    ReindexRegistered,
    ReindexBuilt,
    ReindexValidated,
    ReindexSnapshotPassed,
    ReindexSwapped,
    ReindexDead,
    ReindexDropped,
}

impl CommitPhase {
    fn next(self) -> Phase {
        match self {
            Self::Registered => Phase::WaitBuild,
            Self::Ready => Phase::WaitValidate,
            Self::Validated => Phase::FinishCreate,
            Self::Invalid => Phase::MarkDead,
            Self::Dead => Phase::FinishDrop,
            Self::ReindexRegistered => Phase::WaitBuild,
            Self::ReindexBuilt => Phase::Build,
            Self::ReindexValidated => Phase::ReindexSnapshot,
            Self::ReindexSnapshotPassed => Phase::Validate,
            Self::ReindexSwapped => Phase::ReindexDead,
            Self::ReindexDead => Phase::ReindexDrop,
            Self::ReindexDropped => Phase::ReindexFinish,
        }
    }
}

impl Phase {
    pub(crate) fn commit(self) -> Option<CommitPhase> {
        match self {
            Self::Commit(phase) => Some(phase),
            _ => None,
        }
    }
}

pub(crate) struct Command {
    pub(crate) identity: u64,
    pub(crate) kind: Kind,
    pub(crate) phase: Phase,
    pub(crate) slot: usize,
    pub(crate) created_at: u64,
    table: Option<SessionTableLock>,
    reindex: Option<ReindexBatch>,
    source_origin: type_inputs::Source,
    source: SharedSlice<u8>,
    pub(crate) replay: Option<sequence::StatementReplayCursor>,
    pub(crate) clock: datetime::StatementClock,
    pub(crate) deadline: i64,
    holders: Option<SharedSlice<u32>>,
    next_holder: usize,
    pub(crate) reference: u64,
    pub(crate) cancel_after_commit: bool,
}

struct ReindexBatch {
    replacements: MutableSlice<IndexReplacement>,
    active_start: usize,
    current: usize,
    tables: SharedSlice<ReindexTable>,
    next_table: usize,
    selected_index: Option<i32>,
    tablespace: Option<u16>,
}

impl Command {
    pub(crate) fn capture(
        storage: &Storage,
        transaction: u32,
        kind: Kind,
        slot: usize,
    ) -> Result<Self, SqlError> {
        let (source_origin, source) = type_inputs::inspect_active_source(|origin, text| {
            storage
                .retain_index_command_source(text)
                .map(|source| (origin, source))
        })
        .ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "concurrent index command has no owned SQL source"
            )
        })??;
        let index = storage.index_visible_to(slot, transaction).ok_or_else(|| {
            sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "concurrent index lifetime is not visible"
            )
        })?;
        let table = storage
            .index_table_slot_to(slot, transaction)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "concurrent index has no selected table lifetime"
                )
            })?;
        let table = storage.hold_table_across_commits(transaction, table)?;
        Ok(Self {
            identity: u64::from(transaction),
            kind,
            phase: match kind {
                Kind::Create => Phase::Commit(CommitPhase::Registered),
                Kind::Drop => Phase::Commit(CommitPhase::Invalid),
                Kind::Reindex => unreachable!("REINDEX captures its complete selection"),
            },
            slot,
            created_at: index.created_at,
            table: Some(table),
            reindex: None,
            source_origin,
            source,
            replay: None,
            clock: datetime::statement_clock(),
            deadline: super::query::statement_deadline(),
            holders: None,
            next_holder: 0,
            reference: 0,
            cancel_after_commit: false,
        })
    }

    pub(crate) fn capture_reindex(
        storage: &Storage,
        transaction: u32,
        replacements: &[IndexReplacement],
        tables: &[ReindexTable],
        next_table: usize,
        selected_index: Option<i32>,
        tablespace: Option<u16>,
    ) -> Result<Self, SqlError> {
        let (source_origin, source) = type_inputs::inspect_active_source(|origin, text| {
            storage
                .retain_index_command_source(text)
                .map(|source| (origin, source))
        })
        .ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "concurrent REINDEX command has no owned SQL source"
            )
        })??;
        let retained = storage.retain_index_replacements(replacements)?;
        let tables = storage.retain_reindex_tables(tables)?;
        let first = replacements.first().ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "concurrent REINDEX command has no selected indexes"
            )
        })?;
        let table = storage.hold_table_across_commits(transaction, first.table)?;
        Ok(Self {
            identity: u64::from(transaction),
            kind: Kind::Reindex,
            phase: Phase::Commit(CommitPhase::ReindexRegistered),
            slot: first.new_slot,
            created_at: first.new_created_at,
            table: Some(table),
            reindex: Some(ReindexBatch {
                replacements: retained,
                active_start: 0,
                current: 0,
                tables,
                next_table,
                selected_index,
                tablespace,
            }),
            source_origin,
            source,
            replay: None,
            clock: datetime::statement_clock(),
            deadline: super::query::statement_deadline(),
            holders: None,
            next_holder: 0,
            reference: 0,
            cancel_after_commit: false,
        })
    }

    pub(crate) fn reindex_replacements(&self) -> Option<&[IndexReplacement]> {
        self.reindex.as_ref().map(|batch| &*batch.replacements)
    }

    fn after_commit(&mut self, phase: CommitPhase) {
        let mut next = phase.next();
        if let Some(batch) = &mut self.reindex {
            if matches!(
                phase,
                CommitPhase::ReindexBuilt | CommitPhase::ReindexSnapshotPassed
            ) {
                batch.current += 1;
                if batch.current == batch.replacements.len() {
                    batch.current = batch.active_start;
                    next = if phase == CommitPhase::ReindexBuilt {
                        Phase::WaitValidate
                    } else {
                        Phase::ReindexSwap
                    };
                }
            }
            self.slot = batch.replacements[batch.current].new_slot;
            self.created_at = batch.replacements[batch.current].new_created_at;
        }
        self.advance(next);
    }

    fn validate_source(&self, kind: Kind) -> Result<(), SqlError> {
        let same_source = type_inputs::inspect_active_source(|origin, text| {
            origin == self.source_origin && text.as_bytes() == &*self.source
        })
        .unwrap_or(false);
        if self.kind != kind || !same_source {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "concurrent index command changed while suspended"
            ));
        }
        Ok(())
    }

    pub(crate) fn validate(
        &self,
        storage: &Storage,
        transaction: u32,
        kind: Kind,
    ) -> Result<usize, SqlError> {
        self.validate_source(kind)?;
        let table = storage.validate_session_table_lock(
            self.table.as_ref().expect("selected index table lease"),
            transaction,
        )?;
        if let Some(batch) = &self.reindex {
            for replacement in &batch.replacements[batch.active_start..] {
                let index = storage
                    .index_visible_to(replacement.new_slot, transaction)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::SERIALIZATION_FAILURE,
                            "REINDEX replacement lifetime disappeared"
                        )
                    })?;
                if index.created_at != replacement.new_created_at
                    || storage.index_table_slot_to(replacement.new_slot, transaction)
                        != Some(replacement.table)
                {
                    return Err(sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "REINDEX replacement lifetime changed"
                    ));
                }
                let old = storage
                    .index_for_event_trigger(replacement.old_slot, transaction)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::SERIALIZATION_FAILURE,
                            "REINDEX original lifetime disappeared"
                        )
                    })?;
                if old.created_at != replacement.old_created_at {
                    return Err(sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "REINDEX original lifetime changed"
                    ));
                }
            }
        }
        let index = storage
            .index_for_event_trigger(self.slot, transaction)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "concurrent index lifetime disappeared"
                )
            })?;
        if index.created_at != self.created_at
            || storage.index_table_slot_to(self.slot, transaction) != Some(table)
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "concurrent index lifetime changed while suspended"
            ));
        }
        Ok(table)
    }

    pub(crate) fn wait_for_locks(
        &mut self,
        storage: &Storage,
        transaction: u32,
        table: usize,
        mode: super::ast::TableLockMode,
    ) -> Result<(), SqlError> {
        if self.holders.is_none() {
            self.holders = Some(storage.capture_index_lock_holders(transaction, table, mode)?);
            self.next_holder = 0;
        }
        storage.wait_for_index_holders(
            transaction,
            self.holders.as_ref().expect("captured holders"),
            &mut self.next_holder,
        )
    }

    pub(crate) fn wait_for_snapshots(
        &mut self,
        storage: &Storage,
        transaction: u32,
    ) -> Result<(), SqlError> {
        if self.holders.is_none() {
            self.holders =
                Some(storage.capture_index_snapshot_holders(transaction, self.reference)?);
            self.next_holder = 0;
        }
        storage.wait_for_index_holders(
            transaction,
            self.holders.as_ref().expect("captured snapshot holders"),
            &mut self.next_holder,
        )
    }

    pub(crate) fn advance(&mut self, next: Phase) {
        self.phase = next;
        self.holders = None;
        self.next_holder = 0;
    }
}

impl super::Engine {
    pub(super) fn execute_concurrent_index_phase(
        &mut self,
        txn: &mut super::TxnState,
        kind: Kind,
        arena: &crate::mem::arena::Arena,
        responder: &mut crate::pg::respond::Responder,
    ) -> Result<Result<(), SqlError>, crate::pg::wire::WireFull> {
        let mut command = txn
            .concurrent_index
            .take()
            .expect("index phase has an owner");
        let result = if command.phase == Phase::ReindexFinish {
            match self.advance_reindex_command(&mut command, txn, arena, responder) {
                Ok(result) => result,
                Err(error) => {
                    txn.concurrent_index = Some(command);
                    return Err(error);
                }
            }
        } else {
            (|| -> Result<(), SqlError> {
                command.validate_source(kind)?;
                let table = if command.kind == Kind::Reindex
                    && matches!(command.phase, Phase::ReindexFinish | Phase::Complete)
                {
                    None
                } else {
                    let table = command.validate(&self.storage, txn.txid, kind)?;
                    self.storage.lock_table(
                        txn.txid,
                        table,
                        super::ast::TableLockMode::ShareUpdateExclusive,
                        false,
                    )?;
                    Some(table)
                };
                match command.phase {
                    Phase::WaitBuild | Phase::WaitValidate => {
                        let next = if command.phase == Phase::WaitBuild {
                            Phase::Build
                        } else {
                            Phase::Validate
                        };
                        command.wait_for_locks(
                            &self.storage,
                            txn.txid,
                            table.expect("active index table"),
                            super::ast::TableLockMode::Share,
                        )?;
                        command.advance(next);
                    }
                    Phase::Build => {
                        self.stage_concurrent_index_kind(
                            txn,
                            command.slot,
                            IndexKind::OrdinaryReady,
                        )?;
                        super::exec::build_concurrent_index(
                            &mut self.storage,
                            txn,
                            command.slot,
                            table.expect("active index table"),
                            arena,
                        )?;
                        command.advance(Phase::Commit(if command.reindex.is_some() {
                            CommitPhase::ReindexBuilt
                        } else {
                            CommitPhase::Ready
                        }));
                    }
                    Phase::Validate => {
                        super::exec::validate_concurrent_index(
                            &self.storage,
                            txn,
                            command.slot,
                            table.expect("active index table"),
                            arena,
                        )?;
                        command.reference = self.storage.commit_snapshot();
                        command.advance(Phase::Commit(if command.reindex.is_some() {
                            CommitPhase::ReindexValidated
                        } else {
                            CommitPhase::Validated
                        }));
                    }
                    Phase::FinishCreate => {
                        command.wait_for_snapshots(&self.storage, txn.txid)?;
                        self.stage_concurrent_index_kind(txn, command.slot, IndexKind::Ordinary)?;
                        command.advance(Phase::Complete);
                    }
                    Phase::MarkDead => {
                        command.wait_for_locks(
                            &self.storage,
                            txn.txid,
                            table.expect("active index table"),
                            super::ast::TableLockMode::AccessExclusive,
                        )?;
                        self.stage_concurrent_index_kind(
                            txn,
                            command.slot,
                            IndexKind::OrdinaryDead,
                        )?;
                        command.advance(Phase::Commit(CommitPhase::Dead));
                    }
                    Phase::FinishDrop => {
                        command.wait_for_locks(
                            &self.storage,
                            txn.txid,
                            table.expect("active index table"),
                            super::ast::TableLockMode::AccessExclusive,
                        )?;
                        super::exec::stage_index_drop(
                            &mut self.storage,
                            &mut self.wal,
                            txn,
                            command.slot,
                        )?;
                        command.advance(Phase::Complete);
                    }
                    Phase::ReindexSnapshot => {
                        command.wait_for_snapshots(&self.storage, txn.txid)?;
                        command.advance(Phase::Commit(CommitPhase::ReindexSnapshotPassed));
                    }
                    Phase::ReindexSwap => {
                        let batch = command.reindex.as_ref().expect("REINDEX swap selection");
                        super::exec::swap_reindex_definitions(
                            &mut self.storage,
                            &mut self.wal,
                            txn,
                            &batch.replacements[batch.active_start..],
                            arena,
                        )?;
                        command.advance(Phase::Commit(CommitPhase::ReindexSwapped));
                    }
                    Phase::ReindexDead => {
                        command.wait_for_locks(
                            &self.storage,
                            txn.txid,
                            table.expect("active index table"),
                            super::ast::TableLockMode::AccessExclusive,
                        )?;
                        let batch = command.reindex.as_ref().expect("REINDEX dead selection");
                        for replacement in &batch.replacements[batch.active_start..] {
                            self.stage_concurrent_index_kind(
                                txn,
                                replacement.old_slot,
                                IndexKind::OrdinaryDead,
                            )?;
                        }
                        command.advance(Phase::Commit(CommitPhase::ReindexDead));
                    }
                    Phase::ReindexDrop => {
                        command.wait_for_locks(
                            &self.storage,
                            txn.txid,
                            table.expect("active index table"),
                            super::ast::TableLockMode::AccessExclusive,
                        )?;
                        let batch = command.reindex.as_ref().expect("REINDEX drop selection");
                        for replacement in &batch.replacements[batch.active_start..] {
                            super::exec::stage_index_drop(
                                &mut self.storage,
                                &mut self.wal,
                                txn,
                                replacement.old_slot,
                            )?;
                        }
                        command.advance(Phase::Commit(CommitPhase::ReindexDropped));
                    }
                    Phase::ReindexFinish => {
                        unreachable!("batch continuation has its own wire boundary")
                    }
                    Phase::Complete => {}
                    Phase::Commit(_) => {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "index SQL action entered during its commit phase"
                        ));
                    }
                }
                Ok(())
            })()
        };
        let complete = command.phase == Phase::Complete;
        txn.concurrent_index = Some(command);
        match result {
            Err(error) => Ok(Err(error)),
            Ok(()) => {
                if complete {
                    responder.command_complete(kind.completion())?;
                }
                Ok(Ok(()))
            }
        }
    }

    fn advance_reindex_command(
        &mut self,
        command: &mut Command,
        txn: &mut super::TxnState,
        arena: &crate::mem::arena::Arena,
        responder: &mut crate::pg::respond::Responder,
    ) -> Result<Result<(), SqlError>, crate::pg::wire::WireFull> {
        if let Err(error) = command.validate_source(Kind::Reindex) {
            return Ok(Err(error));
        }
        command.table = None;
        let batch = command.reindex.as_mut().expect("REINDEX continuation");
        while batch.next_table < batch.tables.len() {
            let selected = batch.tables[batch.next_table];
            if !self.storage.table_slot_visible_to(selected.slot, txn.txid)
                || self.storage.table(selected.slot).created_at() != selected.created_at
            {
                batch.next_table += 1;
                continue;
            }
            let mark = txn.statement_mark(self.wal.stage_mark(txn.txid), self.storage.lock_mark());
            let replacements = match super::exec::register_next_reindex_table(
                &mut self.storage,
                &mut self.wal,
                txn,
                selected.slot,
                batch.selected_index,
                batch.tablespace,
                arena,
                responder,
            )? {
                Ok(replacements) => replacements,
                Err(error) => {
                    if error.sqlstate == sqlstate::INTERNAL_LOCK_WAIT
                        || error.sqlstate == sqlstate::INTERNAL_IO_WAIT
                    {
                        self.rollback_waiting_statement(txn, mark);
                    }
                    return Ok(Err(error));
                }
            };
            if replacements.is_empty() {
                batch.next_table += 1;
                continue;
            }
            let result = (|| -> Result<_, SqlError> {
                let first = replacements[0];
                let count = batch
                    .replacements
                    .len()
                    .checked_add(replacements.len())
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "REINDEX replacement receipt count exhausted"
                        )
                    })?;
                batch
                    .replacements
                    .reserve_exact(count)
                    .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))?;
                let lease = self
                    .storage
                    .hold_table_across_commits(txn.txid, selected.slot)?;
                batch.active_start = batch.replacements.len();
                batch.current = batch.active_start;
                batch
                    .replacements
                    .extend_from_slice(replacements)
                    .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))?;
                batch.next_table += 1;
                Ok((first, lease))
            })();
            let (first, lease) = match result {
                Ok(value) => value,
                Err(error) => return Ok(Err(error)),
            };
            command.slot = first.new_slot;
            command.created_at = first.new_created_at;
            command.table = Some(lease);
            command.advance(Phase::Commit(CommitPhase::ReindexRegistered));
            return Ok(Ok(()));
        }
        command.advance(Phase::Complete);
        Ok(Ok(()))
    }

    fn stage_concurrent_index_kind(
        &mut self,
        txn: &mut super::TxnState,
        slot: usize,
        kind: IndexKind,
    ) -> Result<(), SqlError> {
        let index = self
            .storage
            .index_visible_to(slot, txn.txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "concurrent index lifetime disappeared during transition"
                )
            })?;
        let mut definition = index.mutable_for(txn.txid);
        if definition.kind == kind {
            // An unchanged state has no pending image to bind.
            return Ok(());
        }
        definition.kind = kind;
        if !kind.valid() {
            definition.clustered = false;
            definition.replica_identity = false;
        }
        super::exec::stage_index_definition(
            &mut self.storage,
            &mut self.wal,
            txn,
            slot,
            definition,
        )?;
        if kind == IndexKind::OrdinaryDead {
            self.storage.prepare_index_definition_enforcers(
                slot,
                txn.txid,
                crate::storage::IndexBindingOperation::State,
            )?;
        }
        Ok(())
    }
}

impl super::Engine {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn commit_concurrent_index_phase(
        &mut self,
        txn: &mut super::TxnState,
        sqlprep: &mut super::SqlPreparedPool,
        cursors: &mut super::cursor::CursorPool,
        guc: &super::GucState,
        arena: &crate::mem::arena::Arena,
        responder: &mut crate::pg::respond::Responder,
    ) -> Result<(), SqlError> {
        let mut command = txn
            .concurrent_index
            .take()
            .expect("index commit has a command owner");
        let phase = command
            .phase
            .commit()
            .expect("only commit phases enter durability");
        let target = super::txn::CommitTarget::ConcurrentIndex {
            command: command.identity,
            phase,
        };
        let result = self.commit_txn_with_triggers(
            txn,
            guc,
            target,
            Some(super::CursorCommitContext {
                cursors,
                plans: sqlprep,
                arena,
            }),
            arena,
            responder,
        );
        if result.is_ok() {
            command.after_commit(phase);
            datetime::restore_statement_clock(command.clock);
            txn.restore_statement_deadline(command.deadline);
        }
        let canceled = command.cancel_after_commit;
        txn.concurrent_index = Some(command);
        result?;
        if canceled {
            return Err(sql_err!(
                sqlstate::QUERY_CANCELED,
                "canceling statement due to user request"
            ));
        }
        self.ensure_txn(txn, super::TxnMode::Implicit, guc)
    }
}
