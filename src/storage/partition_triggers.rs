//! Inherited triggers have their own lifetimes, independent of table-slot reuse.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PartitionTriggerImage {
    pub(crate) creation: CatalogIdentity,
    pub(crate) trigger: CatalogPosition,
    pub(crate) table: CatalogPosition,
    pub(crate) enabled: TriggerEnabled,
}

impl PartitionTriggerImage {
    pub(crate) fn oid(self) -> i32 {
        FIRST_PARTITION_TRIGGER_OID + self.creation.get() as i32
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingPartitionTriggerState {
    pub(super) txid: u32,
    pub(super) image: Option<PartitionTriggerImage>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PartitionTriggerState {
    pub(super) database: DatabaseOid,
    pub(crate) committed: Option<PartitionTriggerImage>,
    pub(super) pending: Option<PendingPartitionTriggerState>,
}

impl PartitionTriggerState {
    pub(super) const EMPTY: Self = Self {
        database: DatabaseOid::POSTGRES,
        committed: None,
        pending: None,
    };

    pub(crate) fn image_to(self, txid: u32) -> Option<PartitionTriggerImage> {
        match self.pending.filter(|pending| pending.txid == txid) {
            Some(pending) => pending.image,
            None => self.committed,
        }
    }
}

impl Storage {
    pub(super) fn partition_trigger_state_slot(&self, trigger: usize, table: usize) -> usize {
        trigger * self.table_count() + table
    }

    pub(crate) fn partition_trigger_state(&self, slot: usize) -> PartitionTriggerState {
        self.trigger_catalog
            .lock()
            .expect("trigger catalog lock poisoned")
            .partition_states[slot]
    }

    pub(crate) fn partition_trigger_image_to(
        &self,
        trigger: usize,
        table: usize,
        txid: u32,
    ) -> Option<PartitionTriggerImage> {
        let state = self.partition_trigger_state(self.partition_trigger_state_slot(trigger, table));
        (state.database == current_database())
            .then(|| state.image_to(txid))
            .flatten()
    }

    pub(crate) fn partition_trigger_enabled_to(
        &self,
        trigger: usize,
        table: usize,
        txid: u32,
    ) -> TriggerEnabled {
        self.partition_trigger_image_to(trigger, table, txid)
            .expect("visible inherited triggers have a reserved catalog image")
            .enabled
    }

    pub(crate) fn partition_trigger_oid_to(
        &self,
        trigger: &TriggerDef,
        table: usize,
        txid: u32,
    ) -> Result<i32, SqlError> {
        let slot = self
            .triggers_with_slots_visible_to(txid)
            .find_map(|(slot, candidate)| {
                (candidate.created_at == trigger.created_at).then_some(slot)
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "inherited trigger parent is unavailable"
                )
            })?;
        let image = self
            .partition_trigger_image_to(slot, table, txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "inherited trigger identity is unavailable"
                )
            })?;
        if image.trigger.created_at() != trigger.created_at
            || image.table != self.table_position(table)
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "inherited trigger selects a replaced object"
            ));
        }
        Ok(image.oid())
    }

    fn stage_partition_trigger_image(
        &mut self,
        trigger: usize,
        table: usize,
        image: Option<PartitionTriggerImage>,
        txid: u32,
    ) -> Result<Option<(usize, PartitionTriggerState)>, SqlError> {
        let slot = self.partition_trigger_state_slot(trigger, table);
        let mut catalog = self
            .trigger_catalog
            .lock()
            .expect("trigger catalog lock poisoned");
        let prior = catalog.partition_states[slot];
        if let Some(pending) = prior.pending
            && pending.txid != txid
        {
            let name = catalog.definitions[trigger].name;
            drop(catalog);
            return Err(self.catalog_ddl_wait_error(txid, pending.txid, name.as_str()));
        }
        if prior.image_to(txid) == image {
            return Ok(None);
        }
        if (prior.committed.is_some() || prior.pending.is_some())
            && prior.database != current_database()
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "inherited trigger selects another database"
            ));
        }
        catalog.partition_states[slot] = PartitionTriggerState {
            database: current_database(),
            committed: prior.committed,
            pending: Some(PendingPartitionTriggerState { txid, image }),
        };
        Ok(Some((slot, prior)))
    }

    pub(crate) fn stage_partition_trigger_enabled(
        &mut self,
        trigger: usize,
        table: usize,
        enabled: TriggerEnabled,
        txid: u32,
    ) -> Result<Option<(usize, PartitionTriggerState)>, SqlError> {
        let image = match self.partition_trigger_image_to(trigger, table, txid) {
            Some(image) => PartitionTriggerImage { enabled, ..image },
            None => PartitionTriggerImage {
                creation: CatalogIdentity::require(
                    self.catalog_sequence
                        .next_bounded(MAX_CATALOG_CREATION, "inherited trigger")?,
                )?,
                trigger: CatalogPosition::new(trigger, self.trigger(trigger).created_at)?,
                table: self.table_position(table),
                enabled,
            },
        };
        self.stage_partition_trigger_image(trigger, table, Some(image), txid)
    }

    /// DDL owns the topology change and records every changed image for rollback.
    pub(crate) fn reconcile_partition_triggers(
        &mut self,
        txn: &mut crate::sql::txn::TxnState,
    ) -> Result<(), SqlError> {
        for trigger_slot in 0..self.trigger_capacity() {
            let trigger = self.trigger_to(trigger_slot, txn.txid);
            for table in 0..self.table_count() {
                let slot = self.partition_trigger_state_slot(trigger_slot, table);
                let state = self.partition_trigger_state(slot);
                if trigger.database != current_database() && state.database != current_database() {
                    continue;
                }
                let current = (state.database == current_database())
                    .then(|| state.image_to(txn.txid))
                    .flatten();
                let desired = trigger.database == current_database()
                    && trigger.visible_to(txn.txid)
                    && matches!(trigger.level, crate::sql::ast::TriggerLevel::Row)
                    && self.table_slot_visible_to(table, txn.txid)
                    && matches!(trigger.target, TriggerTarget::Table(parent) if self.partition_descends_from(table, usize::from(parent), txn.txid));
                let change = if desired && current.is_none() {
                    let TriggerTarget::Table(parent) = trigger.target else {
                        unreachable!()
                    };
                    let mut ancestor = usize::from(
                        self.table_def(table, txn.txid)
                            .partition
                            .attachment
                            .expect("a partition descendant has a parent")
                            .parent,
                    );
                    let enabled = loop {
                        if ancestor == usize::from(parent) {
                            break trigger.enabled_to(txn.txid);
                        }
                        if let Some(image) =
                            self.partition_trigger_image_to(trigger_slot, ancestor, txn.txid)
                        {
                            break image.enabled;
                        }
                        ancestor = usize::from(
                            self.table_def(ancestor, txn.txid)
                                .partition
                                .attachment
                                .expect("a trigger's ancestor remains attached to its root")
                                .parent,
                        );
                    };
                    self.stage_partition_trigger_enabled(trigger_slot, table, enabled, txn.txid)?
                } else if !desired && current.is_some() {
                    self.stage_partition_trigger_image(trigger_slot, table, None, txn.txid)?
                } else {
                    None
                };
                if let Some((slot, prior)) = change
                    && let Err(error) =
                        txn.record_ddl(crate::sql::txn::DdlUndo::PartitionTriggerAltered {
                            slot: slot as u32,
                            prior,
                        })
                {
                    self.rollback_partition_trigger_state(slot, prior);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn commit_partition_trigger_state(&mut self, slot: usize, txid: u32) {
        let mut catalog = self
            .trigger_catalog
            .lock()
            .expect("trigger catalog lock poisoned");
        let state = &mut catalog.partition_states[slot];
        if let Some(pending) = state.pending
            && pending.txid == txid
        {
            state.committed = pending.image;
            state.pending = None;
        }
    }

    pub(crate) fn rollback_partition_trigger_state(
        &mut self,
        slot: usize,
        prior: PartitionTriggerState,
    ) {
        self.trigger_catalog
            .lock()
            .expect("trigger catalog lock poisoned")
            .partition_states[slot] = prior;
    }

    pub(crate) fn install_partition_trigger_image(
        &mut self,
        expected: Option<PartitionTriggerImage>,
        image: Option<PartitionTriggerImage>,
        reference_transaction: u32,
    ) -> Result<(), SqlError> {
        let selected = image
            .or(expected)
            .ok_or_else(|| sql_err!(sqlstate::DATA_EXCEPTION, "empty inherited trigger change"))?;
        if expected == image
            || selected.trigger.slot() >= self.trigger_capacity()
            || selected.table.slot() >= self.table_count()
            || expected.is_some_and(|old| {
                old.trigger.slot() != selected.trigger.slot()
                    || old.table.slot() != selected.table.slot()
            })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "invalid inherited trigger change"
            ));
        }
        if let Some(image) = image {
            let trigger = self.trigger_to(image.trigger.slot(), reference_transaction);
            if trigger.database != current_database()
                || !trigger.visible_to(reference_transaction)
                || trigger.created_at != image.trigger.created_at()
                || !self.table_slot_visible_to(image.table.slot(), reference_transaction)
                || self.table_position(image.table.slot()) != image.table
                || !matches!(trigger.level, crate::sql::ast::TriggerLevel::Row)
                || !matches!(trigger.target, TriggerTarget::Table(parent) if self.partition_descends_from(image.table.slot(), usize::from(parent), reference_transaction))
            {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "stored inherited trigger selects a replaced or unrelated object"
                ));
            }
        }
        let slot =
            self.partition_trigger_state_slot(selected.trigger.slot(), selected.table.slot());
        let mut catalog = self
            .trigger_catalog
            .lock()
            .expect("trigger catalog lock poisoned");
        let state = &mut catalog.partition_states[slot];
        if state.pending.is_some()
            || state.committed != expected
            || (state.committed.is_some() && state.database != current_database())
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "stored inherited trigger selects another lifetime"
            ));
        }
        *state = PartitionTriggerState {
            database: current_database(),
            committed: image,
            pending: None,
        };
        drop(catalog);
        if let Some(image) = image {
            self.catalog_sequence.observe(image.creation.created_at());
        }
        Ok(())
    }

    pub(super) fn copy_partition_trigger_images(
        &mut self,
        source: DatabaseOid,
        txid: u32,
    ) -> Result<(), SqlError> {
        let capacity = self
            .trigger_catalog
            .lock()
            .expect("trigger catalog lock poisoned")
            .partition_states
            .len();
        for slot in 0..capacity {
            let state = self.partition_trigger_state(slot);
            let Some(image) = state.committed.filter(|_| state.database == source) else {
                continue;
            };
            if self.table_def(image.table.slot(), 0).persistence == RelationPersistence::Temporary {
                continue;
            }
            let trigger = self
                .triggers_with_slots_visible_to(txid)
                .find_map(|(slot, trigger)| {
                    (trigger.created_at == image.trigger.created_at()).then_some(slot)
                })
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "copied inherited trigger has no parent"
                    )
                })?;
            let table = (0..self.table_count())
                .find(|slot| {
                    self.table_slot_visible_to(*slot, txid)
                        && self.table(*slot).created_at() == image.table.created_at()
                })
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "copied inherited trigger has no table"
                    )
                })?;
            self.install_partition_trigger_image(
                None,
                Some(PartitionTriggerImage {
                    trigger: CatalogPosition::new(trigger, image.trigger.created_at())?,
                    table: self.table_position(table),
                    ..image
                }),
                txid,
            )?;
        }
        Ok(())
    }

    pub(crate) fn partition_trigger_states(
        &self,
    ) -> impl Iterator<Item = (DatabaseOid, PartitionTriggerImage)> + '_ {
        PartitionTriggerStateIter {
            catalog: &self.trigger_catalog,
            next_slot: 0,
        }
        .filter_map(|state| state.committed.map(|image| (state.database, image)))
        .filter(|(_, image)| {
            self.table_def(image.table.slot(), 0).persistence != RelationPersistence::Temporary
        })
    }
}
