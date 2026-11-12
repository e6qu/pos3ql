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
use core::cell::Cell;

/// PostgreSQL's unsigned 16-bit prepared-parameter boundary.
pub const MAX_PREP_PARAMS: usize = u16::MAX as usize;
const MAX_PREP_RESULTS: usize = super::exec::MAX_PROJ;

pub struct SqlPreparedPool {
    slots: Vec<Slot>,
    request: FixedBuf,
    request_owner: Option<RequestInputOwner>,
    pub(crate) routine_inputs: super::type_input_plans::RoutineInputPlans,
    pub(crate) plan_invalidations:
        std::sync::Arc<super::plan_invalidations::LocalPlanInvalidations>,
    transient_plan: Option<u64>,
    next_plan: u64,
}

struct RequestInputOwner {
    source: super::type_inputs::Source,
    text_len: usize,
    progress: RequestInputProgress,
}

#[derive(Clone, Copy)]
pub(crate) struct RequestInputProgress {
    pub next: usize,
    pub waiting: Option<PendingTypeInput>,
    pub complete: bool,
    pub mark: super::txn::StatementMark,
    pub deadline: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingTypeInput {
    pub start: usize,
    pub end: usize,
    pub kind: super::type_inputs::InputKind,
    pub target: i32,
}

struct Slot {
    active: bool,
    name: SqlName,
    /// Query text, parameter type OIDs, result OIDs, then bound type inputs.
    /// All metadata shares the slot's configured `prepared_bytes` reservation.
    data: FixedBuf,
    text_len: usize,
    n_params: usize,
    prepared_at: i64,
    custom_plans: i64,
    n_results: usize,
    input_bytes: usize,
    plan: u64,
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
    pub(crate) parameter_types: PreparedTypeOids<'a>,
    pub(crate) prepared_at: i64,
    pub(crate) custom_plans: i64,
    pub(crate) result_types: PreparedTypeOids<'a>,
}

/// Type OIDs stored big-endian in a prepared statement's slot. Parameters
/// keep their exact type, including domains, enums and composites, as
/// PostgreSQL records them.
#[derive(Clone, Copy)]
pub(crate) struct PreparedTypeOids<'a> {
    bytes: &'a [u8],
}

impl PreparedTypeOids<'_> {
    pub(crate) fn len(self) -> usize {
        self.bytes.len() / 4
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = i32> {
        self.bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| i32::from_be_bytes(*bytes))
    }

    /// Copies the OIDs into the statement arena, ending the pool borrow.
    pub(crate) fn copy_into(self, arena: &Arena) -> Result<&[i32], SqlError> {
        let mut oids = self.iter();
        let copied = arena
            .alloc_slice_with(self.len(), |_| oids.next().expect("counted prepared OIDs"))
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "prepared parameter types exceed the statement arena"
                )
            })?;
        Ok(copied)
    }
}

