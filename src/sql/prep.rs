//! SQL-level prepared statements (PREPARE / EXECUTE / DEALLOCATE).
//! A separate namespace from protocol-level prepared statements, as in
//! PostgreSQL. Fixed pool per connection.

use crate::config::Config;
use crate::mem::arena::Arena;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::sql::eval::sqlstate;
use crate::sql_err;
use crate::storage::SqlName;

use super::eval::SqlError;
use super::types::ColType;
use core::cell::Cell;

/// PostgreSQL's unsigned 16-bit prepared-parameter boundary.
pub const MAX_PREP_PARAMS: usize = u16::MAX as usize;
const MAX_PREP_RESULTS: usize = super::exec::MAX_PROJ;

fn type_from_code(code: u8) -> Option<ColType> {
    if code == ColType::Record.code() {
        Some(ColType::Record)
    } else {
        ColType::from_code(code)
    }
}

pub(crate) fn decode_types<'a>(codes: &[u8], arena: &'a Arena) -> Result<&'a [ColType], SqlError> {
    let types = arena
        .alloc_slice_with(codes.len(), |_| ColType::Bool)
        .map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "prepared parameter types exceed the statement arena"
            )
        })?;
    for (target, code) in types.iter_mut().zip(codes.iter().copied()) {
        *target = type_from_code(code).ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "prepared statement contains an invalid parameter type"
            )
        })?;
    }
    Ok(types)
}

pub struct SqlPreparedPool {
    slots: Vec<Slot>,
}

struct Slot {
    active: bool,
    name: SqlName,
    /// Query text followed by one durable type code per declared parameter.
    /// Both share the slot's configured `prepared_bytes` reservation.
    data: FixedBuf,
    text_len: usize,
    n_params: usize,
    prepared_at: i64,
    custom_plans: i64,
    result_types: [i32; MAX_PREP_RESULTS],
    n_results: usize,
}

thread_local! {
    static ACTIVE_PREPARED: Cell<*const SqlPreparedPool> = const { Cell::new(core::ptr::null()) };
}

pub(crate) struct ActivePreparedGuard(*const SqlPreparedPool);

impl Drop for ActivePreparedGuard {
    fn drop(&mut self) {
        ACTIVE_PREPARED.with(|active| active.set(self.0));
    }
}

pub(crate) fn enter_active(pool: *const SqlPreparedPool) -> ActivePreparedGuard {
    let prior = ACTIVE_PREPARED.with(|active| active.replace(pool));
    ActivePreparedGuard(prior)
}

pub(crate) fn with_active<R>(f: impl FnOnce(Option<&SqlPreparedPool>) -> R) -> R {
    ACTIVE_PREPARED.with(|active| {
        let pool = active.get();
        // SAFETY: statement execution owns the pool until the guard restores
        // the prior pointer, and catalog inspection only takes shared access.
        f((!pool.is_null()).then(|| unsafe { &*pool }))
    })
}

pub(crate) struct PreparedInfo<'a> {
    pub(crate) name: &'a str,
    pub(crate) statement: &'a str,
    pub(crate) parameter_type_codes: &'a [u8],
    pub(crate) prepared_at: i64,
    pub(crate) custom_plans: i64,
    pub(crate) result_types: &'a [i32],
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
                data: FixedBuf::new(budget, "sql_prepared_data", config.prepared_bytes)?,
                text_len: 0,
                n_params: 0,
                prepared_at: 0,
                custom_plans: 0,
                result_types: [0; MAX_PREP_RESULTS],
                n_results: 0,
            });
        }
        Ok(Self { slots })
    }

    pub fn store(
        &mut self,
        name: &str,
        sql: &str,
        param_types: &[ColType],
        result_types: &[i32],
    ) -> Result<(), SqlError> {
        if self.get(name).is_some() {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::DUPLICATE_PREPARED_STATEMENT,
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
        if result_types.len() > MAX_PREP_RESULTS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many prepared-statement result columns"
            ));
        }
        let Some(slot) = self.slots.iter_mut().find(|s| !s.active) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many prepared statements (max_prepared)"
            ));
        };
        slot.data.clear();
        if sql.len().saturating_add(param_types.len()) > slot.data.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "prepared statement text and parameter types exceed prepared_bytes"
            ));
        }
        assert!(slot.data.append(sql.as_bytes()), "capacity checked above");
        for ctype in param_types {
            assert!(slot.data.append(&[ctype.code()]), "capacity checked above");
        }
        slot.text_len = sql.len();
        slot.n_params = param_types.len();
        slot.result_types[..result_types.len()].copy_from_slice(result_types);
        slot.n_results = result_types.len();
        slot.name = SqlName::parse(name)?;
        slot.prepared_at = crate::sql::datetime::now_micros();
        slot.custom_plans = 0;
        slot.active = true;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.slots
            .iter()
            .find(|s| s.active && s.name.as_str() == name)
            .map(|s| {
                core::str::from_utf8(&s.data.readable()[..s.text_len])
                    .expect("stored from valid UTF-8")
            })
    }

    /// The declared `$n` parameter types for a prepared statement (empty slice
    /// when none were declared), or None if the statement does not exist.
    pub fn get_type_codes(&self, name: &str) -> Option<&[u8]> {
        self.slots
            .iter()
            .find(|s| s.active && s.name.as_str() == name)
            .map(|s| &s.data.readable()[s.text_len..s.text_len + s.n_params])
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

    pub(crate) fn len(&self) -> usize {
        self.slots.iter().filter(|slot| slot.active).count()
    }

    pub(crate) fn visit(&self, mut visitor: impl FnMut(PreparedInfo<'_>)) {
        for slot in self.slots.iter().filter(|slot| slot.active) {
            visitor(PreparedInfo {
                name: slot.name.as_str(),
                statement: core::str::from_utf8(&slot.data.readable()[..slot.text_len])
                    .expect("prepared SQL was validated as UTF-8"),
                parameter_type_codes: &slot.data.readable()
                    [slot.text_len..slot.text_len + slot.n_params],
                prepared_at: slot.prepared_at,
                custom_plans: slot.custom_plans,
                result_types: &slot.result_types[..slot.n_results],
            });
        }
    }

    pub(crate) fn record_custom_plan(&mut self, name: &str) {
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.active && slot.name.as_str() == name)
            .expect("prepared statement was resolved before execution");
        slot.custom_plans = slot.custom_plans.saturating_add(1);
    }
}
