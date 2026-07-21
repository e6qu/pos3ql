//! A fixed-capacity hash map: open addressing, linear probing, FNV-1a
//! hashing, backward-shift deletion (no tombstones, so probe distances do
//! not degrade over time). The slot array is sized at construction to keep
//! the load factor at or below one half; inserting past the requested
//! capacity is an error naming the map.

use core::fmt;
use core::hash::{Hash, Hasher};

use super::budget::{Budget, BudgetError};

pub struct FixedMap<K, V> {
    what: &'static str,
    slots: Box<[Option<(K, V)>]>,
    mask: usize,
    len: usize,
    max_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapFull {
    pub what: &'static str,
    pub capacity: usize,
}

impl fmt::Display for MapFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "map '{}' is full (capacity {})", self.what, self.capacity)
    }
}

impl std::error::Error for MapFull {}

/// FNV-1a, 64-bit. Deterministic across runs and platforms — required for
/// reproducible simulation — unlike std's randomly seeded default hasher.
/// Constants from the FNV reference: offset basis and prime.
pub struct Fnv1aHasher(u64);

impl Default for Fnv1aHasher {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for Fnv1aHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

impl<K: Hash + Eq, V> FixedMap<K, V> {
    /// Bytes `new` will draw from the budget for a given capacity.
    pub fn budget_bytes(capacity: usize) -> usize {
        Self::slot_count(capacity) * size_of::<Option<(K, V)>>()
    }

    fn slot_count(capacity: usize) -> usize {
        capacity
            .checked_mul(2)
            .and_then(|n| n.checked_next_power_of_two())
            .unwrap_or_else(|| panic!("map capacity {capacity} is unrepresentable"))
            .max(8)
    }

    pub fn new(
        budget: &mut Budget,
        what: &'static str,
        capacity: usize,
    ) -> Result<Self, BudgetError> {
        let slot_count = Self::slot_count(capacity);
        budget.draw_array(slot_count, size_of::<Option<(K, V)>>(), what)?;
        let mut slots = Vec::new();
        slots.resize_with(slot_count, || None);
        Ok(Self {
            what,
            slots: slots.into_boxed_slice(),
            mask: slot_count - 1,
            len: 0,
            max_len: capacity,
        })
    }

    /// Inserts, returning the previous value for the key if any.
    pub fn insert(&mut self, key: K, value: V) -> Result<Option<V>, MapFull> {
        let mut i = self.home(&key);
        loop {
            match &mut self.slots[i] {
                Some((k, v)) if *k == key => {
                    return Ok(Some(core::mem::replace(v, value)));
                }
                Some(_) => i = (i + 1) & self.mask,
                None => {
                    if self.len == self.max_len {
                        return Err(MapFull {
                            what: self.what,
                            capacity: self.max_len,
                        });
                    }
                    self.slots[i] = Some((key, value));
                    self.len += 1;
                    return Ok(None);
                }
            }
        }
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.find(key).map(|i| &self.slots[i].as_ref().unwrap().1)
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.find(key)
            .map(|i| &mut self.slots[i].as_mut().unwrap().1)
    }

    /// Removes a key, restoring the probe invariant by backward-shifting
    /// any displaced entries into the hole.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let mut hole = self.find(key)?;
        let (_, removed) = self.slots[hole].take().unwrap();
        self.len -= 1;

        let mut probe = hole;
        loop {
            probe = (probe + 1) & self.mask;
            let Some((k, _)) = &self.slots[probe] else {
                break;
            };
            let home = self.home(k);
            // The entry at `probe` may fill the hole only if its home does
            // not lie cyclically within (hole, probe] — otherwise lookups
            // starting at its home would no longer reach it.
            let home_in_between = if hole < probe {
                hole < home && home <= probe
            } else {
                home > hole || home <= probe
            };
            if !home_in_between {
                self.slots[hole] = self.slots[probe].take();
                hole = probe;
            }
        }
        Some(removed)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.max_len
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.slots
            .iter()
            .filter_map(|slot| slot.as_ref().map(|(k, v)| (k, v)))
    }

    pub fn clear(&mut self) {
        self.slots.fill_with(|| None);
        self.len = 0;
    }

    fn home(&self, key: &K) -> usize {
        let mut hasher = Fnv1aHasher::default();
        key.hash(&mut hasher);
        (hasher.finish() as usize) & self.mask
    }

