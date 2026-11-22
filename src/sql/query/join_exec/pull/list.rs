//! The growable lists a node keeps its rows and tables in. A retained
//! reader's nodes keep them in `plan_build_bytes` leases, which outlive the
//! statement's per-call memory. The general executor's nodes keep them in
//! the statement arena, as the rows they bind: it never rewinds below them
//! while the plan runs, and a plan of many joins would otherwise take a
//! lease per list from the server-wide pool.

use core::mem::MaybeUninit;

use super::*;

/// Where a plan's node lists live.
#[derive(Clone)]
pub(super) enum Space<'a> {
    Leases(Arc<LeasePool>),
    Arena(&'a Arena),
}

impl<'a> Space<'a> {
    /// An empty list.
    pub(super) fn list<T: Copy>(&self) -> Result<NodeList<'a, T>, ListError> {
        Ok(match self {
            Self::Leases(pool) => NodeList::Lease(pool.lease(0)?),
            Self::Arena(arena) => NodeList::Arena {
                arena,
                buffer: &mut [],
                len: 0,
            },
        })
    }
}

/// A full list: its lease pool's error, or the arena's.
#[derive(Debug)]
pub(super) enum ListError {
    Lease(LeaseError),
    Arena,
}

impl From<LeaseError> for ListError {
    fn from(error: LeaseError) -> Self {
        Self::Lease(error)
    }
}

impl ListError {
    /// The error naming `what` and the full lease pool, or the arena's.
    pub(super) fn into_sql(self, what: &str) -> SqlError {
        match self {
            Self::Lease(error) => crate::sql_err!(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "{} exceed plan_build_bytes: {}",
                what,
                error
            )
            .plan_build_exhausted(),
            Self::Arena => arena_full(),
        }
    }
}

/// A list of `T` in a lease or in the arena.
pub(super) enum NodeList<'a, T: Copy> {
    Lease(MutableSlice<T>),
    Arena {
        arena: &'a Arena,
        buffer: &'a mut [MaybeUninit<T>],
        len: usize,
    },
}

impl<'a, T: Copy> NodeList<'a, T> {
    /// An empty list in the same space.
    pub(super) fn sibling<U: Copy>(&self) -> Result<NodeList<'a, U>, ListError> {
        self.space().list()
    }

    pub(super) fn space(&self) -> Space<'a> {
        match self {
            Self::Lease(slice) => Space::Leases(Arc::clone(slice.pool())),
            Self::Arena { arena, .. } => Space::Arena(arena),
        }
    }

    pub(super) fn len(&self) -> usize {
        match self {
            Self::Lease(slice) => slice.len(),
            Self::Arena { len, .. } => *len,
        }
    }

    pub(super) fn as_slice(&self) -> &[T] {
        match self {
            Self::Lease(slice) => slice.as_slice(),
            // SAFETY: the first `len` entries are initialized.
            Self::Arena { buffer, len, .. } => unsafe {
                core::slice::from_raw_parts(buffer.as_ptr().cast::<T>(), *len)
            },
        }
    }

    pub(super) fn as_mut_slice(&mut self) -> &mut [T] {
        match self {
            Self::Lease(slice) => slice.as_mut_slice(),
            // SAFETY: the first `len` entries are initialized.
            Self::Arena { buffer, len, .. } => unsafe {
                core::slice::from_raw_parts_mut(buffer.as_mut_ptr().cast::<T>(), *len)
            },
        }
    }

    pub(super) fn capacity_entries(&self) -> usize {
        match self {
            Self::Lease(slice) => slice.capacity_entries(),
            Self::Arena { buffer, .. } => buffer.len(),
        }
    }

    pub(super) fn max_entries(&self) -> usize {
        match self {
            Self::Lease(slice) => slice.max_entries(),
            Self::Arena { .. } => isize::MAX as usize / size_of::<T>().max(1),
        }
    }

    /// Grows the capacity to `entries`, keeping the entries.
    pub(super) fn reserve_exact(&mut self, entries: usize) -> Result<(), ListError> {
        match self {
            Self::Lease(slice) => Ok(slice.reserve_exact(entries)?),
            Self::Arena { arena, buffer, len } => {
                if entries <= buffer.len() {
                    return Ok(());
                }
                let grown = arena
                    .alloc_slice_with(entries, |_| MaybeUninit::uninit())
                    .map_err(|_| ListError::Arena)?;
                grown[..*len].copy_from_slice(&buffer[..*len]);
                *buffer = grown;
                Ok(())
            }
        }
    }

    pub(super) fn push(&mut self, value: T) -> Result<(), ListError> {
        if let Self::Lease(slice) = self {
            return Ok(slice.push(value)?);
        }
        if self.len() == self.capacity_entries() {
            self.reserve_exact(self.capacity_entries().saturating_mul(2).max(4))?;
        }
        self.push_reserved(value)
    }

    pub(super) fn push_reserved(&mut self, value: T) -> Result<(), ListError> {
        match self {
            Self::Lease(slice) => Ok(slice.push_reserved(value)?),
            Self::Arena { buffer, len, .. } => {
                let slot = buffer.get_mut(*len).ok_or(ListError::Arena)?;
                *slot = MaybeUninit::new(value);
                *len += 1;
                Ok(())
            }
        }
    }

    pub(super) fn extend_from_slice(&mut self, values: &[T]) -> Result<(), ListError> {
        if let Self::Lease(slice) = self {
            return Ok(slice.extend_from_slice(values)?);
        }
        let needed = self.len() + values.len();
        if needed > self.capacity_entries() {
            self.reserve_exact(needed.max(self.capacity_entries().saturating_mul(2)))?;
        }
        for &value in values {
            self.push_reserved(value)?;
        }
        Ok(())
    }

    pub(super) fn truncate(&mut self, to: usize) {
        match self {
            Self::Lease(slice) => slice.truncate(to),
            Self::Arena { len, .. } => *len = (*len).min(to),
        }
    }

    pub(super) fn clear(&mut self) {
        match self {
            Self::Lease(slice) => slice.clear(),
            Self::Arena { len, .. } => *len = 0,
        }
    }
}

impl<T: Copy> core::ops::Deref for NodeList<'_, T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: Copy> core::ops::DerefMut for NodeList<'_, T> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}
