//! Public identifiers follow durable object lifetimes, never reusable catalog slots.

use core::num::NonZeroU32;

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CatalogIdentity(NonZeroU32);

impl CatalogIdentity {
    pub(crate) fn parse(created_at: u64) -> Option<Self> {
        if created_at > MAX_CATALOG_CREATION {
            return None;
        }
        NonZeroU32::new(u32::try_from(created_at).ok()?).map(Self)
    }

    pub(crate) fn get(self) -> u32 {
        self.0.get()
    }

    pub(crate) fn created_at(self) -> u64 {
        u64::from(self.0.get())
    }

    pub(crate) fn require(created_at: u64) -> Result<Self, SqlError> {
        Self::parse(created_at).ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "stored catalog creation identity is invalid"
            )
        })
    }
}

impl ExtendedStatisticsDef {
    pub(crate) fn creation(&self) -> CatalogIdentity {
        CatalogIdentity::parse(self.created_at)
            .expect("admitted catalog object has a checked creation identity")
    }

    pub(crate) fn oid(&self) -> i32 {
        440_000_000
            + CatalogIdentity::parse(self.created_at)
                .expect("admitted Statistics has a checked creation identity")
                .0
                .get() as i32
    }
}

impl ExtensionDef {
    pub(crate) fn oid(&self) -> i32 {
        460_000_000
            + CatalogIdentity::parse(self.created_at)
                .expect("admitted Extension has a checked creation identity")
                .0
                .get() as i32
    }
}

impl<D: Copy> foreign::ForeignCatalogEntry<D> {
    pub(crate) fn creation(&self) -> CatalogIdentity {
        CatalogIdentity::parse(self.created_at)
            .expect("admitted foreign object has a checked creation identity")
    }
}

impl foreign::ForeignCatalogEntry<foreign::ForeignDataWrapperDefinition> {
    pub(crate) fn oid(&self) -> i32 {
        480_000_000
            + CatalogIdentity::parse(self.created_at)
                .expect("admitted ForeignDataWrapper has a checked creation identity")
                .0
                .get() as i32
    }
}

impl foreign::ForeignCatalogEntry<foreign::ForeignServerDefinition> {
    pub(crate) fn oid(&self) -> i32 {
        500_000_000
            + CatalogIdentity::parse(self.created_at)
                .expect("admitted ForeignServer has a checked creation identity")
                .0
                .get() as i32
    }
}

impl foreign::ForeignCatalogEntry<foreign::UserMappingDefinition> {
    pub(crate) fn oid(&self) -> i32 {
        520_000_000
            + CatalogIdentity::parse(self.created_at)
                .expect("admitted UserMapping has a checked creation identity")
                .0
                .get() as i32
    }
}

impl PublicationDef {
    pub(crate) fn creation(&self) -> CatalogIdentity {
        CatalogIdentity::parse(self.created_at)
            .expect("admitted catalog object has a checked creation identity")
    }

    pub(crate) fn oid(&self) -> i32 {
        540_000_000
            + CatalogIdentity::parse(self.created_at)
                .expect("admitted Publication has a checked creation identity")
                .0
                .get() as i32
    }
}

impl SubscriptionDef {
    pub(crate) fn oid(&self) -> i32 {
        560_000_000
            + CatalogIdentity::parse(self.created_at)
                .expect("admitted Subscription has a checked creation identity")
                .0
                .get() as i32
    }
}

/// New SQL declarations reserve a birth; recovery restores its checked durable birth.
#[derive(Clone, Copy)]
pub(crate) enum CatalogCreation {
    New,
    Restore(CatalogIdentity),
}

impl CatalogCreation {
    pub(super) fn resolve(self, sequence: &CatalogSequence) -> Result<u64, SqlError> {
        match self {
            Self::New => sequence.next_bounded(MAX_CATALOG_CREATION, "catalog object"),
            Self::Restore(identity) => Ok(identity.created_at()),
        }
    }
}

