//! Database copies retain physical positions through journal recovery.

use super::*;
use crate::mem::FixedBuf;

const CLONE_POSITION_BYTES: usize = 1 + CatalogPosition::ENCODED_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum DatabaseCloneKind {
    Table = 0,
    Domain = 1,
    Enum = 2,
    Composite = 3,
    Sequence = 4,
    View = 5,
    Rule = 6,
    MaterializedView = 7,
    Routine = 8,
    ForeignWrapper = 9,
    ForeignServer = 10,
    UserMapping = 11,
    ForeignTable = 12,
    Statistics = 13,
    Publication = 14,
    Extension = 15,
    Collation = 16,
    Conversion = 17,
    TextSearch = 18,
    TextSearchBuiltin = 19,
    Cast = 20,
    AccessMethod = 21,
    EventTrigger = 22,
    Operator = 23,
    OperatorFamily = 24,
    OperatorClass = 25,
    DefaultAcl = 26,
}

impl DatabaseCloneKind {
    fn decode(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::Table,
            1 => Self::Domain,
            2 => Self::Enum,
            3 => Self::Composite,
            4 => Self::Sequence,
            5 => Self::View,
            6 => Self::Rule,
            7 => Self::MaterializedView,
            8 => Self::Routine,
            9 => Self::ForeignWrapper,
            10 => Self::ForeignServer,
            11 => Self::UserMapping,
            12 => Self::ForeignTable,
            13 => Self::Statistics,
            14 => Self::Publication,
            15 => Self::Extension,
            16 => Self::Collation,
            17 => Self::Conversion,
            18 => Self::TextSearch,
            19 => Self::TextSearchBuiltin,
            20 => Self::Cast,
            21 => Self::AccessMethod,
            22 => Self::EventTrigger,
            23 => Self::Operator,
            24 => Self::OperatorFamily,
            25 => Self::OperatorClass,
            26 => Self::DefaultAcl,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DatabaseClonePositions<'bytes> {
    records: &'bytes [u8],
}

impl<'bytes> DatabaseClonePositions<'bytes> {
    pub(crate) fn decode(bytes: &'bytes [u8]) -> Option<Self> {
        let count = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
        let records = bytes.get(4..)?;
        if count.checked_mul(CLONE_POSITION_BYTES)? != records.len() {
            return None;
        }
        let mut previous = None;
        for record in records.as_chunks::<CLONE_POSITION_BYTES>().0 {
            let kind = DatabaseCloneKind::decode(record[0])?;
            let position = CatalogPosition::decode(&record[1..])?;
            let key = (kind, position.slot());
            if previous.is_some_and(|previous| previous >= key) {
                return None;
            }
            previous = Some(key);
        }
        Some(Self { records })
    }

    fn position(
        self,
        kind: DatabaseCloneKind,
        created_at: u64,
    ) -> Result<CatalogPosition, SqlError> {
        let mut matched = None;
        for record in self.records.as_chunks::<CLONE_POSITION_BYTES>().0 {
            let position = CatalogPosition::decode(&record[1..]).expect("validated clone position");
            if record[0] == kind as u8
                && position.created_at() == created_at
                && matched.replace(position).is_some()
            {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "duplicate database clone source lifetime"
                ));
            }
        }
        matched.ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "database clone source lifetime is missing"
            )
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum DatabaseCloneAllocation<'bytes> {
    New,
    Restore(DatabaseClonePositions<'bytes>),
}

