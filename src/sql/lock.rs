//! Startup-bounded PostgreSQL row locks and wait-for graph.
//!
//! The executor owns no provider-specific state here. Row identities are the
//! stable `(table slot, rowid)` pair already used by MVCC and durable SSTs;
//! locks are process-local transaction state and are released at transaction
//! end. A blocked connection remains parked by the protocol reactor and retries
//! the statement after `generation` changes.

use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql::ast::{LockStrength, LockWait};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

pub(crate) type WaitOwner = u64;

const PREPARED_OWNER_BIT: WaitOwner = 1 << 63;

pub(crate) const fn connection_wait_owner(connection_id: i32) -> WaitOwner {
    connection_id as u32 as WaitOwner
}

pub(crate) const fn prepared_wait_owner(transaction_id: u32) -> WaitOwner {
    PREPARED_OWNER_BIT | transaction_id as WaitOwner
}

pub(crate) const fn wait_owner_pid(owner: WaitOwner) -> Option<i32> {
    if owner & PREPARED_OWNER_BIT == 0 && owner != 0 {
        Some(owner as i32)
    } else {
        None
    }
}

#[derive(Clone, Copy)]
struct RowLock {
    table: u32,
    rowid: u64,
    owner: u32,
    wait_owner: WaitOwner,
    /// Acquisition sequence for each PostgreSQL row-lock mode. Zero means
    /// absent. Keeping the modes independently makes savepoint rollback able
    /// to remove only locks or upgrades acquired by the rolled-back
    /// subtransaction.
    modes: [u64; 4],
}

#[derive(Clone, Copy)]
struct WaitEdge {
    waiter: WaitOwner,
    blocker: WaitOwner,
}

/// Result of asking for one row lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockDecision {
    Acquired,
    Skipped,
    Waiting,
}

/// Fixed-capacity row-lock registry plus one outgoing wait edge per live
/// transaction. Connections themselves are the wait queues: a generation
/// change makes the reactor retry every parked statement.
pub(crate) struct LockManager {
    locks: FixedVec<RowLock>,
    waits: FixedVec<WaitEdge>,
    generation: u64,
}

impl LockManager {
    pub(crate) fn budget_bytes(lock_capacity: usize, connection_capacity: usize) -> usize {
        lock_capacity * core::mem::size_of::<RowLock>()
            + connection_capacity * core::mem::size_of::<WaitEdge>()
    }

