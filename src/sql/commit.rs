//! Startup-owned transaction buffers retained through object-store publication.

use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql_err;

use super::ExecutionIdentity;
use super::eval::{SqlError, sqlstate};
use super::guc::{GucState, SeqSession};
use super::txn::TxnState;
use core::num::NonZeroU32;

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PublicationPoint {
    DefinitionsInstalled,
    RowInstalled { table: usize, rowid: u64 },
    DataInstalled,
    Visible,
    RollbackRestored,
}

#[cfg(test)]
pub(super) type PublicationObserver =
    fn(&super::Engine, PublicationPoint, u32) -> Result<(), SqlError>;

#[derive(Clone, Copy)]
pub(super) struct Publication {
    coordinator: NonZeroU32,
    prepared: Option<(NonZeroU32, bool)>,
    pub lsn: u64,
}

impl Publication {
    fn new(coordinator: u32, prepared: Option<(u32, bool)>, lsn: u64) -> Result<Self, SqlError> {
        let invalid = || {
            sql_err!(
                sqlstate::TRANSACTION_RESOLUTION_UNKNOWN,
                "durable publication has an invalid transaction owner"
            )
        };
        let coordinator = NonZeroU32::new(coordinator).ok_or_else(invalid)?;
        let prepared = prepared
            .map(|(transaction, committed)| -> Result<_, SqlError> {
                let transaction = NonZeroU32::new(transaction)
                    .filter(|transaction| *transaction != coordinator)
                    .ok_or_else(invalid)?;
                Ok((transaction, committed))
            })
            .transpose()?;
        Ok(Self {
            coordinator,
            prepared,
            lsn,
        })
    }

    fn owns(self, transaction: u32) -> bool {
        self.coordinator.get() == transaction
            || self
                .prepared
                .is_some_and(|(prepared, _)| prepared.get() == transaction)
    }

    pub fn transactions(self) -> impl Iterator<Item = (u32, bool)> {
        core::iter::once((self.coordinator.get(), true)).chain(
            self.prepared
                .map(|(transaction, committed)| (transaction.get(), committed)),
        )
    }
}

#[derive(Clone, Copy)]
pub(super) enum PublicationState {
    Idle,
    Installing(Publication),
    RecoveryRequired(Publication),
}

impl PublicationState {
    pub fn begin(
        &mut self,
        coordinator: u32,
        prepared: Option<(u32, bool)>,
        lsn: u64,
    ) -> Result<(), SqlError> {
        if !matches!(self, Self::Idle) {
            return Err(sql_err!(
                sqlstate::TRANSACTION_RESOLUTION_UNKNOWN,
                "durable publication requires recovery"
            ));
        }
        *self = Self::Installing(Publication::new(coordinator, prepared, lsn)?);
        Ok(())
    }

    pub fn owns(self, transaction: u32) -> bool {
        match self {
            Self::Idle => false,
            Self::Installing(publication) | Self::RecoveryRequired(publication) => {
                publication.owns(transaction)
            }
        }
    }

    pub fn complete(&mut self) -> Publication {
        let Self::Installing(publication) = *self else {
            panic!("only a fully installed publication can release its owners");
        };
        *self = Self::Idle;
        publication
    }

