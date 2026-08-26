//! Startup-bounded relation identity for logical-subscription apply.
//!
//! A pgoutput relation identifier is connection-local.  This map binds it to
//! one verified local table definition before any tuple reaches row mutation.

use crate::mem::arena::Arena;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::mem::fixed_vec::FixedVec;
use crate::pg::pginput::{CopyData, Message, Relation, ReplicaIdentity};
use crate::pg::respond::Responder;
use crate::sql::ast::ExplainSerialize;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::guc::GucState;
use crate::sql::txn::TxnState;
use crate::sql_err;
use crate::storage::{MAX_COLUMNS, Storage};

/// Internal trigger execution never exposes query output to the publisher.
/// This fixed buffer only retains protocol framing required by the shared
/// executor while discarded output prevents remote rows from consuming it.
const TRIGGER_RESPONSE_BYTES: usize = 4096;

#[derive(Clone, Copy)]
pub struct RelationBinding {
    relation_id: u32,
    table_slot: usize,
    remote_to_local: [usize; MAX_COLUMNS],
    column_count: usize,
    key_remote_to_local: [usize; MAX_COLUMNS],
    key_count: usize,
}

impl RelationBinding {
    pub fn relation_id(&self) -> u32 {
        self.relation_id
    }

    pub fn table_slot(&self) -> usize {
        self.table_slot
    }

    pub fn remote_to_local(&self) -> &[usize] {
        &self.remote_to_local[..self.column_count]
    }

    pub fn old_remote_to_local(&self, identity: ReplicaIdentity) -> &[usize] {
        match identity {
            ReplicaIdentity::Key => &self.key_remote_to_local[..self.key_count],
            ReplicaIdentity::Old => self.remote_to_local(),
        }
    }

    pub fn key_local_columns(&self) -> &[usize] {
        &self.key_remote_to_local[..self.key_count]
    }
}

/// Fixed, connection-local relation bindings for one apply worker.
pub struct RelationMap {
    bindings: FixedVec<RelationBinding>,
}

impl RelationMap {
    pub fn new(budget: &mut Budget, capacity: usize) -> Result<Self, BudgetError> {
        Ok(Self {
            bindings: FixedVec::new(budget, "subscription_relations", capacity)?,
        })
    }

    /// Validates the remote relation against the subscribed local table and
    /// records its current connection-local identifier.  A changed relation
    /// message replaces its prior mapping atomically; incompatible schemas
    /// fail before any data message can be applied.
    pub fn register(
        &mut self,
        storage: &Storage,
        txid: u32,
        relation: Relation<'_>,
    ) -> Result<(), SqlError> {
        let table_slot = storage
            .find_visible(relation.namespace, relation.name, txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "subscription relation \"{}.{}\" does not exist locally",
                    relation.namespace,
                    relation.name
                )
            })?;
        let definition = storage.table_def(table_slot, txid);
        if relation.columns().len() > definition.n_columns {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "subscription relation \"{}.{}\" has {} columns, local table has {}",
                relation.namespace,
                relation.name,
                relation.columns().len(),
                definition.n_columns
            ));
        }
        let mut remote_to_local = [usize::MAX; MAX_COLUMNS];
        let mut key_remote_to_local = [usize::MAX; MAX_COLUMNS];
        let mut key_count = 0;
        for (remote, column) in relation.columns().iter().enumerate() {
            let local = definition.column_index(column.name).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "subscription relation \"{}.{}\" has no local column \"{}\"",
                    relation.namespace,
                    relation.name,
                    column.name
                )
            })?;
            if remote_to_local[..remote].contains(&local) {
                return Err(sql_err!(
                    sqlstate::PROTOCOL_VIOLATION,
                    "subscription relation \"{}.{}\" repeats column \"{}\"",
                    relation.namespace,
                    relation.name,
                    column.name
                ));
            }
            remote_to_local[remote] = local;
            if column.key {
                key_remote_to_local[key_count] = local;
                key_count += 1;
            }
        }
        let binding = RelationBinding {
            relation_id: relation.id,
            table_slot,
            remote_to_local,
            column_count: relation.columns().len(),
            key_remote_to_local,
            key_count,
        };
        if let Some(existing) = self
            .bindings
            .iter_mut()
            .find(|existing| existing.relation_id == relation.id)
        {
            *existing = binding;
            return Ok(());
        }
        self.bindings.push(binding).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "subscription relation cache exceeds {} entries",
                self.bindings.capacity()
            )
        })
    }

    pub fn binding(&self, relation_id: u32) -> Result<RelationBinding, SqlError> {
        self.bindings
            .iter()
            .copied()
            .find(|binding| binding.relation_id == relation_id)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROTOCOL_VIOLATION,
                    "subscription data references unknown relation {}",
                    relation_id
                )
            })
    }

    pub fn clear(&mut self) {
        self.bindings.clear();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteTransaction {
    Idle,
    Applying { final_lsn: u64 },
    Skipping { final_lsn: u64, configured: bool },
    Streaming { xid: u32, segment_open: bool },
}

/// Result of applying one receive frame.  The replication transport may emit
/// a status update only for this proof, which is produced after the local
/// commit and durable subscription frontier succeed together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyResult {
    None,
    Acknowledge {
        flushed_lsn: u64,
        reply_requested: bool,
    },
}

