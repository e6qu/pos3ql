//! Cold prepared catalogs retain private overlays and their original identities.

use super::*;
use crate::sql::txn::DdlUndo;

// Every restored executable owns its original slot and creation identity. New
// values stay private; replacements retain the committed payload for rollback.
macro_rules! restore_definition {
    ($storage:expr, $entries:expr, $slot:expr, $created:expr, $definition:expr,
     $transaction:expr, $entry:ident, $pending:ident, $create:ident, $alter:ident,
     $same_name:expr) => {{
        let entries = $entries;
        let (slot, created_at, definition, transaction) =
            ($slot, $created, $definition, $transaction);
        CatalogIdentity::require(created_at)?;
        let selected = entries.get(slot).cloned().ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared catalog position is out of range"
            )
        })?;
        let occupied = selected.ddl_state != CatalogDdlState::Absent;
        if occupied
            && (selected.database != current_database()
                || selected.created_at != created_at
                || !selected.visible_to(transaction)
                || selected
                    .ddl_state
                    .pending_txid()
                    .is_some_and(|owner| owner != transaction)
                || selected
                    .pending
                    .is_some_and(|pending| pending.txid != transaction))
            || entries.iter().enumerate().any(|(other, candidate)| {
                other != slot
                    && candidate.database == current_database()
                    && candidate.ddl_state != CatalogDdlState::Absent
                    && (candidate.created_at == created_at
                        || ($same_name)(candidate.definition_for(transaction), definition))
            })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared catalog selects another lifetime"
            ));
        }
        let undo = if occupied {
            entries[slot].pending = Some($pending {
                txid: transaction,
                definition,
            });
            DdlUndo::$alter {
                slot: slot as u32,
                prior: selected.pending,
            }
        } else {
            entries[slot] = $entry {
                database: current_database(),
                created_at,
                definition,
                pending: None,
                ddl_state: CatalogDdlState::PendingCreate { txid: transaction },
            };
            DdlUndo::$create(slot as u32)
        };
        $storage.catalog_sequence.observe(created_at);
        undo
    }};
}

