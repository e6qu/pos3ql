//! Input continuations are temporary row scratch until a call actually suspends.

use super::*;
use crate::storage::{
    RoutineDef, RoutineParameterMode, StoredQueryDependency, StoredQueryDependencyView,
};
use core::marker::PhantomData;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputSite<'a> {
    Argument {
        routine: i32,
        parameter: usize,
        written: Option<usize>,
        source: i32,
        target: i32,
    },
    Call {
        name: &'a str,
        bound_oid: Option<i32>,
        arity: usize,
        variadic: bool,
    },
    Query(u8),
    Definition(i32),
}

pub(crate) fn expression_site<'a>(expression: &Expr<'a>) -> Option<InputSite<'a>> {
    match expression {
        Expr::Call {
            name,
            bound_oid,
            args,
            variadic,
            ..
        } => Some(InputSite::Call {
            name,
            bound_oid: *bound_oid,
            arity: args.len(),
            variadic: *variadic,
        }),
        Expr::Subquery(_) => Some(InputSite::Query(0)),
        Expr::RowSubquery { .. } => Some(InputSite::Query(1)),
        Expr::ArraySubquery(_) => Some(InputSite::Query(2)),
        Expr::Exists(_) => Some(InputSite::Query(3)),
        Expr::InSubquery { .. } => Some(InputSite::Query(4)),
        Expr::QuantifiedSubquery { .. } => Some(InputSite::Query(5)),
        _ => None,
    }
}

fn copy_bytes<'a>(bytes: &[u8], arena: &'a Arena) -> Result<&'a [u8], SqlError> {
    arena
        .alloc_persistent_slice_with(bytes.len(), |index| bytes[index])
        .map(|bytes| &*bytes)
        .map_err(|_| full())
}

impl InputSite<'_> {
    fn retain(self, arena: &Arena) -> Result<InputSite<'_>, SqlError> {
        Ok(match self {
            Self::Argument {
                routine,
                parameter,
                written,
                source,
                target,
            } => InputSite::Argument {
                routine,
                parameter,
                written,
                source,
                target,
            },
            Self::Call {
                name,
                bound_oid,
                arity,
                variadic,
            } => InputSite::Call {
                name: core::str::from_utf8(copy_bytes(name.as_bytes(), arena)?)
                    .expect("copied SQL identifier is UTF-8"),
                bound_oid,
                arity,
                variadic,
            },
            Self::Query(kind) => InputSite::Query(kind),
            Self::Definition(oid) => InputSite::Definition(oid),
        })
    }
}

#[derive(Clone, Copy)]
struct ParameterDefault<'a> {
    mode: u8,
    sql: Option<&'a str>,
}

#[derive(Clone, Copy)]
pub(super) struct InputDefinition<'a> {
    signature: RoutineCallContract<'a>,
    defaults: &'a [ParameterDefault<'a>],
    strict: bool,
    dependencies: &'a [StoredQueryDependency],
}

impl<'a> InputDefinition<'a> {
    fn capture(
        routine: &RoutineDef,
        slot: usize,
        storage: &Storage,
        txid: u32,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let result = match routine.kind {
            crate::storage::RoutineKind::Function { result } => RoutineCallResult::Scalar(result),
            crate::storage::RoutineKind::SetFunction { result } => RoutineCallResult::Set(result),
            crate::storage::RoutineKind::RecordFunction { set_returning } => {
                RoutineCallResult::Record { set_returning }
            }
            crate::storage::RoutineKind::TableFunction => RoutineCallResult::Table,
            _ => return Err(changed()),
        };
        let signature = RoutineCallContract::copy_signature(routine, result, |values| {
            arena
                .alloc_slice_copy(values)
                .map(|values| &*values)
                .map_err(|_| full())
        })?;
        let defaults = arena
            .alloc_slice_with(routine.parameter_count, |_| ParameterDefault {
                mode: 0,
                sql: None,
            })
            .map_err(|_| full())?;
        for (output, parameter) in defaults.iter_mut().zip(routine.parameters()) {
            output.mode = parameter.mode.code();
            if let Some(sql) = parameter.mode.default() {
                output.sql = Some(arena.alloc_str(sql.as_str()).map_err(|_| full())?);
            }
        }
        let dependencies = if defaults.iter().any(|parameter| parameter.sql.is_some()) {
            let captured = storage.snapshot_routine_dependencies_for(slot, txid, arena)?;
            &*arena
                .alloc_slice_copy(captured.entries())
                .map_err(|_| full())?
        } else {
            &[]
        };
        Ok(Self {
            signature,
            defaults,
            strict: routine.attributes.strict,
            dependencies,
        })
    }