    pub(crate) fn new(
        budget: &mut Budget,
        lock_capacity: usize,
        connection_capacity: usize,
    ) -> Result<Self, BudgetError> {
        Ok(Self {
            locks: FixedVec::new(budget, "row_locks", lock_capacity)?,
            waits: FixedVec::new(budget, "lock_waits", connection_capacity)?,
            generation: 1,
        })
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn visit_owner(
        &self,
        owner: u32,
        mut visit: impl FnMut(usize, u64, LockStrength) -> Result<(), SqlError>,
    ) -> Result<(), SqlError> {
        for lock in self.locks.iter().filter(|lock| lock.owner == owner) {
            for (index, acquired_at) in lock.modes.iter().enumerate() {
                if *acquired_at == 0 {
                    continue;
                }
                let strength = match index {
                    0 => LockStrength::Update,
                    1 => LockStrength::NoKeyUpdate,
                    2 => LockStrength::Share,
                    _ => LockStrength::KeyShare,
                };
                visit(lock.table as usize, lock.rowid, strength)?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn acquire(
        &mut self,
        table: usize,
        rowid: u64,
        owner: u32,
        wait_owner: WaitOwner,
        strength: LockStrength,
        wait: LockWait,
        sequence: u64,
    ) -> Result<LockDecision, SqlError> {
        let requested = mode_bit(strength);
        let requested_index = requested.trailing_zeros() as usize;
        let own_index = self.locks.iter().position(|lock| {
            lock.table == table as u32 && lock.rowid == rowid && lock.owner == owner
        });
        let combined = own_index
            .map(|index| modes_mask(&self.locks[index].modes) | requested)
            .unwrap_or(requested);
        let blocker = self.locks.iter().find(|lock| {
            lock.table == table as u32
                && lock.rowid == rowid
                && lock.owner != owner
                && conflicts(combined, modes_mask(&lock.modes))
        });
        if let Some(blocker) = blocker {
            return match wait {
                LockWait::SkipLocked => Ok(LockDecision::Skipped),
                LockWait::NoWait => Err(sql_err!(
                    sqlstate::LOCK_NOT_AVAILABLE,
                    "could not obtain lock on row in relation"
                )),
                LockWait::Wait => {
                    self.set_wait(wait_owner, blocker.wait_owner)?;
                    Ok(LockDecision::Waiting)
                }
            };
        }
        self.clear_wait(wait_owner);
        if let Some(index) = own_index {
            if self.locks[index].modes[requested_index] == 0 {
                self.locks[index].modes[requested_index] = sequence;
            }
        } else {
            let mut modes = [0; 4];
            modes[requested_index] = sequence;
            self.locks
                .push(RowLock {
                    table: table as u32,
                    rowid,
                    owner,
                    wait_owner,
                    modes,
                })
                .map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "row-lock registry is full ({} locks)",
                        self.locks.capacity()
                    )
                })?;
        }
        Ok(LockDecision::Acquired)
    }

    /// Parks `waiter` behind a transaction-owned resource whose identity is
    /// tracked by its subsystem (pending row/catalog versions, for example)
    /// rather than by the row-lock registry itself.
    pub(crate) fn wait_for(
        &mut self,
        waiter: WaitOwner,
        blocker: WaitOwner,
    ) -> Result<(), SqlError> {
        self.set_wait(waiter, blocker)
    }

    pub(crate) fn release(&mut self, owner: u32, wait_owner: WaitOwner) {
        let mut changed = false;
        let mut index = 0usize;
        while index < self.locks.len() {
            if self.locks[index].owner == owner {
                self.locks.swap_remove(index);
                changed = true;
            } else {
                index += 1;
            }
        }
        self.clear_wait(wait_owner);
        // Waiters on this owner retry against the registry rather than keeping
        // stale graph edges after the blocker disappears.
        index = 0;
        while index < self.waits.len() {
            if self.waits[index].blocker == wait_owner {
                self.waits.swap_remove(index);
                changed = true;
            } else {
                index += 1;
            }
        }
        if changed {
            self.generation = self.generation.wrapping_add(1).max(1);
        }
    }

    pub(crate) fn rollback_to(&mut self, owner: u32, wait_owner: WaitOwner, mark: u64) {
        let mut changed = false;
        let mut index = 0usize;
        while index < self.locks.len() {
            if self.locks[index].owner != owner {
                index += 1;
                continue;
            }
            for acquired_at in &mut self.locks[index].modes {
                if *acquired_at > mark {
                    *acquired_at = 0;
                    changed = true;
                }
            }
            if modes_mask(&self.locks[index].modes) == 0 {
                self.locks.swap_remove(index);
            } else {
                index += 1;
            }
        }
        self.clear_wait(wait_owner);
        if changed {
            index = 0;
            while index < self.waits.len() {
                if self.waits[index].blocker == wait_owner {
                    self.waits.swap_remove(index);
                } else {
                    index += 1;
                }
            }
            self.generation = self.generation.wrapping_add(1).max(1);
        }
    }

    pub(crate) fn resource_released(&mut self, owner: WaitOwner) {
        let mut changed = false;
        let mut index = 0usize;
        while index < self.waits.len() {
            if self.waits[index].blocker == owner {
                self.waits.swap_remove(index);
                changed = true;
            } else {
                index += 1;
            }
        }
        if changed {
            self.generation = self.generation.wrapping_add(1).max(1);
        }
    }

    pub(crate) fn blocker_pids(&self, waiter_pid: i32, output: &mut [i32]) -> usize {
        let waiter = connection_wait_owner(waiter_pid);
        let mut count = 0usize;
        for edge in self.waits.iter().filter(|edge| edge.waiter == waiter) {
            let Some(pid) = wait_owner_pid(edge.blocker) else {
                continue;
            };
            if output[..count].contains(&pid) || count == output.len() {
                continue;
            }
            output[count] = pid;
            count += 1;
        }
        count
    }

    pub(crate) fn blocker_pid_count(&self, waiter_pid: i32) -> usize {
        let waiter = connection_wait_owner(waiter_pid);
        self.waits
            .iter()
            .filter(|edge| edge.waiter == waiter && wait_owner_pid(edge.blocker).is_some())
            .count()
    }

    pub(crate) fn rebind_wait_owner(&mut self, from: WaitOwner, to: WaitOwner) {
        for lock in self.locks.iter_mut().filter(|lock| lock.wait_owner == from) {
            lock.wait_owner = to;
        }
        for edge in self.waits.iter_mut() {
            if edge.waiter == from {
                edge.waiter = to;
            }
            if edge.blocker == from {
                edge.blocker = to;
            }
        }
    }

    fn set_wait(&mut self, waiter: WaitOwner, blocker: WaitOwner) -> Result<(), SqlError> {
        if let Some(edge) = self.waits.iter_mut().find(|edge| edge.waiter == waiter) {
            edge.blocker = blocker;
        } else {
            self.waits.push(WaitEdge { waiter, blocker }).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "lock wait graph is full ({} transactions)",
                    self.waits.capacity()
                )
            })?;
        }
        let mut cursor = blocker;
        for _ in 0..=self.waits.len() {
            if cursor == waiter {
                self.clear_wait(waiter);
                return Err(sql_err!(sqlstate::DEADLOCK_DETECTED, "deadlock detected"));
            }
            let Some(edge) = self.waits.iter().find(|edge| edge.waiter == cursor) else {
                return Ok(());
            };
            cursor = edge.blocker;
        }
        Ok(())
    }

    pub(crate) fn clear_wait(&mut self, owner: WaitOwner) {
        if let Some(index) = self.waits.iter().position(|edge| edge.waiter == owner) {
            self.waits.swap_remove(index);
        }
    }
}

