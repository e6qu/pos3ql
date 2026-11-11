//! Per-database `pg_proc` rows for PostgreSQL's built-in routines.
//!
//! Every database has its own copy of the built-in `pg_proc` rows, so
//! `ALTER FUNCTION lower(text) STABLE` and `REVOKE EXECUTE ON FUNCTION md5(text)
//! FROM PUBLIC` change one database only. A row exists here only after such a
//! change; its slot is the [`AccessObject`] identity of the built-in routine in
//! that database, so ownership and privilege grants reuse the ordinary object
//! catalogs while the row carries the database identity.

use super::*;

/// Built-in routine rows share the 16-bit object-slot representation.
pub(crate) const MAX_BUILTIN_ROUTINE_CATALOG_SLOTS: usize = u16::MAX as usize;

/// The OID of a PostgreSQL built-in callable routine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BuiltinRoutineOid(i32);

impl BuiltinRoutineOid {
    pub(crate) fn parse(oid: i32) -> Option<Self> {
        crate::sql::catalog::builtin_callable_by_oid(oid).map(|_| Self(oid))
    }

    pub(crate) const fn get(self) -> i32 {
        self.0
    }

    pub(crate) fn signature(self) -> &'static crate::sql::catalog::BuiltinCallableSignature {
        crate::sql::catalog::builtin_callable_by_oid(self.get())
            .expect("parsed built-in routine OID has a declaration")
    }
}

/// The `pg_proc` attributes of a built-in routine that `ALTER FUNCTION` may
/// change. Per-call settings are not stored: `SET` and `RESET` of a built-in
/// are rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BuiltinRoutineAttributes {
    pub attributes: RoutineAttributes,
}

impl BuiltinRoutineAttributes {
    /// The attributes PostgreSQL 18.6 initializes for `routine`.
    pub(crate) fn catalog(routine: BuiltinRoutineOid) -> Self {
        let metadata = &routine.signature().catalog;
        Self {
            attributes: RoutineAttributes {
                strict: metadata.strict,
                volatility: match metadata.volatility {
                    "i" => RoutineVolatility::Immutable,
                    "s" => RoutineVolatility::Stable,
                    _ => RoutineVolatility::Volatile,
                },
                parallel: match metadata.parallel {
                    "s" => RoutineParallel::Safe,
                    "r" => RoutineParallel::Restricted,
                    _ => RoutineParallel::Unsafe,
                },
                security_definer: metadata.security_definer,
                leakproof: metadata.leakproof,
                cost_bits: Some(metadata.cost.to_bits()),
                rows_bits: Some(metadata.rows.to_bits()),
            },
        }
    }

