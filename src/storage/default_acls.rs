//! One default-privilege catalog identity belongs to an owner, schema and
//! object class. Its individual grants share that identity through edits.

use super::*;

impl Storage {
    pub(super) fn validate_default_acl_images(&self) -> Result<(), SqlError> {
        let roles = self
            .role_catalog
            .lock()
            .expect("role catalog lock poisoned");
        let schemas = self.schemas.lock().expect("schema catalog lock poisoned");
        let catalog = self.acl_catalog.lock().expect("ACL catalog lock poisoned");
        for txid in core::iter::once(0).chain(
            catalog
                .defaults
                .iter()
                .filter_map(|entry| entry.pending.map(|pending| pending.txid)),
        ) {
            for (slot, entry) in catalog.defaults.iter().enumerate() {
                let Some(image) = entry.image_to(txid) else {
                    continue;
                };
                image.validate()?;
                let role_matches = |slot: u16, oid: RoleOid| {
                    roles
                        .roles
                        .get(usize::from(slot))
                        .is_some_and(|role| role.visible_to(txid) && role.oid() == oid)
                };
                if !role_matches(entry.owner, image.identity.owner)
                    || match image.grantee {
                        Some(oid) => !role_matches(entry.grantee, oid),
                        None => entry.grantee != PUBLIC_ROLE,
                    }
                    || entry.class != image.identity.class
                    || match image.identity.schema {
                        None => entry.schema != DEFAULT_ACL_ALL_SCHEMAS,
                        Some(identity) => {
                            schemas.get(usize::from(entry.schema)).is_none_or(|schema| {
                                schema.database != entry.database
                                    || !schema.ddl_state.visible_to(txid)
                                    || schema.identity() != Some(identity)
                            })
                        }
                    }
                {
                    return Err(sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "stored default privilege references a replaced owner, recipient or schema"
                    ));
                }
                for other in catalog.defaults[..slot]
                    .iter()
                    .filter(|other| other.database == entry.database)
                {
                    let Some(other) = other.image_to(txid) else {
                        continue;
                    };
                    let same_group = other.identity.owner == image.identity.owner
                        && other.identity.schema == image.identity.schema
                        && other.identity.class == image.identity.class;
                    if (same_group && other.identity != image.identity)
                        || (other.identity.creation == image.identity.creation
                            && other.identity != image.identity)
                        || (same_group && other.grantee == image.grantee)
                    {
                        return Err(sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "stored default privilege group has inconsistent or duplicate grants"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn snapshot_default_acl_grants<'a>(
        &self,
        txid: u32,
        arena: &'a Arena,
    ) -> Result<&'a [DefaultAclGrantImage], SqlError> {
        let catalog = self.acl_catalog.lock().expect("ACL catalog lock poisoned");
        let visible = || {
            catalog
                .defaults
                .iter()
                .filter(|entry| entry.database == current_database())
                .filter_map(|entry| entry.image_to(txid))
        };
        let Some(first) = visible().next() else {
            return Ok(&[]);
        };
        let count = visible().count();
        let images = arena.alloc_slice_with(count, |_| first).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "default privilege snapshot exceeds SQL arena"
            )
        })?;
        for (destination, image) in images.iter_mut().zip(visible()) {
            image.validate()?;
            *destination = image;
        }
        drop(catalog);
        images.sort_unstable_by_key(|image| {
            (
                image.identity.owner.get(),
                image.identity.schema.map_or(0, |schema| schema.oid()),
                image.identity.class as u8,
                image.grantee.map_or(0, |role| role.get() as u32),
            )
        });
        Ok(images)
    }

    pub(crate) const fn default_acl_baseline(
        owner: u16,
        schema: u16,
        class: DefaultPrivilegeClass,
        grantee: u16,
    ) -> (PrivilegeSet, PrivilegeSet) {
        if schema != DEFAULT_ACL_ALL_SCHEMAS {
            return (PrivilegeSet::NONE, PrivilegeSet::NONE);
        }
        if grantee == owner {
            let all = class.all_privileges();
            return (all, PrivilegeSet::NONE);
        }
        if grantee == PUBLIC_ROLE {
            return (class.default_public_privileges(), PrivilegeSet::NONE);
        }
        (PrivilegeSet::NONE, PrivilegeSet::NONE)
    }

