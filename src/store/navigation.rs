//! Immutable summaries for navigating secondary-index data blocks.

use super::{BlockId, BlockStore, BlockType, ValueIndexError};

const FANOUT: usize = 32;
const LEVELS: usize = 14; // 32^14 exceeds the complete u64 entry space.
const HEADER: usize = 8;
const REFERENCE: usize = 32 + 8 + 33;
const NODE_BYTES: usize = HEADER + FANOUT * REFERENCE;
const PENDING: usize = 1 + (FANOUT - 1) * LEVELS;
pub(crate) const SPATIAL_DATA_BYTES: usize = 16 * 1024;
pub(crate) const SIGNATURE_DATA_BYTES: usize = 1024;
pub(crate) const INTERVAL_DATA_BYTES: usize = 8 * 1024;

pub(crate) const INTERVAL_KEY_BYTES: usize = 15;
const INTERVAL_HAS_EMPTY: u8 = 1;

/// A conservative one-dimensional envelope. Keys are order-preserving,
/// possibly lossy prefixes of SQL values. Empty range values are tracked
/// separately because they match containment and equality without occupying
/// any point in the envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IntervalSummary {
    lower: [u8; INTERVAL_KEY_BYTES],
    upper: [u8; INTERVAL_KEY_BYTES],
    flags: u8,
}

impl IntervalSummary {
    pub(crate) fn bounded(
        lower: [u8; INTERVAL_KEY_BYTES],
        upper: [u8; INTERVAL_KEY_BYTES],
    ) -> Self {
        debug_assert!(lower <= upper);
        Self {
            lower,
            upper,
            flags: 0,
        }
    }

    pub(crate) fn empty_value() -> Self {
        Self {
            lower: [u8::MAX; INTERVAL_KEY_BYTES],
            upper: [0; INTERVAL_KEY_BYTES],
            flags: INTERVAL_HAS_EMPTY,
        }
    }

    pub(crate) fn has_empty(self) -> bool {
        self.flags & INTERVAL_HAS_EMPTY != 0
    }

    pub(crate) fn has_values(self) -> bool {
        self.lower <= self.upper
    }

    pub(crate) fn bounds(self) -> ([u8; INTERVAL_KEY_BYTES], [u8; INTERVAL_KEY_BYTES]) {
        (self.lower, self.upper)
    }

    pub(crate) fn intersects(
        self,
        lower: [u8; INTERVAL_KEY_BYTES],
        upper: [u8; INTERVAL_KEY_BYTES],
    ) -> bool {
        self.has_values() && self.lower <= upper && self.upper >= lower
    }

    fn union(self, other: Self) -> Self {
        let (lower, upper) = match (self.has_values(), other.has_values()) {
            (true, true) => (self.lower.min(other.lower), self.upper.max(other.upper)),
            (true, false) => (self.lower, self.upper),
            (false, true) => (other.lower, other.upper),
            (false, false) => ([u8::MAX; INTERVAL_KEY_BYTES], [0; INTERVAL_KEY_BYTES]),
        };
        Self {
            lower,
            upper,
            flags: self.flags | other.flags,
        }
    }
}

/// A small, mergeable token signature. It is deliberately a one-sided filter:
/// collisions may retain irrelevant children, but can never hide a match.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TokenSignature([u64; 4]);

