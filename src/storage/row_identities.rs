use std::sync::atomic::{AtomicU64, Ordering};

use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

/// The persisted next identity is also the exhaustion marker. Reserving MAX
/// prevents arithmetic wrap without changing the checkpoint's unsigned field.
pub(super) struct RowIdentities(AtomicU64);

impl RowIdentities {
    pub(super) const fn new() -> Self {
        Self(AtomicU64::new(1))
    }

    pub(super) fn reserve(&self) -> Result<u64, SqlError> {
        let mut next = self.next();
        loop {
            let following = next.checked_add(1).ok_or_else(|| {
                sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "row identities exhausted")
            })?;
            match self.0.compare_exchange_weak(
                next,
                following,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(next),
                Err(observed) => next = observed,
            }
        }
    }

    pub(super) fn next(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    pub(super) fn observe(&self, rowid: u64) {
        self.0.fetch_max(rowid.saturating_add(1), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_identity_exhaustion_and_recovery_never_wrap() {
        let identities = RowIdentities::new();
        crate::mem::guard::forbid_alloc(|| {
            identities.observe(u64::MAX - 2);
            assert_eq!(identities.reserve().unwrap(), u64::MAX - 1);
            for _ in 0..3 {
                let error = identities.reserve().unwrap_err();
                assert_eq!(error.sqlstate.as_str(), "54000");
                assert_eq!(error.message.as_str(), "row identities exhausted");
            }
            identities.observe(1);
            identities.observe(u64::MAX);
            assert_eq!(identities.next(), u64::MAX);
        });
        let recovered = RowIdentities::new();
        recovered.observe(identities.next() - 1);
        assert!(recovered.reserve().is_err());
    }

    #[test]
    fn row_identity_shared_reservations_are_unique() {
        let identities = RowIdentities::new();
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| {
                    std::array::from_fn::<_, 256, _>(|_| identities.reserve().unwrap())
                })
            });
            let second = scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| {
                    std::array::from_fn::<_, 256, _>(|_| identities.reserve().unwrap())
                })
            });
            let mut ids = [0; 512];
            ids[..256].copy_from_slice(&first.join().unwrap());
            ids[256..].copy_from_slice(&second.join().unwrap());
            crate::mem::guard::forbid_alloc(|| {
                ids.sort_unstable();
                assert_eq!(ids, std::array::from_fn(|i| i as u64 + 1));
            });
        });
        assert_eq!(identities.next(), 513);
    }
}