    pub(crate) fn default_acl_state(
        &self,
        owner: u16,
        schema: u16,
        class: DefaultPrivilegeClass,
        grantee: u16,
        txid: u32,
    ) -> (bool, PrivilegeSet, PrivilegeSet) {
        let catalog = self.acl_catalog.lock().expect("ACL catalog lock poisoned");
        let image = catalog
            .defaults
            .iter()
            .find(|entry| {
                entry.database == current_database()
                    && entry.owner == owner
                    && entry.schema == schema
                    && entry.class == class
                    && entry.grantee == grantee
                    && (entry.committed.is_some() || entry.pending.is_some())
            })
            .and_then(|entry| entry.image_to(txid));
        match image {
            Some(image) => (true, image.privileges, image.grant_options),
            None => (false, PrivilegeSet::NONE, PrivilegeSet::NONE),
        }
    }

    pub(crate) fn default_acl_effective(
        &self,
        owner: u16,
        schema: u16,
        class: DefaultPrivilegeClass,
        grantee: u16,
        txid: u32,
    ) -> (PrivilegeSet, PrivilegeSet) {
        let (defined, privileges, grant_options) =
            self.default_acl_state(owner, schema, class, grantee, txid);
        if defined {
            (privileges, grant_options)
        } else {
            Self::default_acl_baseline(owner, schema, class, grantee)
        }
    }

