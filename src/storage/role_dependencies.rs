//! Role changes wait for references in every database before changing identity.

use super::{
    AccessClass, AccessObject, CollationIter, ConversionIter, OperatorClassIter,
    OperatorFamilyIter, OperatorIter, PolicyIter, PublicationIter, RoleObjectDependency, Storage,
    SubscriptionIter, TextSearchIter, foreign,
};
use crate::sql::eval::SqlError;

impl Storage {
    pub(crate) fn role_object_dependency(
        &self,
        role: usize,
        txid: u32,
    ) -> Result<Option<RoleObjectDependency>, SqlError> {
        let role_oid = self.role_oid(role);
        let pending_acl = {
            let catalog = self.acl_catalog.lock().expect("ACL catalog lock poisoned");
            catalog
                .objects
                .iter()
                .find_map(|entry| {
                    entry
                        .pending
                        .filter(|pending| {
                            pending.txid != txid
                                && [
                                    entry.grantee,
                                    entry.grantor,
                                    pending.grantee,
                                    pending.grantor,
                                ]
                                .contains(&(role as u16))
                        })
                        .map(|pending| pending.txid)
                })
                .or_else(|| {
                    catalog.columns.iter().find_map(|entry| {
                        entry
                            .pending
                            .filter(|pending| {
                                pending.txid != txid
                                    && [
                                        entry.grantee,
                                        entry.grantor,
                                        pending.grantee,
                                        pending.grantor,
                                    ]
                                    .contains(&(role as u16))
                            })
                            .map(|pending| pending.txid)
                    })
                })
                .or_else(|| {
                    catalog.parameters.iter().find_map(|entry| {
                        entry
                            .pending
                            .filter(|pending| {
                                pending.txid != txid
                                    && [
                                        entry.grantee,
                                        entry.grantor,
                                        pending.grantee,
                                        pending.grantor,
                                    ]
                                    .contains(&(role as u16))
                            })
                            .map(|pending| pending.txid)
                    })
                })
                .or_else(|| {
                    catalog.defaults.iter().find_map(|entry| {
                        entry
                            .pending
                            .filter(|pending| {
                                pending.txid != txid
                                    && [entry.owner, entry.grantee].contains(&(role as u16))
                            })
                            .map(|pending| pending.txid)
                    })
                })
        };
        if let Some(owner) = pending_acl {
            return Err(self.catalog_ddl_wait_error(txid, owner, "role privileges"));
        }
        let (pending_role_rows, granted_memberships) = {
            let catalog = self
                .role_catalog
                .lock()
                .expect("role catalog lock poisoned");
            let blocker = catalog
                .memberships
                .iter()
                .find_map(|entry| {
                    entry
                        .pending
                        .filter(|pending| {
                            pending.txid != txid
                                && [entry.role, entry.member, entry.grantor, pending.grantor]
                                    .contains(&(role as u16))
                        })
                        .map(|pending| pending.txid)
                })
                .or_else(|| {
                    catalog.settings.iter().find_map(|entry| {
                        entry
                            .pending
                            .filter(|pending| {
                                pending.txid != txid && entry.scope.role() == Some(role as u16)
                            })
                            .map(|pending| pending.txid)
                    })
                });
            let granted = catalog.memberships.iter().any(|entry| {
                entry.visible_to(txid)
                    && entry.grantor_to(txid) == role as u16
                    && entry.role != role as u16
                    && entry.member != role as u16
            });
            (blocker, granted)
        };
        if let Some(owner) = pending_role_rows {
            return Err(self.catalog_ddl_wait_error(txid, owner, "role membership or setting"));
        }
        let (foreign_wrapper_capacity, foreign_server_capacity) = {
            let catalog = self.foreign_catalog();
            (catalog.wrapper_capacity(), catalog.server_capacity())
        };
        let classes = [
            (AccessClass::Database, self.database_count()),
            (AccessClass::Table, self.tables.len()),
            (AccessClass::View, self.view_count()),
            (AccessClass::MaterializedView, self.matview_count()),
            (AccessClass::Sequence, self.sequence_count()),
            (AccessClass::Schema, self.schema_count()),
            (AccessClass::Domain, self.domain_count()),
            (AccessClass::Enum, self.enum_count()),
            (AccessClass::Index, self.index_count()),
            (AccessClass::Routine, self.routine_count()),
            (AccessClass::Composite, self.composite_count()),
            (AccessClass::Tablespace, self.tablespace_capacity()),
            (AccessClass::Statistics, self.extended_statistics_count()),
            (AccessClass::Extension, self.extension_count()),
            (AccessClass::EventTrigger, self.event_trigger_capacity()),
            (
                AccessClass::LargeObject,
                self.access_class_slots(AccessClass::LargeObject),
            ),
            (AccessClass::ForeignDataWrapper, foreign_wrapper_capacity),
            (AccessClass::ForeignServer, foreign_server_capacity),
        ];
        let mut owned = false;
        for (class, count) in classes {
            for slot in 0..count {
                let object = AccessObject {
                    class,
                    slot: slot as u16,
                };
                let ownership = self.ownership(object);
                if let Some(pending) = ownership.pending
                    && pending.txid != txid
                    && self.access_object_visible_in_any_database(object, pending.txid)
                    && (usize::from(pending.owner) == role
                        || (self.access_object_visible_in_any_database(object, txid)
                            && usize::from(ownership.owner) == role))
                {
                    return Err(self.catalog_ddl_wait_error(txid, pending.txid, "role ownership"));
                }
                owned |= self.access_object_visible_in_any_database(object, txid)
                    && self.object_owner(object, txid) == role;
            }
        }
        for (_, publication) in (PublicationIter {
            catalog: &self.publications,
            next_slot: 0,
        }) {
            if let Some(pending) = publication.ownership.pending
                && pending.txid != txid
                && publication.visible_to(pending.txid)
                && (usize::from(pending.owner) == role
                    || (publication.visible_to(txid)
                        && usize::from(publication.ownership.owner) == role))
            {
                return Err(self.catalog_ddl_wait_error(txid, pending.txid, "publication owner"));
            }
            owned |= publication.visible_to(txid)
                && usize::from(publication.ownership.owner_to(txid)) == role;
        }
        for (_, subscription) in (SubscriptionIter {
            catalog: &self.subscription_catalog,
            next_slot: 0,
        }) {
            if let Some(pending) = subscription.ownership.pending
                && pending.txid != txid
                && subscription.visible_to(pending.txid)
                && (usize::from(pending.owner) == role
                    || (subscription.visible_to(txid)
                        && usize::from(subscription.ownership.owner) == role))
            {
                return Err(self.catalog_ddl_wait_error(txid, pending.txid, "subscription owner"));
            }
            owned |= subscription.visible_to(txid)
                && usize::from(subscription.ownership.owner_to(txid)) == role;
        }
        macro_rules! pending_catalog_owner {
            ($iterator:expr) => {
                for (_, entry) in $iterator {
                    for owner in [
                        entry.pending.map(|pending| pending.txid),
                        entry.ddl_state.pending_txid(),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        if owner != txid
                            && ((entry.visible_to(owner)
                                && entry.definition_for(owner).owner == role_oid)
                                || (entry.visible_to(txid)
                                    && entry.definition_for(txid).owner == role_oid))
                        {
                            return Err(self.catalog_ddl_wait_error(
                                txid,
                                owner,
                                "role catalog ownership",
                            ));
                        }
                    }
                }
            };
        }
        pending_catalog_owner!(OperatorIter {
            catalog: &self.operator_catalog,
            next_slot: 0
        });
        pending_catalog_owner!(OperatorFamilyIter {
            catalog: &self.operator_catalog,
            next_slot: 0
        });
        pending_catalog_owner!(OperatorClassIter {
            catalog: &self.operator_catalog,
            next_slot: 0
        });
        pending_catalog_owner!(CollationIter {
            catalog: &self.text_catalog,
            next_slot: 0
        });
        pending_catalog_owner!(ConversionIter {
            catalog: &self.text_catalog,
            next_slot: 0
        });
        for (_, entry) in (TextSearchIter {
            catalog: &self.text_catalog,
            next_slot: 0,
        }) {
            for owner in [
                entry.pending.map(|pending| pending.txid),
                entry.ddl_state.pending_txid(),
            ]
            .into_iter()
            .flatten()
            {
                if owner != txid
                    && ((entry.visible_to(owner)
                        && entry.definition_for(owner).owner() == Some(role_oid))
                        || (entry.visible_to(txid)
                            && entry.definition_for(txid).owner() == Some(role_oid)))
                {
                    return Err(self.catalog_ddl_wait_error(
                        txid,
                        owner,
                        "role text search ownership",
                    ));
                }
            }
        }
        let (mapping_blocker, mapping_dependency) = {
            let catalog = self.foreign_catalog();
            let mut blocker = None;
            let mut dependency = false;
            for slot in 0..catalog.mapping_capacity() {
                let entry = catalog.entry_mapping(slot);
                dependency |= entry.visible_to(txid)
                    && entry.definition_for(txid).user
                        == foreign::ForeignMappingUser::Role(role as u16);
                if let Some(owner) = entry.pending_owner_other_than(txid)
                    && (entry.definition_for(owner).user
                        == foreign::ForeignMappingUser::Role(role as u16)
                        || entry.definition_for(txid).user
                            == foreign::ForeignMappingUser::Role(role as u16))
                {
                    blocker = Some(owner);
                }
            }
            (blocker, dependency)
        };
        if let Some(owner) = mapping_blocker {
            return Err(self.catalog_ddl_wait_error(txid, owner, "role user mapping"));
        }
        for (slot, entry) in (PolicyIter {
            catalog: &self.policy_catalog,
            next_slot: 0,
        }) {
            for owner in [
                entry.pending_definition.map(|pending| pending.txid),
                entry.ddl_state.pending_txid(),
            ]
            .into_iter()
            .flatten()
            {
                if owner != txid
                    && ((entry.visible_to(owner)
                        && self.policy_roles(slot, owner).contains(&(role as u16)))
                        || (entry.visible_to(txid)
                            && self.policy_roles(slot, txid).contains(&(role as u16))))
                {
                    return Err(self.catalog_ddl_wait_error(txid, owner, "role policy"));
                }
            }
        }
        if granted_memberships {
            return Ok(Some(RoleObjectDependency::GrantedRoleMembership));
        }
        if mapping_dependency {
            return Ok(Some(RoleObjectDependency::ForeignUserMapping));
        }
        let owned_catalog = (OperatorIter {
            catalog: &self.operator_catalog,
            next_slot: 0,
        })
        .filter(|(_, entry)| entry.visible_to(txid))
        .map(|(slot, entry)| (slot, entry.definition_for(txid)))
        .any(|(_, definition)| definition.owner == role_oid)
            || (OperatorFamilyIter {
                catalog: &self.operator_catalog,
                next_slot: 0,
            })
            .filter(|(_, entry)| entry.visible_to(txid))
            .map(|(slot, entry)| (slot, entry.definition_for(txid)))
            .any(|(_, definition)| definition.owner == role_oid)
            || (OperatorClassIter {
                catalog: &self.operator_catalog,
                next_slot: 0,
            })
            .filter(|(_, entry)| entry.visible_to(txid))
            .map(|(slot, entry)| (slot, entry.definition_for(txid)))
            .any(|(_, definition)| definition.owner == role_oid)
            || (CollationIter {
                catalog: &self.text_catalog,
                next_slot: 0,
            })
            .filter(|(_, entry)| entry.visible_to(txid))
            .map(|(slot, entry)| (slot, entry.definition_for(txid)))
            .any(|(_, definition)| definition.owner == role_oid)
            || (ConversionIter {
                catalog: &self.text_catalog,
                next_slot: 0,
            })
            .filter(|(_, entry)| entry.visible_to(txid))
            .map(|(slot, entry)| (slot, entry.definition_for(txid)))
            .any(|(_, definition)| definition.owner == role_oid)
            || (TextSearchIter {
                catalog: &self.text_catalog,
                next_slot: 0,
            })
            .any(|(_, entry)| {
                entry.visible_to(txid) && entry.definition_for(txid).owner() == Some(role_oid)
            });
        if owned {
            return Ok(Some(RoleObjectDependency::OwnedObject));
        }
        if owned_catalog {
            return Ok(Some(RoleObjectDependency::OwnedCatalogObject));
        }
        {
            let catalog = self.acl_catalog.lock().expect("ACL catalog lock poisoned");
            if catalog.objects.iter().any(|entry| {
                let (visible, grantee, grantor, _, _) = Self::acl_visible(entry, txid);
                visible
                    && self.access_object_visible_in_any_database(entry.object, txid)
                    && (grantee == role as u16 || grantor == role as u16)
            }) {
                return Ok(Some(RoleObjectDependency::ObjectPrivilege));
            }
            if catalog.parameters.iter().any(|entry| {
                let (visible, grantee, grantor, _, _) = Self::parameter_acl_visible(entry, txid);
                visible && (grantee == role as u16 || grantor == role as u16)
            }) {
                return Ok(Some(RoleObjectDependency::ParameterPrivilege));
            }
            if catalog.defaults.iter().any(|entry| {
                let (defined, _, _) = Self::default_acl_visible(entry, txid);
                defined && (entry.owner == role as u16 || entry.grantee == role as u16)
            }) {
                return Ok(Some(RoleObjectDependency::DefaultPrivilege));
            }
            if catalog.columns.iter().any(|entry| {
                let (visible, grantee, grantor, _, _) = Self::column_acl_visible(entry, txid);
                visible
                    && self.access_object_visible_in_any_database(entry.target.relation, txid)
                    && (grantee == role as u16 || grantor == role as u16)
            }) {
                return Ok(Some(RoleObjectDependency::ColumnPrivilege));
            }
        }
        Ok((PolicyIter {
            catalog: &self.policy_catalog,
            next_slot: 0,
        })
        .filter(|(_, policy)| policy.visible_to(txid))
        .any(|(slot, _)| self.policy_roles(slot, txid).contains(&(role as u16)))
        .then_some(RoleObjectDependency::Policy))
    }
}
