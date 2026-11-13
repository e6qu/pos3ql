//! Durable identities for constraints and their foreign-key triggers.

use core::num::NonZeroU32;

use super::{CheckConstraint, DetachedPartitionBound, DomainDef, DomainSpec, ForeignKey, Storage};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

pub(crate) const MAX_CONSTRAINT_CREATION: u64 = 9_999_999;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConstraintIdentity(NonZeroU32);

/// Per-attribute stamps of a table's NOT NULL constraint and column default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AttributeIdentities {
    pub(crate) not_null: Option<ConstraintIdentity>,
    pub(crate) default: Option<ConstraintIdentity>,
}

impl AttributeIdentities {
    pub(crate) const NONE: Self = Self {
        not_null: None,
        default: None,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForeignKeyTrigger {
    CheckInsert,
    CheckUpdate,
    ActionDelete,
    ActionUpdate,
}

impl ForeignKeyTrigger {
    pub(crate) const ALL: [Self; 4] = [
        Self::ActionDelete,
        Self::ActionUpdate,
        Self::CheckInsert,
        Self::CheckUpdate,
    ];

    pub(crate) const fn ordinal(self) -> usize {
        match self {
            Self::ActionDelete => 0,
            Self::ActionUpdate => 1,
            Self::CheckInsert => 2,
            Self::CheckUpdate => 3,
        }
    }
}

impl ConstraintIdentity {
    pub(crate) const ENCODED_BYTES: usize = core::mem::size_of::<u32>();

    pub(crate) fn parse(created_at: u64) -> Option<Self> {
        if created_at > MAX_CONSTRAINT_CREATION {
            return None;
        }
        NonZeroU32::new(u32::try_from(created_at).ok()?).map(Self)
    }

    pub(crate) fn created_at(self) -> u64 {
        u64::from(self.0.get())
    }

    pub(crate) fn to_le_bytes(self) -> [u8; 4] {
        self.0.get().to_le_bytes()
    }

    pub(crate) fn from_le_bytes(bytes: [u8; 4]) -> Option<Self> {
        Self::parse(u64::from(u32::from_le_bytes(bytes)))
    }

    pub(crate) fn oid(self) -> i32 {
        super::OidBand::Constraint.oid(self.created_at())
    }

    /// The `pg_attrdef` OID of a default identified by this stamp.
    pub(crate) fn attribute_default_oid(self) -> i32 {
        super::OidBand::AttributeDefault.oid(self.created_at())
    }

    pub(crate) fn foreign_key_trigger_oid(self, trigger: ForeignKeyTrigger) -> i32 {
        super::OidBand::ForeignKeyTrigger.oid(self.created_at() * 4 + trigger.ordinal() as u64)
    }

    fn reserve(storage: &Storage) -> Result<Self, SqlError> {
        let created_at = storage
            .catalog_sequence
            .next_bounded(MAX_CONSTRAINT_CREATION, "constraint")?;
        Ok(Self::parse(created_at).expect("bounded constraint reservation"))
    }
}

impl CheckConstraint {
    pub(crate) fn oid(&self) -> i32 {
        self.identity
            .expect("admitted CHECK has a creation identity")
            .oid()
    }
}

impl ForeignKey {
    pub(crate) fn oid(&self) -> i32 {
        self.identity
            .expect("admitted foreign key has a creation identity")
            .oid()
    }

    pub(crate) fn trigger_oid(&self, trigger: ForeignKeyTrigger) -> i32 {
        self.identity
            .expect("admitted foreign key has a creation identity")
            .foreign_key_trigger_oid(trigger)
    }
}

impl DetachedPartitionBound {
    pub(crate) fn oid(&self) -> i32 {
        self.identity
            .expect("admitted detached partition CHECK has a creation identity")
            .oid()
    }
}

impl<B: super::DefinitionBacking> super::TableDefinition<B> {
    pub(crate) fn not_null_constraint_oid(&self, column: usize) -> i32 {
        let attribute = usize::from(self.attribute_number(column) - 1);
        self.attribute_identities.as_ref()[attribute]
            .not_null
            .expect("admitted NOT NULL has a creation identity")
            .oid()
    }

    pub(crate) fn column_constraint_identity(&self, column: usize) -> Option<ConstraintIdentity> {
        let attribute = usize::from(self.attributes.number(column)? - 1);
        self.inline_constraint_identities.as_ref()[attribute]
    }

    /// The default OID of the column, when it has a default or generation
    /// expression.
    pub(crate) fn attribute_default_oid(&self, column: usize) -> Option<i32> {
        let attribute = usize::from(self.attributes.number(column)? - 1);
        self.columns.as_ref()[column].default.expression()?;
        Some(
            self.attribute_identities.as_ref()[attribute]
                .default
                .expect("admitted column default has a creation identity")
                .attribute_default_oid(),
        )
    }

    /// Stamps of every constraint and column default, in durable order. A
    /// `pg_attrdef` row draws from the same sequence as its table's constraints.
    pub(crate) fn constraint_identities(
        &self,
    ) -> impl Iterator<Item = Option<ConstraintIdentity>> + Clone + '_ {
        self.checks()
            .iter()
            .map(|check| check.identity)
            .chain(self.fkeys().iter().map(|key| key.identity))
            .chain(
                self.attribute_identities
                    .as_ref()
                    .iter()
                    .zip(self.attributes.slots.as_ref())
                    .flat_map(|(identity, slot)| {
                        let (not_null, default) = match *slot {
                            super::column_attributes::AttributeSlot::Live(column) => {
                                let column = &self.columns.as_ref()[usize::from(column)];
                                (
                                    column.not_null.is_required(),
                                    column.default.expression().is_some(),
                                )
                            }
                            _ => (false, false),
                        };
                        not_null
                            .then_some(identity.not_null)
                            .into_iter()
                            .chain(default.then_some(identity.default))
                    }),
            )
            .chain(
                self.partition
                    .detached_bound
                    .iter()
                    .map(|check| check.identity),
            )
            .chain(self.uniques().iter().map(|key| key.identity))
            .chain(self.exclusions().iter().map(|key| key.identity))
            .chain(
                self.inline_constraint_identities
                    .as_ref()
                    .iter()
                    .zip(self.attributes.slots.as_ref())
                    .filter_map(|(identity, slot)| match *slot {
                        super::column_attributes::AttributeSlot::Live(column)
                            if self.columns.as_ref()[usize::from(column)].primary
                                || self.columns.as_ref()[usize::from(column)].unique =>
                        {
                            Some(*identity)
                        }
                        _ => None,
                    }),
            )
    }
}

impl super::TableDefBuilder {
    pub(crate) fn take_column_constraint_identity(
        &mut self,
        column: usize,
    ) -> Option<ConstraintIdentity> {
        let attribute = usize::from(self.attributes.number(column)? - 1);
        self.inline_constraint_identities[attribute].take()
    }