/// Startup-bounded, single-subscription pgoutput apply state.  It owns the
/// local transaction, relation cache, and scratch arena so a remote commit is
/// either entirely applied and durable or entirely rolled back.
pub struct SubscriptionApply {
    stream: crate::storage::SubscriptionStream,
    txn: TxnState,
    guc: GucState,
    relations: RelationMap,
    arena: Arena,
    trigger_response: FixedBuf,
    trigger_scratch: crate::sql::exec::DmlScratch,
    remote: RemoteTransaction,
    confirmed_lsn: u64,
    behavior: crate::storage::SubscriptionBehavior,
}

impl SubscriptionApply {
    pub const fn budget_bytes(
        relation_capacity: usize,
        txn_rows: usize,
        arena_bytes: usize,
    ) -> usize {
        relation_capacity * core::mem::size_of::<RelationBinding>()
            + TxnState::budget_bytes(txn_rows)
            + arena_bytes
            + TRIGGER_RESPONSE_BYTES
            + txn_rows * core::mem::size_of::<crate::sql::exec::PhysicalRow>()
    }

    pub(crate) fn new(
        budget: &mut Budget,
        stream: crate::storage::SubscriptionStream,
        relation_capacity: usize,
        txn_rows: usize,
        arena_bytes: usize,
        confirmed_lsn: u64,
        behavior: crate::storage::SubscriptionBehavior,
    ) -> Result<Self, BudgetError> {
        Ok(Self {
            stream,
            txn: TxnState::new(budget, txn_rows)?,
            guc: GucState::new(),
            relations: RelationMap::new(budget, relation_capacity)?,
            arena: Arena::new(budget, "subscription_apply_arena", arena_bytes)?,
            trigger_response: FixedBuf::new(
                budget,
                "subscription_trigger_response",
                TRIGGER_RESPONSE_BYTES,
            )?,
            trigger_scratch: FixedVec::new(budget, "subscription_trigger_scratch", txn_rows)?,
            remote: RemoteTransaction::Idle,
            confirmed_lsn,
            behavior,
        })
    }

    pub fn confirmed_lsn(&self) -> u64 {
        self.confirmed_lsn
    }

    fn commit(&mut self, engine: &mut crate::sql::Engine) -> Result<(), SqlError> {
        self.trigger_response.clear();
        let mut responder = Responder::new(&mut self.trigger_response);
        engine.commit_txn_with_triggers(&mut self.txn, &self.guc, &self.arena, &mut responder)
    }

