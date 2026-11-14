//! Transactional foreign-data catalog state.

use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql::ast::{ForeignOption as ParsedOption, ForeignOptionAction};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::util::StackStr;

use super::{CatalogDdlState, CatalogIdentity, DatabaseOid, Ownership, PendingOwnership, SqlName};

/// One OPTIONS clause follows the parser's complete bounded list.
pub(crate) const MAX_FOREIGN_OPTIONS: usize = super::MAX_DEFINITION_ITEMS;
/// Column options share one sparse table-level catalog. Its bound admits one
/// option for every accepted column or a complete OPTIONS clause on one
/// column, without the former 32-entry storage-only ceiling.
pub(crate) const MAX_FOREIGN_COLUMN_OPTIONS: usize = MAX_FOREIGN_OPTIONS;
pub(crate) const FOREIGN_OPTION_VALUE_MAX: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ForeignOption {
    pub name: SqlName,
    pub value: StackStr<FOREIGN_OPTION_VALUE_MAX>,
}

impl ForeignOption {
    const EMPTY: Self = Self {
        name: SqlName::EMPTY,
        value: StackStr::new(),
    };

    fn parse(value: ParsedOption<'_>) -> Result<Self, SqlError> {
        let option = StackStr::from_str(value.value);
        if option.is_truncated() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "foreign option value exceeds {} bytes",
                FOREIGN_OPTION_VALUE_MAX
            ));
        }
        Ok(Self {
            name: SqlName::parse(value.name)?,
            value: option,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ForeignOptions {
    entries: [ForeignOption; MAX_FOREIGN_OPTIONS],
    count: u8,
}

impl ForeignOptions {
    pub(crate) const EMPTY: Self = Self {
        entries: [ForeignOption::EMPTY; MAX_FOREIGN_OPTIONS],
        count: 0,
    };

    pub(crate) fn parse(options: &[ParsedOption<'_>]) -> Result<Self, SqlError> {
        if options.len() > MAX_FOREIGN_OPTIONS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many foreign options (limit {})",
                MAX_FOREIGN_OPTIONS
            ));
        }
        let mut result = Self::EMPTY;
        for option in options {
            let parsed = ForeignOption::parse(*option)?;
            if result.get(parsed.name.as_str()).is_some() {
                return Err(sql_err!(
                    sqlstate::DUPLICATE_OBJECT,
                    "option \"{}\" provided more than once",
                    parsed.name.as_str()
                ));
            }
            result.entries[result.count as usize] = parsed;
            result.count += 1;
        }
        Ok(result)
    }

    pub(crate) fn entries(&self) -> &[ForeignOption] {
        &self.entries[..self.count as usize]
    }

    pub(crate) fn restore_option(&mut self, name: &str, value: &str) -> Result<(), SqlError> {
        if self.count as usize == MAX_FOREIGN_OPTIONS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many foreign options (limit {})",
                MAX_FOREIGN_OPTIONS
            ));
        }
        let name = SqlName::parse(name)?;
        if self.get(name.as_str()).is_some() {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "option \"{}\" provided more than once",
                name.as_str()
            ));
        }
        let value = StackStr::from_str(value);
        if value.is_truncated() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "foreign option value exceeds {} bytes",
                FOREIGN_OPTION_VALUE_MAX
            ));
        }
        self.entries[self.count as usize] = ForeignOption { name, value };
        self.count += 1;
        Ok(())
    }

    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.entries()
            .iter()
            .find(|entry| entry.name.as_str() == name)
            .map(|entry| entry.value.as_str())
    }

    pub(crate) fn alter(mut self, actions: &[ForeignOptionAction<'_>]) -> Result<Self, SqlError> {
        for action in actions {
            match *action {
                ForeignOptionAction::Add(option) => {
                    if self.get(option.name).is_some() {
                        return Err(sql_err!(
                            sqlstate::DUPLICATE_OBJECT,
                            "option \"{}\" provided more than once",
                            option.name
                        ));
                    }
                    if self.count as usize == MAX_FOREIGN_OPTIONS {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "too many foreign options (limit {})",
                            MAX_FOREIGN_OPTIONS
                        ));
                    }
                    self.entries[self.count as usize] = ForeignOption::parse(option)?;
                    self.count += 1;
                }
                ForeignOptionAction::Set(option) => {
                    let Some(index) = self
                        .entries()
                        .iter()
                        .position(|entry| entry.name.as_str() == option.name)
                    else {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "option \"{}\" not found",
                            option.name
                        ));
                    };
                    self.entries[index] = ForeignOption::parse(option)?;
                }
                ForeignOptionAction::Drop(name) => {
                    let Some(index) = self
                        .entries()
                        .iter()
                        .position(|entry| entry.name.as_str() == name)
                    else {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "option \"{}\" not found",
                            name
                        ));
                    };
                    self.entries
                        .copy_within(index + 1..self.count as usize, index);
                    self.count -= 1;
                    self.entries[self.count as usize] = ForeignOption::EMPTY;
                }
            }
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// A wrapper's handler: `Postgres` names, by `pg_proc` OID, the `LANGUAGE C`
/// routine bound to postgres_fdw's native handler.
pub(crate) enum ForeignDataHandler {
    None,
    Postgres { routine: i32 },
}

