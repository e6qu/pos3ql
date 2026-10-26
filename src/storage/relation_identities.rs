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
    pub(crate) fn implicit_index_identities(
        &self,
    ) -> impl Iterator<Item = Option<RelationIdentity>> + '_ {
        self.columns()
            .iter()
            .filter(|column| column.primary || column.unique)
            .map(|column| column.index_identity)
            .chain(self.uniques().iter().map(|key| key.index_identity))
            .chain(self.exclusions().iter().map(|key| key.index_identity))
    }

    pub(crate) fn implicit_index_identities_mut(
        &mut self,
    ) -> impl Iterator<Item = &mut Option<RelationIdentity>> {
        self.columns[..self.n_columns]
            .iter_mut()
            .filter(|column| column.primary || column.unique)
            .map(|column| &mut column.index_identity)
            .chain(
                self.uniques[..self.n_uniques]
                    .iter_mut()
                    .map(|key| &mut key.index_identity),
            )
            .chain(
                self.exclusions[..self.n_exclusions]
                    .iter_mut()
                    .map(|key| &mut key.index_identity),
            )
    }

    pub(crate) fn implicit_index_oid(&self, position: usize) -> i32 {
        self.implicit_index_identities()
            .nth(position)
            .flatten()
            .expect("admitted implicit index has a durable identity")
            .oid()
    }
}

impl Storage {
    pub(crate) fn reserve_implicit_index_identities(
        &self,
        definition: &mut TableDef,
    ) -> Result<(), SqlError> {
        for column in &mut definition.columns[..definition.n_columns] {
            if !column.primary && !column.unique {
                column.index_identity = None;
            }
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
