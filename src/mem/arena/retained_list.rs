//! Address-independent lists expose only scoped views of checked retained bytes.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetainedListError {
    Capacity(ArenaFull),
    Storage(ArenaSliceError),
}

pub(crate) struct RetainedList<T: Copy + 'static> {
    entries: Option<ArenaSliceHandle<T>>,
    len: usize,
}

impl<T: Copy + 'static> RetainedList<T> {
    pub(crate) const fn new() -> Self {
        Self {
            entries: None,
            len: 0,
        }
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn with_slice<R>(
        &self,
        arena: &Arena,
        read: impl FnOnce(&[T]) -> R,
    ) -> Result<R, ArenaSliceError> {
        match self.entries {
            Some(handle) => {
                let entries = arena.retained_slice_mut(handle)?;
                Ok(read(&entries[..self.len]))
            }
            None => Ok(read(&[])),
        }
    }

    pub(crate) fn with_mut_slice<R>(
        &mut self,
        arena: &Arena,
        edit: impl FnOnce(&mut [T]) -> R,
    ) -> Result<R, ArenaSliceError> {
        match self.entries {
            Some(handle) => {
                let mut entries = arena.retained_slice_mut(handle)?;
                Ok(edit(&mut entries[..self.len]))
            }
            None => Ok(edit(&mut [])),
        }
    }

    pub(crate) fn get(&self, arena: &Arena, index: usize) -> Result<Option<T>, ArenaSliceError> {
        self.with_slice(arena, |values| values.get(index).copied())
    }

    pub(crate) fn push(&mut self, arena: &Arena, value: T) -> Result<(), RetainedListError> {
        let mut old = self
            .entries
            .map(|handle| arena.retained_slice_mut(handle))
            .transpose()
            .map_err(RetainedListError::Storage)?;
        if let Some(entries) = old.as_mut()
            && self.len < entries.len()
        {
            entries[self.len] = value;
            self.len += 1;
            return Ok(());
        }
        let capacity = self
            .len
            .checked_mul(2)
            .map(|value| value.max(4))
            .ok_or_else(|| RetainedListError::Capacity(arena.full(usize::MAX)))?;
        let handle = arena
            .retain_slice_with(capacity, |_| value)
            .map_err(RetainedListError::Capacity)?;
        let mut entries = arena
            .retained_slice_mut(handle)
            .map_err(RetainedListError::Storage)?;
        if let Some(old) = old.as_ref() {
            entries[..self.len].copy_from_slice(&old[..self.len]);
        }
        entries[self.len] = value;
        self.entries = Some(handle);
        self.len += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_list_views_preserve_positions_and_reject_aliases_and_stale_owners() {
        let mut budget = Budget::new(4096);
        let original = Arena::new(&mut budget, "retained directory", 2048).unwrap();
        let other = Arena::new(&mut budget, "other directory", 2048).unwrap();
        let mut values = RetainedList::new();
        crate::mem::guard::forbid_alloc(|| {
            for value in 0..6u64 {
                values.push(&original, value).unwrap();
            }
            let handle = values.entries.unwrap();
            values
                .with_mut_slice(&original, |items| {
                    items[1..].rotate_right(1);
                    assert!(matches!(
                        original.retained_slice_mut(handle),
                        Err(ArenaSliceError::Borrowed)
                    ));
                })
                .unwrap();
            assert_eq!(values.with_slice(&original, |items| items[1]).unwrap(), 5);
            assert!(matches!(values.get(&other, 0), Err(ArenaSliceError::Stale)));
        });
        let mut moved = original;
        crate::mem::guard::forbid_alloc(|| {
            moved.recycle_front();
            assert_eq!(values.get(&moved, 1).unwrap(), Some(5));
            assert_eq!(values.get(&moved, 6).unwrap(), None);
            moved.reset();
            assert!(matches!(values.get(&moved, 0), Err(ArenaSliceError::Stale)));
            assert!(matches!(
                values.push(&moved, 42),
                Err(RetainedListError::Storage(ArenaSliceError::Stale))
            ));
            assert_eq!(values.len(), 6);
        });
    }
}