    pub fn require_recovery(&mut self) {
        if let Self::Installing(publication) = *self {
            *self = Self::RecoveryRequired(publication);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ticket {
    slot: usize,
    generation: u64,
}

pub(super) struct Work {
    pub transaction: TxnState,
    pub guc: GucState,
    pub identity: ExecutionIdentity,
}

#[allow(
    clippy::large_enum_variant,
    reason = "startup-reserved transaction data and outcomes share one fixed slot"
)]
enum Phase {
    Free(Work),
    Reserved(Work),
    Staged(Work),
    Working,
    Ready {
        work: Work,
        result: Result<(), SqlError>,
    },
}

struct Slot {
    generation: u64,
    phase: Phase,
}

pub(super) struct Queue {
    slots: FixedVec<Slot>,
}

impl Queue {
    pub(super) fn budget_bytes(config: &Config) -> usize {
        config.commit_queue_slots.saturating_mul(
            size_of::<Slot>()
                + TxnState::budget_bytes_with_config(config)
                + GucState::extra_savepoint_budget_bytes(config.max_savepoints_per_transaction)
                + SeqSession::extra_budget_bytes(config.max_sequences),
        )
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        let mut slots = FixedVec::new(budget, "commit_queue_slots", config.commit_queue_slots)?;
        for _ in 0..config.commit_queue_slots {
            slots
                .push(Slot {
                    generation: 0,
                    phase: Phase::Free(Work {
                        transaction: TxnState::new_with_config(budget, config)?,
                        guc: GucState::new_with_capacities(
                            budget,
                            config.max_sequences,
                            config.max_savepoints_per_transaction,
                            config.transaction_mode,
                        )?,
                        identity: ExecutionIdentity::new(0, crate::storage::DatabaseOid::POSTGRES),
                    }),
                })
                .expect("startup commit queue slots");
        }
        Ok(Self { slots })
    }