impl ForeignDataHandler {
    /// The durable form: the routine OID, or 0 for no handler.
    pub(crate) const fn routine_oid(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Postgres { routine } => routine,
        }
    }

    pub(crate) const fn from_routine_oid(oid: i32) -> Option<Self> {
        match oid {
            0 => Some(Self::None),
            routine if routine > 0 => Some(Self::Postgres { routine }),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// A wrapper's validator, by the OID of the routine bound to postgres_fdw's
/// native validator.
pub(crate) enum ForeignDataValidator {
    None,
    Postgres { routine: i32 },
}

impl ForeignDataValidator {
    /// The durable form: the routine OID, or 0 for no validator.
    pub(crate) const fn routine_oid(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Postgres { routine } => routine,
        }
    }

    pub(crate) const fn from_routine_oid(oid: i32) -> Option<Self> {
        match oid {
            0 => Some(Self::None),
            routine if routine > 0 => Some(Self::Postgres { routine }),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ForeignDataWrapperDefinition {
    pub name: SqlName,
    pub handler: ForeignDataHandler,
    pub validator: ForeignDataValidator,
    pub options: ForeignOptions,
}

impl ForeignDataWrapperDefinition {
    const EMPTY: Self = Self {
        name: SqlName::EMPTY,
        handler: ForeignDataHandler::None,
        validator: ForeignDataValidator::None,
        options: ForeignOptions::EMPTY,
    };

    /// PostgreSQL's error for scanning, modifying or importing through a
    /// wrapper created without a handler.
    pub(crate) fn require_handler(&self) -> Result<(), SqlError> {
        match self.handler {
            ForeignDataHandler::Postgres { .. } => Ok(()),
            ForeignDataHandler::None => Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "foreign-data wrapper \"{}\" has no handler",
                self.name.as_str()
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ForeignServerDefinition {
    pub name: SqlName,
    pub wrapper: u16,
    pub server_type: Option<StackStr<FOREIGN_OPTION_VALUE_MAX>>,
    pub version: Option<StackStr<FOREIGN_OPTION_VALUE_MAX>>,
    pub options: ForeignOptions,
}

impl ForeignServerDefinition {
    const EMPTY: Self = Self {
        name: SqlName::EMPTY,
        wrapper: u16::MAX,
        server_type: None,
        version: None,
        options: ForeignOptions::EMPTY,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ForeignMappingUser {
    Public,
    Role(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UserMappingDefinition {
    pub server: u16,
    pub user: ForeignMappingUser,
    pub options: ForeignOptions,
}

impl UserMappingDefinition {
    const EMPTY: Self = Self {
        server: u16::MAX,
        user: ForeignMappingUser::Public,
        options: ForeignOptions::EMPTY,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ForeignColumnOption {
    pub column: u16,
    pub option: ForeignOption,
}

impl ForeignColumnOption {
    const EMPTY: Self = Self {
        column: u16::MAX,
        option: ForeignOption::EMPTY,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ForeignColumnOptions {
    entries: [ForeignColumnOption; MAX_FOREIGN_COLUMN_OPTIONS],
    count: u8,
}

impl ForeignColumnOptions {
    pub(crate) const EMPTY: Self = Self {
        entries: [ForeignColumnOption::EMPTY; MAX_FOREIGN_COLUMN_OPTIONS],
        count: 0,
    };

    pub(crate) fn entries(&self) -> &[ForeignColumnOption] {
        &self.entries[..self.count as usize]
    }

    pub(crate) fn append(&mut self, column: u16, options: ForeignOptions) -> Result<(), SqlError> {
        for option in options.entries() {
            if self.count as usize == MAX_FOREIGN_COLUMN_OPTIONS {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many foreign column options (limit {})",
                    MAX_FOREIGN_COLUMN_OPTIONS
                ));
            }
            self.entries[self.count as usize] = ForeignColumnOption {
                column,
                option: *option,
            };
            self.count += 1;
        }
        Ok(())
    }

    pub(crate) fn options_for(&self, column: u16) -> impl Iterator<Item = ForeignOption> + '_ {
        self.entries()
            .iter()
            .filter(move |entry| entry.column == column)
            .map(|entry| entry.option)
    }

    pub(crate) fn alter(
        self,
        column: u16,
        actions: &[ForeignOptionAction<'_>],
    ) -> Result<Self, SqlError> {
        let mut current = ForeignOptions::EMPTY;
        for option in self.options_for(column) {
            current.entries[current.count as usize] = option;
            current.count += 1;
        }
        let current = current.alter(actions)?;
        let mut updated = Self::EMPTY;
        for entry in self.entries() {
            if entry.column != column {
                if updated.count as usize == MAX_FOREIGN_COLUMN_OPTIONS {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "too many foreign column options (limit {})",
                        MAX_FOREIGN_COLUMN_OPTIONS
                    ));
                }
                updated.entries[updated.count as usize] = *entry;
                updated.count += 1;
            }
        }
        updated.append(column, current)?;
        Ok(updated)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ForeignTableDefinition {
    pub table: u16,
    pub server: u16,
    pub options: ForeignOptions,
    pub column_options: ForeignColumnOptions,
}

impl ForeignTableDefinition {
    const EMPTY: Self = Self {
        table: u16::MAX,
        server: u16::MAX,
        options: ForeignOptions::EMPTY,
        column_options: ForeignColumnOptions::EMPTY,
    };
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingForeignDefinition<D: Copy> {
    pub txid: u32,
    pub definition: D,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ForeignCatalogEntry<D: Copy> {
    pub database: DatabaseOid,
    pub created_at: u64,
    pub definition: D,
    pub pending: Option<PendingForeignDefinition<D>>,
    pub ownership: Ownership,
    pub ddl_state: CatalogDdlState,
}

impl<D: Copy> ForeignCatalogEntry<D> {
    fn empty(definition: D) -> Self {
        Self {
            database: DatabaseOid::POSTGRES,
            created_at: 0,
            definition,
            pending: None,
            ownership: Ownership::BOOTSTRAP,
            ddl_state: CatalogDdlState::Absent,
        }
    }

    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }

    pub(crate) fn definition_for(&self, txid: u32) -> D {
        self.pending
            .filter(|pending| pending.txid == txid)
            .map_or(self.definition, |pending| pending.definition)
    }

    pub(crate) fn pending_owner_other_than(&self, txid: u32) -> Option<u32> {
        [
            self.ddl_state.pending_txid(),
            self.pending.map(|pending| pending.txid),
            self.ownership.pending.map(|pending| pending.txid),
        ]
        .into_iter()
        .flatten()
        .find(|owner| *owner != txid)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreparedForeignKey {
    Wrapper(SqlName),
    Server(SqlName),
    Mapping {
        server: u16,
        user: ForeignMappingUser,
    },
    Table(u16),
}

#[derive(Clone, Copy)]
pub(crate) struct PreparedForeignNamespace {
    pub class: ForeignObjectClass,
    pub slot: usize,
    pub created_at: u64,
    pub key: Option<PreparedForeignKey>,
}

#[derive(Clone, Copy)]
pub(crate) enum PreparedForeignChange<D: Copy> {
    Unchanged,
    Created,
    Dropped,
    Changed {
        // Outer None means unchanged; inner None means no prior private image.
        definition: Option<Option<PendingForeignDefinition<D>>>,
        ownership: Option<Option<PendingOwnership>>,
    },
}

pub(crate) struct PreparedForeignImage<D: Copy> {
    pub database: DatabaseOid,
    pub created_at: u64,
    pub owner: Option<u16>,
    pub definition: Option<D>,
    pub transaction: u32,
}

pub(crate) struct ForeignCatalog {
    wrappers: FixedVec<ForeignCatalogEntry<ForeignDataWrapperDefinition>>,
    servers: FixedVec<ForeignCatalogEntry<ForeignServerDefinition>>,
    mappings: FixedVec<ForeignCatalogEntry<UserMappingDefinition>>,
    tables: FixedVec<ForeignCatalogEntry<ForeignTableDefinition>>,
}

impl ForeignCatalog {
    pub(crate) fn parent_reference_owner(
        &self,
        class: ForeignObjectClass,
        slot: usize,
        database: DatabaseOid,
        transaction: u32,
    ) -> Result<Option<u32>, SqlError> {
        let selected = match class {
            ForeignObjectClass::Wrapper => self.wrappers.get(slot).map(|entry| {
                (
                    entry.database,
                    entry.ddl_state,
                    entry.pending_owner_other_than(transaction),
                )
            }),
            ForeignObjectClass::Server => self.servers.get(slot).map(|entry| {
                (
                    entry.database,
                    entry.ddl_state,
                    entry.pending_owner_other_than(transaction),
                )
            }),
            ForeignObjectClass::Mapping | ForeignObjectClass::Table => None,
        }
        .filter(|(selected_database, existence, _)| {
            *selected_database == database && *existence != CatalogDdlState::Absent
        })
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "referenced foreign catalog parent does not exist"
            )
        })?;
        if let Some(owner) = selected.2 {
            return Ok(Some(owner));
        }
        if !selected.1.visible_to(transaction) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "referenced foreign catalog parent does not exist"
            ));
        }
        Ok(None)
    }

    fn entry_blocks_create<D: Copy>(
        entry: &ForeignCatalogEntry<D>,
        database: DatabaseOid,
        txid: u32,
        identity_matches: impl Fn(D) -> bool,
    ) -> bool {
        if entry.database != database || entry.ddl_state == CatalogDdlState::Absent {
            return false;
        }
        if entry.pending_owner_other_than(txid).is_some()
            && (identity_matches(entry.definition)
                || entry
                    .pending
                    .is_some_and(|pending| identity_matches(pending.definition)))
        {
            return true;
        }
        entry.visible_to(txid) && identity_matches(entry.definition_for(txid))
    }

    pub(crate) fn wrapper_capacity(&self) -> usize {
        self.wrappers.len()
    }

    pub(crate) fn server_capacity(&self) -> usize {
        self.servers.len()
    }

    pub(crate) fn mapping_capacity(&self) -> usize {
        self.mappings.len()
    }

    pub(crate) fn table_capacity(&self) -> usize {
        self.tables.len()
    }

    pub(crate) fn budget_bytes(config: &Config) -> usize {
        config.max_foreign_data_wrappers
            * core::mem::size_of::<ForeignCatalogEntry<ForeignDataWrapperDefinition>>()
            + config.max_foreign_servers
                * core::mem::size_of::<ForeignCatalogEntry<ForeignServerDefinition>>()
            + config.max_user_mappings
                * core::mem::size_of::<ForeignCatalogEntry<UserMappingDefinition>>()
            + config.max_tables
                * core::mem::size_of::<ForeignCatalogEntry<ForeignTableDefinition>>()
    }

    pub(crate) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        let wrappers = Self::filled(
            budget,
            "foreign-data wrappers",
            config.max_foreign_data_wrappers,
            ForeignDataWrapperDefinition::EMPTY,
        )?;
        let servers = Self::filled(
            budget,
            "foreign servers",
            config.max_foreign_servers,
            ForeignServerDefinition::EMPTY,
        )?;
        let mappings = Self::filled(
            budget,
            "foreign user mappings",
            config.max_user_mappings,
            UserMappingDefinition::EMPTY,
        )?;
        let tables = Self::filled(
            budget,
            "foreign tables",
            config.max_tables,
            ForeignTableDefinition::EMPTY,
        )?;
        Ok(Self {
            wrappers,
            servers,
            mappings,
            tables,
        })
    }

    fn filled<D: Copy>(
        budget: &mut Budget,
        label: &'static str,
        capacity: usize,
        empty: D,
    ) -> Result<FixedVec<ForeignCatalogEntry<D>>, BudgetError> {
        let mut entries = FixedVec::new(budget, label, capacity)?;
        for _ in 0..capacity {
            entries
                .push(ForeignCatalogEntry::empty(empty))
                .expect("foreign catalog sized to configured capacity");
        }
        Ok(entries)
    }

    pub(crate) fn wrapper(
        &self,
        database: DatabaseOid,
        name: &str,
        txid: u32,
    ) -> Option<(usize, ForeignDataWrapperDefinition)> {
        self.wrappers.iter().enumerate().find_map(|(slot, entry)| {
            let definition = entry.definition_for(txid);
            (entry.database == database
                && entry.visible_to(txid)
                && definition.name.as_str() == name)
                .then_some((slot, definition))
        })
    }

    pub(crate) fn wrapper_by_slot(
        &self,
        database: DatabaseOid,
        slot: usize,
        txid: u32,
    ) -> Option<ForeignDataWrapperDefinition> {
        let entry = self.wrappers.get(slot)?;
        (entry.database == database && entry.visible_to(txid)).then(|| entry.definition_for(txid))
    }

    pub(crate) fn server(
        &self,
        database: DatabaseOid,
        name: &str,
        txid: u32,
    ) -> Option<(usize, ForeignServerDefinition)> {
        self.servers.iter().enumerate().find_map(|(slot, entry)| {
            let definition = entry.definition_for(txid);
            (entry.database == database
                && entry.visible_to(txid)
                && definition.name.as_str() == name)
                .then_some((slot, definition))
        })
    }

    pub(crate) fn server_by_slot(
        &self,
        database: DatabaseOid,
        slot: usize,
        txid: u32,
    ) -> Option<ForeignServerDefinition> {
        let entry = self.servers.get(slot)?;
        (entry.database == database && entry.visible_to(txid)).then(|| entry.definition_for(txid))
    }

    pub(crate) fn mapping(
        &self,
        database: DatabaseOid,
        server: u16,
        user: ForeignMappingUser,
        txid: u32,
    ) -> Option<(usize, UserMappingDefinition)> {
        self.mappings.iter().enumerate().find_map(|(slot, entry)| {
            let definition = entry.definition_for(txid);
            (entry.database == database
                && entry.visible_to(txid)
                && definition.server == server
                && definition.user == user)
                .then_some((slot, definition))
        })
    }

    pub(crate) fn table(
        &self,
        database: DatabaseOid,
        table: u16,
        txid: u32,
    ) -> Option<(usize, ForeignTableDefinition)> {
        self.tables.iter().enumerate().find_map(|(slot, entry)| {
            let definition = entry.definition_for(txid);
            (entry.database == database && entry.visible_to(txid) && definition.table == table)
                .then_some((slot, definition))
        })
    }

    pub(crate) fn create_wrapper(
        &mut self,
        database: DatabaseOid,
        created_at: u64,
        definition: ForeignDataWrapperDefinition,
        ownership: Ownership,
        txid: u32,
    ) -> Result<usize, SqlError> {
        if let Some(entry) = self.wrappers.iter().find(|entry| {
            Self::entry_blocks_create(entry, database, txid, |candidate| {
                candidate.name == definition.name
            })
        }) {
            if let Some(owner) = entry.pending_owner_other_than(txid) {
                return Err(sql_err!(
                    sqlstate::INTERNAL_LOCK_WAIT,
                    "waiting for transaction {} to finish changing foreign-data wrapper",
                    owner
                ));
            }
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "foreign-data wrapper \"{}\" already exists",
                definition.name.as_str()
            ));
        }
        Self::create_entry(
            &mut self.wrappers,
            database,
            created_at,
            definition,
            ownership,
            txid,
            "foreign-data wrappers",
        )
    }

    pub(crate) fn create_server(
        &mut self,
        database: DatabaseOid,
        created_at: u64,
        definition: ForeignServerDefinition,
        ownership: Ownership,
        txid: u32,
    ) -> Result<usize, SqlError> {
        if let Some(entry) = self.servers.iter().find(|entry| {
            Self::entry_blocks_create(entry, database, txid, |candidate| {
                candidate.name == definition.name
            })
        }) {
            if let Some(owner) = entry.pending_owner_other_than(txid) {
                return Err(sql_err!(
                    sqlstate::INTERNAL_LOCK_WAIT,
                    "waiting for transaction {} to finish changing foreign server",
                    owner
                ));
            }
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "server \"{}\" already exists",
                definition.name.as_str()
            ));
        }
        Self::create_entry(
            &mut self.servers,
            database,
            created_at,
            definition,
            ownership,
            txid,
            "foreign servers",
        )
    }

    pub(crate) fn create_mapping(
        &mut self,
        database: DatabaseOid,
        created_at: u64,
        definition: UserMappingDefinition,
        txid: u32,
    ) -> Result<usize, SqlError> {
        if let Some(entry) = self.mappings.iter().find(|entry| {
            Self::entry_blocks_create(entry, database, txid, |candidate| {
                candidate.server == definition.server && candidate.user == definition.user
            })
        }) {
            if let Some(owner) = entry.pending_owner_other_than(txid) {
                return Err(sql_err!(
                    sqlstate::INTERNAL_LOCK_WAIT,
                    "waiting for transaction {} to finish changing user mapping",
                    owner
                ));
            }
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "user mapping already exists for server"
            ));
        }
        Self::create_entry(
            &mut self.mappings,
            database,
            created_at,
            definition,
            Ownership::BOOTSTRAP,
            txid,
            "user mappings",
        )
    }

    pub(crate) fn create_table(
        &mut self,
        database: DatabaseOid,
        created_at: u64,
        definition: ForeignTableDefinition,
        ownership: Ownership,
        txid: u32,
    ) -> Result<usize, SqlError> {
        if let Some(entry) = self.tables.iter().find(|entry| {
            Self::entry_blocks_create(entry, database, txid, |candidate| {
                candidate.table == definition.table
            })
        }) {
            if let Some(owner) = entry.pending_owner_other_than(txid) {
                return Err(sql_err!(
                    sqlstate::INTERNAL_LOCK_WAIT,
                    "waiting for transaction {} to finish changing foreign table binding",
                    owner
                ));
            }
            return Err(sql_err!(
                sqlstate::DUPLICATE_TABLE,
                "foreign table binding already exists"
            ));
        }
        Self::create_entry(
            &mut self.tables,
            database,
            created_at,
            definition,
            ownership,
            txid,
            "foreign tables",
        )
    }

    fn create_entry<D: Copy>(
        entries: &mut FixedVec<ForeignCatalogEntry<D>>,
        database: DatabaseOid,
        created_at: u64,
        definition: D,
        ownership: Ownership,
        txid: u32,
        noun: &'static str,
    ) -> Result<usize, SqlError> {
        CatalogIdentity::require(created_at)?;
        if entries.iter().any(|entry| {
            entry.database == database
                && entry.ddl_state != CatalogDdlState::Absent
                && entry.created_at == created_at
        }) {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "foreign catalog creation identity is duplicated"
            ));
        }

        let slot = entries
            .iter()
            .position(|entry| entry.ddl_state == CatalogDdlState::Absent)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many {} (limit {})",
                    noun,
                    entries.len()
                )
            })?;
        entries[slot] = ForeignCatalogEntry {
            database,
            created_at,
            definition,
            pending: None,
            ownership,
            ddl_state: CatalogDdlState::PendingCreate { txid },
        };
        Ok(slot)
    }

    fn restore_entry<D: Copy>(
        entries: &mut FixedVec<ForeignCatalogEntry<D>>,
        slot: usize,
        database: DatabaseOid,
        created_at: u64,
        definition: D,
        ownership: Ownership,
    ) -> Result<(), SqlError> {
        CatalogIdentity::require(created_at)?;
        if entries.iter().enumerate().any(|(other, entry)| {
            other != slot
                && entry.database == database
                && entry.ddl_state != CatalogDdlState::Absent
                && entry.created_at == created_at
        }) {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "foreign catalog creation identity is duplicated"
            ));
        }

        let Some(entry) = entries.get_mut(slot) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "foreign catalog slot is out of range"
            ));
        };
        if entry.ddl_state != CatalogDdlState::Absent {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "foreign catalog slot is already used"
            ));
        }
        *entry = ForeignCatalogEntry {
            database,
            created_at,
            definition,
            pending: None,
            ownership,
            ddl_state: CatalogDdlState::Present,
        };
        Ok(())
    }

    pub(crate) fn restore_wrapper(
        &mut self,
        slot: usize,
        database: DatabaseOid,
        created_at: u64,
        definition: ForeignDataWrapperDefinition,
        owner: u16,
    ) -> Result<(), SqlError> {
        Self::restore_entry(
            &mut self.wrappers,
            slot,
            database,
            created_at,
            definition,
            Ownership {
                owner,
                pending: None,
            },
        )
    }

    pub(crate) fn restore_server(
        &mut self,
        slot: usize,
        database: DatabaseOid,
        created_at: u64,
        definition: ForeignServerDefinition,
        owner: u16,
    ) -> Result<(), SqlError> {
        Self::restore_entry(
            &mut self.servers,
            slot,
            database,
            created_at,
            definition,
            Ownership {
                owner,
                pending: None,
            },
        )
    }

    pub(crate) fn restore_mapping(
        &mut self,
        slot: usize,
        database: DatabaseOid,
        created_at: u64,
        definition: UserMappingDefinition,
    ) -> Result<(), SqlError> {
        Self::restore_entry(
            &mut self.mappings,
            slot,
            database,
            created_at,
            definition,
            Ownership::BOOTSTRAP,
        )
    }

    pub(crate) fn restore_table(
        &mut self,
        slot: usize,
        database: DatabaseOid,
        created_at: u64,
        definition: ForeignTableDefinition,
    ) -> Result<(), SqlError> {
        Self::restore_entry(
            &mut self.tables,
            slot,
            database,
            created_at,
            definition,
            Ownership::BOOTSTRAP,
        )
    }

    fn replay_set_entry<D: Copy>(
        entries: &mut FixedVec<ForeignCatalogEntry<D>>,
        slot: usize,
        database: DatabaseOid,
        created_at: u64,
        definition: Option<D>,
        ownership: Ownership,
        empty: D,
    ) -> Result<(), SqlError> {
        if definition.is_some() {
            CatalogIdentity::require(created_at)?;
        }
        if definition.is_some()
            && entries.iter().enumerate().any(|(other, entry)| {
                other != slot
                    && entry.database == database
                    && entry.ddl_state != CatalogDdlState::Absent
                    && entry.created_at == created_at
            })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "foreign catalog creation identity is duplicated"
            ));
        }

        let Some(entry) = entries.get_mut(slot) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "foreign catalog slot is out of range"
            ));
        };
        match definition {
            Some(definition) => {
                *entry = ForeignCatalogEntry {
                    database,
                    created_at,
                    definition,
                    pending: None,
                    ownership,
                    ddl_state: CatalogDdlState::Present,
                };
            }
            None => {
                *entry = ForeignCatalogEntry::empty(empty);
            }
        }
        Ok(())
    }

    fn stage_prepared_entry<D: Copy + PartialEq>(
        entries: &mut FixedVec<ForeignCatalogEntry<D>>,
        slot: usize,
        image: PreparedForeignImage<D>,
        class: ForeignObjectClass,
        key: impl Fn(D) -> PreparedForeignKey,
        namespace: &[PreparedForeignNamespace],
    ) -> Result<PreparedForeignChange<D>, SqlError> {
        CatalogIdentity::require(image.created_at)?;
        if let Some(definition) = image.definition
            && entries.iter().enumerate().any(|(other, entry)| {
                if other == slot
                    || entry.database != image.database
                    || entry.ddl_state == CatalogDdlState::Absent
                {
                    return false;
                }
                if entry.pending_owner_other_than(image.transaction).is_some() {
                    key(entry.definition) == key(definition)
                        || entry
                            .pending
                            .is_some_and(|pending| key(pending.definition) == key(definition))
                } else if let Some(projected) = namespace
                    .iter()
                    .find(|projected| projected.class == class && projected.slot == other)
                {
                    projected.key == Some(key(definition))
                } else {
                    entry.visible_to(image.transaction)
                        && key(entry.definition_for(image.transaction)) == key(definition)
                }
            })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared foreign catalog key is already reserved"
            ));
        }
        if image.definition.is_some()
            && entries.iter().enumerate().any(|(other, entry)| {
                other != slot
                    && entry.database == image.database
                    && entry.ddl_state != CatalogDdlState::Absent
                    && entry.created_at == image.created_at
            })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared foreign catalog creation identity is duplicated"
            ));
        }
        let entry = entries.get_mut(slot).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "prepared foreign catalog slot is out of range"
            )
        })?;
        if entry.ddl_state == CatalogDdlState::Absent {
            let Some(definition) = image.definition else {
                return Ok(PreparedForeignChange::Unchanged);
            };
            *entry = ForeignCatalogEntry {
                database: image.database,
                created_at: image.created_at,
                definition,
                pending: None,
                ownership: image.owner.map_or(Ownership::BOOTSTRAP, |owner| Ownership {
                    owner,
                    pending: None,
                }),
                ddl_state: CatalogDdlState::PendingCreate {
                    txid: image.transaction,
                },
            };
            return Ok(PreparedForeignChange::Created);
        }
        if entry.database != image.database
            || entry.created_at != image.created_at
            || entry.pending_owner_other_than(image.transaction).is_some()
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared foreign catalog image selects another lifetime or owner"
            ));
        }
        let Some(definition) = image.definition else {
            entry.ddl_state = entry.ddl_state.drop_by(image.transaction);
            return Ok(PreparedForeignChange::Dropped);
        };
        let prior_definition = (entry.definition_for(image.transaction) != definition).then(|| {
            let prior = entry.pending;
            entry.pending = Some(PendingForeignDefinition {
                txid: image.transaction,
                definition,
            });
            prior
        });
        let prior_ownership = image
            .owner
            .filter(|owner| *owner != entry.ownership.owner_to(image.transaction))
            .map(|owner| {
                let prior = entry.ownership.pending;
                entry.ownership.pending = Some(PendingOwnership {
                    txid: image.transaction,
                    owner,
                });
                prior
            });
        if prior_definition.is_none() && prior_ownership.is_none() {
            Ok(PreparedForeignChange::Unchanged)
        } else {
            Ok(PreparedForeignChange::Changed {
                definition: prior_definition,
                ownership: prior_ownership,
            })
        }
    }

    pub(crate) fn stage_prepared_wrapper(
        &mut self,
        slot: usize,
        image: PreparedForeignImage<ForeignDataWrapperDefinition>,
        namespace: &[PreparedForeignNamespace],
    ) -> Result<PreparedForeignChange<ForeignDataWrapperDefinition>, SqlError> {
        if image.definition.is_some() && image.owner.is_none() {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared foreign wrapper owner is missing"
            ));
        }
        Self::stage_prepared_entry(
            &mut self.wrappers,
            slot,
            image,
            ForeignObjectClass::Wrapper,
            |definition| PreparedForeignKey::Wrapper(definition.name),
            namespace,
        )
    }

    pub(crate) fn stage_prepared_server(
        &mut self,
        slot: usize,
        image: PreparedForeignImage<ForeignServerDefinition>,
        namespace: &[PreparedForeignNamespace],
    ) -> Result<PreparedForeignChange<ForeignServerDefinition>, SqlError> {
        if let Some(definition) = image.definition {
            if image.owner.is_none() {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "prepared foreign server owner is missing"
                ));
            }
            Self::validate_prepared_parent(
                &self.wrappers,
                usize::from(definition.wrapper),
                image.database,
                image.transaction,
            )?;
        }
        Self::stage_prepared_entry(
            &mut self.servers,
            slot,
            image,
            ForeignObjectClass::Server,
            |definition| PreparedForeignKey::Server(definition.name),
            namespace,
        )
    }

    pub(crate) fn stage_prepared_mapping(
        &mut self,
        slot: usize,
        image: PreparedForeignImage<UserMappingDefinition>,
        namespace: &[PreparedForeignNamespace],
    ) -> Result<PreparedForeignChange<UserMappingDefinition>, SqlError> {
        if let Some(definition) = image.definition {
            Self::validate_prepared_parent(
                &self.servers,
                usize::from(definition.server),
                image.database,
                image.transaction,
            )?;
        }
        Self::stage_prepared_entry(
            &mut self.mappings,
            slot,
            image,
            ForeignObjectClass::Mapping,
            |definition| PreparedForeignKey::Mapping {
                server: definition.server,
                user: definition.user,
            },
            namespace,
        )
    }

    pub(crate) fn stage_prepared_table(
        &mut self,
        slot: usize,
        image: PreparedForeignImage<ForeignTableDefinition>,
        namespace: &[PreparedForeignNamespace],
    ) -> Result<PreparedForeignChange<ForeignTableDefinition>, SqlError> {
        if let Some(definition) = image.definition {
            Self::validate_prepared_parent(
                &self.servers,
                usize::from(definition.server),
                image.database,
                image.transaction,
            )?;
        }
        Self::stage_prepared_entry(
            &mut self.tables,
            slot,
            image,
            ForeignObjectClass::Table,
            |definition| PreparedForeignKey::Table(definition.table),
            namespace,
        )
    }

    fn validate_prepared_parent<D: Copy>(
        entries: &FixedVec<ForeignCatalogEntry<D>>,
        slot: usize,
        database: DatabaseOid,
        transaction: u32,
    ) -> Result<(), SqlError> {
        if entries.get(slot).is_some_and(|parent| {
            parent.database == database
                && parent.visible_to(transaction)
                && parent.pending_owner_other_than(transaction).is_none()
        }) {
            Ok(())
        } else {
            Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "prepared foreign catalog parent is unavailable"
            ))
        }
    }

    pub(crate) fn replay_set_wrapper(
        &mut self,
        slot: usize,
        database: DatabaseOid,
        created_at: u64,
        owner: u16,
        definition: Option<ForeignDataWrapperDefinition>,
    ) -> Result<(), SqlError> {
        Self::replay_set_entry(
            &mut self.wrappers,
            slot,
            database,
            created_at,
            definition,
            Ownership {
                owner,
                pending: None,
            },
            ForeignDataWrapperDefinition::EMPTY,
        )
    }

    pub(crate) fn replay_set_server(
        &mut self,
        slot: usize,
        database: DatabaseOid,
        created_at: u64,
        owner: u16,
        definition: Option<ForeignServerDefinition>,
    ) -> Result<(), SqlError> {
        Self::replay_set_entry(
            &mut self.servers,
            slot,
            database,
            created_at,
            definition,
            Ownership {
                owner,
                pending: None,
            },
            ForeignServerDefinition::EMPTY,
        )
    }

    pub(crate) fn replay_set_mapping(
        &mut self,
        slot: usize,
        database: DatabaseOid,
        created_at: u64,
        definition: Option<UserMappingDefinition>,
    ) -> Result<(), SqlError> {
        Self::replay_set_entry(
            &mut self.mappings,
            slot,
            database,
            created_at,
            definition,
            Ownership::BOOTSTRAP,
            UserMappingDefinition::EMPTY,
        )
    }

    pub(crate) fn replay_set_table(
        &mut self,
        slot: usize,
        database: DatabaseOid,
        created_at: u64,
        definition: Option<ForeignTableDefinition>,
    ) -> Result<(), SqlError> {
        Self::replay_set_entry(
            &mut self.tables,
            slot,
            database,
            created_at,
            definition,
            Ownership::BOOTSTRAP,
            ForeignTableDefinition::EMPTY,
        )
    }

    pub(crate) fn alter_wrapper(
        &mut self,
        slot: usize,
        definition: ForeignDataWrapperDefinition,
        txid: u32,
    ) -> Result<Option<PendingForeignDefinition<ForeignDataWrapperDefinition>>, SqlError> {
        Self::alter_entry(
            &mut self.wrappers,
            slot,
            definition,
            txid,
            "foreign-data wrapper",
        )
    }

    pub(crate) fn alter_server(
        &mut self,
        slot: usize,
        definition: ForeignServerDefinition,
        txid: u32,
    ) -> Result<Option<PendingForeignDefinition<ForeignServerDefinition>>, SqlError> {
        Self::alter_entry(&mut self.servers, slot, definition, txid, "foreign server")
    }

    pub(crate) fn alter_mapping(
        &mut self,
        slot: usize,
        definition: UserMappingDefinition,
        txid: u32,
    ) -> Result<Option<PendingForeignDefinition<UserMappingDefinition>>, SqlError> {
        Self::alter_entry(&mut self.mappings, slot, definition, txid, "user mapping")
    }

    pub(crate) fn alter_table(
        &mut self,
        slot: usize,
        definition: ForeignTableDefinition,
        txid: u32,
    ) -> Result<Option<PendingForeignDefinition<ForeignTableDefinition>>, SqlError> {
        Self::alter_entry(&mut self.tables, slot, definition, txid, "foreign table")
    }

    fn alter_entry<D: Copy>(
        entries: &mut FixedVec<ForeignCatalogEntry<D>>,
        slot: usize,
        definition: D,
        txid: u32,
        noun: &'static str,
    ) -> Result<Option<PendingForeignDefinition<D>>, SqlError> {
        if let Some(owner) = entries[slot].pending_owner_other_than(txid) {
            return Err(sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "waiting for transaction {} to finish changing {}",
                owner,
                noun,
            ));
        }
        let prior = entries[slot].pending;
        entries[slot].pending = Some(PendingForeignDefinition { txid, definition });
        Ok(prior)
    }

    pub(crate) fn commit_create(&mut self, class: ForeignObjectClass, slot: usize) {
        let state = self.entry(class, slot).ddl_state();
        let mut entry = self.entry_mut(class, slot);
        *entry.ddl_state() = state.commit_create();
        let ownership = (*entry.ownership()).committed();
        *entry.ownership() = ownership;
    }

    pub(crate) fn rollback_create(&mut self, class: ForeignObjectClass, slot: usize) {
        match class {
            ForeignObjectClass::Wrapper => {
                self.wrappers[slot] =
                    ForeignCatalogEntry::empty(ForeignDataWrapperDefinition::EMPTY)
            }
            ForeignObjectClass::Server => {
                self.servers[slot] = ForeignCatalogEntry::empty(ForeignServerDefinition::EMPTY)
            }
            ForeignObjectClass::Mapping => {
                self.mappings[slot] = ForeignCatalogEntry::empty(UserMappingDefinition::EMPTY)
            }
            ForeignObjectClass::Table => {
                self.tables[slot] = ForeignCatalogEntry::empty(ForeignTableDefinition::EMPTY)
            }
        }
    }

    pub(crate) fn stage_drop(&mut self, class: ForeignObjectClass, slot: usize, txid: u32) {
        let state = self.entry(class, slot).ddl_state();
        *self.entry_mut(class, slot).ddl_state() = state.drop_by(txid);
    }

    pub(crate) fn commit_drop(&mut self, class: ForeignObjectClass, slot: usize) {
        match class {
            ForeignObjectClass::Wrapper => {
                self.wrappers[slot] =
                    ForeignCatalogEntry::empty(ForeignDataWrapperDefinition::EMPTY)
            }
            ForeignObjectClass::Server => {
                self.servers[slot] = ForeignCatalogEntry::empty(ForeignServerDefinition::EMPTY)
            }
            ForeignObjectClass::Mapping => {
                self.mappings[slot] = ForeignCatalogEntry::empty(UserMappingDefinition::EMPTY)
            }
            ForeignObjectClass::Table => {
                self.tables[slot] = ForeignCatalogEntry::empty(ForeignTableDefinition::EMPTY)
            }
        }
    }

    pub(crate) fn rollback_drop(&mut self, class: ForeignObjectClass, slot: usize, txid: u32) {
        let state = self.entry(class, slot).ddl_state();
        *self.entry_mut(class, slot).ddl_state() = state.rollback_drop(txid);
    }

    pub(crate) fn commit_alter(&mut self, class: ForeignObjectClass, slot: usize, txid: u32) {
        match class {
            ForeignObjectClass::Wrapper => Self::commit_entry_alter(&mut self.wrappers[slot], txid),
            ForeignObjectClass::Server => Self::commit_entry_alter(&mut self.servers[slot], txid),
            ForeignObjectClass::Mapping => Self::commit_entry_alter(&mut self.mappings[slot], txid),
            ForeignObjectClass::Table => Self::commit_entry_alter(&mut self.tables[slot], txid),
        }
    }

    fn commit_entry_alter<D: Copy>(entry: &mut ForeignCatalogEntry<D>, txid: u32) {
        if let Some(pending) = entry.pending.filter(|pending| pending.txid == txid) {
            entry.definition = pending.definition;
            entry.pending = None;
        }
    }

    pub(crate) fn rollback_wrapper_alter(
        &mut self,
        slot: usize,
        prior: Option<PendingForeignDefinition<ForeignDataWrapperDefinition>>,
    ) {
        self.wrappers[slot].pending = prior;
    }

    pub(crate) fn rollback_server_alter(
        &mut self,
        slot: usize,
        prior: Option<PendingForeignDefinition<ForeignServerDefinition>>,
    ) {
        self.servers[slot].pending = prior;
    }

    pub(crate) fn rollback_mapping_alter(
        &mut self,
        slot: usize,
        prior: Option<PendingForeignDefinition<UserMappingDefinition>>,
    ) {
        self.mappings[slot].pending = prior;
    }

    pub(crate) fn rollback_table_alter(
        &mut self,
        slot: usize,
        prior: Option<PendingForeignDefinition<ForeignTableDefinition>>,
    ) {
        self.tables[slot].pending = prior;
    }

    pub(crate) fn owner_to(&self, class: ForeignObjectClass, slot: usize, txid: u32) -> u16 {
        match self.entry(class, slot) {
            ForeignEntryRef::Wrapper(entry) => entry.ownership.owner_to(txid),
            ForeignEntryRef::Server(entry) => entry.ownership.owner_to(txid),
            ForeignEntryRef::Mapping(entry) => entry.ownership.owner_to(txid),
            ForeignEntryRef::Table(entry) => entry.ownership.owner_to(txid),
        }
    }

    pub(crate) fn stage_owner(
        &mut self,
        class: ForeignObjectClass,
        slot: usize,
        owner: u16,
        txid: u32,
    ) -> Result<Option<PendingOwnership>, SqlError> {
        if let Some(blocker) = self.entry(class, slot).pending_owner_other_than(txid) {
            return Err(sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "waiting for transaction {} to finish changing foreign catalog object",
                blocker,
            ));
        }
        let ownership = match self.entry_mut(class, slot) {
            ForeignEntryMut::Wrapper(entry) => &mut entry.ownership,
            ForeignEntryMut::Server(entry) => &mut entry.ownership,
            ForeignEntryMut::Mapping(entry) => &mut entry.ownership,
            ForeignEntryMut::Table(entry) => &mut entry.ownership,
        };
        let prior = ownership.pending;
        ownership.pending = Some(PendingOwnership { txid, owner });
        Ok(prior)
    }

    pub(crate) fn commit_owner(&mut self, class: ForeignObjectClass, slot: usize, txid: u32) {
        let ownership = match self.entry_mut(class, slot) {
            ForeignEntryMut::Wrapper(entry) => &mut entry.ownership,
            ForeignEntryMut::Server(entry) => &mut entry.ownership,
            ForeignEntryMut::Mapping(entry) => &mut entry.ownership,
            ForeignEntryMut::Table(entry) => &mut entry.ownership,
        };
        if let Some(pending) = ownership.pending.filter(|pending| pending.txid == txid) {
            ownership.owner = pending.owner;
            ownership.pending = None;
        }
    }

    pub(crate) fn rollback_owner(
        &mut self,
        class: ForeignObjectClass,
        slot: usize,
        prior: Option<PendingOwnership>,
    ) {
        match self.entry_mut(class, slot) {
            ForeignEntryMut::Wrapper(entry) => entry.ownership.pending = prior,
            ForeignEntryMut::Server(entry) => entry.ownership.pending = prior,
            ForeignEntryMut::Mapping(entry) => entry.ownership.pending = prior,
            ForeignEntryMut::Table(entry) => entry.ownership.pending = prior,
        }
    }

    pub(crate) fn servers(
        &self,
        database: DatabaseOid,
        txid: u32,
    ) -> impl Iterator<Item = (usize, &ForeignCatalogEntry<ForeignServerDefinition>)> {
        self.servers
            .iter()
            .enumerate()
            .filter(move |(_, entry)| entry.database == database && entry.visible_to(txid))
    }

    pub(crate) fn mappings(
        &self,
        database: DatabaseOid,
        txid: u32,
    ) -> impl Iterator<Item = (usize, &ForeignCatalogEntry<UserMappingDefinition>)> {
        self.mappings
            .iter()
            .enumerate()
            .filter(move |(_, entry)| entry.database == database && entry.visible_to(txid))
    }

    pub(crate) fn tables(
        &self,
        database: DatabaseOid,
        txid: u32,
    ) -> impl Iterator<Item = (usize, &ForeignCatalogEntry<ForeignTableDefinition>)> {
        self.tables
            .iter()
            .enumerate()
            .filter(move |(_, entry)| entry.database == database && entry.visible_to(txid))
    }

    pub(crate) fn checkpoint_wrapper(
        &self,
        slot: usize,
    ) -> ForeignCatalogEntry<ForeignDataWrapperDefinition> {
        self.wrappers[slot]
    }

    pub(crate) fn checkpoint_server(
        &self,
        slot: usize,
    ) -> ForeignCatalogEntry<ForeignServerDefinition> {
        self.servers[slot]
    }

    pub(crate) fn entry_wrapper(
        &self,
        slot: usize,
    ) -> ForeignCatalogEntry<ForeignDataWrapperDefinition> {
        self.wrappers[slot]
    }

    pub(crate) fn entry_server(&self, slot: usize) -> ForeignCatalogEntry<ForeignServerDefinition> {
        self.servers[slot]
    }

    pub(crate) fn entry_mapping(&self, slot: usize) -> ForeignCatalogEntry<UserMappingDefinition> {
        self.mappings[slot]
    }

    pub(crate) fn entry_table(&self, slot: usize) -> ForeignCatalogEntry<ForeignTableDefinition> {
        self.tables[slot]
    }

    pub(crate) fn first_server_for_wrapper(
        &self,
        database: DatabaseOid,
        wrapper: u16,
        txid: u32,
    ) -> Option<(usize, ForeignServerDefinition)> {
        self.servers(database, txid).find_map(|(slot, entry)| {
            let definition = entry.definition_for(txid);
            (definition.wrapper == wrapper).then_some((slot, definition))
        })
    }

    pub(crate) fn first_mapping_for_server(
        &self,
        database: DatabaseOid,
        server: u16,
        txid: u32,
    ) -> Option<(usize, UserMappingDefinition)> {
        self.mappings(database, txid).find_map(|(slot, entry)| {
            let definition = entry.definition_for(txid);
            (definition.server == server).then_some((slot, definition))
        })
    }

    pub(crate) fn first_table_for_server(
        &self,
        database: DatabaseOid,
        server: u16,
        txid: u32,
    ) -> Option<(usize, ForeignTableDefinition)> {
        self.tables(database, txid).find_map(|(slot, entry)| {
            let definition = entry.definition_for(txid);
            (definition.server == server).then_some((slot, definition))
        })
    }

    pub(crate) fn has_table_for_wrapper(
        &self,
        database: DatabaseOid,
        wrapper: u16,
        txid: u32,
    ) -> bool {
        self.servers(database, txid).any(|(slot, entry)| {
            entry.definition_for(txid).wrapper == wrapper
                && self
                    .first_table_for_server(database, slot as u16, txid)
                    .is_some()
        })
    }

    fn entry(&self, class: ForeignObjectClass, slot: usize) -> ForeignEntryRef<'_> {
        match class {
            ForeignObjectClass::Wrapper => ForeignEntryRef::Wrapper(&self.wrappers[slot]),
            ForeignObjectClass::Server => ForeignEntryRef::Server(&self.servers[slot]),
            ForeignObjectClass::Mapping => ForeignEntryRef::Mapping(&self.mappings[slot]),
            ForeignObjectClass::Table => ForeignEntryRef::Table(&self.tables[slot]),
        }
    }

    fn entry_mut(&mut self, class: ForeignObjectClass, slot: usize) -> ForeignEntryMut<'_> {
        match class {
            ForeignObjectClass::Wrapper => ForeignEntryMut::Wrapper(&mut self.wrappers[slot]),
            ForeignObjectClass::Server => ForeignEntryMut::Server(&mut self.servers[slot]),
            ForeignObjectClass::Mapping => ForeignEntryMut::Mapping(&mut self.mappings[slot]),
            ForeignObjectClass::Table => ForeignEntryMut::Table(&mut self.tables[slot]),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ForeignObjectClass {
    Wrapper,
    Server,
    Mapping,
    Table,
}

enum ForeignEntryRef<'a> {
    Wrapper(&'a ForeignCatalogEntry<ForeignDataWrapperDefinition>),
    Server(&'a ForeignCatalogEntry<ForeignServerDefinition>),
    Mapping(&'a ForeignCatalogEntry<UserMappingDefinition>),
    Table(&'a ForeignCatalogEntry<ForeignTableDefinition>),
}

impl ForeignEntryRef<'_> {
    fn ddl_state(&self) -> CatalogDdlState {
        match self {
            Self::Wrapper(entry) => entry.ddl_state,
            Self::Server(entry) => entry.ddl_state,
            Self::Mapping(entry) => entry.ddl_state,
            Self::Table(entry) => entry.ddl_state,
        }
    }

    fn pending_owner_other_than(&self, txid: u32) -> Option<u32> {
        match self {
            Self::Wrapper(entry) => entry.pending_owner_other_than(txid),
            Self::Server(entry) => entry.pending_owner_other_than(txid),
            Self::Mapping(entry) => entry.pending_owner_other_than(txid),
            Self::Table(entry) => entry.pending_owner_other_than(txid),
        }
    }
}

enum ForeignEntryMut<'a> {
    Wrapper(&'a mut ForeignCatalogEntry<ForeignDataWrapperDefinition>),
    Server(&'a mut ForeignCatalogEntry<ForeignServerDefinition>),
    Mapping(&'a mut ForeignCatalogEntry<UserMappingDefinition>),
    Table(&'a mut ForeignCatalogEntry<ForeignTableDefinition>),
}

impl ForeignEntryMut<'_> {
    fn ddl_state(&mut self) -> &mut CatalogDdlState {
        match self {
            Self::Wrapper(entry) => &mut entry.ddl_state,
            Self::Server(entry) => &mut entry.ddl_state,
            Self::Mapping(entry) => &mut entry.ddl_state,
            Self::Table(entry) => &mut entry.ddl_state,
        }
    }

    fn ownership(&mut self) -> &mut Ownership {
        match self {
            Self::Wrapper(entry) => &mut entry.ownership,
            Self::Server(entry) => &mut entry.ownership,
            Self::Mapping(entry) => &mut entry.ownership,
            Self::Table(entry) => &mut entry.ownership,
        }
    }
}

impl ForeignCatalog {
    pub(super) fn clone_database(
        &mut self,
        source: DatabaseOid,
        target: DatabaseOid,
        allocation: super::DatabaseCloneAllocation<'_>,
        transaction: u32,
        table_slot: impl Fn(u16) -> Result<u16, SqlError>,
    ) -> Result<(), SqlError> {
        Self::clone_entries(
            &mut self.wrappers,
            source,
            target,
            allocation,
            super::DatabaseCloneKind::ForeignWrapper,
            transaction,
            Ok,
        )?;
        let wrappers = &self.wrappers;
        Self::clone_entries(
            &mut self.servers,
            source,
            target,
            allocation,
            super::DatabaseCloneKind::ForeignServer,
            transaction,
            |mut definition| {
                definition.wrapper =
                    Self::cloned_parent(wrappers, definition.wrapper, source, target)?;
                Ok(definition)
            },
        )?;
        let servers = &self.servers;
        Self::clone_entries(
            &mut self.mappings,
            source,
            target,
            allocation,
            super::DatabaseCloneKind::UserMapping,
            transaction,
            |mut definition| {
                definition.server =
                    Self::cloned_parent(servers, definition.server, source, target)?;
                Ok(definition)
            },
        )?;
        Self::clone_entries(
            &mut self.tables,
            source,
            target,
            allocation,
            super::DatabaseCloneKind::ForeignTable,
            transaction,
            |mut definition| {
                definition.table = table_slot(definition.table)?;
                definition.server =
                    Self::cloned_parent(servers, definition.server, source, target)?;
                Ok(definition)
            },
        )
    }

    fn cloned_parent<D: Copy>(
        entries: &[ForeignCatalogEntry<D>],
        source_slot: u16,
        source: DatabaseOid,
        target: DatabaseOid,
    ) -> Result<u16, SqlError> {
        let invalid = || {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "template foreign catalog parent was not cloned"
            )
        };
        let parent = entries
            .get(usize::from(source_slot))
            .filter(|entry| entry.database == source && entry.ddl_state == CatalogDdlState::Present)
            .ok_or_else(invalid)?;
        entries
            .iter()
            .position(|entry| {
                entry.database == target
                    && entry.ddl_state != CatalogDdlState::Absent
                    && entry.created_at == parent.created_at
            })
            .and_then(|slot| u16::try_from(slot).ok())
            .ok_or_else(invalid)
    }

    fn clone_entries<D: Copy>(
        entries: &mut FixedVec<ForeignCatalogEntry<D>>,
        source: DatabaseOid,
        target: DatabaseOid,
        allocation: super::DatabaseCloneAllocation<'_>,
        kind: super::DatabaseCloneKind,
        transaction: u32,
        definition: impl Fn(D) -> Result<D, SqlError>,
    ) -> Result<(), SqlError> {
        for source_slot in 0..entries.len() {
            let mut entry = entries[source_slot];
            if entry.database != source || entry.ddl_state != CatalogDdlState::Present {
                continue;
            }
            CatalogIdentity::require(entry.created_at)?;
            let target_slot = allocation.select(
                kind,
                entry.created_at,
                entries.iter().enumerate().filter_map(|(slot, entry)| {
                    (entry.ddl_state == CatalogDdlState::Absent).then_some(slot)
                }),
            )?;
            entry.definition = definition(entry.definition)?;
            entry.database = target;
            entry.pending = None;
            entry.ownership = entry.ownership.committed();
            entry.ddl_state = CatalogDdlState::PendingCreate { txid: transaction };
            entries[target_slot] = entry;
        }
        Ok(())
    }

    pub(super) fn commit_database_create(&mut self, database: DatabaseOid, transaction: u32) {
        fn commit<D: Copy>(
            entries: &mut [ForeignCatalogEntry<D>],
            database: DatabaseOid,
            transaction: u32,
        ) {
            for entry in entries {
                if entry.database == database
                    && entry.ddl_state == (CatalogDdlState::PendingCreate { txid: transaction })
                {
                    entry.ddl_state = CatalogDdlState::Present;
                }
            }
        }
        commit(&mut self.wrappers, database, transaction);
        commit(&mut self.servers, database, transaction);
        commit(&mut self.mappings, database, transaction);
        commit(&mut self.tables, database, transaction);
    }

    pub(super) fn clear_database(&mut self, database: DatabaseOid) {
        fn clear<D: Copy>(entries: &mut [ForeignCatalogEntry<D>], database: DatabaseOid, empty: D) {
            for entry in entries {
                if entry.database == database {
                    *entry = ForeignCatalogEntry::empty(empty);
                }
            }
        }
        clear(
            &mut self.wrappers,
            database,
            ForeignDataWrapperDefinition::EMPTY,
        );
        clear(&mut self.servers, database, ForeignServerDefinition::EMPTY);
        clear(&mut self.mappings, database, UserMappingDefinition::EMPTY);
        clear(&mut self.tables, database, ForeignTableDefinition::EMPTY);
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod prepared_tests {
    use super::*;

    fn image<D: Copy>(
        definition: D,
        created_at: u64,
        transaction: u32,
        owner: Option<u16>,
    ) -> PreparedForeignImage<D> {
        PreparedForeignImage {
            database: DatabaseOid::POSTGRES,
            created_at,
            owner,
            definition: Some(definition),
            transaction,
        }
    }

    #[test]
    fn prepared_foreign_images_reject_reserved_keys_and_invalid_parents_before_mutation() {
        let mut config = Config::default_dev();
        config.max_foreign_data_wrappers = 3;
        config.max_foreign_servers = 3;
        config.max_user_mappings = 3;
        config.max_tables = 3;
        let mut budget = Budget::new(ForeignCatalog::budget_bytes(&config));
        let mut catalog = ForeignCatalog::new(&config, &mut budget).unwrap();
        let wrapper = ForeignDataWrapperDefinition {
            name: SqlName::parse("duplicate").unwrap(),
            ..ForeignDataWrapperDefinition::EMPTY
        };
        crate::mem::guard::forbid_alloc(|| {
            assert!(matches!(
                catalog
                    .stage_prepared_wrapper(0, image(wrapper, 41, 7, Some(0)), &[])
                    .unwrap(),
                PreparedForeignChange::Created
            ));
            let Err(error) = catalog.stage_prepared_wrapper(1, image(wrapper, 42, 9, Some(0)), &[])
            else {
                panic!("duplicate wrapper was accepted");
            };
            assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
            assert_eq!(catalog.wrappers[1].ddl_state, CatalogDdlState::Absent);
            catalog.commit_create(ForeignObjectClass::Wrapper, 0);
            let renamed = ForeignDataWrapperDefinition {
                name: SqlName::parse("renamed").unwrap(),
                ..wrapper
            };
            catalog.alter_wrapper(0, renamed, 7).unwrap();
            for name in [wrapper.name, renamed.name] {
                assert!(
                    catalog
                        .stage_prepared_wrapper(
                            1,
                            image(
                                ForeignDataWrapperDefinition { name, ..wrapper },
                                42,
                                9,
                                Some(0)
                            ),
                            &[]
                        )
                        .is_err()
                );
                assert_eq!(catalog.wrappers[1].ddl_state, CatalogDdlState::Absent);
            }
            catalog.commit_alter(ForeignObjectClass::Wrapper, 0, 7);
            let server = ForeignServerDefinition {
                name: SqlName::parse("server").unwrap(),
                wrapper: 0,
                ..ForeignServerDefinition::EMPTY
            };
            catalog
                .stage_prepared_server(0, image(server, 43, 7, Some(0)), &[])
                .unwrap();
            assert!(
                catalog
                    .stage_prepared_server(1, image(server, 44, 9, Some(0)), &[])
                    .is_err()
            );
            assert_eq!(catalog.servers[1].ddl_state, CatalogDdlState::Absent);
            let missing_parent = ForeignServerDefinition {
                name: SqlName::parse("bad_parent").unwrap(),
                wrapper: u16::MAX,
                ..server
            };
            assert!(
                catalog
                    .stage_prepared_server(1, image(missing_parent, 44, 9, Some(0)), &[])
                    .is_err()
            );
            assert_eq!(catalog.servers[1].ddl_state, CatalogDdlState::Absent);
            catalog.commit_create(ForeignObjectClass::Server, 0);
            let mapping = UserMappingDefinition {
                server: 0,
                user: ForeignMappingUser::Public,
                ..UserMappingDefinition::EMPTY
            };
            catalog
                .stage_prepared_mapping(0, image(mapping, 45, 7, None), &[])
                .unwrap();
            assert!(
                catalog
                    .stage_prepared_mapping(1, image(mapping, 46, 9, None), &[])
                    .is_err()
            );
            assert_eq!(catalog.mappings[1].ddl_state, CatalogDdlState::Absent);
            let table = ForeignTableDefinition {
                table: 1,
                server: 0,
                ..ForeignTableDefinition::EMPTY
            };
            catalog
                .stage_prepared_table(0, image(table, 47, 7, None), &[])
                .unwrap();
            assert!(
                catalog
                    .stage_prepared_table(1, image(table, 48, 9, None), &[])
                    .is_err()
            );
            assert_eq!(catalog.tables[1].ddl_state, CatalogDdlState::Absent);
            assert!(
                catalog
                    .stage_prepared_table(
                        1,
                        image(
                            ForeignTableDefinition {
                                table: 2,
                                server: u16::MAX,
                                ..table
                            },
                            48,
                            9,
                            None
                        ),
                        &[]
                    )
                    .is_err()
            );
            assert_eq!(catalog.tables[1].ddl_state, CatalogDdlState::Absent);
        });
    }
}
