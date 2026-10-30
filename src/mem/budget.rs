//! The startup byte budget.
//!
//! Every fixed structure created during startup draws its bytes from one
//! `Budget` computed from config. Overdrawing is a startup error that names
//! the component, so a misconfigured limit fails immediately and
//! diagnosably instead of surfacing as an OOM kill under load.

use std::fmt;

pub struct Budget {
    total: usize,
    used: usize,
    observer: Option<fn(&'static str, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetError {
    pub what: &'static str,
    pub requested: usize,
    pub remaining: usize,
    pub total: usize,
}

impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "memory budget exceeded by '{}': requested {} bytes, {} of {} remaining",
            self.what, self.requested, self.remaining, self.total
        )
    }
}

impl std::error::Error for BudgetError {}

impl Budget {
    pub fn new(total: usize) -> Self {
        Self {
            total,
            used: 0,
            observer: None,
        }
    }

    /// Report successful startup reservations to an explicitly installed
    /// observer. Rejected reservations emit no event.
    pub fn observe_draws(&mut self, observer: fn(&'static str, usize)) {
        self.observer = Some(observer);
    }

    /// Reserves `bytes` for the component named `what`.
    pub fn draw(&mut self, bytes: usize, what: &'static str) -> Result<(), BudgetError> {
        let remaining = self.total - self.used;
        if bytes > remaining {
            return Err(BudgetError {
                what,
                requested: bytes,
                remaining,
                total: self.total,
            });
        }
        self.used += bytes;
        if let Some(observer) = self.observer {
            observer(what, bytes);
        }
        Ok(())
    }

    /// `count` items of `size` bytes each, rejecting arithmetic overflow.
    pub fn draw_array(
        &mut self,
        count: usize,
        size: usize,
        what: &'static str,
    ) -> Result<(), BudgetError> {
        let bytes = count.checked_mul(size).ok_or(BudgetError {
            what,
            requested: usize::MAX,
            remaining: self.total - self.used,
            total: self.total,
        })?;
        self.draw(bytes, what)
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn used(&self) -> usize {
        self.used
    }

    pub fn remaining(&self) -> usize {
        self.total - self.used
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_accumulate() {
        let mut b = Budget::new(100);
        b.draw(60, "a").unwrap();
        b.draw(40, "b").unwrap();
        assert_eq!(b.remaining(), 0);
        assert_eq!(b.used(), 100);
    }

    #[test]
    fn overdraw_names_the_component() {
        let mut b = Budget::new(100);
        b.draw(90, "memtable").unwrap();
        let err = b.draw(11, "block_cache").unwrap_err();
        assert_eq!(err.what, "block_cache");
        assert_eq!(err.requested, 11);
        assert_eq!(err.remaining, 10);
        assert_eq!(err.total, 100);
        // A failed draw reserves nothing.
        assert_eq!(b.remaining(), 10);
    }

    #[test]
    fn array_overflow_is_rejected() {
        let mut b = Budget::new(100);
        let err = b.draw_array(usize::MAX, 2, "huge").unwrap_err();
        assert_eq!(err.what, "huge");
    }

    #[test]
    fn observer_reports_only_successful_reservations() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static EVENTS: AtomicUsize = AtomicUsize::new(0);
        static BYTES: AtomicUsize = AtomicUsize::new(0);
        let mut budget = Budget::new(100);
        budget.draw(10, "before observer").unwrap();
        budget.observe_draws(|name, bytes| {
            assert_eq!(name, "observed");
            EVENTS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(bytes, Ordering::Relaxed);
        });
        budget.draw(60, "observed").unwrap();
        assert!(budget.draw(31, "rejected").is_err());
        assert!(budget.draw_array(usize::MAX, 2, "overflow").is_err());
        budget.draw(30, "observed").unwrap();
        assert_eq!(EVENTS.load(Ordering::Relaxed), 2);
        assert_eq!(BYTES.load(Ordering::Relaxed), 90);
        assert_eq!(budget.used(), 100);
    }
}