    pub(crate) fn change_default_acl(
        &self,
        key: DefaultAclKey,
        defined: bool,
        privileges: PrivilegeSet,
        grant_options: PrivilegeSet,
        txid: u32,
    ) -> Result<Option<(usize, Option<PendingDefaultAcl>)>, SqlError> {
        assert_ne!(
            txid, 0,
            "fresh default privileges require a transaction owner"
        );
        self.role_reference_ready(usize::from(key.owner), txid)?;
        let owner = self.role(usize::from(key.owner)).oid();
        let grantee = if key.grantee == PUBLIC_ROLE {
            None
        } else {
            self.role_reference_ready(usize::from(key.grantee), txid)?;
            Some(self.role(usize::from(key.grantee)).oid())
        };
        let schema = if key.schema == DEFAULT_ACL_ALL_SCHEMAS {
            None
        } else {
            let definition = self.schema_def_for(usize::from(key.schema), txid);
            if !definition.ddl_state.visible_to(txid) {
                return Err(sql_err!(
                    sqlstate::INVALID_SCHEMA_NAME,
                    "default privilege schema is unavailable"
                ));
            }
            Some(
                definition
                    .identity()
                    .expect("admitted schema has a checked identity"),
            )
        };
        let same_group = |entry: &DefaultAclEntry| {
            entry.database == current_database()
                && entry.owner == key.owner
                && entry.schema == key.schema
                && entry.class == key.class
        };
        let mut catalog = self.acl_catalog.lock().expect("ACL catalog lock poisoned");
        if let Some(blocker) = catalog
            .defaults
            .iter()
            .filter(|entry| same_group(entry))
            .filter_map(|entry| entry.pending)
            .find(|pending| pending.txid != txid)
        {
            drop(catalog);
            self.wait_for_transaction(txid, blocker.txid)?;
            return Err(sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for concurrent default privilege DDL"
            ));
        }
        let existing = catalog.defaults.iter().position(|entry| {
            same_group(entry)
                && entry.grantee == key.grantee
                && (entry.committed.is_some() || entry.pending.is_some())
        });
        let current = existing.and_then(|slot| catalog.defaults[slot].image_to(txid));
        if current.is_some_and(|image| {
            image.identity.owner != owner
                || image.identity.schema != schema
                || image.grantee != grantee
        }) {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "default privilege edit selects a replaced owner"
            ));
        }
        if (!defined && current.is_none())
            || (defined
                && current.is_some_and(|image| {
                    image.privileges == privileges && image.grant_options == grant_options
                }))
        {
            return Ok(None);
        }
        let slot = existing
            .or_else(|| {
                catalog
                    .defaults
                    .iter()
                    .position(|entry| entry.committed.is_none() && entry.pending.is_none())
            })
            .unwrap_or(catalog.defaults.len());
        if slot == catalog.defaults.len() && slot == catalog.defaults.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many default privilege entries (limit {})",
                catalog.defaults.capacity()
            ));
        }
        let image = if defined {
            let identity = match catalog
                .defaults
                .iter()
                .filter(|entry| same_group(entry))
                .find_map(|entry| entry.image_to(txid).map(|image| image.identity))
            {
                Some(identity) => {
                    if identity.owner != owner || identity.schema != schema {
                        return Err(sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "default privilege group selects a replaced owner"
                        ));
                    }
                    identity
                }
                None => DefaultAclIdentity {
                    creation: CatalogIdentity::require(
                        self.catalog_sequence
                            .next_bounded(MAX_CATALOG_CREATION, "default privilege")?,
                    )?,
                    owner,
                    schema,
                    class: key.class,
                },
            };
            let image = DefaultAclGrantImage {
                identity,
                grantee,
                privileges,
                grant_options,
            };
            image.validate()?;
            Some(image)
        } else {
            None
        };
        if slot == catalog.defaults.len() {
            catalog
                .defaults
                .push(DefaultAclEntry::EMPTY)
                .expect("default privilege admission checked the reserved capacity");
        }
        let entry = &mut catalog.defaults[slot];
        let prior = entry.pending;
        if entry.committed.is_none() && prior.is_none() {
            entry.database = current_database();
            entry.owner = key.owner;
            entry.schema = key.schema;
            entry.class = key.class;
            entry.grantee = key.grantee;
        }
        entry.pending = Some(PendingDefaultAcl { txid, image });
        Ok(Some((slot, prior)))
    }

    pub(crate) fn install_default_acl_image(
        &self,
        slot: usize,
        expected: Option<DefaultAclGrantImage>,
        image: Option<DefaultAclGrantImage>,
        reference_transaction: u32,
    ) -> Result<(), SqlError> {
        if expected == image {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "stored default privilege change has no effect"
            ));
        }
        if let Some(expected) = expected {
            expected.validate()?;
        }
        if let Some(image) = image {
            image.validate()?;
        }
        let key = if let Some(image) = image {
            let owner = self
                .role_slot_by_oid(image.identity.owner.get(), reference_transaction)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "stored default privilege owner is unavailable"
                    )
                })?;
            let grantee = match image.grantee {
                None => PUBLIC_ROLE,
                Some(grantee) => self
                    .role_slot_by_oid(grantee.get(), reference_transaction)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "stored default privilege grantee is unavailable"
                        )
                    })? as u16,
            };
            let schema = match image.identity.schema {
                None => DEFAULT_ACL_ALL_SCHEMAS,
                Some(identity) => (0..self.schema_count())
                    .find(|slot| {
                        let definition = self.schema_def_for(*slot, reference_transaction);
                        definition.database == current_database()
                            && definition.ddl_state.visible_to(reference_transaction)
                            && definition.identity() == Some(identity)
                    })
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "stored default privilege schema is unavailable"
                        )
                    })? as u16,
            };
            Some(DefaultAclKey {
                owner: owner as u16,
                schema,
                class: image.identity.class,
                grantee,
            })
        } else {
            None
        };
        let mut catalog = self.acl_catalog.lock().expect("ACL catalog lock poisoned");
        if slot >= catalog.defaults.capacity() {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "stored default privilege position is out of range"
            ));
        }
        let existing = catalog
            .defaults
            .get(slot)
            .copied()
            .unwrap_or(DefaultAclEntry::EMPTY);
        if existing.pending.is_some()
            || existing.committed != expected
            || (existing.committed.is_some() && existing.database != current_database())
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "stored default privilege change selects a different lifetime"
            ));
        }
        if let Some(key) = key
            && catalog.defaults.iter().enumerate().any(|(other, entry)| {
                other != slot
                    && entry.database == current_database()
                    && entry.committed.is_some()
                    && entry.owner == key.owner
                    && entry.schema == key.schema
                    && entry.class == key.class
                    && entry.grantee == key.grantee
            })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "stored default privilege grant is duplicated"
            ));
        }
        while catalog.defaults.len() <= slot {
            catalog
                .defaults
                .push(DefaultAclEntry::EMPTY)
                .expect("stored default privilege position fits the reserved capacity");
        }
        let entry = &mut catalog.defaults[slot];
        if let Some(key) = key {
            entry.database = current_database();
            entry.owner = key.owner;
            entry.schema = key.schema;
            entry.class = key.class;
            entry.grantee = key.grantee;
        }
        entry.committed = image;
        entry.pending = None;
        if let Some(image) = image {
            self.catalog_sequence
                .observe(image.identity.creation.created_at());
        }
        Ok(())
    }

    pub(crate) fn default_acl_entry(&self, slot: usize) -> DefaultAclEntry {
        self.acl_catalog
            .lock()
            .expect("ACL catalog lock poisoned")
            .defaults[slot]
    }

    pub(crate) fn default_acl_entry_count(&self) -> usize {
        self.acl_catalog
            .lock()
            .expect("ACL catalog lock poisoned")
            .defaults
            .len()
    }

    pub(crate) fn default_acl_entries(
        &self,
    ) -> impl Iterator<Item = (usize, DefaultAclEntry)> + '_ {
        DefaultAclIter {
            catalog: &self.acl_catalog,
            database: Some(current_database()),
            next_slot: 0,
        }
    }

    pub(crate) fn commit_default_acl(&self, slot: usize, txid: u32) {
        let mut catalog = self.acl_catalog.lock().expect("ACL catalog lock poisoned");
        let entry = &mut catalog.defaults[slot];
        if let Some(pending) = entry.pending
            && pending.txid == txid
        {
            entry.committed = pending.image;
            entry.pending = None;
        }
    }

    pub(crate) fn restore_default_acl_pending(
        &self,
        slot: usize,
        prior: Option<PendingDefaultAcl>,
    ) {
        self.acl_catalog
            .lock()
            .expect("ACL catalog lock poisoned")
            .defaults[slot]
            .pending = prior;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::{test_budget, test_config};

    #[test]
    fn catalog_lifetime_privileges_and_partition_triggers_reject_stale_default_groups() {
        let public = DefaultAclGrantImage {
            identity: DefaultAclIdentity {
                creation: CatalogIdentity::parse(41).unwrap(),
                owner: RoleOid::BOOTSTRAP,
                schema: None,
                class: DefaultPrivilegeClass::Table,
            },
            grantee: None,
            privileges: PrivilegeSet::SELECT,
            grant_options: PrivilegeSet::NONE,
        };
        for second in [
            DefaultAclGrantImage {
                identity: DefaultAclIdentity {
                    schema: Some(SchemaIdentity::parse(0).unwrap()),
                    ..public.identity
                },
                ..public
            },
            DefaultAclGrantImage {
                identity: DefaultAclIdentity {
                    creation: CatalogIdentity::parse(42).unwrap(),
                    ..public.identity
                },
                grantee: Some(RoleOid::BOOTSTRAP),
                privileges: PrivilegeSet::NONE,
                ..public
            },
        ] {
            let config = test_config();
            let mut budget = test_budget(&config);
            let storage = Storage::new(&config, &mut budget, 0).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                storage
                    .install_default_acl_image(0, None, Some(public), 0)
                    .unwrap();
                storage
                    .install_default_acl_image(1, None, Some(second), 0)
                    .unwrap();
                assert_eq!(
                    storage.validate_default_acl_images().unwrap_err().sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                assert_eq!(
                    storage
                        .validate_restored_catalog_identities(42)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                assert_eq!(storage.default_acl_entry(0).committed, Some(public));
            });
        }
        let config = test_config();
        let mut budget = test_budget(&config);
        let storage = Storage::new(&config, &mut budget, 0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            storage
                .install_default_acl_image(0, None, Some(public), 0)
                .unwrap();
            storage.validate_default_acl_images().unwrap();
            assert_eq!(
                storage
                    .install_default_acl_image(1, None, Some(public), 0)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            let replaced = DefaultAclGrantImage {
                identity: DefaultAclIdentity {
                    creation: CatalogIdentity::parse(42).unwrap(),
                    ..public.identity
                },
                ..public
            };
            assert_eq!(
                storage
                    .install_default_acl_image(0, Some(replaced), None, 0)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(storage.default_acl_entry(0).committed, Some(public));
        });
    }
}
