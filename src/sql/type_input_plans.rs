//! Backend-owned checked inputs, qualified by the SQL plan that first used them.

use crate::config::Config;
use crate::mem::{Budget, BudgetError, FixedVec, arena::Arena, lease_pool::SharedSlice};
use crate::sql_err;

use super::{
    eval::{SqlError, sqlstate},
    type_inputs::{RoutineInputKind, Source},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlanId {
    slot: usize,
    generation: u64,
}

impl PlanId {
    pub(crate) fn from_key_bytes(bytes: &[u8]) -> Result<Self, SqlError> {
        let bytes: &[u8; 16] = bytes.try_into().map_err(|_| stale())?;
        Ok(Self {
            slot: usize::try_from(u64::from_le_bytes(
                bytes[..8].try_into().map_err(|_| stale())?,
            ))
            .map_err(|_| stale())?,
            generation: u64::from_le_bytes(bytes[8..].try_into().map_err(|_| stale())?),
        })
    }

    pub(crate) fn key_bytes(self) -> [u8; 16] {
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&(self.slot as u64).to_le_bytes());
        bytes[8..].copy_from_slice(&self.generation.to_le_bytes());
        bytes
    }
}

pub(crate) enum Lookup {
    Ready(PlanId, InputImage),
    Waiting(PlanId),
    Failed(PlanId, bool),
}