/// PostgreSQL's two disjoint advisory-key namespaces. A signed SQL key is
/// carried by bit pattern so negative values retain the same `pg_locks`
/// identity PostgreSQL exposes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AdvisoryKey {
    pub class_id: u32,
    pub object_id: u32,
    pub object_sub_id: i16,
}

impl AdvisoryKey {
    pub(crate) const fn from_bigint(value: i64) -> Self {
        let bits = value as u64;
        Self {
            class_id: (bits >> 32) as u32,
            object_id: bits as u32,
            object_sub_id: 1,
        }
    }

    pub(crate) const fn from_ints(class_id: i32, object_id: i32) -> Self {
        Self {
            class_id: class_id as u32,
            object_id: object_id as u32,
            object_sub_id: 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdvisoryMode {
    Shared,
    Exclusive,
}

impl AdvisoryMode {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Shared => "ShareLock",
            Self::Exclusive => "ExclusiveLock",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdvisoryOwner {
    Session(i32),
    Transaction(u32),
}

#[derive(Clone, Copy)]
struct AdvisoryLock {
    key: AdvisoryKey,
    owner: AdvisoryOwner,
    wait_owner: WaitOwner,
    mode: AdvisoryMode,
    count: u32,
    acquired_at: u64,
}

#[derive(Clone, Copy)]
struct AdvisoryWait {
    key: AdvisoryKey,
    wait_owner: WaitOwner,
    mode: AdvisoryMode,
    started_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdvisoryReplayOperation {
    Acquire {
        key: AdvisoryKey,
        mode: AdvisoryMode,
        try_only: bool,
    },
    Unlock {
        key: AdvisoryKey,
        mode: AdvisoryMode,
    },
    UnlockAll,
}

#[derive(Clone, Copy)]
struct AdvisoryReplayAction {
    connection_id: i32,
    operation: AdvisoryReplayOperation,
    result: bool,
}

#[derive(Clone, Copy)]
struct AdvisoryReplayState {
    connection_id: i32,
    preserved: bool,
    cursor: usize,
    replay_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdvisoryDecision {
    Acquired,
    Unavailable,
    Waiting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AdvisoryLockView {
    pub key: AdvisoryKey,
    pub wait_owner: WaitOwner,
    pub mode: AdvisoryMode,
    pub granted: bool,
    pub wait_start: Option<i64>,
}

/// Startup-bounded advisory locks. Session and transaction owners share the
/// same conflict table, while ownership stays typed so rollback and disconnect
/// cannot release each other's state.
pub(crate) struct AdvisoryLockManager {
    locks: FixedVec<AdvisoryLock>,
    waits: FixedVec<AdvisoryWait>,
    replay_actions: FixedVec<AdvisoryReplayAction>,
    replay_states: FixedVec<AdvisoryReplayState>,
}

impl AdvisoryLockManager {
    pub(crate) fn budget_bytes(lock_capacity: usize, connection_capacity: usize) -> usize {
        lock_capacity * core::mem::size_of::<AdvisoryLock>()
            + connection_capacity * core::mem::size_of::<AdvisoryWait>()
            + lock_capacity * core::mem::size_of::<AdvisoryReplayAction>()
            + connection_capacity * core::mem::size_of::<AdvisoryReplayState>()
    }

    pub(crate) fn new(
        budget: &mut Budget,
        lock_capacity: usize,
        connection_capacity: usize,
    ) -> Result<Self, BudgetError> {
        Ok(Self {
            locks: FixedVec::new(budget, "advisory_locks", lock_capacity)?,
            waits: FixedVec::new(budget, "advisory_lock_waits", connection_capacity)?,
            replay_actions: FixedVec::new(budget, "advisory_lock_replay_actions", lock_capacity)?,
            replay_states: FixedVec::new(
                budget,
                "advisory_lock_replay_states",
                connection_capacity,
            )?,
        })
    }

    /// Starts an executor attempt. A parked statement replays its already
    /// applied session-lock calls because pos3ql retries the expression tree
    /// from its root when the blocker wakes it.
    pub(crate) fn begin_statement(&mut self, connection_id: i32) -> Result<(), SqlError> {
        if let Some(index) = self
            .replay_states
            .iter()
            .position(|state| state.connection_id == connection_id)
        {
            if self.replay_states[index].preserved {
                let replay_count = self
                    .replay_actions
                    .iter()
                    .filter(|action| action.connection_id == connection_id)
                    .count();
                self.replay_states[index].preserved = false;
                self.replay_states[index].cursor = 0;
                self.replay_states[index].replay_count = replay_count;
                return Ok(());
            }
            self.clear_replay_actions(connection_id);
            self.replay_states[index].cursor = 0;
            self.replay_states[index].replay_count = 0;
            return Ok(());
        }
        self.replay_states
            .push(AdvisoryReplayState {
                connection_id,
                preserved: false,
                cursor: 0,
                replay_count: 0,
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "advisory lock statement registry is full ({} connections)",
                    self.replay_states.capacity()
                )
            })
    }

    pub(crate) fn preserve_statement(&mut self, connection_id: i32) {
        if let Some(state) = self
            .replay_states
            .iter_mut()
            .find(|state| state.connection_id == connection_id)
        {
            state.preserved = true;
        }
    }

    /// Abandons a parked executor attempt without releasing session locks that
    /// earlier expressions in the statement successfully acquired.
    pub(crate) fn cancel_statement(&mut self, connection_id: i32, wait_graph: &mut LockManager) {
        let wait_owner = connection_wait_owner(connection_id);
        self.clear_wait(wait_owner);
        wait_graph.clear_wait(wait_owner);
        self.clear_replay_actions(connection_id);
        if let Some(state) = self
            .replay_states
            .iter_mut()
            .find(|state| state.connection_id == connection_id)
        {
            state.preserved = false;
            state.cursor = 0;
            state.replay_count = 0;
        }
    }

    pub(crate) fn drop_connection(&mut self, connection_id: i32) {
        self.clear_replay_actions(connection_id);
        if let Some(index) = self
            .replay_states
            .iter()
            .position(|state| state.connection_id == connection_id)
        {
            self.replay_states.swap_remove(index);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn acquire(
        &mut self,
        key: AdvisoryKey,
        owner: AdvisoryOwner,
        wait_owner: WaitOwner,
        mode: AdvisoryMode,
        try_only: bool,
        acquired_at: u64,
        wait_graph: &mut LockManager,
    ) -> Result<AdvisoryDecision, SqlError> {
        let replay_operation = AdvisoryReplayOperation::Acquire {
            key,
            mode,
            try_only,
        };
        if let AdvisoryOwner::Session(connection_id) = owner
            && let Some(result) = self.replay(connection_id, replay_operation)?
        {
            return Ok(if result {
                AdvisoryDecision::Acquired
            } else {
                AdvisoryDecision::Unavailable
            });
        }
        let own_index = self
            .locks
            .iter()
            .position(|lock| lock.key == key && lock.owner == owner && lock.mode == mode);
        if let Some(blocker) = self
            .locks
            .iter()
            .find(|lock| {
                lock.key == key
                    && lock.wait_owner != wait_owner
                    && (lock.mode == AdvisoryMode::Exclusive || mode == AdvisoryMode::Exclusive)
            })
            .map(|lock| lock.wait_owner)
        {
            self.clear_wait(wait_owner);
            if try_only {
                wait_graph.clear_wait(wait_owner);
                if let AdvisoryOwner::Session(connection_id) = owner {
                    self.record_replay(connection_id, replay_operation, false)?;
                }
                return Ok(AdvisoryDecision::Unavailable);
            }
            self.set_wait(AdvisoryWait {
                key,
                wait_owner,
                mode,
                started_at: crate::sql::datetime::now_micros(),
            })?;
            if let Err(error) = wait_graph.wait_for(wait_owner, blocker) {
                self.clear_wait(wait_owner);
                return Err(error);
            }
            return Ok(AdvisoryDecision::Waiting);
        }
        self.clear_wait(wait_owner);
        wait_graph.clear_wait(wait_owner);
        if let Some(index) = own_index {
            if matches!(owner, AdvisoryOwner::Session(_)) {
                self.locks[index].count =
                    self.locks[index].count.checked_add(1).ok_or_else(|| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "advisory lock acquisition count exceeds 4294967295"
                        )
                    })?;
            }
            if let AdvisoryOwner::Session(connection_id) = owner {
                self.record_replay(connection_id, replay_operation, true)?;
            }
            return Ok(AdvisoryDecision::Acquired);
        }
        self.locks
            .push(AdvisoryLock {
                key,
                owner,
                wait_owner,
                mode,
                count: 1,
                acquired_at,
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::OUT_OF_MEMORY,
                    "out of shared memory for advisory locks ({} entries)",
                    self.locks.capacity()
                )
            })?;
        if let AdvisoryOwner::Session(connection_id) = owner {
            self.record_replay(connection_id, replay_operation, true)?;
        }
        Ok(AdvisoryDecision::Acquired)
    }

    pub(crate) fn unlock_session(
        &mut self,
        connection_id: i32,
        key: AdvisoryKey,
        mode: AdvisoryMode,
        wait_graph: &mut LockManager,
    ) -> Result<bool, SqlError> {
        let operation = AdvisoryReplayOperation::Unlock { key, mode };
        if let Some(result) = self.replay(connection_id, operation)? {
            return Ok(result);
        }
        let Some(index) = self.locks.iter().position(|lock| {
            lock.key == key
                && lock.owner == AdvisoryOwner::Session(connection_id)
                && lock.mode == mode
        }) else {
            self.record_replay(connection_id, operation, false)?;
            return Ok(false);
        };
        if self.locks[index].count > 1 {
            self.locks[index].count -= 1;
        } else {
            let wait_owner = self.locks[index].wait_owner;
            self.locks.swap_remove(index);
            wait_graph.resource_released(wait_owner);
        }
        self.record_replay(connection_id, operation, true)?;
        Ok(true)
    }

    pub(crate) fn unlock_all_session(
        &mut self,
        connection_id: i32,
        wait_graph: &mut LockManager,
    ) -> Result<(), SqlError> {
        if self
            .replay(connection_id, AdvisoryReplayOperation::UnlockAll)?
            .is_some()
        {
            return Ok(());
        }
        let wait_owner = connection_wait_owner(connection_id);
        let mut index = 0usize;
        let mut changed = false;
        while index < self.locks.len() {
            if self.locks[index].owner == AdvisoryOwner::Session(connection_id) {
                self.locks.swap_remove(index);
                changed = true;
            } else {
                index += 1;
            }
        }
        self.clear_wait(wait_owner);
        wait_graph.clear_wait(wait_owner);
        if changed {
            wait_graph.resource_released(wait_owner);
        }
        self.record_replay(connection_id, AdvisoryReplayOperation::UnlockAll, true)
    }

    pub(crate) fn release_transaction(
        &mut self,
        transaction_id: u32,
        wait_owner: WaitOwner,
        wait_graph: &mut LockManager,
    ) {
        let mut index = 0usize;
        let mut changed = false;
        while index < self.locks.len() {
            if self.locks[index].owner == AdvisoryOwner::Transaction(transaction_id) {
                self.locks.swap_remove(index);
                changed = true;
            } else {
                index += 1;
            }
        }
        self.clear_wait(wait_owner);
        wait_graph.clear_wait(wait_owner);
        if changed {
            wait_graph.resource_released(wait_owner);
        }
    }

    pub(crate) fn rollback_to(
        &mut self,
        transaction_id: u32,
        wait_owner: WaitOwner,
        mark: u64,
        wait_graph: &mut LockManager,
    ) {
        let mut index = 0usize;
        let mut changed = false;
        while index < self.locks.len() {
            if self.locks[index].owner == AdvisoryOwner::Transaction(transaction_id)
                && self.locks[index].acquired_at > mark
            {
                self.locks.swap_remove(index);
                changed = true;
            } else {
                index += 1;
            }
        }
        self.clear_wait(wait_owner);
        wait_graph.clear_wait(wait_owner);
        if changed {
            wait_graph.resource_released(wait_owner);
        }
    }

    pub(crate) fn rebind_transaction_wait_owner(
        &mut self,
        transaction_id: u32,
        from: WaitOwner,
        to: WaitOwner,
    ) {
        for lock in self.locks.iter_mut().filter(|lock| {
            lock.owner == AdvisoryOwner::Transaction(transaction_id) && lock.wait_owner == from
        }) {
            lock.wait_owner = to;
        }
    }

    pub(crate) fn visit(&self, mut visit: impl FnMut(AdvisoryLockView)) {
        for lock in self.locks.iter() {
            visit(AdvisoryLockView {
                key: lock.key,
                wait_owner: lock.wait_owner,
                mode: lock.mode,
                granted: true,
                wait_start: None,
            });
        }
        for wait in self.waits.iter() {
            visit(AdvisoryLockView {
                key: wait.key,
                wait_owner: wait.wait_owner,
                mode: wait.mode,
                granted: false,
                wait_start: Some(wait.started_at),
            });
        }
    }

    pub(crate) fn view_count(&self) -> usize {
        self.locks.len() + self.waits.len()
    }

    pub(crate) fn visit_transaction(
        &self,
        transaction_id: u32,
        mut visit: impl FnMut(AdvisoryKey, AdvisoryMode),
    ) {
        for lock in self
            .locks
            .iter()
            .filter(|lock| lock.owner == AdvisoryOwner::Transaction(transaction_id))
        {
            visit(lock.key, lock.mode);
        }
    }

    pub(crate) fn blocker_pid_count(&self, waiter_pid: i32) -> usize {
        let wait_owner = connection_wait_owner(waiter_pid);
        let Some(request) = self.waits.iter().find(|wait| wait.wait_owner == wait_owner) else {
            return 0;
        };
        self.locks
            .iter()
            .enumerate()
            .filter(|(index, lock)| {
                lock.key == request.key
                    && lock.wait_owner != wait_owner
                    && (lock.mode == AdvisoryMode::Exclusive
                        || request.mode == AdvisoryMode::Exclusive)
                    && wait_owner_pid(lock.wait_owner).is_some()
                    && !self.locks.iter().take(*index).any(|prior| {
                        prior.key == request.key
                            && prior.wait_owner == lock.wait_owner
                            && (prior.mode == AdvisoryMode::Exclusive
                                || request.mode == AdvisoryMode::Exclusive)
                    })
            })
            .count()
    }

    pub(crate) fn blocker_pids(&self, waiter_pid: i32, output: &mut [i32]) -> usize {
        let wait_owner = connection_wait_owner(waiter_pid);
        let Some(request) = self.waits.iter().find(|wait| wait.wait_owner == wait_owner) else {
            return 0;
        };
        let mut count = 0usize;
        for lock in self.locks.iter() {
            if lock.key != request.key
                || lock.wait_owner == wait_owner
                || lock.mode != AdvisoryMode::Exclusive && request.mode != AdvisoryMode::Exclusive
            {
                continue;
            }
            let Some(pid) = wait_owner_pid(lock.wait_owner) else {
                continue;
            };
            if output[..count].contains(&pid) {
                continue;
            }
            if count == output.len() {
                break;
            }
            output[count] = pid;
            count += 1;
        }
        count
    }

    fn set_wait(&mut self, request: AdvisoryWait) -> Result<(), SqlError> {
        if let Some(wait) = self
            .waits
            .iter_mut()
            .find(|wait| wait.wait_owner == request.wait_owner)
        {
            if wait.key != request.key || wait.mode != request.mode {
                *wait = request;
            }
            return Ok(());
        }
        self.waits.push(request).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "advisory lock wait registry is full ({} connections)",
                self.waits.capacity()
            )
        })
    }

    fn clear_wait(&mut self, wait_owner: WaitOwner) {
        if let Some(index) = self
            .waits
            .iter()
            .position(|wait| wait.wait_owner == wait_owner)
        {
            self.waits.swap_remove(index);
        }
    }

    fn replay(
        &mut self,
        connection_id: i32,
        operation: AdvisoryReplayOperation,
    ) -> Result<Option<bool>, SqlError> {
        let Some(state_index) = self
            .replay_states
            .iter()
            .position(|state| state.connection_id == connection_id)
        else {
            return Ok(None);
        };
        let state = self.replay_states[state_index];
        if state.cursor >= state.replay_count {
            return Ok(None);
        }
        let action = self
            .replay_actions
            .iter()
            .filter(|action| action.connection_id == connection_id)
            .nth(state.cursor)
            .copied()
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "advisory lock statement replay is incomplete"
                )
            })?;
        if action.operation != operation {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "advisory lock calls changed while a blocked statement was resumed"
            ));
        }
        self.replay_states[state_index].cursor += 1;
        Ok(Some(action.result))
    }

    fn record_replay(
        &mut self,
        connection_id: i32,
        operation: AdvisoryReplayOperation,
        result: bool,
    ) -> Result<(), SqlError> {
        if !self
            .replay_states
            .iter()
            .any(|state| state.connection_id == connection_id)
        {
            return Ok(());
        }
        self.replay_actions
            .push(AdvisoryReplayAction {
                connection_id,
                operation,
                result,
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "advisory lock statement replay exceeds {} calls",
                    self.replay_actions.capacity()
                )
            })
    }

    fn clear_replay_actions(&mut self, connection_id: i32) {
        let mut index = 0usize;
        while index < self.replay_actions.len() {
            if self.replay_actions[index].connection_id == connection_id {
                for cursor in index + 1..self.replay_actions.len() {
                    self.replay_actions[cursor - 1] = self.replay_actions[cursor];
                }
                self.replay_actions.pop();
            } else {
                index += 1;
            }
        }
    }
}