    /// Accepts an image of `routine` whose estimates are present.
    pub(crate) fn new(
        _routine: BuiltinRoutineOid,
        attributes: RoutineAttributes,
    ) -> Result<Self, SqlError> {
        if attributes.cost_bits.is_none() || attributes.rows_bits.is_none() {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "built-in routine attribute image is invalid"
            ));
        }
        Ok(Self { attributes })
    }

    pub(crate) fn cost(&self) -> f64 {
        f64::from_bits(self.attributes.cost_bits.expect("built-in cost is present"))
    }

    pub(crate) fn rows(&self) -> f64 {
        f64::from_bits(
            self.attributes
                .rows_bits
                .expect("built-in rows are present"),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingBuiltinRoutine {
    pub txid: u32,
    pub attributes: BuiltinRoutineAttributes,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BuiltinRoutineEntry {
    pub database: DatabaseOid,
    /// `None` marks a free slot.
    pub routine: Option<BuiltinRoutineOid>,
    pub ownership: Ownership,
    pub committed: BuiltinRoutineAttributes,
    pub pending: Option<PendingBuiltinRoutine>,
}

impl BuiltinRoutineEntry {
    pub(super) fn empty() -> Self {
        let placeholder = BuiltinRoutineOid::parse(870).expect("lower(text) is built in");
        Self {
            database: DatabaseOid::POSTGRES,
            routine: None,
            ownership: Ownership::BOOTSTRAP,
            committed: BuiltinRoutineAttributes::catalog(placeholder),
            pending: None,
        }
    }

    pub(crate) fn attributes_to(&self, txid: u32) -> BuiltinRoutineAttributes {
        match self.pending {
            Some(pending) if pending.txid == txid => pending.attributes,
            _ => self.committed,
        }
    }

    /// A row equal to the initialized catalog carries no information.
    fn is_initial(&self) -> bool {
        self.routine.is_some_and(|routine| {
            self.pending.is_none()
                && self.ownership.pending.is_none()
                && self.ownership.owner == BOOTSTRAP_ROLE
                && self.committed == BuiltinRoutineAttributes::catalog(routine)
        })
    }
}

/// The built-in routine row state a transaction sees, for catalogs and calls.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BuiltinRoutineState {
    pub slot: Option<u16>,
    pub owner: u16,
    pub attributes: BuiltinRoutineAttributes,
}

impl BuiltinRoutineState {
    pub(crate) fn access_object(self) -> Option<AccessObject> {
        self.slot.map(|slot| AccessObject {
            class: AccessClass::BuiltinRoutine,
            slot,
        })
    }
}

impl Storage {
    pub(crate) fn builtin_routine_entry(&self, slot: usize) -> BuiltinRoutineEntry {
        self.builtin_routines
            .lock()
            .expect("built-in routine catalog lock poisoned")[slot]
    }

    pub(crate) fn builtin_routine_count(&self) -> usize {
        self.builtin_routines
            .lock()
            .expect("built-in routine catalog lock poisoned")
            .len()
    }

    pub(super) fn builtin_routine_slot_in(
        &self,
        database: DatabaseOid,
        routine: BuiltinRoutineOid,
    ) -> Option<usize> {
        self.builtin_routines
            .lock()
            .expect("built-in routine catalog lock poisoned")
            .iter()
            .position(|entry| entry.database == database && entry.routine == Some(routine))
    }

    /// The row of a built-in routine in the current database, if any.
    pub(crate) fn builtin_routine_slot(&self, routine: BuiltinRoutineOid) -> Option<usize> {
        self.builtin_routine_slot_in(current_database(), routine)
    }

    /// What `txid` sees for built-in routine `oid` in the current database.
    pub(crate) fn builtin_routine_state(
        &self,
        routine: BuiltinRoutineOid,
        txid: u32,
    ) -> BuiltinRoutineState {
        match self.builtin_routine_slot(routine) {
            Some(slot) => {
                let entry = self.builtin_routine_entry(slot);
                BuiltinRoutineState {
                    slot: Some(slot as u16),
                    owner: entry.ownership.owner_to(txid),
                    attributes: entry.attributes_to(txid),
                }
            }
            None => BuiltinRoutineState {
                slot: None,
                owner: BOOTSTRAP_ROLE,
                attributes: BuiltinRoutineAttributes::catalog(routine),
            },
        }
    }

    /// Whether the current database marked a built-in routine named `name`
    /// stable or volatile. Name-based checks treat every overload alike.
    pub(crate) fn builtin_routine_name_is_nonimmutable(&self, name: &str, txid: u32) -> bool {
        let name = name.strip_prefix("pg_catalog.").unwrap_or(name);
        let database = current_database();
        self.builtin_routines
            .lock()
            .expect("built-in routine catalog lock poisoned")
            .iter()
            .any(|entry| {
                entry.database == database
                    && entry
                        .routine
                        .is_some_and(|routine| routine.signature().name == name)
                    && entry.attributes_to(txid).attributes.volatility
                        != RoutineVolatility::Immutable
            })
    }

    /// The row for `routine` in the current database, claiming a free or
    /// reclaimable slot on first use. Exhaustion is a named `54000`.
    pub(crate) fn ensure_builtin_routine(
        &self,
        routine: BuiltinRoutineOid,
    ) -> Result<u16, SqlError> {
        self.ensure_builtin_routine_in(current_database(), routine)
    }

    pub(super) fn ensure_builtin_routine_in(
        &self,
        database: DatabaseOid,
        routine: BuiltinRoutineOid,
    ) -> Result<u16, SqlError> {
        if let Some(slot) = self.builtin_routine_slot_in(database, routine) {
            return Ok(slot as u16);
        }
        let mut catalog = self
            .builtin_routines
            .lock()
            .expect("built-in routine catalog lock poisoned");
        let free = catalog.iter().position(|entry| entry.routine.is_none());
        let slot = match free {
            Some(slot) => slot,
            None => {
                // Rows equal to the initialized catalog and named by no
                // privilege can be reused for another routine.
                let reclaimable = (0..catalog.len()).find(|slot| {
                    catalog[*slot].is_initial()
                        && !self.access_object_has_acl(AccessObject {
                            class: AccessClass::BuiltinRoutine,
                            slot: *slot as u16,
                        })
                });
                match reclaimable {
                    Some(slot) => slot,
                    None if catalog.len() < catalog.capacity() => {
                        catalog
                            .push(BuiltinRoutineEntry::empty())
                            .expect("capacity was checked");
                        catalog.len() - 1
                    }
                    None => {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "built-in routine catalog is full (max_builtin_routine_entries = {})",
                            catalog.capacity()
                        ));
                    }
                }
            }
        };
        catalog[slot] = BuiltinRoutineEntry {
            database,
            routine: Some(routine),
            ownership: Ownership::BOOTSTRAP,
            committed: BuiltinRoutineAttributes::catalog(routine),
            pending: None,
        };
        Ok(slot as u16)
    }

    /// Stages `attributes` for `txid`, waiting for another transaction's
    /// unfinished change of the same row. Returns the prior private image.
    pub(crate) fn set_builtin_routine_attributes(
        &self,
        slot: u16,
        attributes: BuiltinRoutineAttributes,
        txid: u32,
    ) -> Result<Option<PendingBuiltinRoutine>, SqlError> {
        let mut catalog = self
            .builtin_routines
            .lock()
            .expect("built-in routine catalog lock poisoned");
        let entry = &mut catalog[usize::from(slot)];
        if let Some(pending) = entry.pending.filter(|pending| pending.txid != txid) {
            drop(catalog);
            return Err(self.catalog_ddl_wait_error(txid, pending.txid, "pg_proc"));
        }
        let prior = entry.pending;
        if txid == 0 {
            entry.committed = attributes;
            entry.pending = None;
        } else {
            entry.pending = Some(PendingBuiltinRoutine { txid, attributes });
        }
        Ok(prior)
    }

    pub(crate) fn restore_builtin_routine_pending(
        &self,
        slot: u16,
        prior: Option<PendingBuiltinRoutine>,
    ) {
        self.builtin_routines
            .lock()
            .expect("built-in routine catalog lock poisoned")[usize::from(slot)]
        .pending = prior;
    }

    pub(crate) fn commit_builtin_routine(&self, slot: u16, txid: u32) {
        let mut catalog = self
            .builtin_routines
            .lock()
            .expect("built-in routine catalog lock poisoned");
        let entry = &mut catalog[usize::from(slot)];
        if let Some(pending) = entry.pending.filter(|pending| pending.txid == txid) {
            entry.committed = pending.attributes;
            entry.pending = None;
        }
    }

    /// Replays a committed row image (journal or checkpoint).
    pub(crate) fn restore_builtin_routine(
        &self,
        database: DatabaseOid,
        routine: BuiltinRoutineOid,
        owner: u16,
        attributes: BuiltinRoutineAttributes,
    ) -> Result<u16, SqlError> {
        let slot = self.ensure_builtin_routine_in(database, routine)?;
        let mut catalog = self
            .builtin_routines
            .lock()
            .expect("built-in routine catalog lock poisoned");
        let entry = &mut catalog[usize::from(slot)];
        entry.committed = attributes;
        entry.pending = None;
        entry.ownership = Ownership {
            owner,
            pending: None,
        };
        Ok(slot)
    }

    /// Replays a committed attribute image, keeping the row's owner.
    pub(crate) fn replay_builtin_routine_attributes(
        &self,
        database: DatabaseOid,
        routine: BuiltinRoutineOid,
        attributes: BuiltinRoutineAttributes,
    ) -> Result<(), SqlError> {
        let slot = self.ensure_builtin_routine_in(database, routine)?;
        let mut catalog = self
            .builtin_routines
            .lock()
            .expect("built-in routine catalog lock poisoned");
        let entry = &mut catalog[usize::from(slot)];
        entry.committed = attributes;
        entry.pending = None;
        Ok(())
    }

    /// The catalog OID that journal and checkpoint records use to address a
    /// routine object.
    pub(crate) fn access_object_routine_oid(&self, object: AccessObject) -> Option<i32> {
        match object.class {
            AccessClass::Routine => Some(routine_oid(&self.routine(usize::from(object.slot)))),
            AccessClass::BuiltinRoutine => self
                .builtin_routine_entry(usize::from(object.slot))
                .routine
                .map(BuiltinRoutineOid::get),
            _ => None,
        }
    }

    /// Resolves a journal or checkpoint object reference. Routines are named
    /// by OID; a built-in routine's row in the current database is claimed
    /// when first referenced.
    pub(crate) fn resolve_recorded_access_object(
        &self,
        class: AccessClass,
        routine_oid: i32,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Result<Option<AccessObject>, SqlError> {
        match class {
            AccessClass::Routine => Ok(self
                .routine_slot_by_oid(routine_oid, txid)
                .map(Self::routine_access_object)),
            AccessClass::BuiltinRoutine => {
                let routine = BuiltinRoutineOid::parse(routine_oid).ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "recorded built-in routine OID {} is unknown",
                        routine_oid
                    )
                })?;
                Ok(Some(AccessObject {
                    class,
                    slot: self.ensure_builtin_routine(routine)?,
                }))
            }
            _ => Ok(self.resolve_access_object(class, schema, name, txid)),
        }
    }

    pub(crate) fn builtin_routine_ownership(&self, slot: usize) -> Ownership {
        self.builtin_routine_entry(slot).ownership
    }

    pub(super) fn builtin_routine_ownership_mut<R>(
        &self,
        slot: usize,
        change: impl FnOnce(&mut Ownership) -> R,
    ) -> R {
        change(
            &mut self
                .builtin_routines
                .lock()
                .expect("built-in routine catalog lock poisoned")[slot]
                .ownership,
        )
    }

    /// `DROP DATABASE` removes the database's copies of the built-in rows.
    pub(super) fn release_database_builtin_routines(&self, database: DatabaseOid) {
        for entry in self
            .builtin_routines
            .lock()
            .expect("built-in routine catalog lock poisoned")
            .iter_mut()
            .filter(|entry| entry.database == database)
        {
            *entry = BuiltinRoutineEntry::empty();
        }
    }

    /// `CREATE DATABASE ... TEMPLATE` copies the template's committed rows,
    /// including unchanged rows that privilege rows name.
    pub(super) fn clone_database_builtin_routines(
        &self,
        source: DatabaseOid,
        target: DatabaseOid,
    ) -> Result<(), SqlError> {
        let count = self.builtin_routine_count();
        for slot in 0..count {
            let entry = self.builtin_routine_entry(slot);
            let Some(routine) = entry.routine.filter(|_| entry.database == source) else {
                continue;
            };
            self.restore_builtin_routine(target, routine, entry.ownership.owner, entry.committed)?;
        }
        Ok(())
    }

    /// Committed rows that differ from the initialized catalog or carry
    /// privileges, for checkpoints.
    pub(crate) fn checkpoint_builtin_routines(
        &self,
    ) -> impl Iterator<Item = (usize, BuiltinRoutineEntry)> + '_ {
        (0..self.builtin_routine_count()).filter_map(move |slot| {
            let entry = self.builtin_routine_entry(slot);
            (entry.routine.is_some()
                && (!entry.is_initial()
                    || self.access_object_has_acl(AccessObject {
                        class: AccessClass::BuiltinRoutine,
                        slot: slot as u16,
                    })))
            .then_some((slot, entry))
        })
    }

    pub(crate) fn builtin_routine_budget_bytes(config: &crate::config::Config) -> usize {
        config
            .max_builtin_routine_entries
            .saturating_mul(core::mem::size_of::<BuiltinRoutineEntry>())
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;

    #[test]
    fn builtin_routine_oids_are_closed_over_the_catalog() {
        assert!(BuiltinRoutineOid::parse(870).is_some());
        assert!(BuiltinRoutineOid::parse(0).is_none());
        assert!(BuiltinRoutineOid::parse(16_384).is_none());
        let lower = BuiltinRoutineOid::parse(870).unwrap();
        let catalog = BuiltinRoutineAttributes::catalog(lower);
        assert_eq!(catalog.attributes.volatility, RoutineVolatility::Immutable);
        assert!(catalog.attributes.strict);
        assert_eq!(catalog.cost(), 1.0);
        let mut changed = catalog.attributes;
        changed.volatility = RoutineVolatility::Stable;
        assert!(BuiltinRoutineAttributes::new(lower, changed).is_ok());
        changed.strict = false;
        changed.security_definer = true;
        assert!(BuiltinRoutineAttributes::new(lower, changed).is_ok());
        changed.cost_bits = None;
        assert!(BuiltinRoutineAttributes::new(lower, changed).is_err());
    }
}