#[derive(Clone)]
pub(crate) enum InputImage {
    Runtime(SharedSlice<u8>),
    InlineAttempt {
        bytes: SharedSlice<u8>,
        disposition: InlineDisposition,
    },
    Stored(
        crate::storage::StoredTypeInputs,
        crate::storage::StoredInputSection,
    ),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InlineDisposition {
    Accepted,
    Rejected,
}

impl InputImage {
    pub(crate) fn bindings<'a>(
        &self,
        arena: &'a Arena,
    ) -> Result<&'a [super::type_inputs::Binding<'a>], SqlError> {
        match self {
            Self::Runtime(bytes) | Self::InlineAttempt { bytes, .. } if bytes.is_empty() => Ok(&[]),
            Self::Runtime(bytes) | Self::InlineAttempt { bytes, .. } => {
                let bytes = arena
                    .alloc_slice_copy(bytes)
                    .map_err(|_| super::eval::arena_full())?;
                super::type_inputs::decode(bytes, arena)
            }
            Self::Stored(inputs, section) => inputs.detached_bindings(*section, arena),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    Transient { generation: u64, statement: usize },
    Dynamic { epoch: u64, generation: u64 },
    ProtocolPrepared(u64),
    ProtocolPortal(u64),
    SqlPrepared(u64),
    Cursor(u64),
    Backend,
    Unlocated,
    Nested(PlanId),
}

#[derive(Clone, Copy)]
pub(crate) enum DynamicCommandKind {
    Statement,
    Cursor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    pub owner: Owner,
    pub source: Source,
}

#[derive(Debug)]
pub(crate) struct BodyAdmission {
    pub key: Key,
    pub reserved: Option<PlanId>,
}

enum State {
    Vacant,
    Preparing { key: Key, waiting: bool },
    Ready { key: Key, inputs: InputImage },
    Failed { key: Key, output_exhausted: bool },
}

struct Slot {
    generation: u64,
    state: State,
    specialization: SharedSlice<u8>,
    validity: Option<crate::storage::RuntimeInputValidity>,
    // An executing obsolete generation remains pinned but cannot become current
    // again when its caller later restores the former search path.
    invalidated: bool,
    input_attempt: Option<super::sequence::StatementInputAttempt>,
    /// A PL/pgSQL instance keeps the `plpgsql.variable_conflict` and
    /// `plpgsql.print_strict_params` values in effect when this backend first
    /// compiled it, as PostgreSQL's function cache does.
    compile_settings: Option<super::guc::PlpgsqlSettings>,
}

impl Slot {
    fn vacate(&mut self) {
        self.state = State::Vacant;
        self.specialization = SharedSlice::EMPTY;
        self.validity = None;
        self.invalidated = false;
        self.input_attempt = None;
        self.compile_settings = None;
    }
}

pub(crate) struct RoutineInputPlans {
    slots: FixedVec<Slot>,
    next_generation: u64,
}

impl RoutineInputPlans {
    pub(crate) fn budget_bytes(config: &Config) -> usize {
        config
            .routine_input_slots
            .saturating_mul(core::mem::size_of::<Slot>())
    }

    pub(crate) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        let mut slots = FixedVec::new(budget, "routine_input_slots", config.routine_input_slots)?;
        for _ in 0..config.routine_input_slots {
            slots
                .push(Slot {
                    generation: 0,
                    state: State::Vacant,
                    specialization: SharedSlice::EMPTY,
                    validity: None,
                    invalidated: false,
                    input_attempt: None,
                    compile_settings: None,
                })
                .expect("startup reserved every routine-input control");
        }
        Ok(Self {
            slots,
            next_generation: 1,
        })
    }

    pub(crate) fn find(&self, key: Key) -> Result<Option<Lookup>, SqlError> {
        if let Owner::Nested(parent) = key.owner {
            self.slot(parent)?;
        }
        for (index, slot) in self.slots.iter().enumerate() {
            // Runtime instance keys already contain the exact generation;
            // executing frames may still inspect that pinned obsolete image.
            if slot.invalidated && !matches!(key.source, Source::RuntimeRoutineInstance { .. }) {
                continue;
            }
            match &slot.state {
                State::Ready { key: found, inputs } if *found == key => {
                    return Ok(Some(Lookup::Ready(
                        PlanId {
                            slot: index,
                            generation: slot.generation,
                        },
                        inputs.clone(),
                    )));
                }
                State::Preparing {
                    key: found,
                    waiting: true,
                } if *found == key => {
                    return Ok(Some(Lookup::Waiting(PlanId {
                        slot: index,
                        generation: slot.generation,
                    })));
                }
                State::Preparing {
                    key: found,
                    waiting: false,
                } if *found == key => {
                    return Err(sql_err!(
                        sqlstate::OBJECT_IN_USE,
                        "recursive routine input preparation has not completed"
                    ));
                }
                _ => {}
            }
        }
        Ok(None)
    }

    pub(crate) fn find_validated(
        &mut self,
        key: Key,
        mut valid: impl FnMut(Option<&crate::storage::RuntimeInputValidity>) -> Result<bool, SqlError>,
    ) -> Result<Option<Lookup>, SqlError> {
        if matches!(key.source, Source::RuntimeRoutineInstance { .. }) {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "runtime routine instances require specialized lookup"
            ));
        }
        if let Some(plan) = super::sequence::replayed_input_plan()? {
            return self.lookup_replayed(plan, key).map(Some);
        }
        for index in 0..self.slots.len() {
            let slot = &self.slots[index];
            let found = match slot.state {
                State::Vacant => continue,
                State::Preparing { key, .. }
                | State::Ready { key, .. }
                | State::Failed { key, .. } => key,
            };
            if found != key {
                continue;
            }
            let plan = PlanId {
                slot: index,
                generation: slot.generation,
            };
            let obsolete = match slot.state {
                State::Preparing { .. } => !self.is_retained(plan),
                State::Failed { .. } => true,
                _ => slot.invalidated || !valid(slot.validity.as_ref())?,
            };
            if obsolete {
                self.slots[index].invalidated = true;
                if !self.is_retained(plan) {
                    self.abandon(plan)?;
                }
            }
        }
        self.find(key)
    }

    pub(crate) fn reserve(&mut self, key: Key) -> Result<PlanId, SqlError> {
        if matches!(key.source, Source::RuntimeRoutineInstance { .. }) {
            return Err(stale());
        }
        self.retire_obsolete_generations();
        if let Owner::Nested(parent) = key.owner {
            self.slot(parent)?;
        }
        let (index, generation) = self.available_slot()?;
        Ok(self.install(index, generation, key, SharedSlice::EMPTY))
    }

    pub(crate) fn runtime_routine_key(
        &mut self,
        kind: RoutineInputKind,
        routine_oid: i32,
        definition_version: u64,
        canonical: &[u8],
        mut valid: impl FnMut(Option<&crate::storage::RuntimeInputValidity>) -> Result<bool, SqlError>,
        freeze: impl FnOnce(&[u8]) -> Result<SharedSlice<u8>, SqlError>,
    ) -> Result<BodyAdmission, SqlError> {
        if canonical.is_empty() {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "routine instance specialization is empty"
            ));
        }
        self.retire_obsolete_generations();
        if kind == RoutineInputKind::Sql
            && let Some(plan) = super::sequence::replayed_input_plan()?
        {
            let slot = self.slot(plan)?;
            if !slot
                .input_attempt
                .is_some_and(super::sequence::retains_input_attempt)
            {
                return Err(stale());
            }
            let key = match slot.state {
                State::Preparing { key, .. }
                | State::Ready { key, .. }
                | State::Failed { key, .. } => key,
                State::Vacant => return Err(stale()),
            };
            if !matches!(key, Key { owner: Owner::Backend,
                source: Source::RuntimeRoutineInstance { kind: selected, routine_oid: oid,
                    definition_version: version, .. } }
                if selected == kind && oid == routine_oid && version == definition_version)
                || slot.specialization.as_ref() != canonical
            {
                return Err(stale());
            }
            return Ok(BodyAdmission {
                key,
                reserved: None,
            });
        }
        let mut current = None;
        // A replaced instance of the same definition keeps the settings it
        // was compiled under, as PostgreSQL's function cache does until the
        // function itself changes.
        let mut compiled = None;
        for index in 0..self.slots.len() {
            let slot = &self.slots[index];
            let key = match &slot.state {
                State::Vacant => continue,
                State::Preparing { key, .. }
                | State::Ready { key, .. }
                | State::Failed { key, .. } => *key,
            };
            if let Key {
                owner: Owner::Backend,
                source:
                    Source::RuntimeRoutineInstance {
                        kind: found_kind,
                        routine_oid: oid,
                        definition_version: version,
                        ..
                    },
            } = key
                && found_kind == kind
                && oid == routine_oid
                && version == definition_version
                && slot.specialization.as_ref() == canonical
            {
                let plan = PlanId {
                    slot: index,
                    generation: slot.generation,
                };
                // A waiting generation belongs to its recorded attempt. After
                // cancellation it cannot supply another statement's metadata.
                let reusable = match slot.state {
                    State::Preparing { .. } => self.is_retained(plan),
                    State::Failed { .. } => false,
                    _ => !slot.invalidated && valid(slot.validity.as_ref())?,
                };
                if reusable {
                    current = Some(key);
                } else {
                    compiled = compiled.or(self.slots[index].compile_settings);
                    self.slots[index].invalidated = true;
                    if !self.is_retained(plan) {
                        self.abandon(plan)?;
                    }
                }
            }
        }
        if let Some(key) = current {
            return Ok(BodyAdmission {
                key,
                reserved: None,
            });
        }
        let (index, generation) = self.available_slot()?;
        let specialization = freeze(canonical)?;
        if specialization.as_ref() != canonical {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "routine instance specialization changed during reservation"
            ));
        }
        let key = Key {
            owner: Owner::Backend,
            source: Source::RuntimeRoutineInstance {
                kind,
                routine_oid,
                definition_version,
                specialization: generation,
            },
        };
        let reserved = self.install(index, generation, key, specialization);
        self.slots[index].compile_settings = compiled;
        Ok(BodyAdmission {
            key,
            reserved: Some(reserved),
        })
    }

    pub(crate) fn adopt_preparing(&self, plan: PlanId, key: Key) -> Result<(), SqlError> {
        let slot = self.slot(plan)?;
        if matches!(slot.state, State::Preparing { key: found, waiting: false } if found == key)
            && match key.source {
                Source::RuntimeRoutineInstance { specialization, .. } => {
                    key.owner == Owner::Backend
                        && specialization == plan.generation
                        && !slot.specialization.is_empty()
                }
                _ => slot.specialization.is_empty(),
            }
        {
            Ok(())
        } else {
            Err(stale())
        }
    }

    pub(crate) fn set_plan_validity(
        &mut self,
        plan: PlanId,
        validity: crate::storage::RuntimeInputValidity,
    ) -> Result<(), SqlError> {
        let slot = self.slot_mut(plan)?;
        if !matches!(slot.state, State::Preparing { .. }) {
            return Err(stale());
        }
        slot.validity = Some(validity);
        Ok(())
    }

    pub(crate) fn finish_preparation(
        &mut self,
        plan: PlanId,
        storage: &crate::storage::Storage,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let slot = self.slot_mut(plan)?;
        if !matches!(slot.state, State::Preparing { .. }) {
            return Err(stale());
        }
        if let Some(validity) = &mut slot.validity {
            validity.finish_preparation(storage, transaction)?;
        }
        Ok(())
    }

    fn available_slot(&self) -> Result<(usize, u64), SqlError> {
        let index = self
            .slots
            .iter()
            .position(|slot| matches!(slot.state, State::Vacant))
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "checked routine plans exceed routine_input_slots"
                )
            })?;
        let generation = self.next_generation;
        generation.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "routine-input plan identities exhausted"
            )
        })?;
        Ok((index, generation))
    }

    fn install(
        &mut self,
        index: usize,
        generation: u64,
        key: Key,
        specialization: SharedSlice<u8>,
    ) -> PlanId {
        self.next_generation = generation + 1;
        self.slots[index] = Slot {
            generation,
            state: State::Preparing {
                key,
                waiting: false,
            },
            specialization,
            validity: None,
            invalidated: false,
            input_attempt: None,
            compile_settings: None,
        };
        PlanId {
            slot: index,
            generation,
        }
    }

    pub(crate) fn complete(&mut self, plan: PlanId, inputs: InputImage) -> Result<(), SqlError> {
        let slot = self.slot_mut(plan)?;
        let State::Preparing { key, .. } = slot.state else {
            return Err(stale());
        };
        slot.state = State::Ready { key, inputs };
        Ok(())
    }

    pub(crate) fn retain_for_statement(
        &mut self,
        plan: PlanId,
        attempt: Option<super::sequence::StatementInputAttempt>,
    ) -> Result<(), SqlError> {
        self.slot_mut(plan)?.input_attempt = attempt;
        Ok(())
    }

    pub(crate) fn lookup(&self, plan: PlanId, key: Key) -> Result<Lookup, SqlError> {
        if let Owner::Nested(parent) = key.owner {
            self.slot(parent)?;
        }
        match &self.slot(plan)?.state {
            State::Ready { key: found, inputs } if *found == key => {
                Ok(Lookup::Ready(plan, inputs.clone()))
            }
            State::Preparing {
                key: found,
                waiting: true,
            } if *found == key => Ok(Lookup::Waiting(plan)),
            State::Failed {
                key: found,
                output_exhausted,
            } if *found == key => Ok(Lookup::Failed(plan, *output_exhausted)),
            _ => Err(stale()),
        }
    }

    pub(crate) fn lookup_replayed(&self, plan: PlanId, key: Key) -> Result<Lookup, SqlError> {
        if !self
            .slot(plan)?
            .input_attempt
            .is_some_and(super::sequence::retains_input_attempt)
        {
            return Err(stale());
        }
        self.lookup(plan, key)
    }

    pub(crate) fn matching_replayed(
        &self,
        plan: PlanId,
        key: Key,
    ) -> Result<Option<Lookup>, SqlError> {
        let found = match self.slot(plan)?.state {
            State::Preparing { key, .. } | State::Ready { key, .. } | State::Failed { key, .. } => {
                key
            }
            State::Vacant => return Err(stale()),
        };
        if found == key {
            self.lookup_replayed(plan, key).map(Some)
        } else {
            Ok(None)
        }
    }

    pub(crate) fn retire_unused_site(&mut self, key: Key) {
        loop {
            let expired = self.slots.iter().enumerate().find_map(|(index, slot)| {
                let found = match slot.state {
                    State::Preparing { key, .. }
                    | State::Ready { key, .. }
                    | State::Failed { key, .. } => key,
                    State::Vacant => return None,
                };
                let plan = PlanId {
                    slot: index,
                    generation: slot.generation,
                };
                (found == key && !self.is_retained(plan)).then_some(plan)
            });
            let Some(plan) = expired else { break };
            self.abandon(plan)
                .expect("selected unused input site is live");
        }
    }

    pub(crate) fn fail(&mut self, plan: PlanId, output_exhausted: bool) -> Result<(), SqlError> {
        let slot = self.slot_mut(plan)?;
        let key = match slot.state {
            State::Preparing { key, .. } | State::Ready { key, .. } | State::Failed { key, .. } => {
                key
            }
            State::Vacant => return Err(stale()),
        };
        slot.state = State::Failed {
            key,
            output_exhausted,
        };
        Ok(())
    }

    fn retire_obsolete_generations(&mut self) {
        loop {
            let expired = self.slots.iter().enumerate().find_map(|(index, slot)| {
                let plan = PlanId {
                    slot: index,
                    generation: slot.generation,
                };
                ((slot.invalidated || matches!(slot.state, State::Failed { .. }))
                    && !self.is_retained(plan))
                .then_some(plan)
            });
            let Some(plan) = expired else { break };
            self.abandon(plan)
                .expect("selected obsolete generation is live");
        }
    }

    fn is_retained(&self, plan: PlanId) -> bool {
        if plan_is_active(plan) {
            return true;
        }
        // Recorded input generations and their owners survive until their
        // statement's ordered receipts are consumed or discarded.
        for (index, slot) in self.slots.iter().enumerate() {
            if !slot
                .input_attempt
                .is_some_and(super::sequence::retains_input_attempt)
            {
                continue;
            }
            let mut current = PlanId {
                slot: index,
                generation: slot.generation,
            };
            for _ in 0..self.slots.len() {
                if current == plan {
                    return true;
                }
                let Ok(slot) = self.slot(current) else { break };
                let key = match slot.state {
                    State::Preparing { key, .. }
                    | State::Ready { key, .. }
                    | State::Failed { key, .. } => key,
                    State::Vacant => break,
                };
                let Owner::Nested(parent) = key.owner else {
                    break;
                };
                current = parent;
            }
        }
        false
    }

    pub(crate) fn set_waiting(&mut self, plan: PlanId, waiting: bool) -> Result<(), SqlError> {
        let State::Preparing { waiting: value, .. } = &mut self.slot_mut(plan)?.state else {
            return Err(stale());
        };
        *value = waiting;
        Ok(())
    }

    pub(crate) fn finish_inline_attempt(
        &mut self,
        plan: PlanId,
        disposition: InlineDisposition,
    ) -> Result<(), SqlError> {
        let State::Ready { inputs, .. } = &mut self.slot_mut(plan)?.state else {
            return Err(stale());
        };
        match inputs {
            InputImage::Runtime(bytes) => {
                let bytes = match disposition {
                    InlineDisposition::Accepted => core::mem::replace(bytes, SharedSlice::EMPTY),
                    InlineDisposition::Rejected => SharedSlice::EMPTY,
                };
                *inputs = InputImage::InlineAttempt { bytes, disposition };
                Ok(())
            }
            InputImage::InlineAttempt {
                disposition: current,
                ..
            } if *current == disposition => Ok(()),
            _ => Err(stale()),
        }
    }

    pub(crate) fn abandon(&mut self, plan: PlanId) -> Result<(), SqlError> {
        self.slot_mut(plan)?.vacate();
        self.retire(Owner::Nested(plan));
        Ok(())
    }

    pub(crate) fn retire(&mut self, owner: Owner) {
        for slot in self.slots.iter_mut() {
            let key = match &slot.state {
                State::Vacant => continue,
                State::Preparing { key, .. }
                | State::Ready { key, .. }
                | State::Failed { key, .. } => *key,
            };
            if key.owner == owner {
                slot.vacate();
            }
        }
        // Reclaim descendants without recursion or a growing work queue.
        loop {
            let mut retired = false;
            for index in 0..self.slots.len() {
                let parent = match &self.slots[index].state {
                    State::Preparing {
                        key:
                            Key {
                                owner: Owner::Nested(parent),
                                ..
                            },
                        ..
                    }
                    | State::Ready {
                        key:
                            Key {
                                owner: Owner::Nested(parent),
                                ..
                            },
                        ..
                    }
                    | State::Failed {
                        key:
                            Key {
                                owner: Owner::Nested(parent),
                                ..
                            },
                        ..
                    } => *parent,
                    _ => continue,
                };
                if self.slot(parent).is_err() {
                    self.slots[index].vacate();
                    retired = true;
                }
            }
            if !retired {
                break;
            }
        }
    }

    pub(crate) fn retire_invalid_definitions(&mut self, mut visible: impl FnMut(i32, u64) -> bool) {
        loop {
            let retired = self.slots.iter().enumerate().find_map(|(index, slot)| {
                let key = match &slot.state {
                    State::Vacant => return None,
                    State::Preparing { key, .. }
                    | State::Ready { key, .. }
                    | State::Failed { key, .. } => key,
                };
                let (oid, version) = key.source.routine_definition()?;
                let plan = PlanId {
                    slot: index,
                    generation: slot.generation,
                };
                (!visible(oid, version) && !self.is_retained(plan)).then_some(plan)
            });
            let Some(retired) = retired else { break };
            self.abandon(retired).expect("selected plan is live");
        }
    }

    pub(crate) fn retire_invalid_cursors(&mut self, mut retained: impl FnMut(u64) -> bool) {
        loop {
            let owner = self.slots.iter().find_map(|slot| {
                let key = match &slot.state {
                    State::Vacant => return None,
                    State::Preparing { key, .. }
                    | State::Ready { key, .. }
                    | State::Failed { key, .. } => key,
                };
                let Owner::Cursor(generation) = key.owner else {
                    return None;
                };
                (!retained(generation)).then_some(key.owner)
            });
            let Some(owner) = owner else {
                break;
            };
            self.retire(owner);
        }
    }

    pub(crate) fn retire_invalid_protocol_owners(
        &mut self,
        mut retained: impl FnMut(Owner) -> bool,
    ) {
        loop {
            let owner = self.slots.iter().find_map(|slot| {
                let key = match &slot.state {
                    State::Preparing { key, .. }
                    | State::Ready { key, .. }
                    | State::Failed { key, .. } => key,
                    State::Vacant => return None,
                };
                (matches!(
                    key.owner,
                    Owner::ProtocolPrepared(_) | Owner::ProtocolPortal(_)
                ) && !retained(key.owner))
                .then_some(key.owner)
            });
            let Some(owner) = owner else { break };
            self.retire(owner);
        }
    }

    fn has_bound_owner(&self, mut plan: PlanId) -> bool {
        for _ in 0..self.slots.len() {
            let Ok(slot) = self.slot(plan) else {
                return false;
            };
            let key = match slot.state {
                State::Preparing { key, .. }
                | State::Ready { key, .. }
                | State::Failed { key, .. } => key,
                State::Vacant => return false,
            };
            match key.owner {
                Owner::ProtocolPrepared(_) | Owner::ProtocolPortal(_) | Owner::Cursor(_) => {
                    return true;
                }
                Owner::Nested(parent) => plan = parent,
                _ => return false,
            }
        }
        false
    }

    pub(crate) fn discard_unpinned_plans(&mut self) {
        for index in 0..self.slots.len() {
            let slot = &self.slots[index];
            let key = match slot.state {
                State::Preparing { key, .. }
                | State::Ready { key, .. }
                | State::Failed { key, .. } => key,
                State::Vacant => continue,
            };
            let plan = PlanId {
                slot: index,
                generation: slot.generation,
            };
            if self.has_bound_owner(plan)
                || matches!(
                    key.source,
                    Source::RuntimeRoutineInstance {
                        kind: RoutineInputKind::Plpgsql,
                        ..
                    }
                )
            {
                continue;
            }
            // Pending parsers complete after DISCARD. Running completed images
            // stay alive, but a later admission must choose a new generation.
            if !matches!(slot.state, State::Preparing { .. }) {
                self.slots[index].invalidated = true;
            }
            if !self.is_retained(plan) {
                self.abandon(plan)
                    .expect("selected discarded generation is live");
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        for slot in self.slots.iter_mut() {
            slot.vacate();
        }
    }

    pub(crate) fn retire_transient(&mut self, generation: u64) {
        loop {
            let owner = self.slots.iter().find_map(|slot| {
                let key = match &slot.state {
                    State::Preparing { key, .. }
                    | State::Ready { key, .. }
                    | State::Failed { key, .. } => key,
                    State::Vacant => return None,
                };
                matches!(key.owner, Owner::Transient { generation: found, .. }
                    | Owner::Dynamic { epoch: found, .. } if found == generation)
                .then_some(key.owner)
            });
            let Some(owner) = owner else {
                break;
            };
            self.retire(owner);
        }
    }

    /// Records the settings a PL/pgSQL instance compiled with; the first
    /// recording wins for the instance's lifetime.
    pub(crate) fn compiled_settings(
        &mut self,
        plan: PlanId,
        current: super::guc::PlpgsqlSettings,
    ) -> Result<super::guc::PlpgsqlSettings, SqlError> {
        let slot = self.slot_mut(plan)?;
        Ok(*slot.compile_settings.get_or_insert(current))
    }

    fn slot(&self, id: PlanId) -> Result<&Slot, SqlError> {
        self.slots
            .get(id.slot)
            .filter(|slot| slot.generation == id.generation && !matches!(slot.state, State::Vacant))
            .ok_or_else(stale)
    }

    fn slot_mut(&mut self, id: PlanId) -> Result<&mut Slot, SqlError> {
        self.slots
            .get_mut(id.slot)
            .filter(|slot| slot.generation == id.generation && !matches!(slot.state, State::Vacant))
            .ok_or_else(stale)
    }
}

fn stale() -> SqlError {
    sql_err!(
        sqlstate::SERIALIZATION_FAILURE,
        "routine-input plan owner was replaced"
    )
}

thread_local! {
    static OWNER: core::cell::Cell<Option<Owner>> = const { core::cell::Cell::new(None) };
    static ACTIVE_PLANS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    static ACTIVE_INPUTS: core::cell::Cell<*const ActiveInputPlan> = const { core::cell::Cell::new(core::ptr::null()) };
}

struct ActiveInputPlan {
    plan: PlanId,
    previous: *const Self,
}

fn plan_is_active(plan: PlanId) -> bool {
    ACTIVE_INPUTS.with(|active| {
        let mut pointer = active.get();
        // Each link borrows an immutable with_owner stack frame. Its guard
        // restores the previous link before that frame leaves scope.
        while let Some(frame) = unsafe { pointer.as_ref() } {
            if frame.plan == plan {
                return true;
            }
            pointer = frame.previous;
        }
        false
    })
}

pub(crate) fn has_active_plan() -> bool {
    ACTIVE_PLANS.with(core::cell::Cell::get) != 0
}

pub(crate) fn owner() -> Result<Owner, SqlError> {
    OWNER.with(core::cell::Cell::get).ok_or_else(|| {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "routine input preparation has no SQL plan owner"
        )
    })
}

