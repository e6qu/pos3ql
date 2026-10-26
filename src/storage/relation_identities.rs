//! SQL relation identities use durable creation stamps, never reusable slots.

use core::num::NonZeroU32;

use super::{Storage, TableDef};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

pub(crate) const MAX_RELATION_GENERATION: u64 = 9_999_999;
pub(crate) const FIRST_RELATION_OID: i32 = 300_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RelationIdentity(NonZeroU32);

impl RelationIdentity {
    pub(crate) fn parse(created_at: u64) -> Option<Self> {
        if created_at > MAX_RELATION_GENERATION {
            return None;
        }
        NonZeroU32::new(u32::try_from(created_at).ok()?).map(Self)
    }

    pub(crate) fn created_at(self) -> u64 {
        u64::from(self.0.get())
    }

    pub(crate) fn oid(self) -> i32 {
        FIRST_RELATION_OID + self.0.get() as i32
    }

    pub(crate) fn companion_oid(self, first: i32) -> i32 {
        first + self.0.get() as i32
    }
}

impl TableDef {
    pub(crate) fn column_index_identity(&self, column: usize) -> Option<RelationIdentity> {
        let attribute = usize::from(self.attributes.number(column)? - 1);
        self.inline_index_identities[attribute]
    }

    pub(crate) fn take_column_index_identity(&mut self, column: usize) -> Option<RelationIdentity> {
        let attribute = usize::from(self.attributes.number(column)? - 1);
        self.inline_index_identities[attribute].take()
    }

    pub(crate) fn implicit_index_identities(&self) -> impl Iterator<Item = Option<RelationIdentity>> + '_ {
        self.inline_index_identities[..usize::from(self.attributes.count)].iter()
            .zip(&self.attributes.slots)
            .filter_map(|(identity, slot)| match *slot {
                super::column_attributes::AttributeSlot::Live(column)
                    if self.columns[usize::from(column)].primary || self.columns[usize::from(column)].unique => Some(*identity),
                _ => None,
            })
            .chain(self.uniques().iter().map(|key| key.index_identity))
            .chain(self.exclusions().iter().map(|key| key.index_identity))
    }

    pub(crate) fn implicit_index_identities_mut(&mut self) -> impl Iterator<Item = &mut Option<RelationIdentity>> {
        let columns = &self.columns;
        self.inline_index_identities[..usize::from(self.attributes.count)].iter_mut()
            .zip(&self.attributes.slots)
            .filter_map(move |(identity, slot)| match *slot {
                super::column_attributes::AttributeSlot::Live(column)
                    if columns[usize::from(column)].primary || columns[usize::from(column)].unique => Some(identity),
                _ => None,
            })
            .chain(self.uniques[..self.n_uniques].iter_mut().map(|key| &mut key.index_identity))
            .chain(self.exclusions[..self.n_exclusions].iter_mut().map(|key| &mut key.index_identity))
    }

    /// Event positions follow live columns; the durable array follows attribute numbers.
    pub(crate) fn implicit_index_oid(&self, position: usize) -> i32 {
        self.columns().iter().enumerate().filter(|(_, column)| column.primary || column.unique)
            .map(|(column, _)| self.column_index_identity(column))
            .chain(self.uniques().iter().map(|key| key.index_identity))
            .chain(self.exclusions().iter().map(|key| key.index_identity))
            .nth(position).flatten().expect("admitted implicit index has a durable identity").oid()
    }
}

impl Storage {
    pub(crate) fn reserve_implicit_index_identities(
        &self,
        definition: &mut TableDef,
    ) -> Result<(), SqlError> {
        for (identity, slot) in definition.inline_index_identities.iter_mut().zip(&definition.attributes.slots) {
            let active = match *slot {
                super::column_attributes::AttributeSlot::Live(column) => {
                    definition.columns[usize::from(column)].primary || definition.columns[usize::from(column)].unique
                }
                _ => false,
            };
            if !active { *identity = None; }
        }
        for identity in definition.implicit_index_identities_mut() {
            if identity.is_none() {
                *identity = Some(
                    RelationIdentity::parse(
                        self.catalog_sequence
                            .next_bounded(MAX_RELATION_GENERATION, "relation")?,
                    )
                    .expect("bounded creation stamp is a relation identity"),
                );
            }
        }
        Ok(())
    }

