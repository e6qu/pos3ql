//! Transactional foreign-data catalog state.

use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql::ast::{ForeignOption as ParsedOption, ForeignOptionAction};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::util::StackStr;

use super::{CatalogDdlState, DatabaseOid, Ownership, PendingOwnership, SqlName};

pub(crate) const MAX_FOREIGN_OPTIONS: usize = 16;
pub(crate) const MAX_FOREIGN_COLUMN_OPTIONS: usize = 32;
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
                            "option \"{}\" already exists",
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
pub(crate) enum ForeignDataHandler {
    None,
    Postgres,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ForeignDataValidator {
    None,
    Postgres,
}

#[derive(Clone, Copy, Debug)]
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
}

#[derive(Clone, Copy, Debug)]
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

#[derive(Clone, Copy, Debug)]
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

#[derive(Clone, Copy, Debug)]
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

#[derive(Clone, Copy, Debug)]
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

#[derive(Clone, Copy, Debug)]
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
    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        self.ddl_state.visible_to(txid)
    }

    pub(crate) fn definition_for(&self, txid: u32) -> D {
        self.pending
            .filter(|pending| pending.txid == txid)
            .map_or(self.definition, |pending| pending.definition)
    }
}

pub(crate) struct ForeignCatalog {
    wrappers: FixedVec<ForeignCatalogEntry<ForeignDataWrapperDefinition>>,
    servers: FixedVec<ForeignCatalogEntry<ForeignServerDefinition>>,
    mappings: FixedVec<ForeignCatalogEntry<UserMappingDefinition>>,
    tables: FixedVec<ForeignCatalogEntry<ForeignTableDefinition>>,
}

impl ForeignCatalog {
    pub(crate) fn wrapper_capacity(&self) -> usize {
        self.wrappers.len()
    }

    pub(crate) fn server_capacity(&self) -> usize {
        self.servers.len()
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
                .push(ForeignCatalogEntry {
                    database: DatabaseOid::POSTGRES,
                    created_at: 0,
                    definition: empty,
                    pending: None,
                    ownership: Ownership::BOOTSTRAP,
                    ddl_state: CatalogDdlState::Absent,
                })
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
        if self
            .wrapper(database, definition.name.as_str(), txid)
            .is_some()
        {
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
        if self
            .server(database, definition.name.as_str(), txid)
            .is_some()
        {
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
        if self
            .mapping(database, definition.server, definition.user, txid)
            .is_some()
        {
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
        if self.table(database, definition.table, txid).is_some() {
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
    ) -> Result<(), SqlError> {
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
                entry.pending = None;
                entry.ownership.pending = None;
                entry.ddl_state = CatalogDdlState::Absent;
            }
        }
        Ok(())
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
        let prior = entries[slot].pending;
        if prior.is_some_and(|pending| pending.txid != txid) {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "{} is being altered by another transaction",
                noun
            ));
        }
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
        let state = self.entry(class, slot).ddl_state();
        *self.entry_mut(class, slot).ddl_state() = state.rollback_create();
    }

    pub(crate) fn drop(&mut self, class: ForeignObjectClass, slot: usize, txid: u32) {
        let state = self.entry(class, slot).ddl_state();
        *self.entry_mut(class, slot).ddl_state() = state.drop_by(txid);
    }

    pub(crate) fn commit_drop(&mut self, class: ForeignObjectClass, slot: usize) {
        let state = self.entry(class, slot).ddl_state();
        let mut entry = self.entry_mut(class, slot);
        *entry.ddl_state() = state.commit_drop();
        entry.clear_pending();
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
    ) -> Option<PendingOwnership> {
        let ownership = match self.entry_mut(class, slot) {
            ForeignEntryMut::Wrapper(entry) => &mut entry.ownership,
            ForeignEntryMut::Server(entry) => &mut entry.ownership,
            ForeignEntryMut::Mapping(entry) => &mut entry.ownership,
            ForeignEntryMut::Table(entry) => &mut entry.ownership,
        };
        let prior = ownership.pending;
        ownership.pending = Some(PendingOwnership { txid, owner });
        prior
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

    pub(crate) fn wrappers(
        &self,
        database: DatabaseOid,
        txid: u32,
    ) -> impl Iterator<Item = (usize, &ForeignCatalogEntry<ForeignDataWrapperDefinition>)> {
        self.wrappers
            .iter()
            .enumerate()
            .filter(move |(_, entry)| entry.database == database && entry.visible_to(txid))
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

    pub(crate) fn checkpoint_wrappers(
        &self,
    ) -> impl Iterator<Item = (usize, &ForeignCatalogEntry<ForeignDataWrapperDefinition>)> {
        self.wrappers
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.ddl_state == CatalogDdlState::Present)
    }

    pub(crate) fn checkpoint_servers(
        &self,
    ) -> impl Iterator<Item = (usize, &ForeignCatalogEntry<ForeignServerDefinition>)> {
        self.servers
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.ddl_state == CatalogDdlState::Present)
    }

    pub(crate) fn checkpoint_mappings(
        &self,
    ) -> impl Iterator<Item = (usize, &ForeignCatalogEntry<UserMappingDefinition>)> {
        self.mappings
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.ddl_state == CatalogDdlState::Present)
    }

    pub(crate) fn checkpoint_tables(
        &self,
    ) -> impl Iterator<Item = (usize, &ForeignCatalogEntry<ForeignTableDefinition>)> {
        self.tables
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.ddl_state == CatalogDdlState::Present)
    }

    pub(crate) fn checkpoint_wrapper(
        &self,
        slot: usize,
    ) -> &ForeignCatalogEntry<ForeignDataWrapperDefinition> {
        &self.wrappers[slot]
    }

    pub(crate) fn checkpoint_server(
        &self,
        slot: usize,
    ) -> &ForeignCatalogEntry<ForeignServerDefinition> {
        &self.servers[slot]
    }

    pub(crate) fn entry_wrapper(
        &self,
        slot: usize,
    ) -> &ForeignCatalogEntry<ForeignDataWrapperDefinition> {
        &self.wrappers[slot]
    }

    pub(crate) fn entry_server(
        &self,
        slot: usize,
    ) -> &ForeignCatalogEntry<ForeignServerDefinition> {
        &self.servers[slot]
    }

    pub(crate) fn entry_wrapper_mut(
        &mut self,
        slot: usize,
    ) -> &mut ForeignCatalogEntry<ForeignDataWrapperDefinition> {
        &mut self.wrappers[slot]
    }

    pub(crate) fn entry_server_mut(
        &mut self,
        slot: usize,
    ) -> &mut ForeignCatalogEntry<ForeignServerDefinition> {
        &mut self.servers[slot]
    }

    pub(crate) fn entry_mapping(&self, slot: usize) -> &ForeignCatalogEntry<UserMappingDefinition> {
        &self.mappings[slot]
    }

    pub(crate) fn entry_table(&self, slot: usize) -> &ForeignCatalogEntry<ForeignTableDefinition> {
        &self.tables[slot]
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

    fn clear_pending(&mut self) {
        match self {
            Self::Wrapper(entry) => entry.pending = None,
            Self::Server(entry) => entry.pending = None,
            Self::Mapping(entry) => entry.pending = None,
            Self::Table(entry) => entry.pending = None,
        }
        self.ownership().pending = None;
    }
}
