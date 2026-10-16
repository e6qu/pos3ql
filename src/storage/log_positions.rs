//! Checked provisional record positions; durable commit ordering belongs to WAL.

use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::mem::{Budget, BudgetError};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

pub(super) struct LogPositions(AtomicU64);

/// A position reservation is resolved at WAL staging, so exhaustion follows
/// the same catalog/row cleanup paths as an encoding or staging-capacity error.
#[must_use = "a log position reservation must be resolved by journal staging"]
pub(crate) struct ReservedLogPosition(Option<u64>);

impl ReservedLogPosition {
    pub(crate) fn resolve(self) -> Result<u64, SqlError> {
        self.0
            .ok_or_else(|| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "WAL LSN space exhausted"))
    }
}

impl LogPositions {
    pub(super) const fn budget_bytes() -> usize {
        size_of::<Self>()
    }

    pub(super) fn new(budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw(Self::budget_bytes(), "storage log position controls")?;
        Ok(Self(AtomicU64::new(0)))
    }

    pub(super) fn reserve(&self) -> ReservedLogPosition {
        let mut current = self.0.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(1) else {
                return ReservedLogPosition(None);
            };
            match self
                .0
                .compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return ReservedLogPosition(Some(next)),
                Err(observed) => current = observed,
            }
        }
    }

    pub(super) fn current(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    pub(super) fn observe(&self, value: u64) {
        self.0.fetch_max(value, Ordering::AcqRel);
    }

    /// Replay may revisit earlier records; exclusive access excludes allocators.
    pub(super) fn restore(&mut self, value: u64) {
        *self.0.get_mut() = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_log_positions_are_unique_under_concurrent_reservations() {
        let mut budget = Budget::new(LogPositions::budget_bytes());
        let positions = LogPositions::new(&mut budget).unwrap();
        let ready = std::sync::Barrier::new(5);
        let positions = &positions;
        let ready = &ready;
        let mut all = [0; 256];
        std::thread::scope(|scope| {
            let workers: [_; 4] = core::array::from_fn(|_| {
                scope.spawn(move || {
                    ready.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        let mut values = [0; 64];
                        for value in &mut values {
                            *value = positions.reserve().resolve()?;
                        }
                        Ok::<_, SqlError>(values)
                    })
                })
            });
            ready.wait();
            for (ordinal, worker) in workers.into_iter().enumerate() {
                all[ordinal * 64..(ordinal + 1) * 64]
                    .copy_from_slice(&worker.join().unwrap().unwrap());
            }
        });
        crate::mem::guard::forbid_alloc(|| {
            all.sort_unstable();
            for (ordinal, position) in all.into_iter().enumerate() {
                assert_eq!(position, ordinal as u64 + 1);
            }
            assert_eq!(positions.current(), 256);
        });
    }

    #[test]
    fn shared_log_positions_exhaust_without_reuse_and_observation_never_regresses() {
        let mut budget = Budget::new(LogPositions::budget_bytes());
        let mut positions = LogPositions::new(&mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            positions.observe(11);
            positions.observe(4);
            assert_eq!(positions.reserve().resolve().unwrap(), 12);
            positions.observe(12);
            positions.observe(9);
            assert_eq!(positions.current(), 12);
            positions.restore(u64::MAX - 1);
            assert_eq!(positions.reserve().resolve().unwrap(), u64::MAX);
            for _ in 0..2 {
                let error = positions.reserve().resolve().unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                assert_eq!(error.message.as_str(), "WAL LSN space exhausted");
                assert_eq!(positions.current(), u64::MAX);
            }
            positions.restore(7);
            positions.observe(5);
            assert_eq!(positions.reserve().resolve().unwrap(), 8);
        });
    }
}
