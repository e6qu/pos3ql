//! Selected enum labels remain immutable until their last reader releases them.

use {crate::mem::mutex::Mutex, std::sync::Arc};

use super::EnumMember;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

/// Creation order restores member ordinals after each allocation-free validation sort.
fn validate_members(members: &mut [EnumMember]) -> Result<(), SqlError> {
    let mut previous = 0;
    for member in members.iter() {
        let creation = member
            .identity
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "enum member has no creation identity"
                )
            })?
            .created_at();
        if creation <= previous
            || !member.sort.is_finite()
            || f64::from(member.sort as f32) != member.sort
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "enum member identity or sort position is invalid"
            ));
        }
        previous = creation;
    }
    members.sort_unstable_by(|left, right| left.label.as_str().cmp(right.label.as_str()));
    let duplicate_label = members
        .windows(2)
        .any(|pair| pair[0].label == pair[1].label);
    members.sort_unstable_by(|left, right| left.sort.total_cmp(&right.sort));
    let duplicate_sort = members.windows(2).any(|pair| pair[0].sort == pair[1].sort);
    members.sort_unstable_by_key(|member| {
        member
            .identity
            .expect("checked member identity")
            .created_at()
    });
    if duplicate_label || duplicate_sort {
        return Err(sql_err!(
            sqlstate::DATA_EXCEPTION,
            "enum labels or sort positions are duplicated"
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct ImageSlot {
    owned: bool,
    readers: usize,
}

#[derive(Debug)]
pub(super) struct EnumMemberImages {
    members: FixedVec<EnumMember>,
    slots: FixedVec<ImageSlot>,
    width: usize,
}

impl EnumMemberImages {
    pub(super) fn budget_bytes(images: usize, width: usize) -> usize {
        images
            .saturating_mul(width)
            .saturating_mul(size_of::<EnumMember>())
            .saturating_add(images.saturating_mul(size_of::<ImageSlot>()))
            .saturating_add(
                size_of::<Arc<Mutex<Self>>>() + size_of::<Mutex<Self>>() + 2 * size_of::<usize>(),
            )
    }

    pub(super) fn new(
        budget: &mut Budget,
        images: usize,
        width: usize,
    ) -> Result<Arc<Mutex<Self>>, BudgetError> {
        if images == 0 || images > u32::MAX as usize {
            return Err(BudgetError {
                what: "enum member-image address range",
                requested: images,
                remaining: budget.remaining(),
                total: budget.total(),
            });
        }
        let entries = images.checked_mul(width).ok_or(BudgetError {
            what: "enum member-image slab",
            requested: usize::MAX,
            remaining: budget.remaining(),
            total: budget.total(),
        })?;
        budget.draw(
            size_of::<Arc<Mutex<Self>>>() + size_of::<Mutex<Self>>() + 2 * size_of::<usize>(),
            "enum member-image controls",
        )?;
        let mut members = FixedVec::new(budget, "enum_members", entries)?;
        for _ in 0..entries {
            members
                .push(EnumMember::EMPTY)
                .expect("reserved label slab");
        }
        let mut slots = FixedVec::new(budget, "enum member-image slots", images)?;
        for index in 0..images {
            slots
                .push(ImageSlot {
                    // The final image is the isolated replay scratch.
                    owned: index == images - 1,
                    readers: 0,
                })
                .expect("reserved image controls");
        }
        Ok(Arc::new(Mutex::new(Self {
            members,
            slots,
            width,
        })))
    }

    fn validate_count(&self, count: usize) -> Result<u32, SqlError> {
        if count > self.width {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "an enum type may have at most {} labels",
                self.width
            ));
        }
        u32::try_from(count).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "enum label count exceeds durable representation"
            )
        })
    }

    fn reserve(&mut self) -> Result<u32, SqlError> {
        let image = self
            .slots
            .iter()
            .position(|slot| !slot.owned && slot.readers == 0)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "enum member-image pool is exhausted"
                )
            })?;
        self.slots[image].owned = true;
        Ok(image as u32)
    }

    pub(super) fn release(&mut self, image: u32) {
        assert_ne!(image as usize, self.slots.len() - 1);
        let slot = &mut self.slots[image as usize];
        assert!(slot.owned, "enum member image has a logical owner");
        slot.owned = false;
    }

    pub(super) fn replay_image(&self) -> u32 {
        (self.slots.len() - 1) as u32
    }

    pub(super) fn replay_members(&self, count: usize) -> &[EnumMember] {
        let start = self.replay_image() as usize * self.width;
        &self.members[start..start + count]
    }

    pub(super) fn replay_members_mut(&mut self, count: usize) -> &mut [EnumMember] {
        let start = self.replay_image() as usize * self.width;
        &mut self.members[start..start + count]
    }

    pub(super) fn image_members(&self, image: u32, count: usize) -> &[EnumMember] {
        let start = image as usize * self.width;
        &self.members[start..start + count]
    }

    pub(super) fn validate_replay(&mut self, count: usize) -> Result<(), SqlError> {
        self.validate_count(count)?;
        let start = self.replay_image() as usize * self.width;
        validate_members(&mut self.members.as_mut_slice()[start..start + count])
    }

    pub(super) fn replace_from_replay(
        &mut self,
        current: Option<u32>,
        count: usize,
    ) -> Result<(u32, u32), SqlError> {
        let count = self.validate_count(count)?;
        let mut previous = 0;
        for member in self.replay_members(count as usize) {
            let identity = member.identity.ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "stored enum member has no creation identity"
                )
            })?;
            if identity.created_at() <= previous {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "stored enum member creation identities are not increasing"
                ));
            }
            previous = identity.created_at();
        }
        let reusable = if let Some(image) = current {
            if image == self.replay_image()
                || self
                    .slots
                    .get(image as usize)
                    .is_none_or(|slot| !slot.owned)
            {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "enum replay target has no catalog owner"
                ));
            }
            self.slots[image as usize].readers == 0
        } else {
            false
        };
        let target = if reusable {
            current.expect("reusable catalog image")
        } else {
            self.reserve()?
        };
        let start = self.replay_image() as usize * self.width;
        let destination = target as usize * self.width;
        self.members
            .as_mut_slice()
            .copy_within(start..start + count as usize, destination);
        if !reusable && let Some(previous) = current {
            self.release(previous);
        }
        Ok((target, count))
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.members.len()
    }
}

