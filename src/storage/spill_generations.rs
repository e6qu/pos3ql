//! Owned immutable row and index files retained until their readers release them.

use core::cell::Cell;
use core::mem::size_of;
use core::num::{NonZeroU64, NonZeroUsize};
use core::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use {std::sync::Arc, crate::mem::mutex::Mutex, std::sync::RwLock, std::sync::RwLockReadGuard, std::sync::RwLockWriteGuard};

use super::{ColType, MAX_COLUMNS, RelationPersistence, SpillReader, Storage, table_slot_capacity};
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::store::{BlockId, SstHandle, ValueIndexHandle};

/// A table-local saved-file identity; never a position in its current file list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct SpillGenerationId(u32);

impl SpillGenerationId {
    fn checked(value: u64) -> Result<Self, SqlError> {
        u32::try_from(value).map(Self).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "table SST generation identity space exhausted"
            )
        })
    }

    pub(super) const fn encoded(self) -> u32 {
        self.0
    }

    pub(super) const fn from_record(value: u32) -> Self {
        Self(value)
    }

    #[cfg(test)]
    pub(crate) const fn fixture(value: u32) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SpillGenerationRange {
    first: SpillGenerationId,
    last: SpillGenerationId,
}

impl SpillGenerationRange {
    pub(super) const fn single(id: SpillGenerationId) -> Self {
        Self {
            first: id,
            last: id,
        }
    }

    fn contains(self, id: SpillGenerationId) -> bool {
        self.first <= id && id <= self.last
    }
}

/// Handles and their eviction boundaries are published as one complete list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SpillGeneration {
    pub(crate) handle: SstHandle,
    coverage: SpillCoverage,
    range: SpillGenerationRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SpillCoverage {
    Unknown,
    CommittedThrough(u64),
    ResidentThroughGeneration(u64),
}

impl SpillCoverage {
    fn compatible(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::Unknown, _)
                | (_, Self::Unknown)
                | (Self::CommittedThrough(_), Self::CommittedThrough(_))
                | (
                    Self::ResidentThroughGeneration(_),
                    Self::ResidentThroughGeneration(_)
                )
        )
    }

    fn merged(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unknown, coverage) | (coverage, Self::Unknown) => coverage,
            (Self::CommittedThrough(left), Self::CommittedThrough(right)) => {
                Self::CommittedThrough(left.max(right))
            }
            (Self::ResidentThroughGeneration(left), Self::ResidentThroughGeneration(right)) => {
                Self::ResidentThroughGeneration(left.max(right))
            }
            _ => panic!("temporary and durable SST coverage cannot mix"),
        }
    }
}

pub(crate) struct SpillGenerationList {
    members: FixedVec<SpillGeneration>,
    staging: FixedVec<SpillGeneration>,
    next_id: u64,
    installed_before: u64,
}

impl SpillGenerationList {
    pub(crate) fn len(&self) -> usize {
        self.members.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub(crate) fn capacity(&self) -> usize {
        self.members.capacity()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &SpillGeneration> {
        self.members.iter()
    }

    pub(crate) fn handle(&self, index: usize) -> Option<SstHandle> {
        self.members.get(index).map(|member| member.handle)
    }

    pub(super) fn reserve_id(&mut self) -> Result<SpillGenerationId, SqlError> {
        let id = SpillGenerationId::checked(self.next_id)?;
        self.next_id += 1;
        Ok(id)
    }

    fn validate_reservation(&self, id: SpillGenerationId) -> Result<(), SqlError> {
        if u64::from(id.0) >= self.next_id || u64::from(id.0) < self.installed_before {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "SST generation identity is not a fresh reservation"
            ));
        }
        Ok(())
    }

    pub(super) fn eviction_member(
        &self,
        commit_lsn: u64,
        row_generation: u64,
    ) -> Option<SpillGenerationId> {
        self.members
            .iter()
            .find(|member| match member.coverage {
                SpillCoverage::Unknown => false,
                SpillCoverage::CommittedThrough(through) => commit_lsn <= through,
                SpillCoverage::ResidentThroughGeneration(generation) => {
                    row_generation <= generation
                }
            })
            .map(|member| member.range.last)
    }

    pub(super) fn published_through(&self) -> Option<u64> {
        self.members
            .iter()
            .filter_map(|member| match member.coverage {
                SpillCoverage::CommittedThrough(through) => Some(through),
                _ => None,
            })
            .max()
    }

    pub(super) fn clear(&mut self) {
        self.members.clear();
        self.staging.clear();
    }

    pub(super) fn replace(
        &mut self,
        len: usize,
        handles: impl Iterator<Item = SstHandle>,
    ) -> Result<(), SqlError> {
        self.staging.clear();
        if len > self.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "table SST generations exceed startup capacity ({})",
                self.capacity()
            ));
        }
        if len != 0 {
            SpillGenerationId::checked(self.next_id + len as u64 - 1)?;
        }
        for handle in handles {
            if self.staging.len() == len {
                self.staging.clear();
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "SST generation iterator exceeds declared length"
                ));
            }
            self.staging
                .push(SpillGeneration {
                    handle,
                    coverage: SpillCoverage::Unknown,
                    range: SpillGenerationRange::single(SpillGenerationId::checked(
                        self.next_id + self.staging.len() as u64,
                    )?),
                })
                .expect("validated generation capacity");
        }
        if self.staging.len() != len {
            self.staging.clear();
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "SST generation iterator is shorter than declared length"
            ));
        }
        std::mem::swap(&mut self.members, &mut self.staging);
        self.next_id += len as u64;
        if len != 0 {
            self.installed_before = self.next_id;
        }
        self.staging.clear();
        Ok(())
    }

    pub(super) fn copy_from(&mut self, source: &Self) -> Result<(), SqlError> {
        if source.len() > self.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "cloned SST generations exceed startup capacity ({})",
                self.capacity()
            ));
        }
        self.clear();
        for &member in source.iter() {
            self.members.push(member).expect("validated clone capacity");
        }
        self.next_id = source.next_id;
        self.installed_before = source.installed_before;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn append(&mut self, handle: SstHandle, through_lsn: u64) -> Result<(), SqlError> {
        self.append_covered(handle, SpillCoverage::CommittedThrough(through_lsn))
    }

    #[cfg(test)]
    pub(super) fn append_covered(
        &mut self,
        handle: SstHandle,
        coverage: SpillCoverage,
    ) -> Result<(), SqlError> {
        let id = self.reserve_id()?;
        self.append_reserved(handle, coverage, id)
    }

    pub(super) fn append_reserved(
        &mut self,
        handle: SstHandle,
        coverage: SpillCoverage,
        id: SpillGenerationId,
    ) -> Result<(), SqlError> {
        self.validate_reservation(id)?;
        if self
            .members
            .iter()
            .any(|member| !member.coverage.compatible(coverage))
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "temporary and durable SST coverage cannot mix"
            ));
        }
        self.members
            .push(SpillGeneration {
                handle,
                coverage,
                range: SpillGenerationRange::single(id),
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table SST generations exceed startup capacity ({})",
                    self.capacity()
                )
            })?;
        self.installed_before = u64::from(id.0) + 1;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn collapse(&mut self, handle: SstHandle, through_lsn: u64) -> Result<(), SqlError> {
        self.collapse_covered(handle, SpillCoverage::CommittedThrough(through_lsn))
    }

    #[cfg(test)]
    pub(super) fn collapse_covered(
        &mut self,
        handle: SstHandle,
        coverage: SpillCoverage,
    ) -> Result<(), SqlError> {
        let id = self.reserve_id()?;
        self.collapse_reserved(handle, coverage, id)
    }

    pub(super) fn collapse_reserved(
        &mut self,
        handle: SstHandle,
        coverage: SpillCoverage,
        id: SpillGenerationId,
    ) -> Result<(), SqlError> {
        self.validate_reservation(id)?;
        if self.capacity() == 0 {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "table SST generations exceed startup capacity (0)"
            ));
        }
        let first = self.members.first().map_or(id, |member| member.range.first);
        self.clear();
        self.members
            .push(SpillGeneration {
                handle,
                coverage,
                range: SpillGenerationRange { first, last: id },
            })
            .expect("validated generation capacity");
        self.installed_before = u64::from(id.0) + 1;
        Ok(())
    }

    pub(super) fn merge_pair(&mut self, at: usize, handle: Option<SstHandle>) {
        assert!(
            at.checked_add(1).is_some_and(|next| next < self.len()),
            "merge pair is outside the installed generation list"
        );
        let coverage = self.members[at]
            .coverage
            .merged(self.members[at + 1].coverage);
        let removed = if let Some(handle) = handle {
            let range = SpillGenerationRange {
                first: self.members[at].range.first,
                last: self.members[at + 1].range.last,
            };
            self.members[at] = SpillGeneration {
                handle,
                coverage,
                range,
            };
            1
        } else {
            2
        };
        let destination = at + 2 - removed;
        self.members
            .as_mut_slice()
            .copy_within(at + 2.., destination);
        for _ in 0..removed {
            self.members.pop();
        }
    }
}

pub(super) struct SpillGenerationRoster {
    state: RwLock<SpillGenerationList>,
}

#[derive(Clone)]
pub(crate) struct SpillPublication {
    registry: Arc<SpillRootRegistry>,
    table: usize,
    created_at: u64,
    rewrite_generation: u64,
}

#[derive(Clone)]
pub(crate) struct SpillGenerationReservation {
    pub(super) target: SpillPublication,
    pub(super) id: SpillGenerationId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpillInstallation {
    Installed,
    RetiredTable,
}

impl SpillPublication {
    pub(super) fn capture(
        storage: &Storage,
        table: usize,
        created_at: u64,
        rewrite_generation: u64,
    ) -> Result<Self, SqlError> {
        let target = Self {
            registry: Arc::clone(&storage.spill_roots),
            table,
            created_at,
            rewrite_generation,
        };
        if target.with_table(storage, |_| Ok(()))?.is_none() {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "SST publication table identity changed"
            ));
        }
        Ok(target)
    }

    pub(super) fn with_table<R>(
        &self,
        storage: &Storage,
        publish: impl FnOnce(&super::Table) -> Result<R, SqlError>,
    ) -> Result<Option<R>, SqlError> {
        if !Arc::ptr_eq(&self.registry, &storage.spill_roots) {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "SST publication belongs to different storage"
            ));
        }
        let table = storage.tables.get(self.table).ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "SST publication table slot is invalid"
            )
        })?;
        let _lifecycle = table
            .lifecycle
            .lock()
            .expect("table lifecycle lock poisoned");
        if !table.live()
            || table.created_at() != self.created_at
            || table.rewrite_generation() != self.rewrite_generation
        {
            return Ok(None);
        }
        publish(table).map(Some)
    }
}

impl SpillGenerationRoster {
    pub(super) fn member_budget_bytes(capacity: usize) -> usize {
        2 * capacity * size_of::<SpillGeneration>()
    }

    pub(super) fn new(budget: &mut Budget, capacity: usize) -> Result<Self, BudgetError> {
        Ok(Self {
            state: RwLock::new(SpillGenerationList {
                members: FixedVec::new(budget, "table SST generations", capacity)?,
                staging: FixedVec::new(budget, "table SST generation staging", capacity)?,
                next_id: 0,
                installed_before: 0,
            }),
        })
    }

    pub(super) fn try_read_for_batch(
        &self,
    ) -> Result<RwLockReadGuard<'_, SpillGenerationList>, super::row_ownership::LockWait<'_>> {
        super::row_ownership::try_read(&self.state, "table SST generation lock poisoned")
    }

    pub(super) fn read(&self) -> RwLockReadGuard<'_, SpillGenerationList> {
        self.state
            .read()
            .expect("table SST generation lock poisoned")
    }

    pub(super) fn write(&self) -> RwLockWriteGuard<'_, SpillGenerationList> {
        self.state
            .write()
            .expect("table SST generation lock poisoned")
    }
}

trait RetainedFileRoot: Copy + Eq {
    const ROOT_BUDGET: &'static str;
    const MEMBERSHIP_BUDGET: &'static str;
    const CAPACITY_ERROR: &'static str;
    const REFERENCE_ERROR: &'static str;

    fn durable(self) -> bool;
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RowFileRoot {
    handle: SstHandle,
    durable: bool,
}

impl RetainedFileRoot for RowFileRoot {
    const ROOT_BUDGET: &'static str = "retained SST roots";
    const MEMBERSHIP_BUDGET: &'static str = "statement file membership";
    const CAPACITY_ERROR: &'static str = "retained SST roots exceed the startup capacity";
    const REFERENCE_ERROR: &'static str = "retained SST reader count exhausted";

    fn durable(self) -> bool {
        self.durable
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct IndexFileRoot(BlockId);

impl RetainedFileRoot for IndexFileRoot {
    const ROOT_BUDGET: &'static str = "retained index files";
    const MEMBERSHIP_BUDGET: &'static str = "statement index file membership";
    const CAPACITY_ERROR: &'static str = "retained index files exceed the startup capacity";
    const REFERENCE_ERROR: &'static str = "retained index file reader count exhausted";

    fn durable(self) -> bool {
        true
    }
}

struct PinnedRoot<Root> {
    handle: Root,
    readers: NonZeroUsize,
    garbage_retained: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StatementFileIdentity {
    slot: usize,
    serial: NonZeroU64,
}

struct StatementFileOwner {
    serial: NonZeroU64,
    references: NonZeroUsize,
    has_roots: bool,
    type_definitions: Option<super::TypeDefinitionImages>,
    definition_parent: Option<StatementFileIdentity>,
    definition_transaction: Option<u32>,
    definition_database: Option<super::DatabaseOid>,
}

struct RetainedRootPool<Root> {
    roots: FixedVec<PinnedRoot<Root>>,
    members: FixedVec<u64>,
    words_per_root: usize,
}

impl<Root> Deref for RetainedRootPool<Root> {
    type Target = FixedVec<PinnedRoot<Root>>;

    fn deref(&self) -> &Self::Target {
        &self.roots
    }
}

impl<Root> DerefMut for RetainedRootPool<Root> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.roots
    }
}

impl<Root: RetainedFileRoot> RetainedRootPool<Root> {
    fn budget_bytes(capacity: usize, statement_capacity: usize) -> usize {
        capacity
            .saturating_mul(size_of::<PinnedRoot<Root>>())
            .saturating_add(
                capacity
                    .saturating_mul(statement_capacity.div_ceil(64))
                    .saturating_mul(size_of::<u64>()),
            )
    }

    fn new(
        budget: &mut Budget,
        capacity: usize,
        statement_capacity: usize,
    ) -> Result<Self, BudgetError> {
        let roots = FixedVec::new(budget, Root::ROOT_BUDGET, capacity)?;
        let words_per_root = statement_capacity.div_ceil(64);
        let member_capacity = capacity.saturating_mul(words_per_root);
        let mut members = FixedVec::new(budget, Root::MEMBERSHIP_BUDGET, member_capacity)?;
        for _ in 0..member_capacity {
            members.push(0).expect("reserved statement file membership");
        }
        Ok(Self {
            roots,
            members,
            words_per_root,
        })
    }

    fn contains_statement(&self, root: usize, statement: StatementFileIdentity) -> bool {
        self.members[root * self.words_per_root + statement.slot / 64]
            & (1u64 << (statement.slot % 64))
            != 0
    }

    fn set_statement(&mut self, root: usize, statement: StatementFileIdentity, present: bool) {
        let word = &mut self.members[root * self.words_per_root + statement.slot / 64];
        let mask = 1u64 << (statement.slot % 64);
        if present {
            *word |= mask;
        } else {
            *word &= !mask;
        }
    }

    /// Dense removal moves the membership row with its immutable root.
    fn remove_root(&mut self, position: usize) -> PinnedRoot<Root> {
        let last = self.roots.len() - 1;
        let width = self.words_per_root;
        debug_assert!(
            self.members[position * width..(position + 1) * width]
                .iter()
                .all(|word| *word == 0)
        );
        self.members
            .copy_within(last * width..(last + 1) * width, position * width);
        self.members[last * width..(last + 1) * width].fill(0);
        self.roots.swap_remove(position)
    }

    fn pin(
        &mut self,
        keys: impl Iterator<Item = Root> + Clone,
        statement: Option<StatementFileIdentity>,
    ) -> Result<(), SqlError> {
        let mut needed = 0;
        for (index, key) in keys.clone().enumerate() {
            if !self.roots.iter().any(|root| root.handle == key)
                && !keys.clone().take(index).any(|prior| prior == key)
            {
                needed += 1;
            }
        }
        for (position, root) in self.roots.iter().enumerate() {
            let count = keys.clone().filter(|key| *key == root.handle).count();
            let statement_pin = count != 0
                && statement.is_some_and(|identity| !self.contains_statement(position, identity));
            if root
                .readers
                .checked_add(count)
                .and_then(|count| count.checked_add(usize::from(statement_pin)))
                .is_none()
            {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "{}",
                    Root::REFERENCE_ERROR
                ));
            }
        }
        if needed > self.roots.capacity() - self.roots.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "{} ({})",
                Root::CAPACITY_ERROR,
                self.roots.capacity()
            ));
        }
        for key in keys {
            if let Some(position) = self.roots.iter().position(|root| root.handle == key) {
                let statement_pin =
                    statement.is_some_and(|identity| !self.contains_statement(position, identity));
                self.roots[position].readers = self.roots[position]
                    .readers
                    .checked_add(1 + usize::from(statement_pin))
                    .expect("preflighted reader count");
                if let Some(identity) = statement {
                    self.set_statement(position, identity, true);
                }
            } else {
                let position = self.roots.len();
                self.roots
                    .push(PinnedRoot {
                        handle: key,
                        readers: NonZeroUsize::new(1 + usize::from(statement.is_some()))
                            .expect("nonzero"),
                        garbage_retained: false,
                    })
                    .expect("preflighted root capacity");
                if let Some(identity) = statement {
                    self.set_statement(position, identity, true);
                }
            }
        }
        Ok(())
    }

    fn release_one(&mut self, position: usize) -> bool {
        if let Some(remaining) = NonZeroUsize::new(self.roots[position].readers.get() - 1) {
            self.roots[position].readers = remaining;
            false
        } else {
            let retired = self.remove_root(position);
            retired.handle.durable() && retired.garbage_retained
        }
    }

    fn release(&mut self, keys: impl Iterator<Item = Root>) -> bool {
        let mut reclamation = false;
        for key in keys {
            let position = self
                .roots
                .iter()
                .position(|root| root.handle == key)
                .expect("reader retains its root");
            reclamation |= self.release_one(position);
        }
        reclamation
    }

    fn release_statement(&mut self, identity: StatementFileIdentity) -> bool {
        let mut reclamation = false;
        let mut position = 0;
        while position < self.roots.len() {
            if self.contains_statement(position, identity) {
                self.set_statement(position, identity, false);
                let removed = self.roots[position].readers.get() == 1;
                reclamation |= self.release_one(position);
                if removed {
                    continue;
                }
            }
            position += 1;
        }
        reclamation
    }
}

struct RetainedRootState {
    roots: RetainedRootPool<RowFileRoot>,
    index_roots: RetainedRootPool<IndexFileRoot>,
    statements: FixedVec<Option<StatementFileOwner>>,
    next_statement: u64,
    type_selections: Box<[core::mem::MaybeUninit<usize>]>,
}

impl Deref for RetainedRootState {
    type Target = RetainedRootPool<RowFileRoot>;

    fn deref(&self) -> &Self::Target {
        &self.roots
    }
}

impl DerefMut for RetainedRootState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.roots
    }
}

impl RetainedRootState {
    fn statement_mut(&mut self, identity: StatementFileIdentity) -> &mut StatementFileOwner {
        self.statements[identity.slot]
            .as_mut()
            .filter(|owner| owner.serial == identity.serial)
            .expect("statement file lease owns a live identity")
    }
}

#[derive(Clone, Copy)]
struct SelectedStatementFiles {
    registry: usize,
    identity: StatementFileIdentity,
    selection_identity: StatementFileIdentity,
}

thread_local! {
    static SELECTED_STATEMENT_FILES: Cell<Option<SelectedStatementFiles>> = const { Cell::new(None) };
    static STATEMENT_FILE_SCOPE_DEPTH: Cell<usize> = const { Cell::new(0) };
}

#[must_use = "the statement must retain its selected files until execution ends"]
pub(crate) struct StatementFileLease {
    registry: Arc<SpillRootRegistry>,
    identity: StatementFileIdentity,
}

/// Only a validated statement selection can name a cached definition image.
pub(super) struct StatementDefinitionIndex<'lease, Definition> {
    index: usize,
    owner: &'lease super::TypeDefinitionImages,
    _lease: core::marker::PhantomData<&'lease StatementFileLease>,
    _definition: core::marker::PhantomData<fn() -> Definition>,
}

impl<'lease, Definition> StatementDefinitionIndex<'lease, Definition> {
    fn new(
        _lease: &'lease StatementFileLease,
        owner: &'lease super::TypeDefinitionImages,
        index: usize,
    ) -> Self {
        Self {
            index,
            owner,
            _lease: core::marker::PhantomData,
            _definition: core::marker::PhantomData,
        }
    }
    pub(super) fn index(self, owner: &super::TypeDefinitionImages) -> Result<usize, SqlError> {
        if !core::ptr::eq(self.owner, owner) {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "selected type image belongs to a different statement root"
            ));
        }
        Ok(self.index)
    }
}

/// A nested execution adds its selected files to the outer statement owner.
/// The scope owns a reference, so moving or releasing a snapshot cannot retire
/// its files while execution still uses the selected context.
#[must_use = "file selection must stay scoped to the executing statement"]
pub(crate) struct StatementFileScope {
    _lease: StatementFileLease,
    _definition_lease: Option<StatementFileLease>,
    previous: Option<SelectedStatementFiles>,
    depth: usize,
    thread: core::marker::PhantomData<std::rc::Rc<()>>,
}