impl TokenSignature {
    pub(crate) fn insert(&mut self, namespace: u8, hash: u64) {
        let mixed = hash ^ (u64::from(namespace).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        for shift in [0, 21, 42] {
            let bit = ((mixed.rotate_left(shift) ^ (mixed >> (shift / 3 + 1))) & 255) as usize;
            self.0[bit / 64] |= 1u64 << (bit % 64);
        }
    }

    pub(crate) fn union(self, other: Self) -> Self {
        Self(core::array::from_fn(|index| self.0[index] | other.0[index]))
    }

    pub(crate) fn intersection(self, other: Self) -> Self {
        Self(core::array::from_fn(|index| self.0[index] & other.0[index]))
    }

    pub(crate) fn contains(self, required: Self) -> bool {
        self.0
            .iter()
            .zip(required.0)
            .all(|(available, required)| available & required == required)
    }

    pub(crate) fn intersects(self, required: Self) -> bool {
        self.0
            .iter()
            .zip(required.0)
            .any(|(available, required)| available & required != 0)
    }

    pub(crate) fn is_empty(self) -> bool {
        self.0 == [0; 4]
    }
}

/// Empty means no non-NULL keys. Unbounded retains values that cannot be
/// summarized for exact SQL recheck; it must never imply an exclusion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum NavigationSummary {
    Empty,
    Unbounded,
    Finite(SpatialBox),
    Signature(TokenSignature),
    Interval(IntervalSummary),
}