    pub(crate) fn establish_frontier(
        &mut self,
        engine: &mut crate::sql::Engine,
        confirmed_lsn: u64,
    ) -> Result<(), SqlError> {
        engine.begin_subscription_apply(&mut self.txn, &self.guc);
        let result = engine
            .stage_subscription_advance(&mut self.txn, self.stream, confirmed_lsn)
            .and_then(|advanced| {
                if !advanced {
                    return Err(Self::protocol_error(
                        "subscription bootstrap frontier did not advance",
                    ));
                }
                self.commit(engine)
            });
        if result.is_err() && self.txn.is_active() {
            engine.rollback_txn(&mut self.txn, &self.guc);
        }
        if result.is_ok() {
            self.confirmed_lsn = confirmed_lsn;
        }
        result
    }

    pub(crate) fn begin_bootstrap(
        &mut self,
        engine: &mut crate::sql::Engine,
    ) -> Result<(), SqlError> {
        engine.begin_subscription_apply(&mut self.txn, &self.guc);
        self.txn.mode = crate::sql::txn::TxnMode::Explicit;
        engine.begin_subscription_relation_refresh(&mut self.txn, self.stream)
    }

    pub(crate) fn register_bootstrap_relation(
        &mut self,
        engine: &mut crate::sql::Engine,
        schema: crate::storage::SqlName,
        table: crate::storage::SqlName,
    ) -> Result<(), SqlError> {
        engine.stage_subscription_relation(
            &mut self.txn,
            self.stream,
            schema.as_str(),
            table.as_str(),
        )
    }

    pub(crate) fn start_copy_table(
        &mut self,
        engine: &mut crate::sql::Engine,
        schema: crate::storage::SqlName,
        table: crate::storage::SqlName,
        columns: &[crate::storage::SqlName],
    ) -> Result<crate::sql::exec::CopySetup, SqlError> {
        let setup = engine.subscription_copy_setup(schema, table, columns, self.txn.txid)?;
        self.trigger_response.clear();
        let mut responder = Responder::new(&mut self.trigger_response);
        engine.copy_start(
            &setup,
            &mut self.txn,
            self.guc.seq_session(),
            &self.arena,
            &mut responder,
        )?;
        Ok(setup)
    }

    pub(crate) fn copy_line(
        &mut self,
        engine: &mut crate::sql::Engine,
        setup: &crate::sql::exec::CopySetup,
        line: &[u8],
    ) -> Result<(), SqlError> {
        let mark = self.arena.mark();
        self.trigger_response.clear();
        let mut responder = Responder::new(&mut self.trigger_response);
        let result = engine
            .copy_row_line(
                setup,
                &mut self.txn,
                self.guc.seq_session(),
                &self.arena,
                &mut responder,
                line,
            )
            .map(|_| ());
        unsafe { self.arena.rewind_to(mark) };
        result
    }

    pub(crate) fn copy_binary_row(
        &mut self,
        engine: &mut crate::sql::Engine,
        setup: &crate::sql::exec::CopySetup,
        row: &[u8],
    ) -> Result<(), SqlError> {
        let mark = self.arena.mark();
        self.trigger_response.clear();
        let mut responder = Responder::new(&mut self.trigger_response);
        let result = engine
            .copy_row_binary(
                setup,
                &mut self.txn,
                self.guc.seq_session(),
                &self.arena,
                &mut responder,
                row,
            )
            .map(|_| ());
        unsafe { self.arena.rewind_to(mark) };
        result
    }

    pub(crate) fn finish_copy_table(
        &mut self,
        engine: &mut crate::sql::Engine,
        setup: &crate::sql::exec::CopySetup,
    ) -> Result<(), SqlError> {
        self.trigger_response.clear();
        let mut responder = Responder::new(&mut self.trigger_response);
        engine.copy_finish(setup, &mut self.txn, &self.guc, &mut responder)
    }

    pub(crate) fn finish_bootstrap(
        &mut self,
        engine: &mut crate::sql::Engine,
        confirmed_lsn: u64,
    ) -> Result<(), SqlError> {
        let advanced =
            engine.stage_subscription_advance(&mut self.txn, self.stream, confirmed_lsn)?;
        if !advanced {
            return Err(Self::protocol_error(
                "subscription bootstrap frontier did not advance",
            ));
        }
        let result = self.commit(engine);
        if result.is_ok() {
            self.confirmed_lsn = confirmed_lsn;
        }
        result
    }