impl StatementFileLease {
    pub(super) fn type_definition<Definition: 'static>(
        &self,
        storage: &Storage,
        target: super::type_definitions::TypeDefinitionTarget,
        transaction: u32,
        select: impl for<'images, 'selection> FnOnce(
            &'images super::TypeDefinitionImages,
            Option<StatementDefinitionIndex<'selection, Definition>>,
        ) -> Result<
            Option<
                super::definition_pool::DefinitionImageHandle<
                    super::type_definitions::TypeImageKey,
                    Definition,
                >,
            >,
            SqlError,
        >,
    ) -> Result<Option<super::type_definitions::TypeDefinitionRead<Definition>>, SqlError> {
        if !Arc::ptr_eq(&self.registry, &storage.spill_roots) {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "type definition lease belongs to different storage"
            ));
        }
        let source_slot = self
            .registry
            .type_source_index(target.slot())
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "selected type source is outside its catalog"
                )
            })?;
        let image = {
            let mut state = self
                .registry
                .roots
                .lock()
                .expect("retained file root lock poisoned");
            let database = super::current_database();
            if state
                .statement_mut(self.identity)
                .definition_database
                .is_some_and(|owner| owner != database)
            {
                return Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "statement type definitions belong to a different database"
                ));
            }
            let root = self.definition_root(&mut state, storage)?;
            let width = self.registry.type_offsets[3];
            let start = self.identity.slot * width;
            if state.statement_mut(self.identity).definition_transaction != Some(transaction) {
                for selection in &mut state.type_selections[start..start + width] {
                    selection.write(usize::MAX);
                }
                let owner = state.statement_mut(self.identity);
                owner.definition_transaction = Some(transaction);
                owner.definition_database = Some(database);
            }
            let position = start + source_slot;
            // SAFETY: the transaction flag is set only after the complete
            // selection slice is initialized. Registry ownership serializes it.
            let previous = unsafe { state.type_selections[position].assume_init() };
            if previous == usize::MAX - 1 {
                return match target {
                    super::type_definitions::TypeDefinitionTarget::Slot(_) => Ok(None),
                    super::type_definitions::TypeDefinitionTarget::Bound(_) => Err(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "captured type source is absent from the statement"
                    )),
                };
            }
            let images = state
                .statement_mut(root)
                .type_definitions
                .as_ref()
                .expect("type retention root owns its image chains");
            let selected = (previous != usize::MAX)
                .then(|| StatementDefinitionIndex::new(self, images, previous));
            let Some(image) = select(images, selected)? else {
                state.type_selections[position].write(usize::MAX - 1);
                return Ok(None);
            };
            state.type_selections[position].write(image.index());
            image
        };
        let lease = self.try_clone()?;
        // SAFETY: the original statement reference retains either its own
        // immutable chains or a counted direct parent. Cached indices come
        // only from the same root's correctly typed source image selector.
        Ok(Some(unsafe {
            super::type_definitions::TypeDefinitionRead::new(lease, image)
        }))
    }

    fn definition_root(
        &self,
        state: &mut RetainedRootState,
        storage: &Storage,
    ) -> Result<StatementFileIdentity, SqlError> {
        let owner = state.statement_mut(self.identity);
        if let Some(root) = owner.definition_parent {
            return Ok(root);
        }
        if owner.type_definitions.is_some() {
            return Ok(self.identity);
        }
        let selected = SELECTED_STATEMENT_FILES
            .with(Cell::get)
            .filter(|selected| selected.registry == Arc::as_ptr(&self.registry) as usize)
            .map_or(self.identity, |selected| selected.identity);
        let root = state
            .statement_mut(selected)
            .definition_parent
            .unwrap_or(selected);
        let root_owner = state.statement_mut(root);
        assert!(
            root_owner.definition_parent.is_none(),
            "type definition parents are direct roots"
        );
        let references = if root != self.identity {
            Some(root_owner.references.checked_add(1).ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement definition references exhausted"
                )
            })?)
        } else {
            None
        };
        root_owner
            .type_definitions
            .get_or_insert_with(|| storage.type_definition_images());
        if let Some(references) = references {
            root_owner.references = references;
            state.statement_mut(self.identity).definition_parent = Some(root);
        }
        Ok(root)
    }

    pub(super) fn try_clone(&self) -> Result<Self, SqlError> {
        let mut state = self
            .registry
            .roots
            .lock()
            .expect("retained file root lock poisoned");
        let owner = state.statement_mut(self.identity);
        owner.references = owner.references.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "statement file references exhausted"
            )
        })?;
        Ok(Self {
            registry: Arc::clone(&self.registry),
            identity: self.identity,
        })
    }

    pub(crate) fn enter(&self) -> Result<StatementFileScope, SqlError> {
        let previous = SELECTED_STATEMENT_FILES.with(Cell::get);
        let depth = STATEMENT_FILE_SCOPE_DEPTH
            .with(Cell::get)
            .checked_add(1)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement file scope nesting exhausted"
                )
            })?;
        let registry = Arc::as_ptr(&self.registry) as usize;
        let identity = previous
            .filter(|selected| selected.registry == registry)
            .map_or(self.identity, |selected| selected.identity);
        let mut state = self
            .registry
            .roots
            .lock()
            .expect("retained SST root lock poisoned");
        let references = state
            .statement_mut(identity)
            .references
            .checked_add(1)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement file references exhausted"
                )
            })?;
        let definition_references = if identity != self.identity {
            Some(
                state
                    .statement_mut(self.identity)
                    .references
                    .checked_add(1)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "statement definition references exhausted"
                        )
                    })?,
            )
        } else {
            None
        };
        state.statement_mut(identity).references = references;
        if let Some(references) = definition_references {
            state.statement_mut(self.identity).references = references;
        }
        drop(state);
        SELECTED_STATEMENT_FILES.with(|selected| {
            selected.set(Some(SelectedStatementFiles {
                registry,
                identity,
                selection_identity: self.identity,
            }));
        });
        STATEMENT_FILE_SCOPE_DEPTH.with(|current| current.set(depth));
        Ok(StatementFileScope {
            _lease: Self {
                registry: Arc::clone(&self.registry),
                identity,
            },
            _definition_lease: definition_references.map(|_| Self {
                registry: Arc::clone(&self.registry),
                identity: self.identity,
            }),
            previous,
            depth,
            thread: core::marker::PhantomData,
        })
    }
}

impl Drop for StatementFileScope {
    fn drop(&mut self) {
        // An out-of-order exit must not install a retired owner's context.
        STATEMENT_FILE_SCOPE_DEPTH.with(|depth| {
            assert_eq!(
                depth.get(),
                self.depth,
                "statement file scopes must exit in reverse entry order"
            );
            depth.set(self.depth - 1);
        });
        SELECTED_STATEMENT_FILES.with(|selected| selected.set(self.previous));
    }
}

impl Drop for StatementFileLease {
    fn drop(&mut self) {
        self.registry.release_statement(self.identity);
    }
}

#[must_use = "the selected index generation must keep its files alive"]
pub(super) struct RetainedIndexFile {
    registry: Arc<SpillRootRegistry>,
    handle: ValueIndexHandle,
}

impl RetainedIndexFile {
    pub(super) fn handle(&self) -> ValueIndexHandle {
        self.handle
    }
}

impl Drop for RetainedIndexFile {
    fn drop(&mut self) {
        self.registry.release_index_file(self.handle);
    }
}

pub(super) struct SpillRootRegistry {
    roots: Mutex<RetainedRootState>,
    reclamation_pending: AtomicBool,
    type_offsets: [usize; 4],
}

impl SpillRootRegistry {
    fn type_offsets(config: &Config) -> [usize; 4] {
        let enums = config.max_domains;
        let composites = enums.saturating_add(config.max_enums);
        [
            0,
            enums,
            composites,
            composites.saturating_add(config.max_composites),
        ]
    }

    pub(super) fn capacity(config: &Config) -> usize {
        table_slot_capacity(config)
            .saturating_mul(config.max_spill_generations_per_table)
            .saturating_mul(2)
    }

    pub(super) fn index_capacity(config: &Config) -> usize {
        config.max_value_indexes.saturating_mul(2)
    }

    pub(super) fn budget_bytes(config: &Config) -> usize {
        let statements = super::statement_snapshot_capacity(config);
        (size_of::<Self>() + 2 * size_of::<std::sync::atomic::AtomicUsize>())
            .saturating_add(statements.saturating_mul(size_of::<Option<StatementFileOwner>>()))
            .saturating_add(
                statements
                    .saturating_mul(Self::type_offsets(config)[3])
                    .saturating_mul(size_of::<usize>()),
            )
            .saturating_add(RetainedRootPool::<RowFileRoot>::budget_bytes(
                Self::capacity(config),
                statements,
            ))
            .saturating_add(RetainedRootPool::<IndexFileRoot>::budget_bytes(
                Self::index_capacity(config),
                statements,
            ))
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Arc<Self>, BudgetError> {
        budget.draw(
            size_of::<Self>() + 2 * size_of::<std::sync::atomic::AtomicUsize>(),
            "retained file root owner",
        )?;
        let statement_capacity = super::statement_snapshot_capacity(config);
        let type_offsets = Self::type_offsets(config);
        let type_selection_count = statement_capacity.saturating_mul(type_offsets[3]);
        budget.draw_array(
            type_selection_count,
            size_of::<usize>(),
            "statement type definition selections",
        )?;
        let type_selections = Box::<[usize]>::new_uninit_slice(type_selection_count);
        let roots = RetainedRootPool::new(budget, Self::capacity(config), statement_capacity)?;
        let index_roots =
            RetainedRootPool::new(budget, Self::index_capacity(config), statement_capacity)?;
        let mut statements = FixedVec::new(budget, "statement file owners", statement_capacity)?;
        for _ in 0..statement_capacity {
            statements
                .push(None)
                .expect("reserved statement file owners");
        }
        Ok(Arc::new(Self {
            roots: Mutex::new(RetainedRootState {
                roots,
                index_roots,
                statements,
                next_statement: 0,
                type_selections,
            }),
            reclamation_pending: AtomicBool::new(false),
            type_offsets,
        }))
    }

    pub(super) fn retain_statement(self: &Arc<Self>) -> Result<StatementFileLease, SqlError> {
        let mut state = self.roots.lock().expect("retained SST root lock poisoned");
        let serial = state
            .next_statement
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement file identity exhausted"
                )
            })?;
        let slot = state
            .statements
            .iter()
            .position(Option::is_none)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement file owners are exhausted ({})",
                    state.statements.len()
                )
            })?;
        state.statements[slot] = Some(StatementFileOwner {
            serial,
            references: NonZeroUsize::new(1).expect("nonzero"),
            has_roots: false,
            type_definitions: None,
            definition_parent: None,
            definition_transaction: None,
            definition_database: None,
        });
        state.next_statement = serial.get();
        Ok(StatementFileLease {
            registry: Arc::clone(self),
            identity: StatementFileIdentity { slot, serial },
        })
    }

    fn release_statement(&self, identity: StatementFileIdentity) {
        let mut state = self.roots.lock().expect("retained file root lock poisoned");
        let mut next = Some(identity);
        while let Some(identity) = next.take() {
            let owner = state.statement_mut(identity);
            if let Some(remaining) = NonZeroUsize::new(owner.references.get() - 1) {
                owner.references = remaining;
                break;
            }
            let has_roots = owner.has_roots;
            next = owner.definition_parent;
            if has_roots {
                let rows = state.roots.release_statement(identity);
                let indexes = state.index_roots.release_statement(identity);
                if rows || indexes {
                    self.reclamation_pending.store(true, Ordering::Release);
                }
            }
            state.statements[identity.slot] = None;
        }
    }

    /// Nested readers join the active statement; a standalone callback owns a
    /// separate statement reference for exactly the callback's duration.
    pub(super) fn type_definition_reader(self: &Arc<Self>) -> Result<StatementFileLease, SqlError> {
        let mut state = self.roots.lock().expect("retained file root lock poisoned");
        let selected = SELECTED_STATEMENT_FILES
            .with(Cell::get)
            .filter(|selected| selected.registry == Arc::as_ptr(self) as usize)
            .map(|selected| selected.selection_identity);
        let Some(identity) = selected else {
            drop(state);
            return self.retain_statement();
        };
        let owner = state.statement_mut(identity);
        owner.references = owner
            .references
            .get()
            .checked_add(1)
            .and_then(NonZeroUsize::new)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement file references exhausted"
                )
            })?;
        Ok(StatementFileLease {
            registry: Arc::clone(self),
            identity,
        })
    }

    fn type_source_index(&self, source: super::NamedTypeSlot) -> Option<usize> {
        let (kind, index) = match source {
            super::NamedTypeSlot::Domain(index) => (0, index),
            super::NamedTypeSlot::Enum(index) => (1, index),
            super::NamedTypeSlot::Composite(index) => (2, index),
        };
        (index < self.type_offsets[kind + 1] - self.type_offsets[kind])
            .then(|| self.type_offsets[kind] + index)
    }

    fn selected_statement(&self, state: &mut RetainedRootState) -> Option<StatementFileIdentity> {
        let statement = SELECTED_STATEMENT_FILES
            .with(Cell::get)
            .filter(|selected| selected.registry == self as *const Self as usize)
            .map(|selected| selected.identity);
        if let Some(identity) = statement {
            state.statement_mut(identity);
        }
        statement
    }

    fn pin(&self, handles: &[SstHandle], durable: bool) -> Result<(), SqlError> {
        let mut state = self.roots.lock().expect("retained file root lock poisoned");
        let statement = self.selected_statement(&mut state);
        state.roots.pin(
            handles
                .iter()
                .copied()
                .map(|handle| RowFileRoot { handle, durable }),
            statement,
        )?;
        if !handles.is_empty()
            && let Some(identity) = statement
        {
            state.statement_mut(identity).has_roots = true;
        }
        Ok(())
    }

    fn release(&self, handles: &[SstHandle], durable: bool) {
        let mut state = self.roots.lock().expect("retained file root lock poisoned");
        if state.roots.release(
            handles
                .iter()
                .copied()
                .map(|handle| RowFileRoot { handle, durable }),
        ) {
            self.reclamation_pending.store(true, Ordering::Release);
        }
    }

    pub(super) fn retain_index_file(
        self: &Arc<Self>,
        handle: ValueIndexHandle,
    ) -> Result<RetainedIndexFile, SqlError> {
        let mut state = self.roots.lock().expect("retained file root lock poisoned");
        let statement = self.selected_statement(&mut state);
        state
            .index_roots
            .pin(core::iter::once(IndexFileRoot(handle.roster)), statement)?;
        if let Some(identity) = statement {
            state.statement_mut(identity).has_roots = true;
        }
        Ok(RetainedIndexFile {
            registry: Arc::clone(self),
            handle,
        })
    }

    fn release_index_file(&self, handle: ValueIndexHandle) {
        let mut state = self.roots.lock().expect("retained file root lock poisoned");
        if state
            .index_roots
            .release(core::iter::once(IndexFileRoot(handle.roster)))
        {
            self.reclamation_pending.store(true, Ordering::Release);
        }
    }

    pub(super) fn copy_roots(
        &self,
        durable: bool,
        output: &mut Vec<SstHandle>,
    ) -> Result<(), SqlError> {
        let mut roots = self.roots.lock().expect("retained SST root lock poisoned");
        output.clear();
        let count = roots
            .iter()
            .filter(|root| root.handle.durable == durable)
            .count();
        if count > output.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained SST root snapshot exceeds garbage collection scratch ({})",
                output.capacity()
            ));
        }
        for root in roots
            .iter_mut()
            .filter(|root| root.handle.durable == durable)
        {
            root.garbage_retained = true;
            output.push(root.handle.handle);
        }
        Ok(())
    }
    pub(super) fn copy_index_roots(&self, output: &mut Vec<BlockId>) -> Result<(), SqlError> {
        let mut state = self.roots.lock().expect("retained file root lock poisoned");
        output.clear();
        if state.index_roots.len() > output.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained index file snapshot exceeds garbage collection scratch ({})",
                output.capacity()
            ));
        }
        for root in state.index_roots.iter_mut() {
            root.garbage_retained = true;
            output.push(root.handle.0);
        }
        Ok(())
    }

    pub(super) fn reclamation_pending(&self) -> bool {
        self.reclamation_pending.load(Ordering::Acquire)
    }
    pub(super) fn take_reclamation_pending(&self) -> bool {
        self.reclamation_pending.swap(false, Ordering::AcqRel)
    }
}

pub(super) struct SpillRelation<'a> {
    pub(super) persistence: RelationPersistence,
    pub(super) schema: [ColType; MAX_COLUMNS],
    pub(super) n_columns: usize,
    pub(super) handles: &'a [SstHandle],
    pub(super) ranges: &'a [SpillGenerationRange],
}

impl<'a> SpillRelation<'a> {
    pub(super) fn len(&self) -> usize {
        self.handles.len()
    }

    pub(super) fn handle(&self, member: usize) -> SstHandle {
        self.handles[member]
    }

    pub(super) fn generation_id(&self, member: usize) -> SpillGenerationId {
        self.ranges[member].last
    }
}

struct QueryRootSlot {
    retained: AtomicBool,
    readers: AtomicUsize,
    snapshot: RwLock<SpillGenerationSnapshot>,
}

pub(super) struct QueryRootPool {
    slots: FixedVec<QueryRootSlot>,
    slots_per_use: usize,
}

pub(super) struct QueryRootLease {
    pool: Arc<QueryRootPool>,
    slot: usize,
}

impl QueryRootPool {
    fn slot_count(config: &Config) -> usize {
        config
            .query_workspace_slots
            .saturating_mul(super::MAX_ROW_WALK_NESTING)
    }

    pub(super) fn budget_bytes(config: &Config) -> usize {
        size_of::<Self>()
            + 2 * size_of::<std::sync::atomic::AtomicUsize>()
            + 2 * Self::slot_count(config)
                * (size_of::<QueryRootSlot>()
                    + SpillGenerationSnapshot::budget_bytes(config.max_spill_generations_per_table))
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Arc<Self>, BudgetError> {
        budget.draw(
            size_of::<Self>() + 2 * size_of::<std::sync::atomic::AtomicUsize>(),
            "query SST root owner",
        )?;
        let count = Self::slot_count(config);
        let mut slots = FixedVec::new(budget, "query SST root slots", 2 * count)?;
        for _ in 0..2 * count {
            budget.draw(
                SpillGenerationSnapshot::budget_bytes(config.max_spill_generations_per_table),
                "query SST root members",
            )?;
            slots
                .push(QueryRootSlot {
                    retained: AtomicBool::new(false),
                    readers: AtomicUsize::new(0),
                    snapshot: RwLock::new(SpillGenerationSnapshot::new(
                        config.max_spill_generations_per_table,
                    )),
                })
                .expect("sized query root slots");
        }
        Ok(Arc::new(Self {
            slots,
            slots_per_use: count,
        }))
    }

    #[cfg(test)]
    pub(super) fn retain(
        self: &Arc<Self>,
        storage: &Storage,
        table: usize,
        lsn: u64,
    ) -> Result<QueryRootLease, SqlError> {
        self.retain_in(storage, table, lsn, 0)
    }

    pub(super) fn retain_point(
        self: &Arc<Self>,
        storage: &Storage,
        table: usize,
        lsn: u64,
    ) -> Result<QueryRootLease, SqlError> {
        self.retain_in(storage, table, lsn, self.slots_per_use)
    }

    fn retain_in(
        self: &Arc<Self>,
        storage: &Storage,
        table: usize,
        lsn: u64,
        start: usize,
    ) -> Result<QueryRootLease, SqlError> {
        self.reserve_in(start, |snapshot| {
            snapshot.capture(storage, table)?;
            snapshot.commit_lsn = lsn;
            Ok(())
        })
    }

    fn retain_image(
        self: &Arc<Self>,
        storage: &Storage,
        image: SpillGenerationImage,
        generations: &SpillGenerationList,
    ) -> Result<QueryRootLease, SqlError> {
        self.reserve_in(0, |snapshot| {
            snapshot.capture_image(storage, image, generations)
        })
    }

    pub(super) fn retain_point_image(
        self: &Arc<Self>,
        storage: &Storage,
        image: SpillGenerationImage,
        generations: &SpillGenerationList,
    ) -> Result<QueryRootLease, SqlError> {
        self.reserve_in(self.slots_per_use, |snapshot| {
            snapshot.capture_image(storage, image, generations)
        })
    }

    fn retain_source(
        self: &Arc<Self>,
        source: &SpillSourceView<'_>,
    ) -> Result<QueryRootLease, SqlError> {
        self.reserve_in(self.slots_per_use, |snapshot| snapshot.copy_source(source))
    }

    fn reserve_in(
        self: &Arc<Self>,
        start: usize,
        capture: impl FnOnce(&mut SpillGenerationSnapshot) -> Result<(), SqlError>,
    ) -> Result<QueryRootLease, SqlError> {
        for slot in start..start + self.slots_per_use {
            let candidate = &self.slots[slot];
            if candidate
                .retained
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            candidate.readers.store(1, Ordering::Relaxed);
            let lease = QueryRootLease {
                pool: Arc::clone(self),
                slot,
            };
            {
                let mut snapshot = candidate
                    .snapshot
                    .write()
                    .expect("query SST roots poisoned");
                capture(&mut snapshot)?;
            }
            return Ok(lease);
        }
        Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "query SST root slots exhausted ({})",
            self.slots_per_use
        ))
    }
}

impl QueryRootLease {
    pub(super) fn try_clone(&self) -> Result<Self, SqlError> {
        let readers = &self.pool.slots[self.slot].readers;
        let mut count = readers.load(Ordering::Relaxed);
        loop {
            let next = count.checked_add(1).ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "query SST source reader count exhausted"
                )
            })?;
            match readers.compare_exchange_weak(count, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(current) => count = current,
            }
        }
        Ok(Self {
            pool: Arc::clone(&self.pool),
            slot: self.slot,
        })
    }

    /// Only this source and its clones own the slot. Its guard excludes no unrelated reader,
    /// publisher, or retirement; registry ownership ends before object I/O.
    pub(super) fn with_snapshot<T>(
        &self,
        read: impl FnOnce(&SpillGenerationSnapshot) -> Result<T, SqlError>,
    ) -> Result<T, SqlError> {
        let snapshot = self.pool.slots[self.slot]
            .snapshot
            .read()
            .expect("query SST roots poisoned");
        read(&snapshot)
    }
}

impl Drop for QueryRootLease {
    fn drop(&mut self) {
        let slot = &self.pool.slots[self.slot];
        if slot.readers.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        slot.snapshot
            .write()
            .expect("query SST roots poisoned")
            .clear();
        slot.retained.store(false, Ordering::Release);
    }
}

