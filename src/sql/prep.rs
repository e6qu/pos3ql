//! SQL-level prepared statements (PREPARE / EXECUTE / DEALLOCATE).
//! A separate namespace from protocol-level prepared statements, as in
//! PostgreSQL. Fixed pool per connection.

use crate::sql::eval::sqlstate;
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::sql_err;
use crate::storage::SqlName;

use super::eval::SqlError;
use super::types::ColType;

/// Upper bound on declared PREPARE parameter types (matches the parser's
/// per-list limit).
pub const MAX_PREP_PARAMS: usize = super::parser::MAX_LIST;

pub struct SqlPreparedPool {
    slots: Vec<Slot>,
}

struct Slot {
    active: bool,
    name: SqlName,
    text: FixedBuf,
    /// Declared `$n` types (`param_types[..n_params]`), used to coerce EXECUTE
    /// arguments. Empty when PREPARE declared none.
    param_types: [ColType; MAX_PREP_PARAMS],
    n_params: usize,
}

impl SqlPreparedPool {
    pub fn budget_bytes(config: &Config) -> usize {
        config.max_prepared * config.prepared_bytes
    }

    pub fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        let mut slots = Vec::with_capacity(config.max_prepared);
        for _ in 0..config.max_prepared {
            slots.push(Slot {
                active: false,
                name: SqlName::parse("").expect("empty fits"),
                text: FixedBuf::new(budget, "sql_prepared_text", config.prepared_bytes)?,
                param_types: [ColType::Bool; MAX_PREP_PARAMS],
                n_params: 0,
            });
        }
        Ok(Self { slots })
    }

    pub fn store(&mut self, name: &str, sql: &str, param_types: &[ColType]) -> Result<(), SqlError> {
        if self.get(name).is_some() {
            return Err(sql_err!(
                "42P05",
                "prepared statement \"{}\" already exists",
                name
            ));
        }
        if param_types.len() > MAX_PREP_PARAMS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many declared parameters (max {})",
                MAX_PREP_PARAMS
            ));
        }
        let Some(slot) = self.slots.iter_mut().find(|s| !s.active) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many prepared statements (max_prepared)"
            ));
        };
        slot.text.clear();
        if !slot.text.append(sql.as_bytes()) {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "prepared statement exceeds prepared_bytes"
            ));
        }
        slot.param_types[..param_types.len()].copy_from_slice(param_types);
        slot.n_params = param_types.len();
        slot.name = SqlName::parse(name)?;
        slot.active = true;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.slots
            .iter()
            .find(|s| s.active && s.name.as_str() == name)
            .map(|s| core::str::from_utf8(s.text.readable()).expect("stored from valid UTF-8"))
    }

    /// The declared `$n` parameter types for a prepared statement (empty slice
    /// when none were declared), or None if the statement does not exist.
    pub fn get_types(&self, name: &str) -> Option<&[ColType]> {
        self.slots
            .iter()
            .find(|s| s.active && s.name.as_str() == name)
            .map(|s| &s.param_types[..s.n_params])
    }

    /// Returns whether the statement existed.
    pub fn remove(&mut self, name: &str) -> bool {
        if let Some(s) = self
            .slots
            .iter_mut()
            .find(|s| s.active && s.name.as_str() == name)
        {
            s.active = false;
            true
        } else {
            false
        }
    }

    pub fn clear(&mut self) {
        for s in &mut self.slots {
            s.active = false;
        }
    }
}