    fn restore(&self, routine: &mut RoutineDef) -> Result<(), SqlError> {
        self.signature.bind(routine)?;
        routine.attributes.strict = self.strict;
        for ((parameter, selected), signature) in routine
            .parameters
            .iter_mut()
            .zip(self.defaults)
            .zip(self.signature.parameters)
        {
            parameter.name = signature.name;
            parameter.mode = RoutineParameterMode::from_code(
                selected.mode,
                selected.sql.map(crate::util::StackStr::from_str),
            )
            .ok_or_else(changed)?;
        }
        Ok(())
    }

    pub(super) fn dependencies(self) -> StoredQueryDependencyView<'a> {
        StoredQueryDependencyView::new(self.dependencies)
    }

    fn retain(self, arena: &Arena) -> Result<InputDefinition<'_>, SqlError> {
        let signature = RoutineCallContract {
            oid: self.signature.oid,
            result: self.signature.result,
            arguments: arena
                .alloc_persistent_slice_with(self.signature.arguments.len(), |index| {
                    self.signature.arguments[index]
                })
                .map_err(|_| full())?,
            parameters: arena
                .alloc_persistent_slice_with(self.signature.parameters.len(), |index| {
                    self.signature.parameters[index]
                })
                .map_err(|_| full())?,
            result_columns: arena
                .alloc_persistent_slice_with(self.signature.result_columns.len(), |index| {
                    self.signature.result_columns[index]
                })
                .map_err(|_| full())?,
        };
        let defaults = arena
            .alloc_persistent_slice_with(self.defaults.len(), |_| ParameterDefault {
                mode: 0,
                sql: None,
            })
            .map_err(|_| full())?;
        for (output, source) in defaults.iter_mut().zip(self.defaults) {
            output.mode = source.mode;
            output.sql = source
                .sql
                .map(|sql| {
                    core::str::from_utf8(copy_bytes(sql.as_bytes(), arena)?).map_err(|_| changed())
                })
                .transpose()?;
        }
        Ok(InputDefinition {
            signature,
            defaults,
            strict: self.strict,
            dependencies: arena
                .alloc_persistent_slice_with(self.dependencies.len(), |index| {
                    self.dependencies[index]
                })
                .map_err(|_| full())?,
        })
    }
}

#[derive(Clone, Copy)]
enum InputState<'a> {
    Pending,
    Value {
        encoded: &'a [u8],
        next_input: usize,
        next_invocation: usize,
        next_call: usize,
    },
    Definition(InputDefinition<'a>),
}

#[derive(Clone, Copy)]
struct InputFrame<'a> {
    site: InputSite<'a>,
    state: InputState<'a>,
}

#[derive(Clone, Copy)]
enum AttemptFrame<'statement, 'query> {
    Saved(&'statement InputFrame<'statement>),
    Temporary(InputFrame<'query>),
}

impl<'statement: 'query, 'query> AttemptFrame<'statement, 'query> {
    fn frame(self) -> InputFrame<'query> {
        match self {
            Self::Saved(frame) => *frame,
            Self::Temporary(frame) => frame,
        }
    }
}

#[derive(Clone, Copy)]
struct SavedInputs<'a> {
    ordinal: usize,
    frames: &'a [&'a InputFrame<'a>],
}

pub(super) struct RoutineInputs<'a> {
    arena: &'a Arena,
    next: Cell<usize>,
    saved: RefCell<ArenaList<'a, SavedInputs<'a>>>,
}

impl<'a> RoutineInputs<'a> {
    pub(super) fn new(arena: &'a Arena) -> Self {
        Self {
            arena,
            next: Cell::new(0),
            saved: RefCell::new(ArenaList::new_persistent(arena)),
        }
    }
    pub(super) fn begin_attempt(&self) {
        self.next.set(0);
    }
}