impl SqlPreparedPool {
    pub fn budget_bytes(config: &Config) -> usize {
        config
            .max_prepared
            .saturating_add(1)
            .saturating_mul(config.prepared_bytes)
            .saturating_add(super::type_input_plans::RoutineInputPlans::budget_bytes(
                config,
            ))
            .saturating_add(
                super::plan_invalidations::LocalPlanInvalidations::budget_bytes(
                    config.routine_input_slots,
                ),
            )
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
                n_results: 0,
                input_bytes: 0,
                plan: 0,
            });
        }
        let request = FixedBuf::new(budget, "sql_request_type_inputs", config.prepared_bytes)?;
        let routine_inputs = super::type_input_plans::RoutineInputPlans::new(config, budget)?;
        Ok(Self {
            slots,
            request,
            request_owner: None,
            routine_inputs,
            plan_invalidations: super::plan_invalidations::LocalPlanInvalidations::new(
                config.routine_input_slots,
                budget,
            )?,
            transient_plan: None,
            next_plan: 1,
        })
    }

    pub fn store(
        &mut self,
        name: &str,
        sql: &str,
        param_types: &[i32],
        result_types: &[i32],
        type_inputs: &[u8],
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
        let plan = self.reserve_plan_ids(1)?;
        let Some(slot) = self.slots.iter_mut().find(|s| !s.active) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many prepared statements (max_prepared)"
            ));
        };
        slot.data.clear();
        let result_bytes = result_types.len().saturating_mul(4);
        if sql
            .len()
            .saturating_add(param_types.len().saturating_mul(4))
            .saturating_add(result_bytes)
            .saturating_add(type_inputs.len())
            > slot.data.capacity()
        {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "prepared statement text and type metadata exceed prepared_bytes"
            ));
        }
        assert!(slot.data.append(sql.as_bytes()), "capacity checked above");
        for type_oid in param_types.iter().chain(result_types) {
            assert!(
                slot.data.append(&type_oid.to_be_bytes()),
                "capacity checked above"
            );
        }
        assert!(slot.data.append(type_inputs), "capacity checked above");
        slot.text_len = sql.len();
        slot.n_params = param_types.len();
        slot.n_results = result_types.len();
        slot.input_bytes = type_inputs.len();
        slot.name = SqlName::parse(name)?;
        slot.prepared_at = crate::sql::datetime::now_micros();
        slot.custom_plans = 0;
        slot.active = true;
        slot.plan = plan;
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

    /// Every `$n` parameter type a prepared statement recorded at PREPARE,
    /// or None if the statement does not exist.
    pub(crate) fn parameter_types(&self, name: &str) -> Option<PreparedTypeOids<'_>> {
        self.slots
            .iter()
            .find(|s| s.active && s.name.as_str() == name)
            .map(|s| PreparedTypeOids {
                bytes: &s.data.readable()[s.text_len..s.text_len + s.n_params * 4],
            })
    }

    pub(crate) fn type_inputs(&self, name: &str) -> Option<&[u8]> {
        self.slots
            .iter()
            .find(|slot| slot.active && slot.name.as_str() == name)
            .map(|slot| {
                let start = slot.text_len + (slot.n_params + slot.n_results) * 4;
                &slot.data.readable()[start..start + slot.input_bytes]
            })
    }

    pub(crate) fn request_type_inputs(
        &self,
        source: super::type_inputs::Source,
        text: &str,
    ) -> Result<Option<(&[u8], RequestInputProgress)>, SqlError> {
        let Some(owner) = &self.request_owner else {
            return Ok(None);
        };
        if owner.source != source {
            return Ok(None);
        }
        if self.request.readable().get(..owner.text_len) != Some(text.as_bytes()) {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "active type-input source was replaced"
            ));
        }
        Ok(Some((
            &self.request.readable()[owner.text_len..],
            owner.progress,
        )))
    }

    pub(crate) fn request_input_progress(&self) -> Option<RequestInputProgress> {
        self.request_owner.as_ref().map(|owner| owner.progress)
    }

    pub(crate) fn request_input_waiting(&self) -> bool {
        self.request_owner
            .as_ref()
            .is_some_and(|owner| owner.progress.waiting.is_some())
    }

    pub(crate) fn save_request_input_progress(
        &mut self,
        source: super::type_inputs::Source,
        text: &str,
        bindings: &[super::type_inputs::Binding<'_>],
        progress: RequestInputProgress,
    ) -> Result<(), SqlError> {
        let bytes = super::type_inputs::encoded_len(bindings)?;
        if text
            .len()
            .checked_add(bytes)
            .is_none_or(|bytes| bytes > self.request.capacity())
        {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "active SQL source and bound inputs exceed prepared_bytes"
            ));
        }
        self.request.clear();
        assert!(
            self.request.append(text.as_bytes()),
            "capacity checked above"
        );
        super::type_inputs::encode(bindings, &mut self.request)?;
        self.request_owner = Some(RequestInputOwner {
            source,
            text_len: text.len(),
            progress,
        });
        Ok(())
    }

    pub(crate) fn clear_request_type_inputs(&mut self) {
        self.request_owner = None;
        self.request.clear();
        if let Some(generation) = self.transient_plan.take() {
            self.routine_inputs.retire_transient(generation);
        }
    }

    pub(crate) fn reserve_plan_ids(&mut self, count: usize) -> Result<u64, SqlError> {
        let id = self.next_plan;
        let count = u64::try_from(count).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "SQL plan identities exhausted"
            )
        })?;
        self.next_plan = id.checked_add(count).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "SQL plan identities exhausted"
            )
        })?;
        Ok(id)
    }

    pub(crate) fn capacity(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn transient_owner(
        &mut self,
        statement: usize,
    ) -> Result<super::type_input_plans::Owner, SqlError> {
        let generation = self.execution_epoch()?;
        Ok(super::type_input_plans::Owner::Transient {
            generation,
            statement,
        })
    }

    pub(crate) fn execution_epoch(&mut self) -> Result<u64, SqlError> {
        let generation = match self.transient_plan {
            Some(generation) => generation,
            None => {
                let generation = self.reserve_plan_ids(1)?;
                self.transient_plan = Some(generation);
                generation
            }
        };
        Ok(generation)
    }

    pub(crate) fn dynamic_owner(
        &mut self,
        kind: super::type_input_plans::DynamicCommandKind,
        source: &str,
        parameter_types: &[i32],
        arguments: &[super::types::Datum<'_>],
        arena: &Arena,
    ) -> Result<super::type_input_plans::Owner, SqlError> {
        use super::types::Datum;
        let epoch = self.execution_epoch()?;
        let length = parameter_types
            .len()
            .checked_mul(4)
            .ok_or_else(super::eval::arena_full)?;
        let types = arena
            .alloc_slice_with(length, |_| 0u8)
            .map_err(|_| super::eval::arena_full())?;
        for (out, oid) in types.as_chunks_mut::<4>().0.iter_mut().zip(parameter_types) {
            out.copy_from_slice(&oid.to_le_bytes());
        }
        let key = arena
            .alloc_slice_with(
                arguments
                    .len()
                    .checked_add(3)
                    .ok_or_else(super::eval::arena_full)?,
                |_| Datum::Null,
            )
            .map_err(|_| super::eval::arena_full())?;
        key[0] = Datum::Text(source);
        key[1] = Datum::Int4(match kind {
            super::type_input_plans::DynamicCommandKind::Statement => 0,
            super::type_input_plans::DynamicCommandKind::Cursor => 1,
        });
        key[2] = Datum::Bytea(types);
        key[3..].copy_from_slice(arguments);
        let values = super::sequence::statement_effect(
            super::sequence::StatementEffectKind::PlanAllocation,
            key,
            super::sequence::StatementEffectRetry::Restart,
            arena,
            || {
                let result = (|| {
                    let generation = self.reserve_plan_ids(1)?;
                    let bytes = arena
                        .alloc_slice_copy(&generation.to_le_bytes())
                        .map_err(|_| super::eval::arena_full())?;
                    arena
                        .alloc_slice_copy(&[Datum::Bytea(bytes)])
                        .map(|values| &*values)
                        .map_err(|_| super::eval::arena_full())
                })();
                super::sequence::StatementEffectAttempt {
                    result,
                    rewound_state: false,
                }
            },
        )?;
        let [Datum::Bytea(bytes)] = values else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "dynamic SQL plan identity has an invalid shape"
            ));
        };
        let generation = u64::from_le_bytes((*bytes).try_into().map_err(|_| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "dynamic SQL plan identity has an invalid length"
            )
        })?);
        Ok(super::type_input_plans::Owner::Dynamic { epoch, generation })
    }

    pub(crate) fn plan_owner(&self, name: &str) -> Option<super::type_input_plans::Owner> {
        self.slots
            .iter()
            .find(|slot| slot.active && slot.name.as_str() == name)
            .map(|slot| super::type_input_plans::Owner::SqlPrepared(slot.plan))
    }

    /// Returns whether the statement existed.
    pub fn remove(&mut self, name: &str) -> bool {
        if let Some(s) = self
            .slots
            .iter_mut()
            .find(|s| s.active && s.name.as_str() == name)
        {
            s.active = false;
            self.routine_inputs
                .retire(super::type_input_plans::Owner::SqlPrepared(s.plan));
            true
        } else {
            false
        }
    }

    pub fn clear(&mut self) {
        for s in &mut self.slots {
            if s.active {
                self.routine_inputs
                    .retire(super::type_input_plans::Owner::SqlPrepared(s.plan));
            }
            s.active = false;
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.iter().filter(|slot| slot.active).count()
    }

    pub(crate) fn visit(&self, mut visitor: impl FnMut(PreparedInfo<'_>)) {
        for slot in self.slots.iter().filter(|slot| slot.active) {
            let result_start = slot.text_len + slot.n_params * 4;
            let result_end = result_start + slot.n_results * 4;
            visitor(PreparedInfo {
                name: slot.name.as_str(),
                statement: core::str::from_utf8(&slot.data.readable()[..slot.text_len])
                    .expect("prepared SQL was validated as UTF-8"),
                parameter_types: PreparedTypeOids {
                    bytes: &slot.data.readable()[slot.text_len..result_start],
                },
                prepared_at: slot.prepared_at,
                custom_plans: slot.custom_plans,
                result_types: PreparedTypeOids {
                    bytes: &slot.data.readable()[result_start..result_end],
                },
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

/// PostgreSQL's assignment coercion test for an `EXECUTE` argument
/// (`coerce_to_target_type` with `COERCION_ASSIGNMENT`): unknown literals and
/// equal types always pass; otherwise domains compare by base type, a `pg_cast`
/// entry must permit assignment, any type converts to a string type by I/O,
/// arrays coerce element-wise and an anonymous record becomes a composite.
fn assignment_coercible(
    storage: &crate::storage::Storage,
    txid: u32,
    source: i32,
    target: i32,
) -> bool {
    use super::types::{ColType, oid};
    if source == target || source == oid::UNKNOWN {
        return true;
    }
    let base = |type_oid| {
        storage
            .domain_base_type_for_oid(type_oid, txid)
            .and_then(ColType::try_oid)
            .unwrap_or(type_oid)
    };
    let (source, target) = (base(source), base(target));
    if source == target {
        return true;
    }
    let declared = super::catalog::builtin_cast_context(source, target).or_else(|| {
        storage
            .cast_for_oids(source, target, txid)
            .map(|(_, cast)| cast.context)
    });
    if declared.is_some_and(crate::storage::CastContext::permits_assignment) {
        return true;
    }
    let target_type = storage
        .routine_result_for_oid(target, txid)
        .map(|result| result.ctype);
    if matches!(
        target_type,
        Some(ColType::Text | ColType::Varchar | ColType::Bpchar | ColType::Name)
    ) {
        return true;
    }
    if source == oid::RECORD && matches!(target_type, Some(ColType::Composite(_))) {
        return true;
    }
    let source_type = storage
        .routine_result_for_oid(source, txid)
        .map(|result| result.ctype);
    match (source_type, target_type) {
        (Some(ColType::Array(source)), Some(ColType::Array(target))) => {
            assignment_coercible(storage, txid, source.element_oid(), target.element_oid())
        }
        _ => false,
    }
}

/// Converts one `EXECUTE` argument to the type its parameter recorded at
/// PREPARE. Unknown literals use the target's input function, so domains,
/// enums and composites receive their own checks.
#[expect(
    clippy::too_many_arguments,
    reason = "argument coercion names the prepared position, value, both types and its catalog context"
)]
pub(crate) fn coerce_argument<'a>(
    storage: &crate::storage::Storage,
    txid: u32,
    position: usize,
    value: super::types::Datum<'a>,
    source: i32,
    target: i32,
    arena: &'a Arena,
    sequences: Option<&dyn super::eval::SequenceAccess>,
) -> Result<super::types::Datum<'a>, SqlError> {
    use super::types::{Datum, oid};
    if !assignment_coercible(storage, txid, source, target) {
        let name = |type_oid| {
            super::catalog::user_type_name_fixed(storage, txid, type_oid).map(|name| {
                name.or_else(|| super::exec::builtin_type_name_fixed(type_oid))
                    .unwrap_or_else(|| crate::util::StackStr::from_str("unknown"))
            })
        };
        super::eval::stash_diagnostic(
            None,
            Some(crate::util::StackStr::from_str(
                "You will need to rewrite or cast the expression.",
            )),
        );
        return Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "parameter ${} of type {} cannot be coerced to the expected type {}",
            position + 1,
            name(source)?.as_str(),
            name(target)?.as_str()
        ));
    }
    if value.is_null() {
        return super::exec::coerce_binary_input_null(storage, target, arena, txid, sequences);
    }
    if source == oid::UNKNOWN
        && let Datum::Text(text) = value
    {
        return super::exec::decode_text_input(
            storage,
            target,
            text.as_bytes(),
            arena,
            txid,
            sequences,
        );
    }
    if source == target {
        return Ok(value);
    }
    let declared = storage
        .routine_result_for_oid(target, txid)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "prepared parameter type {} is unavailable",
                target
            )
        })?;
    if declared.user_type.is_none() {
        return super::eval::cast(value, declared.ctype.internal_name(), arena);
    }
    super::exec::coerce_routine_argument(
        value,
        crate::storage::RoutineArgumentDef {
            name: crate::storage::SqlName::EMPTY,
            ctype: declared.ctype,
            user_type: declared.user_type,
        },
        storage,
        txid,
        arena,
        sequences,
    )
}