    /// Rebinds a preallocated worker after its former transport has stopped.
    /// The worker must be idle, so no transaction, row lock, relation mapping,
    /// or arena reference can cross a subscription identity change.
    pub(crate) fn bind(
        &mut self,
        stream: crate::storage::SubscriptionStream,
        confirmed_lsn: u64,
        behavior: crate::storage::SubscriptionBehavior,
    ) -> Result<(), SqlError> {
        if self.remote != RemoteTransaction::Idle || self.txn.is_active() {
            return Err(Self::protocol_error(
                "subscription worker cannot change binding during a remote transaction",
            ));
        }
        self.stream = stream;
        self.confirmed_lsn = confirmed_lsn;
        self.behavior = behavior;
        self.relations.clear();
        self.arena.reset();
        self.trigger_response.clear();
        self.trigger_scratch.clear();
        Ok(())
    }

    pub fn unbind(&mut self) {
        debug_assert_eq!(self.remote, RemoteTransaction::Idle);
        debug_assert!(!self.txn.is_active());
        self.stream = crate::storage::SubscriptionStream::EMPTY;
        self.confirmed_lsn = 0;
        self.relations.clear();
        self.arena.reset();
        self.trigger_response.clear();
        self.trigger_scratch.clear();
    }

    /// Stops a worker at a transport boundary.  Losing a publisher connection
    /// must roll back its incomplete remote transaction before the fixed slot
    /// can be rebound and replayed from the durable acknowledgement frontier.
    pub fn stop(&mut self, engine: &mut crate::sql::Engine) {
        self.abort(engine);
    }

    fn protocol_error(message: &'static str) -> SqlError {
        sql_err!(sqlstate::PROTOCOL_VIOLATION, "{message}")
    }

    fn abort(&mut self, engine: &mut crate::sql::Engine) {
        if self.txn.is_active() {
            engine.rollback_txn(&mut self.txn, &self.guc);
        }
        self.remote = RemoteTransaction::Idle;
        self.arena.reset();
    }

    fn require_message_xid(&self, xid: Option<u32>) -> Result<(), SqlError> {
        match (self.remote, xid) {
            (
                RemoteTransaction::Streaming {
                    segment_open: true, ..
                },
                Some(_),
            ) => Ok(()),
            (RemoteTransaction::Applying { .. } | RemoteTransaction::Skipping { .. }, None) => {
                Ok(())
            }
            _ => Err(Self::protocol_error(
                "subscription message transaction identity does not match the active transaction",
            )),
        }
    }

    fn begin_message_subtransaction(
        &mut self,
        engine: &mut crate::sql::Engine,
        xid: Option<u32>,
    ) -> Result<(), SqlError> {
        self.require_message_xid(xid)?;
        if let (
            RemoteTransaction::Streaming {
                xid: top,
                segment_open: true,
            },
            Some(message),
        ) = (self.remote, xid)
            && message != top
        {
            engine.begin_subscription_subtransaction(&mut self.txn, &self.guc, message)?;
        }
        Ok(())
    }