impl Storage {
    pub(crate) fn validate_restored_catalog_identities(
        &self,
        saved_sequence: u64,
    ) -> Result<(), SqlError> {
        const WORDS: usize = 1024;
        const WIDTH: usize = WORDS * u64::BITS as usize;
        let statistics = self
            .extended_statistics_catalog
            .lock()
            .expect("statistics catalog lock poisoned");
        let extensions = self.extension_catalog();
        let foreign = self.foreign_catalog();
        let publications = self
            .publications
            .lock()
            .expect("publication catalog lock poisoned");
        let subscriptions = self
            .subscription_catalog
            .lock()
            .expect("subscription catalog lock poisoned");
        let text = self
            .text_catalog
            .lock()
            .expect("text catalog lock poisoned");
        let casts = self.casts.lock().expect("cast catalog lock poisoned");
        let operators = self
            .operator_catalog
            .lock()
            .expect("operator catalog lock poisoned");
        let events = self
            .event_triggers
            .lock()
            .expect("event trigger catalog lock poisoned");
        // Subscription OIDs belong to a shared catalog. Other families are
        // database-local and template copies may preserve their numeric OIDs.
        let scopes = core::iter::once(None).chain(
            self.databases_visible_to(0)
                .map(|(_, database)| Some(database.oid)),
        );
        for database in scopes {
            for first in (1..=saved_sequence.clamp(1, MAX_CATALOG_CREATION)).step_by(WIDTH) {
                let mut seen = [0u64; WORDS];
                let mut check = |database_oid: DatabaseOid,
                                 state: CatalogDdlState,
                                 created_at: u64|
                 -> Result<(), SqlError> {
                    if database.is_some_and(|selected| database_oid != selected)
                        || state == CatalogDdlState::Absent
                    {
                        return Ok(());
                    }
                    CatalogIdentity::require(created_at)?;
                    if created_at > saved_sequence {
                        return Err(sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "stored catalog identity exceeds its reserved counter"
                        ));
                    }
                    if !(first..first + WIDTH as u64).contains(&created_at) {
                        return Ok(());
                    }
                    let offset = (created_at - first) as usize;
                    let bit = 1u64 << (offset % u64::BITS as usize);
                    let word = &mut seen[offset / u64::BITS as usize];
                    if *word & bit != 0 {
                        return Err(sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "stored catalog creation identity is duplicated"
                        ));
                    }
                    *word |= bit;
                    Ok(())
                };
                if database.is_some() {
                    for definition in statistics.definitions.iter() {
                        check(
                            definition.database,
                            definition.ddl_state,
                            definition.created_at,
                        )?;
                    }
                    for definition in extensions.definitions.iter() {
                        check(
                            definition.database,
                            definition.ddl_state,
                            definition.created_at,
                        )?;
                    }
                    for slot in 0..foreign.wrapper_capacity() {
                        let entry = foreign.entry_wrapper(slot);
                        check(entry.database, entry.ddl_state, entry.created_at)?;
                    }
                    for slot in 0..foreign.server_capacity() {
                        let entry = foreign.entry_server(slot);
                        check(entry.database, entry.ddl_state, entry.created_at)?;
                    }
                    for slot in 0..foreign.mapping_capacity() {
                        let entry = foreign.entry_mapping(slot);
                        check(entry.database, entry.ddl_state, entry.created_at)?;
                    }
                    for definition in text.collations.iter() {
                        check(
                            definition.database,
                            definition.ddl_state,
                            definition.created_at,
                        )?;
                    }
                    for definition in text.conversions.iter() {
                        check(
                            definition.database,
                            definition.ddl_state,
                            definition.created_at,
                        )?;
                    }
                    let mut builtin_seen = 0u8;
                    for definition in text.text_search_objects.iter().filter(|root| {
                        root.ddl_state != CatalogDdlState::Absent && Some(root.database) == database
                    }) {
                        match TextSearchIdentity::admit(
                            definition.created_at,
                            definition.definition,
                        )? {
                            TextSearchIdentity::Created(created) => check(
                                definition.database,
                                definition.ddl_state,
                                created.created_at(),
                            )?,
                            TextSearchIdentity::Builtin { ordinal, .. } => {
                                let bit = 1 << ordinal;
                                if builtin_seen & bit != 0 {
                                    return Err(sql_err!(
                                        sqlstate::DATA_EXCEPTION,
                                        "stored builtin text search identity is duplicated"
                                    ));
                                }
                                builtin_seen |= bit;
                            }
                        }
                    }
                    for definition in casts.iter() {
                        check(
                            definition.database,
                            definition.ddl_state,
                            definition.created_at,
                        )?;
                    }
                    for definition in operators.access_methods.iter() {
                        check(
                            definition.database,
                            definition.ddl_state,
                            definition.created_at,
                        )?;
                    }
                    for definition in events.iter() {
                        check(
                            definition.database,
                            definition.ddl_state,
                            definition.created_at,
                        )?;
                    }
                    for definition in operators.operators.iter() {
                        check(
                            definition.database,
                            definition.ddl_state,
                            definition.created_at,
                        )?;
                    }
                    for definition in operators.families.iter() {
                        check(
                            definition.database,
                            definition.ddl_state,
                            definition.created_at,
                        )?;
                    }
                    for definition in operators.classes.iter() {
                        check(
                            definition.database,
                            definition.ddl_state,
                            definition.created_at,
                        )?;
                    }
                    for definition in publications.iter() {
                        check(
                            definition.database,
                            definition.ddl_state,
                            definition.created_at,
                        )?;
                    }
                }
                for definition in subscriptions.definitions.iter() {
                    check(
                        definition.database,
                        definition.ddl_state,
                        definition.created_at,
                    )?;
                }
            }
        }
        drop(events);
        drop(operators);
        drop(casts);
        drop(text);
        drop(statistics);
        drop(extensions);
        drop(foreign);
        drop(publications);
        drop(subscriptions);
        for table in self.tables.iter().take(self.table_count()) {
            let state = table.definition.read();
            if state.identity.existence == CatalogDdlState::Absent {
                continue;
            }
            self.pending_table_defs
                .try_for_each_locked(&state, |definition| {
                    self.validate_saved_collations(
                        state.identity.database,
                        definition.columns().iter().map(|column| column.collation),
                    )?;
                    if let TableAccessMethod::Catalog(selected) = definition.access_method {
                        let catalog = self
                            .operator_catalog
                            .lock()
                            .expect("operator catalog lock poisoned");
                        if !catalog.access_methods.iter().any(|root| {
                            root.database == state.identity.database
                                && root.ddl_state != CatalogDdlState::Absent
                                && root.oid() == selected
                        }) {
                            return Err(sql_err!(
                                sqlstate::DATA_EXCEPTION,
                                "stored table access method does not match a live lifetime"
                            ));
                        }
                    }
                    Ok(())
                })?;
        }
        {
            let types = self
                .type_catalog
                .lock()
                .expect("type catalog lock poisoned");
            for definition in types
                .composites
                .iter()
                .filter(|definition| definition.ddl_state != CatalogDdlState::Absent)
            {
                self.validate_saved_collations(
                    definition.database,
                    definition
                        .fields()
                        .iter()
                        .filter(|field| !field.dropped)
                        .map(|field| field.collation),
                )?;
                if let Some(pending) = definition.pending_definition.as_ref() {
                    self.validate_saved_collations(
                        definition.database,
                        pending.fields[..pending.n_fields]
                            .iter()
                            .filter(|field| !field.dropped)
                            .map(|field| field.collation),
                    )?;
                }
            }
        }
        for definition in self
            .index_catalog()
            .iter()
            .filter(|definition| definition.ddl_state != CatalogDdlState::Absent)
        {
            self.validate_saved_collations(
                definition.database,
                definition.collations[..definition.n_cols].iter().copied(),
            )?;
            let catalog = self
                .operator_catalog
                .lock()
                .expect("operator catalog lock poisoned");
            for class in definition.operator_classes[..definition.n_cols]
                .iter()
                .flatten()
            {
                if let IndexOperatorClass::Catalog(selected) = class
                    && !catalog.classes.iter().any(|root| {
                        root.database == definition.database
                            && root.ddl_state != CatalogDdlState::Absent
                            && root.oid() == selected.get()
                    })
                {
                    return Err(sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "stored index operator class does not match a live lifetime"
                    ));
                }
            }
        }
        Ok(())
    }
    fn validate_saved_collations(
        &self,
        database: DatabaseOid,
        values: impl Iterator<Item = Collation>,
    ) -> Result<(), SqlError> {
        let catalog = self
            .text_catalog
            .lock()
            .expect("text catalog lock poisoned");
        for value in values {
            if let Collation::Catalog(selected) = value
                && !catalog.collations.iter().any(|root| {
                    root.database == database
                        && root.ddl_state != CatalogDdlState::Absent
                        && root.binding() == selected
                })
            {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "stored collation reference does not match a live lifetime"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::{test_budget, test_config};

    fn storage() -> Storage {
        let mut config = test_config();
        config.max_publications = 2;
        config.max_subscriptions = 2;
        let mut budget = test_budget(&config);
        Storage::new(&config, &mut budget, 0).unwrap()
    }

    #[test]
    fn catalog_lifetime_executable_roots_require_unique_reserved_births_per_database() {
        let storage = storage();
        let prior = current_database();
        crate::sql::set_execution_database_oid(DatabaseOid::POSTGRES);
        for family in 0..6 {
            let set = |birth, state| match family {
                0 => {
                    let mut catalog = storage.casts.lock().unwrap();
                    catalog[0].database = DatabaseOid::POSTGRES;
                    catalog[0].created_at = birth;
                    catalog[0].ddl_state = state;
                }
                1 => {
                    let mut catalog = storage.event_triggers.lock().unwrap();
                    catalog[0].database = DatabaseOid::POSTGRES;
                    catalog[0].created_at = birth;
                    catalog[0].ddl_state = state;
                }
                kind => {
                    let mut catalog = storage.operator_catalog.lock().unwrap();
                    match kind {
                        2 => {
                            let root = &mut catalog.access_methods[0];
                            root.database = DatabaseOid::POSTGRES;
                            root.created_at = birth;
                            root.ddl_state = state;
                        }
                        3 => {
                            let root = &mut catalog.operators[0];
                            root.database = DatabaseOid::POSTGRES;
                            root.created_at = birth;
                            root.ddl_state = state;
                        }
                        4 => {
                            let root = &mut catalog.families[0];
                            root.database = DatabaseOid::POSTGRES;
                            root.created_at = birth;
                            root.ddl_state = state;
                        }
                        5 => {
                            let root = &mut catalog.classes[0];
                            root.database = DatabaseOid::POSTGRES;
                            root.created_at = birth;
                            root.ddl_state = state;
                        }
                        _ => unreachable!(),
                    }
                }
            };
            crate::mem::guard::forbid_alloc(|| {
                for birth in [0, MAX_CATALOG_CREATION + 1] {
                    set(birth, CatalogDdlState::Present);
                    assert_eq!(
                        storage
                            .validate_restored_catalog_identities(MAX_CATALOG_CREATION)
                            .unwrap_err()
                            .sqlstate,
                        sqlstate::DATA_EXCEPTION
                    );
                }
                set(1, CatalogDdlState::Present);
                assert_eq!(
                    storage
                        .validate_restored_catalog_identities(0)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                storage.validate_restored_catalog_identities(1).unwrap();
                {
                    let mut roots = storage.text_catalog.lock().unwrap();
                    roots.conversions[0] = ConversionDef {
                        database: DatabaseOid::POSTGRES,
                        created_at: 1,
                        ddl_state: CatalogDdlState::Present,
                        ..ConversionDef::EMPTY
                    };
                }
                assert_eq!(
                    storage
                        .validate_restored_catalog_identities(1)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                storage.text_catalog.lock().unwrap().conversions[0].database =
                    DatabaseOid::TEMPLATE1;
                storage.validate_restored_catalog_identities(1).unwrap();
                storage.text_catalog.lock().unwrap().conversions[0] = ConversionDef::EMPTY;
                set(0, CatalogDdlState::Absent);
            });
        }
        crate::sql::set_execution_database_oid(prior);
    }

    #[test]
    fn catalog_lifetime_executable_replay_rejects_invalid_births_before_mutation() {
        let mut storage = storage();
        crate::mem::guard::forbid_alloc(|| {
            for birth in [0, MAX_CATALOG_CREATION + 1, u64::MAX] {
                let mut cast = CastDef::EMPTY;
                cast.created_at = birth;
                assert_eq!(
                    storage
                        .create_cast_from_image(CatalogPosition::new(0, 1).unwrap(), cast)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                if let Ok(identity) = CatalogPosition::new(0, birth) {
                    assert_eq!(
                        storage
                            .replay_set_operator(identity, OperatorDefinition::EMPTY)
                            .unwrap_err()
                            .sqlstate,
                        sqlstate::DATA_EXCEPTION
                    );
                    assert_eq!(
                        storage
                            .replay_set_operator_family(identity, OperatorFamilyDefinition::EMPTY)
                            .unwrap_err()
                            .sqlstate,
                        sqlstate::DATA_EXCEPTION
                    );
                    assert_eq!(
                        storage
                            .replay_set_operator_class(identity, OperatorClassDefinition::EMPTY)
                            .unwrap_err()
                            .sqlstate,
                        sqlstate::DATA_EXCEPTION
                    );
                    assert_eq!(
                        storage
                            .replay_access_method(identity, AccessMethodDefinition::EMPTY)
                            .unwrap_err()
                            .sqlstate,
                        sqlstate::DATA_EXCEPTION
                    );
                } else {
                    assert_eq!(birth, 0);
                }
                assert_eq!(
                    storage
                        .replay_event_trigger(0, birth, EventTriggerDefinition::EMPTY)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                assert!(CatalogIdentity::parse(birth).is_none());
            }
            assert!(
                storage
                    .casts
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|root| root.ddl_state == CatalogDdlState::Absent)
            );
            let catalog = storage.operator_catalog.lock().unwrap();
            assert!(
                catalog
                    .access_methods
                    .iter()
                    .all(|root| root.ddl_state == CatalogDdlState::Absent)
            );
            assert!(
                catalog
                    .operators
                    .iter()
                    .all(|root| root.ddl_state == CatalogDdlState::Absent)
            );
            assert!(
                catalog
                    .families
                    .iter()
                    .all(|root| root.ddl_state == CatalogDdlState::Absent)
            );
            assert!(
                catalog
                    .classes
                    .iter()
                    .all(|root| root.ddl_state == CatalogDdlState::Absent)
            );
            assert!(
                storage
                    .event_triggers
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|root| root.ddl_state == CatalogDdlState::Absent)
            );
        });
    }

    #[test]
    fn catalog_lifetime_executable_identifiers_reject_unreserved_ranges() {
        crate::mem::guard::forbid_alloc(|| {
            for birth in [1, MAX_CATALOG_CREATION] {
                assert_eq!(
                    AccessMethodOid::parse(ACCESS_METHOD_OID_BASE + birth as i32)
                        .unwrap()
                        .get(),
                    ACCESS_METHOD_OID_BASE + birth as i32
                );
                assert_eq!(
                    OperatorClassOid::parse(OPERATOR_CLASS_OID_BASE + birth as i32)
                        .unwrap()
                        .get(),
                    OPERATOR_CLASS_OID_BASE + birth as i32
                );
                assert!(
                    StoredDependencyIdentity::OperatorOid(OPERATOR_OID_BASE + birth as i32)
                        .valid_for(DependencyClass::Operator)
                );
            }
            for offset in [0, MAX_CATALOG_CREATION as i32 + 1] {
                assert!(AccessMethodOid::parse(ACCESS_METHOD_OID_BASE + offset).is_none());
                assert!(OperatorClassOid::parse(OPERATOR_CLASS_OID_BASE + offset).is_none());
                assert!(
                    !StoredDependencyIdentity::OperatorOid(OPERATOR_OID_BASE + offset)
                        .valid_for(DependencyClass::Operator)
                );
            }
            assert!(AccessMethodOid::parse(i32::MAX).is_none());
            assert!(OperatorClassOid::parse(i32::MAX).is_none());
        });
    }

    #[test]
    fn catalog_lifetime_saved_collations_check_every_database_and_pending_definition() {
        for pending in [false, true] {
            let storage = storage();
            let prior = current_database();
            crate::sql::set_execution_database_oid(DatabaseOid::TEMPLATE1);
            let mut definition = crate::storage::tests::make_def(
                "foreign_database_text",
                &[("value", crate::sql::types::ColType::Text, false)],
            );
            let invalid = Collation::Catalog(
                crate::sql::ast::CatalogCollation::from_creation(41, false, false).unwrap(),
            );
            if !pending {
                definition.columns[0].collation = invalid;
            }
            let table = storage.create_table_in(definition, 7).unwrap();
            storage.commit_create(table);
            if pending {
                definition.columns[0].collation = invalid;
                storage
                    .pending_table_defs
                    .stage(
                        &storage.tables[table].definition,
                        PendingTableDef {
                            txid: 8,
                            revision: 1,
                            def: definition,
                            transition: [u16::MAX; MAX_COLUMNS],
                            column_mapping: [None; MAX_COLUMNS],
                            rewrites_rows: false,
                            class_tuple_private: true,
                        },
                        2,
                    )
                    .unwrap();
            }
            crate::sql::set_execution_database_oid(DatabaseOid::POSTGRES);
            crate::mem::guard::forbid_alloc(|| {
                assert_eq!(
                    storage
                        .validate_restored_catalog_identities(100)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
            });
            crate::sql::set_execution_database_oid(prior);
        }
    }

    #[test]
    fn catalog_lifetime_text_exhaustion_preserves_empty_roots() {
        let mut storage = storage();
        let name = SqlName::parse("exhausted_text").unwrap();
        let schema = SqlName::parse("public").unwrap();
        let collation = CollationDefinition {
            schema,
            name,
            owner: 10,
            provider: CollationProvider::Libc,
            collate: crate::util::StackStr::from_str("C"),
            ctype: crate::util::StackStr::from_str("C"),
            behavior: CollationBehavior::Bytewise,
            ..CollationDefinition::EMPTY
        };
        let conversion = ConversionDefinition {
            schema,
            name,
            owner: 10,
            ..ConversionDefinition::EMPTY
        };
        let template = TextSearchDefinition::Template {
            schema,
            name,
            oid: 0,
            init: 0,
            lexize: 3726,
            behavior: TextSearchDictionaryBehavior::Simple { accept: true },
        };
        storage.catalog_sequence.observe(MAX_CATALOG_CREATION);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                storage.create_collation(collation, 7).unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(
                storage
                    .create_conversion(conversion, 7)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(
                storage
                    .create_text_search_object(template, 7)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            let catalog = storage.text_catalog.lock().unwrap();
            assert!(
                catalog
                    .collations
                    .iter()
                    .all(|root| root.ddl_state == CatalogDdlState::Absent)
            );
            assert!(
                catalog
                    .conversions
                    .iter()
                    .all(|root| root.ddl_state == CatalogDdlState::Absent)
            );
            assert!(
                catalog
                    .text_search_objects
                    .iter()
                    .all(|root| root.created_at == 0)
            );
        });
    }

    #[test]
    fn catalog_lifetime_text_replay_rejects_invalid_births_before_mutation() {
        let mut storage = storage();
        let schema = SqlName::parse("public").unwrap();
        let name = SqlName::parse("bad_text_birth").unwrap();
        let collation = CollationDefinition {
            schema,
            name,
            owner: 10,
            provider: CollationProvider::Libc,
            collate: crate::util::StackStr::from_str("C"),
            ctype: crate::util::StackStr::from_str("C"),
            behavior: CollationBehavior::Bytewise,
            ..CollationDefinition::EMPTY
        };
        let conversion = ConversionDefinition {
            schema,
            name,
            owner: 10,
            ..ConversionDefinition::EMPTY
        };
        let template = TextSearchDefinition::Template {
            schema,
            name,
            oid: 660_000_001,
            init: 0,
            lexize: 3726,
            behavior: TextSearchDictionaryBehavior::Simple { accept: true },
        };
        crate::mem::guard::forbid_alloc(|| {
            for birth in [0, MAX_CATALOG_CREATION + 1, u64::MAX] {
                assert_eq!(
                    storage
                        .replay_collation(0, birth, collation)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                assert_eq!(
                    storage
                        .replay_conversion(0, birth, conversion)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                assert_eq!(
                    storage
                        .replay_text_search_object(0, birth, template)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
            }
            assert_eq!(
                storage
                    .replay_text_search_object(0, 2, template)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            let catalog = storage.text_catalog.lock().unwrap();
            assert_eq!(catalog.collations[0].ddl_state, CatalogDdlState::Absent);
            assert_eq!(catalog.conversions[0].ddl_state, CatalogDdlState::Absent);
            assert_eq!(
                catalog.text_search_objects[0].ddl_state,
                CatalogDdlState::Present
            );
            assert_eq!(catalog.text_search_objects[0].definition.oid(), 3722);
        });
    }

    #[test]
    fn catalog_lifetime_collation_codes_reject_slots_and_invalid_algorithms() {
        use crate::sql::ast::{CatalogCollation, Collation};
        crate::mem::guard::forbid_alloc(|| {
            for birth in [1, 41, MAX_CATALOG_CREATION] {
                for (database, unicode) in [(false, false), (true, false), (false, true)] {
                    let selected =
                        CatalogCollation::from_creation(birth, database, unicode).unwrap();
                    let value = Collation::Catalog(selected);
                    assert_eq!(Collation::from_code(value.code()), Some(value));
                    assert_eq!(selected.created_at(), birth);
                }
            }
            for code in [
                5,
                132,
                134,
                255,
                0x8000_0000,
                0x8000_0001,
                0x8000_0007,
                u32::MAX,
            ] {
                assert!(Collation::from_code(code).is_none(), "{code}");
            }
            assert!(CatalogCollation::from_creation(1, true, true).is_none());
            for birth in [0, MAX_CATALOG_CREATION + 1, u64::MAX] {
                assert!(CatalogCollation::from_creation(birth, false, false).is_none());
            }
        });
    }

    #[test]
    fn catalog_lifetime_corruption_is_rejected_without_allocation() {
        let storage = storage();
        storage.publications.lock().unwrap()[0] = PublicationDef {
            created_at: 7,
            ddl_state: CatalogDdlState::Present,
            ..PublicationDef::EMPTY
        };
        crate::mem::guard::forbid_alloc(|| {
            storage.validate_restored_catalog_identities(7).unwrap();
            assert_eq!(
                storage
                    .validate_restored_catalog_identities(6)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            for created_at in [0, MAX_CATALOG_CREATION + 1, u64::MAX] {
                storage.publications.lock().unwrap()[0].created_at = created_at;
                assert_eq!(
                    storage
                        .validate_restored_catalog_identities(u64::MAX)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
            }
            storage.publications.lock().unwrap()[0].created_at = 7;
            storage.subscription_catalog.lock().unwrap().definitions[0] = SubscriptionDef {
                created_at: 7,
                ddl_state: CatalogDdlState::PendingCreate { txid: 9 },
                ..SubscriptionDef::EMPTY
            };
            assert_eq!(
                storage
                    .validate_restored_catalog_identities(7)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            storage.subscription_catalog.lock().unwrap().definitions[0].created_at = 8;
            storage.validate_restored_catalog_identities(8).unwrap();
        });
    }

    #[test]
    fn catalog_lifetime_database_scopes_preserve_clones_and_shared_identifiers() {
        let mut storage = storage();
        for (slot, database) in [DatabaseOid::POSTGRES, DatabaseOid::TEMPLATE1]
            .into_iter()
            .enumerate()
        {
            storage.publications.lock().unwrap()[slot] = PublicationDef {
                database,
                created_at: 7,
                ddl_state: CatalogDdlState::Present,
                ..PublicationDef::EMPTY
            };
        }
        crate::mem::guard::forbid_alloc(|| storage.validate_restored_catalog_identities(7))
            .unwrap();
        for (slot, database) in [DatabaseOid::POSTGRES, DatabaseOid::TEMPLATE1]
            .into_iter()
            .enumerate()
        {
            storage.subscription_catalog.lock().unwrap().definitions[slot] = SubscriptionDef {
                database,
                created_at: 8,
                definition_generation: 1,
                ddl_state: CatalogDdlState::Present,
                ..SubscriptionDef::EMPTY
            };
        }
        let sequence = storage.catalog_identity_sequence();
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                storage
                    .validate_restored_catalog_identities(8)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(
                storage
                    .restore_subscription_stream_identity(1, 8, 2)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(storage.catalog_identity_sequence(), sequence);
            assert_eq!(
                storage.subscription_catalog.lock().unwrap().definitions[1].definition_generation,
                1
            );
            storage
                .restore_subscription_stream_identity(1, 9, 2)
                .unwrap();
            storage.validate_restored_catalog_identities(9).unwrap();
        });
    }

    #[test]
    fn catalog_lifetime_foreign_replay_rejects_invalid_births_atomically() {
        let storage = storage();
        let definition = foreign::ForeignDataWrapperDefinition {
            name: SqlName::parse("original").unwrap(),
            handler: foreign::ForeignDataHandler::None,
            validator: foreign::ForeignDataValidator::None,
            options: foreign::ForeignOptions::EMPTY,
        };
        storage
            .restore_foreign_wrapper(0, 7, definition, 0)
            .unwrap();
        let oid = storage.foreign_wrapper_entry(0).oid();
        let sequence = storage.catalog_identity_sequence();
        crate::mem::guard::forbid_alloc(|| {
            for created_at in [0, MAX_CATALOG_CREATION + 1, u64::MAX] {
                assert_eq!(
                    storage
                        .replay_set_foreign_wrapper(0, created_at, 0, Some(definition))
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                assert_eq!(storage.catalog_identity_sequence(), sequence);
                assert_eq!(storage.foreign_wrapper_entry(0).oid(), oid);
                assert_eq!(
                    storage.foreign_wrapper_entry(0).definition.name.as_str(),
                    "original"
                );
            }
            let duplicate = foreign::ForeignDataWrapperDefinition {
                name: SqlName::parse("duplicate").unwrap(),
                ..definition
            };
            assert_eq!(
                storage
                    .restore_foreign_wrapper(1, 7, duplicate, 0)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(
                storage.foreign_wrapper_entry(1).ddl_state,
                CatalogDdlState::Absent
            );
            assert_eq!(storage.catalog_identity_sequence(), sequence);
        });
    }

    #[test]
    fn catalog_lifetime_exhaustion_installs_no_replacement() {
        let storage = storage();
        storage.restore_catalog_identity_sequence(MAX_CATALOG_CREATION - 1);
        let original = foreign::ForeignDataWrapperDefinition {
            name: SqlName::parse("last_wrapper").unwrap(),
            handler: foreign::ForeignDataHandler::None,
            validator: foreign::ForeignDataValidator::None,
            options: foreign::ForeignOptions::EMPTY,
        };
        crate::mem::guard::forbid_alloc(|| {
            let slot = storage.create_foreign_wrapper(original, 9).unwrap();
            let last = storage.foreign_wrapper_entry(slot).oid();
            let replacement = foreign::ForeignDataWrapperDefinition {
                name: SqlName::parse("overflow").unwrap(),
                ..original
            };
            assert_eq!(
                storage
                    .create_foreign_wrapper(replacement, 9)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(storage.catalog_identity_sequence(), MAX_CATALOG_CREATION);
            assert_eq!(storage.foreign_wrapper_entry(slot).oid(), last);
            assert!(storage.foreign_wrapper("overflow", 9).is_none());
        });
    }
}
