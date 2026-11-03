//! Backend-owned checked inputs, qualified by the SQL plan that first used them.

use crate::config::Config;
use crate::mem::{Budget, BudgetError, FixedVec, arena::Arena, lease_pool::SharedSlice};
use crate::sql_err;

use super::{
    eval::{SqlError, sqlstate},
    type_inputs::Source,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlanId {
    slot: usize,
    generation: u64,
}

impl PlanId {
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
}

#[derive(Clone)]
pub(crate) enum InputImage {
    Runtime(SharedSlice<u8>),
    Stored(
        crate::storage::StoredTypeInputs,
        crate::storage::StoredInputSection,
    ),
}

impl InputImage {
    pub(crate) fn bindings<'a>(
        &self,
        arena: &'a Arena,
    ) -> Result<&'a [super::type_inputs::Binding<'a>], SqlError> {
        match self {
            Self::Runtime(bytes) => {
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
    ProtocolPrepared(u64),
    SqlPrepared(u64),
    Backend,
    Unlocated,
    Nested(PlanId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    pub owner: Owner,
    pub source: Source,
}

enum State {
    Vacant,
    Preparing { key: Key, waiting: bool },
    Ready { key: Key, inputs: InputImage },
}

struct Slot {
    generation: u64,
    state: State,
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

    pub(crate) fn reserve(&mut self, key: Key) -> Result<PlanId, SqlError> {
        if let Owner::Nested(parent) = key.owner {
            self.slot(parent)?;
        }
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
        self.next_generation = generation.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "routine-input plan identities exhausted"
            )
        })?;
        self.slots[index] = Slot {
            generation,
            state: State::Preparing {
                key,
                waiting: false,
            },
        };
        Ok(PlanId {
            slot: index,
            generation,
        })
    }

    pub(crate) fn complete(&mut self, plan: PlanId, inputs: InputImage) -> Result<(), SqlError> {
        let slot = self.slot_mut(plan)?;
        let State::Preparing { key, .. } = slot.state else {
            return Err(stale());
        };
        slot.state = State::Ready { key, inputs };
        Ok(())
    }

    pub(crate) fn set_waiting(&mut self, plan: PlanId, waiting: bool) -> Result<(), SqlError> {
        let State::Preparing { waiting: value, .. } = &mut self.slot_mut(plan)?.state else {
            return Err(stale());
        };
        *value = waiting;
        Ok(())
    }

    pub(crate) fn abandon(&mut self, plan: PlanId) -> Result<(), SqlError> {
        self.slot_mut(plan)?.state = State::Vacant;
        self.retire(Owner::Nested(plan));
        Ok(())
    }

    pub(crate) fn retire(&mut self, owner: Owner) {
        for slot in self.slots.iter_mut() {
            let key = match &slot.state {
                State::Vacant => continue,
                State::Preparing { key, .. } | State::Ready { key, .. } => *key,
            };
            if key.owner == owner {
                slot.state = State::Vacant;
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
                    } => *parent,
                    _ => continue,
                };
                if self.slot(parent).is_err() {
                    self.slots[index].state = State::Vacant;
                    retired = true;
                }
            }
            if !retired {
                break;
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        for slot in self.slots.iter_mut() {
            slot.state = State::Vacant;
        }
    }

    pub(crate) fn retire_transient(&mut self, generation: u64) {
        loop {
            let owner = self.slots.iter().find_map(|slot| {
                let key = match &slot.state {
                    State::Preparing { key, .. } | State::Ready { key, .. } => key,
                    State::Vacant => return None,
                };
                matches!(key.owner, Owner::Transient { generation: found, .. } if found == generation)
                    .then_some(key.owner)
            });
            let Some(owner) = owner else {
                break;
            };
            self.retire(owner);
        }
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
    struct Restore(Option<Owner>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OWNER.with(|owner| owner.set(self.0));
        }
    }
    let _restore = Restore(OWNER.with(|active| active.replace(Some(owner))));
    run()
}