    pub(super) fn reserve(
        &mut self,
        guc: &GucState,
        identity: ExecutionIdentity,
    ) -> Result<Ticket, SqlError> {
        let (index, slot) = self
            .slots
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| matches!(slot.phase, Phase::Free(_)))
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "commit_queue_slots exhausted"
                )
            })?;
        let generation = slot.generation.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "commit queue ticket generation exhausted"
            )
        })?;
        let Phase::Free(mut work) = core::mem::replace(&mut slot.phase, Phase::Working) else {
            unreachable!("selected free commit slot");
        };
        if let Err(error) = guc.capture_commit_context(&mut work.guc) {
            slot.phase = Phase::Free(work);
            return Err(error);
        }
        work.identity = identity;
        slot.generation = generation;
        slot.phase = Phase::Reserved(work);
        Ok(Ticket {
            slot: index,
            generation,
        })
    }

    fn slot(&mut self, ticket: Ticket) -> Result<&mut Slot, SqlError> {
        self.slots
            .get_mut(ticket.slot)
            .filter(|slot| slot.generation == ticket.generation)
            .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "stale commit queue ticket"))
    }

    pub(super) fn cancel_reservation(&mut self, ticket: Ticket) {
        let slot = self.slot(ticket).expect("reserved commit ticket");
        let Phase::Reserved(work) = core::mem::replace(&mut slot.phase, Phase::Working) else {
            panic!("only an unlogged reservation may be canceled");
        };
        slot.phase = Phase::Free(work);
    }

    pub(super) fn stage(&mut self, ticket: Ticket, transaction: &mut TxnState) {
        let slot = self.slot(ticket).expect("reserved commit ticket");
        let Phase::Reserved(mut work) = core::mem::replace(&mut slot.phase, Phase::Working) else {
            panic!("commit staging owns its reservation");
        };
        let control = transaction.commit_control();
        let mut staged = transaction
            .staged_commit()
            .expect("journal commit is staged");
        assert!(staged.queue.is_none());
        core::mem::swap(transaction, &mut work.transaction);
        staged.queue = Some(ticket);
        transaction.install_commit_wait(control, staged);
        slot.phase = Phase::Staged(work);
    }

    pub(super) fn first_staged_lsn(&self) -> Option<u64> {
        self.slots
            .iter()
            .filter_map(|slot| {
                let Phase::Staged(work) = &slot.phase else {
                    return None;
                };
                Some(
                    work.transaction
                        .staged_commit()
                        .expect("queued log boundary")
                        .lsn,
                )
            })
            .min()
    }

    pub(super) fn next_ready(&self, durable_lsn: u64) -> Option<Ticket> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let Phase::Staged(work) = &slot.phase else {
                    return None;
                };
                let staged = work
                    .transaction
                    .staged_commit()
                    .expect("queued commit has a log boundary");
                Some((
                    staged.lsn,
                    Ticket {
                        slot: index,
                        generation: slot.generation,
                    },
                ))
            })
            .min_by_key(|(lsn, _)| *lsn)
            .filter(|(lsn, _)| *lsn <= durable_lsn)
            .map(|(_, ticket)| ticket)
    }

    pub(super) fn take_work(&mut self, ticket: Ticket) -> Work {
        let slot = self.slot(ticket).expect("ready commit ticket");
        let Phase::Staged(work) = core::mem::replace(&mut slot.phase, Phase::Working) else {
            panic!("commit worker owns staged data");
        };
        work
    }

    pub(super) fn finish_work(&mut self, ticket: Ticket, work: Work, result: Result<(), SqlError>) {
        let slot = self.slot(ticket).expect("working commit ticket");
        assert!(matches!(slot.phase, Phase::Working));
        slot.phase = Phase::Ready { work, result };
    }

    pub(super) fn receipt(
        &mut self,
        ticket: Ticket,
        transaction: &mut TxnState,
        guc: &GucState,
    ) -> Result<bool, SqlError> {
        let slot = self.slot(ticket)?;
        match &slot.phase {
            Phase::Ready {
                result: Err(error), ..
            } => {
                return Err(SqlError {
                    sqlstate: error.sqlstate,
                    message: error.message,
                });
            }
            Phase::Ready { result: Ok(()), .. } => {}
            Phase::Staged(_) | Phase::Working => return Ok(false),
            _ => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "commit receipt has no staged owner"
                ));
            }
        }
        let Phase::Ready {
            mut work,
            result: Ok(()),
        } = core::mem::replace(&mut slot.phase, Phase::Working)
        else {
            unreachable!("checked completion");
        };
        assert!(
            !work.transaction.is_active(),
            "completed commit released transaction data"
        );
        core::mem::swap(transaction, &mut work.transaction);
        work.transaction.clear();
        guc.commit_transaction();
        slot.phase = Phase::Free(work);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::txn::{CommitBarrier, CommitTarget, TxnMode};

    fn config() -> Config {
        let mut config = Config::default_dev();
        config.commit_queue_slots = 2;
        config.txn_rows = 4;
        config.max_ddl_per_transaction = 4;
        config.max_sequences = 2;
        config.max_savepoints_per_transaction = 2;
        config
    }

    #[test]
    fn parallel_commit_queue_reserves_exact_budget_and_rejects_exhaustion() {
        let config = config();
        let bytes = Queue::budget_bytes(&config);
        let mut budget = Budget::new(bytes);
        let mut queue = Queue::new(&config, &mut budget).unwrap();
        assert_eq!(budget.used(), bytes);
        let mut caller_budget = Budget::new(
            GucState::extra_savepoint_budget_bytes(config.max_savepoints_per_transaction)
                + SeqSession::extra_budget_bytes(config.max_sequences),
        );
        let guc = GucState::new_with_capacities(
            &mut caller_budget,
            config.max_sequences,
            config.max_savepoints_per_transaction,
            config.transaction_mode,
        )
        .unwrap();
        let identity = ExecutionIdentity::new(7, crate::storage::DatabaseOid::POSTGRES);
        crate::mem::guard::forbid_alloc(|| {
            let first = queue.reserve(&guc, identity).unwrap();
            let second = queue.reserve(&guc, identity).unwrap();
            assert_eq!(
                queue.reserve(&guc, identity).unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            queue.cancel_reservation(first);
            let replacement = queue.reserve(&guc, identity).unwrap();
            assert_eq!(first.slot, replacement.slot);
            assert_ne!(first.generation, replacement.generation);
            assert!(queue.slot(first).is_err());
            queue.cancel_reservation(second);
            queue.cancel_reservation(replacement);
        });
        let mut short = Budget::new(bytes - 1);
        assert!(Queue::new(&config, &mut short).is_err());
    }

    #[test]
    fn parallel_commit_queue_promotes_in_log_order_and_retains_unclaimed_receipts() {
        let config = config();
        let mut budget = Budget::new(
            Queue::budget_bytes(&config)
                + 2 * TxnState::budget_bytes_with_config(&config)
                + GucState::extra_savepoint_budget_bytes(config.max_savepoints_per_transaction)
                + SeqSession::extra_budget_bytes(config.max_sequences),
        );
        let mut queue = Queue::new(&config, &mut budget).unwrap();
        let guc = GucState::new_with_capacities(
            &mut budget,
            config.max_sequences,
            config.max_savepoints_per_transaction,
            config.transaction_mode,
        )
        .unwrap();
        let mut first = TxnState::new_with_config(&mut budget, &config).unwrap();
        let mut second = TxnState::new_with_config(&mut budget, &config).unwrap();
        first.txid = 7;
        first.mode = TxnMode::Implicit;
        second.txid = 8;
        second.mode = TxnMode::Implicit;
        let identity = ExecutionIdentity::new(7, crate::storage::DatabaseOid::POSTGRES);
        crate::mem::guard::forbid_alloc(|| {
            let late = queue.reserve(&guc, identity).unwrap();
            let early = queue.reserve(&guc, identity).unwrap();
            first.stage_commit(20, CommitTarget::Ordinary, CommitBarrier::Journal);
            second.stage_commit(10, CommitTarget::Ordinary, CommitBarrier::Journal);
            queue.stage(late, &mut first);
            queue.stage(early, &mut second);
            assert_eq!(first.staged_commit().unwrap().queue, Some(late));
            assert_eq!(queue.first_staged_lsn(), Some(10));
            assert_eq!(queue.next_ready(9), None);
            assert_eq!(queue.next_ready(20), Some(early));
            assert!(!queue.receipt(late, &mut first, &guc).unwrap());
            let mut work = queue.take_work(early);
            assert_eq!(work.transaction.txid, 8);
            assert_eq!(work.transaction.staged_commit().unwrap().queue, None);
            work.transaction.clear();
            queue.finish_work(early, work, Ok(()));
            assert_eq!(queue.next_ready(20), Some(late));
            let mut work = queue.take_work(late);
            work.transaction.clear();
            queue.finish_work(late, work, Ok(()));
            assert!(queue.reserve(&guc, identity).is_err());
            // Receipt order need not equal publication order; the client may
            // disconnect after its data has already become durable and visible.
            assert!(queue.receipt(late, &mut first, &guc).unwrap());
            assert!(!first.is_active());
            let reused = queue.reserve(&guc, identity).unwrap();
            queue.cancel_reservation(reused);
            assert!(queue.receipt(early, &mut second, &guc).unwrap());
            assert!(!second.is_active());
        });
    }

    #[test]
    fn parallel_commit_queue_preserves_failed_completion_until_recovery() {
        let config = config();
        let mut budget = Budget::new(
            Queue::budget_bytes(&config)
                + TxnState::budget_bytes_with_config(&config)
                + GucState::extra_savepoint_budget_bytes(config.max_savepoints_per_transaction)
                + SeqSession::extra_budget_bytes(config.max_sequences),
        );
        let mut queue = Queue::new(&config, &mut budget).unwrap();
        let guc = GucState::new_with_capacities(
            &mut budget,
            config.max_sequences,
            config.max_savepoints_per_transaction,
            config.transaction_mode,
        )
        .unwrap();
        let mut transaction = TxnState::new_with_config(&mut budget, &config).unwrap();
        transaction.txid = 3;
        transaction.mode = TxnMode::Implicit;
        let identity = ExecutionIdentity::new(3, crate::storage::DatabaseOid::POSTGRES);
        crate::mem::guard::forbid_alloc(|| {
            let ticket = queue.reserve(&guc, identity).unwrap();
            transaction.stage_commit(10, CommitTarget::Ordinary, CommitBarrier::Journal);
            queue.stage(ticket, &mut transaction);
            let work = queue.take_work(ticket);
            queue.finish_work(
                ticket,
                work,
                Err(sql_err!(
                    sqlstate::TRANSACTION_RESOLUTION_UNKNOWN,
                    "recovery required"
                )),
            );
            for _ in 0..2 {
                assert_eq!(
                    queue
                        .receipt(ticket, &mut transaction, &guc)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::TRANSACTION_RESOLUTION_UNKNOWN
                );
                assert_eq!(transaction.staged_commit().unwrap().queue, Some(ticket));
            }
        });
    }
}
