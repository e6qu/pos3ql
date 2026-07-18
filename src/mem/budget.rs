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
        Self { total, used: 0 }
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
}