pub(crate) type SpatialBounds = NavigationSummary;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NavigationKind {
    Spatial,
    Signature,
    Interval,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NavigationSpec {
    pub(crate) position: u8,
    pub(crate) kind: NavigationKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SpatialBox {
    minimum_x: f64,
    minimum_y: f64,
    maximum_x: f64,
    maximum_y: f64,
}

impl SpatialBox {
    pub(crate) fn coordinates(self) -> [f64; 4] {
        [
            self.minimum_x,
            self.minimum_y,
            self.maximum_x,
            self.maximum_y,
        ]
    }
}

impl NavigationSummary {
    pub(crate) fn new(minimum_x: f64, minimum_y: f64, maximum_x: f64, maximum_y: f64) -> Self {
        if [minimum_x, minimum_y, maximum_x, maximum_y]
            .iter()
            .all(|value| value.is_finite())
            && minimum_x <= maximum_x
            && minimum_y <= maximum_y
        {
            Self::Finite(SpatialBox {
                minimum_x,
                minimum_y,
                maximum_x,
                maximum_y,
            })
        } else {
            Self::Unbounded
        }
    }

    pub(crate) fn union(self, other: Self) -> Self {
        match (self, other) {
            (Self::Empty, bounds) | (bounds, Self::Empty) => bounds,
            (Self::Unbounded, _) | (_, Self::Unbounded) => Self::Unbounded,
            (Self::Finite(left), Self::Finite(right)) => Self::new(
                left.minimum_x.min(right.minimum_x),
                left.minimum_y.min(right.minimum_y),
                left.maximum_x.max(right.maximum_x),
                left.maximum_y.max(right.maximum_y),
            ),
            (Self::Signature(left), Self::Signature(right)) => Self::Signature(left.union(right)),
            (Self::Interval(left), Self::Interval(right)) => Self::Interval(left.union(right)),
            _ => Self::Unbounded,
        }
    }

    fn encode(self, output: &mut [u8]) {
        output.fill(0);
        match self {
            Self::Empty => {}
            Self::Unbounded => output[0] = 1,
            Self::Finite(bounds) => {
                output[0] = 2;
                for (bytes, value) in output[1..]
                    .as_chunks_mut::<8>()
                    .0
                    .iter_mut()
                    .zip(bounds.coordinates())
                {
                    bytes.copy_from_slice(&value.to_le_bytes());
                }
            }
            Self::Signature(signature) => {
                output[0] = 3;
                for (bytes, value) in output[1..]
                    .as_chunks_mut::<8>()
                    .0
                    .iter_mut()
                    .zip(signature.0)
                {
                    bytes.copy_from_slice(&value.to_le_bytes());
                }
            }
            Self::Interval(interval) => {
                output[0] = 4;
                output[1] = interval.flags;
                output[2..17].copy_from_slice(&interval.lower);
                output[17..32].copy_from_slice(&interval.upper);
            }
        }
    }

    fn decode(input: &[u8]) -> Result<Self, ValueIndexError> {
        match input[0] {
            0 | 1 if input[1..].iter().all(|byte| *byte == 0) => Ok(if input[0] == 0 {
                Self::Empty
            } else {
                Self::Unbounded
            }),
            2 => {
                let values = core::array::from_fn::<_, 4, _>(|index| {
                    f64::from_le_bytes(input[1 + index * 8..9 + index * 8].try_into().unwrap())
                });
                let bounds = Self::new(values[0], values[1], values[2], values[3]);
                if matches!(bounds, Self::Finite(_)) {
                    Ok(bounds)
                } else {
                    Err(ValueIndexError::Corrupt)
                }
            }
            3 => Ok(Self::Signature(TokenSignature(core::array::from_fn(
                |index| u64::from_le_bytes(input[1 + index * 8..9 + index * 8].try_into().unwrap()),
            )))),
            4 if input[1] & !INTERVAL_HAS_EMPTY == 0 && input[32] == 0 => {
                let interval = IntervalSummary {
                    lower: input[2..17].try_into().unwrap(),
                    upper: input[17..32].try_into().unwrap(),
                    flags: input[1],
                };
                if interval.has_values() || interval.has_empty() {
                    Ok(Self::Interval(interval))
                } else {
                    Err(ValueIndexError::Corrupt)
                }
            }
            _ => Err(ValueIndexError::Corrupt),
        }
    }
}

#[derive(Clone, Copy)]
struct NavigationReference {
    id: BlockId,
    entries: u64,
    summary: NavigationSummary,
}

impl NavigationReference {
    const EMPTY: Self = Self {
        id: BlockId([0; 32]),
        entries: 0,
        summary: NavigationSummary::Empty,
    };

    fn encode(self, output: &mut [u8]) {
        output[..32].copy_from_slice(&self.id.0);
        output[32..40].copy_from_slice(&self.entries.to_le_bytes());
        self.summary.encode(&mut output[40..]);
    }

    fn decode(input: &[u8]) -> Result<Self, ValueIndexError> {
        let entries = u64::from_le_bytes(input[32..40].try_into().unwrap());
        if entries == 0 || input[..32].iter().all(|byte| *byte == 0) {
            return Err(ValueIndexError::Corrupt);
        }
        Ok(Self {
            id: BlockId(input[..32].try_into().unwrap()),
            entries,
            summary: NavigationSummary::decode(&input[40..])?,
        })
    }
}

/// Streaming bottom-up construction. Each level owns exactly one fixed-size
/// node; publication never needs a resident list of all leaves.
pub(crate) struct NavigationWriter {
    nodes: Box<[u8]>,
    counts: [usize; LEVELS],
    entries: [u64; LEVELS],
    summaries: [NavigationSummary; LEVELS],
    position: u8,
    covering: bool,
}

impl NavigationWriter {
    pub(crate) fn new() -> Self {
        Self {
            nodes: vec![0; NODE_BYTES * LEVELS].into_boxed_slice(),
            counts: [0; LEVELS],
            entries: [0; LEVELS],
            summaries: [NavigationSummary::Empty; LEVELS],
            position: 0,
            covering: false,
        }
    }

    pub(crate) fn budget_bytes() -> usize {
        NODE_BYTES * LEVELS
    }

    pub(crate) fn reset(&mut self, position: u8, covering: bool) {
        self.counts.fill(0);
        self.entries.fill(0);
        self.summaries.fill(NavigationSummary::Empty);
        self.position = position;
        self.covering = covering;
    }

    pub(crate) fn append(
        &mut self,
        store: &mut dyn BlockStore,
        id: BlockId,
        entries: u64,
        summary: NavigationSummary,
    ) -> Result<(), ValueIndexError> {
        if entries == 0 || id == BlockId([0; 32]) {
            return Err(ValueIndexError::Corrupt);
        }
        self.push(
            store,
            0,
            NavigationReference {
                id,
                entries,
                summary,
            },
        )
    }

    fn push(
        &mut self,
        store: &mut dyn BlockStore,
        level: usize,
        reference: NavigationReference,
    ) -> Result<(), ValueIndexError> {
        if level >= LEVELS {
            return Err(ValueIndexError::Corrupt);
        }
        // Retain a full top-level node until its successor arrives, avoiding
        // redundant one-child roots when a generation exactly fills a node.
        if self.counts[level] == FANOUT {
            let parent = self.publish(store, level)?;
            self.push(store, level + 1, parent)?;
        }
        let entries = self.entries[level]
            .checked_add(reference.entries)
            .ok_or(ValueIndexError::Corrupt)?;
        let at = level * NODE_BYTES + HEADER + self.counts[level] * REFERENCE;
        reference.encode(&mut self.nodes[at..at + REFERENCE]);
        self.counts[level] += 1;
        self.entries[level] = entries;
        self.summaries[level] = self.summaries[level].union(reference.summary);
        Ok(())
    }

    fn publish(
        &mut self,
        store: &mut dyn BlockStore,
        level: usize,
    ) -> Result<NavigationReference, ValueIndexError> {
        let at = level * NODE_BYTES;
        let header = &mut self.nodes[at..at + HEADER];
        header.fill(0);
        header[0] = 1;
        header[1] = level as u8;
        header[2] = self.position;
        header[3] = u8::from(self.covering);
        header[4..6].copy_from_slice(&(self.counts[level] as u16).to_le_bytes());
        let id = store.put(
            &self.nodes[at..at + HEADER + self.counts[level] * REFERENCE],
            BlockType::ValueIndexNavigationV1,
            0,
        )?;
        let reference = NavigationReference {
            id,
            entries: self.entries[level],
            summary: self.summaries[level],
        };
        self.counts[level] = 0;
        self.entries[level] = 0;
        self.summaries[level] = NavigationSummary::Empty;
        Ok(reference)
    }

    pub(crate) fn finish(
        &mut self,
        store: &mut dyn BlockStore,
    ) -> Result<BlockId, ValueIndexError> {
        for level in 0..LEVELS {
            if self.counts[level + 1..].iter().all(|count| *count == 0) {
                return Ok(self.publish(store, level)?.id);
            }
            if self.counts[level] != 0 {
                let parent = self.publish(store, level)?;
                self.push(store, level + 1, parent)?;
            }
        }
        Err(ValueIndexError::Corrupt)
    }
}

#[derive(Clone, Copy)]
struct PendingReference {
    reference: NavigationReference,
    height: Option<u8>,
}

/// A fixed-depth DFS cursor. Its bound follows the on-disk fan-out and u64
/// entry count, not table cardinality; malformed depth is rejected before push.
pub(crate) struct NavigationCursor {
    pending: [PendingReference; PENDING],
    count: usize,
    position: Option<u8>,
    covering: Option<bool>,
    root_entries_known: bool,
}

impl NavigationCursor {
    pub(crate) fn new(root: BlockId, entries: u64) -> Self {
        let mut result = Self {
            pending: [PendingReference {
                reference: NavigationReference::EMPTY,
                height: None,
            }; PENDING],
            count: 1,
            position: None,
            covering: None,
            root_entries_known: true,
        };
        result.pending[0].reference = NavigationReference {
            id: root,
            entries,
            summary: NavigationSummary::Unbounded,
        };
        result
    }

    pub(crate) fn for_gc(root: BlockId) -> Self {
        let mut cursor = Self::new(root, 0);
        cursor.root_entries_known = false;
        cursor
    }

    pub(crate) fn next(
        &mut self,
        store: &mut dyn BlockStore,
        scratch: &mut [u8],
        intersects: &mut impl FnMut(u8, NavigationSummary) -> bool,
        visit_node: &mut impl FnMut(BlockId) -> bool,
    ) -> Result<Option<(BlockId, u64, bool)>, ValueIndexError> {
        while self.count != 0 {
            self.count -= 1;
            let pending = self.pending[self.count];
            if self
                .position
                .is_some_and(|position| !intersects(position, pending.reference.summary))
            {
                continue;
            }
            if pending.height == Some(0) {
                return Ok(Some((
                    pending.reference.id,
                    pending.reference.entries,
                    self.covering.ok_or(ValueIndexError::Corrupt)?,
                )));
            }
            if !visit_node(pending.reference.id) {
                return Ok(None);
            }
            let (len, kind) = store.get(&pending.reference.id, scratch)?;
            if kind != BlockType::ValueIndexNavigationV1
                || len < HEADER
                || scratch[0] != 1
                || scratch[1] as usize >= LEVELS
                || scratch[2] >= 32
                || scratch[3] > 1
                || scratch[6..8] != [0, 0]
            {
                return Err(ValueIndexError::Corrupt);
            }
            let height = scratch[1];
            if pending
                .height
                .is_some_and(|expected| height + 1 != expected)
            {
                return Err(ValueIndexError::Corrupt);
            }
            if self.position.is_some_and(|position| position != scratch[2])
                || self
                    .covering
                    .is_some_and(|covering| covering != (scratch[3] == 1))
            {
                return Err(ValueIndexError::Corrupt);
            }
            self.position = Some(scratch[2]);
            self.covering = Some(scratch[3] == 1);
            let count = u16::from_le_bytes(scratch[4..6].try_into().unwrap()) as usize;
            if count > FANOUT
                || len != HEADER + count * REFERENCE
                || self.count + count > PENDING
                || (count == 0
                    && (pending.height.is_some()
                        || (self.root_entries_known && pending.reference.entries != 0)))
            {
                return Err(ValueIndexError::Corrupt);
            }
            let mut entries = 0u64;
            let mut summary = NavigationSummary::Empty;
            for index in (0..count).rev() {
                let at = HEADER + index * REFERENCE;
                let reference = NavigationReference::decode(&scratch[at..at + REFERENCE])?;
                entries = entries
                    .checked_add(reference.entries)
                    .ok_or(ValueIndexError::Corrupt)?;
                summary = summary.union(reference.summary);
                self.pending[self.count] = PendingReference {
                    reference,
                    height: Some(height),
                };
                self.count += 1;
            }
            if ((pending.height.is_some() || self.root_entries_known)
                && entries != pending.reference.entries)
                || (pending.height.is_some() && summary != pending.reference.summary)
            {
                return Err(ValueIndexError::Corrupt);
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Budget;
    use crate::store::MemoryBlockStore;

    #[test]
    fn interval_summaries_preserve_empty_values_and_validate_encoding() {
        let left = IntervalSummary::bounded([1; INTERVAL_KEY_BYTES], [3; INTERVAL_KEY_BYTES]);
        let right = IntervalSummary::bounded([5; INTERVAL_KEY_BYTES], [9; INTERVAL_KEY_BYTES]);
        let summary = NavigationSummary::Interval(left)
            .union(NavigationSummary::Interval(IntervalSummary::empty_value()))
            .union(NavigationSummary::Interval(right));
        let NavigationSummary::Interval(interval) = summary else {
            panic!("interval summary");
        };
        assert_eq!(
            interval.bounds(),
            ([1; INTERVAL_KEY_BYTES], [9; INTERVAL_KEY_BYTES])
        );
        assert!(interval.has_empty());
        let mut encoded = [0; 33];
        summary.encode(&mut encoded);
        assert_eq!(NavigationSummary::decode(&encoded).unwrap(), summary);
        encoded[32] = 1;
        assert_eq!(
            NavigationSummary::decode(&encoded),
            Err(ValueIndexError::Corrupt)
        );
    }

    #[test]
    fn streaming_tree_prunes_three_levels_without_runtime_allocation() {
        let mut budget = Budget::new(16 << 20);
        let mut store = MemoryBlockStore::new(&mut budget, "navigation", 8 << 20, 2048).unwrap();
        let mut writer = NavigationWriter::new();
        let mut scratch = vec![0; super::super::MAX_PAYLOAD];
        writer.reset(2, true);
        let root = crate::mem::guard::forbid_alloc(|| {
            for index in 0..1057u64 {
                let id = store
                    .put(&index.to_le_bytes(), BlockType::ValueIndexData, 0)
                    .unwrap();
                let x = index as f64 * 100.0;
                writer
                    .append(&mut store, id, 1, SpatialBounds::new(x, x, x, x))
                    .unwrap();
            }
            writer.finish(&mut store).unwrap()
        });
        let before = store.reads();
        let mut visited_nodes = 0;
        let mut selected = NavigationCursor::new(root, 1057);
        let mut intersect = |position, bounds| {
            assert_eq!(position, 2);
            match bounds {
                SpatialBounds::Finite(bounds) => {
                    let [minimum_x, _, maximum_x, _] = bounds.coordinates();
                    minimum_x <= 51200.0 && maximum_x >= 51200.0
                }
                SpatialBounds::Empty => false,
                SpatialBounds::Unbounded => true,
                SpatialBounds::Signature(_) => true,
                SpatialBounds::Interval(_) => true,
            }
        };
        crate::mem::guard::forbid_alloc(|| {
            let (id, entries, covering) = selected
                .next(&mut store, &mut scratch, &mut intersect, &mut |_| {
                    visited_nodes += 1;
                    true
                })
                .unwrap()
                .unwrap();
            assert_eq!(entries, 1);
            assert!(covering);
            let (len, kind) = store.get(&id, &mut scratch).unwrap();
            assert_eq!(kind, BlockType::ValueIndexData);
            assert_eq!(len, 8);
            assert_eq!(u64::from_le_bytes(scratch[..8].try_into().unwrap()), 512);
            assert!(
                selected
                    .next(&mut store, &mut scratch, &mut intersect, &mut |_| {
                        visited_nodes += 1;
                        true
                    })
                    .unwrap()
                    .is_none()
            );
        });
        assert!(
            store.reads() - before <= 4,
            "{} reads",
            store.reads() - before
        );
        assert_eq!(visited_nodes, 3);
        let mut all = NavigationCursor::new(root, 1057);
        let mut leaves = 0;
        while all
            .next(&mut store, &mut scratch, &mut |_, _| true, &mut |_| true)
            .unwrap()
            .is_some()
        {
            leaves += 1;
        }
        assert_eq!(leaves, 1057);
    }

    #[test]
    fn empty_exact_fanout_and_recycled_builders_preserve_entry_counts() {
        let mut budget = Budget::new(8 << 20);
        let mut store =
            MemoryBlockStore::new(&mut budget, "navigation boundaries", 4 << 20, 256).unwrap();
        let mut writer = NavigationWriter::new();
        let mut scratch = vec![0; super::super::MAX_PAYLOAD];
        for entries in [0u64, 1, 32, 33, 64, 65, 0] {
            writer.reset(0, false);
            for index in 0..entries {
                let id = store
                    .put(&index.to_le_bytes(), BlockType::ValueIndexData, 0)
                    .unwrap();
                writer
                    .append(&mut store, id, 1, SpatialBounds::Unbounded)
                    .unwrap();
            }
            let root = writer.finish(&mut store).unwrap();
            let mut cursor = NavigationCursor::new(root, entries);
            let mut actual = 0;
            while let Some((_, count, covering)) = cursor
                .next(&mut store, &mut scratch, &mut |_, _| true, &mut |_| true)
                .unwrap()
            {
                actual += count;
                assert!(!covering);
            }
            assert_eq!(actual, entries);
        }
    }

    #[test]
    fn signature_tree_prunes_exact_tokens_without_false_negatives() {
        let mut budget = Budget::new(16 << 20);
        let mut store =
            MemoryBlockStore::new(&mut budget, "signature tree", 8 << 20, 2048).unwrap();
        let mut writer = NavigationWriter::new();
        let mut scratch = vec![0; super::super::MAX_PAYLOAD];
        writer.reset(0, false);
        for index in 0..1057u64 {
            let id = store
                .put(&index.to_le_bytes(), BlockType::ValueIndexData, 0)
                .unwrap();
            let mut signature = TokenSignature::default();
            signature.insert(7, index);
            writer
                .append(&mut store, id, 1, NavigationSummary::Signature(signature))
                .unwrap();
        }
        let root = writer.finish(&mut store).unwrap();
        let mut required = TokenSignature::default();
        required.insert(7, 512);
        let mut cursor = NavigationCursor::new(root, 1057);
        let mut found = false;
        let mut candidates = 0;
        while let Some((id, _, _)) = cursor
            .next(
                &mut store,
                &mut scratch,
                &mut |_, summary| match summary {
                    NavigationSummary::Signature(available) => available.contains(required),
                    NavigationSummary::Empty => false,
                    _ => true,
                },
                &mut |_| true,
            )
            .unwrap()
        {
            let (len, kind) = store.get(&id, &mut scratch).unwrap();
            assert_eq!(kind, BlockType::ValueIndexData);
            assert_eq!(len, 8);
            found |= u64::from_le_bytes(scratch[..8].try_into().unwrap()) == 512;
            candidates += 1;
        }
        assert!(found);
        assert!(candidates < 64, "{candidates} candidate leaves");
    }

    #[test]
    fn malformed_node_headers_counts_bounds_and_child_height_are_loud() {
        let mut budget = Budget::new(8 << 20);
        let mut store = MemoryBlockStore::new(&mut budget, "bad navigation", 4 << 20, 128).unwrap();
        let mut writer = NavigationWriter::new();
        writer.reset(0, false);
        let data = store.put(b"data", BlockType::ValueIndexData, 0).unwrap();
        for (id, entries) in [(data, 0), (BlockId([0; 32]), 1)] {
            assert!(matches!(
                writer.append(&mut store, id, entries, SpatialBounds::Empty),
                Err(ValueIndexError::Corrupt)
            ));
        }
        writer
            .append(&mut store, data, 1, SpatialBounds::new(1.0, 2.0, 3.0, 4.0))
            .unwrap();
        let root = writer.finish(&mut store).unwrap();
        let mut original = [0; NODE_BYTES];
        let (len, _) = store.get(&root, &mut original).unwrap();
        let mut scratch = vec![0; super::super::MAX_PAYLOAD];
        for (at, value) in [
            (0, 2),
            (1, 14),
            (2, 32),
            (3, 2),
            (4, 33),
            (6, 1),
            (HEADER + 32, 0),
            (HEADER + 40, 5),
        ] {
            let mut bytes = original;
            bytes[at] = value;
            let bad = store
                .put(&bytes[..len], BlockType::ValueIndexNavigationV1, 0)
                .unwrap();
            assert!(
                matches!(
                    NavigationCursor::new(bad, 1).next(
                        &mut store,
                        &mut scratch,
                        &mut |_, _| true,
                        &mut |_| true
                    ),
                    Err(ValueIndexError::Corrupt)
                ),
                "byte {at}"
            );
        }
        let mut bytes = original;
        bytes[HEADER + 41..HEADER + 49].copy_from_slice(&f64::NAN.to_le_bytes());
        let bad = store
            .put(&bytes[..len], BlockType::ValueIndexNavigationV1, 0)
            .unwrap();
        assert!(matches!(
            NavigationCursor::new(bad, 1).next(
                &mut store,
                &mut scratch,
                &mut |_, _| true,
                &mut |_| true
            ),
            Err(ValueIndexError::Corrupt)
        ));
        let mut bytes = original;
        bytes[1] = 1;
        let bad = store
            .put(&bytes[..len], BlockType::ValueIndexNavigationV1, 0)
            .unwrap();
        assert!(matches!(
            NavigationCursor::new(bad, 1).next(
                &mut store,
                &mut scratch,
                &mut |_, _| true,
                &mut |_| true
            ),
            Err(ValueIndexError::Corrupt)
        ));
        assert!(matches!(
            NavigationCursor::new(root, 2).next(
                &mut store,
                &mut scratch,
                &mut |_, _| true,
                &mut |_| true
            ),
            Err(ValueIndexError::Corrupt)
        ));
    }
}