    fn apply_message(
        &mut self,
        engine: &mut crate::sql::Engine,
        frame_end_lsn: u64,
        message: Message<'_>,
    ) -> Result<ApplyResult, SqlError> {
        match message {
            Message::Begin { final_lsn, .. } => {
                if self.remote != RemoteTransaction::Idle {
                    return Err(Self::protocol_error(
                        "subscription received BEGIN before the prior transaction committed",
                    ));
                }
                self.arena.reset();
                if final_lsn <= self.confirmed_lsn {
                    self.remote = RemoteTransaction::Skipping {
                        final_lsn,
                        configured: false,
                    };
                } else if self.behavior.skip_lsn == Some(final_lsn) {
                    engine.begin_subscription_apply(&mut self.txn, &self.guc);
                    self.remote = RemoteTransaction::Skipping {
                        final_lsn,
                        configured: true,
                    };
                } else {
                    engine.begin_subscription_apply(&mut self.txn, &self.guc);
                    self.remote = RemoteTransaction::Applying { final_lsn };
                }
                Ok(ApplyResult::None)
            }
            Message::Commit {
                commit_lsn,
                end_lsn,
            } => match self.remote {
                // XLogData's walEnd is the publisher's current WAL frontier,
                // which may be ahead of this transaction's commit end.
                RemoteTransaction::Applying { final_lsn }
                    if commit_lsn == final_lsn && end_lsn <= frame_end_lsn =>
                {
                    if !engine.stage_subscription_advance(&mut self.txn, self.stream, end_lsn)? {
                        return Err(Self::protocol_error(
                            "subscription received a non-monotonic commit after apply began",
                        ));
                    }
                    self.commit(engine)?;
                    self.confirmed_lsn = end_lsn;
                    self.remote = RemoteTransaction::Idle;
                    self.arena.reset();
                    Ok(ApplyResult::Acknowledge {
                        flushed_lsn: end_lsn,
                        reply_requested: false,
                    })
                }
                RemoteTransaction::Skipping {
                    final_lsn,
                    configured: false,
                } if commit_lsn == final_lsn && end_lsn <= self.confirmed_lsn => {
                    self.remote = RemoteTransaction::Idle;
                    self.arena.reset();
                    Ok(ApplyResult::Acknowledge {
                        flushed_lsn: self.confirmed_lsn,
                        reply_requested: false,
                    })
                }
                RemoteTransaction::Skipping {
                    final_lsn,
                    configured: true,
                } if commit_lsn == final_lsn && end_lsn <= frame_end_lsn => {
                    engine.stage_subscription_skip(
                        &mut self.txn,
                        self.stream,
                        final_lsn,
                        end_lsn,
                    )?;
                    self.commit(engine)?;
                    self.behavior.skip_lsn = None;
                    self.confirmed_lsn = end_lsn;
                    self.remote = RemoteTransaction::Idle;
                    self.arena.reset();
                    Ok(ApplyResult::Acknowledge {
                        flushed_lsn: end_lsn,
                        reply_requested: false,
                    })
                }
                _ => Err(sql_err!(
                    sqlstate::PROTOCOL_VIOLATION,
                    "subscription COMMIT is not valid for the active apply transaction (commit end {}, frame end {})",
                    end_lsn,
                    frame_end_lsn
                )),
            },
            Message::Relation { xid, relation } => {
                if xid.is_some() {
                    self.require_message_xid(xid)?;
                }
                engine.register_subscription_relation(&mut self.relations, &self.txn, relation)?;
                Ok(ApplyResult::None)
            }
            Message::Insert {
                xid,
                relation_id,
                new,
            } => {
                self.begin_message_subtransaction(engine, xid)?;
                if matches!(self.remote, RemoteTransaction::Skipping { .. }) {
                    return Ok(ApplyResult::None);
                }
                if !matches!(
                    self.remote,
                    RemoteTransaction::Applying { .. }
                        | RemoteTransaction::Streaming {
                            segment_open: true,
                            ..
                        }
                ) {
                    return Err(Self::protocol_error(
                        "subscription INSERT is outside BEGIN/COMMIT",
                    ));
                }
                self.trigger_response.clear();
                self.trigger_scratch.clear();
                let mut responder = Responder::new(&mut self.trigger_response);
                responder.begin_discard_query_output(ExplainSerialize::None);
                let result = {
                    let mut trigger_context = crate::sql::exec::ReplicationTriggerContext::new(
                        self.guc.seq_session(),
                        &mut responder,
                        &mut self.trigger_scratch,
                    );
                    engine.apply_subscription_insert(
                        &mut self.txn,
                        self.stream,
                        self.relations.binding(relation_id)?,
                        new,
                        &self.arena,
                        &mut trigger_context,
                    )
                };
                let _ = responder.finish_discard_query_output();
                result?;
                Ok(ApplyResult::None)
            }
            Message::Update {
                xid,
                relation_id,
                identity,
                new,
            } => {
                self.begin_message_subtransaction(engine, xid)?;
                if matches!(self.remote, RemoteTransaction::Skipping { .. }) {
                    return Ok(ApplyResult::None);
                }
                if !matches!(
                    self.remote,
                    RemoteTransaction::Applying { .. }
                        | RemoteTransaction::Streaming {
                            segment_open: true,
                            ..
                        }
                ) {
                    return Err(Self::protocol_error(
                        "subscription UPDATE is outside BEGIN/COMMIT",
                    ));
                }
                self.trigger_response.clear();
                self.trigger_scratch.clear();
                let mut responder = Responder::new(&mut self.trigger_response);
                responder.begin_discard_query_output(ExplainSerialize::None);
                let result = {
                    let mut trigger_context = crate::sql::exec::ReplicationTriggerContext::new(
                        self.guc.seq_session(),
                        &mut responder,
                        &mut self.trigger_scratch,
                    );
                    engine.apply_subscription_update(
                        &mut self.txn,
                        self.stream,
                        self.relations.binding(relation_id)?,
                        crate::sql::exec::ReplicationUpdate { identity, new },
                        &self.arena,
                        &mut trigger_context,
                    )
                };
                let _ = responder.finish_discard_query_output();
                result?;
                Ok(ApplyResult::None)
            }
            Message::Delete {
                xid,
                relation_id,
                old,
            } => {
                self.begin_message_subtransaction(engine, xid)?;
                if matches!(self.remote, RemoteTransaction::Skipping { .. }) {
                    return Ok(ApplyResult::None);
                }
                if !matches!(
                    self.remote,
                    RemoteTransaction::Applying { .. }
                        | RemoteTransaction::Streaming {
                            segment_open: true,
                            ..
                        }
                ) {
                    return Err(Self::protocol_error(
                        "subscription DELETE is outside BEGIN/COMMIT",
                    ));
                }
                self.trigger_response.clear();
                self.trigger_scratch.clear();
                let mut responder = Responder::new(&mut self.trigger_response);
                responder.begin_discard_query_output(ExplainSerialize::None);
                let result = {
                    let mut trigger_context = crate::sql::exec::ReplicationTriggerContext::new(
                        self.guc.seq_session(),
                        &mut responder,
                        &mut self.trigger_scratch,
                    );
                    engine.apply_subscription_delete(
                        &mut self.txn,
                        self.stream,
                        self.relations.binding(relation_id)?,
                        old,
                        &self.arena,
                        &mut trigger_context,
                    )
                };
                let _ = responder.finish_discard_query_output();
                result?;
                Ok(ApplyResult::None)
            }
            // Relation carries the complete OID/type-modifier contract used by
            // row application.  A Type frame contains no relation membership
            // or row state, so its successful typed decode has no local state
            // transition to make.
            Message::Type { xid, .. } => {
                if xid.is_some() {
                    self.require_message_xid(xid)?;
                }
                Ok(ApplyResult::None)
            }
            Message::Truncate { xid, truncate } => {
                self.begin_message_subtransaction(engine, xid)?;
                if matches!(self.remote, RemoteTransaction::Skipping { .. }) {
                    return Ok(ApplyResult::None);
                }
                if !matches!(
                    self.remote,
                    RemoteTransaction::Applying { .. }
                        | RemoteTransaction::Streaming {
                            segment_open: true,
                            ..
                        }
                ) {
                    return Err(Self::protocol_error(
                        "subscription TRUNCATE is outside BEGIN/COMMIT",
                    ));
                }
                let mut tables = [0_usize; crate::sql::txn::MAX_TRUNCATE_TABLES];
                if truncate.relation_ids().len() > tables.len() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "subscription TRUNCATE has {} tables, maximum is {}",
                        truncate.relation_ids().len(),
                        tables.len()
                    ));
                }
                for (index, &relation_id) in truncate.relation_ids().iter().enumerate() {
                    tables[index] = self.relations.binding(relation_id)?.table_slot();
                }
                engine.apply_subscription_truncate(
                    &mut self.txn,
                    self.stream,
                    &tables[..truncate.relation_ids().len()],
                    truncate.cascade,
                    truncate.restart_identity,
                )?;
                Ok(ApplyResult::None)
            }
            Message::StreamStart { xid, first_segment } => {
                match self.remote {
                    RemoteTransaction::Idle if first_segment => {
                        self.arena.reset();
                        engine.begin_subscription_apply(&mut self.txn, &self.guc);
                        self.remote = RemoteTransaction::Streaming {
                            xid,
                            segment_open: true,
                        };
                    }
                    RemoteTransaction::Streaming {
                        xid: active,
                        segment_open: false,
                    } if active == xid && !first_segment => {
                        self.remote = RemoteTransaction::Streaming {
                            xid,
                            segment_open: true,
                        };
                    }
                    _ => {
                        return Err(Self::protocol_error(
                            "subscription STREAM START is not valid for the active transaction",
                        ));
                    }
                }
                Ok(ApplyResult::None)
            }
            Message::StreamStop => {
                let RemoteTransaction::Streaming {
                    xid,
                    segment_open: true,
                } = self.remote
                else {
                    return Err(Self::protocol_error(
                        "subscription STREAM STOP has no open segment",
                    ));
                };
                self.remote = RemoteTransaction::Streaming {
                    xid,
                    segment_open: false,
                };
                Ok(ApplyResult::None)
            }
            Message::StreamCommit {
                xid,
                commit_lsn,
                end_lsn,
            } => {
                let RemoteTransaction::Streaming {
                    xid: active,
                    segment_open: false,
                } = self.remote
                else {
                    return Err(Self::protocol_error(
                        "subscription STREAM COMMIT has no stopped transaction",
                    ));
                };
                if active != xid || commit_lsn > end_lsn || end_lsn > frame_end_lsn {
                    return Err(Self::protocol_error(
                        "subscription STREAM COMMIT has an invalid transaction identity or LSN",
                    ));
                }
                if !engine.stage_subscription_advance(&mut self.txn, self.stream, end_lsn)? {
                    return Err(Self::protocol_error(
                        "subscription received a non-monotonic streamed commit",
                    ));
                }
                self.commit(engine)?;
                self.confirmed_lsn = end_lsn;
                self.remote = RemoteTransaction::Idle;
                self.arena.reset();
                Ok(ApplyResult::Acknowledge {
                    flushed_lsn: end_lsn,
                    reply_requested: false,
                })
            }
            Message::StreamAbort { xid, subxid, .. } => {
                let RemoteTransaction::Streaming { xid: active, .. } = self.remote else {
                    return Err(Self::protocol_error(
                        "subscription STREAM ABORT has no active streamed transaction",
                    ));
                };
                if active != xid {
                    return Err(Self::protocol_error(
                        "subscription STREAM ABORT targets another transaction",
                    ));
                }
                if subxid == xid {
                    self.abort(engine);
                } else if !engine.rollback_subscription_subtransaction(
                    &mut self.txn,
                    &self.guc,
                    subxid,
                ) {
                    return Err(Self::protocol_error(
                        "subscription STREAM ABORT targets an unknown subtransaction",
                    ));
                }
                Ok(ApplyResult::None)
            }
        }
    }

    /// Consumes one fully decoded receive frame.  A failed frame aborts the
    /// active local transaction before it escapes, so reconnect/replay starts
    /// at the last acknowledged durable position.
    pub fn receive(
        &mut self,
        engine: &mut crate::sql::Engine,
        frame: CopyData<'_>,
    ) -> Result<ApplyResult, SqlError> {
        let result = match frame {
            CopyData::XLogData {
                end_lsn, message, ..
            } => self.apply_message(engine, end_lsn, message),
            CopyData::PrimaryKeepalive {
                end_lsn: _,
                reply_requested,
            } => Ok(ApplyResult::Acknowledge {
                flushed_lsn: self.confirmed_lsn,
                reply_requested,
            }),
        };
        if result.is_err() {
            self.abort(engine);
        }
        result
    }
}