    pub(crate) fn constraint_identities_mut(
        &mut self,
    ) -> impl Iterator<Item = &mut Option<ConstraintIdentity>> {
        let columns = &self.columns;
        self.checks[..self.n_checks]
            .iter_mut()
            .map(|check| &mut check.identity)
            .chain(
                self.fkeys[..self.n_fkeys]
                    .iter_mut()
                    .map(|key| &mut key.identity),
            )
            .chain(
                self.attribute_identities
                    .iter_mut()
                    .zip(&self.attributes.slots)
                    .flat_map(move |(identity, slot)| {
                        let (not_null, default) = match *slot {
                            super::column_attributes::AttributeSlot::Live(column) => {
                                let column = &columns[usize::from(column)];
                                (
                                    column.not_null.is_required(),
                                    column.default.expression().is_some(),
                                )
                            }
                            _ => (false, false),
                        };
                        let AttributeIdentities {
                            not_null: not_null_identity,
                            default: default_identity,
                        } = identity;
                        not_null
                            .then_some(not_null_identity)
                            .into_iter()
                            .chain(default.then_some(default_identity))
                    }),
            )
            .chain(
                self.partition
                    .detached_bound
                    .iter_mut()
                    .map(|check| &mut check.identity),
            )
            .chain(
                self.uniques[..self.n_uniques]
                    .iter_mut()
                    .map(|key| &mut key.identity),
            )
            .chain(
                self.exclusions[..self.n_exclusions]
                    .iter_mut()
                    .map(|key| &mut key.identity),
            )
            .chain(
                self.inline_constraint_identities
                    .iter_mut()
                    .zip(&self.attributes.slots)
                    .filter_map(move |(identity, slot)| match *slot {
                        super::column_attributes::AttributeSlot::Live(column)
                            if columns[usize::from(column)].primary
                                || columns[usize::from(column)].unique =>
                        {
                            Some(identity)
                        }
                        _ => None,
                    }),
            )
    }
}

impl DomainDef {
    pub(crate) fn constraint_name(&self, identity: ConstraintIdentity) -> Option<super::SqlName> {
        if self.not_null && self.not_null_identity == Some(identity) {
            return self.not_null_name;
        }
        self.checks()
            .iter()
            .find(|check| check.identity == Some(identity))
            .map(|check| check.name)
    }