/// An unpublished image releases its reservation on every error path.
pub(super) struct EnumImageReservation {
    images: Arc<Mutex<EnumMemberImages>>,
    image: Option<u32>,
    count: u32,
}

impl EnumImageReservation {
    pub(super) fn write(
        images: &Arc<Mutex<EnumMemberImages>>,
        members: &[EnumMember],
        sequence: &super::CatalogSequence,
        previous: Option<(u32, usize)>,
    ) -> Result<Self, SqlError> {
        let mut state = images.lock().expect("enum member-image lock poisoned");
        let count = state.validate_count(members.len())?;
        if previous.is_some_and(|(image, count)| {
            count > state.width
                || state
                    .slots
                    .get(image as usize)
                    .is_none_or(|slot| !slot.owned && slot.readers == 0)
        }) {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "enum member source image is unavailable"
            ));
        }
        let image = state.reserve()?;
        let start = image as usize * state.width;
        for (index, member) in members.iter().enumerate() {
            let old = previous
                .filter(|(_, count)| index < *count)
                .map(|(source, _)| state.members[source as usize * state.width + index].identity);
            let identity = match old {
                Some(Some(identity))
                    if member.identity.is_none_or(|supplied| supplied == identity) =>
                {
                    Ok(identity)
                }
                None if member.identity.is_none() => super::EnumLabelIdentity::reserve(sequence),
                _ => Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "enum member identity does not match its source"
                )),
            };
            let identity = match identity {
                Ok(identity) => identity,
                Err(error) => {
                    state.release(image);
                    return Err(error);
                }
            };
            state.members[start + index] = EnumMember {
                identity: Some(identity),
                ..*member
            };
        }
        if let Err(error) =
            validate_members(&mut state.members.as_mut_slice()[start..start + members.len()])
        {
            state.release(image);
            return Err(error);
        }
        Ok(Self {
            images: Arc::clone(images),
            image: Some(image),
            count,
        })
    }

    pub(super) fn copy(
        images: &Arc<Mutex<EnumMemberImages>>,
        source: u32,
        count: usize,
    ) -> Result<Self, SqlError> {
        let mut state = images.lock().expect("enum member-image lock poisoned");
        let count = state.validate_count(count)?;
        if state
            .slots
            .get(source as usize)
            .is_none_or(|slot| !slot.owned && slot.readers == 0)
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "enum member copy has no retained source image"
            ));
        }
        let image = state.reserve()?;
        let start = source as usize * state.width;
        let target = image as usize * state.width;
        state
            .members
            .as_mut_slice()
            .copy_within(start..start + count as usize, target);
        Ok(Self {
            images: Arc::clone(images),
            image: Some(image),
            count,
        })
    }

    pub(super) fn count(&self) -> u32 {
        self.count
    }

    pub(super) fn publish(mut self) -> u32 {
        self.image.take().expect("reserved enum member image")
    }
}

impl Drop for EnumImageReservation {
    fn drop(&mut self) {
        if let Some(image) = self.image {
            self.images
                .lock()
                .expect("enum member-image lock poisoned")
                .release(image);
        }
    }
}

#[derive(Debug)]
pub(super) struct EnumImageRead {
    images: Arc<Mutex<EnumMemberImages>>,
    image: Option<u32>,
}

impl EnumImageRead {
    pub(super) fn selects(&self, image: Option<u32>) -> bool {
        self.image == image
    }

    pub(super) fn capture(images: &Arc<Mutex<EnumMemberImages>>, image: Option<u32>) -> Self {
        if let Some(image) = image {
            let mut state = images.lock().expect("enum member-image lock poisoned");
            let slot = &mut state.slots[image as usize];
            assert!(
                slot.owned || slot.readers != 0,
                "selected enum member image is retained"
            );
            slot.readers = slot
                .readers
                .checked_add(1)
                .expect("enum image owner count overflow");
        }
        Self {
            images: Arc::clone(images),
            image,
        }
    }

    pub(super) fn member(&self, index: usize) -> EnumMember {
        let state = self.images.lock().expect("enum member-image lock poisoned");
        let image = self.image.expect("nonempty enum image");
        state.members[image as usize * state.width + index]
    }
}