/// A point verdict includes deletions so an older reader can still select
/// historical bytes from the same captured generation set.
pub(super) struct SpilledRowState {
    roots: QueryRootLease,
    rowid: u64,
    version: super::SpillVersion,
}

impl SpilledRowState {
    pub(super) fn new(roots: QueryRootLease, rowid: u64, version: super::SpillVersion) -> Self {
        Self {
            roots,
            rowid,
            version,
        }
    }

    pub(super) fn state(&self) -> super::RowState {
        super::RowState {
            committed: self.version.len.map(|len| super::RowHome::Spilled {
                len,
                sst: self.version.generation,
                commit_lsn: self.version.commit_lsn,
            }),
            committed_lsn: self.version.commit_lsn,
            committed_generation: Some(0),
            checkpoint_change_lsn: 0,
            history: super::CommittedHistory::empty(),
            pending: super::PendingVersions::empty(),
        }
    }

    /// Converts a verdict admitted by the captured file walk into its exact
    /// byte owner. Validate the selected position before publishing the read.
    pub(super) fn into_captured_read(self) -> Result<SpilledRowRead, SqlError> {
        let len = self.version.len.ok_or_else(|| {
            sql_err!(sqlstate::INTERNAL_ERROR, "captured saved row is a deletion")
        })?;
        self.roots.with_snapshot(|source| {
            if self.version.commit_lsn > source.commit_lsn {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "saved row exceeds the captured read position"
                ));
            }
            if self.version.member as usize >= source.handles.len() {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "captured saved row has no file member"
                ));
            }
            Ok(())
        })?;
        SpilledRowRead::new(
            self.roots,
            self.rowid,
            len,
            self.version.generation,
            self.version.commit_lsn,
        )
    }

    pub(super) fn validate_request(&self, table: usize, rowid: u64) -> Result<(), SqlError> {
        self.roots.with_snapshot(|source| {
            if source.table_slot != table || self.rowid != rowid {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "retained object state does not match the requested table and row"
                ));
            }
            Ok(())
        })
    }

    pub(super) fn visible_at(
        &self,
        commit_snapshot: u64,
    ) -> Result<Option<SpilledRowRead>, SqlError> {
        let version = self.roots.with_snapshot(|source| {
            if self.version.commit_lsn <= commit_snapshot && commit_snapshot <= source.commit_lsn {
                Ok(Some(self.version))
            } else {
                Storage::probe_spill_snapshot(source, self.rowid, commit_snapshot)
            }
        })?;
        let Some(super::SpillVersion {
            len: Some(len),
            generation,
            commit_lsn,
            ..
        }) = version
        else {
            return Ok(None);
        };
        SpilledRowRead::new(
            self.roots.try_clone()?,
            self.rowid,
            len,
            generation,
            commit_lsn,
        )
        .map(Some)
    }
}

/// The selected version and its complete immutable source share one lease.
/// Older snapshot probes never consult a replacement table's generation list.
pub(super) struct SpilledRowRead {
    roots: QueryRootLease,
    pub(super) table_slot: usize,
    pub(super) created_at: u64,
    pub(super) rowid: u64,
    pub(super) len: u32,
    pub(super) member: SpillGenerationId,
    pub(super) commit_lsn: u64,
}

impl SpilledRowRead {
    pub(super) fn new(
        roots: QueryRootLease,
        rowid: u64,
        len: u32,
        member: SpillGenerationId,
        commit_lsn: u64,
    ) -> Result<Self, SqlError> {
        let (table_slot, created_at) = roots.with_snapshot(|source| {
            source.handle_for_generation(member)?;
            Ok((source.table_slot, source.created_at))
        })?;
        Ok(Self {
            roots,
            table_slot,
            created_at,
            rowid,
            len,
            member,
            commit_lsn,
        })
    }

    pub(super) fn snapshot(&self) -> super::RowSnapshot {
        super::RowSnapshot {
            created_at: self.created_at,
            version: super::row_images::RowVersionIdentity::Committed(self.commit_lsn),
        }
    }

    pub(super) fn validate_request(&self, table: usize, rowid: u64) -> Result<(), SqlError> {
        if table != self.table_slot || rowid != self.rowid {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "retained object row does not match the requested table and row"
            ));
        }
        Ok(())
    }

    pub(super) fn copy_bytes<'arena>(
        &self,
        arena: &'arena crate::mem::arena::Arena,
    ) -> Result<&'arena [u8], SqlError> {
        self.roots.with_snapshot(|source| {
            let reader = source.reader.as_ref().ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "retained object row has no reader"
                )
            })?;
            let handle = source.handle_for_generation(self.member)?;
            reader.copy_row_bytes(
                source.persistence,
                handle,
                self.rowid,
                self.len,
                self.commit_lsn,
                arena,
            )
        })
    }

    pub(super) fn with_bytes<R>(
        &self,
        consume: impl FnOnce(&[u8]) -> Result<R, SqlError>,
    ) -> Result<R, SqlError> {
        self.roots.with_snapshot(|source| {
            let reader = source.reader.as_ref().ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "retained object row has no reader"
                )
            })?;
            let handle = source.handle_for_generation(self.member)?;
            reader.with_row_bytes(
                source.persistence,
                handle,
                self.rowid,
                self.len,
                self.commit_lsn,
                consume,
            )
        })
    }
}

#[derive(Clone, Copy)]
pub(super) struct SpillGenerationImage {
    persistence: RelationPersistence,
    schema: [ColType; MAX_COLUMNS],
    n_columns: usize,
    table_slot: usize,
    pub(super) created_at: u64,
    data_generation: u64,
    pub(super) commit_lsn: u64,
}

impl SpillGenerationImage {
    const EMPTY: Self = Self {
        persistence: RelationPersistence::Permanent,
        schema: [ColType::Bool; MAX_COLUMNS],
        n_columns: 0,
        table_slot: 0,
        created_at: 0,
        data_generation: 0,
        commit_lsn: 0,
    };

    pub(super) fn capture(
        definition: &super::table_definitions::DefinitionState,
        generation: u64,
        table_slot: usize,
        commit_lsn: u64,
    ) -> Self {
        let mut image = Self::EMPTY;
        image.persistence = definition.committed.persistence;
        image.n_columns = definition.committed.schema(&mut image.schema);
        image.table_slot = table_slot;
        image.created_at = definition.identity.created_at;
        image.data_generation = generation;
        image.commit_lsn = commit_lsn;
        image
    }
}

#[derive(Clone, Copy)]
struct SpillGenerationMetadata {
    persistence: RelationPersistence,
    table_slot: usize,
    created_at: u64,
    data_generation: u64,
    commit_lsn: u64,
}

impl SpillGenerationImage {
    pub(super) fn table_slot(&self) -> usize {
        self.table_slot
    }

    fn metadata(&self) -> SpillGenerationMetadata {
        SpillGenerationMetadata {
            persistence: self.persistence,
            table_slot: self.table_slot,
            created_at: self.created_at,
            data_generation: self.data_generation,
            commit_lsn: self.commit_lsn,
        }
    }
}

pub(super) struct SpillSourceView<'a> {
    registry: &'a Arc<SpillRootRegistry>,
    pub(super) reader: Option<&'a Arc<SpillReader>>,
    handles: &'a [SstHandle],
    ranges: &'a [SpillGenerationRange],
    metadata: SpillGenerationMetadata,
    schema: &'a [ColType],
}

impl SpillSourceView<'_> {
    fn image(&self) -> SpillGenerationImage {
        let mut image = SpillGenerationImage::EMPTY;
        image.persistence = self.metadata.persistence;
        image.table_slot = self.metadata.table_slot;
        image.created_at = self.metadata.created_at;
        image.data_generation = self.metadata.data_generation;
        image.commit_lsn = self.metadata.commit_lsn;
        image.n_columns = self.schema.len();
        image.schema[..self.schema.len()].copy_from_slice(self.schema);
        image
    }

    pub(super) fn relation(&self) -> SpillRelation<'_> {
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        schema[..self.schema.len()].copy_from_slice(self.schema);
        SpillRelation {
            persistence: self.metadata.persistence,
            schema,
            n_columns: self.schema.len(),
            handles: self.handles,
            ranges: self.ranges,
        }
    }

    pub(super) fn commit_lsn(&self) -> u64 {
        self.metadata.commit_lsn
    }
}

pub(super) struct StatementSpillRoots<'a> {
    registry: Arc<SpillRootRegistry>,
    reader: Option<Arc<SpillReader>>,
    metadata: SpillGenerationMetadata,
    schema: &'a [ColType],
    handles: &'a [SstHandle],
    ranges: &'a [SpillGenerationRange],
}

impl<'a> StatementSpillRoots<'a> {
    fn capture(
        storage: &Storage,
        image: SpillGenerationImage,
        generations: &SpillGenerationList,
        arena: &'a crate::mem::arena::Arena,
    ) -> Result<Self, SqlError> {
        let metadata = image.metadata();
        let schema = arena
            .alloc_slice_with(image.n_columns, |index| image.schema[index])
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "retained scan schema exceeds the statement arena"
                )
            })?;
        let handles = arena
            .alloc_slice_with(generations.len(), |index| {
                generations.handle(index).expect("complete generation list")
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "retained scan SST roots exceed the statement arena"
                )
            })?;
        let ranges = arena
            .alloc_slice_with(generations.len(), |index| generations.members[index].range)
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "retained scan SST identities exceed the statement arena"
                )
            })?;
        storage
            .spill_roots
            .pin(handles, image.persistence != RelationPersistence::Temporary)?;
        Ok(Self {
            registry: Arc::clone(&storage.spill_roots),
            reader: storage.spill.as_ref().map(Arc::clone),
            metadata,
            schema,
            handles,
            ranges,
        })
    }

    fn view(&self) -> SpillSourceView<'_> {
        SpillSourceView {
            registry: &self.registry,
            reader: self.reader.as_ref(),
            handles: self.handles,
            ranges: self.ranges,
            metadata: self.metadata,
            schema: self.schema,
        }
    }
}

impl Drop for StatementSpillRoots<'_> {
    fn drop(&mut self) {
        self.registry.release(
            self.handles,
            self.metadata.persistence != RelationPersistence::Temporary,
        );
    }
}

pub(super) enum CapturedSpillRoots<'a> {
    Empty,
    Reserved(QueryRootLease),
    Statement(StatementSpillRoots<'a>),
}

impl<'a> CapturedSpillRoots<'a> {
    pub(super) fn capture(
        storage: &Storage,
        image: SpillGenerationImage,
        generations: &SpillGenerationList,
        arena: Option<&'a crate::mem::arena::Arena>,
    ) -> Result<Self, SqlError> {
        if generations.is_empty() {
            return Ok(Self::Empty);
        }
        match arena {
            Some(arena) => StatementSpillRoots::capture(storage, image, generations, arena)
                .map(Self::Statement),
            None => storage
                .query_roots
                .retain_image(storage, image, generations)
                .map(Self::Reserved),
        }
    }

    pub(super) fn with_view<T>(
        &self,
        read: impl FnOnce(&SpillSourceView<'_>) -> Result<T, SqlError>,
    ) -> Result<Option<T>, SqlError> {
        match self {
            Self::Empty => Ok(None),
            Self::Reserved(lease) => {
                lease.with_snapshot(|snapshot| read(&snapshot.view()).map(Some))
            }
            Self::Statement(source) => read(&source.view()).map(Some),
        }
    }

    pub(super) fn owned_source(
        &self,
        storage: &Storage,
    ) -> Result<Option<QueryRootLease>, SqlError> {
        self.owned_source_in(&storage.query_roots)
    }

    pub(super) fn owned_source_in(
        &self,
        pool: &Arc<QueryRootPool>,
    ) -> Result<Option<QueryRootLease>, SqlError> {
        match self {
            Self::Empty => Ok(None),
            Self::Reserved(lease) => lease.try_clone().map(Some),
            Self::Statement(source) => pool.retain_source(&source.view()).map(Some),
        }
    }

    pub(super) fn matches_read(&self, read: &SpilledRowRead) -> Result<bool, SqlError> {
        self.with_view(|source| {
            read.roots.with_snapshot(|other| {
                let member = source
                    .ranges
                    .partition_point(|range| range.last < read.member);
                if !source
                    .ranges
                    .get(member)
                    .is_some_and(|range| range.contains(read.member))
                {
                    return Ok(false);
                }
                let handle = source.handles[member];
                let readers_match = match (source.reader, other.reader.as_ref()) {
                    (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                    (None, None) => true,
                    _ => false,
                };
                Ok(readers_match
                    && source.metadata.persistence == other.persistence
                    && source.schema == &other.schema[..other.n_columns]
                    && Arc::ptr_eq(
                        source.registry,
                        other.registry.as_ref().expect("retained registry"),
                    )
                    && other
                        .handle_for_generation(read.member)
                        .is_ok_and(|other_handle| handle == other_handle))
            })
        })
        .map(|matches| matches.unwrap_or(false))
    }
}

/// The buffers are constructed at startup. Capturing only clones existing Arc
/// owners and copies handles; registry ownership never spans object I/O.
pub(super) struct SpillGenerationSnapshot {
    registry: Option<Arc<SpillRootRegistry>>,
    pub(super) reader: Option<Arc<SpillReader>>,
    handles: Vec<SstHandle>,
    ranges: Vec<SpillGenerationRange>,
    image: SpillGenerationImage,
}

impl Deref for SpillGenerationSnapshot {
    type Target = SpillGenerationImage;
    fn deref(&self) -> &Self::Target {
        &self.image
    }
}

impl DerefMut for SpillGenerationSnapshot {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.image
    }
}

impl SpillGenerationSnapshot {
    pub(super) fn budget_bytes(max_generations: usize) -> usize {
        max_generations * (size_of::<SstHandle>() + size_of::<SpillGenerationRange>())
    }

    pub(super) fn new(max_generations: usize) -> Self {
        Self {
            registry: None,
            reader: None,
            handles: Vec::with_capacity(max_generations),
            ranges: Vec::with_capacity(max_generations),
            image: SpillGenerationImage::EMPTY,
        }
    }

    pub(super) fn capture(&mut self, storage: &Storage, table_slot: usize) -> Result<(), SqlError> {
        self.clear();
        let table = storage.table(table_slot);
        let definition = table.definition.read();
        self.image = SpillGenerationImage::capture(
            &definition,
            table.generation(),
            table_slot,
            storage.lsn(),
        );
        // Retain roots before releasing list ownership. Replacement cannot
        // retire a member between copying its handle and registering the read.
        let generations = table.spill_generations();
        self.retain_members(&generations, &storage.spill_roots)?;
        self.reader = storage.spill.as_ref().map(Arc::clone);
        Ok(())
    }

    pub(super) fn capture_image(
        &mut self,
        storage: &Storage,
        image: SpillGenerationImage,
        generations: &SpillGenerationList,
    ) -> Result<(), SqlError> {
        self.clear();
        self.image = image;
        self.retain_members(generations, &storage.spill_roots)?;
        self.reader = storage.spill.as_ref().map(Arc::clone);
        Ok(())
    }

    pub(super) fn view(&self) -> SpillSourceView<'_> {
        SpillSourceView {
            registry: self.registry.as_ref().expect("captured source registry"),
            reader: self.reader.as_ref(),
            handles: &self.handles,
            ranges: &self.ranges,
            metadata: self.image.metadata(),
            schema: &self.schema[..self.n_columns],
        }
    }

    fn copy_source(&mut self, source: &SpillSourceView<'_>) -> Result<(), SqlError> {
        self.clear();
        if source.handles.len() > self.handles.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained SST members exceed startup capacity ({})",
                self.handles.capacity()
            ));
        }
        self.image = source.image();
        for &handle in source.handles {
            self.handles.push(handle);
        }
        self.ranges.extend_from_slice(source.ranges);
        if let Err(error) = source.registry.pin(
            &self.handles,
            self.persistence != RelationPersistence::Temporary,
        ) {
            self.handles.clear();
            self.ranges.clear();
            return Err(error);
        }
        self.registry = Some(Arc::clone(source.registry));
        self.reader = source.reader.map(Arc::clone);
        Ok(())
    }

    fn retain_members(
        &mut self,
        generations: &SpillGenerationList,
        registry: &Arc<SpillRootRegistry>,
    ) -> Result<(), SqlError> {
        if generations.len() > self.handles.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained SST members exceed startup capacity ({})",
                self.handles.capacity()
            ));
        }
        for member in generations.iter() {
            self.handles.push(member.handle);
            self.ranges.push(member.range);
        }
        if let Err(error) = registry.pin(
            &self.handles,
            self.persistence != RelationPersistence::Temporary,
        ) {
            self.handles.clear();
            self.ranges.clear();
            return Err(error);
        }
        self.registry = Some(Arc::clone(registry));
        Ok(())
    }

    pub(super) fn validate(&self, storage: &Storage, table_slot: usize) -> Result<(), SqlError> {
        if self
            .registry
            .as_ref()
            .is_none_or(|registry| !Arc::ptr_eq(registry, &storage.spill_roots))
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "retained SST reader belongs to different storage"
            ));
        }
        if table_slot != self.table_slot
            || table_slot >= storage.physical_table_count()
            || !storage.table(table_slot).live()
            || storage.table(table_slot).created_at() != self.created_at
            || storage.table(table_slot).generation() != self.data_generation
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "checkpoint source table identity changed while SST roots were retained"
            ));
        }
        let table = storage.table(table_slot);
        let definition = table.definition();
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        if definition.persistence != self.persistence
            || definition.schema(&mut schema) != self.n_columns
            || schema[..self.n_columns] != self.schema[..self.n_columns]
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "checkpoint source schema changed while SST roots were retained"
            ));
        }
        Ok(())
    }

    pub(super) fn relation(&self) -> SpillRelation<'_> {
        SpillRelation {
            persistence: self.persistence,
            schema: self.schema,
            n_columns: self.n_columns,
            handles: &self.handles,
            ranges: &self.ranges,
        }
    }

    pub(super) fn clear(&mut self) {
        if let Some(registry) = self.registry.take() {
            registry.release(
                &self.handles,
                self.persistence != RelationPersistence::Temporary,
            );
        }
        self.handles.clear();
        self.ranges.clear();
        self.reader = None;
    }

    fn handle_for_generation(&self, id: SpillGenerationId) -> Result<SstHandle, SqlError> {
        let member = self.ranges.partition_point(|range| range.last < id);
        self.ranges
            .get(member)
            .filter(|range| range.contains(id))
            .and_then(|_| self.handles.get(member))
            .copied()
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "retained object row has no SST generation"
                )
            })
    }
}

impl Drop for SpillGenerationSnapshot {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::{make_def, test_budget, test_config};
    use crate::store::{BlockId, OwnedObjectStore, RowSstFormat, TieredStore};

    fn handle(tag: u8) -> SstHandle {
        SstHandle {
            index: BlockId([tag; 32]),
            filter: BlockId([tag.wrapping_add(1); 32]),
            roster: BlockId([tag.wrapping_add(2); 32]),
            format: RowSstFormat::PackedV4,
        }
    }

    fn index_handle(tag: u8) -> ValueIndexHandle {
        ValueIndexHandle {
            roster: BlockId([tag; 32]),
            entries: 3,
            published_lsn: 7,
        }
    }

    #[test]
    fn index_file_owners_reserve_independent_capacity_and_preserve_handle_metadata() {
        let mut config = test_config();
        config.max_value_indexes = 1;
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        assert_eq!(budget.remaining(), 0);
        let mut roots = Vec::with_capacity(2);
        crate::mem::guard::forbid_alloc(|| {
            let first_handle = index_handle(1);
            let newer_handle = ValueIndexHandle {
                entries: 9,
                published_lsn: 11,
                ..first_handle
            };
            let first = registry.retain_index_file(first_handle).unwrap();
            let newer = registry.retain_index_file(newer_handle).unwrap();
            assert_eq!(first.handle(), first_handle);
            assert_eq!(newer.handle(), newer_handle);
            let second = registry.retain_index_file(index_handle(2)).unwrap();
            let error = registry.retain_index_file(index_handle(3)).err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(
                error
                    .message
                    .as_str()
                    .starts_with(IndexFileRoot::CAPACITY_ERROR)
            );
            // A full index pool must not consume the independently reserved row pool.
            registry.pin(&[handle(1)], true).unwrap();
            registry.release(&[handle(1)], true);
            registry.copy_index_roots(&mut roots).unwrap();
            assert_eq!(roots, [first_handle.roster, index_handle(2).roster]);
            drop(first);
            assert!(!registry.take_reclamation_pending());
            drop(newer);
            assert!(registry.take_reclamation_pending());
            let replacement = registry.retain_index_file(index_handle(3)).unwrap();
            drop(second);
            assert!(registry.take_reclamation_pending());
            drop(replacement);
            assert!(!registry.take_reclamation_pending());
            assert!(registry.roots.lock().unwrap().index_roots.is_empty());
        });
    }

    #[test]
    fn index_file_owners_miri_final_release_clears_both_file_pools() {
        let config = test_config();
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        let mut rows = Vec::with_capacity(2);
        let mut indexes = Vec::with_capacity(2);
        crate::mem::guard::forbid_alloc(|| {
            let first = registry.retain_statement().unwrap();
            let second = registry.retain_statement().unwrap();
            for (owner, tag) in [(&first, 1), (&second, 5)] {
                let scope = owner.enter().unwrap();
                registry.pin(&[handle(tag)], true).unwrap();
                registry.release(&[handle(tag)], true);
                drop(registry.retain_index_file(index_handle(tag)).unwrap());
                drop(scope);
            }
            registry.copy_roots(true, &mut rows).unwrap();
            registry.copy_index_roots(&mut indexes).unwrap();
            drop(first);
            assert!(registry.take_reclamation_pending());
            registry.copy_roots(true, &mut rows).unwrap();
            registry.copy_index_roots(&mut indexes).unwrap();
            assert_eq!(rows, [handle(5)]);
            assert_eq!(indexes, [index_handle(5).roster]);
            drop(second);
            assert!(registry.take_reclamation_pending());
            let state = registry.roots.lock().unwrap();
            assert!(state.roots.is_empty());
            assert!(state.index_roots.is_empty());
            assert!(state.index_roots.members.iter().all(|word| *word == 0));
        });
    }