    pub(crate) fn not_null_constraint_oid(&self) -> i32 {
        self.not_null_identity
            .expect("admitted domain NOT NULL has a creation identity")
            .oid()
    }

    pub(crate) fn constraint_identities(
        &self,
    ) -> impl Iterator<Item = Option<ConstraintIdentity>> + Clone + '_ {
        self.checks()
            .iter()
            .map(|check| check.identity)
            .chain(self.not_null.then_some(self.not_null_identity))
    }
}

impl DomainSpec {
    pub(crate) fn constraint_identities(
        &self,
    ) -> impl Iterator<Item = Option<ConstraintIdentity>> + Clone + '_ {
        self.checks[..self.n_checks]
            .iter()
            .map(|check| check.identity)
            .chain(self.not_null.then_some(self.not_null_identity))
    }

    pub(crate) fn constraint_identities_mut(
        &mut self,
    ) -> impl Iterator<Item = &mut Option<ConstraintIdentity>> {
        self.checks[..self.n_checks]
            .iter_mut()
            .map(|check| &mut check.identity)
            .chain(self.not_null.then_some(&mut self.not_null_identity))
    }
}

pub(crate) fn validate_constraint_identities(
    identities: impl Iterator<Item = Option<ConstraintIdentity>> + Clone,
) -> Result<(), SqlError> {
    for (position, identity) in identities.clone().enumerate() {
        let identity = identity.ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "saved constraint has no creation identity"
            )
        })?;
        if identities
            .clone()
            .take(position)
            .any(|earlier| earlier == Some(identity))
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "saved constraint creation identity is duplicated"
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_constraint_lifetimes(
    identities: impl Iterator<Item = Option<ConstraintIdentity>> + Clone,
    owner_birth: u64,
) -> Result<(), SqlError> {
    validate_constraint_identities(identities.clone())?;
    if identities
        .flatten()
        .any(|identity| identity.created_at() <= owner_birth)
    {
        return Err(sql_err!(
            sqlstate::DATA_EXCEPTION,
            "saved constraint creation identity precedes its owner"
        ));
    }
    Ok(())
}

/// A key or exclusion constraint of a table being created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeclaredKey {
    /// The compact single-column key on this column.
    Column(u16),
    /// `uniques[i]`.
    Unique(u16),
    /// `exclusions[i]`.
    Exclusion(u16),
}

const MAX_DECLARED_KEYS: usize = super::MAX_COLUMNS + super::MAX_UNIQUES + super::MAX_EXCLUSIONS;

/// The order CREATE TABLE declared its NOT NULL, key and exclusion
/// constraints in, by which PostgreSQL numbers them: NOT NULL constraints
/// declared on a column in column order, then a table primary key's other
/// columns; the primary key, then the other keys and exclusion constraints
/// as written.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ConstraintDeclarationOrder {
    declared_not_null: super::ColumnSet,
    keys: [DeclaredKey; MAX_DECLARED_KEYS],
    count: usize,
}

impl ConstraintDeclarationOrder {
    pub(crate) const fn new() -> Self {
        Self {
            declared_not_null: super::ColumnSet::EMPTY,
            keys: [DeclaredKey::Column(0); MAX_DECLARED_KEYS],
            count: 0,
        }
    }

    /// Column `column` is NOT NULL by its own definition (`NOT NULL` or
    /// `PRIMARY KEY`).
    pub(crate) fn declare_not_null(&mut self, column: usize) {
        self.declared_not_null.insert(column);
    }