pub(crate) fn with_owner<T>(owner: Owner, run: impl FnOnce() -> T) -> T {
    struct Restore<'a> {
        owner: Option<Owner>,
        depth: usize,
        inputs: *const ActiveInputPlan,
        _frame: &'a Option<ActiveInputPlan>,
    }
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            OWNER.with(|owner| owner.set(self.owner));
            ACTIVE_PLANS.with(|active| active.set(self.depth));
            ACTIVE_INPUTS.with(|active| active.set(self.inputs));
        }
    }
    let depth = ACTIVE_PLANS.with(core::cell::Cell::get);
    let inputs = ACTIVE_INPUTS.with(core::cell::Cell::get);
    let frame = match owner {
        Owner::Nested(plan) => Some(ActiveInputPlan {
            plan,
            previous: inputs,
        }),
        _ => None,
    };
    let _restore = Restore {
        owner: OWNER.with(|active| active.replace(Some(owner))),
        depth,
        inputs,
        _frame: &frame,
    };
    if let Some(frame) = &frame {
        ACTIVE_INPUTS.with(|active| active.set(frame));
    }
    if matches!(owner, Owner::Nested(_) | Owner::Cursor(_)) {
        ACTIVE_PLANS.with(|active| {
            active.set(
                depth
                    .checked_add(1)
                    .expect("plan depth fits the call stack"),
            )
        });
    }
    run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::lease_pool::{LeasePool, Purpose};

    #[test]
    fn discard_plans_keeps_active_generations_and_replaces_future_query_admissions() {
        let mut config = Config::default_dev();
        config.routine_input_slots = 4;
        let mut budget =
            Budget::new(RoutineInputPlans::budget_bytes(&config) + LeasePool::budget_bytes(128, 2));
        let pool = LeasePool::new(&mut budget, Purpose::TABLE_METADATA, 128, 2).unwrap();
        let mut plans = RoutineInputPlans::new(&config, &mut budget).unwrap();
        let freeze = |bytes: &[u8]| {
            pool.copy_slice(bytes)
                .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))
        };
        crate::mem::guard::forbid_alloc(|| {
            let instance = plans
                .runtime_routine_key(
                    RoutineInputKind::Plpgsql,
                    100_001,
                    7,
                    b"pl",
                    |_| Ok(true),
                    freeze,
                )
                .unwrap();
            let parent = instance.reserved.unwrap();
            plans
                .complete(parent, InputImage::Runtime(SharedSlice::EMPTY))
                .unwrap();
            let sql = plans
                .runtime_routine_key(
                    RoutineInputKind::Sql,
                    100_002,
                    7,
                    b"sql",
                    |_| Ok(true),
                    freeze,
                )
                .unwrap();
            plans
                .complete(
                    sql.reserved.unwrap(),
                    InputImage::Runtime(SharedSlice::EMPTY),
                )
                .unwrap();
            let key = Key {
                owner: Owner::Nested(parent),
                source: source(1),
            };
            let original = plans.reserve(key).unwrap();
            plans
                .complete(original, InputImage::Runtime(SharedSlice::EMPTY))
                .unwrap();
            let replacement = with_owner(Owner::Nested(parent), || {
                with_owner(Owner::Nested(original), || {
                    plans.discard_unpinned_plans();
                    assert!(
                        matches!(plans.lookup(original, key).unwrap(), Lookup::Ready(found, _) if found == original)
                    );
                    assert!(plans.find(sql.key).unwrap().is_none());
                    assert!(plans.find_validated(key, |_| Ok(true)).unwrap().is_none());
                    let replacement = plans.reserve(key).unwrap();
                    assert_ne!(replacement, original);
                    plans
                        .complete(replacement, InputImage::Runtime(SharedSlice::EMPTY))
                        .unwrap();
                    replacement
                })
            });
            let again = plans
                .runtime_routine_key(
                    RoutineInputKind::Plpgsql,
                    100_001,
                    7,
                    b"pl",
                    |_| Ok(true),
                    |_| panic!("DISCARD PLANS does not replace the compiled PL function"),
                )
                .unwrap();
            assert_eq!(again.key, instance.key);
            assert!(plans.lookup(original, key).is_err());
            assert!(
                matches!(plans.find_validated(key, |_| Ok(true)).unwrap(), Some(Lookup::Ready(found, _)) if found == replacement)
            );
        });
    }

    #[test]
    fn caught_input_errors_keep_exact_generations_until_later_waits_finish() {
        use super::super::sequence::{
            self, StatementEffectAttempt, StatementEffectKind, StatementEffectRetry,
        };
        use super::super::types::Datum;
        let mut config = Config::default_dev();
        config.routine_input_slots = 3;
        let mut budget = Budget::new(RoutineInputPlans::budget_bytes(&config) + (64 << 10));
        let mut plans = RoutineInputPlans::new(&config, &mut budget).unwrap();
        let mut arena = Arena::new(&mut budget, "caught input error receipts", 64 << 10).unwrap();
        let guc = super::super::guc::GucState::new();
        let session = guc.seq_session();
        let key = Key {
            owner: Owner::Backend,
            source: source(1),
        };
        let wait_key = Key {
            source: source(2),
            ..key
        };
        let calls = core::cell::Cell::new(0);
        crate::mem::guard::forbid_alloc(|| {
            let mut failed = [None; 2];
            let waiting;
            {
                let _scope = sequence::enter_statement(&arena, session, false, None);
                for failed in &mut failed {
                    assert!(
                        plans
                            .find_validated(key, |_| panic!("a failed attempt cannot be reused"))
                            .unwrap()
                            .is_none()
                    );
                    let plan = plans.reserve(key).unwrap();
                    *failed = Some(plan);
                    let bytes = plan.key_bytes();
                    let error = sequence::statement_effect(
                        StatementEffectKind::RoutineInput,
                        &[Datum::Bytea(&bytes)],
                        StatementEffectRetry::Restart,
                        &arena,
                        || {
                            plans
                                .retain_for_statement(plan, sequence::input_attempt())
                                .unwrap();
                            calls.set(calls.get() + 1);
                            StatementEffectAttempt {
                                result: Err(sql_err!(
                                    sqlstate::CHECK_VIOLATION,
                                    "test CHECK rejected the value"
                                )),
                                rewound_state: false,
                            }
                        },
                    )
                    .unwrap_err();
                    assert_eq!(error.sqlstate, sqlstate::CHECK_VIOLATION);
                    plans.fail(plan, false).unwrap();
                    assert!(plans.find(key).unwrap().is_none());
                }
                waiting = plans.reserve(wait_key).unwrap();
                let bytes = waiting.key_bytes();
                let error = sequence::statement_effect(
                    StatementEffectKind::RoutineInput,
                    &[Datum::Bytea(&bytes)],
                    StatementEffectRetry::Restart,
                    &arena,
                    || {
                        plans
                            .retain_for_statement(waiting, sequence::input_attempt())
                            .unwrap();
                        plans.set_waiting(waiting, true).unwrap();
                        StatementEffectAttempt {
                            result: Err(sql_err!(sqlstate::INTERNAL_LOCK_WAIT, "test row lock")),
                            rewound_state: false,
                        }
                    },
                )
                .unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::INTERNAL_LOCK_WAIT);
            }
            arena.recycle_front();
            {
                let scope = sequence::enter_statement(&arena, session, true, None);
                for failed in failed {
                    let plan = failed.unwrap();
                    assert!(
                        matches!(plans.find_validated(key, |_| panic!("replay restores the caught error")).unwrap(), Some(Lookup::Failed(found, false)) if found == plan)
                    );
                    let bytes = plan.key_bytes();
                    let error = sequence::statement_effect(
                        StatementEffectKind::RoutineInput,
                        &[Datum::Bytea(&bytes)],
                        StatementEffectRetry::Restart,
                        &arena,
                        || panic!("a caught CHECK must not execute twice"),
                    )
                    .unwrap_err();
                    assert_eq!(error.sqlstate, sqlstate::CHECK_VIOLATION);
                    plans.fail(plan, false).unwrap();
                }
                assert!(
                    matches!(plans.find_validated(wait_key, |_| panic!("resume the original wait")).unwrap(), Some(Lookup::Waiting(found)) if found == waiting)
                );
                let bytes = waiting.key_bytes();
                sequence::statement_effect(
                    StatementEffectKind::RoutineInput,
                    &[Datum::Bytea(&bytes)],
                    StatementEffectRetry::Restart,
                    &arena,
                    || {
                        plans
                            .complete(waiting, InputImage::Runtime(SharedSlice::EMPTY))
                            .unwrap();
                        StatementEffectAttempt {
                            result: Ok(&[]),
                            rewound_state: false,
                        }
                    },
                )
                .unwrap();
                scope.finish(session).unwrap();
            }
            assert_eq!(calls.get(), 2);
            let scope = sequence::enter_statement(&arena, session, false, None);
            assert!(
                plans
                    .find_validated(key, |_| panic!("a failed plan has no checked values"))
                    .unwrap()
                    .is_none()
            );
            let fresh = plans.reserve(key).unwrap();
            assert!(failed.iter().all(|failed| *failed != Some(fresh)));
            plans.abandon(fresh).unwrap();
            scope.finish(session).unwrap();
        });
    }

    #[test]
    fn statement_receipts_retain_each_input_generation_and_release_between_attempts() {
        use super::super::sequence::{
            self, StatementEffectAttempt, StatementEffectKind, StatementEffectRetry,
        };
        use super::super::types::Datum;
        let mut config = Config::default_dev();
        config.routine_input_slots = 3;
        let mut budget = Budget::new(RoutineInputPlans::budget_bytes(&config) + (64 << 10));
        let mut plans = RoutineInputPlans::new(&config, &mut budget).unwrap();
        let mut arena =
            Arena::new(&mut budget, "retained routine input generations", 64 << 10).unwrap();
        let guc = super::super::guc::GucState::new();
        let session = guc.seq_session();
        let parent = plans
            .reserve(Key {
                owner: Owner::Backend,
                source: source(18),
            })
            .unwrap();
        plans
            .complete(parent, InputImage::Runtime(SharedSlice::EMPTY))
            .unwrap();
        let key = Key {
            owner: Owner::Nested(parent),
            source: source(19),
        };
        crate::mem::guard::forbid_alloc(|| {
            let first = plans.reserve(key).unwrap();
            let second;
            let original_attempt;
            {
                let _scope = sequence::enter_statement(&arena, session, false, None);
                let bytes = first.key_bytes();
                sequence::statement_effect(
                    StatementEffectKind::RoutineInput,
                    &[Datum::Bytea(&bytes)],
                    StatementEffectRetry::Restart,
                    &arena,
                    || {
                        plans
                            .retain_for_statement(first, sequence::input_attempt())
                            .unwrap();
                        plans
                            .complete(first, InputImage::Runtime(SharedSlice::EMPTY))
                            .unwrap();
                        StatementEffectAttempt {
                            result: Ok(&[]),
                            rewound_state: false,
                        }
                    },
                )
                .unwrap();
                original_attempt = sequence::input_attempt().unwrap();
                assert!(plans.find_validated(key, |_| Ok(false)).unwrap().is_none());
                assert!(plans.is_retained(first));
                assert!(plans.is_retained(parent));
                second = plans.reserve(key).unwrap();
                let bytes = second.key_bytes();
                sequence::statement_effect(
                    StatementEffectKind::RoutineInput,
                    &[Datum::Bytea(&bytes)],
                    StatementEffectRetry::Restart,
                    &arena,
                    || {
                        plans
                            .retain_for_statement(second, sequence::input_attempt())
                            .unwrap();
                        plans
                            .complete(second, InputImage::Runtime(SharedSlice::EMPTY))
                            .unwrap();
                        StatementEffectAttempt {
                            result: Ok(&[]),
                            rewound_state: false,
                        }
                    },
                )
                .unwrap();
            }
            arena.recycle_front();
            {
                let scope = sequence::enter_statement(&arena, session, true, None);
                for expected in [first, second] {
                    assert!(
                        matches!(plans.find_validated(key, |_| panic!("a receipt selects its original generation")).unwrap(),
                        Some(Lookup::Ready(found, _)) if found == expected)
                    );
                    assert_eq!(
                        plans
                            .lookup(
                                expected,
                                Key {
                                    source: source(20),
                                    ..key
                                }
                            )
                            .err()
                            .unwrap()
                            .sqlstate,
                        sqlstate::SERIALIZATION_FAILURE
                    );
                    let bytes = expected.key_bytes();
                    sequence::statement_effect(
                        StatementEffectKind::RoutineInput,
                        &[Datum::Bytea(&bytes)],
                        StatementEffectRetry::Restart,
                        &arena,
                        || panic!("completed checked values cannot execute again"),
                    )
                    .unwrap();
                }
                scope.finish(session).unwrap();
                assert!(!plans.is_retained(first));
                assert!(!plans.is_retained(second));
                assert!(!plans.is_retained(parent));
            }
            {
                let scope = sequence::enter_statement(&arena, session, false, None);
                assert!(plans.find_validated(key, |_| Ok(false)).unwrap().is_none());
                let replacement = plans.reserve(key).unwrap();
                assert_ne!(replacement, first);
                assert_ne!(replacement, second);
                let bytes = replacement.key_bytes();
                sequence::statement_effect(
                    StatementEffectKind::RoutineInput,
                    &[Datum::Bytea(&bytes)],
                    StatementEffectRetry::Restart,
                    &arena,
                    || {
                        plans
                            .retain_for_statement(replacement, sequence::input_attempt())
                            .unwrap();
                        plans
                            .complete(replacement, InputImage::Runtime(SharedSlice::EMPTY))
                            .unwrap();
                        StatementEffectAttempt {
                            result: Ok(&[]),
                            rewound_state: false,
                        }
                    },
                )
                .unwrap();
                assert!(sequence::input_attempt() != Some(original_attempt));
                scope.finish(session).unwrap();
            }
            arena.reset();
            {
                let scope = sequence::enter_statement(&arena, session, false, None);
                assert!(!sequence::retains_input_attempt(original_attempt));
                assert!(plans.find_validated(key, |_| Ok(false)).unwrap().is_none());
                assert!(plans.lookup(first, key).is_err());
                let replacement = plans.reserve(key).unwrap();
                let bytes = replacement.key_bytes();
                sequence::statement_effect(
                    StatementEffectKind::RoutineInput,
                    &[Datum::Bytea(&bytes)],
                    StatementEffectRetry::Restart,
                    &arena,
                    || {
                        plans
                            .retain_for_statement(replacement, sequence::input_attempt())
                            .unwrap();
                        plans
                            .complete(replacement, InputImage::Runtime(SharedSlice::EMPTY))
                            .unwrap();
                        StatementEffectAttempt {
                            result: Ok(&[]),
                            rewound_state: false,
                        }
                    },
                )
                .unwrap();
                assert!(sequence::input_attempt() != Some(original_attempt));
                scope.finish(session).unwrap();
            }
        });
    }

    #[test]
    fn discarding_plans_preserves_bound_protocol_and_cursor_owners_until_release() {
        let mut config = Config::default_dev();
        config.routine_input_slots = 8;
        let mut budget =
            Budget::new(RoutineInputPlans::budget_bytes(&config) + LeasePool::budget_bytes(64, 1));
        let pool = LeasePool::new(&mut budget, Purpose::TABLE_METADATA, 64, 1).unwrap();
        let mut plans = RoutineInputPlans::new(&config, &mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let keys = [
                Owner::ProtocolPrepared(1),
                Owner::ProtocolPortal(2),
                Owner::Cursor(3),
                Owner::Backend,
                Owner::SqlPrepared(4),
            ]
            .map(|owner| Key {
                owner,
                source: source(0),
            });
            let mut ids = [None; 5];
            for (at, key) in keys.iter().enumerate() {
                let id = plans.reserve(*key).unwrap();
                let bytes = if at == 0 {
                    pool.copy_slice(b"TIP2\0\0\0\0").unwrap()
                } else {
                    SharedSlice::EMPTY
                };
                plans.complete(id, InputImage::Runtime(bytes)).unwrap();
                ids[at] = Some(id);
            }
            let parent = ids[0].unwrap();
            let child = Key {
                owner: Owner::Nested(parent),
                source: source(1),
            };
            let child_id = plans.reserve(child).unwrap();
            plans
                .complete(child_id, InputImage::Runtime(SharedSlice::EMPTY))
                .unwrap();
            let Some(Lookup::Ready(_, pinned)) = plans.find(keys[0]).unwrap() else {
                panic!("ready generic plan")
            };
            plans.discard_unpinned_plans();
            for key in &keys[..3] {
                assert!(matches!(
                    plans.find(*key).unwrap(),
                    Some(Lookup::Ready(_, _))
                ));
            }
            assert!(matches!(
                plans.find(child).unwrap(),
                Some(Lookup::Ready(_, _))
            ));
            for key in &keys[3..] {
                assert!(plans.find(*key).unwrap().is_none());
            }
            plans.retire_invalid_protocol_owners(|owner| owner == Owner::ProtocolPortal(2));
            assert!(plans.find(keys[0]).unwrap().is_none());
            assert!(plans.find(child).is_err());
            assert!(matches!(
                plans.find(keys[1]).unwrap(),
                Some(Lookup::Ready(_, _))
            ));
            assert!(matches!(
                plans.find(keys[2]).unwrap(),
                Some(Lookup::Ready(_, _))
            ));
            assert!(pool.lease::<u8>(1).is_err());
            drop(pinned);
            assert!(pool.lease::<u8>(64).is_ok());
            plans.retire_invalid_protocol_owners(|_| false);
            plans.retire_invalid_cursors(|_| false);
            for key in keys {
                assert!(plans.find(key).unwrap().is_none());
            }
        });
    }

    #[test]
    fn runtime_routine_languages_keep_distinct_instances_and_reuse_owned_keys() {
        let mut config = Config::default_dev();
        config.routine_input_slots = 2;
        let key_storage = 2 * 64;
        let mut budget = Budget::new(
            RoutineInputPlans::budget_bytes(&config) + LeasePool::budget_bytes(key_storage, 2),
        );
        let pool = LeasePool::new(&mut budget, Purpose::TABLE_METADATA, key_storage, 2).unwrap();
        let mut plans = RoutineInputPlans::new(&config, &mut budget).unwrap();
        let freeze = |bytes: &[u8]| {
            pool.copy_slice(bytes)
                .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))
        };
        crate::mem::guard::forbid_alloc(|| {
            let sql = plans
                .runtime_routine_key(
                    RoutineInputKind::Sql,
                    100_001,
                    7,
                    b"text/c",
                    |_| Ok(true),
                    freeze,
                )
                .unwrap();
            let pl = plans
                .runtime_routine_key(
                    RoutineInputKind::Plpgsql,
                    100_001,
                    7,
                    b"text/c",
                    |_| Ok(true),
                    freeze,
                )
                .unwrap();
            assert_ne!(sql.key, pl.key);
            plans
                .complete(
                    sql.reserved.unwrap(),
                    InputImage::Runtime(SharedSlice::EMPTY),
                )
                .unwrap();
            plans
                .complete(
                    pl.reserved.unwrap(),
                    InputImage::Runtime(SharedSlice::EMPTY),
                )
                .unwrap();
            for _ in 0..1100 {
                let again = plans
                    .runtime_routine_key(
                        RoutineInputKind::Plpgsql,
                        100_001,
                        7,
                        b"text/c",
                        |_| Ok(true),
                        |_| panic!("ready PL instance must retain its existing key"),
                    )
                    .unwrap();
                assert_eq!(again.key, pl.key);
                assert!(again.reserved.is_none());
            }
            plans.clear();
            assert!(pool.lease::<u8>(key_storage).is_ok());
        });
    }

    #[test]
    fn runtime_sql_body_keys_compare_exact_bytes_and_share_ready_and_waiting_owners() {
        let mut config = Config::default_dev();
        config.routine_input_slots = 3;
        let mut budget =
            Budget::new(RoutineInputPlans::budget_bytes(&config) + LeasePool::budget_bytes(256, 4));
        let pool = LeasePool::new(&mut budget, Purpose::TABLE_METADATA, 256, 4).unwrap();
        let mut plans = RoutineInputPlans::new(&config, &mut budget).unwrap();
        let freeze = |bytes: &[u8]| {
            pool.copy_slice(bytes)
                .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))
        };
        crate::mem::guard::forbid_alloc(|| {
            let mut bytes = *b"declared-int4,collation-0,result-int4";
            let original = bytes;
            let first = plans
                .runtime_routine_key(
                    RoutineInputKind::Sql,
                    100_001,
                    7,
                    &bytes,
                    |_| Ok(true),
                    freeze,
                )
                .unwrap();
            let first_id = first.reserved.unwrap();
            plans.adopt_preparing(first_id, first.key).unwrap();
            assert_eq!(
                plans.find(first.key).err().unwrap().sqlstate,
                sqlstate::OBJECT_IN_USE
            );
            bytes[9] = b'X';
            let changed = plans
                .runtime_routine_key(
                    RoutineInputKind::Sql,
                    100_001,
                    7,
                    &bytes,
                    |_| Ok(true),
                    freeze,
                )
                .unwrap();
            let waiting = changed.reserved.unwrap();
            assert_ne!(changed.key, first.key);
            plans.adopt_preparing(waiting, changed.key).unwrap();
            plans.set_waiting(waiting, true).unwrap();
            let reused = with_owner(Owner::Nested(waiting), || {
                plans
                    .runtime_routine_key(
                        RoutineInputKind::Sql,
                        100_001,
                        7,
                        &bytes,
                        |_| Ok(true),
                        |_| panic!("waiting key must retain its existing bytes"),
                    )
                    .unwrap()
            });
            assert_eq!(reused.key, changed.key);
            assert!(reused.reserved.is_none());
            assert!(
                matches!(plans.find(changed.key).unwrap(), Some(Lookup::Waiting(id)) if id == waiting)
            );
            assert!(plans.adopt_preparing(waiting, changed.key).is_err());
            plans
                .complete(
                    first_id,
                    InputImage::Runtime(pool.copy_slice(b"TIP2\0\0\0\0").unwrap()),
                )
                .unwrap();
            let Some(Lookup::Ready(_, pinned)) = plans.find(first.key).unwrap() else {
                panic!("ready typed SQL body");
            };
            let other = plans
                .runtime_routine_key(
                    RoutineInputKind::Sql,
                    100_002,
                    7,
                    &original,
                    |_| Ok(true),
                    freeze,
                )
                .unwrap();
            plans
                .adopt_preparing(other.reserved.unwrap(), other.key)
                .unwrap();
            assert_ne!(other.key, first.key);
            assert_eq!(pool.lease::<u8>(1).unwrap_err().available_leases, 0);
            let reused = plans
                .runtime_routine_key(
                    RoutineInputKind::Sql,
                    100_001,
                    7,
                    &original,
                    |_| Ok(true),
                    |_| panic!("ready key must not allocate when the pool is full"),
                )
                .unwrap();
            assert_eq!(reused.key, first.key);
            assert!(reused.reserved.is_none());
            let error = plans
                .runtime_routine_key(
                    RoutineInputKind::Sql,
                    100_001,
                    8,
                    &original,
                    |_| Ok(true),
                    |_| panic!("control admission must precede payload reservation"),
                )
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(error.message.as_str().contains("routine_input_slots"));
            plans.retire_invalid_definitions(|oid, version| oid == 100_002 && version == 7);
            assert!(plans.find(first.key).unwrap().is_none());
            assert!(plans.find(changed.key).unwrap().is_none());
            assert!(
                matches!(plans.find(other.key).err(), Some(error) if error.sqlstate == sqlstate::OBJECT_IN_USE)
            );
            assert!(plans.adopt_preparing(first_id, first.key).is_err());
            plans.clear();
            assert_eq!(pool.lease::<u8>(256).unwrap_err().free_bytes, 192);
            drop(pinned);
            assert!(pool.lease::<u8>(256).is_ok());
        });
    }

    #[test]
    fn waiting_input_attempts_preserve_their_generation_across_environment_changes() {
        let mut config = Config::default_dev();
        config.routine_input_slots = 1;
        let mut budget =
            Budget::new(RoutineInputPlans::budget_bytes(&config) + LeasePool::budget_bytes(128, 2));
        let pool = LeasePool::new(&mut budget, Purpose::TABLE_METADATA, 128, 2).unwrap();
        let mut plans = RoutineInputPlans::new(&config, &mut budget).unwrap();
        let freeze = |bytes: &[u8]| {
            pool.copy_slice(bytes)
                .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))
        };
        crate::mem::guard::forbid_alloc(|| {
            let admission = plans
                .runtime_routine_key(
                    RoutineInputKind::Sql,
                    100_001,
                    7,
                    b"int4",
                    |_| Ok(true),
                    freeze,
                )
                .unwrap();
            let plan = admission.reserved.unwrap();
            plans.set_waiting(plan, true).unwrap();
            let resumed = with_owner(Owner::Nested(plan), || {
                plans
                    .runtime_routine_key(
                        RoutineInputKind::Sql,
                        100_001,
                        7,
                        b"int4",
                        |_| panic!("an unfinished input still owns its original retry receipt"),
                        |_| panic!("resuming an input must not allocate another generation"),
                    )
                    .unwrap()
            });
            assert_eq!(resumed.key, admission.key);
            assert!(resumed.reserved.is_none());
            assert!(
                matches!(plans.find(admission.key).unwrap(), Some(Lookup::Waiting(found)) if found == plan)
            );
            plans
                .complete(plan, InputImage::Runtime(SharedSlice::EMPTY))
                .unwrap();
            let replacement = plans
                .runtime_routine_key(
                    RoutineInputKind::Sql,
                    100_001,
                    7,
                    b"int4",
                    |_| Ok(false),
                    freeze,
                )
                .unwrap();
            assert_ne!(replacement.reserved.unwrap(), plan);
            plans.abandon(replacement.reserved.unwrap()).unwrap();
            let key = Key {
                owner: Owner::SqlPrepared(1),
                source: source(31),
            };
            let plan = plans.reserve(key).unwrap();
            plans.set_waiting(plan, true).unwrap();
            assert!(
                matches!(with_owner(Owner::Nested(plan), || plans.find_validated(key, |_| panic!("an active statement keeps its receipt"))).unwrap(),
                Some(Lookup::Waiting(found)) if found == plan)
            );
            plans
                .complete(plan, InputImage::Runtime(SharedSlice::EMPTY))
                .unwrap();
            assert!(plans.find_validated(key, |_| Ok(false)).unwrap().is_none());
            assert_ne!(plans.reserve(key).unwrap(), plan);
        });
    }

    #[test]
    fn cancelled_preparations_release_waiting_generations_before_fresh_admission() {
        use super::super::sequence::{
            self, StatementEffectAttempt, StatementEffectKind, StatementEffectRetry,
        };
        use super::super::types::Datum;
        let mut config = Config::default_dev();
        config.routine_input_slots = 1;
        let mut budget = Budget::new(
            RoutineInputPlans::budget_bytes(&config) + LeasePool::budget_bytes(64, 1) + (64 << 10),
        );
        let pool = LeasePool::new(&mut budget, Purpose::TABLE_METADATA, 64, 1).unwrap();
        let mut plans = RoutineInputPlans::new(&config, &mut budget).unwrap();
        let arena = Arena::new(&mut budget, "cancelled routine input receipts", 64 << 10).unwrap();
        let guc = super::super::guc::GucState::new();
        let session = guc.seq_session();
        let freeze = |bytes: &[u8]| {
            pool.copy_slice(bytes)
                .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))
        };
        crate::mem::guard::forbid_alloc(|| {
            for specialized in [true, false] {
                let scope = sequence::enter_statement(&arena, session, false, None);
                let (key, original) = if specialized {
                    let admitted = plans
                        .runtime_routine_key(
                            RoutineInputKind::Sql,
                            100_001,
                            7,
                            b"int4",
                            |_| Ok(true),
                            freeze,
                        )
                        .unwrap();
                    (admitted.key, admitted.reserved.unwrap())
                } else {
                    let key = Key {
                        owner: Owner::Backend,
                        source: source(31),
                    };
                    (key, plans.reserve(key).unwrap())
                };
                let bytes = original.key_bytes();
                let error = sequence::statement_effect(
                    StatementEffectKind::RoutineInput,
                    &[Datum::Bytea(&bytes)],
                    StatementEffectRetry::Restart,
                    &arena,
                    || {
                        plans
                            .retain_for_statement(original, sequence::input_attempt())
                            .unwrap();
                        plans.set_waiting(original, true).unwrap();
                        StatementEffectAttempt {
                            result: Err(sql_err!(sqlstate::INTERNAL_LOCK_WAIT, "test row lock")),
                            rewound_state: false,
                        }
                    },
                )
                .unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::INTERNAL_LOCK_WAIT);
                scope.discard(session);
                drop(scope);
                let fresh = sequence::enter_statement(&arena, session, false, None);
                let replacement = if specialized {
                    let admitted = plans
                        .runtime_routine_key(
                            RoutineInputKind::Sql,
                            100_001,
                            7,
                            b"int4",
                            |_| panic!("cancelled metadata cannot be reused"),
                            freeze,
                        )
                        .unwrap();
                    admitted.reserved.unwrap()
                } else {
                    assert!(
                        plans
                            .find_validated(key, |_| panic!("cancelled metadata cannot be reused"))
                            .unwrap()
                            .is_none()
                    );
                    plans.reserve(key).unwrap()
                };
                assert_ne!(replacement, original);
                assert!(plans.lookup(original, key).is_err());
                plans.abandon(replacement).unwrap();
                fresh.finish(session).unwrap();
            }
        });
    }

    #[test]
    fn runtime_sql_body_key_exhaustion_preserves_control_and_payload_ownership() {
        let mut config = Config::default_dev();
        config.routine_input_slots = 2;
        let mut budget =
            Budget::new(RoutineInputPlans::budget_bytes(&config) + LeasePool::budget_bytes(64, 1));
        let pool = LeasePool::new(&mut budget, Purpose::TABLE_METADATA, 64, 1).unwrap();
        let mut plans = RoutineInputPlans::new(&config, &mut budget).unwrap();
        let freeze = |bytes: &[u8]| {
            pool.copy_slice(bytes)
                .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))
        };
        crate::mem::guard::forbid_alloc(|| {
            let first = plans
                .runtime_routine_key(
                    RoutineInputKind::Sql,
                    100_001,
                    7,
                    b"declared-int4",
                    |_| Ok(true),
                    freeze,
                )
                .unwrap();
            let id = first.reserved.unwrap();
            plans.adopt_preparing(id, first.key).unwrap();
            plans
                .complete(id, InputImage::Runtime(SharedSlice::EMPTY))
                .unwrap();
            let generation = plans.next_generation;
            let error = plans
                .runtime_routine_key(
                    RoutineInputKind::Sql,
                    100_001,
                    7,
                    b"declared-int8",
                    |_| Ok(true),
                    freeze,
                )
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(plans.next_generation, generation);
            assert!(
                matches!(plans.find(first.key).unwrap(), Some(Lookup::Ready(found, _)) if found == id)
            );
            assert_eq!(
                plans
                    .slots
                    .iter()
                    .filter(|slot| matches!(slot.state, State::Vacant))
                    .count(),
                1
            );
            plans.abandon(id).unwrap();
            let replacement = plans
                .runtime_routine_key(
                    RoutineInputKind::Sql,
                    100_001,
                    7,
                    b"declared-int8",
                    |_| Ok(true),
                    freeze,
                )
                .unwrap();
            assert_ne!(replacement.key, first.key);
            assert!(plans.adopt_preparing(id, replacement.key).is_err());
            plans.clear();
            plans.next_generation = u64::MAX;
            let error = plans
                .runtime_routine_key(
                    RoutineInputKind::Sql,
                    100_001,
                    7,
                    b"declared-int4",
                    |_| Ok(true),
                    |_| panic!("identity exhaustion must precede payload reservation"),
                )
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(pool.lease::<u8>(64).is_ok());
        });
    }

    #[test]
    fn invalid_runtime_body_inputs_retire_generations_and_preserve_pinned_images() {
        let mut config = Config::default_dev();
        config.routine_input_slots = 3;
        let mut budget =
            Budget::new(RoutineInputPlans::budget_bytes(&config) + LeasePool::budget_bytes(256, 4));
        let pool = LeasePool::new(&mut budget, Purpose::TABLE_METADATA, 256, 4).unwrap();
        let mut plans = RoutineInputPlans::new(&config, &mut budget).unwrap();
        let freeze = |bytes: &[u8]| {
            pool.copy_slice(bytes)
                .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))
        };
        crate::mem::guard::forbid_alloc(|| {
            let first = plans
                .runtime_routine_key(
                    RoutineInputKind::Sql,
                    100_001,
                    7,
                    b"int4",
                    |_| Ok(true),
                    freeze,
                )
                .unwrap();
            let old = first.reserved.unwrap();
            plans
                .complete(
                    old,
                    InputImage::Runtime(pool.copy_slice(b"TIP2\0\0\0\0").unwrap()),
                )
                .unwrap();
            let Some(Lookup::Ready(_, pinned)) = plans.find(first.key).unwrap() else {
                panic!("ready body")
            };
            let other = plans
                .runtime_routine_key(
                    RoutineInputKind::Sql,
                    100_002,
                    7,
                    b"int4",
                    |_| Ok(true),
                    freeze,
                )
                .unwrap();
            plans
                .complete(
                    other.reserved.unwrap(),
                    InputImage::Runtime(SharedSlice::EMPTY),
                )
                .unwrap();
            let recursive = with_owner(Owner::Nested(old), || {
                let replaced_other = plans
                    .runtime_routine_key(
                        RoutineInputKind::Sql,
                        100_002,
                        7,
                        b"int4",
                        |_| Ok(false),
                        freeze,
                    )
                    .unwrap();
                assert_ne!(replaced_other.key, other.key);
                with_owner(
                    Owner::Dynamic {
                        epoch: 1,
                        generation: 2,
                    },
                    || {
                        let replacement = plans
                            .runtime_routine_key(
                                RoutineInputKind::Sql,
                                100_001,
                                7,
                                b"int4",
                                |_| Ok(false),
                                freeze,
                            )
                            .unwrap();
                        let new = replacement.reserved.unwrap();
                        assert_ne!(new, old);
                        plans
                            .complete(new, InputImage::Runtime(SharedSlice::EMPTY))
                            .unwrap();
                        let reused = plans
                            .runtime_routine_key(
                                RoutineInputKind::Sql,
                                100_001,
                                7,
                                b"int4",
                                |_| Ok(true),
                                |_| panic!("active obsolete ancestor must not hide a current plan"),
                            )
                            .unwrap();
                        assert_eq!(reused.key, replacement.key);
                        assert!(reused.reserved.is_none());
                        assert!(matches!(
                            plans.find(first.key).unwrap(),
                            Some(Lookup::Ready(_, _))
                        ));
                        replacement
                    },
                )
            });
            assert!(matches!(
                plans.find(first.key).unwrap(),
                Some(Lookup::Ready(_, _))
            ));
            let replacement = plans
                .runtime_routine_key(
                    RoutineInputKind::Sql,
                    100_001,
                    7,
                    b"int4",
                    |_| Ok(true),
                    |_| panic!("retire only the now-inactive obsolete generation"),
                )
                .unwrap();
            assert_eq!(replacement.key, recursive.key);
            assert!(replacement.reserved.is_none());
            assert!(plans.find(first.key).unwrap().is_none());
            assert!(
                matches!(pinned, InputImage::Runtime(ref bytes) if bytes.as_ref() == b"TIP2\0\0\0\0")
            );
            drop(pinned);
        });
    }

    fn source(callsite: usize) -> Source {
        Source::SqlRoutineCallSite {
            routine_oid: 100_001,
            definition_version: 7,
            callsite,
        }
    }

    #[test]
    fn closed_cursor_owners_release_waiting_descendants_and_preserve_pinned_images() {
        let mut config = Config::default_dev();
        config.routine_input_slots = 3;
        let mut budget =
            Budget::new(RoutineInputPlans::budget_bytes(&config) + LeasePool::budget_bytes(64, 1));
        let pool = LeasePool::new(&mut budget, Purpose::TABLE_METADATA, 64, 1).unwrap();
        let mut plans = RoutineInputPlans::new(&config, &mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let bytes = pool.copy_slice(b"TIP2\0\0\0\0").unwrap();
            let root = Key {
                owner: Owner::Cursor(1),
                source: source(1),
            };
            let parent = plans.reserve(root).unwrap();
            plans
                .complete(parent, InputImage::Runtime(bytes.clone()))
                .unwrap();
            let child = Key {
                owner: Owner::Nested(parent),
                source: source(2),
            };
            let pending = plans.reserve(child).unwrap();
            plans.set_waiting(pending, true).unwrap();
            let current = Key {
                owner: Owner::Cursor(2),
                source: source(1),
            };
            let current_id = plans.reserve(current).unwrap();
            plans
                .complete(current_id, InputImage::Runtime(bytes.clone()))
                .unwrap();
            let Some(Lookup::Ready(_, pinned)) = plans.find(root).unwrap() else {
                panic!("ready cursor image");
            };
            assert!(!has_active_plan());
            with_owner(root.owner, || assert!(has_active_plan()));
            assert!(!has_active_plan());
            plans.retire_invalid_cursors(|generation| generation == 2);
            assert!(plans.find(root).unwrap().is_none());
            assert!(plans.find(child).is_err());
            assert!(plans.set_waiting(pending, false).is_err());
            assert!(
                matches!(plans.find(current).unwrap(), Some(Lookup::Ready(id, _)) if id == current_id)
            );
            plans.retire_invalid_cursors(|_| false);
            drop(bytes);
            assert!(pool.lease::<u8>(1).is_err());
            assert!(
                matches!(&pinned, InputImage::Runtime(bytes) if bytes.as_ref() == b"TIP2\0\0\0\0")
            );
            drop(pinned);
            assert!(pool.lease::<u8>(64).is_ok());
        });
    }

    #[test]
    fn rejected_inline_attempts_release_values_and_accepted_attempts_keep_one_root() {
        let mut config = Config::default_dev();
        config.routine_input_slots = 2;
        let mut budget =
            Budget::new(RoutineInputPlans::budget_bytes(&config) + LeasePool::budget_bytes(64, 1));
        let pool = LeasePool::new(&mut budget, Purpose::TABLE_METADATA, 64, 1).unwrap();
        let mut plans = RoutineInputPlans::new(&config, &mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let rejected_key = Key {
                owner: Owner::Cursor(1),
                source: source(1),
            };
            let rejected = plans.reserve(rejected_key).unwrap();
            plans
                .complete(
                    rejected,
                    InputImage::Runtime(pool.copy_slice(b"TIP2\0\0\0\0").unwrap()),
                )
                .unwrap();
            plans
                .finish_inline_attempt(rejected, InlineDisposition::Rejected)
                .unwrap();
            assert!(
                matches!(plans.find(rejected_key).unwrap(), Some(Lookup::Ready(_, InputImage::InlineAttempt { bytes, disposition: InlineDisposition::Rejected })) if bytes.is_empty())
            );
            let accepted_key = Key {
                owner: Owner::Cursor(1),
                source: source(2),
            };
            let accepted = plans.reserve(accepted_key).unwrap();
            plans
                .complete(
                    accepted,
                    InputImage::Runtime(pool.copy_slice(b"TIP2\0\0\0\0").unwrap()),
                )
                .unwrap();
            plans
                .finish_inline_attempt(accepted, InlineDisposition::Accepted)
                .unwrap();
            let Some(Lookup::Ready(
                _,
                InputImage::InlineAttempt {
                    bytes,
                    disposition: InlineDisposition::Accepted,
                },
            )) = plans.find(accepted_key).unwrap()
            else {
                panic!("accepted image");
            };
            plans.retire(Owner::Cursor(1));
            assert!(pool.lease::<u8>(1).is_err());
            assert_eq!(bytes.as_ref(), b"TIP2\0\0\0\0");
            drop(bytes);
            assert!(pool.lease::<u8>(64).is_ok());
        });
    }

    #[test]
    fn plan_inputs_keep_payloads_after_parent_retirement_and_pool_owner_drop() {
        let mut config = Config::default_dev();
        config.routine_input_slots = 2;
        let mut budget = Budget::new(
            RoutineInputPlans::budget_bytes(&config) + LeasePool::budget_bytes(1024, 4) + 4096,
        );
        let pool = LeasePool::new(&mut budget, Purpose::TABLE_METADATA, 1024, 4).unwrap();
        let mut plans = RoutineInputPlans::new(&config, &mut budget).unwrap();
        let arena = Arena::new(&mut budget, "retained plan values", 4096).unwrap();
        assert_eq!(budget.remaining(), 0);
        let root = Key {
            owner: Owner::ProtocolPrepared(11),
            source: source(7),
        };
        let (retained, old_child) = crate::mem::guard::forbid_alloc(|| {
            let bytes = pool.copy_slice(b"TIP2\0\0\0\0").unwrap();
            let parent = plans.reserve(root).unwrap();
            plans
                .complete(parent, InputImage::Runtime(bytes.clone()))
                .unwrap();
            let child = Key {
                owner: Owner::Nested(parent),
                source: source(31),
            };
            let child_id = plans.reserve(child).unwrap();
            plans
                .complete(child_id, InputImage::Runtime(bytes.clone()))
                .unwrap();
            let Some(Lookup::Ready(_, retained)) = plans.find(root).unwrap() else {
                panic!("ready root")
            };
            plans.retire(root.owner);
            assert!(plans.find(root).unwrap().is_none());
            assert!(plans.find(child).is_err());
            assert!(retained.bindings(&arena).unwrap().is_empty());
            let replacement = plans.reserve(root).unwrap();
            assert_ne!(replacement, parent);
            assert!(
                plans
                    .complete(child_id, InputImage::Runtime(bytes.clone()))
                    .is_err()
            );
            plans
                .complete(replacement, InputImage::Runtime(bytes))
                .unwrap();
            (retained, child_id)
        });
        drop(pool);
        drop(plans);
        crate::mem::guard::forbid_alloc(|| {
            assert!(retained.bindings(&arena).unwrap().is_empty());
            assert_ne!(old_child.generation, 0);
            drop(retained);
        });
    }

    #[test]
    fn input_plan_exhaustion_and_waits_preserve_ready_owners_and_exact_identities() {
        let mut config = Config::default_dev();
        config.routine_input_slots = 2;
        let mut budget = Budget::new(
            RoutineInputPlans::budget_bytes(&config) + LeasePool::budget_bytes(1024, 4),
        );
        let pool = LeasePool::new(&mut budget, Purpose::TABLE_METADATA, 1024, 4).unwrap();
        let mut plans = RoutineInputPlans::new(&config, &mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let first = Key {
                owner: Owner::SqlPrepared(3),
                source: source(0),
            };
            let second = Key {
                owner: Owner::SqlPrepared(3),
                source: source(10),
            };
            let parent = plans.reserve(first).unwrap();
            let bytes = pool.copy_slice(b"TIP2\0\0\0\0").unwrap();
            plans.complete(parent, InputImage::Runtime(bytes)).unwrap();
            let waiting = plans.reserve(second).unwrap();
            assert_eq!(
                plans.find(second).err().unwrap().sqlstate,
                sqlstate::OBJECT_IN_USE
            );
            plans.set_waiting(waiting, true).unwrap();
            assert!(
                matches!(plans.find(second).unwrap(), Some(Lookup::Waiting(found)) if found == waiting)
            );
            let third = Key {
                owner: Owner::SqlPrepared(3),
                source: source(20),
            };
            assert_eq!(
                plans.reserve(third).unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert!(
                matches!(plans.find(first).unwrap(), Some(Lookup::Ready(found, _)) if found == parent)
            );
            plans.abandon(waiting).unwrap();
            let replacement = plans.reserve(third).unwrap();
            assert_ne!(replacement, waiting);
            assert!(plans.set_waiting(waiting, false).is_err());
            plans.clear();
            assert!(plans.find(first).unwrap().is_none());
        });
    }

    #[test]
    fn retired_definitions_and_dynamic_epochs_release_only_their_owned_plans() {
        let mut config = Config::default_dev();
        config.routine_input_slots = 4;
        let mut budget = Budget::new(
            RoutineInputPlans::budget_bytes(&config) + LeasePool::budget_bytes(1024, 4),
        );
        let pool = LeasePool::new(&mut budget, Purpose::TABLE_METADATA, 1024, 4).unwrap();
        let mut plans = RoutineInputPlans::new(&config, &mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let bytes = pool.copy_slice(b"TIP2\0\0\0\0").unwrap();
            let old_key = Key {
                owner: Owner::Backend,
                source: source(1),
            };
            let old = plans.reserve(old_key).unwrap();
            plans
                .complete(old, InputImage::Runtime(bytes.clone()))
                .unwrap();
            let child_key = Key {
                owner: Owner::Nested(old),
                source: source(2),
            };
            let child = plans.reserve(child_key).unwrap();
            plans
                .complete(child, InputImage::Runtime(bytes.clone()))
                .unwrap();
            let retained = match plans.find(old_key).unwrap().unwrap() {
                Lookup::Ready(_, image) => image,
                Lookup::Waiting(_) | Lookup::Failed(_, _) => panic!("ready image"),
            };
            let next_key = Key {
                owner: Owner::Backend,
                source: Source::SqlRoutineCallSite {
                    routine_oid: 100_001,
                    definition_version: 8,
                    callsite: 1,
                },
            };
            let next = plans.reserve(next_key).unwrap();
            plans
                .complete(next, InputImage::Runtime(bytes.clone()))
                .unwrap();
            plans.retire_invalid_definitions(|oid, version| oid == 100_001 && version == 8);
            assert!(plans.find(old_key).unwrap().is_none());
            assert!(plans.find(child_key).is_err());
            assert!(
                matches!(plans.find(next_key).unwrap(), Some(Lookup::Ready(id, _)) if id == next)
            );
            assert!(
                matches!(&retained, InputImage::Runtime(value) if value.as_ref() == b"TIP2\0\0\0\0")
            );
            let dynamic_key = Key {
                owner: Owner::Dynamic {
                    epoch: 11,
                    generation: 19,
                },
                source: source(3),
            };
            let dynamic = plans.reserve(dynamic_key).unwrap();
            plans
                .complete(dynamic, InputImage::Runtime(bytes.clone()))
                .unwrap();
            let transient_key = Key {
                owner: Owner::Transient {
                    generation: 11,
                    statement: 0,
                },
                source: source(4),
            };
            let transient = plans.reserve(transient_key).unwrap();
            plans
                .complete(transient, InputImage::Runtime(bytes))
                .unwrap();
            assert!(!has_active_plan());
            with_owner(Owner::Backend, || {
                assert!(!has_active_plan());
                with_owner(Owner::Nested(next), || {
                    assert!(has_active_plan());
                    with_owner(dynamic_key.owner, || {
                        assert!(has_active_plan());
                        assert_eq!(owner().unwrap(), dynamic_key.owner);
                    });
                    assert_eq!(owner().unwrap(), Owner::Nested(next));
                });
                assert!(!has_active_plan());
                assert_eq!(owner().unwrap(), Owner::Backend);
            });
            assert!(!has_active_plan());
            plans.retire_transient(11);
            assert!(plans.find(dynamic_key).unwrap().is_none());
            assert!(plans.find(transient_key).unwrap().is_none());
            assert!(
                matches!(plans.find(next_key).unwrap(), Some(Lookup::Ready(id, _)) if id == next)
            );
            assert!(plans.set_waiting(old, false).is_err());
            drop(retained);
        });
    }
}