    #[test]
    fn statement_files_miri_definition_children_keep_parent_owners_until_final_release() {
        let config = test_config();
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        let mut roots = Vec::with_capacity(1);
        crate::mem::guard::forbid_alloc(|| {
            let parent = registry.retain_statement().unwrap();
            let child = registry.retain_statement().unwrap();
            let scope = parent.enter().unwrap();
            registry.pin(&[handle(1)], true).unwrap();
            registry.release(&[handle(1)], true);
            {
                let mut state = registry.roots.lock().unwrap();
                let owner = state.statement_mut(parent.identity);
                owner.references = owner.references.checked_add(1).unwrap();
                state.statement_mut(child.identity).definition_parent = Some(parent.identity);
            }
            let child_scope = child.enter().unwrap();
            let selected = registry.type_definition_reader().unwrap();
            assert_eq!(selected.identity, child.identity);
            drop(child_scope);
            drop(scope);
            drop(child);
            drop(parent);
            assert!(!registry.take_reclamation_pending());
            assert_eq!(registry.roots.lock().unwrap().roots.len(), 1);
            registry.copy_roots(true, &mut roots).unwrap();
            assert_eq!(roots, [handle(1)]);
            drop(selected);
            assert!(registry.take_reclamation_pending());
            let state = registry.roots.lock().unwrap();
            assert!(state.roots.is_empty());
            assert!(state.statements.iter().all(Option::is_none));
        });
    }

    #[test]
    fn index_file_owners_miri_scope_outlives_storage_and_reused_owner_slots() {
        let config = test_config();
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        let weak = Arc::downgrade(&registry);
        let mut roots = Vec::with_capacity(1);
        crate::mem::guard::forbid_alloc(|| {
            let old = registry.retain_statement().unwrap();
            let old_identity = old.identity;
            drop(old);
            let owner = registry.retain_statement().unwrap();
            assert_eq!(owner.identity.slot, old_identity.slot);
            assert_ne!(owner.identity.serial, old_identity.serial);
            let scope = owner.enter().unwrap();
            drop(registry.retain_index_file(index_handle(1)).unwrap());
            drop(owner);
            drop(registry);
            let registry = weak.upgrade().unwrap();
            registry.copy_index_roots(&mut roots).unwrap();
            assert_eq!(roots, [index_handle(1).roster]);
            drop(scope);
            assert!(registry.take_reclamation_pending());
            assert!(registry.roots.lock().unwrap().index_roots.is_empty());
            drop(registry);
            assert!(weak.upgrade().is_none());
        });
    }