    /// Appends the next declared key; a repeat keeps its first position.
    pub(crate) fn declare_key(&mut self, key: DeclaredKey) {
        if !self.keys[..self.count].contains(&key) && self.count < MAX_DECLARED_KEYS {
            self.keys[self.count] = key;
            self.count += 1;
        }
    }

    fn keys(&self) -> &[DeclaredKey] {
        &self.keys[..self.count]
    }
}

/// `definition`'s key and exclusion identities, primary key first and then
/// in `declared` order, each with its constraint and index identity; keys
/// `declared` does not name follow in the definition's own order.
pub(super) fn for_each_key_in_creation_order(
    definition: &mut super::TableDefBuilder,
    declared: Option<&ConstraintDeclarationOrder>,
    mut visit: impl FnMut(
        &mut Option<ConstraintIdentity>,
        &mut Option<super::RelationIdentity>,
    ) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let attributes = usize::from(definition.attributes.count);
    let attribute_of = |definition: &super::TableDefBuilder, column: usize| {
        (0..attributes).find(|&attribute| {
            definition.attributes.slots[attribute]
                == super::column_attributes::AttributeSlot::Live(column as u16)
        })
    };
    let mut order = [DeclaredKey::Column(0); MAX_DECLARED_KEYS];
    let mut count = 0;
    let mut push = |key: DeclaredKey| {
        if !order[..count].contains(&key) && count < MAX_DECLARED_KEYS {
            order[count] = key;
            count += 1;
        }
    };
    let compact = |definition: &super::TableDefBuilder, column: usize, primary: bool| {
        let column = &definition.columns[column];
        if primary {
            column.primary
        } else {
            column.unique
        }
    };
    for attribute in 0..attributes {
        if let super::column_attributes::AttributeSlot::Live(column) =
            definition.attributes.slots[attribute]
            && compact(definition, usize::from(column), true)
        {
            push(DeclaredKey::Column(column));
        }
    }
    for (index, key) in definition.uniques().iter().enumerate() {
        if key.kind.is_primary() {
            push(DeclaredKey::Unique(index as u16));
        }
    }
    let valid = |definition: &super::TableDefBuilder, key: DeclaredKey| match key {
        DeclaredKey::Column(column) => {
            usize::from(column) < definition.n_columns
                && compact(definition, usize::from(column), false)
        }
        DeclaredKey::Unique(index) => usize::from(index) < definition.n_uniques,
        DeclaredKey::Exclusion(index) => usize::from(index) < definition.n_exclusions,
    };
    for &key in declared.map_or(&[][..], ConstraintDeclarationOrder::keys) {
        if valid(definition, key) {
            push(key);
        }
    }
    for attribute in 0..attributes {
        if let super::column_attributes::AttributeSlot::Live(column) =
            definition.attributes.slots[attribute]
            && compact(definition, usize::from(column), false)
        {
            push(DeclaredKey::Column(column));
        }
    }
    for index in 0..definition.n_uniques {
        push(DeclaredKey::Unique(index as u16));
    }
    for index in 0..definition.n_exclusions {
        push(DeclaredKey::Exclusion(index as u16));
    }
    for &key in &order[..count] {
        match key {
            DeclaredKey::Column(column) => {
                let attribute = attribute_of(definition, usize::from(column))
                    .expect("a compact key's column is live");
                visit(
                    &mut definition.inline_constraint_identities[attribute],
                    &mut definition.inline_index_identities[attribute],
                )?;
            }
            DeclaredKey::Unique(index) => {
                let key = &mut definition.uniques[usize::from(index)];
                visit(&mut key.identity, &mut key.index_identity)?;
            }
            DeclaredKey::Exclusion(index) => {
                let exclusion = &mut definition.exclusions[usize::from(index)];
                visit(&mut exclusion.identity, &mut exclusion.index_identity)?;
            }
        }
    }
    Ok(())
}