    fn find(&self, key: &K) -> Option<usize> {
        let mut i = self.home(key);
        loop {
            match &self.slots[i] {
                Some((k, _)) if k == key => return Some(i),
                Some(_) => i = (i + 1) & self.mask,
                None => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_get_replace_remove() {
        let mut budget = Budget::new(4096);
        let mut m: FixedMap<u64, u64> = FixedMap::new(&mut budget, "test", 8).unwrap();
        assert_eq!(m.insert(1, 10).unwrap(), None);
        assert_eq!(m.insert(2, 20).unwrap(), None);
        assert_eq!(m.insert(1, 11).unwrap(), Some(10));
        assert_eq!(m.get(&1), Some(&11));
        assert_eq!(m.get(&3), None);
        assert_eq!(m.remove(&1), Some(11));
        assert_eq!(m.get(&1), None);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn full_map_rejects_new_keys_but_updates_existing() {
        let mut budget = Budget::new(4096);
        let mut m: FixedMap<u64, u64> = FixedMap::new(&mut budget, "catalog", 2).unwrap();
        m.insert(1, 10).unwrap();
        m.insert(2, 20).unwrap();
        let err = m.insert(3, 30).unwrap_err();
        assert_eq!(err.what, "catalog");
        assert_eq!(err.capacity, 2);
        // Updating an existing key is not growth.
        assert_eq!(m.insert(2, 21).unwrap(), Some(20));
    }

    /// A key type whose hash is constant: every entry collides, exercising
    /// the probe chain and backward-shift deletion including wraparound.
    #[derive(PartialEq, Eq, Debug, Clone, Copy)]
    struct Colliding(u64);

    impl Hash for Colliding {
        fn hash<H: Hasher>(&self, state: &mut H) {
            0u64.hash(state);
        }
    }

    #[test]
    fn colliding_keys_survive_removal_in_any_order() {
        // Removal orders that stress backward shift: front, middle, back.
        for removal_order in [[0u64, 1, 2, 3], [3, 2, 1, 0], [1, 3, 0, 2], [2, 0, 3, 1]] {
            let mut budget = Budget::new(4096);
            let mut m: FixedMap<Colliding, u64> = FixedMap::new(&mut budget, "test", 4).unwrap();
            for k in 0..4 {
                m.insert(Colliding(k), k * 100).unwrap();
            }
            for (n, k) in removal_order.into_iter().enumerate() {
                assert_eq!(m.remove(&Colliding(k)), Some(k * 100), "removing {k}");
                for still_in in removal_order.into_iter().skip(n + 1) {
                    assert_eq!(
                        m.get(&Colliding(still_in)),
                        Some(&(still_in * 100)),
                        "key {still_in} must survive removal of {k}"
                    );
                }
            }
            assert!(m.is_empty());
        }
    }

    #[test]
    fn differential_against_std_hashmap() {
        // Deterministic xorshift64* drives a random insert/remove/get mix,
        // cross-checked against std::collections::HashMap.
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
            state
        };

        let mut budget = Budget::new(1 << 20);
        let mut ours: FixedMap<u64, u64> = FixedMap::new(&mut budget, "test", 64).unwrap();
        let mut reference = std::collections::HashMap::new();

        for _ in 0..10_000 {
            let key = next() % 96;
            match next() % 3 {
                0 if reference.len() < 64 => {
                    let value = next();
                    assert_eq!(
                        ours.insert(key, value).unwrap(),
                        reference.insert(key, value)
                    );
                }
                1 => assert_eq!(ours.remove(&key), reference.remove(&key)),
                _ => assert_eq!(ours.get(&key), reference.get(&key)),
            }
            assert_eq!(ours.len(), reference.len());
        }
        let mut ours_sorted: Vec<(u64, u64)> = ours.iter().map(|(k, v)| (*k, *v)).collect();
        ours_sorted.sort_unstable();
        let mut ref_sorted: Vec<(u64, u64)> = reference.into_iter().collect();
        ref_sorted.sort_unstable();
        assert_eq!(ours_sorted, ref_sorted);
    }

    #[test]
    fn operations_do_not_allocate() {
        let mut budget = Budget::new(1 << 16);
        let mut m: FixedMap<u64, u64> = FixedMap::new(&mut budget, "test", 32).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for k in 0..32 {
                m.insert(k, k).unwrap();
            }
            for k in 0..32 {
                assert_eq!(m.get(&k), Some(&k));
                m.remove(&k);
            }
        });
    }
}
