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

#[derive(Clone, Copy)]
struct RowLock {
    table: u32,
    rowid: u64,
    owner: u32,
    /// Acquisition sequence for each PostgreSQL row-lock mode. Zero means
    /// absent. Keeping the modes independently makes savepoint rollback able
    /// to remove only locks or upgrades acquired by the rolled-back
    /// subtransaction.
    modes: [u64; 4],
}

#[derive(Clone, Copy)]
struct WaitEdge {
    waiter: u32,
    blocker: u32,
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

    pub(crate) fn acquire(
        &mut self,
        table: usize,
        rowid: u64,
        owner: u32,
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
                    self.set_wait(owner, blocker.owner)?;
                    Ok(LockDecision::Waiting)
                }
            };
        }
        self.clear_wait(owner);
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
    pub(crate) fn wait_for(&mut self, waiter: u32, blocker: u32) -> Result<(), SqlError> {
        self.set_wait(waiter, blocker)
    }

    pub(crate) fn release(&mut self, owner: u32) {
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
        self.clear_wait(owner);
        // Waiters on this owner retry against the registry rather than keeping
        // stale graph edges after the blocker disappears.
        index = 0;
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

    pub(crate) fn rollback_to(&mut self, owner: u32, mark: u64) {
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
        self.clear_wait(owner);
        if changed {
            index = 0;
            while index < self.waits.len() {
                if self.waits[index].blocker == owner {
                    self.waits.swap_remove(index);
                } else {
                    index += 1;
                }
            }
            self.generation = self.generation.wrapping_add(1).max(1);
        }
    }

    pub(crate) fn resource_released(&mut self, owner: u32) {
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

    fn set_wait(&mut self, waiter: u32, blocker: u32) -> Result<(), SqlError> {
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

    fn clear_wait(&mut self, owner: u32) {
        if let Some(index) = self.waits.iter().position(|edge| edge.waiter == owner) {
            self.waits.swap_remove(index);
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
                .acquire(0, 1, 10, LockStrength::KeyShare, LockWait::Wait, 1)
                .unwrap(),
            LockDecision::Acquired
        );
        assert_eq!(
            locks
                .acquire(0, 1, 20, LockStrength::NoKeyUpdate, LockWait::Wait, 2)
                .unwrap(),
            LockDecision::Acquired
        );
        assert_eq!(
            locks
                .acquire(0, 1, 30, LockStrength::Share, LockWait::SkipLocked, 3)
                .unwrap(),
            LockDecision::Skipped
        );
        let error = locks
            .acquire(0, 1, 30, LockStrength::Update, LockWait::NoWait, 4)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::LOCK_NOT_AVAILABLE);

        assert_eq!(
            locks
                .acquire(0, 2, 10, LockStrength::Update, LockWait::Wait, 5)
                .unwrap(),
            LockDecision::Acquired
        );
        assert_eq!(
            locks
                .acquire(0, 3, 20, LockStrength::Update, LockWait::Wait, 6)
                .unwrap(),
            LockDecision::Acquired
        );
        assert_eq!(
            locks
                .acquire(0, 3, 10, LockStrength::Update, LockWait::Wait, 7)
                .unwrap(),
            LockDecision::Waiting
        );
        let deadlock = locks
            .acquire(0, 2, 20, LockStrength::Update, LockWait::Wait, 8)
            .unwrap_err();
        assert_eq!(deadlock.sqlstate, sqlstate::DEADLOCK_DETECTED);
    }
}