impl Storage {
    pub(super) fn reserve_constraint_identities(
        &self,
        definition: &mut super::TableDefBuilder,
        declared: Option<&ConstraintDeclarationOrder>,
    ) -> Result<(), SqlError> {
        for (identity, slot) in definition
            .attribute_identities
            .iter_mut()
            .zip(&definition.attributes.slots)
        {
            let column = match *slot {
                super::column_attributes::AttributeSlot::Live(column) => {
                    Some(&definition.columns[usize::from(column)])
                }
                _ => None,
            };
            if !column.is_some_and(|column| column.not_null.is_required()) {
                identity.not_null = None;
            }
            if !column.is_some_and(|column| column.default.expression().is_some()) {
                identity.default = None;
            }
        }
        for (identity, slot) in definition
            .inline_constraint_identities
            .iter_mut()
            .zip(&definition.attributes.slots)
        {
            let active = match *slot {
                super::column_attributes::AttributeSlot::Live(column) => {
                    let column = &definition.columns[usize::from(column)];
                    column.primary || column.unique
                }
                _ => false,
            };
            if !active {
                *identity = None;
            }
        }
        self.reserve_table_constraints_in_creation_order(definition, declared)?;
        validate_constraint_identities(definition.constraint_identities())
    }

    /// Stamps new table constraints in the order PostgreSQL's CREATE TABLE
    /// assigns their OIDs: column defaults, CHECK constraints, NOT NULL
    /// constraints (a table primary key's undeclared columns last, in key
    /// order), the primary key, the other keys and exclusion constraints as
    /// `declared`, then foreign keys.
    fn reserve_table_constraints_in_creation_order(
        &self,
        definition: &mut super::TableDefBuilder,
        declared: Option<&ConstraintDeclarationOrder>,
    ) -> Result<(), SqlError> {
        let reserve = |identity: &mut Option<ConstraintIdentity>| -> Result<(), SqlError> {
            if identity.is_none() {
                *identity = Some(ConstraintIdentity::reserve(self)?);
            }
            Ok(())
        };
        let live_column = |definition: &super::TableDefBuilder, attribute: usize| match definition
            .attributes
            .slots[attribute]
        {
            super::column_attributes::AttributeSlot::Live(column) => Some(usize::from(column)),
            _ => None,
        };
        let attributes = usize::from(definition.attributes.count);
        for attribute in 0..attributes {
            if live_column(definition, attribute)
                .is_some_and(|column| definition.columns[column].default.expression().is_some())
            {
                reserve(&mut definition.attribute_identities[attribute].default)?;
            }
        }
        for check in &mut definition.checks[..definition.n_checks] {
            reserve(&mut check.identity)?;
        }
        // The primary key's columns, in key order.
        let mut primary = [0u16; super::MAX_INDEX_COLS];
        let mut primary_count = 0;
        if let Some(key) = definition
            .uniques()
            .iter()
            .find(|key| key.kind.is_primary())
        {
            primary[..key.n_cols].copy_from_slice(key.columns());
            primary_count = key.n_cols;
        } else if let Some(column) = definition
            .columns()
            .iter()
            .position(|column| column.primary)
        {
            primary[0] = column as u16;
            primary_count = 1;
        }
        let primary = &primary[..primary_count];
        let not_null = |definition: &super::TableDefBuilder, attribute: usize| {
            live_column(definition, attribute)
                .filter(|&column| definition.columns[column].not_null.is_required())
        };
        let declared_not_null = |column: usize| {
            declared.is_some_and(|declared| declared.declared_not_null.contains(column))
        };
        for attribute in 0..attributes {
            if not_null(definition, attribute).is_some_and(|column| {
                !primary.contains(&(column as u16)) || declared_not_null(column)
            }) {
                reserve(&mut definition.attribute_identities[attribute].not_null)?;
            }
        }
        for &column in primary {
            if let Some(attribute) = (0..attributes)
                .find(|&attribute| not_null(definition, attribute) == Some(usize::from(column)))
            {
                reserve(&mut definition.attribute_identities[attribute].not_null)?;
            }
        }
        for_each_key_in_creation_order(definition, declared, |identity, _| reserve(identity))?;
        for key in &mut definition.fkeys[..definition.n_fkeys] {
            reserve(&mut key.identity)?;
        }
        // Anything the creation order above does not name keeps the
        // definition's own order.
        for identity in definition.constraint_identities_mut() {
            reserve(identity)?;
        }
        Ok(())
    }

