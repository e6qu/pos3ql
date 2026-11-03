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
            Self::Runtime(bytes) if bytes.is_empty() => Ok(&[]),
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
    Dynamic { epoch: u64, generation: u64 },
    ProtocolPrepared(u64),
    SqlPrepared(u64),
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

    pub(crate) fn retire_invalid_definitions(&mut self, mut visible: impl FnMut(i32, u64) -> bool) {
        loop {
            let retired = self.slots.iter().enumerate().find_map(|(index, slot)| {
                let key = match &slot.state {
                    State::Vacant => return None,
                    State::Preparing { key, .. } | State::Ready { key, .. } => key,
                };
                let (oid, version) = key.source.routine_definition()?;
                (!visible(oid, version)).then_some(PlanId {
                    slot: index,
                    generation: slot.generation,
                })
            });
            let Some(retired) = retired else { break };
            self.abandon(retired).expect("selected plan is live");
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
    struct Restore {
        owner: Option<Owner>,
        depth: usize,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            OWNER.with(|owner| owner.set(self.owner));
            ACTIVE_PLANS.with(|active| active.set(self.depth));
        }
    }
    let depth = ACTIVE_PLANS.with(core::cell::Cell::get);
    let _restore = Restore {
        owner: OWNER.with(|active| active.replace(Some(owner))),
        depth,
    };
    if matches!(owner, Owner::Nested(_)) {
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

    fn source(callsite: usize) -> Source {
        Source::SqlRoutineCallSite {
            routine_oid: 100_001,
            definition_version: 7,
            callsite,
        }
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
                Lookup::Waiting(_) => panic!("ready image"),
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
