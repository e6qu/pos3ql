//! Exact index lifetimes and replay boundaries across durable SQL phases.

use super::{datetime, sequence, type_inputs};
use crate::mem::lease_pool::SharedSlice;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::storage::{IndexKind, SessionTableLock, Storage};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Create,
    Drop,
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
            _ => None,
        }
    }

    pub(crate) fn completion(self) -> &'static str {
        match self {
            Self::Create => "CREATE INDEX",
            Self::Drop => "DROP INDEX",
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
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitPhase {
    Registered,
    Ready,
    Validated,
    Invalid,
    Dead,
}

impl CommitPhase {
    fn next(self) -> Phase {
        match self {
            Self::Registered => Phase::WaitBuild,
            Self::Ready => Phase::WaitValidate,
            Self::Validated => Phase::FinishCreate,
            Self::Invalid => Phase::MarkDead,
            Self::Dead => Phase::FinishDrop,
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
    table: SessionTableLock,
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
            },
            slot,
            created_at: index.created_at,
            table,
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

    pub(crate) fn validate(
        &self,
        storage: &Storage,
        transaction: u32,
        kind: Kind,
    ) -> Result<usize, SqlError> {
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
        let table = storage.validate_session_table_lock(&self.table, transaction)?;
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
        let result = (|| -> Result<(), SqlError> {
            let table = command.validate(&self.storage, txn.txid, kind)?;
            self.storage.lock_table(
                txn.txid,
                table,
                super::ast::TableLockMode::ShareUpdateExclusive,
                false,
            )?;
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
                        table,
                        super::ast::TableLockMode::Share,
                    )?;
                    command.advance(next);
                }
                Phase::Build => {
                    self.stage_concurrent_index_kind(txn, command.slot, IndexKind::OrdinaryReady)?;
                    super::exec::build_concurrent_index(
                        &mut self.storage,
                        txn,
                        command.slot,
                        table,
                        arena,
                    )?;
                    command.advance(Phase::Commit(CommitPhase::Ready));
                }
                Phase::Validate => {
                    super::exec::validate_concurrent_index(
                        &self.storage,
                        txn,
                        command.slot,
                        table,
                        arena,
                    )?;
                    command.reference = self.storage.commit_snapshot();
                    command.advance(Phase::Commit(CommitPhase::Validated));
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
                        table,
                        super::ast::TableLockMode::AccessExclusive,
                    )?;
                    self.stage_concurrent_index_kind(txn, command.slot, IndexKind::OrdinaryDead)?;
                    self.storage.prepare_index_definition_enforcers(
                        command.slot,
                        txn.txid,
                        crate::storage::IndexBindingOperation::State,
                    )?;
                    command.advance(Phase::Commit(CommitPhase::Dead));
                }
                Phase::FinishDrop => {
                    command.wait_for_locks(
                        &self.storage,
                        txn.txid,
                        table,
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
                Phase::Complete => {}
                Phase::Commit(_) => {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "index SQL action entered during its commit phase"
                    ));
                }
            }
            Ok(())
        })();
        let complete = command.phase == Phase::Complete;
        txn.concurrent_index = Some(command);
        match result {
            Err(error) => Ok(Err(error)),
            Ok(()) => {
                if complete {
                    responder.command_complete(match kind {
                        Kind::Create => "CREATE INDEX",
                        Kind::Drop => "DROP INDEX",
                    })?;
                }
                Ok(Ok(()))
            }
        }
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
            return Ok(());
        }
        definition.kind = kind;
        if !kind.valid() {
            definition.clustered = false;
            definition.replica_identity = false;
        }
        super::exec::stage_index_definition(&mut self.storage, &mut self.wal, txn, slot, definition)
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
            command.advance(phase.next());
            datetime::restore_statement_clock(command.clock);
            super::query::restore_statement_deadline(command.deadline);
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