struct InputAttempt<'statement, 'query> {
    registry: &'query RoutineInputs<'statement>,
    invocation: &'query Cell<usize>,
    arena: &'query Arena,
    next: Cell<usize>,
    frames: RefCell<ArenaList<'query, AttemptFrame<'statement, 'query>>>,
}

impl<'statement: 'query, 'query> InputAttempt<'statement, 'query> {
    fn frame(&self, site: InputSite<'query>) -> Result<(usize, InputFrame<'query>), SqlError> {
        let ordinal = self.next.get();
        self.next.set(ordinal.checked_add(1).ok_or_else(full)?);
        let mut frames = self.frames.borrow_mut();
        if ordinal == frames.len() {
            frames
                .push(AttemptFrame::Temporary(InputFrame {
                    site,
                    state: InputState::Pending,
                }))
                .map_err(|_| full())?;
        }
        let frame = frames
            .as_slice()
            .get(ordinal)
            .copied()
            .ok_or_else(changed)?
            .frame();
        if frame.site != site {
            return Err(changed());
        }
        Ok((ordinal, frame))
    }

    fn value(
        &self,
        site: InputSite<'query>,
        evaluate: impl FnOnce() -> Result<Datum<'query>, SqlError>,
    ) -> Result<Datum<'query>, SqlError> {
        let (ordinal, mut frame) = self.frame(site)?;
        match frame.state {
            InputState::Value {
                encoded,
                next_input,
                next_invocation,
                next_call,
            } => {
                self.next.set(next_input);
                self.invocation.set(next_invocation);
                self.registry.next.set(next_call);
                return super::super::exec::decode_projected_col_record(encoded, 0, self.arena);
            }
            InputState::Pending => {}
            InputState::Definition(_) => return Err(changed()),
        }
        // A nested expression may append frames; never hold a borrow across it.
        let result = evaluate()?;
        frame.state = InputState::Value {
            encoded: super::super::exec::encode_projected_pub(&[result], self.arena)?,
            next_input: self.next.get(),
            next_invocation: self.invocation.get(),
            next_call: self.registry.next.get(),
        };
        self.frames
            .borrow_mut()
            .as_mut_slice()
            .map_err(|_| full())?[ordinal] = AttemptFrame::Temporary(frame);
        Ok(result)
    }

    fn definition(
        &self,
        routine: &mut RoutineDef,
        slot: usize,
        storage: &Storage,
        txid: u32,
    ) -> Result<InputDefinition<'query>, SqlError> {
        let (ordinal, mut frame) =
            self.frame(InputSite::Definition(crate::storage::routine_oid(routine)))?;
        let definition = match frame.state {
            InputState::Pending => {
                let definition =
                    InputDefinition::capture(routine, slot, storage, txid, self.arena)?;
                frame.state = InputState::Definition(definition);
                self.frames
                    .borrow_mut()
                    .as_mut_slice()
                    .map_err(|_| full())?[ordinal] = AttemptFrame::Temporary(frame);
                definition
            }
            InputState::Definition(definition) => definition,
            InputState::Value { .. } => return Err(changed()),
        };
        definition.restore(routine)?;
        Ok(definition)
    }

    fn retain(&self, ordinal: usize) -> Result<(), SqlError> {
        let arena = self.registry.arena;
        let mut copied = ArenaList::new(arena);
        for frame in self.frames.borrow().as_slice() {
            let saved = match *frame {
                AttemptFrame::Saved(frame) => frame,
                AttemptFrame::Temporary(frame) => {
                    let state = match frame.state {
                        InputState::Pending => InputState::Pending,
                        InputState::Value {
                            encoded,
                            next_input,
                            next_invocation,
                            next_call,
                        } => InputState::Value {
                            encoded: copy_bytes(encoded, arena)?,
                            next_input,
                            next_invocation,
                            next_call,
                        },
                        InputState::Definition(definition) => {
                            InputState::Definition(definition.retain(arena)?)
                        }
                    };
                    let site = frame.site.retain(arena)?;
                    &arena
                        .alloc_persistent_slice_with(1, |_| InputFrame { site, state })
                        .map_err(|_| full())?[0]
                }
            };
            copied.push(saved).map_err(|_| full())?;
        }
        let frames = arena
            .alloc_persistent_slice_with(copied.len(), |index| copied.as_slice()[index])
            .map_err(|_| full())?;
        let saved = SavedInputs { ordinal, frames };
        let mut entries = self.registry.saved.borrow_mut();
        if let Some(index) = entries
            .as_slice()
            .iter()
            .position(|entry| entry.ordinal == ordinal)
        {
            entries.as_mut_slice().map_err(|_| full())?[index] = saved;
        } else {
            entries.push(saved).map_err(|_| full())?;
        }
        Ok(())
    }
}