impl Clone for EnumImageRead {
    fn clone(&self) -> Self {
        Self::capture(&self.images, self.image)
    }
}

impl Drop for EnumImageRead {
    fn drop(&mut self) {
        if let Some(image) = self.image {
            let mut state = self.images.lock().expect("enum member-image lock poisoned");
            let slot = &mut state.slots[image as usize];
            slot.readers = slot
                .readers
                .checked_sub(1)
                .expect("retained enum image owner");
        }
    }
}

impl super::Storage {
    pub(crate) fn validate_restored_enum_label_identities(
        &self,
        saved_sequence: u64,
    ) -> Result<(), SqlError> {
        const WORDS: usize = 1024;
        const WIDTH: u64 = (WORDS * u64::BITS as usize) as u64;
        let catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let images = self
            .enum_member_images
            .lock()
            .expect("enum member-image lock poisoned");
        for (_, database) in self.databases_visible_to(0) {
            let last = saved_sequence.clamp(1, super::type_identities::MAX_TYPE_CREATION);
            for first in (1..=last).step_by(WIDTH as usize) {
                let mut seen = [0u64; WORDS];
                for definition in catalog.enums.iter().filter(|definition| {
                    definition.database == database.oid
                        && definition.ddl_state != super::CatalogDdlState::Absent
                }) {
                    let (image, count) = definition.pending_definition.map_or(
                        (definition.member_image, definition.member_count as usize),
                        |pending| (Some(pending.image), pending.member_count as usize),
                    );
                    let Some(image) = image else {
                        if count == 0 {
                            continue;
                        }
                        return Err(sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "stored enum member image is missing"
                        ));
                    };
                    let mut previous = definition.created_at;
                    for member in images.image_members(image, count) {
                        let creation = member
                            .identity
                            .ok_or_else(|| {
                                sql_err!(
                                    sqlstate::DATA_EXCEPTION,
                                    "stored enum member has no creation identity"
                                )
                            })?
                            .created_at();
                        if creation <= previous || creation > saved_sequence {
                            return Err(sql_err!(
                                sqlstate::DATA_EXCEPTION,
                                "stored enum member creation identity is invalid"
                            ));
                        }
                        previous = creation;
                        if !(first..first + WIDTH).contains(&creation) {
                            continue;
                        }
                        let offset = (creation - first) as usize;
                        let bit = 1u64 << (offset % u64::BITS as usize);
                        let word = &mut seen[offset / u64::BITS as usize];
                        if *word & bit != 0 {
                            return Err(sql_err!(
                                sqlstate::DATA_EXCEPTION,
                                "stored enum member creation identity is duplicated"
                            ));
                        }
                        *word |= bit;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::SqlName;
    use crate::storage::tests::{test_budget, test_config, test_database_definition};
    use crate::storage::{DatabaseOid, EnumSpec, Storage};

    fn storage(enum_capacity: usize) -> Storage {
        let mut config = test_config();
        config.max_enums = enum_capacity;
        config.max_enum_labels_per_type = 4;
        config.max_catalog_versions_per_object = 1;
        config.max_databases = 4;
        config.max_schemas = 8;
        let mut budget = test_budget(&config);
        Storage::new(&config, &mut budget, 0).unwrap()
    }

    fn create(storage: &mut Storage, name: &str) -> usize {
        storage
            .create_enum(
                SqlName::parse("public").unwrap(),
                SqlName::parse(name).unwrap(),
                EnumSpec {
                    members: &[member("new", 1.0)],
                },
                0,
            )
            .unwrap()
            .slot()
    }

    fn alter(
        storage: &Storage,
        slot: usize,
        members: &[EnumMember],
    ) -> Result<super::super::EnumAlterUndo, SqlError> {
        storage.stage_enum_alter(
            storage.prepare_enum_edit(slot, 77).unwrap().1,
            SqlName::parse("public").unwrap(),
            SqlName::parse("state").unwrap(),
            members,
            77,
        )
    }

    fn member(label: &str, sort: f64) -> EnumMember {
        EnumMember {
            identity: None,
            label: SqlName::parse(label).unwrap(),
            legacy_label: None,
            sort,
        }
    }

    fn images(count: usize, width: usize) -> Arc<Mutex<EnumMemberImages>> {
        let mut budget = Budget::new(EnumMemberImages::budget_bytes(count, width));
        let images = EnumMemberImages::new(&mut budget, count, width).unwrap();
        assert_eq!(budget.remaining(), 0);
        images
    }

    #[test]
    fn enum_image_ownership_pool_readers_keep_labels_after_release_and_pool_removal() {
        let images = images(3, 2);
        let old = member("new", 1.0);
        let replacement = member("paid", 2.0);
        crate::mem::guard::forbid_alloc(|| {
            let image = EnumImageReservation::write(
                &images,
                &[old],
                &super::super::CatalogSequence::new(0),
                None,
            )
            .unwrap()
            .publish();
            let selected = EnumImageRead::capture(&images, Some(image));
            images.lock().unwrap().release(image);
            let new_image = EnumImageReservation::write(
                &images,
                &[replacement],
                &super::super::CatalogSequence::new(0),
                None,
            )
            .unwrap()
            .publish();
            assert_ne!(new_image, image);
            let retained = selected.clone();
            drop(selected);
            images.lock().unwrap().release(new_image);
            drop(images);
            assert_eq!(retained.member(0).label, old.label);
            assert_eq!(retained.member(0).sort, old.sort);
        });
    }

    #[test]
    fn enum_image_ownership_pool_capacity_resumes_after_the_last_reader_and_reservation_drop() {
        let images = images(3, 2);
        let label = member("old", 1.0);
        crate::mem::guard::forbid_alloc(|| {
            let first = EnumImageReservation::write(
                &images,
                &[label],
                &super::super::CatalogSequence::new(0),
                None,
            )
            .unwrap()
            .publish();
            let selected = EnumImageRead::capture(&images, Some(first));
            let retained = selected.clone();
            images.lock().unwrap().release(first);
            let second = EnumImageReservation::write(
                &images,
                &[label],
                &super::super::CatalogSequence::new(0),
                None,
            )
            .unwrap()
            .publish();
            let error = EnumImageReservation::write(
                &images,
                &[label],
                &super::super::CatalogSequence::new(0),
                None,
            )
            .err()
            .unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(
                error.message.as_str(),
                "enum member-image pool is exhausted"
            );
            drop(selected);
            assert!(
                EnumImageReservation::write(
                    &images,
                    &[label],
                    &super::super::CatalogSequence::new(0),
                    None
                )
                .is_err()
            );
            assert_eq!(retained.member(0).label, label.label);
            drop(retained);
            let reservation = EnumImageReservation::write(
                &images,
                &[label],
                &super::super::CatalogSequence::new(0),
                None,
            )
            .unwrap();
            drop(reservation);
            let third = EnumImageReservation::write(
                &images,
                &[label],
                &super::super::CatalogSequence::new(0),
                None,
            )
            .unwrap()
            .publish();
            assert_eq!(third, first);
            images.lock().unwrap().release(second);
            images.lock().unwrap().release(third);
        });
    }

    #[test]
    fn enum_image_ownership_pool_validation_and_copy_preserve_source_and_capacity() {
        let images = images(3, 2);
        let label = member("source", 1.0);
        crate::mem::guard::forbid_alloc(|| {
            let source = EnumImageReservation::write(
                &images,
                &[label],
                &super::super::CatalogSequence::new(0),
                None,
            )
            .unwrap()
            .publish();
            let invalid = EnumImageReservation::write(
                &images,
                &[label, label, label],
                &super::super::CatalogSequence::new(0),
                None,
            )
            .err()
            .unwrap();
            assert_eq!(invalid.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            let invalid = EnumImageReservation::copy(&images, u32::MAX, 1)
                .err()
                .unwrap();
            assert_eq!(invalid.sqlstate, sqlstate::INTERNAL_ERROR);
            let copy = EnumImageReservation::copy(&images, source, 1)
                .unwrap()
                .publish();
            let selected = EnumImageRead::capture(&images, Some(copy));
            images.lock().unwrap().release(source);
            images.lock().unwrap().release(copy);
            let reused = EnumImageReservation::write(
                &images,
                &[member("changed", 2.0)],
                &super::super::CatalogSequence::new(0),
                None,
            )
            .unwrap()
            .publish();
            assert_eq!(reused, source);
            assert_eq!(selected.member(0).label, label.label);
            images.lock().unwrap().release(reused);
        });
    }

    #[test]
    fn enum_label_validation_rejects_invalid_values_without_changing_member_ordinals() {
        crate::mem::guard::forbid_alloc(|| {
            for case in 0..6 {
                let mut values = [
                    EnumMember {
                        identity: super::super::EnumLabelIdentity::parse(1),
                        ..member("later", 2.0)
                    },
                    EnumMember {
                        identity: super::super::EnumLabelIdentity::parse(2),
                        ..member("earlier", 1.0)
                    },
                ];
                match case {
                    0 => values[1].label = values[0].label,
                    1 => values[1].sort = values[0].sort,
                    2 => values[1].sort = f64::NAN,
                    3 => values[1].sort = 0.1,
                    4 => values[1].identity = None,
                    5 => values[1].identity = values[0].identity,
                    _ => unreachable!(),
                }
                let before = values;
                assert_eq!(
                    validate_members(&mut values).unwrap_err().sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                for (value, saved) in values.iter().zip(before) {
                    assert_eq!(value.label, saved.label);
                    assert_eq!(value.identity, saved.identity);
                    assert_eq!(value.sort.to_bits(), saved.sort.to_bits());
                }
            }
        });
    }

    #[test]
    fn enum_label_identity_layout_and_failed_admission_preserve_reserved_images() {
        assert_eq!(size_of::<super::super::EnumLabelIdentity>(), 3);
        assert_eq!(size_of::<Option<super::super::EnumLabelIdentity>>(), 3);
        assert!(size_of::<EnumMember>() <= 144);
        let images = images(2, 2);
        let sequence = super::super::CatalogSequence::new(
            super::super::type_identities::MAX_TYPE_CREATION - 1,
        );
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..8 {
                let error = EnumImageReservation::write(
                    &images,
                    &[member("one", 1.0), member("two", 2.0)],
                    &sequence,
                    None,
                )
                .err()
                .unwrap();
                assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                let state = images.lock().unwrap();
                assert!(!state.slots[0].owned);
                assert_eq!(state.slots[0].readers, 0);
            }
            let fresh = super::super::CatalogSequence::new(10);
            let source = EnumImageReservation::write(&images, &[member("one", 1.0)], &fresh, None)
                .unwrap()
                .publish();
            let selected = EnumImageRead::capture(&images, Some(source));
            assert_eq!(selected.member(0).identity.unwrap().created_at(), 11);
            assert_eq!(selected.member(0).identity.unwrap().oid(), 360_000_011);
            drop(selected);
            images.lock().unwrap().release(source);
        });
    }

    #[test]
    fn enum_label_identity_recovery_rejects_duplicates_without_changing_catalogs() {
        let mut storage = storage(2);
        crate::mem::guard::forbid_alloc(|| {
            let first = create(&mut storage, "first");
            let second = create(&mut storage, "second");
            storage
                .validate_restored_enum_label_identities(storage.catalog_identity_sequence())
                .unwrap();
            let before_first = storage.enum_for(first, 0);
            let before_second = storage.enum_for(second, 0);
            let first_creation = before_first.members().next().unwrap().identity.unwrap();
            let second_creation = before_second.members().next().unwrap().identity.unwrap();
            drop(before_first);
            drop(before_second);
            let (first_image, second_image) = {
                let catalog = storage.type_catalog.lock().unwrap();
                (
                    catalog.enums[first].member_image.unwrap(),
                    catalog.enums[second].member_image.unwrap(),
                )
            };
            storage.restore_catalog_identity_sequence(100);
            {
                let mut images = storage.enum_member_images.lock().unwrap();
                let width = images.width;
                images.members[first_image as usize * width].identity =
                    super::super::EnumLabelIdentity::parse(100);
                images.members[second_image as usize * width].identity =
                    super::super::EnumLabelIdentity::parse(100);
            }
            assert_eq!(
                storage
                    .validate_restored_enum_label_identities(100)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(storage.catalog_identity_sequence(), 100);
            {
                let mut images = storage.enum_member_images.lock().unwrap();
                let width = images.width;
                images.members[first_image as usize * width].identity = Some(first_creation);
                images.members[second_image as usize * width].identity = Some(second_creation);
            }
            storage
                .validate_restored_enum_label_identities(100)
                .unwrap();
            assert_eq!(
                storage
                    .validate_restored_enum_label_identities(0)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(
                storage
                    .enum_for(first, 0)
                    .members()
                    .next()
                    .unwrap()
                    .identity,
                Some(first_creation)
            );
            assert_eq!(
                storage
                    .enum_for(second, 0)
                    .members()
                    .next()
                    .unwrap()
                    .identity,
                Some(second_creation)
            );
        });
    }

    #[test]
    fn enum_image_ownership_pool_replay_reuses_only_unretained_images() {
        let images = images(2, 1);
        let original = member("old", 1.0);
        let changed = EnumMember {
            identity: super::super::EnumLabelIdentity::parse(2),
            ..member("new", 2.0)
        };
        crate::mem::guard::forbid_alloc(|| {
            let source = EnumImageReservation::write(
                &images,
                &[original],
                &super::super::CatalogSequence::new(0),
                None,
            )
            .unwrap()
            .publish();
            images.lock().unwrap().replay_members_mut(1)[0] = changed;
            let retained = EnumImageRead::capture(&images, Some(source));
            let error = images
                .lock()
                .unwrap()
                .replace_from_replay(Some(source), 1)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(retained.member(0).label, original.label);
            drop(retained);
            let (image, count) = images
                .lock()
                .unwrap()
                .replace_from_replay(Some(source), 1)
                .unwrap();
            assert_eq!(image, source);
            assert_eq!(count, 1);
            let retained = EnumImageRead::capture(&images, Some(image));
            assert_eq!(retained.member(0).label, changed.label);
            images.lock().unwrap().release(image);
        });
    }

    #[test]
    fn enum_image_ownership_selected_views_and_iterators_survive_commit_and_slot_reuse() {
        let mut storage = storage(1);
        crate::mem::guard::forbid_alloc(|| {
            let slot = create(&mut storage, "state");
            let before = storage.enum_for(slot, 0);
            let selected = before.clone();
            let mut labels = before.members();
            drop(before);
            alter(&storage, slot, &[member("new", 1.0), member("paid", 2.0)]).unwrap();
            storage
                .commit_enum_alter(storage.prepare_enum_edit(slot, 77).unwrap().1)
                .unwrap();
            assert_eq!(storage.enum_for(slot, 0).members().len(), 2);
            assert_eq!(selected.members().len(), 1);
            let dropped = storage
                .stage_type_drop(crate::storage::NamedTypeKind::Enum, "public", "state", 77)
                .unwrap()
                .unwrap();
            assert_eq!(dropped.slot(), slot);
            storage.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Commit);
            let replacement = storage
                .create_enum(
                    SqlName::parse("public").unwrap(),
                    SqlName::parse("replacement").unwrap(),
                    EnumSpec {
                        members: &[member("other", 3.0)],
                    },
                    0,
                )
                .unwrap()
                .slot();
            assert_eq!(replacement, slot);
            assert_ne!(
                selected.created_at,
                storage.enum_for(replacement, 0).created_at
            );
            assert_eq!(selected.sort_of("new"), Some(1.0));
            assert_eq!(labels.next().unwrap().label.as_str(), "new");
            assert!(labels.next().is_none());
            assert_eq!(storage.enum_for(replacement, 0).sort_of("other"), Some(3.0));
        });
    }

    #[test]
    fn enum_image_ownership_capacity_and_rollback_release_selected_images() {
        let mut storage = storage(1);
        crate::mem::guard::forbid_alloc(|| {
            let slot = create(&mut storage, "state");
            let before = storage.enum_for(slot, 0);
            alter(&storage, slot, &[member("new", 1.0), member("paid", 2.0)]).unwrap();
            storage
                .commit_enum_alter(storage.prepare_enum_edit(slot, 77).unwrap().1)
                .unwrap();
            let members = [
                member("new", 1.0),
                member("paid", 2.0),
                member("shipped", 3.0),
            ];
            let error = alter(&storage, slot, &members).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(
                error.message.as_str(),
                "enum member-image pool is exhausted"
            );
            assert_eq!(storage.enum_for(slot, 0).members().len(), 2);
            drop(before);
            let prior = alter(&storage, slot, &members).unwrap();
            let pending = storage.enum_for(slot, 77);
            storage.rollback_enum_alter(prior);
            assert_eq!(pending.sort_of("shipped"), Some(3.0));
            assert!(alter(&storage, slot, &members).is_err());
            drop(pending);
            let prior = alter(&storage, slot, &members).unwrap();
            storage.rollback_enum_alter(prior);
            assert_eq!(storage.enum_for(slot, 0).members().len(), 2);
        });
    }

    #[test]
    fn enum_image_ownership_iterator_releases_at_end_and_outlives_storage() {
        let mut storage = storage(1);
        let slot = crate::mem::guard::forbid_alloc(|| create(&mut storage, "state"));
        let mut labels = crate::mem::guard::forbid_alloc(|| storage.enum_for(slot, 0).members());
        drop(storage);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(labels.next().unwrap().label.as_str(), "new");
            assert!(labels.next().is_none());
        });
    }

    #[test]
    fn shared_enum_publication_active_table_reader_and_unrelated_write_progress() {
        use crate::sql::types::{ColType, Datum};
        use crate::storage::UserTypeName;
        use crate::storage::tests::make_def;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        let mut storage = storage(1);
        let slot = create(&mut storage, "state");
        let identity = UserTypeName::declaration(
            SqlName::parse("public").unwrap(),
            SqlName::parse("state").unwrap(),
        );
        let mut definition = make_def(
            "enum_reader",
            &[("state", ColType::Enum(Some(storage.enum_for(slot, 0).creation())), false)],
        );
        definition.columns[0].user_type = Some(identity);
        let table = storage.create_table(definition).unwrap();
        let identity = storage.table_def(table, 0).columns()[0].user_type.unwrap();
        let unrelated = storage
            .create_table(make_def("unrelated", &[("id", ColType::Int4, false)]))
            .unwrap();
        let ready = AtomicBool::new(false);
        let published = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let storage = &storage;
            let ready = &ready;
            let published = &published;
            let reader = scope.spawn(move || {
                crate::mem::guard::forbid_alloc(|| {
                    let definition = storage.table_def(table, 0);
                    let selected = storage.enum_for(slot, 0);
                    ready.store(true, Ordering::Release);
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !published.load(Ordering::Acquire) && Instant::now() < deadline {
                        std::thread::yield_now();
                    }
                    assert!(
                        published.load(Ordering::Acquire),
                        "member publication waited for an active reader"
                    );
                    assert_eq!(definition.columns()[0].user_type, Some(identity));
                    assert_eq!(selected.sort_of("new"), Some(1.0));
                    assert_eq!(selected.sort_of("renamed"), None);
                    assert_eq!(storage.enum_for(slot, 0).sort_of("renamed"), Some(1.0));
                });
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ready.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::yield_now();
            }
            assert!(
                ready.load(Ordering::Acquire),
                "reader did not select its definitions"
            );
            crate::mem::guard::forbid_alloc(|| {
                alter(storage, slot, &[member("renamed", 1.0)]).unwrap();
                storage
                    .commit_enum_alter(storage.prepare_enum_edit(slot, 77).unwrap().1)
                    .unwrap();
                let location = storage.heap.append_row(&[Datum::Int4(42)]).unwrap();
                storage
                    .write_pending(unrelated, 1, 91, 1, Some(location))
                    .unwrap();
                let row = storage.resident_row_state(unrelated, 1).unwrap().unwrap();
                assert_eq!(row.pending_last().unwrap().txid, 91);
            });
            published.store(true, Ordering::Release);
            reader.join().unwrap();
        });
    }

    #[test]
    fn shared_enum_publication_identity_move_waits_for_its_transaction_owner() {
        use crate::sql::types::ColType;
        use crate::storage::UserTypeName;
        use crate::storage::tests::make_def;

        let mut storage = storage(1);
        let slot = create(&mut storage, "state");
        let before = UserTypeName::declaration(
            SqlName::parse("public").unwrap(),
            SqlName::parse("state").unwrap(),
        );
        let after = UserTypeName::declaration(before.schema, SqlName::parse("moved").unwrap());
        let mut definition = make_def(
            "enum_reference",
            &[("state", ColType::Enum(Some(storage.enum_for(slot, 0).creation())), false)],
        );
        definition.columns[0].user_type = Some(before);
        let table = storage.create_table(definition).unwrap();
        let captured = storage.table_def(table, 0).columns()[0].user_type;
        crate::mem::guard::forbid_alloc(|| {
            storage
                .stage_enum_alter(
                    storage.prepare_enum_edit(slot, 77).unwrap().1,
                    after.schema,
                    after.name,
                    &[member("new", 1.0), member("paid", 2.0)],
                    77,
                )
                .unwrap();
            let selected = storage.enum_for(slot, 77);
            storage
                .commit_enum_alter(storage.prepare_enum_edit(slot, 88).unwrap().1)
                .unwrap();
            assert_eq!(storage.enum_for(slot, 0).name, before.name);
            assert_eq!(storage.enum_for(slot, 0).members().len(), 1);
            assert_eq!(storage.enum_for(slot, 77).name, after.name);
            assert_eq!(storage.enum_for(slot, 77).members().len(), 2);
            assert_eq!(selected.sort_of("paid"), Some(2.0));
            assert_eq!(storage.table_def(table, 0).columns()[0].user_type, captured);
            assert_eq!(storage.enum_for(slot, 77).name, after.name);
        });
        crate::mem::guard::forbid_alloc(|| {
            storage
                .commit_enum_alter(storage.prepare_enum_edit(slot, 77).unwrap().1)
                .unwrap();
            assert_eq!(storage.enum_for(slot, 0).name, after.name);
            assert!(
                storage.table_def(table, 0).columns()[0]
                    .user_type
                    .unwrap()
                    .is_named(after.schema, after.name)
            );
            assert_eq!(storage.enum_for(slot, 0).members().len(), 2);
            assert_eq!(storage.table_def(table, 0).columns()[0].user_type, captured);
            assert_eq!(
                storage
                    .declared_column_type(&storage.table_def(table, 0).columns()[0], 0)
                    .unwrap()
                    .catalog_oid(),
                crate::sql::types::oid::enum_oid(storage.enum_for(slot, 0).creation())
            );
        });
    }

    #[test]
    fn shared_enum_publication_stale_undo_cannot_erase_later_edits_or_reused_types() {
        let mut storage = storage(1);
        let slot = create(&mut storage, "state");
        let old = crate::mem::guard::forbid_alloc(|| {
            let old = alter(&storage, slot, &[member("first", 1.0)]).unwrap();
            storage.rollback_enum_alter(old);
            let later = alter(&storage, slot, &[member("second", 1.0)]).unwrap();
            storage.rollback_enum_alter(old);
            assert_eq!(storage.enum_for(slot, 77).sort_of("second"), Some(1.0));
            storage.rollback_enum_alter(later);
            let other = storage
                .stage_enum_alter(
                    storage.prepare_enum_edit(slot, 88).unwrap().1,
                    SqlName::parse("public").unwrap(),
                    SqlName::parse("state").unwrap(),
                    &[member("other", 1.0)],
                    88,
                )
                .unwrap();
            storage.rollback_enum_alter(old);
            assert_eq!(storage.enum_for(slot, 88).sort_of("other"), Some(1.0));
            storage.rollback_enum_alter(other);
            old
        });
        crate::mem::guard::forbid_alloc(|| {
            let dropped = storage
                .stage_type_drop(crate::storage::NamedTypeKind::Enum, "public", "state", 77)
                .unwrap()
                .unwrap();
            assert_eq!(dropped.slot(), slot);
            storage.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Commit);
            let replacement = create(&mut storage, "state");
            assert_eq!(replacement, slot);
            let later = alter(&storage, replacement, &[member("replacement", 1.0)]).unwrap();
            storage.rollback_enum_alter(old);
            assert_eq!(
                storage.enum_for(replacement, 77).sort_of("replacement"),
                Some(1.0)
            );
            storage.rollback_enum_alter(later);
            assert_eq!(storage.enum_for(replacement, 0).sort_of("new"), Some(1.0));
        });
    }

    #[test]
    fn shared_enum_publication_generation_exhaustion_preserves_images_and_undo() {
        let mut storage = storage(1);
        let slot = create(&mut storage, "state");
        let before = storage.enum_for(slot, 0);
        let previous = storage
            .type_catalog
            .lock()
            .unwrap()
            .versions
            .replace_sequence_for_test(u64::MAX);
        crate::mem::guard::forbid_alloc(|| {
            let error = alter(&storage, slot, &[member("changed", 1.0)]).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(storage.enum_for(slot, 77).sort_of("new"), Some(1.0));
            assert_eq!(before.sort_of("new"), Some(1.0));
        });
        storage
            .type_catalog
            .lock()
            .unwrap()
            .versions
            .replace_sequence_for_test(previous);
        crate::mem::guard::forbid_alloc(|| {
            let undo = alter(&storage, slot, &[member("changed", 1.0)]).unwrap();
            storage.rollback_enum_alter(undo);
            assert_eq!(storage.enum_for(slot, 0).sort_of("new"), Some(1.0));
        });
    }

    #[test]
    fn enum_image_ownership_worker_view_keeps_catalog_writers_available() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        let mut storage = storage(1);
        let slot = create(&mut storage, "state");
        let selected = storage.enum_for(slot, 0);
        let published = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let flag = &published;
            let reader = scope.spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !flag.load(Ordering::Acquire) && Instant::now() < deadline {
                    std::thread::yield_now();
                }
                assert!(
                    flag.load(Ordering::Acquire),
                    "enum writer waited for a selected reader"
                );
                crate::mem::guard::forbid_alloc(|| {
                    assert_eq!(selected.members().len(), 1);
                    assert_eq!(selected.sort_of("new"), Some(1.0));
                    assert_eq!(selected.sort_of("paid"), None);
                });
            });
            let result = crate::mem::guard::forbid_alloc(|| {
                alter(&storage, slot, &[member("new", 1.0), member("paid", 2.0)])?;
                storage
                    .commit_enum_alter(storage.prepare_enum_edit(slot, 77).unwrap().1)
                    .unwrap();
                Ok::<(), SqlError>(())
            });
            published.store(true, Ordering::Release);
            result.unwrap();
            reader.join().unwrap();
        });
        crate::mem::guard::forbid_alloc(|| {
            let prior = alter(
                &storage,
                slot,
                &[
                    member("new", 1.0),
                    member("paid", 2.0),
                    member("shipped", 3.0),
                ],
            )
            .unwrap();
            storage.rollback_enum_alter(prior);
        });
    }

    #[test]
    fn enum_image_ownership_replay_preserves_readers_and_failed_replacement() {
        let mut storage = storage(1);
        crate::mem::guard::forbid_alloc(|| {
            let slot = create(&mut storage, "state");
            let before = storage.enum_for(slot, 0);
            let created_at = before.created_at;
            storage.enum_replay_members(2).unwrap().copy_from_slice(&[
                EnumMember {
                    identity: super::super::EnumLabelIdentity::parse(2),
                    ..member("new", 1.0)
                },
                EnumMember {
                    identity: super::super::EnumLabelIdentity::parse(3),
                    ..member("paid", 2.0)
                },
            ]);
            storage
                .finish_enum_replay(
                    SqlName::parse("public").unwrap(),
                    SqlName::parse("state").unwrap(),
                    2,
                    created_at,
                )
                .unwrap();
            assert_eq!(before.members().len(), 1);
            let current = storage.enum_for(slot, 0);
            storage.enum_replay_members(2).unwrap().copy_from_slice(&[
                EnumMember {
                    identity: super::super::EnumLabelIdentity::parse(2),
                    ..member("later", 4.0)
                },
                EnumMember {
                    identity: super::super::EnumLabelIdentity::parse(3),
                    ..member("paid", 2.0)
                },
            ]);
            let error = storage
                .finish_enum_replay(
                    SqlName::parse("public").unwrap(),
                    SqlName::parse("state").unwrap(),
                    2,
                    created_at,
                )
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(storage.enum_for(slot, 0).sort_of("paid"), Some(2.0));
            drop(before);
            storage
                .finish_enum_replay(
                    SqlName::parse("public").unwrap(),
                    SqlName::parse("state").unwrap(),
                    2,
                    created_at,
                )
                .unwrap();
            assert_eq!(current.sort_of("paid"), Some(2.0));
            assert_eq!(storage.enum_for(slot, 0).sort_of("later"), Some(4.0));
        });
    }

    #[test]
    fn enum_image_ownership_database_clone_and_retirement_keep_selected_labels() {
        let mut storage = storage(2);
        crate::mem::guard::forbid_alloc(|| {
            let source = create(&mut storage, "state");
            let source_view = storage.enum_for(source, 0);
            // Repeat beyond spare image capacity; retirement must release owners.
            for rollback in [false, true, false, true] {
                let database = storage
                    .create_database(
                        None,
                        DatabaseOid::POSTGRES,
                        crate::storage::DatabaseCloneAllocation::New,
                        test_database_definition("enum_clone"),
                        0,
                        77,
                    )
                    .unwrap();
                let oid = storage.database(database).oid;
                let cloned = if rollback {
                    let catalog = storage.type_catalog.lock().unwrap();
                    catalog
                        .enums
                        .iter()
                        .enumerate()
                        .find_map(|(slot, definition)| {
                            (definition.database == oid && definition.visible_to(77))
                                .then_some(slot)
                        })
                        .unwrap()
                } else {
                    storage.commit_database_create(database);
                    storage.select_database(oid).unwrap();
                    storage.enum_slot("public", "state", 0).unwrap()
                };
                let selected = storage.enum_for(cloned, 77);
                assert_ne!(cloned, source);
                assert_eq!(selected.database, oid);
                storage.select_database(DatabaseOid::POSTGRES).unwrap();
                if rollback {
                    storage.rollback_database_create(database);
                } else {
                    storage.drop_database(database, 77);
                    storage.commit_database_drop(database);
                }
                assert_eq!(source_view.sort_of("new"), Some(1.0));
                assert_eq!(selected.sort_of("new"), Some(1.0));
                assert_eq!(storage.enum_for(source, 0).sort_of("new"), Some(1.0));
            }
        });
    }
}