    pub(super) fn validate_implicit_index_identities(
        &self,
        definition: &TableDef,
    ) -> Result<(), SqlError> {
        for (position, identity) in definition.implicit_index_identities().enumerate() {
            let identity = identity.ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "saved implicit index has no creation identity"
                )
            })?;
            if definition
                .implicit_index_identities()
                .take(position)
                .any(|earlier| earlier == Some(identity))
            {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "saved implicit index creation identity is duplicated"
                ));
            }
        }
        for identity in definition.implicit_index_identities().flatten() {
            self.catalog_sequence.observe(identity.created_at());
        }
        Ok(())
    }
}

impl Storage {
    /// Recovery has exclusive startup access. A fixed bitmap checks one stamp
    /// range at a time, without reserving memory proportional to catalog churn.
    pub(crate) fn validate_restored_relation_identities(
        &self,
        saved_sequence: u64,
    ) -> Result<(), SqlError> {
        const WORDS: usize = 1024;
        const WIDTH: u64 = (WORDS * u64::BITS as usize) as u64;
        let indexes = self.index_catalog();
        let types = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let views = self.view_rule_catalog();
        let sequences = self.sequence_catalog();
        for (_, database) in self.databases_visible_to(0) {
            for first in (1..=saved_sequence.min(MAX_RELATION_GENERATION)).step_by(WIDTH as usize) {
                let mut seen = [0u64; WORDS];
                let mut check = |created_at: u64| -> Result<(), SqlError> {
                    if RelationIdentity::parse(created_at).is_none() || created_at > saved_sequence
                    {
                        return Err(sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "restored relation creation identity is invalid"
                        ));
                    }
                    if !(first..first + WIDTH).contains(&created_at) {
                        return Ok(());
                    }
                    let offset = (created_at - first) as usize;
                    let bit = 1u64 << (offset % u64::BITS as usize);
                    let word = &mut seen[offset / u64::BITS as usize];
                    if *word & bit != 0 {
                        return Err(sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "restored relation creation identity is duplicated"
                        ));
                    }
                    *word |= bit;
                    Ok(())
                };
                for (slot, table) in self.tables.iter().enumerate() {
                    let identity = table.identity();
                    if slot == self.large_object_page_table()
                        || identity.database != database.oid
                        || identity.existence == super::CatalogDdlState::Absent
                    {
                        continue;
                    }
                    check(identity.created_at)?;
                    let definition = table.definition();
                    for (column, _) in definition.columns().iter().enumerate().filter(|(_, column)| column.primary || column.unique) {
                        check(definition.column_index_identity(column).ok_or_else(|| sql_err!(sqlstate::DATA_EXCEPTION, "restored inline index has no identity"))?.created_at())?;
                    }
                    for key in definition.uniques() {
                        let identity = key.index_identity.ok_or_else(|| {
                            sql_err!(
                                sqlstate::DATA_EXCEPTION,
                                "restored unique index has no identity"
                            )
                        })?;
                        // USING INDEX represents one relation in both catalogs.
                        let adopted = indexes.iter().any(|index| {
                            index.database == database.oid
                                && index.ddl_state != super::CatalogDdlState::Absent
                                && index.created_at == identity.created_at()
                                && index.schema == definition.schema
                                && index.table == definition.name
                                && index.name == key.name
                                && index.unique
                                && index.n_cols == key.n_cols
                                && index.columns[..index.n_cols] == key.columns[..key.n_cols]
                                && index.expressions[..index.n_cols]
                                    .iter()
                                    .all(Option::is_none)
                        });
                        if !adopted {
                            check(identity.created_at())?;
                        }
                    }
                    for key in definition.exclusions() {
                        check(
                            key.index_identity
                                .ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::DATA_EXCEPTION,
                                        "restored exclusion index has no identity"
                                    )
                                })?
                                .created_at(),
                        )?;
                    }
                }
                for index in indexes.iter().filter(|index| {
                    index.database == database.oid
                        && index.ddl_state != super::CatalogDdlState::Absent
                }) {
                    check(index.created_at)?;
                }
                for view in views.views.iter().filter(|view| {
                    view.database == database.oid
                        && view.ddl_state != super::CatalogDdlState::Absent
                }) {
                    check(view.created_at)?;
                }
                for sequence in sequences.definitions.iter().filter(|sequence| {
                    sequence.database == database.oid
                        && sequence.ddl_state != super::CatalogDdlState::Absent
                }) {
                    check(sequence.created_at)?;
                }
                for composite in types.composites.iter().filter(|composite| {
                    composite.database == database.oid
                        && composite.ddl_state != super::CatalogDdlState::Absent
                }) {
                    check(composite.created_at)?;
                }
            }
        }
        Ok(())
    }
}