type ActiveInput = *const ();
thread_local! {
    static AVAILABLE_INPUT: Cell<Option<ActiveInput>> = const { Cell::new(None) };
    static ACTIVE_INPUT: Cell<Option<ActiveInput>> = const { Cell::new(None) };
}

pub(super) struct InputScope<'a> {
    previous: Option<ActiveInput>,
    _borrow: PhantomData<&'a ()>,
}
impl Drop for InputScope<'_> {
    fn drop(&mut self) {
        ACTIVE_INPUT.with(|active| active.set(self.previous));
    }
}

pub(super) fn enter() -> InputScope<'static> {
    let current = AVAILABLE_INPUT.with(Cell::get);
    InputScope {
        previous: ACTIVE_INPUT.with(|active| active.replace(current)),
        _borrow: PhantomData,
    }
}

struct AvailableScope(Option<ActiveInput>);
impl Drop for AvailableScope {
    fn drop(&mut self) {
        AVAILABLE_INPUT.with(|active| active.set(self.0));
    }
}

pub(super) fn call<T>(
    arena: &Arena,
    execute: impl FnOnce() -> Result<T, SqlError>,
) -> Result<T, SqlError> {
    if ACTIVE_INPUT.with(Cell::get).is_some() {
        return execute();
    }
    let Some((invocations, _)) = active_routine_invocations() else {
        return execute();
    };
    let registry = &invocations.inputs;
    let ordinal = registry.next.get();
    registry.next.set(ordinal.checked_add(1).ok_or_else(full)?);
    let mut frames = ArenaList::new(arena);
    if let Some(saved) = registry
        .saved
        .borrow()
        .as_slice()
        .iter()
        .find(|entry| entry.ordinal == ordinal)
    {
        for frame in saved.frames {
            frames
                .push(AttemptFrame::Saved(frame))
                .map_err(|_| full())?;
        }
    }
    let attempt = InputAttempt {
        registry,
        invocation: &invocations.next,
        arena,
        next: Cell::new(0),
        frames: RefCell::new(frames),
    };
    let _available =
        AvailableScope(AVAILABLE_INPUT.with(|active| {
            active.replace(Some(&attempt as *const InputAttempt<'_, '_> as *const ()))
        }));
    let result = execute();
    if result
        .as_ref()
        .is_err_and(|error| error.sqlstate == sqlstate::INTERNAL_ROUTINE_INVOCATION)
    {
        attempt.retain(ordinal)?;
    }
    result
}

pub(crate) fn value<'a>(
    site: InputSite<'a>,
    arena: &'a Arena,
    evaluate: impl FnOnce() -> Result<Datum<'a>, SqlError>,
) -> Result<Datum<'a>, SqlError> {
    let Some(active) = ACTIVE_INPUT.with(Cell::get) else {
        return evaluate();
    };
    // call owns this attempt until synchronous evaluation returns; enter limits
    // use to its input preparation, and both guards restore nested scopes.
    let attempt = unsafe { &*(active as *const InputAttempt<'_, 'a>) };
    debug_assert!(core::ptr::eq(arena, attempt.arena));
    attempt.value(site, evaluate)
}

pub(super) fn definition<'a>(
    routine: &mut RoutineDef,
    slot: usize,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<InputDefinition<'a>>, SqlError> {
    let Some(active) = ACTIVE_INPUT.with(Cell::get) else {
        return Ok(None);
    };
    let attempt = unsafe { &*(active as *const InputAttempt<'_, 'a>) };
    debug_assert!(core::ptr::eq(arena, attempt.arena));
    attempt.definition(routine, slot, storage, txid).map(Some)
}

fn full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "retained routine inputs exceed statement memory"
    )
}
fn changed() -> SqlError {
    sql_err!(
        sqlstate::SERIALIZATION_FAILURE,
        "retained routine input execution changed while waiting"
    )
}