impl DatabaseCloneAllocation<'_> {
    pub(super) fn select(
        self,
        kind: DatabaseCloneKind,
        created_at: u64,
        mut available: impl Iterator<Item = usize>,
    ) -> Result<usize, SqlError> {
        match self {
            Self::New => available.next().ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "database clone {:?} catalog is full",
                    kind
                )
            }),
            Self::Restore(positions) => {
                let position = positions.position(kind, created_at)?;
                if available.any(|slot| slot == position.slot()) {
                    Ok(position.slot())
                } else {
                    Err(sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "saved database clone position is unavailable"
                    ))
                }
            }
        }
    }

    pub(super) fn validate(self, storage: &Storage, database: DatabaseOid) -> Result<(), SqlError> {
        let Self::Restore(positions) = self else {
            return Ok(());
        };
        let mut count = 0;
        let mut valid = true;
        storage.visit_database_clone_positions(database, |kind, position| {
            count += 1;
            valid &= positions.position(kind, position.created_at()).ok() == Some(position);
        });
        if valid && count == positions.records.len() / CLONE_POSITION_BYTES {
            Ok(())
        } else {
            Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "database clone inventory does not match its saved positions"
            ))
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum WalDatabaseClonePositions<'source> {
    Captured {
        storage: &'source Storage,
        database: DatabaseOid,
    },
    Encoded(DatabaseClonePositions<'source>),
}

impl core::fmt::Debug for WalDatabaseClonePositions<'_> {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        output
            .debug_struct("WalDatabaseClonePositions")
            .field("bytes", &self.encoded_len())
            .finish()
    }
}

impl WalDatabaseClonePositions<'_> {
    pub(crate) fn encoded_len(self) -> usize {
        match self {
            Self::Encoded(positions) => 4 + positions.records.len(),
            Self::Captured { storage, database } => {
                let mut count = 0;
                storage.visit_database_clone_positions(database, |_, _| count += 1);
                4 + count * CLONE_POSITION_BYTES
            }
        }
    }

    pub(crate) fn append(self, output: &mut FixedBuf) -> bool {
        match self {
            Self::Encoded(positions) => {
                output.append(
                    &((positions.records.len() / CLONE_POSITION_BYTES) as u32).to_le_bytes(),
                ) && output.append(positions.records)
            }
            Self::Captured { storage, database } => {
                let count = (self.encoded_len() - 4) / CLONE_POSITION_BYTES;
                let mut complete = output.append(&(count as u32).to_le_bytes());
                storage.visit_database_clone_positions(database, |kind, position| {
                    complete &= output.append(&[kind as u8]) && output.append(&position.encode());
                });
                complete
            }
        }
    }
}