impl Storage {
    pub(crate) fn stage_default_acl_image(
        &self,
        slot: usize,
        expected: Option<DefaultAclGrantImage>,
        image: Option<DefaultAclGrantImage>,
        transaction: u32,
    ) -> Result<Option<PendingDefaultAcl>, SqlError> {
        assert_ne!(transaction, 0);
        if expected == image {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared default privilege change has no effect"
            ));
        }
        for value in [expected, image].into_iter().flatten() {
            value.validate()?;
        }
        let selected = image
            .or(expected)
            .expect("different default privilege images are not both empty");
        let owner = self
            .role_slot_by_oid(selected.identity.owner.get(), transaction)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared default privilege owner is unavailable"
                )
            })? as u16;
        let grantee = selected
            .grantee
            .map(|role| {
                self.role_slot_by_oid(role.get(), transaction)
                    .map(|slot| slot as u16)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "prepared default privilege grantee is unavailable"
                        )
                    })
            })
            .transpose()?
            .unwrap_or(PUBLIC_ROLE);
        let schema = selected
            .identity
            .schema
            .map(|identity| {
                (0..self.schema_count())
                    .find(|slot| {
                        let schema = self.schema_def_for(*slot, transaction);
                        schema.database == current_database()
                            && schema.ddl_state.visible_to(transaction)
                            && schema.identity() == Some(identity)
                    })
                    .map(|slot| slot as u16)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "prepared default privilege schema is unavailable"
                        )
                    })
            })
            .transpose()?
            .unwrap_or(DEFAULT_ACL_ALL_SCHEMAS);
        let mut catalog = self.acl_catalog.lock().expect("ACL catalog lock poisoned");
        if slot >= catalog.defaults.capacity() {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared default privilege position is out of range"
            ));
        }
        let old = catalog
            .defaults
            .get(slot)
            .copied()
            .unwrap_or(DefaultAclEntry::EMPTY);
        if old
            .pending
            .is_some_and(|pending| pending.txid != transaction)
            || old.image_to(transaction) != expected
            || (expected.is_some() && old.database != current_database())
            || ((old.committed.is_some() || old.pending.is_some())
                && (old.owner != owner
                    || old.grantee != grantee
                    || old.schema != schema
                    || old.class != selected.identity.class))
            || expected.zip(image).is_some_and(|(old, new)| {
                old.identity.owner != new.identity.owner
                    || old.identity.schema != new.identity.schema
                    || old.identity.class != new.identity.class
                    || old.grantee != new.grantee
            })
            || catalog.defaults.iter().enumerate().any(|(other, entry)| {
                other != slot
                    && entry.database == current_database()
                    && entry.image_to(transaction).is_some()
                    && entry.owner == owner
                    && entry.grantee == grantee
                    && entry.schema == schema
                    && entry.class == selected.identity.class
            })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared default privilege selects another lifetime"
            ));
        }
        while catalog.defaults.len() <= slot {
            catalog
                .defaults
                .push(DefaultAclEntry::EMPTY)
                .expect("validated default privilege capacity");
        }
        catalog.defaults[slot] = DefaultAclEntry {
            database: current_database(),
            owner,
            grantee,
            schema,
            class: selected.identity.class,
            committed: old.committed,
            pending: Some(PendingDefaultAcl {
                txid: transaction,
                image,
            }),
        };
        self.catalog_sequence
            .observe(selected.identity.creation.created_at());
        Ok(old.pending)
    }

    pub(crate) fn restore_prepared_cast(
        &mut self,
        identity: CatalogPosition,
        mut definition: CastDef,
        transaction: u32,
    ) -> Result<DdlUndo, SqlError> {
        CatalogIdentity::require(identity.created_at())?;
        if definition.created_at != identity.created_at() {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared cast lifetime does not match its position"
            ));
        }
        definition.database = current_database();
        definition.source = self.bind_routine_result(definition.source, transaction)?;
        definition.target = self.bind_routine_result(definition.target, transaction)?;
        self.validate_cast_definition(definition, transaction)?;
        let mut owners = self.catalog_type_owners(&self.casts);
        owners.bind_definition(&mut definition, transaction)?;
        let entries = &mut owners.catalog;
        let slot = identity.slot();
        if slot >= entries.len()
            || entries[slot].ddl_state != CatalogDdlState::Absent
            || entries.iter().any(|candidate| {
                candidate.database == current_database()
                    && candidate.ddl_state != CatalogDdlState::Absent
                    && (candidate.created_at == identity.created_at()
                        || (candidate.source == definition.source
                            && candidate.target == definition.target))
            })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared cast position is unavailable"
            ));
        }
        definition.ddl_state = CatalogDdlState::PendingCreate { txid: transaction };
        entries[slot] = definition;
        self.catalog_sequence.observe(identity.created_at());
        Ok(DdlUndo::CastCreated(slot as u32))
    }

    pub(crate) fn restore_prepared_operator(
        &self,
        identity: CatalogPosition,
        definition: OperatorDefinition,
        transaction: u32,
    ) -> Result<DdlUndo, SqlError> {
        let mut definition = self.bind_catalog_definition(definition, transaction)?;
        let creation = CatalogIdentity::require(identity.created_at())?;
        self.validate_operator_definition(
            definition,
            transaction,
            Some(OPERATOR_OID_BASE + creation.get() as i32),
        )?;
        let mut owners = self.catalog_type_owners(&self.operator_catalog);
        owners.bind_definition(&mut definition, transaction)?;
        Ok(restore_definition!(
            self,
            &mut owners.catalog.operators,
            identity.slot(),
            identity.created_at(),
            definition,
            transaction,
            OperatorDef,
            PendingOperatorDefinition,
            OperatorCreated,
            OperatorAltered,
            |left: OperatorDefinition, right: OperatorDefinition| left.schema == right.schema
                && left.name == right.name
                && left.signature == right.signature
        ))
    }

    pub(crate) fn restore_prepared_operator_family(
        &self,
        identity: CatalogPosition,
        definition: OperatorFamilyDefinition,
        transaction: u32,
    ) -> Result<DdlUndo, SqlError> {
        let mut definition = self.bind_catalog_definition(definition, transaction)?;
        self.validate_operator_family_definition(definition, transaction)?;
        let mut owners = self.catalog_type_owners(&self.operator_catalog);
        owners.bind_definition(&mut definition, transaction)?;
        Ok(restore_definition!(
            self,
            &mut owners.catalog.families,
            identity.slot(),
            identity.created_at(),
            definition,
            transaction,
            OperatorFamilyDef,
            PendingOperatorFamilyDefinition,
            OperatorFamilyCreated,
            OperatorFamilyAltered,
            |left: OperatorFamilyDefinition, right: OperatorFamilyDefinition| left.schema
                == right.schema
                && left.name == right.name
        ))
    }

    pub(crate) fn restore_prepared_operator_class(
        &self,
        identity: CatalogPosition,
        definition: OperatorClassDefinition,
        transaction: u32,
    ) -> Result<DdlUndo, SqlError> {
        let mut definition = self.bind_catalog_definition(definition, transaction)?;
        self.validate_operator_class_definition(definition, transaction)?;
        let mut owners = self.catalog_type_owners(&self.operator_catalog);
        owners.bind_definition(&mut definition, transaction)?;
        if definition.default
            && (crate::sql::types::BtreeOperatorClass::for_type(definition.input.ctype).is_some()
                || owners
                    .catalog
                    .classes
                    .iter()
                    .enumerate()
                    .any(|(slot, candidate)| {
                        let selected = candidate.definition_for(transaction);
                        slot != identity.slot()
                            && candidate.database == current_database()
                            && candidate.ddl_state != CatalogDdlState::Absent
                            && selected.default
                            && selected.input == definition.input
                    }))
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared default operator class is duplicated"
            ));
        }
        Ok(restore_definition!(
            self,
            &mut owners.catalog.classes,
            identity.slot(),
            identity.created_at(),
            definition,
            transaction,
            OperatorClassDef,
            PendingOperatorClassDefinition,
            OperatorClassCreated,
            OperatorClassAltered,
            |left: OperatorClassDefinition, right: OperatorClassDefinition| left.schema
                == right.schema
                && left.name == right.name
        ))
    }

    pub(crate) fn restore_prepared_collation(
        &self,
        slot: usize,
        created_at: u64,
        definition: CollationDefinition,
        transaction: u32,
    ) -> Result<DdlUndo, SqlError> {
        if self
            .find_schema_visible(definition.schema.as_str(), transaction)
            .is_none()
            || self
                .role_slot_by_oid(definition.owner, transaction)
                .is_none()
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared collation source is unavailable"
            ));
        }
        let behavior = self.executable_collation_behavior(
            definition.provider,
            definition.collate.as_str(),
            definition.ctype.as_str(),
            definition.locale.as_str(),
        )?;
        if behavior != definition.behavior {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared collation behavior does not match its definition"
            ));
        }
        let mut catalog = self
            .text_catalog
            .lock()
            .expect("text catalog lock poisoned");
        Ok(restore_definition!(
            self,
            &mut catalog.collations,
            slot,
            created_at,
            definition,
            transaction,
            CollationDef,
            PendingCollationDefinition,
            CollationCreated,
            CollationAltered,
            |left: CollationDefinition, right: CollationDefinition| left.schema == right.schema
                && left.name == right.name
        ))
    }

    pub(crate) fn restore_prepared_conversion(
        &self,
        slot: usize,
        created_at: u64,
        definition: ConversionDefinition,
        transaction: u32,
    ) -> Result<DdlUndo, SqlError> {
        if self
            .find_schema_visible(definition.schema.as_str(), transaction)
            .is_none()
            || self
                .role_slot_by_oid(definition.owner, transaction)
                .is_none()
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared conversion source is unavailable"
            ));
        }
        let mut catalog = self
            .text_catalog
            .lock()
            .expect("text catalog lock poisoned");
        if definition.default
            && catalog
                .conversions
                .iter()
                .enumerate()
                .any(|(other, entry)| {
                    let candidate = entry.definition_for(transaction);
                    other != slot
                        && entry.database == current_database()
                        && entry.ddl_state != CatalogDdlState::Absent
                        && candidate.schema == definition.schema
                        && candidate.source == definition.source
                        && candidate.destination == definition.destination
                        && candidate.default
                })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared default conversion is duplicated"
            ));
        }
        Ok(restore_definition!(
            self,
            &mut catalog.conversions,
            slot,
            created_at,
            definition,
            transaction,
            ConversionDef,
            PendingConversionDefinition,
            ConversionCreated,
            ConversionAltered,
            |left: ConversionDefinition, right: ConversionDefinition| left.schema == right.schema
                && left.name == right.name
        ))
    }

    pub(crate) fn restore_prepared_text_search(
        &self,
        slot: usize,
        created_at: u64,
        definition: TextSearchDefinition,
        transaction: u32,
    ) -> Result<DdlUndo, SqlError> {
        TextSearchIdentity::admit(created_at, definition)?;
        if definition.oid() <= 0
            || !definition.executable_shape()
            || self
                .find_schema_visible(definition.schema().as_str(), transaction)
                .is_none()
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared text-search definition is invalid"
            ));
        }
        let mut catalog = self
            .text_catalog
            .lock()
            .expect("text catalog lock poisoned");
        Ok(restore_definition!(
            self,
            &mut catalog.text_search_objects,
            slot,
            created_at,
            definition,
            transaction,
            TextSearchDef,
            PendingTextSearchDefinition,
            TextSearchCreated,
            TextSearchAltered,
            |left: TextSearchDefinition, right: TextSearchDefinition| left.kind() == right.kind()
                && left.schema() == right.schema()
                && left.name() == right.name()
        ))
    }

    pub(crate) fn restore_prepared_event_trigger(
        &self,
        slot: usize,
        created_at: u64,
        definition: EventTriggerDefinition,
        transaction: u32,
    ) -> Result<DdlUndo, SqlError> {
        self.validate_event_trigger_definition(definition, transaction)?;
        let mut catalog = self
            .event_triggers
            .lock()
            .expect("event trigger catalog lock poisoned");
        Ok(restore_definition!(
            self,
            &mut *catalog,
            slot,
            created_at,
            definition,
            transaction,
            EventTriggerDef,
            PendingEventTriggerDefinition,
            EventTriggerCreated,
            EventTriggerAltered,
            |left: EventTriggerDefinition, right: EventTriggerDefinition| left.name == right.name
        ))
    }

    pub(crate) fn restore_prepared_role(
        &self,
        slot: usize,
        oid: RoleOid,
        name: SqlName,
        attributes: RoleAttributes,
        transaction: u32,
    ) -> Result<Option<PendingRole>, SqlError> {
        assert_ne!(transaction, 0);
        let mut catalog = self
            .role_catalog
            .lock()
            .expect("role catalog lock poisoned");
        let selected = catalog.roles.get(slot).ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared role position exceeds max_roles"
            )
        })?;
        if (slot == 0) != (oid == RoleOid::BOOTSTRAP)
            || PredefinedRole::from_slot(slot).is_some()
            || PredefinedRole::from_oid(oid.get()).is_some()
            || ((selected.live || selected.pending.is_some()) && selected.lifetime != Some(oid))
            || selected
                .pending
                .is_some_and(|pending| pending.txid != transaction)
            || catalog.roles.iter().enumerate().any(|(other, role)| {
                other != slot
                    && (((role.live || role.pending.is_some()) && role.lifetime == Some(oid))
                        || role.name_blocker(name, transaction).is_some()
                        || role.visible_to(transaction) && role.name_to(transaction) == name)
            })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared role selects another lifetime"
            ));
        }
        let selected = &mut catalog.roles[slot];
        let prior = selected.pending;
        if !selected.live && prior.is_none() {
            selected.name = name;
        }
        selected.lifetime = Some(oid);
        selected.pending = Some(PendingRole {
            txid: transaction,
            exists: true,
            name,
            attributes,
        });
        self.observe_role_oid(oid);
        Ok(prior)
    }

    pub(crate) fn restore_prepared_role_membership(
        &self,
        identity: CatalogPosition,
        role: RoleOid,
        member: RoleOid,
        grantor: RoleOid,
        options: RoleMembershipOptions,
        transaction: u32,
    ) -> Result<(usize, Option<PendingRoleMembership>), SqlError> {
        Self::require_membership_identity(identity)?;
        let resolve = |oid: RoleOid| {
            self.role_slot_by_oid(oid.get(), transaction)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "prepared membership role is unavailable"
                    )
                })
        };
        let (role, member, grantor) = (resolve(role)?, resolve(member)?, resolve(grantor)?);
        let mut catalog = self
            .role_catalog
            .lock()
            .expect("role catalog lock poisoned");
        let slot = identity.slot();
        let selected = catalog.memberships.get(slot).copied().ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared membership position is out of range"
            )
        })?;
        if selected
            .pending
            .is_some_and(|pending| pending.txid != transaction)
            || ((selected.live || selected.pending.is_some())
                && (selected.creation_to(transaction) > identity.created_at()
                    || usize::from(selected.role) != role
                    || usize::from(selected.member) != member
                    || usize::from(selected.grantor) != grantor))
            || catalog
                .memberships
                .iter()
                .enumerate()
                .any(|(other, candidate)| {
                    other != slot
                        && (candidate.live || candidate.pending.is_some())
                        && (candidate.creation_to(transaction) == identity.created_at()
                            || (usize::from(candidate.role) == role
                                && usize::from(candidate.member) == member
                                && usize::from(candidate.grantor) == grantor))
                })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared membership selects another lifetime"
            ));
        }
        let prior = selected.pending;
        catalog.memberships[slot] = RoleMembership {
            created_at: selected.created_at,
            role: role as u16,
            member: member as u16,
            grantor: grantor as u16,
            options: selected.options,
            live: selected.live,
            pending: Some(PendingRoleMembership {
                txid: transaction,
                created_at: identity.created_at(),
                exists: true,
                options,
            }),
        };
        if initdb_membership(identity.created_at()).is_none() {
            self.catalog_sequence.observe(identity.created_at());
        }
        Ok((slot, prior))
    }

    pub(crate) fn restore_prepared_large_object(
        &mut self,
        oid: LargeObjectOid,
        created_at: u64,
        allocated: bool,
        transaction: u32,
    ) -> Result<usize, SqlError> {
        let identity = CatalogIdentity::require(created_at)?;
        let (slot, _) =
            self.create_large_object(CatalogCreation::Restore(identity), Some(oid), transaction)?;
        let mut catalog = self
            .large_objects
            .lock()
            .expect("large object catalog lock poisoned");
        catalog.definitions[slot].allocated = allocated;
        if allocated {
            catalog.observe_allocated_oid(oid);
        }
        Ok(slot)
    }
}