    #[test]
    fn index_file_owners_membership_crosses_word_boundaries_and_deduplicates() {
        let mut config = test_config();
        config.max_value_indexes = 1;
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        let mut owners = Vec::with_capacity(66);
        let mut roots = Vec::with_capacity(2);
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..66 {
                let owner = registry.retain_statement().unwrap();
                let scope = owner.enter().unwrap();
                for _ in 0..3 {
                    drop(registry.retain_index_file(index_handle(1)).unwrap());
                }
                drop(scope);
                owners.push(owner);
            }
            assert_eq!(
                registry.roots.lock().unwrap().index_roots[0].readers.get(),
                66
            );
            registry.copy_index_roots(&mut roots).unwrap();
            while owners.len() > 1 {
                drop(owners.pop());
                assert!(!registry.take_reclamation_pending());
            }
            drop(owners.pop());
            assert!(registry.take_reclamation_pending());
            assert!(registry.roots.lock().unwrap().index_roots.is_empty());
        });
    }

    #[test]
    fn index_file_owners_exhaustion_and_snapshot_errors_leave_memberships_intact() {
        let mut config = test_config();
        config.max_value_indexes = 1;
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        let mut short = Vec::with_capacity(0);
        let mut roots = Vec::with_capacity(2);
        crate::mem::guard::forbid_alloc(|| {
            let file = registry.retain_index_file(index_handle(1)).unwrap();
            let owner = registry.retain_statement().unwrap();
            let scope = owner.enter().unwrap();
            registry.roots.lock().unwrap().index_roots[0].readers =
                NonZeroUsize::new(usize::MAX).unwrap();
            let error = registry.retain_index_file(index_handle(1)).err().unwrap();
            assert_eq!(error.message.as_str(), IndexFileRoot::REFERENCE_ERROR);
            {
                let mut state = registry.roots.lock().unwrap();
                assert!(state.index_roots.members.iter().all(|word| *word == 0));
                assert!(!state.statement_mut(owner.identity).has_roots);
                state.index_roots[0].readers = NonZeroUsize::new(1).unwrap();
            }
            assert_eq!(
                registry.copy_index_roots(&mut short).unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            drop(file);
            assert!(!registry.take_reclamation_pending());
            drop(registry.retain_index_file(index_handle(2)).unwrap());
            drop(registry.retain_index_file(index_handle(3)).unwrap());
            assert!(registry.retain_index_file(index_handle(4)).is_err());
            registry.copy_index_roots(&mut roots).unwrap();
            assert_eq!(roots, [index_handle(2).roster, index_handle(3).roster]);
            drop(scope);
            drop(owner);
            assert!(registry.take_reclamation_pending());
            assert!(registry.roots.lock().unwrap().index_roots.is_empty());
        });
    }

    struct IndexFixture {
        storage: Storage,
        table: usize,
        handle: ValueIndexHandle,
        blocks: Arc<Mutex<TieredStore<OwnedObjectStore>>>,
        config: Config,
    }

    fn index_fixture(label: &str, navigation: bool) -> IndexFixture {
        let mut config = test_config();
        config.object_store_on = true;
        config.object_store_sim = true;
        config.object_store_bucket = format!("{label}-{}", std::process::id());
        config.block_cache_bytes = 0;
        config.disk_cache_bytes = 0;
        let mut budget = Budget::new(
            config.memtable_bytes
                + Storage::extra_budget_bytes(&config)
                + OwnedObjectStore::budget_bytes(&config)
                + SpillReader::budget_bytes(true, 3)
                + (1 << 20),
        );
        let base = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
        let mut store = crate::store::build_tiers(
            &mut budget,
            base,
            crate::store::StackPlan::resolve(0, 0),
            std::path::Path::new("."),
        )
        .unwrap();
        let mut writer = crate::store::ValueIndexWriter::new();
        if navigation {
            writer.reset_navigation(0, crate::store::NavigationKind::Spatial, true);
        }
        for rowid in 1..=3 {
            if navigation {
                writer
                    .append_navigation(
                        &mut store,
                        (11, rowid, 7),
                        b"key",
                        Some(b"old value"),
                        crate::store::SpatialBounds::new(0.0, 0.0, 1.0, 1.0),
                        &mut |left, right| left.cmp(right),
                    )
                    .unwrap();
            } else {
                writer
                    .append_covering(
                        &mut store,
                        (11, rowid, 7),
                        b"key",
                        b"old value",
                        &mut |left, right| left.cmp(right),
                    )
                    .unwrap();
            }
        }
        let handle = writer.finish(&mut store, 7).unwrap().unwrap();
        let blocks = Arc::new(Mutex::new(store));
        let reader = SpillReader::new(&mut budget, 3, Some(Arc::clone(&blocks)), None).unwrap();
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        storage.attach_spill(reader);
        let mut definition = make_def(label, &[("id", ColType::Int4, false)]);
        definition.columns[0].unique = true;
        let table = storage.create_table(definition).unwrap();
        {
            let mut publication = storage.tables[table].publication();
            let enforcer = publication.enforcers[0].as_mut().unwrap();
            enforcer.durable = Some(handle);
            enforcer.cache.write().mark_incomplete();
        }
        IndexFixture {
            storage,
            table,
            handle,
            blocks,
            config,
        }
    }

    impl Drop for IndexFixture {
        fn drop(&mut self) {
            crate::object_store::sim::drop_namespace(&self.config.object_store_bucket);
        }
    }

    #[test]
    fn index_file_owners_reader_callbacks_release_store_and_allow_nested_probes() {
        let fixture = index_fixture("index-owner-nested", false);
        let IndexFixture {
            storage,
            table,
            blocks,
            handle,
            ..
        } = &fixture;
        crate::mem::guard::forbid_alloc(|| {
            let check_nested = || -> Result<(), SqlError> {
                assert!(
                    blocks.try_lock().is_ok(),
                    "callbacks must not hold the block store"
                );
                assert!(storage.tables[*table].publication.try_lock().is_ok());
                let mut nested = 0;
                assert!(storage.probe_value_binding(*table, 0, 11, |_, key| {
                    assert_eq!(key, Some(b"key".as_slice()));
                    nested += 1;
                    Ok(())
                })?);
                assert_eq!(nested, 3);
                Ok(())
            };
            let mut visited = 0;
            assert!(
                storage
                    .probe_value_binding(*table, 0, 11, |_, _| {
                        check_nested()?;
                        visited += 1;
                        Ok(())
                    })
                    .unwrap()
            );
            assert_eq!(visited, 3);
            assert!(
                storage
                    .walk_value_index(*table, &[0], |_, _| check_nested())
                    .unwrap()
            );
            assert!(
                storage
                    .range_value_index_binding(
                        *table,
                        0,
                        |_| {
                            check_nested()?;
                            Ok(crate::store::ValueIndexPosition::Match)
                        },
                        |_, _, _, payload| {
                            assert_eq!(payload, b"old value");
                            check_nested()
                        }
                    )
                    .unwrap()
            );
            let input = storage
                .select_value_index(*table, super::super::ValueIndexSelection::Binding(0))
                .unwrap()
                .unwrap();
            assert_eq!(input.handle(), *handle);
            drop(input);
            assert!(
                storage
                    .spill_roots
                    .roots
                    .lock()
                    .unwrap()
                    .index_roots
                    .is_empty()
            );
        });
    }

    #[test]
    fn index_file_owners_ranked_leaves_survive_binding_replacement_and_wait() {
        let fixture = index_fixture("index-owner-ranked", true);
        let IndexFixture {
            storage,
            table,
            blocks,
            handle,
            ..
        } = &fixture;
        let mut roots = Vec::with_capacity(2);
        crate::mem::guard::forbid_alloc(|| {
            let owner = storage.retain_statement_snapshot(71, |_| 7).unwrap();
            let scope = owner.enter_files(storage).unwrap();
            let mut candidate = None;
            assert!(
                storage
                    .rank_value_index_binding(
                        *table,
                        0,
                        |_, _| {
                            assert!(blocks.try_lock().is_ok());
                            true
                        },
                        |_, _| {
                            assert!(blocks.try_lock().is_ok());
                            0.0
                        },
                        |source, rowid, lsn, _, _| {
                            assert!(blocks.try_lock().is_ok());
                            candidate = Some((source, rowid, lsn));
                            Ok(())
                        }
                    )
                    .unwrap()
            );
            drop(scope);
            storage.tables[*table].publication().enforcers[0]
                .as_mut()
                .unwrap()
                .durable = None;
            storage.spill_roots.copy_index_roots(&mut roots).unwrap();
            assert_eq!(roots, [handle.roster]);
            let scope = owner.enter_files(storage).unwrap();
            let (source, rowid, lsn) = candidate.unwrap();
            let mut visited = false;
            assert!(
                storage
                    .read_ranked_value_binding_entry(
                        *table,
                        0,
                        source,
                        rowid,
                        lsn,
                        |key, payload| {
                            assert!(blocks.try_lock().is_ok());
                            assert_eq!(key, b"key");
                            assert_eq!(payload, b"old value");
                            visited = true;
                        }
                    )
                    .unwrap()
            );
            assert!(visited);
            drop(scope);
            drop(owner);
            assert!(storage.spill_roots.take_reclamation_pending());
            assert!(
                storage
                    .spill_roots
                    .roots
                    .lock()
                    .unwrap()
                    .index_roots
                    .is_empty()
            );
        });
    }

    #[test]
    fn index_file_owners_reject_future_generation_and_release_callback_errors() {
        let fixture = index_fixture("index-owner-errors", false);
        let IndexFixture {
            storage,
            table,
            handle,
            ..
        } = &fixture;
        let mut roots = Vec::with_capacity(2);
        crate::mem::guard::forbid_alloc(|| {
            let owner = storage.retain_statement_snapshot(71, |_| 6).unwrap();
            storage.set_commit_snapshot(6);
            let scope = owner.enter_files(storage).unwrap();
            assert!(
                storage
                    .select_value_index(*table, super::super::ValueIndexSelection::Binding(0))
                    .unwrap()
                    .is_none()
            );
            drop(scope);
            drop(owner);
            let owner = storage.retain_statement_snapshot(71, |_| 7).unwrap();
            storage.set_commit_snapshot(7);
            let scope = owner.enter_files(storage).unwrap();
            let error = storage
                .walk_value_index(*table, &[0], |_, _| {
                    Err(sql_err!(sqlstate::INTERNAL_IO_WAIT, "test suspended index"))
                })
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::INTERNAL_IO_WAIT);
            drop(scope);
            storage.spill_roots.copy_index_roots(&mut roots).unwrap();
            assert_eq!(roots, [handle.roster]);
            drop(owner);
            assert!(storage.spill_roots.take_reclamation_pending());
            assert!(
                storage
                    .spill_roots
                    .roots
                    .lock()
                    .unwrap()
                    .index_roots
                    .is_empty()
            );
        });
    }

    #[test]
    fn index_file_owners_snapshot_eligibility_uses_binding_changes_and_saved_generation() {
        let fixture = index_fixture("index-owner-snapshot", false);
        let IndexFixture {
            storage,
            table,
            handle,
            ..
        } = &fixture;
        crate::mem::guard::forbid_alloc(|| {
            storage.set_commit_snapshot(7);
            assert!(storage.value_indexes_match_snapshot(*table));
            storage.tables[*table].publication().enforcers[0]
                .as_mut()
                .unwrap()
                .durable_dirty_lsn = 9;
            assert!(!storage.value_indexes_match_snapshot(*table));
            // Publication clears dirtiness; its former rebuild reservation is
            // no longer a newer committed change to the saved generation.
            storage.tables[*table].publication().enforcers[0]
                .as_mut()
                .unwrap()
                .durable_dirty = false;
            assert!(storage.value_indexes_match_snapshot(*table));
            storage.tables[*table].publication().enforcers[0]
                .as_mut()
                .unwrap()
                .durable_dirty = true;
            storage.set_commit_snapshot(9);
            assert!(storage.value_indexes_match_snapshot(*table));
            storage.tables[*table].publication().enforcers[0]
                .as_mut()
                .unwrap()
                .durable_dirty = false;
            storage.tables[*table].publication().enforcers[0]
                .as_mut()
                .unwrap()
                .durable = Some(ValueIndexHandle {
                published_lsn: 11,
                ..*handle
            });
            assert!(!storage.value_indexes_match_snapshot(*table));
            storage.set_commit_snapshot(11);
            assert!(storage.value_indexes_match_snapshot(*table));
        });
    }

    #[test]
    fn statement_files_scopes_retain_deduplicate_and_release_selected_roots() {
        let config = test_config();
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        assert_eq!(budget.remaining(), 0);
        let mut retained = Vec::with_capacity(2);
        crate::mem::guard::forbid_alloc(|| {
            let owner = registry.retain_statement().unwrap();
            let scope = owner.enter().unwrap();
            let selected = [handle(1), handle(1), handle(5)];
            registry.pin(&selected, true).unwrap();
            registry.release(&selected, true);
            for _ in 0..128 {
                registry.pin(&selected, true).unwrap();
                registry.release(&selected, true);
            }
            assert!(
                registry
                    .roots
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|root| root.readers.get() == 1)
            );
            drop(scope);
            registry.copy_roots(true, &mut retained).unwrap();
            assert_eq!(retained, [handle(1), handle(5)]);
            assert!(!registry.take_reclamation_pending());
            drop(owner);
            registry.copy_roots(true, &mut retained).unwrap();
            assert!(retained.is_empty());
            assert!(registry.take_reclamation_pending());
            assert!(!registry.take_reclamation_pending());
        });
    }

    #[test]
    fn statement_files_nested_scopes_keep_child_files_with_the_outer_statement() {
        let config = test_config();
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        let mut retained = Vec::with_capacity(2);
        crate::mem::guard::forbid_alloc(|| {
            let outer = registry.retain_statement().unwrap();
            let outer_scope = outer.enter().unwrap();
            registry.pin(&[handle(1)], false).unwrap();
            registry.release(&[handle(1)], false);
            let inner = registry.retain_statement().unwrap();
            let inner_scope = inner.enter().unwrap();
            registry.pin(&[handle(5)], true).unwrap();
            registry.release(&[handle(5)], true);
            drop(inner_scope);
            drop(inner);
            registry.copy_roots(true, &mut retained).unwrap();
            assert_eq!(retained, [handle(5)]);
            registry.copy_roots(false, &mut retained).unwrap();
            assert_eq!(retained, [handle(1)]);
            drop(outer_scope);
            drop(outer);
            assert!(registry.roots.lock().unwrap().is_empty());
            assert!(registry.take_reclamation_pending());
        });
    }

    #[test]
    fn statement_files_miri_dense_root_moves_preserve_other_statement_memberships() {
        let config = test_config();
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        let mut retained = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            let first = registry.retain_statement().unwrap();
            let second = registry.retain_statement().unwrap();
            let temporary = registry.retain_statement().unwrap();
            for (owner, root, durable) in [
                (&first, handle(1), true),
                (&second, handle(5), true),
                (&temporary, handle(1), false),
            ] {
                let scope = owner.enter().unwrap();
                registry.pin(&[root], durable).unwrap();
                registry.release(&[root], durable);
                drop(scope);
            }
            registry.copy_roots(true, &mut retained).unwrap();
            registry.copy_roots(false, &mut retained).unwrap();
            drop(first);
            assert!(registry.take_reclamation_pending());
            registry.copy_roots(true, &mut retained).unwrap();
            assert_eq!(retained, [handle(5)]);
            registry.copy_roots(false, &mut retained).unwrap();
            assert_eq!(retained, [handle(1)]);
            drop(second);
            assert!(registry.take_reclamation_pending());
            registry.copy_roots(false, &mut retained).unwrap();
            assert_eq!(retained, [handle(1)]);
            drop(temporary);
            assert!(!registry.take_reclamation_pending());
            assert!(registry.roots.lock().unwrap().is_empty());
            let reused = registry.retain_statement().unwrap();
            let scope = reused.enter().unwrap();
            registry.pin(&[handle(9)], true).unwrap();
            registry.release(&[handle(9)], true);
            drop(scope);
            drop(reused);
            assert!(registry.roots.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn statement_files_miri_execution_scope_outlives_its_snapshot_and_registry_owner() {
        let config = test_config();
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        let weak = Arc::downgrade(&registry);
        let mut retained = Vec::with_capacity(1);
        crate::mem::guard::forbid_alloc(|| {
            let owner = registry.retain_statement().unwrap();
            let scope = owner.enter().unwrap();
            registry.pin(&[handle(1)], true).unwrap();
            registry.release(&[handle(1)], true);
            drop(owner);
            drop(registry);
            let remaining = weak.upgrade().unwrap();
            remaining.copy_roots(true, &mut retained).unwrap();
            assert_eq!(retained, [handle(1)]);
            drop(scope);
            assert!(remaining.roots.lock().unwrap().is_empty());
            drop(remaining);
            assert!(weak.upgrade().is_none());
        });
    }

    #[test]
    fn statement_files_owner_capacity_and_identity_reuse_are_exact() {
        let mut config = test_config();
        config.max_connections = 1;
        let capacity = super::super::statement_snapshot_capacity(&config);
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        assert_eq!(budget.remaining(), 0);
        assert!(capacity > config.max_connections as usize);
        let mut leases = Vec::with_capacity(capacity);
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..capacity {
                leases.push(registry.retain_statement().unwrap());
            }
            let before = registry.roots.lock().unwrap().next_statement;
            let error = registry.retain_statement().err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(error.message.as_str().contains("statement file owners"));
            assert_eq!(registry.roots.lock().unwrap().next_statement, before);
            let last = leases.pop().unwrap();
            let prior = last.identity;
            drop(last);
            let reused = registry.retain_statement().unwrap();
            assert_eq!(reused.identity.slot, prior.slot);
            assert_ne!(reused.identity.serial, prior.serial);
            drop(reused);
            leases.clear();
            registry.roots.lock().unwrap().next_statement = u64::MAX;
            let error = registry.retain_statement().err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(error.message.as_str(), "statement file identity exhausted");
            assert!(
                registry
                    .roots
                    .lock()
                    .unwrap()
                    .statements
                    .iter()
                    .all(Option::is_none)
            );
        });
    }

    #[test]
    fn statement_files_reference_and_root_failures_preserve_existing_owners() {
        let mut config = test_config();
        config.max_tables = 1;
        config.max_spill_generations_per_table = 1;
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let owner = registry.retain_statement().unwrap();
            STATEMENT_FILE_SCOPE_DEPTH.with(|depth| depth.set(usize::MAX));
            assert_eq!(
                owner.enter().err().unwrap().message.as_str(),
                "statement file scope nesting exhausted"
            );
            assert!(SELECTED_STATEMENT_FILES.with(Cell::get).is_none());
            STATEMENT_FILE_SCOPE_DEPTH.with(|depth| depth.set(0));
            registry
                .roots
                .lock()
                .unwrap()
                .statement_mut(owner.identity)
                .references = NonZeroUsize::new(usize::MAX).unwrap();
            assert_eq!(
                owner.enter().err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert!(SELECTED_STATEMENT_FILES.with(Cell::get).is_none());
            registry
                .roots
                .lock()
                .unwrap()
                .statement_mut(owner.identity)
                .references = NonZeroUsize::new(1).unwrap();
            registry.pin(&[handle(1), handle(5)], true).unwrap();
            let scope = owner.enter().unwrap();
            registry.roots.lock().unwrap()[0].readers = NonZeroUsize::new(usize::MAX).unwrap();
            assert_eq!(
                registry
                    .pin(&[handle(1), handle(5)], true)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            {
                let mut state = registry.roots.lock().unwrap();
                assert!(state.members.iter().all(|word| *word == 0));
                assert_eq!(state[1].readers.get(), 1);
                state[0].readers = NonZeroUsize::new(1).unwrap();
            }
            let selected = [handle(1), handle(1), handle(9), handle(13)];
            registry.pin(&selected, true).unwrap();
            registry.release(&selected, true);
            assert_eq!(
                registry.pin(&[handle(17)], true).unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            registry.release(&[handle(1), handle(5)], true);
            drop(scope);
            drop(owner);
            assert!(registry.roots.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn statement_files_nested_storage_scopes_restore_their_own_registry() {
        let config = test_config();
        let mut budget = Budget::new(2 * SpillRootRegistry::budget_bytes(&config));
        let first = SpillRootRegistry::new(&config, &mut budget).unwrap();
        let second = SpillRootRegistry::new(&config, &mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let outer = first.retain_statement().unwrap();
            let inner = second.retain_statement().unwrap();
            let outer_scope = outer.enter().unwrap();
            let inner_scope = inner.enter().unwrap();
            first.pin(&[handle(1)], true).unwrap();
            first.release(&[handle(1)], true);
            assert!(first.roots.lock().unwrap().is_empty());
            second.pin(&[handle(1)], true).unwrap();
            second.release(&[handle(1)], true);
            drop(inner_scope);
            first.pin(&[handle(5)], true).unwrap();
            first.release(&[handle(5)], true);
            drop(outer_scope);
            drop(outer);
            assert!(first.roots.lock().unwrap().is_empty());
            assert_eq!(second.roots.lock().unwrap().len(), 1);
            drop(inner);
            assert!(second.roots.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn statement_files_execution_scopes_are_local_to_their_worker_thread() {
        let config = test_config();
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        let first = registry.retain_statement().unwrap();
        let second = registry.retain_statement().unwrap();
        let mut retained = Vec::with_capacity(2);
        let first_scope = crate::mem::guard::forbid_alloc(|| {
            let scope = first.enter().unwrap();
            registry.pin(&[handle(1)], true).unwrap();
            registry.release(&[handle(1)], true);
            scope
        });
        let second = std::thread::scope(|threads| {
            let registry = &registry;
            threads
                .spawn(move || {
                    crate::mem::guard::forbid_alloc(|| {
                        assert!(SELECTED_STATEMENT_FILES.with(Cell::get).is_none());
                        let scope = second.enter().unwrap();
                        registry.pin(&[handle(5)], true).unwrap();
                        registry.release(&[handle(5)], true);
                        drop(scope);
                        assert!(SELECTED_STATEMENT_FILES.with(Cell::get).is_none());
                        second
                    })
                })
                .join()
                .unwrap()
        });
        crate::mem::guard::forbid_alloc(|| {
            registry.copy_roots(true, &mut retained).unwrap();
            assert_eq!(retained, [handle(1), handle(5)]);
            drop(second);
            registry.copy_roots(true, &mut retained).unwrap();
            assert_eq!(retained, [handle(1)]);
            drop(first_scope);
            drop(first);
            assert!(SELECTED_STATEMENT_FILES.with(Cell::get).is_none());
            assert!(registry.roots.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn sliced_heap_cleanup_zero_commit_and_temporary_snapshot_coverage_are_distinct() {
        let mut budget = Budget::new(SpillGenerationRoster::member_budget_bytes(3));
        let roster = SpillGenerationRoster::new(&mut budget, 3).unwrap();
        assert_eq!(budget.remaining(), 0);
        crate::mem::guard::forbid_alloc(|| {
            let mut list = roster.write();
            list.replace(1, core::iter::once(handle(1))).unwrap();
            assert_eq!(list.eviction_member(0, 7), None);
            list.append_covered(handle(5), SpillCoverage::ResidentThroughGeneration(7))
                .unwrap();
            assert_eq!(
                list.eviction_member(0, 6),
                Some(SpillGenerationId::fixture(1))
            );
            assert_eq!(
                list.eviction_member(0, 7),
                Some(SpillGenerationId::fixture(1))
            );
            assert_eq!(list.eviction_member(0, 8), None);
            let error = list.append(handle(9), 0).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
            assert_eq!(list.len(), 2);
            list.append_covered(handle(9), SpillCoverage::ResidentThroughGeneration(8))
                .unwrap();
            assert_eq!(
                list.eviction_member(0, 8),
                Some(SpillGenerationId::fixture(3))
            );
            assert_eq!(list.published_through(), None);
            list.merge_pair(1, Some(handle(13)));
            assert_eq!(
                list.eviction_member(0, 8),
                Some(SpillGenerationId::fixture(3))
            );
            list.clear();
            list.append(handle(17), 0).unwrap();
            assert_eq!(
                list.eviction_member(0, 8),
                Some(SpillGenerationId::fixture(4))
            );
            assert_eq!(list.eviction_member(1, 8), None);
            assert_eq!(list.published_through(), Some(0));
        });
    }

    #[test]
    fn stable_spill_generation_ranges_survive_merges_removals_and_clear() {
        let mut budget = Budget::new(SpillGenerationRoster::member_budget_bytes(4));
        let roster = SpillGenerationRoster::new(&mut budget, 4).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut list = roster.write();
            for tag in [1, 5, 9, 13] {
                list.append(handle(tag), u64::from(tag)).unwrap();
            }
            list.merge_pair(1, Some(handle(17)));
            assert_eq!(
                list.members[0].range,
                SpillGenerationRange::single(SpillGenerationId::fixture(0))
            );
            assert!(
                list.members[1]
                    .range
                    .contains(SpillGenerationId::fixture(1))
            );
            assert!(
                list.members[1]
                    .range
                    .contains(SpillGenerationId::fixture(2))
            );
            assert_eq!(
                list.members[2].range,
                SpillGenerationRange::single(SpillGenerationId::fixture(3))
            );
            list.merge_pair(0, None);
            assert_eq!(
                list.members[0].range,
                SpillGenerationRange::single(SpillGenerationId::fixture(3))
            );
            list.append(handle(21), 21).unwrap();
            assert_eq!(
                list.members[1].range,
                SpillGenerationRange::single(SpillGenerationId::fixture(4))
            );
            list.clear();
            assert_eq!(
                list.append_reserved(
                    handle(25),
                    SpillCoverage::CommittedThrough(25),
                    SpillGenerationId::fixture(4)
                )
                .unwrap_err()
                .sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            list.append(handle(25), 25).unwrap();
            assert_eq!(
                list.members[0].range,
                SpillGenerationRange::single(SpillGenerationId::fixture(5))
            );
        });
    }

    #[test]
    fn stable_spill_generation_exhaustion_preserves_the_published_list() {
        let mut budget = Budget::new(SpillGenerationRoster::member_budget_bytes(2));
        let roster = SpillGenerationRoster::new(&mut budget, 2).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut list = roster.write();
            list.append(handle(1), 7).unwrap();
            list.next_id = u64::from(u32::MAX);
            assert_eq!(
                list.replace(2, [handle(5), handle(9)].into_iter())
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(list.handle(0), Some(handle(1)));
            let last = list.reserve_id().unwrap();
            assert_eq!(last, SpillGenerationId::fixture(u32::MAX));
            assert_eq!(
                list.reserve_id().unwrap_err().message.as_str(),
                "table SST generation identity space exhausted"
            );
            // A receipt reserved before durability still installs after exhaustion.
            list.collapse_reserved(handle(5), SpillCoverage::CommittedThrough(9), last)
                .unwrap();
            assert_eq!(list.handle(0), Some(handle(5)));
            assert!(
                list.members[0]
                    .range
                    .contains(SpillGenerationId::fixture(0))
            );
            list.clear();
            assert_eq!(
                list.reserve_id().unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert!(list.is_empty());
        });
    }

    #[test]
    fn guarded_spill_generations_preserve_previous_list_on_failed_install() {
        let bytes = SpillGenerationRoster::member_budget_bytes(3);
        let mut budget = Budget::new(bytes);
        let roster = SpillGenerationRoster::new(&mut budget, 3).unwrap();
        assert_eq!(budget.remaining(), 0);
        crate::mem::guard::forbid_alloc(|| {
            let mut list = roster.write();
            list.append(handle(1), 7).unwrap();
            list.append(handle(5), 9).unwrap();
            for (declared, members, state) in [
                (4, &[handle(9)][..], sqlstate::PROGRAM_LIMIT_EXCEEDED),
                (2, &[handle(9)][..], sqlstate::INTERNAL_ERROR),
                (1, &[handle(9), handle(13)][..], sqlstate::INTERNAL_ERROR),
                (0, &[handle(9)][..], sqlstate::INTERNAL_ERROR),
            ] {
                let error = list.replace(declared, members.iter().copied()).unwrap_err();
                assert_eq!(error.sqlstate, state);
                assert_eq!(list.len(), 2);
                assert_eq!(list.handle(0), Some(handle(1)));
                assert_eq!(list.handle(1), Some(handle(5)));
                assert_eq!(list.members[0].coverage, SpillCoverage::CommittedThrough(7));
                assert_eq!(list.members[1].coverage, SpillCoverage::CommittedThrough(9));
                assert!(list.staging.is_empty());
            }
            list.append(handle(9), 11).unwrap();
            let error = list.append(handle(17), 19).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(list.len(), 3);
            assert_eq!(
                list.members[2],
                SpillGeneration {
                    handle: handle(9),
                    coverage: SpillCoverage::CommittedThrough(11),
                    range: SpillGenerationRange::single(SpillGenerationId::fixture(2)),
                }
            );
            list.merge_pair(0, Some(handle(13)));
            assert_eq!(list.len(), 2);
            assert_eq!(
                list.members[0],
                SpillGeneration {
                    handle: handle(13),
                    coverage: SpillCoverage::CommittedThrough(9),
                    range: SpillGenerationRange {
                        first: SpillGenerationId::fixture(0),
                        last: SpillGenerationId::fixture(1)
                    },
                }
            );
            assert_eq!(
                list.members[1],
                SpillGeneration {
                    handle: handle(9),
                    coverage: SpillCoverage::CommittedThrough(11),
                    range: SpillGenerationRange::single(SpillGenerationId::fixture(2)),
                }
            );
            list.merge_pair(0, None);
            assert!(list.is_empty());
            list.replace(3, [handle(1), handle(5), handle(9)].into_iter())
                .unwrap();
            list.merge_pair(1, None);
            assert_eq!(list.len(), 1);
            assert_eq!(list.handle(0), Some(handle(1)));
            list.replace(0, [].into_iter()).unwrap();
            assert!(list.is_empty());
            list.collapse(handle(17), 19).unwrap();
            assert_eq!(
                list.members[0].coverage,
                SpillCoverage::CommittedThrough(19)
            );
        });
    }

    #[test]
    fn guarded_spill_generations_retain_roots_before_releasing_list() {
        use std::time::{Duration, Instant};
        let config = test_config();
        let mut budget = Budget::new(
            SpillRootRegistry::budget_bytes(&config)
                + SpillGenerationRoster::member_budget_bytes(2),
        );
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        let roster = SpillGenerationRoster::new(&mut budget, 2).unwrap();
        roster
            .write()
            .replace(2, [handle(1), handle(5)].into_iter())
            .unwrap();
        let mut snapshot = SpillGenerationSnapshot::new(2);
        let ready = AtomicBool::new(false);
        let mut copied = Vec::with_capacity(2);
        let pinned = registry.roots.lock().unwrap();
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| {
                    let list = roster.read();
                    ready.store(true, Ordering::Release);
                    snapshot.retain_members(&list, &registry).unwrap();
                });
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ready.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::yield_now();
            }
            let started = ready.load(Ordering::Acquire);
            let excluded = matches!(
                roster.state.try_write(),
                Err(std::sync::TryLockError::WouldBlock)
            );
            // Release before assertions so a failing test cannot strand the reader.
            drop(pinned);
            reader.join().unwrap();
            assert!(started, "reader did not reach root registration");
            assert!(excluded, "replacement raced root registration");
        });
        crate::mem::guard::forbid_alloc(|| {
            roster.write().replace(1, [handle(9)].into_iter()).unwrap();
            assert_eq!(snapshot.handles, [handle(1), handle(5)]);
            registry.copy_roots(true, &mut copied).unwrap();
            assert_eq!(copied, [handle(1), handle(5)]);
            snapshot.clear();
            assert!(registry.take_reclamation_pending());
            registry.copy_roots(true, &mut copied).unwrap();
            assert!(copied.is_empty());
            let list = roster.read();
            snapshot.retain_members(&list, &registry).unwrap();
            assert_eq!(snapshot.handles, [handle(9)]);
            drop(list);
            snapshot.clear();
        });
    }

    #[test]
    fn guarded_spill_generations_capture_complete_lists_during_replacement() {
        let mut config = test_config();
        config.max_spill_generations_per_table = 3;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("coherent", &[("id", ColType::Int4, true)]))
            .unwrap();
        storage.set_spill_list(table, &[handle(1), handle(5)]);
        let first = [handle(1), handle(5)];
        let second = [handle(9), handle(13), handle(17)];
        let mut snapshot = SpillGenerationSnapshot::new(3);
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                barrier.wait();
                crate::mem::guard::forbid_alloc(|| {
                    for index in 0..512 {
                        let members: &[SstHandle] = if index % 2 == 0 { &second } else { &first };
                        storage
                            .table(table)
                            .spill_generations
                            .write()
                            .replace(members.len(), members.iter().copied())
                            .unwrap();
                    }
                });
            });
            barrier.wait();
            crate::mem::guard::forbid_alloc(|| {
                for _ in 0..512 {
                    snapshot.capture(&storage, table).unwrap();
                    assert!(snapshot.handles == first || snapshot.handles == second);
                    assert_eq!(snapshot.created_at, storage.table(table).created_at());
                    assert_eq!(snapshot.schema[0], ColType::Int4);
                    snapshot.clear();
                }
            });
        });
        assert!(storage.spill_roots.roots.lock().unwrap().is_empty());
    }

    struct PointFixture {
        storage: Storage,
        table: usize,
        roots: [SstHandle; 3],
        arena: crate::mem::arena::Arena,
        path: std::path::PathBuf,
    }

    fn point_fixture(label: &str) -> PointFixture {
        use crate::sql::types::Datum;
        use crate::storage::rowenc;
        use crate::store::{EphemeralBlockStore, SstKey, SstWriter};
        let mut config = test_config();
        config.max_spill_generations_per_table = 3;
        let bytes = 32 * crate::store::BLOCK_SIZE;
        let mut budget = Budget::new(
            Storage::extra_budget_bytes(&config)
                + config.memtable_bytes
                + SpillReader::budget_bytes(false, 3)
                + EphemeralBlockStore::budget_bytes(bytes)
                + (1 << 20),
        );
        let path = std::env::temp_dir().join(format!(
            "pos3ql-retained-point-{label}-{}.blocks",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut blocks = EphemeralBlockStore::open(&mut budget, &path, bytes).unwrap();
        let mut writer = Box::new(SstWriter::new());
        let mut encoded = [0u8; 128];
        for (rowid, value) in [(1, 41), (2, 17)] {
            let values = [Datum::Int4(value)];
            let len = rowenc::encoded_len(&values);
            rowenc::encode(&values, &mut encoded[..len]);
            writer
                .append_version(&mut blocks, SstKey::at(rowid, 7), &encoded[..len])
                .unwrap();
        }
        let old = writer.finish(&mut blocks).unwrap().unwrap();
        writer.reset();
        let values = [Datum::Int4(88)];
        let len = rowenc::encoded_len(&values);
        rowenc::encode(&values, &mut encoded[..len]);
        writer
            .append_version(&mut blocks, SstKey::at(1, 9), &encoded[..len])
            .unwrap();
        writer
            .append_tombstone_version(&mut blocks, SstKey::at(2, 9))
            .unwrap();
        let current = writer.finish(&mut blocks).unwrap().unwrap();
        writer.reset();
        let values = [Datum::Text("replacement")];
        let len = rowenc::encoded_len(&values);
        rowenc::encode(&values, &mut encoded[..len]);
        writer
            .append_version(&mut blocks, SstKey::at(1, 11), &encoded[..len])
            .unwrap();
        let replacement = writer.finish(&mut blocks).unwrap().unwrap();
        let blocks = Arc::new(Mutex::new(blocks));
        let reader = SpillReader::new(&mut budget, 3, None, Some(blocks)).unwrap();
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        storage.attach_spill(reader);
        let mut definition = make_def(label, &[("id", ColType::Int4, true)]);
        definition.persistence = RelationPersistence::Temporary;
        let table = storage.create_table(definition).unwrap();
        storage.set_spill_list(table, &[old, current]);
        let arena = crate::mem::arena::Arena::new(&mut budget, "point result", 1 << 18).unwrap();
        PointFixture {
            storage,
            table,
            roots: [old, current, replacement],
            arena,
            path,
        }
    }

    #[test]
    fn statement_files_keep_real_scan_roots_after_walks_and_arena_are_released() {
        let PointFixture {
            mut storage,
            table,
            roots,
            mut arena,
            path,
        } = point_fixture("statement-files-waits");
        let registry = Arc::clone(&storage.spill_roots);
        let mut retained = Vec::with_capacity(3);
        let lease = crate::mem::guard::forbid_alloc(|| {
            let lease = storage.retain_statement_snapshot(71, |_| 7).unwrap();
            let scope = lease.enter_files(&storage).unwrap();
            // Repeated source aliases use the statement arena, not the point-source pool.
            for _ in 0..super::super::MAX_ROW_WALK_NESTING * 2 {
                drop(storage.row_scan_in(table, &arena).unwrap());
            }
            drop(storage.row_scan(table).unwrap());
            drop(scope);
            lease
        });
        arena.reset();
        crate::mem::guard::forbid_alloc(|| {
            storage.set_spill_list(table, &[roots[2]]);
            registry.copy_roots(false, &mut retained).unwrap();
            assert_eq!(retained, roots[..2]);
            let scope = lease.enter_files(&storage).unwrap();
            drop(storage.row_scan_in(table, &arena).unwrap());
            drop(scope);
            registry.copy_roots(false, &mut retained).unwrap();
            assert_eq!(retained, roots);
            drop(lease);
            registry.copy_roots(false, &mut retained).unwrap();
            assert!(retained.is_empty());
            assert!(!registry.take_reclamation_pending());
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn statement_files_release_after_source_error_replacement_and_storage_retirement() {
        let PointFixture {
            mut storage,
            table,
            roots,
            arena,
            path,
        } = point_fixture("statement-files-retire");
        let registry = Arc::clone(&storage.spill_roots);
        let mut retained = Vec::with_capacity(3);
        let lease = crate::mem::guard::forbid_alloc(|| {
            let lease = storage.retain_statement_snapshot(71, |_| 7).unwrap();
            let scope = lease.enter_files(&storage).unwrap();
            let scan = storage.row_scan_in(table, &arena).unwrap();
            let result = scan.roots.with_view::<()>(|_| {
                Err(sql_err!(
                    sqlstate::INTERNAL_IO_WAIT,
                    "suspend selected files"
                ))
            });
            assert_eq!(result.unwrap_err().sqlstate, sqlstate::INTERNAL_IO_WAIT);
            drop(scan);
            drop(scope);
            storage.set_spill_list(table, &[roots[2]]);
            storage.drop_table(table);
            lease
        });
        drop(storage);
        crate::mem::guard::forbid_alloc(|| {
            registry.copy_roots(false, &mut retained).unwrap();
            assert_eq!(retained, roots[..2]);
            drop(lease);
            registry.copy_roots(false, &mut retained).unwrap();
            assert!(retained.is_empty());
        });
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn statement_files_exhaustion_precedes_snapshot_selection_and_registration() {
        let config = test_config();
        let storage = Storage::new(&config, &mut test_budget(&config)).unwrap();
        let capacity = storage.spill_roots.roots.lock().unwrap().statements.len();
        let mut owners = Vec::with_capacity(capacity);
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..capacity {
                owners.push(storage.spill_roots.retain_statement().unwrap());
            }
            let before = storage.snapshot_state().next_statement;
            let error = storage
                .retain_statement_snapshot(71, |_| {
                    panic!("full file ownership must fail before selecting a snapshot")
                })
                .err()
                .unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(error.message.as_str().contains("statement file owners"));
            assert_eq!(storage.snapshot_state().next_statement, before);
            assert!(!storage.has_active_snapshots());
            owners.clear();
            storage.spill_roots.roots.lock().unwrap().next_statement = u64::MAX;
            let error = storage
                .retain_statement_snapshot(71, |_| {
                    panic!("file identity exhaustion must fail before snapshot selection")
                })
                .err()
                .unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(error.message.as_str(), "statement file identity exhausted");
            assert_eq!(storage.snapshot_state().next_statement, before);
            assert!(!storage.has_active_snapshots());
        });
    }

    #[test]
    fn statement_files_reject_a_foreign_snapshot_before_changing_execution_context() {
        let config = test_config();
        let first = Storage::new(&config, &mut test_budget(&config)).unwrap();
        let second = Storage::new(&config, &mut test_budget(&config)).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let first_lease = first.retain_statement_snapshot(71, |_| 7).unwrap();
            let second_lease = second.retain_statement_snapshot(71, |_| 9).unwrap();
            let scope = second_lease.enter_files(&second).unwrap();
            let selected = SELECTED_STATEMENT_FILES.with(Cell::get).unwrap();
            let error = first_lease.enter_files(&second).err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
            assert_eq!(
                error.message.as_str(),
                "statement snapshot belongs to another storage instance"
            );
            let after = SELECTED_STATEMENT_FILES.with(Cell::get).unwrap();
            assert_eq!(after.registry, selected.registry);
            assert_eq!(after.identity.serial, selected.identity.serial);
            assert_eq!(STATEMENT_FILE_SCOPE_DEPTH.with(Cell::get), 1);
            assert_eq!(first.oldest_snapshot(), Some(7));
            assert_eq!(second.oldest_snapshot(), Some(9));
            drop(scope);
            drop((first_lease, second_lease));
            assert!(SELECTED_STATEMENT_FILES.with(Cell::get).is_none());
            assert!(!first.has_active_snapshots() && !second.has_active_snapshots());
        });
    }

    #[test]
    fn captured_saved_verdict_conversion_rejects_invalid_positions_and_releases_roots() {
        let PointFixture {
            storage,
            table,
            roots: _,
            arena,
            path,
        } = point_fixture("captured-verdict");
        let mut retained = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            let length =
                crate::storage::rowenc::encoded_len(&[crate::sql::types::Datum::Int4(41)]) as u32;
            for version in [
                super::super::SpillVersion {
                    len: None,
                    member: 0,
                    generation: crate::storage::SpillGenerationId::fixture(0),
                    commit_lsn: 7,
                },
                super::super::SpillVersion {
                    len: Some(length),
                    member: 0,
                    generation: crate::storage::SpillGenerationId::fixture(0),
                    commit_lsn: 9,
                },
                super::super::SpillVersion {
                    len: Some(length),
                    member: 2,
                    generation: crate::storage::SpillGenerationId::fixture(2),
                    commit_lsn: 7,
                },
            ] {
                let roots = storage.query_roots.retain(&storage, table, 7).unwrap();
                let state = SpilledRowState::new(roots, 1, version);
                assert_eq!(
                    state.into_captured_read().err().unwrap().sqlstate,
                    sqlstate::INTERNAL_ERROR
                );
                storage
                    .spill_roots
                    .copy_roots(false, &mut retained)
                    .unwrap();
                assert!(retained.is_empty());
                assert!(
                    storage
                        .query_roots
                        .slots
                        .iter()
                        .all(|slot| !slot.retained.load(Ordering::Acquire))
                );
            }
            let roots = storage.query_roots.retain(&storage, table, 7).unwrap();
            let read = SpilledRowState::new(
                roots,
                1,
                super::super::SpillVersion {
                    len: Some(length),
                    member: 0,
                    generation: crate::storage::SpillGenerationId::fixture(0),
                    commit_lsn: 7,
                },
            )
            .into_captured_read()
            .unwrap();
            assert_point_value(&read, &arena, 41);
            drop(read);
            storage
                .spill_roots
                .copy_roots(false, &mut retained)
                .unwrap();
            assert!(retained.is_empty());
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn constraint_validation_and_analyze_sources_keep_current_rows_after_retirement() {
        use super::super::RowState;
        use super::super::row_images::VisibleRowBytes;
        use crate::sql::types::Datum;
        use core::ops::ControlFlow;
        let PointFixture {
            mut storage,
            table,
            roots,
            arena,
            path,
        } = point_fixture("index-validation-sources");
        let registry = Arc::clone(&storage.spill_roots);
        let mut retained = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            let command = storage.read_snapshot();
            let commit = storage.commit_snapshot();
            for (rowid, value) in [(3, 30), (4, 40), (5, 50)] {
                let location = storage.heap.append_row(&[Datum::Int4(value)]).unwrap();
                storage.tables[table]
                    .rows
                    .insert(rowid, RowState::committed_only_at(location, 11))
                    .unwrap();
            }
            let pending = storage.heap.append_row(&[Datum::Int4(55)]).unwrap();
            let own = storage
                .write_pending(table, 3, 11, 9, Some(pending))
                .unwrap();
            let foreign = storage
                .write_pending(table, 4, 12, 9, Some(pending))
                .unwrap();
            let deleted = storage.write_pending(table, 5, 11, 9, None).unwrap();
            storage.set_read_snapshot(1);
            storage.set_commit_snapshot(7);
            let scan = storage.index_validation_scan_in(table, 11, &arena).unwrap();
            storage.set_lsn(11);
            let analysis = {
                let _snapshot = storage.analyze_snapshot();
                storage.captured_row_scan_in(table, 11, &arena).unwrap()
            };
            let validation = storage
                .validation_read()
                .scan_in(&storage, table, 11, &arena)
                .unwrap();
            let captured_generation = storage.tables[table].generation();
            storage.tables[table].publication().generation += 1;
            assert_eq!(
                analysis.publication_generation(&storage, table).unwrap(),
                captured_generation
            );
            assert_eq!(storage.read_snapshot(), 1);
            assert_eq!(storage.commit_snapshot(), 7);
            assert!(
                scan.for_each_home(&mut |_, _| {
                    assert!(storage.row_versions.test_write().is_ok());
                    assert!(storage.tables[table].rows.test_write().is_ok());
                    Ok(ControlFlow::Break(()))
                })
                .unwrap()
                .is_break()
            );
            assert_eq!(
                scan.for_each_home(&mut |_, _| Err(sql_err!(
                    sqlstate::QUERY_CANCELED,
                    "cancel index validation"
                )))
                .unwrap_err()
                .sqlstate,
                sqlstate::QUERY_CANCELED
            );
            storage.restore_pending(table, 3, 11, own);
            storage.restore_pending(table, 4, 12, foreign);
            storage.restore_pending(table, 5, 11, deleted);
            storage.commit_drop(table);
            assert_eq!(
                storage
                    .create_table(make_def(
                        "index_replacement",
                        &[("id", ColType::Text, true)]
                    ))
                    .unwrap(),
                table
            );
            storage.set_spill_list(table, &[roots[2]]);
            assert_eq!(
                analysis
                    .publication_generation(&storage, table)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            storage.set_read_snapshot(command);
            storage.set_commit_snapshot(commit);
            drop(storage);
            registry.copy_roots(false, &mut retained).unwrap();
            assert_eq!(retained.len(), 2);
            assert!(retained.contains(&roots[0]) && retained.contains(&roots[1]));
            for source in [&scan, &analysis, &validation] {
                let mut found = [(0, 0); 3];
                let mut count = 0;
                assert!(
                    source
                        .for_each_home(&mut |rowid, home| {
                            let snapshot = home.snapshot();
                            let reread = source.read_snapshot(rowid, snapshot)?;
                            if rowid == 1 {
                                let mut older = snapshot;
                                older.version =
                                    super::super::row_images::RowVersionIdentity::Committed(7);
                                assert_eq!(
                                    source.read_snapshot(rowid, older).err().unwrap().sqlstate,
                                    sqlstate::SERIALIZATION_FAILURE
                                );
                            }
                            drop(home);
                            let mut decode = |bytes: &[u8]| {
                                let mut values = [Datum::Null];
                                crate::storage::rowenc::decode(
                                    bytes,
                                    &[ColType::Int4],
                                    &mut values,
                                )?;
                                let Datum::Int4(value) = values[0] else {
                                    panic!("integer fixture")
                                };
                                found[count] = (rowid, value);
                                count += 1;
                                Ok(())
                            };
                            match reread.bytes {
                                VisibleRowBytes::Heap(bytes) => decode(&bytes)?,
                                VisibleRowBytes::Spilled(source) => source.with_bytes(decode)?,
                            }
                            Ok(ControlFlow::Continue(()))
                        })
                        .unwrap()
                        .is_continue()
                );
                assert_eq!(count, 3);
                found.sort_unstable();
                assert_eq!(found, [(1, 88), (3, 55), (4, 40)]);
            }
            drop(scan);
            drop(analysis);
            drop(validation);
            registry.copy_roots(false, &mut retained).unwrap();
            assert!(retained.is_empty());
        });
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn captured_resident_byte_scans_keep_mixed_saved_versions_after_retirement() {
        use super::super::row_images::VisibleRowBytes;
        use super::super::{RowHome, RowLoc, RowState};
        use crate::sql::types::Datum;
        let PointFixture {
            mut storage,
            table,
            roots,
            arena,
            path,
        } = point_fixture("captured-mixed");
        let registry = Arc::clone(&storage.spill_roots);
        let mut retained = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            let old_command = storage.read_snapshot();
            let old_commit = storage.commit_snapshot();
            let future = storage.heap.append_row(&[Datum::Int4(99)]).unwrap();
            let resident = storage.heap.append_row(&[Datum::Int4(30)]).unwrap();
            let pending = storage.heap.append_row(&[Datum::Int4(55)]).unwrap();
            storage.tables[table]
                .rows
                .insert(1, RowState::committed_only_at(future, 11))
                .unwrap();
            storage.tables[table]
                .rows
                .insert(3, RowState::committed_only_at(resident, 7))
                .unwrap();
            let undo = storage
                .write_pending(table, 1, 11, 1, Some(pending))
                .unwrap();
            storage.set_read_snapshot(1);
            storage.set_commit_snapshot(7);
            let old = storage.captured_row_scan_in(table, 11, &arena).unwrap();
            storage.set_commit_snapshot(9);
            let current = storage.captured_row_scan_in(table, 11, &arena).unwrap();
            let length = crate::storage::rowenc::encoded_len(&[Datum::Int4(17)]) as u32;
            let mut saved = RowState::committed_only_at(RowLoc::test(0, length), 7);
            saved.committed = Some(RowHome::Spilled {
                len: length,
                sst: crate::storage::SpillGenerationId::fixture(0),
                commit_lsn: 7,
            });
            storage.tables[table].rows.insert(2, saved).unwrap();
            let deleted = storage.write_pending(table, 2, 11, 1, None).unwrap();
            let removed = storage.write_pending(table, 3, 11, 1, None).unwrap();
            storage.set_read_snapshot(2);
            storage.set_commit_snapshot(7);
            let own = storage.captured_row_scan_in(table, 11, &arena).unwrap();
            storage.restore_pending(table, 1, 11, undo);
            storage.restore_pending(table, 2, 11, deleted);
            storage.restore_pending(table, 3, 11, removed);
            storage.commit_drop(table);
            let replacement = storage
                .create_table(make_def(
                    "captured_replacement",
                    &[("id", ColType::Text, true)],
                ))
                .unwrap();
            assert_eq!(replacement, table);
            storage.set_spill_list(table, &[roots[2]]);
            storage.set_read_snapshot(old_command);
            storage.set_commit_snapshot(old_commit);
            drop(storage);
            registry.copy_roots(false, &mut retained).unwrap();
            assert_eq!(retained.len(), 2);
            assert!(retained.contains(&roots[0]) && retained.contains(&roots[1]));
            for (scan, expected_overlay, expected_saved, expected_all) in [
                (
                    &old,
                    &[(1, 41), (3, 30)][..],
                    &[(2, 17)][..],
                    &[(1, 41), (2, 17), (3, 30)][..],
                ),
                (
                    &current,
                    &[(1, 88), (3, 30)][..],
                    &[][..],
                    &[(1, 88), (3, 30)][..],
                ),
                (&own, &[(1, 55)][..], &[][..], &[(1, 55)][..]),
            ] {
                let records = arena
                    .alloc_slice_with(expected_all.len(), |_| None)
                    .unwrap();
                let mut filled = 0;
                let done = scan
                    .for_each_home(&mut |rowid, home| {
                        records[filled] = Some(scan.retain(rowid, &home)?);
                        filled += 1;
                        Ok(core::ops::ControlFlow::Continue(()))
                    })
                    .unwrap();
                assert!(done.is_continue());
                assert_eq!(filled, records.len());
                records.sort_unstable_by_key(|row| row.unwrap().rowid());
                for (record, expected) in records.iter().zip(expected_all) {
                    let record = record.unwrap();
                    let consume = |bytes: &[u8]| {
                        let mut decoded = [Datum::Null];
                        crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut decoded)?;
                        assert_eq!(
                            (record.rowid(), decoded[0]),
                            (expected.0, Datum::Int4(expected.1))
                        );
                        Ok(())
                    };
                    match record.read().unwrap().bytes {
                        VisibleRowBytes::Heap(bytes) => consume(&bytes).unwrap(),
                        VisibleRowBytes::Spilled(source) => source.with_bytes(consume).unwrap(),
                    }
                }
                let mut count = 0;
                let done = scan
                    .for_each_overlay(&mut |rowid, home| {
                        let consume = |bytes: &[u8]| {
                            let mut decoded = [Datum::Null];
                            crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut decoded)?;
                            assert_eq!(
                                (rowid, decoded[0]),
                                (
                                    expected_overlay[count].0,
                                    Datum::Int4(expected_overlay[count].1)
                                )
                            );
                            Ok(())
                        };
                        match home.bytes {
                            VisibleRowBytes::Heap(bytes) => {
                                consume(&bytes)?;
                            }
                            VisibleRowBytes::Spilled(source) => source.with_bytes(consume)?,
                        }
                        count += 1;
                        Ok(core::ops::ControlFlow::Continue(()))
                    })
                    .unwrap();
                assert!(done.is_continue());
                assert_eq!(count, expected_overlay.len());
                for mask in [None, Some(1)] {
                    let mut count = 0;
                    scan.for_each_spilled_row_batch(&arena, true, mask, &mut |rows| {
                        for row in rows {
                            let mut decoded = [Datum::Null];
                            let values = match row.representation {
                                super::super::SpilledRowRepresentation::Encoded(bytes) => {
                                    crate::storage::rowenc::decode(
                                        bytes,
                                        &[ColType::Int4],
                                        &mut decoded,
                                    )?;
                                    &decoded[..]
                                }
                                super::super::SpilledRowRepresentation::Values(values) => values,
                            };
                            assert_eq!(
                                (row.rowid, values[0]),
                                (
                                    expected_saved[count].0,
                                    Datum::Int4(expected_saved[count].1)
                                )
                            );
                            count += 1;
                        }
                        Ok(core::ops::ControlFlow::Continue(()))
                    })
                    .unwrap();
                    assert_eq!(count, expected_saved.len());
                }
            }
            drop(old);
            drop(current);
            drop(own);
            registry.copy_roots(false, &mut retained).unwrap();
            assert!(retained.is_empty());
        });
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn captured_resident_byte_scans_release_saved_sources_after_capacity_and_callback_errors() {
        use super::super::RowState;
        use crate::sql::types::Datum;
        let PointFixture {
            mut storage,
            table,
            roots: _,
            arena,
            path,
        } = point_fixture("captured-release");
        let alignment = core::mem::align_of::<SstHandle>();
        let schema_bytes = (core::mem::size_of::<ColType>() + alignment - 1) & !(alignment - 1);
        let root_capacity = schema_bytes + 2 * core::mem::size_of::<SstHandle>();
        let range_alignment = core::mem::align_of::<SpillGenerationRange>();
        let range_start = (root_capacity + range_alignment - 1) & !(range_alignment - 1);
        let capacity = range_start + 2 * core::mem::size_of::<SpillGenerationRange>();
        let mut budget = Budget::new(root_capacity + capacity);
        let missing_identities =
            crate::mem::arena::Arena::new(&mut budget, "captured roots only", root_capacity)
                .unwrap();
        let small =
            crate::mem::arena::Arena::new(&mut budget, "captured metadata only", capacity).unwrap();
        let mut retained = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            let old_commit = storage.commit_snapshot();
            storage.set_commit_snapshot(7);
            let location = storage.heap.append_row(&[Datum::Int4(30)]).unwrap();
            storage.tables[table]
                .rows
                .insert(3, RowState::committed_only_at(location, 7))
                .unwrap();
            let error = storage
                .captured_row_scan_in(table, 0, &missing_identities)
                .err()
                .unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(
                error.message.as_str(),
                "retained scan SST identities exceed the statement arena"
            );
            let error = storage
                .captured_row_scan_in(table, 0, &small)
                .err()
                .unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(error.message.as_str().contains("captured resident rows"));
            storage
                .spill_roots
                .copy_roots(false, &mut retained)
                .unwrap();
            assert!(
                retained.is_empty(),
                "failed capture must release files already registered"
            );
            assert!(storage.heap.test_exclusive_write().is_ok());
            let scan = storage.captured_row_scan_in(table, 0, &arena).unwrap();
            let nested = storage.captured_row_scan_in(table, 0, &arena).unwrap();
            let mut count = 0;
            assert!(
                scan.for_each_home(&mut |_, _| {
                    count += 1;
                    let mut other_count = 0;
                    let done = nested.for_each_home(&mut |_, _| {
                        other_count += 1;
                        Ok(core::ops::ControlFlow::Continue(()))
                    })?;
                    assert!(done.is_continue());
                    assert_eq!(other_count, 3);
                    Ok(if count == 2 {
                        core::ops::ControlFlow::Break(())
                    } else {
                        core::ops::ControlFlow::Continue(())
                    })
                })
                .unwrap()
                .is_break()
            );
            assert_eq!(count, 2);
            let mut count = 0;
            assert_eq!(
                scan.for_each_home(&mut |_, _| {
                    count += 1;
                    if count == 2 {
                        return Err(sql_err!(
                            sqlstate::QUERY_CANCELED,
                            "cancel saved metadata walk"
                        ));
                    }
                    Ok(core::ops::ControlFlow::Continue(()))
                })
                .unwrap_err()
                .sqlstate,
                sqlstate::QUERY_CANCELED
            );
            scan.for_each_spilled_row_batch(&arena, true, None, &mut |rows| {
                assert_eq!(rows.len(), 2);
                assert!(storage.tables[table].rows.test_write().is_ok());
                assert!(storage.row_versions.test_write().is_ok());
                nested.for_each_spilled_row_batch(&arena, true, Some(1), &mut |other| {
                    assert_eq!(other.len(), 2);
                    Ok(core::ops::ControlFlow::Break(()))
                })?;
                Ok(core::ops::ControlFlow::Break(()))
            })
            .unwrap();
            assert_eq!(
                scan.for_each_spilled_row_batch(&arena, true, None, &mut |_| {
                    Err(sql_err!(
                        sqlstate::QUERY_CANCELED,
                        "cancel captured saved scan"
                    ))
                })
                .unwrap_err()
                .sqlstate,
                sqlstate::QUERY_CANCELED
            );
            let reader = storage.spill.as_ref().unwrap();
            assert!(
                reader
                    .cursor_contexts
                    .iter()
                    .all(|slot| slot.try_lock().is_ok())
            );
            assert!(
                reader
                    .scan_contexts
                    .iter()
                    .all(|slot| slot.try_lock().is_ok())
            );
            drop(scan);
            drop(nested);
            storage
                .spill_roots
                .copy_roots(false, &mut retained)
                .unwrap();
            assert!(retained.is_empty());
            assert!(
                storage
                    .query_roots
                    .slots
                    .iter()
                    .all(|slot| !slot.retained.load(Ordering::Acquire))
            );
            let retry = storage.captured_row_scan_in(table, 0, &arena).unwrap();
            drop(retry);
            storage.set_commit_snapshot(old_commit);
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn owned_immutable_scan_keeps_saved_rows_after_retirement() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<super::super::row_walks::ImmutableRowScan<'static>>();
        for statement in [false, true] {
            let PointFixture {
                mut storage,
                table,
                roots,
                arena,
                path,
            } = point_fixture(if statement {
                "owned-scan-arena"
            } else {
                "owned-scan-pool"
            });
            let registry = Arc::clone(&storage.spill_roots);
            let mut retained = Vec::with_capacity(3);
            crate::mem::guard::forbid_alloc(|| {
                storage.set_commit_snapshot(7);
                let old = if statement {
                    storage.immutable_row_scan_in(table, &arena)
                } else {
                    storage.immutable_row_scan(table)
                }
                .unwrap()
                .unwrap();
                storage.set_commit_snapshot(9);
                let current = if statement {
                    storage.immutable_row_scan_in(table, &arena)
                } else {
                    storage.immutable_row_scan(table)
                }
                .unwrap()
                .unwrap();
                storage.commit_drop(table);
                let mut definition = make_def("reused_scan", &[("id", ColType::Text, true)]);
                definition.persistence = RelationPersistence::Temporary;
                let replacement = storage.create_table(definition).unwrap();
                assert_eq!(replacement, table);
                storage.set_spill_list(replacement, &[roots[2]]);
                drop(storage);
                registry.copy_roots(false, &mut retained).unwrap();
                assert_eq!(retained.len(), 2);
                assert!(retained.contains(&roots[0]) && retained.contains(&roots[1]));
                for mask in [None, Some(1)] {
                    for (scan, expected) in
                        [(&old, &[(1, 41), (2, 17)][..]), (&current, &[(1, 88)][..])]
                    {
                        let mut count = 0;
                        scan.for_each_spilled_row_batch(&arena, true, mask, &mut |rows| {
                            for row in rows {
                                let mut decoded = [crate::sql::types::Datum::Null];
                                let values = match row.representation {
                                    super::super::SpilledRowRepresentation::Encoded(bytes) => {
                                        crate::storage::rowenc::decode(
                                            bytes,
                                            &[ColType::Int4],
                                            &mut decoded,
                                        )?;
                                        &decoded[..]
                                    }
                                    super::super::SpilledRowRepresentation::Values(values) => {
                                        values
                                    }
                                };
                                assert_eq!(
                                    (row.rowid, values[0]),
                                    (
                                        expected[count].0,
                                        crate::sql::types::Datum::Int4(expected[count].1)
                                    )
                                );
                                count += 1;
                            }
                            Ok(core::ops::ControlFlow::Continue(()))
                        })
                        .unwrap();
                        assert_eq!(count, expected.len());
                    }
                }
                drop(old);
                drop(current);
                registry.copy_roots(false, &mut retained).unwrap();
                assert!(retained.is_empty());
            });
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn owned_immutable_scan_callbacks_release_contexts_and_keep_admission() {
        let PointFixture {
            mut storage,
            table,
            roots,
            arena,
            path,
        } = point_fixture("owned-scan-callbacks");
        let registry = Arc::clone(&storage.spill_roots);
        let mut retained = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            storage.set_commit_snapshot(7);
            let scan = storage.immutable_row_scan(table).unwrap().unwrap();
            let nested = storage
                .immutable_row_scan_in(table, &arena)
                .unwrap()
                .unwrap();
            storage.set_spill_list(table, &[roots[2]]);
            let location = storage
                .heap
                .append_row(&[crate::sql::types::Datum::Int4(99)])
                .unwrap();
            let undo = storage
                .write_pending(table, 3, 7, 1, Some(location))
                .unwrap();
            let used = arena.used();
            assert!(
                storage
                    .immutable_row_scan_in(table, &arena)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                arena.used(),
                used,
                "rejected admission must not reserve scan memory"
            );
            let mut count = 0;
            scan.for_each_spilled_row_batch(&arena, true, None, &mut |rows| {
                assert!(storage.tables[table].rows.test_write().is_ok());
                assert!(storage.row_versions.test_write().is_ok());
                count += rows.len();
                storage.restore_pending(table, 3, 7, undo);
                nested.for_each_spilled_row_batch(&arena, true, Some(1), &mut |other| {
                    assert_eq!(other.len(), 2);
                    Ok(core::ops::ControlFlow::Break(()))
                })?;
                Ok(core::ops::ControlFlow::Break(()))
            })
            .unwrap();
            assert_eq!(
                count, 2,
                "later rows and replacement files cannot change admission"
            );
            let error = scan
                .for_each_spilled_row_batch(&arena, true, None, &mut |_| {
                    Err(sql_err!(
                        sqlstate::QUERY_CANCELED,
                        "cancel owned immutable scan"
                    ))
                })
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::QUERY_CANCELED);
            let reader = storage.spill.as_ref().unwrap();
            assert!(
                reader
                    .cursor_contexts
                    .iter()
                    .all(|slot| slot.try_lock().is_ok())
            );
            assert!(
                reader
                    .scan_contexts
                    .iter()
                    .all(|slot| slot.try_lock().is_ok())
            );
            drop(scan);
            registry.copy_roots(false, &mut retained).unwrap();
            assert_eq!(retained.len(), 2, "nested source still owns the old files");
            drop(nested);
            registry.copy_roots(false, &mut retained).unwrap();
            assert!(retained.is_empty());
            assert!(
                storage
                    .query_roots
                    .slots
                    .iter()
                    .all(|slot| !slot.retained.load(Ordering::Acquire))
            );
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn owned_immutable_scan_capacity_failures_do_not_pin_files() {
        let PointFixture {
            storage,
            table,
            roots: _,
            arena: _,
            path,
        } = point_fixture("owned-scan-capacity");
        let mut budget = Budget::new(4);
        let arena = crate::mem::arena::Arena::new(&mut budget, "small scan", 4).unwrap();
        let mut scans = Vec::with_capacity(super::super::MAX_ROW_WALK_NESTING);
        let mut retained = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            let error = storage.immutable_row_scan_in(table, &arena).err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            storage
                .spill_roots
                .copy_roots(false, &mut retained)
                .unwrap();
            assert!(retained.is_empty());
            for _ in 0..super::super::MAX_ROW_WALK_NESTING {
                scans.push(storage.immutable_row_scan(table).unwrap().unwrap());
            }
            assert_eq!(
                storage.immutable_row_scan(table).err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            drop(scans.pop());
            scans.push(storage.immutable_row_scan(table).unwrap().unwrap());
            scans.clear();
            storage
                .spill_roots
                .copy_roots(false, &mut retained)
                .unwrap();
            assert!(retained.is_empty());
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn owned_ordered_rows_keep_saved_versions_without_live_metadata() {
        for statement in [false, true] {
            let PointFixture {
                mut storage,
                table,
                roots,
                arena,
                path,
            } = point_fixture(if statement {
                "ordered-saved-arena"
            } else {
                "ordered-saved-pool"
            });
            let registry = Arc::clone(&storage.spill_roots);
            let mut retained = Vec::with_capacity(3);
            crate::mem::guard::forbid_alloc(|| {
                let source = storage
                    .table_row_snapshot(table, statement.then_some(&arena))
                    .unwrap();
                let state = storage
                    .spill_row_state_at(table, 1, u64::MAX)
                    .unwrap()
                    .unwrap();
                let old = state.visible_at(7).unwrap().unwrap();
                let current = state.visible_at(9).unwrap().unwrap();
                let old_image = super::super::VisibleRowHome::spilled(old);
                let current_image = super::super::VisibleRowHome::spilled(current);
                let old = source.retain(1, &old_image).unwrap();
                let current = source.retain(1, &current_image).unwrap();
                drop(old_image);
                drop(current_image);
                drop(state);
                storage.set_spill_list(table, &[roots[2]]);
                let changed = super::super::VisibleRowHome::spilled(
                    storage
                        .retain_spilled_row(table, 1, 18, SpillGenerationId::fixture(2), 11)
                        .unwrap(),
                );
                assert_eq!(
                    source.retain(1, &changed).err().unwrap().sqlstate,
                    sqlstate::SERIALIZATION_FAILURE
                );
                drop(changed);
                storage.commit_drop(table);
                let mut definition =
                    make_def("ordered_replacement", &[("id", ColType::Text, true)]);
                definition.persistence = RelationPersistence::Temporary;
                assert_eq!(storage.create_table(definition).unwrap(), table);
                storage.set_spill_list(table, &[roots[2]]);
                drop(storage);
                for (record, expected) in [(old, 41), (current, 88)] {
                    let image = record.read().unwrap();
                    let super::super::VisibleRowBytes::Spilled(read) = image.bytes else {
                        panic!("object fixture")
                    };
                    assert_point_value(&read, &arena, expected);
                }
                // The copyable records borrow their source; it cannot be
                // released or replaced until their final use above.
                drop(source);
                registry.copy_roots(false, &mut retained).unwrap();
                assert!(retained.is_empty());
            });
            std::fs::remove_file(path).unwrap();
        }
    }

    fn scan_fixture(label: &str) -> PointFixture {
        use crate::sql::types::Datum;
        use crate::store::SstWriter;
        let mut fixture = point_fixture(label);
        let mut encoded = [0u8; 128];
        let mut writer = Box::new(SstWriter::new());
        let spill = fixture.storage.spill.as_ref().unwrap();
        let mut blocks = spill.relation_blocks_for(RelationPersistence::Temporary);
        for (rowid, commit_lsn, value) in [
            (1, 11, Some(111)),
            (1, 9, Some(88)),
            (1, 7, Some(41)),
            (2, 9, None),
            (2, 7, Some(17)),
        ] {
            if let Some(value) = value {
                let values = [Datum::Int4(value)];
                let len = crate::storage::rowenc::encoded_len(&values);
                crate::storage::rowenc::encode(&values, &mut encoded[..len]);
                writer
                    .append_version(
                        &mut *blocks,
                        crate::store::SstKey::at(rowid, commit_lsn),
                        &encoded[..len],
                    )
                    .unwrap();
            } else {
                writer
                    .append_tombstone_version(
                        &mut *blocks,
                        crate::store::SstKey::at(rowid, commit_lsn),
                    )
                    .unwrap();
            }
        }
        fixture.roots[2] = writer.finish(&mut *blocks).unwrap().unwrap();
        drop(blocks);
        let location = fixture.storage.heap.append_row(&[Datum::Int4(99)]).unwrap();
        fixture.storage.tables[fixture.table]
            .rows
            .insert(0, super::super::RowState::committed_only_at(location, 7))
            .unwrap();
        fixture
    }

    #[test]
    fn stable_spill_generation_shared_merge_keeps_old_reads_and_row_metadata() {
        let PointFixture {
            storage,
            table,
            roots,
            arena,
            path,
        } = scan_fixture("stable-merge");
        let mut state =
            super::super::RowState::committed_only_at(super::super::RowLoc::test(0, 0), 9);
        state.committed = Some(super::super::RowHome::Spilled {
            len: 7,
            sst: SpillGenerationId::fixture(1),
            commit_lsn: 9,
        });
        storage.tables[table].rows.write().insert(1, state).unwrap();
        let mut retained = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            let old = storage
                .retain_spilled_row(table, 1, 7, SpillGenerationId::fixture(0), 7)
                .unwrap();
            let current = storage
                .retain_spilled_row(table, 1, 7, SpillGenerationId::fixture(1), 9)
                .unwrap();
            let row_owner = storage.tables[table].rows.read();
            let version_owner = storage.row_versions.read();
            let target = storage
                .capture_spill_publication(
                    table,
                    storage.table(table).created_at(),
                    storage.table(table).rewrite_generation(),
                )
                .unwrap();
            assert_eq!(
                storage
                    .merge_spill_pair(&target, 0, (roots[0], roots[1]), Some(roots[2]))
                    .unwrap(),
                SpillInstallation::Installed
            );
            assert_eq!(row_owner.get(&1).unwrap().committed, state.committed);
            assert_point_value(&old, &arena, 41);
            assert_point_value(&current, &arena, 88);
            for (id, lsn, value) in [(0, 7, 41), (1, 9, 88)] {
                let new = storage
                    .retain_spilled_row(table, 1, 7, SpillGenerationId::fixture(id), lsn)
                    .unwrap();
                assert_point_value(&new, &arena, value);
            }
            storage
                .spill_roots
                .copy_roots(false, &mut retained)
                .unwrap();
            assert_eq!(retained, roots[..2]);
            assert_eq!(
                storage
                    .merge_spill_pair(&target, 0, (roots[0], roots[1]), Some(roots[2]))
                    .unwrap_err()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            drop(version_owner);
            drop(row_owner);
            drop(old);
            drop(current);
            storage
                .spill_roots
                .copy_roots(false, &mut retained)
                .unwrap();
            assert!(retained.is_empty());
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn stable_spill_generation_receipts_reject_foreign_and_retired_tables() {
        let mut config = test_config();
        config.max_spill_generations_per_table = 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("receipt", &[("id", ColType::Int4, true)]))
            .unwrap();
        let mut other_budget = test_budget(&config);
        let other = Storage::new(&config, &mut other_budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let receipt = storage
                .reserve_spill_generation(
                    table,
                    storage.table(table).created_at(),
                    storage.table(table).rewrite_generation(),
                )
                .unwrap();
            assert_eq!(
                other
                    .append_spill_through(&receipt, handle(1), 7)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            assert_eq!(
                storage
                    .append_spill_through(&receipt, handle(1), 7)
                    .unwrap(),
                SpillInstallation::Installed
            );
            storage.clear_spill_publication(&receipt.target).unwrap();
            assert_eq!(
                storage
                    .append_spill_through(&receipt, handle(5), 9)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            let pending = storage
                .reserve_spill_generation(
                    table,
                    storage.table(table).created_at(),
                    storage.table(table).rewrite_generation(),
                )
                .unwrap();
            storage.tables[table].publication().rewrite_generation += 1;
            assert_eq!(
                storage
                    .append_spill_through(&pending, handle(9), 11)
                    .unwrap(),
                SpillInstallation::RetiredTable
            );
            let pending = storage
                .reserve_spill_generation(
                    table,
                    storage.table(table).created_at(),
                    storage.table(table).rewrite_generation(),
                )
                .unwrap();
            storage.commit_drop(table);
            assert_eq!(
                storage
                    .create_table(make_def("replacement", &[("id", ColType::Text, true)]))
                    .unwrap(),
                table
            );
            assert_eq!(
                storage
                    .append_spill_through(&pending, handle(9), 11)
                    .unwrap(),
                SpillInstallation::RetiredTable
            );
            assert_eq!(
                storage.clear_spill_publication(&pending.target).unwrap(),
                SpillInstallation::RetiredTable
            );
            assert!(storage.table(table).spill_generations().is_empty());
        });
    }

    #[test]
    fn captured_row_scans_metadata_keeps_files_and_boundary_before_callbacks() {
        use crate::sql::types::Datum;
        let PointFixture {
            storage,
            table,
            roots,
            arena: _arena,
            path,
        } = scan_fixture("metadata-partition");
        let mut seen = [u64::MAX; 3];
        let mut copied = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            storage.set_commit_snapshot(7);
            let mut count = 0;
            storage
                .for_each_row_state(table, &mut |rowid, state| {
                    seen[count] = rowid;
                    count += 1;
                    if rowid == 0 {
                        drop(state);
                        assert!(storage.tables[table].rows.test_write().is_ok());
                        assert!(storage.row_versions.test_write().is_ok());
                        storage
                            .table(table)
                            .spill_generations
                            .write()
                            .replace(1, [roots[2]].into_iter())?;
                        storage.set_commit_snapshot(u64::MAX);
                        storage.spill_roots.copy_roots(false, &mut copied)?;
                        assert_eq!(copied, roots[..2]);
                    } else {
                        assert_eq!(state.committed_lsn, 7);
                        let home = storage
                            .visible_row_home_at(
                                table,
                                rowid,
                                state,
                                8,
                                super::super::SNAPSHOT_ALL,
                                7,
                            )?
                            .unwrap();
                        storage.with_row_bytes(table, rowid, home, |bytes| {
                            let mut values = [Datum::Null];
                            crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                            assert_eq!(values, [Datum::Int4(if rowid == 1 { 41 } else { 17 })]);
                            Ok(())
                        })?;
                    }
                    Ok(core::ops::ControlFlow::Continue(()))
                })
                .unwrap();
            assert_eq!(seen, [0, 1, 2]);
            storage.spill_roots.copy_roots(false, &mut copied).unwrap();
            assert!(copied.is_empty());
            storage
                .table(table)
                .spill_generations
                .write()
                .replace(2, roots[..2].iter().copied())
                .unwrap();
            storage.set_commit_snapshot(7);
            let error = storage
                .for_each_row_state(table, &mut |_, state| {
                    drop(state);
                    Err(sql_err!(
                        sqlstate::QUERY_CANCELED,
                        "cancel captured overlay"
                    ))
                })
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::QUERY_CANCELED);
            storage.spill_roots.copy_roots(false, &mut copied).unwrap();
            assert!(copied.is_empty());
            assert!(
                storage
                    .query_roots
                    .slots
                    .iter()
                    .all(|slot| !slot.retained.load(Ordering::Acquire))
            );
            assert!(storage.row_scan(table).is_ok());
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn captured_row_scans_statement_bytes_keep_partition_through_replacement() {
        use crate::sql::types::Datum;
        let PointFixture {
            storage,
            table,
            roots,
            arena,
            path,
        } = scan_fixture("statement-partition");
        let mut copied = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            storage.set_commit_snapshot(7);
            let scan = storage.row_scan_in(table, &arena).unwrap();
            assert!(scan.rowids.contains(0));
            let mut overlay = 0;
            let completed = scan
                .for_each_overlay(&mut |rowid, state| {
                    assert_eq!(rowid, 0);
                    drop(state);
                    overlay += 1;
                    storage
                        .table(table)
                        .spill_generations
                        .write()
                        .replace(1, [roots[2]].into_iter())?;
                    storage.set_commit_snapshot(u64::MAX);
                    Ok(core::ops::ControlFlow::Continue(()))
                })
                .unwrap();
            assert!(completed.is_continue());
            assert_eq!(overlay, 1);
            for columns in [None, Some(1)] {
                let mut ids = [0u64; 2];
                let mut count = 0;
                scan.for_each_spilled_row_batch(&arena, true, columns, &mut |batch| {
                    assert!(storage.tables[table].rows.test_write().is_ok());
                    for row in batch {
                        ids[count] = row.rowid;
                        count += 1;
                        let expected = Datum::Int4(if row.rowid == 1 { 41 } else { 17 });
                        match row.representation {
                            super::super::SpilledRowRepresentation::Encoded(bytes) => {
                                let mut values = [Datum::Null];
                                crate::storage::rowenc::decode(
                                    bytes,
                                    &[ColType::Int4],
                                    &mut values,
                                )?;
                                assert_eq!(values, [expected]);
                            }
                            super::super::SpilledRowRepresentation::Values(values) => {
                                assert_eq!(values, [expected])
                            }
                        }
                    }
                    Ok(core::ops::ControlFlow::Continue(()))
                })
                .unwrap();
                assert_eq!((count, ids), (2, [1, 2]));
            }
            let later = storage.row_scan_in(table, &arena).unwrap();
            later
                .roots
                .with_view(|source| {
                    assert_eq!(source.handles, [roots[2]]);
                    assert_eq!(source.commit_lsn(), u64::MAX);
                    Ok(())
                })
                .unwrap();
            drop(later);
            storage.spill_roots.copy_roots(false, &mut copied).unwrap();
            assert_eq!(copied, roots[..2]);
            let error = scan
                .for_each_spilled_row_batch(&arena, true, None, &mut |_| {
                    Err(sql_err!(sqlstate::QUERY_CANCELED, "cancel captured bytes"))
                })
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::QUERY_CANCELED);
            drop(scan);
            storage.spill_roots.copy_roots(false, &mut copied).unwrap();
            assert!(copied.is_empty());
            assert!(
                storage
                    .query_roots
                    .slots
                    .iter()
                    .all(|slot| !slot.retained.load(Ordering::Acquire))
            );
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn captured_row_scans_point_source_survives_statement_memory_reset() {
        let PointFixture {
            storage,
            table,
            roots,
            mut arena,
            path,
        } = scan_fixture("owned-overlay");
        let mut state =
            super::super::RowState::committed_only_at(super::super::RowLoc::test(0, 0), 7);
        state.committed = Some(super::super::RowHome::Spilled {
            len: 7,
            sst: crate::storage::SpillGenerationId::fixture(0),
            commit_lsn: 7,
        });
        state.checkpoint_change_lsn = 7;
        storage.tables[table].rows.write().insert(2, state).unwrap();
        let mut saved = None;
        let mut copied = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            storage.set_commit_snapshot(7);
            let scan = storage.row_scan_in(table, &arena).unwrap();
            assert!(scan.rowids.contains(2));
            storage.tables[table].rows.write().remove(&2);
            storage
                .table(table)
                .spill_generations
                .write()
                .replace(1, [roots[2]].into_iter())
                .unwrap();
            let completed = scan
                .for_each_overlay(&mut |rowid, state| {
                    if rowid == 2 {
                        assert!(
                            state.committed.is_none(),
                            "captured newest deletion remains metadata"
                        );
                        let home = storage
                            .visible_row_home_at(
                                table,
                                rowid,
                                state,
                                8,
                                super::super::SNAPSHOT_ALL,
                                7,
                            )?
                            .unwrap();
                        let super::super::VisibleRowBytes::Spilled(source) = home.bytes else {
                            unreachable!()
                        };
                        saved = Some(source);
                    }
                    Ok(core::ops::ControlFlow::Continue(()))
                })
                .unwrap();
            assert!(completed.is_continue());
            drop(scan);
            arena.reset();
            let source = saved.take().unwrap();
            assert_point_value(&source, &arena, 17);
            storage.spill_roots.copy_roots(false, &mut copied).unwrap();
            assert_eq!(copied, roots[..2]);
            drop(source);
            storage.spill_roots.copy_roots(false, &mut copied).unwrap();
            assert!(copied.is_empty());
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn captured_row_scans_resident_spill_uses_captured_versions_after_replacement() {
        use crate::sql::types::Datum;
        let PointFixture {
            storage,
            table,
            roots,
            arena,
            path,
        } = scan_fixture("resident-spill");
        let mut state =
            super::super::RowState::committed_only_at(super::super::RowLoc::test(0, 0), 9);
        state.committed = Some(super::super::RowHome::Spilled {
            len: 7,
            sst: crate::storage::SpillGenerationId::fixture(1),
            commit_lsn: 9,
        });
        state.checkpoint_change_lsn = 9;
        storage.tables[table].rows.write().insert(1, state).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            storage.set_commit_snapshot(7);
            let scan = storage.row_scan_in(table, &arena).unwrap();
            {
                let mut generations = storage.table(table).spill_generations.write();
                let mut rows = storage.tables[table].rows.write();
                generations.replace(1, [roots[2]].into_iter()).unwrap();
                rows.get_mut(&1).unwrap().committed = Some(super::super::RowHome::Spilled {
                    len: 7,
                    sst: crate::storage::SpillGenerationId::fixture(2),
                    commit_lsn: 9,
                });
            }
            for latest in [false, true] {
                let mut selected = 0;
                let completed = scan
                    .for_each_overlay(&mut |rowid, state| {
                        if rowid != 1 {
                            drop(state);
                            return Ok(core::ops::ControlFlow::Continue(()));
                        }
                        let home = if latest {
                            storage.committed_row_home(table, rowid, state)?
                        } else {
                            storage.visible_row_home_at(
                                table,
                                rowid,
                                state,
                                8,
                                super::super::SNAPSHOT_ALL,
                                7,
                            )?
                        }
                        .unwrap();
                        assert!(storage.row_versions.test_write().is_ok());
                        selected += 1;
                        storage.with_row_bytes(table, rowid, home, |bytes| {
                            let mut values = [Datum::Null];
                            crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                            assert_eq!(values, [Datum::Int4(if latest { 88 } else { 41 })]);
                            Ok(())
                        })?;
                        Ok(core::ops::ControlFlow::Continue(()))
                    })
                    .unwrap();
                assert!(completed.is_continue());
                assert_eq!(selected, 1);
            }
            scan.roots
                .with_view(|source| {
                    assert_eq!(source.handles, &roots[..2]);
                    Ok(())
                })
                .unwrap();
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn captured_row_scans_statement_depth_and_failed_capture_release_are_bounded() {
        let PointFixture {
            storage,
            table,
            roots,
            arena,
            path,
        } = point_fixture("scan-capacity");
        let count = super::super::MAX_ROW_WALK_NESTING + 1;
        let mut scans = Vec::with_capacity(count);
        let source_capacity = storage.query_roots.slots_per_use;
        let mut leases = Vec::with_capacity(source_capacity);
        let mut small_budget = Budget::new(size_of::<ColType>());
        let small = crate::mem::arena::Arena::new(
            &mut small_budget,
            "partial scan source",
            size_of::<ColType>(),
        )
        .unwrap();
        let mut copied = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..count {
                scans.push(storage.row_scan_in(table, &arena).unwrap());
            }
            assert!(
                storage
                    .query_roots
                    .slots
                    .iter()
                    .all(|slot| !slot.retained.load(Ordering::Acquire)),
                "statement sources must preserve metadata source capacity"
            );
            assert!(storage.row_scan(table).is_ok());
            let error = storage.row_scan_in(table, &small).err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(error.message.as_str().contains("SST roots"));
            scans.clear();
            storage.spill_roots.copy_roots(false, &mut copied).unwrap();
            assert!(
                copied.is_empty(),
                "arena allocation failure cannot partially register roots"
            );
            for _ in 0..source_capacity {
                leases.push(storage.query_roots.retain(&storage, table, 7).unwrap());
            }
            assert_eq!(
                storage.row_scan(table).err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            leases.clear();
            assert!(
                storage.row_scan(table).is_ok(),
                "failed source capture releases its row identity slot"
            );
            storage.spill_roots.copy_roots(false, &mut copied).unwrap();
            assert!(copied.is_empty());
            assert_eq!(
                storage.table(table).spill_generations().handle(0),
                Some(roots[0])
            );
        });
        drop(scans);
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    fn assert_point_value(source: &SpilledRowRead, arena: &crate::mem::arena::Arena, value: i32) {
        use crate::sql::types::Datum;
        let bytes = source.copy_bytes(arena).unwrap();
        let mut values = [Datum::Null];
        crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values).unwrap();
        assert_eq!(values, [Datum::Int4(value)]);
        source
            .with_bytes(|bytes| {
                let mut values = [Datum::Null];
                crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                assert_eq!(values, [Datum::Int4(value)]);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn retained_object_rows_keep_versions_schema_and_deletions_after_retirement() {
        let PointFixture {
            mut storage,
            table,
            roots,
            arena,
            path,
        } = point_fixture("retirement");
        let mut retained = Vec::with_capacity(3);
        crate::mem::guard::forbid_alloc(|| {
            let state = storage
                .spill_row_state_at(table, 1, u64::MAX)
                .unwrap()
                .unwrap();
            let deleted = storage
                .spill_row_state_at(table, 2, u64::MAX)
                .unwrap()
                .unwrap();
            let current = state.visible_at(u64::MAX).unwrap().unwrap();
            let old = state.visible_at(7).unwrap().unwrap();
            let deleted_old = deleted.visible_at(7).unwrap().unwrap();
            assert!(deleted.visible_at(u64::MAX).unwrap().is_none());
            let deferred = storage
                .read_row_snapshot(
                    table,
                    2,
                    super::super::RowSnapshot {
                        created_at: storage.table(table).created_at(),
                        version: super::super::row_images::RowVersionIdentity::Committed(7),
                    },
                )
                .unwrap();
            assert_eq!(
                deferred.snapshot().version,
                super::super::row_images::RowVersionIdentity::Committed(7)
            );
            let bytes = storage.row_bytes(table, 2, deferred, &arena).unwrap();
            let mut values = [crate::sql::types::Datum::Null];
            crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values).unwrap();
            assert_eq!(values, [crate::sql::types::Datum::Int4(17)]);
            let registry = Arc::clone(&storage.spill_roots);
            storage.set_spill_list(table, &[roots[2]]);
            storage.drop_table(table);
            let mut definition = make_def("retirement", &[("id", ColType::Text, true)]);
            definition.persistence = RelationPersistence::Temporary;
            let replacement = storage.create_table(definition).unwrap();
            storage.set_spill_list(replacement, &[roots[2]]);
            drop(storage);
            registry.copy_roots(false, &mut retained).unwrap();
            assert_eq!(retained.len(), 2);
            assert!(retained.contains(&roots[0]) && retained.contains(&roots[1]));
            state
                .roots
                .with_snapshot(|snapshot| {
                    assert_eq!(snapshot.relation().schema[0], ColType::Int4);
                    Ok(())
                })
                .unwrap();
            assert_point_value(&current, &arena, 88);
            assert_point_value(&old, &arena, 41);
            assert_point_value(&deleted_old, &arena, 17);
            assert!(deleted.visible_at(9).unwrap().is_none());
            current.with_bytes(|_| old.with_bytes(|_| Ok(()))).unwrap();
            assert_eq!(
                old.validate_request(table, 2).unwrap_err().sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            let error = current
                .with_bytes::<()>(|_| {
                    Err(sql_err!(sqlstate::QUERY_CANCELED, "cancel retained row"))
                })
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::QUERY_CANCELED);
            drop((state, deleted, current, old, deleted_old));
            registry.copy_roots(false, &mut retained).unwrap();
            assert!(retained.is_empty());
        });
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn retained_object_rows_use_captured_roots_for_latest_committed_validation() {
        let PointFixture {
            storage,
            table,
            arena,
            path,
            ..
        } = point_fixture("validation");
        crate::mem::guard::forbid_alloc(|| {
            storage.set_commit_snapshot(7);
            storage
                .for_each_row_state(table, &mut |rowid, state| {
                    assert_eq!(state.committed_lsn, 7);
                    let home = storage.committed_row_home(table, rowid, state)?;
                    if rowid == 1 {
                        let home = home.expect("newer committed row");
                        assert_eq!(
                            home.snapshot().version,
                            super::super::row_images::RowVersionIdentity::Committed(9)
                        );
                        let super::super::VisibleRowBytes::Spilled(source) = home.bytes else {
                            unreachable!()
                        };
                        assert_point_value(&source, &arena, 88);
                    } else {
                        assert!(home.is_none(), "latest deletion wins uniqueness validation");
                    }
                    Ok(core::ops::ControlFlow::Continue(()))
                })
                .unwrap();
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn retained_object_rows_allow_replacement_while_a_reader_consumes_old_bytes() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        struct Release<'a>(&'a AtomicBool);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let PointFixture {
            mut storage,
            table,
            roots,
            path,
            ..
        } = point_fixture("overlap");
        let state = storage
            .spill_row_state_at(table, 1, u64::MAX)
            .unwrap()
            .unwrap();
        let source = state.visible_at(7).unwrap().unwrap();
        let ready = AtomicBool::new(false);
        let replaced = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let finish = Release(&replaced);
            let reader = scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| {
                    source
                        .with_bytes(|bytes| {
                            ready.store(true, Ordering::Release);
                            while !replaced.load(Ordering::Acquire) {
                                std::thread::yield_now();
                            }
                            let mut values = [crate::sql::types::Datum::Null];
                            crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                            assert_eq!(values, [crate::sql::types::Datum::Int4(41)]);
                            Ok(())
                        })
                        .unwrap();
                });
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ready.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::yield_now();
            }
            let reading = ready.load(Ordering::Acquire);
            if reading {
                crate::mem::guard::forbid_alloc(|| {
                    storage.set_spill_list(table, &[roots[2]]);
                    storage.drop_table(table);
                    let mut definition = make_def("overlap", &[("id", ColType::Text, true)]);
                    definition.persistence = RelationPersistence::Temporary;
                    let replacement = storage.create_table(definition).unwrap();
                    storage.set_spill_list(replacement, &[roots[2]]);
                });
            }
            drop(finish);
            reader.join().unwrap();
            assert!(
                reading,
                "replacement must finish before the reader releases its bytes"
            );
        });
        drop((state, source, storage));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn retained_object_rows_bound_scratch_and_release_after_nested_errors() {
        let PointFixture {
            storage,
            table,
            arena,
            path,
            ..
        } = point_fixture("scratch");
        let mut tiny_budget = Budget::new(1);
        let tiny_arena = crate::mem::arena::Arena::new(&mut tiny_budget, "tiny result", 1).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let state = storage
                .spill_row_state_at(table, 1, u64::MAX)
                .unwrap()
                .unwrap();
            let row = state.visible_at(7).unwrap().unwrap();
            row.with_bytes(|_| {
                row.with_bytes(|_| {
                    let error = row.with_bytes(|_| Ok(())).unwrap_err();
                    assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                    Ok(())
                })
            })
            .unwrap();
            assert_point_value(&row, &arena, 41);
            let mut oversized = state.visible_at(7).unwrap().unwrap();
            oversized.len = u32::MAX;
            let error = oversized.with_bytes(|_| Ok(())).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_point_value(&row, &arena, 41);
            let error = state
                .visible_at(7)
                .unwrap()
                .unwrap()
                .copy_bytes(&tiny_arena)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        });
        drop(storage);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn query_sst_roots_charge_exact_startup_capacity_and_reuse_released_slots() {
        let mut config = test_config();
        config.query_workspace_slots = 1;
        config.max_spill_generations_per_table = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("query_roots", &[("id", ColType::Int4, true)]))
            .unwrap();
        storage.set_spill_list(table, &[handle(1)]);
        let expected = QueryRootPool::budget_bytes(&config);
        let mut pool_budget = Budget::new(expected);
        let pool = QueryRootPool::new(&config, &mut pool_budget).unwrap();
        assert_eq!(pool_budget.used(), expected);
        let mut leases = Vec::with_capacity(pool.slots.len());
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..pool.slots_per_use {
                leases.push(pool.retain(&storage, table, 7).unwrap());
            }
            assert_eq!(
                pool.retain(&storage, table, 9).err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            drop(leases.pop());
            let replacement = pool.retain(&storage, table, 9).unwrap();
            replacement
                .with_snapshot(|snapshot| {
                    assert_eq!(snapshot.commit_lsn, 9);
                    assert_eq!(snapshot.relation().handle(0), handle(1));
                    replacement.with_snapshot(|nested| {
                        assert_eq!(nested.commit_lsn, snapshot.commit_lsn);
                        assert_eq!(nested.relation().handle(0), snapshot.relation().handle(0));
                        Ok(())
                    })?;
                    Ok(())
                })
                .unwrap();
            drop(replacement);
            leases.clear();
            assert!(
                pool.slots
                    .iter()
                    .all(|slot| !slot.retained.load(Ordering::Acquire))
            );
        });
    }

    #[test]
    fn retained_object_rows_preserve_scan_capacity_and_reuse_only_after_last_reader() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("source_capacity", &[("id", ColType::Int4, true)]))
            .unwrap();
        storage.set_spill_list(table, &[handle(1)]);
        let pool = Arc::clone(&storage.query_roots);
        let mut scans = Vec::with_capacity(pool.slots_per_use);
        let mut points = Vec::with_capacity(pool.slots_per_use);
        let mut retained = Vec::with_capacity(1);
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..pool.slots_per_use {
                scans.push(pool.retain(&storage, table, 7).unwrap());
                points.push(pool.retain_point(&storage, table, 7).unwrap());
            }
            assert_eq!(
                pool.retain(&storage, table, 7).err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(
                pool.retain_point(&storage, table, 7)
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            let source = points.pop().unwrap();
            let copy = source.try_clone().unwrap();
            let slot = source.slot;
            drop(source);
            assert!(pool.slots[slot].retained.load(Ordering::Acquire));
            assert_eq!(
                pool.retain_point(&storage, table, 7)
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            drop(copy);
            assert!(!pool.slots[slot].retained.load(Ordering::Acquire));
            let replacement = pool.retain_point(&storage, table, 7).unwrap();
            pool.slots[replacement.slot]
                .readers
                .store(usize::MAX, Ordering::Relaxed);
            assert_eq!(
                replacement.try_clone().err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            pool.slots[replacement.slot]
                .readers
                .store(1, Ordering::Relaxed);
            storage
                .copy_retained_spill_roots(true, &mut retained)
                .unwrap();
            assert_eq!(retained, [handle(1)]);
            drop(replacement);
            scans.clear();
            points.clear();
            storage
                .copy_retained_spill_roots(true, &mut retained)
                .unwrap();
            assert!(retained.is_empty());
            assert!(storage.spill_roots.take_reclamation_pending());
            assert!(
                pool.slots
                    .iter()
                    .all(|slot| !slot.retained.load(Ordering::Acquire)
                        && slot.readers.load(Ordering::Relaxed) == 0)
            );
        });
    }

    #[test]
    fn query_sst_roots_release_failed_capture_without_partial_pins() {
        let mut config = test_config();
        config.max_spill_generations_per_table = 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def(
                "query_root_failure",
                &[("id", ColType::Int4, true)],
            ))
            .unwrap();
        storage.set_spill_list(table, &[handle(1), handle(2)]);
        config.max_spill_generations_per_table = 1;
        let mut pool_budget = Budget::new(QueryRootPool::budget_bytes(&config));
        let pool = QueryRootPool::new(&config, &mut pool_budget).unwrap();
        let mut retained = Vec::with_capacity(2);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                pool.retain(&storage, table, 7).err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(
                pool.retain_point(&storage, table, 7)
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert!(
                pool.slots
                    .iter()
                    .all(|slot| !slot.retained.load(Ordering::Acquire))
            );
            storage
                .copy_retained_spill_roots(true, &mut retained)
                .unwrap();
            assert!(retained.is_empty());
            storage.set_spill_list(table, &[handle(1)]);
            drop(pool.retain(&storage, table, 7).unwrap());
        });
    }

    #[test]
    fn query_sst_roots_read_captured_versions_after_table_and_storage_retirement() {
        use crate::sql::types::Datum;
        use crate::storage::row_walks::RowScanIds;
        use crate::storage::{SpillOverlayMode, SpilledRowRepresentation, rowenc};
        use crate::store::{EphemeralBlockStore, SstKey, SstWriter};
        let mut config = test_config();
        config.max_spill_generations_per_table = 2;
        let bytes = 32 * crate::store::BLOCK_SIZE;
        let mut budget = Budget::new(
            Storage::extra_budget_bytes(&config)
                + config.memtable_bytes
                + SpillReader::budget_bytes(false, 2)
                + EphemeralBlockStore::budget_bytes(bytes)
                + (1 << 20),
        );
        let path =
            std::env::temp_dir().join(format!("pos3ql-query-root-{}.blocks", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut blocks = EphemeralBlockStore::open(&mut budget, &path, bytes).unwrap();
        let mut writer = Box::new(SstWriter::new());
        let mut row = [0u8; 32];
        for (id, lsn, value) in [(1, 9, 88), (1, 7, 41), (2, 9, 99)] {
            let values = [Datum::Int4(value)];
            let len = rowenc::encoded_len(&values);
            rowenc::encode(&values, &mut row[..len]);
            writer
                .append_version(&mut blocks, SstKey::at(id, lsn), &row[..len])
                .unwrap();
        }
        let root = writer.finish(&mut blocks).unwrap().unwrap();
        let blocks = Arc::new(Mutex::new(blocks));
        let reader = SpillReader::new(&mut budget, 2, None, Some(blocks)).unwrap();
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        storage.attach_spill(reader);
        let mut definition = make_def("retiring_query", &[("id", ColType::Int4, true)]);
        definition.persistence = RelationPersistence::Temporary;
        let table = storage.create_table(definition).unwrap();
        storage.set_spill_list(table, &[root]);
        let mut arena =
            crate::mem::arena::Arena::new(&mut budget, "query root result", 1 << 18).unwrap();
        let mut retained = Vec::with_capacity(1);
        crate::mem::guard::forbid_alloc(|| {
            storage.set_commit_snapshot(7);
            let scan = storage.row_scan(table).unwrap();
            assert!(matches!(&scan.rowids, RowScanIds::Reserved(_)));
            let roots = scan.roots.owned_source(&storage).unwrap().unwrap();
            let mut metadata_rows = 0;
            Storage::spill_merged_walk(&roots, &scan.rowids, &mut |rowid, source| {
                let lsn = source.version.commit_lsn;
                assert_eq!((rowid, lsn), (1, 7));
                storage.set_commit_snapshot(u64::MAX);
                metadata_rows += 1;
                Ok(core::ops::ControlFlow::Continue(()))
            })
            .unwrap();
            assert_eq!(
                metadata_rows, 1,
                "nested callbacks cannot advance the outer read boundary"
            );
            storage.set_commit_snapshot(7);
            let error = Storage::spill_merged_walk(&roots, &scan.rowids, &mut |_, _| {
                Err(sql_err!(sqlstate::QUERY_CANCELED, "cancel retained scan"))
            })
            .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::QUERY_CANCELED);
            drop(roots);
            drop(scan);
            assert!(
                storage
                    .query_roots
                    .slots
                    .iter()
                    .all(|slot| !slot.retained.load(Ordering::Acquire))
            );
            let lease = storage.query_roots.retain(&storage, table, 7).unwrap();
            let registry = lease
                .with_snapshot(|snapshot| Ok(Arc::clone(snapshot.registry.as_ref().unwrap())))
                .unwrap();
            storage.drop_table(table);
            storage
                .create_table(make_def("retiring_query", &[("id", ColType::Text, true)]))
                .unwrap();
            drop(storage);
            registry.copy_roots(false, &mut retained).unwrap();
            assert_eq!(retained, [root]);
            let mut count = 0;
            lease
                .with_snapshot(|snapshot| {
                    let relation = snapshot.relation();
                    assert_eq!(relation.schema[0], ColType::Int4);
                    assert_eq!(snapshot.commit_lsn, 7);
                    Storage::walk_spill_bytes(
                        snapshot.reader.as_ref().unwrap(),
                        &relation,
                        snapshot.commit_lsn,
                        &arena,
                        true,
                        None,
                        true,
                        SpillOverlayMode::VisibleScan(&RowScanIds::Statement(&[])),
                        None,
                        &mut |rowid, lsn, representation| {
                            assert_eq!((rowid, lsn), (1, 7));
                            let mut decoded = [Datum::Null; MAX_COLUMNS];
                            let values = match representation {
                                SpilledRowRepresentation::Encoded(bytes) => {
                                    rowenc::decode(
                                        bytes,
                                        &relation.schema[..relation.n_columns],
                                        &mut decoded,
                                    )?;
                                    &decoded[..relation.n_columns]
                                }
                                SpilledRowRepresentation::Values(values) => values,
                            };
                            assert_eq!(values, [Datum::Int4(41)]);
                            count += 1;
                            Ok(core::ops::ControlFlow::Continue(()))
                        },
                    )?;
                    Ok(())
                })
                .unwrap();
            assert_eq!(count, 1);
            drop(lease);
            registry.copy_roots(false, &mut retained).unwrap();
            assert!(retained.is_empty());
            assert!(!registry.take_reclamation_pending());
        });
        arena.reset();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn retained_sst_registry_preflights_capacity_and_reader_overflow() {
        let mut config = test_config();
        config.max_tables = 1;
        config.max_spill_generations_per_table = 1;
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        assert_eq!(budget.remaining(), 0);
        let roots = [handle(1), handle(5), handle(9), handle(13)];
        let mut copied = Vec::with_capacity(4);
        crate::mem::guard::forbid_alloc(|| {
            registry.pin(&roots, true).unwrap();
            registry.pin(&[roots[0], roots[0]], true).unwrap();
            let error = registry.pin(&[handle(17)], true).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            registry.copy_roots(true, &mut copied).unwrap();
            assert_eq!(copied, roots);
            {
                let mut pinned = registry.roots.lock().unwrap();
                assert_eq!(pinned[0].readers.get(), 3);
                pinned[0].readers = NonZeroUsize::new(usize::MAX).unwrap();
            }
            let error = registry.pin(&roots[..2], true).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            {
                let mut pinned = registry.roots.lock().unwrap();
                assert_eq!(
                    pinned[1].readers.get(),
                    1,
                    "failure cannot partially pin another root"
                );
                pinned[0].readers = NonZeroUsize::new(3).unwrap();
            }
            registry.release(&[roots[0], roots[0]], true);
            registry.copy_roots(true, &mut copied).unwrap();
            assert_eq!(copied.len(), 4);
            registry.release(&roots, true);
            assert!(registry.take_reclamation_pending());
            assert!(!registry.take_reclamation_pending());
            registry.copy_roots(true, &mut copied).unwrap();
            assert!(copied.is_empty());
            registry.pin(&[handle(17)], true).unwrap();
            registry.release(&[handle(17)], true);
        });
    }

    #[test]
    fn retained_sst_registry_keeps_durable_and_temporary_namespaces_separate() {
        let config = test_config();
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        let root = handle(1);
        let mut copied = Vec::with_capacity(1);
        let mut too_small = Vec::new();
        crate::mem::guard::forbid_alloc(|| {
            registry.pin(&[root], true).unwrap();
            registry.pin(&[root], false).unwrap();
            let error = registry.copy_roots(true, &mut too_small).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            registry.copy_roots(true, &mut copied).unwrap();
            assert_eq!(copied, [root]);
            registry.copy_roots(false, &mut copied).unwrap();
            assert_eq!(copied, [root]);
            registry.release(&[root], true);
            registry.copy_roots(true, &mut copied).unwrap();
            assert!(copied.is_empty());
            registry.copy_roots(false, &mut copied).unwrap();
            assert_eq!(copied, [root]);
            registry.release(&[root], false);
            registry.copy_roots(false, &mut copied).unwrap();
            assert!(copied.is_empty());
        });
    }

    #[test]
    fn owned_row_lookup_keeps_object_sources_and_older_versions_after_retirement() {
        use crate::sql::types::Datum;
        use crate::storage::{RowHome, RowLoc, RowState};
        let PointFixture {
            mut storage,
            table,
            roots,
            arena,
            path,
        } = point_fixture("owned_lookup");
        let retained = crate::mem::guard::forbid_alloc(|| {
            let length = crate::storage::rowenc::encoded_len(&[Datum::Int4(88)]) as u32;
            let mut row = RowState::committed_only_at(RowLoc::test(0, length), 9);
            row.committed = Some(RowHome::Spilled {
                len: length,
                sst: crate::storage::SpillGenerationId::fixture(1),
                commit_lsn: 9,
            });
            storage.tables[table].rows.insert(1, row).unwrap();
            storage.resident_row_state(table, 1).unwrap().unwrap()
        });
        crate::mem::guard::forbid_alloc(|| {
            storage.commit_drop(table);
            let replacement = storage
                .create_table(make_def(
                    "lookup_replacement",
                    &[("id", ColType::Text, false)],
                ))
                .unwrap();
            assert_eq!(replacement, table);
            storage.set_spill_list(table, &[roots[2]]);
            drop(storage);
            for (snapshot, expected) in [(7, 41), (9, 88)] {
                let image = retained
                    .select_owned(table, 1, 0, super::super::SNAPSHOT_ALL, snapshot)
                    .unwrap()
                    .unwrap();
                let super::super::row_images::VisibleRowBytes::Spilled(source) = image.bytes else {
                    panic!("object fixture")
                };
                assert_point_value(&source, &arena, expected);
            }
            drop(retained);
        });
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn retained_sst_images_survive_replacement_and_storage_drop() {
        let mut config = test_config();
        config.max_spill_generations_per_table = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("retained_sst", &[("id", ColType::Int4, true)]))
            .unwrap();
        let original = handle(1);
        let replacement = handle(5);
        storage.set_spill_list(table, &[original]);
        let mut snapshot = SpillGenerationSnapshot::new(1);
        let mut too_small = SpillGenerationSnapshot::new(0);
        let mut roots = Vec::with_capacity(1);
        let mut other_budget = test_budget(&config);
        let other = Storage::new(&config, &mut other_budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let error = too_small.capture(&storage, table).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(too_small.registry.is_none());
            snapshot.capture(&storage, table).unwrap();
            assert!(snapshot.validate(&storage, table).is_ok());
            assert_eq!(
                snapshot.validate(&other, table).unwrap_err().sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            storage.collapse_spill(table, replacement).unwrap();
            assert_eq!(snapshot.relation().handle(0), original);
            assert_eq!(
                storage.table(table).spill_generations().handle(0).unwrap(),
                replacement
            );
            storage.drop_table(table);
            assert_eq!(
                snapshot.validate(&storage, table).unwrap_err().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            let registry = Arc::clone(snapshot.registry.as_ref().unwrap());
            drop(storage);
            registry.copy_roots(true, &mut roots).unwrap();
            assert_eq!(roots, [original]);
            assert_eq!(snapshot.relation().schema[0], ColType::Int4);
            snapshot.clear();
            registry.copy_roots(true, &mut roots).unwrap();
            assert!(roots.is_empty());
        });
    }
}