    /// Reserves a stamp for every view column default that lacks one. A
    /// replaced view keeps the stamp of a default it carries over unchanged.
    pub(super) fn reserve_view_default_identities(
        &self,
        columns: &mut super::ViewColumns,
        prior: Option<&super::ViewColumns>,
    ) -> Result<(), SqlError> {
        if let Some(prior) = prior {
            for index in 0..columns.len() {
                if columns.default_identity(index).is_none()
                    && columns.default_at_ref(index).is_some()
                    && columns.default_at_ref(index) == prior.default_at_ref(index)
                    && let Some(identity) = prior.default_identity(index)
                {
                    *columns = columns
                        .with_default_identity(index, identity)
                        .expect("a carried default has an entry");
                }
            }
        }
        for identity in columns.default_identities_mut() {
            if identity.is_none() {
                *identity = Some(ConstraintIdentity::reserve(self)?);
            }
        }
        validate_constraint_identities(columns.default_identities())
    }

    pub(super) fn reserve_domain_constraint_identities(
        &self,
        definition: &mut DomainSpec,
    ) -> Result<(), SqlError> {
        if !definition.not_null {
            definition.not_null_identity = None;
        }
        for identity in definition.constraint_identities_mut() {
            if identity.is_none() {
                *identity = Some(ConstraintIdentity::reserve(self)?);
            }
        }
        validate_constraint_identities(definition.constraint_identities())
    }
}

const VALIDATION_WORDS: usize = 1024;
const VALIDATION_WIDTH: u64 = (VALIDATION_WORDS * u64::BITS as usize) as u64;

fn validate_window(
    identities: impl Iterator<Item = Option<ConstraintIdentity>> + Clone,
    owner_birth: u64,
    saved_sequence: u64,
    first: u64,
    seen: &[u64; VALIDATION_WORDS],
    owned: &mut [u64; VALIDATION_WORDS],
) -> Result<(), SqlError> {
    validate_constraint_identities(identities.clone())?;
    for identity in identities.flatten() {
        let created_at = identity.created_at();
        if created_at <= owner_birth || created_at > saved_sequence {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "saved constraint creation identity is outside its owner lifetime"
            ));
        }
        if !(first..first + VALIDATION_WIDTH).contains(&created_at) {
            continue;
        }
        let offset = (created_at - first) as usize;
        let word = offset / u64::BITS as usize;
        let bit = 1u64 << (offset % u64::BITS as usize);
        if seen[word] & bit != 0 {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "saved constraint identity belongs to more than one object"
            ));
        }
        owned[word] |= bit;
    }
    Ok(())
}