fn mode_bit(strength: LockStrength) -> u8 {
    match strength {
        LockStrength::Update => 1 << 0,
        LockStrength::NoKeyUpdate => 1 << 1,
        LockStrength::Share => 1 << 2,
        LockStrength::KeyShare => 1 << 3,
    }
}

fn modes_mask(sequences: &[u64; 4]) -> u8 {
    sequences
        .iter()
        .enumerate()
        .fold(0, |mask, (index, sequence)| {
            mask | u8::from(*sequence != 0) << index
        })
}

fn conflicts(left: u8, right: u8) -> bool {
    for left_bit in [1 << 0, 1 << 1, 1 << 2, 1 << 3] {
        if left & left_bit == 0 {
            continue;
        }
        for right_bit in [1 << 0, 1 << 1, 1 << 2, 1 << 3] {
            if right & right_bit != 0 && pair_conflicts(left_bit, right_bit) {
                return true;
            }
        }
    }
    false
}

fn pair_conflicts(left: u8, right: u8) -> bool {
    left == 1 || right == 1 || matches!((left, right), (2, 2) | (2, 4) | (4, 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_wait_modes_and_deadlocks() {
        let mut budget = Budget::new(16 << 10);
        let mut locks = LockManager::new(&mut budget, 16, 4).unwrap();
        assert_eq!(
            locks
                .acquire(0, 1, 10, 10, LockStrength::KeyShare, LockWait::Wait, 1)
                .unwrap(),
            LockDecision::Acquired
        );
        assert_eq!(
            locks
                .acquire(0, 1, 20, 20, LockStrength::NoKeyUpdate, LockWait::Wait, 2)
                .unwrap(),
            LockDecision::Acquired
        );
        assert_eq!(
            locks
                .acquire(0, 1, 30, 30, LockStrength::Share, LockWait::SkipLocked, 3)
                .unwrap(),
            LockDecision::Skipped
        );
        let error = locks
            .acquire(0, 1, 30, 30, LockStrength::Update, LockWait::NoWait, 4)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::LOCK_NOT_AVAILABLE);

        assert_eq!(
            locks
                .acquire(0, 2, 10, 10, LockStrength::Update, LockWait::Wait, 5)
                .unwrap(),
            LockDecision::Acquired
        );
        assert_eq!(
            locks
                .acquire(0, 3, 20, 20, LockStrength::Update, LockWait::Wait, 6)
                .unwrap(),
            LockDecision::Acquired
        );
        assert_eq!(
            locks
                .acquire(0, 3, 10, 10, LockStrength::Update, LockWait::Wait, 7)
                .unwrap(),
            LockDecision::Waiting
        );
        let deadlock = locks
            .acquire(0, 2, 20, 20, LockStrength::Update, LockWait::Wait, 8)
            .unwrap_err();
        assert_eq!(deadlock.sqlstate, sqlstate::DEADLOCK_DETECTED);
    }

    #[test]
    fn advisory_namespaces_reentrancy_shared_waits_and_deadlocks() {
        let mut budget = Budget::new(32 << 10);
        let mut graph = LockManager::new(&mut budget, 4, 4).unwrap();
        let mut advisory = AdvisoryLockManager::new(&mut budget, 16, 4).unwrap();
        let first = connection_wait_owner(11);
        let second = connection_wait_owner(22);
        let bigint = AdvisoryKey::from_bigint(1);
        let pair = AdvisoryKey::from_ints(0, 1);

        for sequence in 1..=2 {
            assert_eq!(
                advisory
                    .acquire(
                        bigint,
                        AdvisoryOwner::Session(11),
                        first,
                        AdvisoryMode::Exclusive,
                        false,
                        sequence,
                        &mut graph,
                    )
                    .unwrap(),
                AdvisoryDecision::Acquired
            );
        }
        assert_eq!(
            advisory
                .acquire(
                    pair,
                    AdvisoryOwner::Session(22),
                    second,
                    AdvisoryMode::Exclusive,
                    true,
                    3,
                    &mut graph,
                )
                .unwrap(),
            AdvisoryDecision::Acquired,
            "the bigint and two-int key spaces are disjoint"
        );
        assert!(
            advisory
                .unlock_session(11, bigint, AdvisoryMode::Exclusive, &mut graph)
                .unwrap()
        );
        assert_eq!(advisory.view_count(), 2, "one reentrant hold remains");

        let other = AdvisoryKey::from_bigint(2);
        assert_eq!(
            advisory
                .acquire(
                    other,
                    AdvisoryOwner::Session(22),
                    second,
                    AdvisoryMode::Exclusive,
                    false,
                    4,
                    &mut graph,
                )
                .unwrap(),
            AdvisoryDecision::Acquired
        );
        assert_eq!(
            advisory
                .acquire(
                    other,
                    AdvisoryOwner::Transaction(101),
                    first,
                    AdvisoryMode::Shared,
                    false,
                    5,
                    &mut graph,
                )
                .unwrap(),
            AdvisoryDecision::Waiting
        );
        let deadlock = advisory
            .acquire(
                bigint,
                AdvisoryOwner::Transaction(202),
                second,
                AdvisoryMode::Exclusive,
                false,
                6,
                &mut graph,
            )
            .unwrap_err();
        assert_eq!(deadlock.sqlstate, sqlstate::DEADLOCK_DETECTED);
        assert_eq!(advisory.blocker_pid_count(22), 0);
    }

    #[test]
    fn advisory_statement_replay_order_survives_other_session_cleanup() {
        let mut budget = Budget::new(32 << 10);
        let mut graph = LockManager::new(&mut budget, 4, 4).unwrap();
        let mut advisory = AdvisoryLockManager::new(&mut budget, 16, 4).unwrap();
        let a = connection_wait_owner(11);
        let b = connection_wait_owner(22);

        advisory.begin_statement(11).unwrap();
        advisory
            .acquire(
                AdvisoryKey::from_bigint(1),
                AdvisoryOwner::Session(11),
                a,
                AdvisoryMode::Exclusive,
                false,
                1,
                &mut graph,
            )
            .unwrap();
        advisory.begin_statement(22).unwrap();
        for (key, sequence) in [(2, 2), (3, 3)] {
            advisory
                .acquire(
                    AdvisoryKey::from_bigint(key),
                    AdvisoryOwner::Session(22),
                    b,
                    AdvisoryMode::Exclusive,
                    false,
                    sequence,
                    &mut graph,
                )
                .unwrap();
        }

        advisory.begin_statement(11).unwrap();
        advisory.preserve_statement(22);
        advisory.begin_statement(22).unwrap();
        for key in [2, 3] {
            assert_eq!(
                advisory
                    .acquire(
                        AdvisoryKey::from_bigint(key),
                        AdvisoryOwner::Session(22),
                        b,
                        AdvisoryMode::Exclusive,
                        false,
                        4,
                        &mut graph,
                    )
                    .unwrap(),
                AdvisoryDecision::Acquired
            );
        }
        assert_eq!(advisory.view_count(), 3, "replay did not add holds");
    }
}