impl Storage {
    fn visit_database_clone_positions(
        &self,
        database: DatabaseOid,
        mut visit: impl FnMut(DatabaseCloneKind, CatalogPosition),
    ) {
        let mut object = |kind, slot, created_at, owner, state| {
            if owner == database && state != CatalogDdlState::Absent {
                visit(
                    kind,
                    CatalogPosition::new(slot, created_at)
                        .expect("copied object has a durable position"),
                );
            }
        };
        for (slot, table) in self.tables.iter().take(self.table_count()).enumerate() {
            let identity = table.identity();
            object(
                DatabaseCloneKind::Table,
                slot,
                identity.created_at,
                identity.database,
                identity.existence,
            );
        }
        {
            let catalog = self
                .type_catalog
                .lock()
                .expect("type catalog lock poisoned");
            for (slot, definition) in catalog.domains.iter().enumerate() {
                object(
                    DatabaseCloneKind::Domain,
                    slot,
                    definition.created_at,
                    definition.database,
                    definition.ddl_state,
                );
            }
            for (slot, definition) in catalog.enums.iter().enumerate() {
                object(
                    DatabaseCloneKind::Enum,
                    slot,
                    definition.created_at,
                    definition.database,
                    definition.ddl_state,
                );
            }
            for (slot, definition) in catalog.composites.iter().enumerate() {
                object(
                    DatabaseCloneKind::Composite,
                    slot,
                    definition.created_at,
                    definition.database,
                    definition.ddl_state,
                );
            }
        }
        {
            let catalog = self.sequence_catalog();
            for (slot, definition) in catalog.definitions.iter().enumerate() {
                object(
                    DatabaseCloneKind::Sequence,
                    slot,
                    definition.created_at,
                    definition.database,
                    definition.ddl_state,
                );
            }
        }
        {
            let catalog = self.view_rule_catalog();
            for (slot, definition) in catalog.views.iter().enumerate() {
                object(
                    DatabaseCloneKind::View,
                    slot,
                    definition.created_at,
                    definition.database,
                    definition.ddl_state,
                );
            }
            for (slot, definition) in catalog.rules.iter().enumerate() {
                object(
                    DatabaseCloneKind::Rule,
                    slot,
                    definition.created_at,
                    definition.database,
                    definition.ddl_state,
                );
            }
        }
        for (slot, definition) in self.matview_catalog().iter().enumerate() {
            object(
                DatabaseCloneKind::MaterializedView,
                slot,
                definition.created_at,
                definition.database,
                definition.ddl_state,
            );
        }
        for (slot, definition) in self
            .routines
            .lock()
            .expect("routine catalog lock poisoned")
            .iter()
            .enumerate()
        {
            object(
                DatabaseCloneKind::Routine,
                slot,
                definition.created_at,
                definition.database,
                definition.ddl_state,
            );
        }
        let catalog = self.foreign_catalog();
        for slot in 0..catalog.wrapper_capacity() {
            let entry = catalog.entry_wrapper(slot);
            object(
                DatabaseCloneKind::ForeignWrapper,
                slot,
                entry.created_at,
                entry.database,
                entry.ddl_state,
            );
        }
        for slot in 0..catalog.server_capacity() {
            let entry = catalog.entry_server(slot);
            object(
                DatabaseCloneKind::ForeignServer,
                slot,
                entry.created_at,
                entry.database,
                entry.ddl_state,
            );
        }
        for slot in 0..catalog.mapping_capacity() {
            let entry = catalog.entry_mapping(slot);
            object(
                DatabaseCloneKind::UserMapping,
                slot,
                entry.created_at,
                entry.database,
                entry.ddl_state,
            );
        }
        for slot in 0..catalog.table_capacity() {
            let entry = catalog.entry_table(slot);
            object(
                DatabaseCloneKind::ForeignTable,
                slot,
                entry.created_at,
                entry.database,
                entry.ddl_state,
            );
        }
        drop(catalog);
        for (slot, definition) in self
            .extended_statistics_catalog
            .lock()
            .expect("statistics catalog lock poisoned")
            .definitions
            .iter()
            .enumerate()
        {
            object(
                DatabaseCloneKind::Statistics,
                slot,
                definition.created_at,
                definition.database,
                definition.ddl_state,
            );
        }
        for (slot, definition) in self
            .publications
            .lock()
            .expect("publication catalog lock poisoned")
            .iter()
            .enumerate()
        {
            object(
                DatabaseCloneKind::Publication,
                slot,
                definition.created_at,
                definition.database,
                definition.ddl_state,
            );
        }
        for (slot, definition) in self.extension_catalog().definitions.iter().enumerate() {
            object(
                DatabaseCloneKind::Extension,
                slot,
                definition.created_at,
                definition.database,
                definition.ddl_state,
            );
        }
        let text = self
            .text_catalog
            .lock()
            .expect("text catalog lock poisoned");
        for (slot, definition) in text.collations.iter().enumerate() {
            object(
                DatabaseCloneKind::Collation,
                slot,
                definition.created_at,
                definition.database,
                definition.ddl_state,
            );
        }
        for (slot, definition) in text.conversions.iter().enumerate() {
            object(
                DatabaseCloneKind::Conversion,
                slot,
                definition.created_at,
                definition.database,
                definition.ddl_state,
            );
        }
        for selected_kind in [
            DatabaseCloneKind::TextSearch,
            DatabaseCloneKind::TextSearchBuiltin,
        ] {
            for (slot, definition) in text.text_search_objects.iter().enumerate() {
                if definition.database != database
                    || definition.ddl_state == CatalogDdlState::Absent
                {
                    continue;
                }
                let (kind, identity) =
                    TextSearchIdentity::admit(definition.created_at, definition.definition)
                        .expect("admitted text object has a checked identity")
                        .clone_key();
                if kind == selected_kind {
                    object(
                        kind,
                        slot,
                        identity,
                        definition.database,
                        definition.ddl_state,
                    );
                }
            }
        }
        let casts = self.casts.lock().expect("cast catalog lock poisoned");
        for (slot, definition) in casts.iter().enumerate() {
            object(
                DatabaseCloneKind::Cast,
                slot,
                definition.created_at,
                definition.database,
                definition.ddl_state,
            );
        }
        let operators = self
            .operator_catalog
            .lock()
            .expect("operator catalog lock poisoned");
        for (slot, definition) in operators.access_methods.iter().enumerate() {
            object(
                DatabaseCloneKind::AccessMethod,
                slot,
                definition.created_at,
                definition.database,
                definition.ddl_state,
            );
        }
        let events = self
            .event_triggers
            .lock()
            .expect("event trigger catalog lock poisoned");
        for (slot, definition) in events.iter().enumerate() {
            object(
                DatabaseCloneKind::EventTrigger,
                slot,
                definition.created_at,
                definition.database,
                definition.ddl_state,
            );
        }
        for (slot, definition) in operators.operators.iter().enumerate() {
            object(
                DatabaseCloneKind::Operator,
                slot,
                definition.created_at,
                definition.database,
                definition.ddl_state,
            );
        }
        for (slot, definition) in operators.families.iter().enumerate() {
            object(
                DatabaseCloneKind::OperatorFamily,
                slot,
                definition.created_at,
                definition.database,
                definition.ddl_state,
            );
        }
        for (slot, definition) in operators.classes.iter().enumerate() {
            object(
                DatabaseCloneKind::OperatorClass,
                slot,
                definition.created_at,
                definition.database,
                definition.ddl_state,
            );
        }
        drop(operators);
        drop(casts);
        drop(text);
        {
            let catalog = self.acl_catalog.lock().expect("ACL catalog lock poisoned");
            for (slot, entry) in catalog.defaults.iter().enumerate() {
                if let Some(image) = entry.committed {
                    object(
                        DatabaseCloneKind::DefaultAcl,
                        slot,
                        image.clone_key(),
                        entry.database,
                        CatalogDdlState::Present,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_clone_positions_reject_missing_occupied_and_malformed_sources() {
        let mut bytes = [0; 26];
        bytes[..4].copy_from_slice(&2u32.to_le_bytes());
        bytes[4] = DatabaseCloneKind::Table as u8;
        bytes[5..15].copy_from_slice(&CatalogPosition::new(7, 42).unwrap().encode());
        bytes[15] = DatabaseCloneKind::Sequence as u8;
        bytes[16..].copy_from_slice(&CatalogPosition::new(8, 43).unwrap().encode());
        crate::mem::guard::forbid_alloc(|| {
            let positions = DatabaseClonePositions::decode(&bytes).unwrap();
            let allocation = DatabaseCloneAllocation::Restore(positions);
            assert_eq!(
                allocation
                    .select(DatabaseCloneKind::Table, 42, [7].into_iter())
                    .unwrap(),
                7
            );
            assert_eq!(
                allocation
                    .select(DatabaseCloneKind::Table, 42, [0, 1].into_iter())
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(
                allocation
                    .select(DatabaseCloneKind::Table, 99, [7].into_iter())
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            for end in 0..bytes.len() {
                assert!(DatabaseClonePositions::decode(&bytes[..end]).is_none());
            }
            let mut invalid = bytes;
            invalid[4] = u8::MAX;
            assert!(DatabaseClonePositions::decode(&invalid).is_none());
            invalid = bytes;
            invalid[7..15].fill(0);
            assert!(DatabaseClonePositions::decode(&invalid).is_none());
            invalid = bytes;
            invalid[15..].copy_from_slice(&bytes[4..15]);
            assert!(DatabaseClonePositions::decode(&invalid).is_none());
            invalid = bytes;
            invalid[0] = 3;
            assert!(DatabaseClonePositions::decode(&invalid).is_none());
        });
    }
}