impl Storage {
    /// Exclusive startup access includes prepared definitions. Versions of one
    /// owner may share an identity; distinct tables or domains may not.
    pub(crate) fn validate_restored_constraint_identities(
        &mut self,
        saved_sequence: u64,
    ) -> Result<(), SqlError> {
        for (_, database) in self.databases_visible_to(0) {
            let last = saved_sequence.clamp(1, MAX_CONSTRAINT_CREATION);
            for first in (1..=last).step_by(VALIDATION_WIDTH as usize) {
                let mut seen = [0u64; VALIDATION_WORDS];
                let mut owned = [0u64; VALIDATION_WORDS];
                for (slot, table) in self.tables.iter().enumerate() {
                    if slot == self.large_object_page_table() {
                        continue;
                    }
                    let state = table.definition.read();
                    if state.identity.database != database.oid
                        || state.identity.existence == super::CatalogDdlState::Absent
                    {
                        continue;
                    }
                    owned.fill(0);
                    self.pending_table_defs
                        .try_for_each_locked(&state, |definition| {
                            validate_window(
                                definition.constraint_identities(),
                                state.identity.created_at,
                                saved_sequence,
                                first,
                                &seen,
                                &mut owned,
                            )
                        })?;
                    for (all, owner) in seen.iter_mut().zip(&owned) {
                        *all |= owner;
                    }
                }
                let views = self
                    .view_rule_catalog
                    .lock()
                    .expect("view and rule catalog lock poisoned");
                for view in views.views.iter().filter(|view| {
                    view.database == database.oid
                        && view.ddl_state != super::CatalogDdlState::Absent
                }) {
                    owned.fill(0);
                    validate_window(
                        view.columns.default_identities(),
                        view.created_at,
                        saved_sequence,
                        first,
                        &seen,
                        &mut owned,
                    )?;
                    for (all, owner) in seen.iter_mut().zip(&owned) {
                        *all |= owner;
                    }
                }
                drop(views);
                let types = self
                    .type_catalog
                    .lock()
                    .expect("type catalog lock poisoned");
                for domain in types.domains.iter().filter(|domain| {
                    domain.database == database.oid
                        && domain.ddl_state != super::CatalogDdlState::Absent
                }) {
                    owned.fill(0);
                    validate_window(
                        domain.constraint_identities(),
                        domain.created_at,
                        saved_sequence,
                        first,
                        &seen,
                        &mut owned,
                    )?;
                    if let Some(pending) = &domain.pending_definition {
                        validate_window(
                            pending.spec.constraint_identities(),
                            domain.created_at,
                            saved_sequence,
                            first,
                            &seen,
                            &mut owned,
                        )?;
                    }
                    for (all, owner) in seen.iter_mut().zip(&owned) {
                        *all |= owner;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;
    use crate::sql::types::ColType;
    use crate::storage::tests::{make_def, test_budget, test_config};
    use crate::storage::{CatalogPosition, SqlName, TableCreation};

    fn checked_table(name: &str) -> super::super::TableDefBuilder {
        let mut definition = make_def(name, &[("id", ColType::Int4, true)]);
        definition.n_checks = 1;
        definition.checks[0].name = SqlName::parse("positive").unwrap();
        definition.checks[0].expression = crate::util::StackStr::from_str("id > 0");
        definition
    }

    #[test]
    fn key_constraints_keep_independent_identities_through_index_replacement_and_restore() {
        let mut config = test_config();
        config.max_tables = 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let mut restored_budget = test_budget(&config);
        let mut restored = Storage::new(&config, &mut restored_budget, 0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut definition = make_def(
                "key_owner",
                &[("id", ColType::Int4, true), ("value", ColType::Int4, false)],
            );
            definition.columns[0].primary = true;
            definition.columns[0].unique = true;
            definition.columns[1].unique = true;
            definition.n_uniques = 1;
            definition.uniques[0].name = SqlName::parse("pair_key").unwrap();
            definition.uniques[0].columns[..2].copy_from_slice(&[0, 1]);
            definition.uniques[0].n_cols = 2;
            definition.n_exclusions = 1;
            definition.exclusions[0].name = SqlName::parse("value_exclusion").unwrap();
            definition.exclusions[0].columns[0] = 1;
            definition.exclusions[0].n_cols = 1;
            let slot = storage.create_table(definition).unwrap();
            let position = CatalogPosition::new(slot, storage.table(slot).created_at()).unwrap();
            let mut replacement =
                super::super::TableDefBuilder::from_definition(&storage.table_def(slot, 0));
            let primary = replacement.column_constraint_identity(0).unwrap();
            assert_ne!(
                primary.oid(),
                replacement.column_index_identity(0).unwrap().oid()
            );
            assert_ne!(replacement.uniques[0].identity, Some(primary));
            assert_eq!(replacement.constraint_identities().count(), 5);

            let mut named = super::super::UniqueKey::EMPTY;
            named.name = replacement.take_column_key_name(0).unwrap();
            named.kind = super::super::UniqueKeyKind::Primary;
            named.columns[0] = 0;
            named.n_cols = 1;
            named.identity = replacement.take_column_constraint_identity(0);
            named.index_identity = replacement.take_column_index_identity(0);
            replacement.columns[0].primary = false;
            replacement.columns[0].unique = false;
            replacement.uniques[1] = named;
            replacement.n_uniques = 2;
            let constraints: [Option<ConstraintIdentity>; 5] = core::array::from_fn(|position| {
                replacement.constraint_identities().nth(position).unwrap()
            });
            let indexes: [Option<super::super::RelationIdentity>; 4] =
                core::array::from_fn(|position| {
                    replacement
                        .implicit_index_identities()
                        .nth(position)
                        .unwrap()
                });
            for identity in replacement.implicit_index_identities_mut() {
                *identity = None;
            }
            storage
                .reserve_table_catalog_identities(&mut replacement)
                .unwrap();
            assert!(replacement.constraint_identities().eq(constraints));
            assert!(
                replacement
                    .implicit_index_identities()
                    .zip(indexes)
                    .all(|(new, old)| new != old)
            );
            assert_eq!(replacement.uniques[1].identity, Some(primary));
            assert_eq!(replacement.inline_constraint_identities[0], None);

            restored
                .restore_table(position, replacement.clone(), TableCreation::Committed)
                .unwrap();
            {
                let recovered = restored.table_def(slot, 0);
                assert!(
                    recovered
                        .constraint_identities()
                        .eq(replacement.constraint_identities())
                );
                assert!(
                    recovered
                        .implicit_index_identities()
                        .eq(replacement.implicit_index_identities())
                );
            }
            restored
                .validate_restored_constraint_identities(restored.catalog_identity_sequence())
                .unwrap();

            let mut invalid = replacement.clone();
            invalid.uniques[0].identity = invalid.exclusions[0].identity;
            assert_eq!(
                validate_constraint_identities(invalid.constraint_identities())
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            replacement.name = SqlName::parse("copied_key_owner").unwrap();
            let copied = storage.create_table(replacement).unwrap();
            let copied = storage.table_def(copied, 0);
            assert!(
                copied
                    .constraint_identities()
                    .flatten()
                    .all(|identity| !constraints.contains(&Some(identity)))
            );
        });
    }

    #[test]
    fn constraint_identity_restore_rejects_bad_definitions_without_catalog_or_counter_changes() {
        let mut config = test_config();
        config.max_tables = 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let slot = storage
                .create_table(checked_table("constraint_owner"))
                .unwrap();
            let original =
                crate::storage::TableDefBuilder::from_definition(&storage.table_def(slot, 0));
            let before = storage.catalog_identity_sequence();
            let position = CatalogPosition::new(1, before + 10).unwrap();
            for rejected_kind in 0..3 {
                let mut rejected = original.clone();
                rejected.name = SqlName::parse("rejected_restore").unwrap();
                match rejected_kind {
                    0 => rejected.checks[0].identity = None,
                    1 => {
                        rejected.checks[0].identity =
                            ConstraintIdentity::parse(position.created_at())
                    }
                    _ => {
                        let duplicate = ConstraintIdentity::parse(position.created_at() + 1);
                        rejected.checks[0].identity = duplicate;
                        rejected.attribute_identities[0].not_null = duplicate;
                    }
                }
                assert_eq!(
                    storage
                        .restore_table(position, rejected, TableCreation::Committed)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                assert_eq!(storage.catalog_identity_sequence(), before);
                assert_eq!(storage.find_table("public", "rejected_restore"), None);
                let current = storage.table_def(slot, 0);
                assert_eq!(current.checks[0].identity, original.checks[0].identity);
                assert_eq!(
                    current.attribute_identities[0],
                    original.attribute_identities[0]
                );
            }
            let next = storage
                .create_table(checked_table("after_rejection"))
                .unwrap();
            assert_eq!(next, 1);
            let counter = storage.catalog_identity_sequence();
            storage
                .validate_restored_constraint_identities(counter)
                .unwrap();
        });
    }

    #[test]
    fn constraint_identity_recovery_rejects_cross_owner_aliases_without_mutation() {
        let mut config = test_config();
        config.max_tables = 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let first = storage
                .create_table(checked_table("first_constraint_owner"))
                .unwrap();
            let second = storage
                .create_table(checked_table("second_constraint_owner"))
                .unwrap();
            let counter = storage.catalog_identity_sequence();
            storage
                .validate_restored_constraint_identities(counter)
                .unwrap();
            let original = storage.table_def(first, 0).checks[0].identity;
            let duplicate = storage.table_def(second, 0).checks[0].identity;
            let revision = storage.reserve_table_definition_revision().unwrap();
            let mut definition =
                crate::storage::TableDefBuilder::from_definition(&storage.table_def(first, 0));
            definition.checks[0].identity = duplicate;
            let replacement = definition.freeze(&storage.metadata_pool).unwrap();
            *storage.tables[first].definition_mut(revision) = replacement;
            let before = storage.catalog_identity_sequence();
            assert_eq!(
                storage
                    .validate_restored_constraint_identities(before)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(storage.catalog_identity_sequence(), before);
            assert_eq!(storage.table_def(first, 0).checks[0].identity, duplicate);
            definition.checks[0].identity = original;
            let replacement = definition.freeze(&storage.metadata_pool).unwrap();
            *storage.tables[first].definition_mut(revision) = replacement;
            storage
                .validate_restored_constraint_identities(before)
                .unwrap();
        });
    }
}
