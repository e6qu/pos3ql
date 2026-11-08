//! Input continuations are temporary row scratch until a call actually suspends.

use super::*;
use crate::mem::arena::{RetainedList, RetainedListError};
use crate::storage::{
    RoutineDef, RoutineParameterMode, StoredQueryDependency, StoredQueryDependencyView,
};

pub(crate) type InputSite<'a> = InputPosition<&'a str>;
type OwnedInputSite = InputPosition<crate::mem::arena::ArenaSliceHandle<u8>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputPosition<Name> {
    Argument {
        routine: i32,
        parameter: usize,
        written: Option<usize>,
        source: i32,
        target: i32,
    },
    Call {
        name: Name,
        kind: crate::sql::ast::CallKind,
        bound_oid: Option<i32>,
        arity: usize,
        variadic: bool,
    },
    Query(InputQuery),
    Expression(InputNode),
    Classification {
        routine: i32,
        parameter: usize,
        written: Option<usize>,
    },
    Definition(i32),
    CastDefinition {
        source: i32,
        target: i32,
        context: super::super::eval::DefinedCastContext,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputNode {
    Value,
    Reference,
    Unary,
    Binary,
    Cast,
    Composite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputQuery {
    Scalar,
    Row,
    Array,
    Exists,
    In,
    Quantified,
}

pub(crate) fn expression_site<'a>(expression: &Expr<'a>) -> InputSite<'a> {
    match expression {
        Expr::Call {
            name,
            kind,
            bound_oid,
            args,
            variadic,
            ..
        } => InputSite::Call {
            name,
            kind: *kind,
            bound_oid: *bound_oid,
            arity: args.len(),
            variadic: *variadic,
        },
        Expr::Subquery(_) => InputSite::Query(InputQuery::Scalar),
        Expr::RowSubquery { .. } => InputSite::Query(InputQuery::Row),
        Expr::ArraySubquery(_) => InputSite::Query(InputQuery::Array),
        Expr::Exists(_) => InputSite::Query(InputQuery::Exists),
        Expr::InSubquery { .. } => InputSite::Query(InputQuery::In),
        Expr::QuantifiedSubquery { .. } => InputSite::Query(InputQuery::Quantified),
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::NumericLit(_)
        | Expr::Str(_)
        | Expr::BoundLiteral { .. }
        | Expr::BitLit(_) => InputSite::Expression(InputNode::Value),
        Expr::Column { .. }
        | Expr::RoutineParam { .. }
        | Expr::Param(_)
        | Expr::WholeRow(_)
        | Expr::SchemaColumn { .. } => InputSite::Expression(InputNode::Reference),
        Expr::Unary { .. } => InputSite::Expression(InputNode::Unary),
        Expr::Binary { .. } => InputSite::Expression(InputNode::Binary),
        Expr::Cast { .. } => InputSite::Expression(InputNode::Cast),
        _ => InputSite::Expression(InputNode::Composite),
    }
}

impl<Name> InputPosition<Name> {
    fn map_name<Output>(
        self,
        copy: impl FnOnce(Name) -> Result<Output, SqlError>,
    ) -> Result<InputPosition<Output>, SqlError> {
        Ok(match self {
            Self::Argument {
                routine,
                parameter,
                written,
                source,
                target,
            } => InputPosition::Argument {
                routine,
                parameter,
                written,
                source,
                target,
            },
            Self::Call {
                name,
                kind,
                bound_oid,
                arity,
                variadic,
            } => InputPosition::Call {
                name: copy(name)?,
                kind,
                bound_oid,
                arity,
                variadic,
            },
            Self::Query(kind) => InputPosition::Query(kind),
            Self::Expression(kind) => InputPosition::Expression(kind),
            Self::Definition(oid) => InputPosition::Definition(oid),
            Self::CastDefinition {
                source,
                target,
                context,
            } => InputPosition::CastDefinition {
                source,
                target,
                context,
            },
            Self::Classification {
                routine,
                parameter,
                written,
            } => InputPosition::Classification {
                routine,
                parameter,
                written,
            },
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
        let signature = RoutineCallContract::copy_signature(
            routine,
            result,
            |values| {
                arena
                    .alloc_slice_copy(values)
                    .map(|values| &*values)
                    .map_err(|_| full())
            },
            |parameters| {
                arena
                    .alloc_slice_with(parameters.len(), |index| {
                        let parameter = &parameters[index];
                        crate::storage::RoutineArgumentDef {
                            name: parameter.name,
                            ctype: parameter.ctype,
                            user_type: parameter.user_type,
                        }
                    })
                    .map(|values| &*values)
                    .map_err(|_| full())
            },
        )?;
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
}

#[derive(Clone, Copy)]
enum InputValue<'a> {
    Evaluated(&'a Datum<'a>),
    Retained(crate::mem::arena::ArenaSliceHandle<u8>),
}

#[derive(Clone, Copy)]
enum InputState<'a> {
    Pending,
    Value {
        value: InputValue<'a>,
        next_input: usize,
        next_invocation: usize,
        next_call: usize,
        replay: Option<&'a super::super::sequence::StatementReplayCursor>,
    },
    Definition(&'a InputDefinition<'a>),
    CastDefinition(Option<&'a super::super::eval::BoundCast>),
}

#[derive(Clone, Copy)]
struct InputFrame<'a> {
    site: InputSite<'a>,
    state: InputState<'a>,
}

#[derive(Clone, Copy)]
enum AttemptFrame<'query> {
    Saved(crate::mem::arena::ArenaHandle<saved::OwnedInputFrame>),
    Temporary(&'query Cell<InputFrame<'query>>),
}

impl<'query> AttemptFrame<'query> {
    fn frame(self, source: &Arena, output: &'query Arena) -> Result<InputFrame<'query>, SqlError> {
        match self {
            Self::Saved(handle) => source
                .retained(handle)
                .ok_or_else(changed)?
                .load(source, output),
            Self::Temporary(frame) => Ok(frame.get()),
        }
    }

    fn definition_oid(self, source: &Arena) -> Result<Option<i32>, SqlError> {
        match self {
            Self::Saved(handle) => source
                .retained(handle)
                .ok_or_else(changed)?
                .definition_oid(source),
            Self::Temporary(frame) => Ok(definition_oid(frame.get())),
        }
    }
}

#[derive(Clone, Copy)]
struct SavedInputs {
    ordinal: usize,
    frames:
        crate::mem::arena::ArenaSliceHandle<crate::mem::arena::ArenaHandle<saved::OwnedInputFrame>>,
}

mod saved;

#[derive(Clone, Copy)]
struct CastSelection {
    source: i32,
    target: i32,
    context: super::super::eval::DefinedCastContext,
    cast: Option<crate::mem::arena::ArenaHandle<super::super::eval::BoundCast>>,
}

impl CastSelection {
    fn value(self, arena: &Arena) -> Result<Option<super::super::eval::BoundCast>, SqlError> {
        self.cast
            .map(|handle| arena.retained(handle).ok_or_else(changed))
            .transpose()
    }

    fn key(self) -> (i32, i32, u8) {
        use super::super::eval::DefinedCastContext;
        (
            self.source,
            self.target,
            match self.context {
                DefinedCastContext::Implicit => 0,
                DefinedCastContext::Assignment => 1,
                DefinedCastContext::Explicit => 2,
            },
        )
    }
}

pub(super) struct RoutineInputs {
    next: Cell<usize>,
    saved: RefCell<RetainedList<SavedInputs>>,
    evaluations: RefCell<RetainedList<(i32, super::super::eval::ExpressionEvaluation)>>,
    cast_selections: RefCell<RetainedList<CastSelection>>,
}

impl RoutineInputs {
    pub(super) fn relocate(&self, source: &Arena, target: &Arena) -> Result<Self, SqlError> {
        let output = Self::new();
        output.next.set(self.next.get());
        let saved = self.saved.borrow();
        for index in 0..saved.len() {
            let entry = saved
                .get(source, index)
                .map_err(|_| changed())?
                .ok_or_else(changed)?;
            let frames = source
                .retained_slice_mut(entry.frames)
                .map_err(|_| changed())?;
            let copied = target
                .retain_slice_with(frames.len(), |index| frames[index])
                .map_err(|_| full())?;
            for (index, handle) in frames.iter().copied().enumerate() {
                let frame = source.retained(handle).ok_or_else(changed)?;
                let frame = frame.relocate(source, target)?;
                let frame = target.retain(frame).map_err(|_| full())?;
                target.retained_slice_mut(copied).map_err(|_| changed())?[index] = frame;
            }
            output
                .saved
                .borrow_mut()
                .push(
                    target,
                    SavedInputs {
                        ordinal: entry.ordinal,
                        frames: copied,
                    },
                )
                .map_err(directory_error)?;
        }
        let evaluations = self.evaluations.borrow();
        for index in 0..evaluations.len() {
            let entry = evaluations
                .get(source, index)
                .map_err(|_| changed())?
                .ok_or_else(changed)?;
            output
                .evaluations
                .borrow_mut()
                .push(target, entry)
                .map_err(directory_error)?;
        }
        let casts = self.cast_selections.borrow();
        for index in 0..casts.len() {
            let entry = casts
                .get(source, index)
                .map_err(|_| changed())?
                .ok_or_else(changed)?;
            let cast = entry
                .value(source)?
                .map(|cast| target.retain(cast).map_err(|_| full()))
                .transpose()?;
            output
                .cast_selections
                .borrow_mut()
                .push(target, CastSelection { cast, ..entry })
                .map_err(directory_error)?;
        }
        Ok(output)
    }

    pub(super) fn new() -> Self {
        Self {
            next: Cell::new(0),
            saved: RefCell::new(RetainedList::new()),
            evaluations: RefCell::new(RetainedList::new()),
            cast_selections: RefCell::new(RetainedList::new()),
        }
    }
    pub(super) fn begin_attempt(&self) {
        self.next.set(0);
    }
}

struct InputAttempt<'statement, 'query> {
    registry: &'query RoutineInputs,
    owner: &'statement Arena,
    invocation: &'query Cell<usize>,
    arena: &'query Arena,
    next: Cell<usize>,
    frames: RefCell<ArenaList<'query, AttemptFrame<'query>>>,
}

impl<'statement: 'query, 'query> InputAttempt<'statement, 'query> {
    fn frame(&self, site: InputSite<'_>) -> Result<(usize, InputFrame<'query>), SqlError> {
        let ordinal = self.next.get();
        self.next.set(ordinal.checked_add(1).ok_or_else(full)?);
        let mut frames = self.frames.borrow_mut();
        if ordinal == frames.len() {
            let site = match site {
                InputSite::Argument {
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
                InputSite::Call {
                    name,
                    kind,
                    bound_oid,
                    arity,
                    variadic,
                } => InputSite::Call {
                    name: self.arena.alloc_str(name).map_err(|_| full())?,
                    kind,
                    bound_oid,
                    arity,
                    variadic,
                },
                InputSite::Query(kind) => InputSite::Query(kind),
                InputSite::Expression(kind) => InputSite::Expression(kind),
                InputSite::Definition(oid) => InputSite::Definition(oid),
                InputSite::CastDefinition {
                    source,
                    target,
                    context,
                } => InputSite::CastDefinition {
                    source,
                    target,
                    context,
                },
                InputSite::Classification {
                    routine,
                    parameter,
                    written,
                } => InputSite::Classification {
                    routine,
                    parameter,
                    written,
                },
            };
            frames
                .push(AttemptFrame::Temporary(Cell::from_mut(
                    self.arena
                        .alloc(InputFrame {
                            site,
                            state: InputState::Pending,
                        })
                        .map_err(|_| full())?,
                )))
                .map_err(|_| full())?;
        }
        let frame = frames
            .borrowed_slice()
            .get(ordinal)
            .copied()
            .ok_or_else(changed)?
            .frame(self.owner, self.arena)?;
        if frame.site != site {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "retained input {} changed: {:?} to {:?}",
                ordinal,
                frame.site,
                site
            ));
        }
        Ok((ordinal, frame))
    }

    fn update_frame(&self, ordinal: usize, frame: InputFrame<'query>) -> Result<(), SqlError> {
        let mut frames = self.frames.borrow_mut();
        let selected = frames
            .as_mut_slice()
            .map_err(|_| full())?
            .get_mut(ordinal)
            .ok_or_else(changed)?;
        match selected {
            AttemptFrame::Temporary(current) => current.set(frame),
            AttemptFrame::Saved(_) => {
                *selected = AttemptFrame::Temporary(Cell::from_mut(
                    self.arena.alloc(frame).map_err(|_| full())?,
                ));
            }
        }
        Ok(())
    }

    fn value<'value: 'query>(
        &self,
        site: InputSite<'_>,
        output: &'value Arena,
        evaluate: impl FnOnce() -> Result<Datum<'value>, SqlError>,
    ) -> Result<Datum<'value>, SqlError> {
        let (ordinal, mut frame) = self.frame(site)?;
        match frame.state {
            InputState::Value {
                value,
                next_input,
                next_invocation,
                next_call,
                replay,
            } => {
                super::super::sequence::restore_replay_cursor(
                    replay
                        .copied()
                        .unwrap_or(super::super::sequence::StatementReplayCursor::EMPTY),
                )?;
                self.next.set(next_input);
                self.invocation.set(next_invocation);
                self.registry.next.set(next_call);
                return match value {
                    InputValue::Retained(handle) => {
                        let bytes = self
                            .owner
                            .retained_slice_mut(handle)
                            .map_err(|_| changed())?;
                        let encoded = output.alloc_slice_copy(&bytes).map_err(|_| full())?;
                        super::super::exec::decode_projected_col_record(encoded, 0, output)
                    }
                    InputValue::Evaluated(value) => {
                        super::super::exec::detach_routine_datum(*value, output)
                    }
                };
            }
            InputState::Pending => {}
            InputState::Definition(_) | InputState::CastDefinition(_) => return Err(changed()),
        }
        // A nested expression may append frames; never hold a borrow across it.
        let before = super::super::sequence::begin_replay_cursor()?;
        let result = evaluate()?;
        let replay = super::super::sequence::finish_replay_cursor(before, self.arena)?;
        // Ordinary arguments without a replay context need no cursor payload.
        // Keep large live cursors outside the geometrically grown frame list.
        let replay = if replay.is_empty() {
            None
        } else {
            Some(&*self.arena.alloc(replay).map_err(|_| full())?)
        };
        frame.state = InputState::Value {
            value: InputValue::Evaluated(self.arena.alloc(result).map_err(|_| full())?),
            next_input: self.next.get(),
            next_invocation: self.invocation.get(),
            next_call: self.registry.next.get(),
            replay,
        };
        self.update_frame(ordinal, frame)?;
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
                let definition: &'query InputDefinition<'query> = self
                    .arena
                    .alloc(InputDefinition::capture(
                        routine, slot, storage, txid, self.arena,
                    )?)
                    .map_err(|_| full())?;
                frame.state = InputState::Definition(definition);
                self.update_frame(ordinal, frame)?;
                definition
            }
            InputState::Definition(definition) => definition,
            InputState::Value { .. } | InputState::CastDefinition(_) => return Err(changed()),
        };
        definition.restore(routine)?;
        Ok(*definition)
    }

    fn cast_definition(
        &self,
        source: i32,
        target: i32,
        context: super::super::eval::DefinedCastContext,
        select: impl FnOnce() -> Result<Option<super::super::eval::BoundCast>, SqlError>,
    ) -> Result<Option<super::super::eval::BoundCast>, SqlError> {
        let (ordinal, mut frame) = self.frame(InputSite::CastDefinition {
            source,
            target,
            context,
        })?;
        match frame.state {
            InputState::Pending => {
                let selected = select()?;
                let definition = match selected {
                    Some(definition) => Some(&*self.arena.alloc(definition).map_err(|_| full())?),
                    None => None,
                };
                frame.state = InputState::CastDefinition(definition);
                self.update_frame(ordinal, frame)?;
                Ok(selected)
            }
            InputState::CastDefinition(selected) => Ok(selected.copied()),
            InputState::Value { .. } | InputState::Definition(_) => Err(changed()),
        }
    }

    fn retain(&self, ordinal: usize) -> Result<(), SqlError> {
        let arena = self.owner;
        // The parent front may be lent to the current query. Build this
        // directory entirely in retained storage before publishing the entry.
        let mut copied = ArenaList::new_persistent(arena);
        for frame in self.frames.borrow().borrowed_slice() {
            let saved = match *frame {
                AttemptFrame::Saved(handle) => handle,
                AttemptFrame::Temporary(frame) => {
                    let saved = saved::OwnedInputFrame::capture(frame.get(), arena)?;
                    arena.retain(saved).map_err(|_| full())?
                }
            };
            copied.push(saved).map_err(|_| full())?;
        }
        let frames = arena
            .retain_slice_with(copied.len(), |index| copied.borrowed_slice()[index])
            .map_err(|_| full())?;
        let saved = SavedInputs { ordinal, frames };
        let mut entries = self.registry.saved.borrow_mut();
        let position = entries
            .with_slice(arena, |values| {
                values.binary_search_by_key(&ordinal, |entry| entry.ordinal)
            })
            .map_err(|_| changed())?;
        match position {
            Ok(index) => entries
                .with_mut_slice(arena, |values| values[index] = saved)
                .map_err(|_| changed())?,
            Err(index) => {
                entries.push(arena, saved).map_err(directory_error)?;
                entries
                    .with_mut_slice(arena, |values| values[index..].rotate_right(1))
                    .map_err(|_| changed())?;
            }
        }
        Ok(())
    }
}

type ActiveInput = *const ();
thread_local! {
    static AVAILABLE_INPUT: Cell<Option<ActiveInput>> = const { Cell::new(None) };
    static ACTIVE_INPUT: Cell<Option<ActiveInput>> = const { Cell::new(None) };
    static CURRENT_CALL: Cell<Option<usize>> = const { Cell::new(None) };
    static CURRENT_SOURCE_POSITION: Cell<Option<super::InvocationSite>> = const { Cell::new(None) };
    static CURRENT_ASSIGNMENT_SITE: Cell<Option<crate::sql::type_inputs::AssignmentSite>> = const { Cell::new(None) };
    static IN_BODY: Cell<bool> = const { Cell::new(false) };
}

/// Replanning a paused outer query cannot adopt a function's new volatility.
/// Keep one decision per catalog identity, rather than one allocation per row.
pub(super) fn evaluation(
    oid: i32,
    current: super::super::eval::ExpressionEvaluation,
) -> Result<super::super::eval::ExpressionEvaluation, SqlError> {
    if IN_BODY.with(Cell::get) {
        return Ok(current);
    }
    with_active_routine_invocations(|context| {
        let Some((invocations, arena)) = context else {
            return Ok(current);
        };
        invocations.require_arena(arena)?;
        let mut entries = invocations.inputs.evaluations.borrow_mut();
        let position = entries
            .with_slice(arena, |values| {
                values.binary_search_by_key(&oid, |entry| entry.0)
            })
            .map_err(|_| changed())?;
        match position {
            Ok(index) => Ok(entries
                .get(arena, index)
                .map_err(|_| changed())?
                .ok_or_else(changed)?
                .1),
            Err(index) => {
                entries
                    .push(arena, (oid, current))
                    .map_err(directory_error)?;
                entries
                    .with_mut_slice(arena, |values| values[index..].rotate_right(1))
                    .map_err(|_| changed())?;
                Ok(current)
            }
        }
    })
}

pub(crate) struct ExpressionPosition {
    previous: Option<usize>,
    previous_source: Option<super::InvocationSite>,
    pub(crate) target: Option<i32>,
    _thread: core::marker::PhantomData<*const ()>,
}
impl Drop for ExpressionPosition {
    fn drop(&mut self) {
        CURRENT_CALL.with(|current| current.set(self.previous));
        CURRENT_SOURCE_POSITION.with(|current| current.set(self.previous_source));
    }
}

fn definition_oid(frame: InputFrame<'_>) -> Option<i32> {
    match frame.state {
        InputState::Definition(definition) => Some(definition.signature.oid),
        _ => None,
    }
}

pub(super) fn selected_target() -> Result<Option<i32>, SqlError> {
    let Some(active) = AVAILABLE_INPUT.with(Cell::get) else {
        return Ok(None);
    };
    // AvailableScope keeps the stack-owned attempt live during this lookup.
    let attempt = unsafe { &*(active as *const InputAttempt<'_, '_>) };
    attempt
        .frames
        .borrow()
        .borrowed_slice()
        .get(attempt.next.get())
        .copied()
        .map(|frame| frame.definition_oid(attempt.owner))
        .transpose()
        .map(Option::flatten)
}

pub(crate) fn enter_expression(expression: &Expr<'_>) -> Result<ExpressionPosition, SqlError> {
    let previous = CURRENT_CALL.with(Cell::get);
    let source = match expression {
        Expr::Call {
            source_position, ..
        }
        | Expr::Cast {
            source_position, ..
        }
        | Expr::Binary {
            source_position, ..
        }
        | Expr::Unary {
            source_position, ..
        } => source_position
            .get()
            .map(super::InvocationSite::Expression)
            .or_else(|| CURRENT_SOURCE_POSITION.with(Cell::get)),
        _ => CURRENT_SOURCE_POSITION.with(Cell::get),
    };
    let previous_source = CURRENT_SOURCE_POSITION.with(|current| current.replace(source));
    let mut position = ExpressionPosition {
        previous,
        previous_source,
        target: None,
        _thread: core::marker::PhantomData,
    };
    if !matches!(expression, Expr::Call { .. }) {
        return Ok(position);
    }
    with_active_routine_invocations(|context| {
        let Some((invocations, statement_arena)) = context else {
            return Ok(position);
        };
        invocations.require_arena(statement_arena)?;
        let registry = &invocations.inputs;
        let ordinal = registry.next.get();
        registry.next.set(ordinal.checked_add(1).ok_or_else(full)?);
        CURRENT_CALL.with(|current| current.set(Some(ordinal)));
        let target = if ACTIVE_INPUT.with(Cell::get).is_some() {
            selected_target()?
        } else {
            let saved = registry
                .saved
                .borrow()
                .with_slice(statement_arena, |values| {
                    values
                        .binary_search_by_key(&ordinal, |entry| entry.ordinal)
                        .ok()
                        .map(|index| values[index])
                })
                .map_err(|_| changed())?;
            match saved {
                Some(saved) => {
                    let frames = statement_arena
                        .retained_slice_mut(saved.frames)
                        .map_err(|_| changed())?;
                    frames
                        .first()
                        .copied()
                        .map(|handle| {
                            statement_arena
                                .retained(handle)
                                .ok_or_else(changed)?
                                .definition_oid(statement_arena)
                        })
                        .transpose()?
                        .flatten()
                }
                None => None,
            }
        };
        position.target = target;
        Ok(position)
    })
}

pub(crate) fn invocation_site() -> Option<super::InvocationSite> {
    CURRENT_SOURCE_POSITION.with(Cell::get).or_else(|| {
        CURRENT_ASSIGNMENT_SITE
            .with(Cell::get)
            .map(super::InvocationSite::Assignment)
    })
}

pub(crate) struct ArrayElementScope {
    previous: Option<super::InvocationSite>,
    _thread: core::marker::PhantomData<*const ()>,
}

impl Drop for ArrayElementScope {
    fn drop(&mut self) {
        CURRENT_SOURCE_POSITION.with(|current| current.set(self.previous));
    }
}

pub(crate) fn enter_array_element(site: super::InvocationSite) -> ArrayElementScope {
    ArrayElementScope {
        previous: CURRENT_SOURCE_POSITION.with(|current| current.replace(Some(site))),
        _thread: core::marker::PhantomData,
    }
}

pub(crate) struct AssignmentScope {
    previous: Option<crate::sql::type_inputs::AssignmentSite>,
    previous_source: Option<super::InvocationSite>,
    _thread: core::marker::PhantomData<*const ()>,
}

impl Drop for AssignmentScope {
    fn drop(&mut self) {
        CURRENT_ASSIGNMENT_SITE.with(|current| current.set(self.previous));
        CURRENT_SOURCE_POSITION.with(|current| current.set(self.previous_source));
    }
}

pub(crate) fn enter_assignment(
    site: Option<crate::sql::type_inputs::AssignmentSite>,
) -> AssignmentScope {
    AssignmentScope {
        previous: CURRENT_ASSIGNMENT_SITE.with(|current| current.replace(site)),
        previous_source: CURRENT_SOURCE_POSITION.with(|current| current.replace(None)),
        _thread: core::marker::PhantomData,
    }
}

pub(crate) struct InputScope {
    previous: Option<ActiveInput>,
}
impl Drop for InputScope {
    fn drop(&mut self) {
        ACTIVE_INPUT.with(|active| active.set(self.previous));
    }
}

pub(crate) fn enter() -> InputScope {
    let current = AVAILABLE_INPUT.with(Cell::get);
    InputScope {
        previous: ACTIVE_INPUT.with(|active| active.replace(current)),
    }
}

struct AvailableScope(Option<ActiveInput>);
impl Drop for AvailableScope {
    fn drop(&mut self) {
        AVAILABLE_INPUT.with(|active| active.set(self.0));
    }
}

/// A function body and a subquery own their row scratch and nested input roots.
/// They must not append parent-input frames inside a recycling scan's frontier.
pub(crate) struct BodyScope {
    active: Option<ActiveInput>,
    available: Option<ActiveInput>,
    expression: Option<usize>,
    source_position: Option<super::InvocationSite>,
    assignment: Option<crate::sql::type_inputs::AssignmentSite>,
    in_body: bool,
}
impl Drop for BodyScope {
    fn drop(&mut self) {
        ACTIVE_INPUT.with(|active| active.set(self.active));
        AVAILABLE_INPUT.with(|active| active.set(self.available));
        CURRENT_CALL.with(|current| current.set(self.expression));
        CURRENT_SOURCE_POSITION.with(|current| current.set(self.source_position));
        CURRENT_ASSIGNMENT_SITE.with(|current| current.set(self.assignment));
        IN_BODY.with(|current| current.set(self.in_body));
    }
}
pub(crate) fn enter_body() -> BodyScope {
    BodyScope {
        active: ACTIVE_INPUT.with(|active| active.replace(None)),
        available: AVAILABLE_INPUT.with(|active| active.replace(None)),
        expression: CURRENT_CALL.with(|current| current.replace(None)),
        source_position: CURRENT_SOURCE_POSITION.with(|current| current.replace(None)),
        assignment: CURRENT_ASSIGNMENT_SITE.with(|current| current.replace(None)),
        in_body: IN_BODY.with(|current| current.replace(true)),
    }
}

pub(crate) fn call<T>(
    arena: &Arena,
    execute: impl FnOnce() -> Result<T, SqlError>,
) -> Result<T, SqlError> {
    if ACTIVE_INPUT.with(Cell::get).is_some() {
        return execute();
    }
    with_active_routine_invocations(|context| {
        let Some((invocations, statement_arena)) = context else {
            return execute();
        };
        invocations.require_arena(statement_arena)?;
        let registry = &invocations.inputs;
        let ordinal = match CURRENT_CALL.with(Cell::get) {
            Some(ordinal) => ordinal,
            None => {
                let ordinal = registry.next.get();
                registry.next.set(ordinal.checked_add(1).ok_or_else(full)?);
                ordinal
            }
        };
        let mut frames = ArenaList::new(arena);
        let saved = registry
            .saved
            .borrow()
            .with_slice(statement_arena, |values| {
                values
                    .binary_search_by_key(&ordinal, |entry| entry.ordinal)
                    .ok()
                    .map(|index| values[index])
            })
            .map_err(|_| changed())?;
        if let Some(saved) = saved {
            let retained = statement_arena
                .retained_slice_mut(saved.frames)
                .map_err(|_| changed())?;
            for frame in retained.iter().copied() {
                frames
                    .push(AttemptFrame::Saved(frame))
                    .map_err(|_| full())?;
            }
        }
        let attempt = InputAttempt {
            registry,
            owner: statement_arena,
            invocation: &invocations.next,
            arena,
            next: Cell::new(0),
            frames: RefCell::new(frames),
        };
        let _available = AvailableScope(AVAILABLE_INPUT.with(|active| {
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
    })
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
    attempt.value(site, arena, evaluate)
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
    let _ = arena;
    attempt.definition(routine, slot, storage, txid).map(Some)
}

/// Planning keeps one selected path per type/context without advancing an
/// executing expression's input cursor. New function bodies get fresh choices.
pub(super) fn cast_selection(
    source: i32,
    target: i32,
    context: super::super::eval::DefinedCastContext,
    select: impl FnOnce() -> Result<Option<super::super::eval::BoundCast>, SqlError>,
) -> Result<Option<super::super::eval::BoundCast>, SqlError> {
    if IN_BODY.with(Cell::get) {
        return select();
    }
    with_active_routine_invocations(|active_context| {
        let Some((invocations, arena)) = active_context else {
            return select();
        };
        invocations.require_arena(arena)?;
        let key = CastSelection {
            source,
            target,
            context,
            cast: None,
        }
        .key();
        let existing = invocations
            .inputs
            .cast_selections
            .borrow()
            .with_slice(arena, |values| {
                values
                    .binary_search_by_key(&key, |entry| entry.key())
                    .ok()
                    .map(|index| values[index])
            })
            .map_err(|_| changed())?;
        if let Some(existing) = existing {
            return existing.value(arena);
        }
        let selected = select()?;
        let cast = selected
            .map(|cast| arena.retain(cast).map_err(|_| full()))
            .transpose()?;
        let mut entries = invocations.inputs.cast_selections.borrow_mut();
        let position = entries
            .with_slice(arena, |values| {
                values.binary_search_by_key(&key, |entry| entry.key())
            })
            .map_err(|_| changed())?;
        match position {
            Ok(index) => {
                return entries
                    .get(arena, index)
                    .map_err(|_| changed())?
                    .ok_or_else(changed)?
                    .value(arena);
            }
            Err(index) => {
                entries
                    .push(
                        arena,
                        CastSelection {
                            source,
                            target,
                            context,
                            cast,
                        },
                    )
                    .map_err(directory_error)?;
                entries
                    .with_mut_slice(arena, |values| values[index..].rotate_right(1))
                    .map_err(|_| changed())?;
            }
        }
        Ok(selected)
    })
}

pub(super) fn cast_definition(
    source: i32,
    target: i32,
    context: super::super::eval::DefinedCastContext,
    select: impl FnOnce() -> Result<Option<super::super::eval::BoundCast>, SqlError>,
) -> Result<Option<super::super::eval::BoundCast>, SqlError> {
    let select = || cast_selection(source, target, context, select);
    let Some(active) = ACTIVE_INPUT.with(Cell::get) else {
        return select();
    };
    // Execution has its own ordinal frames; the type/context selection is
    // shared with planning and cannot change after a source effect.
    let attempt = unsafe { &*(active as *const InputAttempt<'_, '_>) };
    attempt.cast_definition(source, target, context, select)
}

fn directory_error(error: RetainedListError) -> SqlError {
    match error {
        RetainedListError::Capacity(_) => full(),
        RetainedListError::Storage(_) => changed(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;

    #[test]
    fn invocation_registry_relocates_checked_inputs_results_and_cast_selections() {
        let mut budget = Budget::new(64 << 10);
        let owner = Arena::new(&mut budget, "retained query receipts", 32 << 10).unwrap();
        let mut work = Arena::new(&mut budget, "checked query inputs", 16 << 10).unwrap();
        let invocations = crate::mem::guard::forbid_alloc(|| {
            owner
                .with_remaining_region(|query| {
                    let invocations = RoutineInvocationState::new(query);
                    with_routine_invocation_scope(
                        Some(RoutineInvocationContext::new(&invocations, query)),
                        || {
                            let result = call(&work, || {
                                let _input = enter();
                                let text = work.alloc_str("saved input").unwrap();
                                value(InputSite::Query(InputQuery::Scalar), &work, || {
                                    Ok(Datum::Text(text))
                                })?;
                                Err::<(), _>(sql_err!(
                                    sqlstate::INTERNAL_ROUTINE_INVOCATION,
                                    "input handoff"
                                ))
                            });
                            assert_eq!(
                                result.unwrap_err().sqlstate,
                                sqlstate::INTERNAL_ROUTINE_INVOCATION
                            );
                            invocations
                                .complete(
                                    OwnedRoutineResult::capture_scalar(Datum::Int4(42), query)
                                        .unwrap(),
                                    query,
                                    &work,
                                )
                                .unwrap();
                            evaluation(77, crate::sql::eval::ExpressionEvaluation::Runtime)
                                .unwrap();
                            cast_selection(
                                23,
                                25,
                                crate::sql::eval::DefinedCastContext::Explicit,
                                || Ok(Some(captured_cast(99))),
                            )
                            .unwrap();
                        },
                    );
                    invocations.relocate(query, &owner).unwrap()
                })
                .unwrap()
        });
        let mut moved = owner;
        crate::mem::guard::forbid_alloc(|| {
            moved.recycle_front();
            moved.alloc_slice_with(4096, |_| 0xccu8).unwrap();
            let retained = moved.used();
            with_routine_invocation_scope(
                Some(RoutineInvocationContext::new(&invocations, &moved)),
                || {
                    for _ in 0..8 {
                        work.reset();
                        invocations.begin_attempt();
                        let result = call(&work, || {
                            let _input = enter();
                            value(InputSite::Query(InputQuery::Scalar), &work, || {
                                panic!("checked input ran twice")
                            })
                        })
                        .unwrap();
                        assert_eq!(result, Datum::Text("saved input"));
                        assert_eq!(
                            invocations
                                .resolve_intrinsic(42, &[], &moved, &work)
                                .unwrap(),
                            Datum::Int4(42)
                        );
                        assert_eq!(
                            evaluation(77, crate::sql::eval::ExpressionEvaluation::Immutable)
                                .unwrap(),
                            crate::sql::eval::ExpressionEvaluation::Runtime
                        );
                        let cast = cast_selection(
                            23,
                            25,
                            crate::sql::eval::DefinedCastContext::Explicit,
                            || panic!("cast was selected again"),
                        )
                        .unwrap()
                        .unwrap();
                        assert_eq!(
                            cast.definition.method,
                            crate::storage::CastMethod::Function(99)
                        );
                        assert_eq!(moved.used(), retained);
                    }
                },
            );
        });
    }

    #[test]
    fn invocation_registry_keeps_inputs_results_and_selections_after_owner_move() {
        let mut budget = Budget::new(64 << 10);
        let original = Arena::new(&mut budget, "movable invocation owner", 32 << 10).unwrap();
        let mut work = Arena::new(&mut budget, "movable invocation scratch", 16 << 10).unwrap();
        let wrong = Arena::new(&mut budget, "foreign invocation owner", 16 << 10).unwrap();
        let invocations = RoutineInvocationState::new(&original);
        crate::mem::guard::forbid_alloc(|| {
            with_routine_invocation_scope(
                Some(RoutineInvocationContext::new(&invocations, &original)),
                || {
                    let result = call(&work, || {
                        let _input = enter();
                        let text = work.alloc_str("saved input").unwrap();
                        value(InputSite::Query(InputQuery::Scalar), &work, || {
                            Ok(Datum::Text(text))
                        })?;
                        Err::<(), _>(sql_err!(
                            sqlstate::INTERNAL_ROUTINE_INVOCATION,
                            "input handoff"
                        ))
                    });
                    assert_eq!(
                        result.unwrap_err().sqlstate,
                        sqlstate::INTERNAL_ROUTINE_INVOCATION
                    );
                    invocations
                        .complete(
                            OwnedRoutineResult::capture_scalar(Datum::Int4(42), &original).unwrap(),
                            &original,
                            &original,
                        )
                        .unwrap();
                    evaluation(77, crate::sql::eval::ExpressionEvaluation::Runtime).unwrap();
                    cast_selection(
                        23,
                        25,
                        crate::sql::eval::DefinedCastContext::Explicit,
                        || Ok(Some(captured_cast(99))),
                    )
                    .unwrap();
                },
            )
        });
        let (mut moved, invocations) = (original, invocations);
        crate::mem::guard::forbid_alloc(|| {
            moved.recycle_front();
            let retained = moved.used();
            {
                with_routine_invocation_scope(
                    Some(RoutineInvocationContext::new(&invocations, &moved)),
                    || {
                        for _ in 0..8 {
                            work.reset();
                            invocations.begin_attempt();
                            let result = call(&work, || {
                                let _input = enter();
                                value(InputSite::Query(InputQuery::Scalar), &work, || {
                                    panic!("saved input must not run again")
                                })
                            })
                            .unwrap();
                            assert_eq!(result, Datum::Text("saved input"));
                            assert_eq!(
                                invocations
                                    .resolve_intrinsic(42, &[], &moved, &work)
                                    .unwrap(),
                                Datum::Int4(42)
                            );
                            assert!(
                                evaluation(77, crate::sql::eval::ExpressionEvaluation::Immutable)
                                    .unwrap()
                                    == crate::sql::eval::ExpressionEvaluation::Runtime
                            );
                            assert_eq!(
                                cast_selection(
                                    23,
                                    25,
                                    crate::sql::eval::DefinedCastContext::Explicit,
                                    || panic!("selected cast must not change")
                                )
                                .unwrap()
                                .unwrap()
                                .definition
                                .method,
                                crate::storage::CastMethod::Function(99)
                            );
                            assert_eq!(moved.used(), retained);
                        }
                        assert_eq!(
                            invocations
                                .resolve_intrinsic(42, &[], &wrong, &work)
                                .unwrap_err()
                                .sqlstate,
                            "XX000"
                        );
                    },
                )
            }
            let retained = OwnedRoutineResult::capture_scalar(Datum::Null, &moved).unwrap();
            moved.reset();
            assert_eq!(
                invocations
                    .complete(retained, &moved, &moved,)
                    .unwrap_err()
                    .sqlstate,
                "XX000"
            );
        });
    }

    #[test]
    fn saved_input_images_keep_values_names_defaults_and_types_after_arena_move() {
        let mut budget = Budget::new(64 << 10);
        let original = Arena::new(&mut budget, "saved input images", 32 << 10).unwrap();
        let mut output = Arena::new(&mut budget, "saved input views", 16 << 10).unwrap();
        let wrong = Arena::new(&mut budget, "other input images", 16 << 10).unwrap();
        let (value, definition, cast) = crate::mem::guard::forbid_alloc(|| {
            original
                .with_remaining_region(|scratch| {
                    let text = scratch.alloc_str("saved input").unwrap();
                    let field = scratch.alloc_str("payload").unwrap();
                    let fields = scratch
                        .alloc_slice_copy(&[crate::sql::types::RecordField {
                            name: field,
                            type_oid: crate::sql::types::oid::TEXT,
                            value: Datum::Text(text),
                        }])
                        .unwrap();
                    let datum = scratch.alloc(Datum::Record(fields)).unwrap();
                    let value = saved::OwnedInputFrame::capture(
                        InputFrame {
                            site: InputSite::Call {
                                name: scratch.alloc_str("selected_name").unwrap(),
                                kind: crate::sql::ast::CallKind::Routine,
                                bound_oid: Some(42),
                                arity: 1,
                                variadic: false,
                            },
                            state: InputState::Value {
                                value: InputValue::Evaluated(datum),
                                next_input: 8,
                                next_invocation: 9,
                                next_call: 10,
                                replay: Some(
                                    scratch
                                        .alloc(crate::sql::sequence::StatementReplayCursor::EMPTY)
                                        .unwrap(),
                                ),
                            },
                        },
                        &original,
                    )
                    .unwrap();
                    let arguments = scratch
                        .alloc_slice_copy(&[crate::storage::RoutineArgumentDef {
                            name: crate::storage::SqlName::parse("argument").unwrap(),
                            ctype: ColType::Int4,
                            user_type: None,
                        }])
                        .unwrap();
                    let defaults = scratch
                        .alloc_slice_copy(&[ParameterDefault {
                            mode: crate::storage::RoutineParameterMode::In { default: None }.code(),
                            sql: Some(scratch.alloc_str("SELECT 7").unwrap()),
                        }])
                        .unwrap();
                    let definition = scratch
                        .alloc(InputDefinition {
                            signature: RoutineCallContract {
                                oid: 42,
                                arguments,
                                parameters: arguments,
                                result_columns: &[],
                                result: RoutineCallResult::Scalar(
                                    crate::storage::RoutineResult::builtin(ColType::Int4),
                                ),
                            },
                            defaults,
                            strict: true,
                            dependencies: &[],
                        })
                        .unwrap();
                    let definition = saved::OwnedInputFrame::capture(
                        InputFrame {
                            site: InputSite::Definition(42),
                            state: InputState::Definition(definition),
                        },
                        &original,
                    )
                    .unwrap();
                    let cast = scratch.alloc(captured_cast(111)).unwrap();
                    let cast = saved::OwnedInputFrame::capture(
                        InputFrame {
                            site: InputSite::CastDefinition {
                                source: 23,
                                target: 25,
                                context: crate::sql::eval::DefinedCastContext::Explicit,
                            },
                            state: InputState::CastDefinition(Some(cast)),
                        },
                        &original,
                    )
                    .unwrap();
                    (value, definition, cast)
                })
                .unwrap()
        });
        let mut moved = original;
        crate::mem::guard::forbid_alloc(|| {
            moved.recycle_front();
            for _ in 0..4 {
                output.reset();
                let frame = value.load(&moved, &output).unwrap();
                assert!(matches!(
                    frame.site,
                    InputSite::Call {
                        name: "selected_name",
                        bound_oid: Some(42),
                        ..
                    }
                ));
                let InputState::Value {
                    value: InputValue::Retained(handle),
                    next_input: 8,
                    next_invocation: 9,
                    next_call: 10,
                    replay,
                } = frame.state
                else {
                    panic!("saved input state")
                };
                assert!(replay.unwrap().is_empty());
                let bytes = moved.retained_slice_mut(handle).unwrap();
                let bytes = output.alloc_slice_copy(&bytes).unwrap();
                let Datum::Record(fields) =
                    crate::sql::exec::decode_projected_col_record(bytes, 0, &output).unwrap()
                else {
                    panic!("saved record input")
                };
                assert_eq!(fields[0].name, "payload");
                assert_eq!(fields[0].value, Datum::Text("saved input"));
                let InputState::Definition(selected) =
                    definition.load(&moved, &output).unwrap().state
                else {
                    panic!("saved definition")
                };
                assert!(selected.strict);
                assert_eq!(selected.signature.arguments[0].ctype, ColType::Int4);
                assert_eq!(selected.defaults[0].sql, Some("SELECT 7"));
                assert_eq!(definition.definition_oid(&moved).unwrap(), Some(42));
                let InputState::CastDefinition(Some(selected)) =
                    cast.load(&moved, &output).unwrap().state
                else {
                    panic!("saved cast")
                };
                assert_eq!(
                    selected.definition.method,
                    crate::storage::CastMethod::Function(111)
                );
            }
            assert_eq!(
                definition.definition_oid(&wrong).unwrap_err().sqlstate,
                "40001"
            );
            moved.reset();
            assert_eq!(
                definition.definition_oid(&moved).unwrap_err().sqlstate,
                "40001"
            );
            assert!(value.load(&moved, &output).is_err());
        });
    }

    #[test]
    fn input_suspension_uses_retained_storage_while_the_parent_front_is_lent() {
        let mut budget = Budget::new(16 << 10);
        let statement = Arena::new(&mut budget, "parent input owner", 16 << 10).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        with_routine_invocation_scope(
            Some(RoutineInvocationContext::new(&invocations, &statement)),
            || {
                crate::mem::guard::forbid_alloc(|| {
                    let error = statement
                        .with_remaining_region(|work| {
                            call(work, || {
                                let _input = enter();
                                value(InputSite::Query(InputQuery::Scalar), work, || {
                                    Ok(Datum::Int4(7))
                                })?;
                                Err::<(), _>(sql_err!(
                                    sqlstate::INTERNAL_ROUTINE_INVOCATION,
                                    "input handoff"
                                ))
                            })
                            .unwrap_err()
                        })
                        .unwrap();
                    assert_eq!(error.sqlstate, sqlstate::INTERNAL_ROUTINE_INVOCATION);
                    let retained = statement.used();
                    for _ in 0..8 {
                        invocations.begin_attempt();
                        statement
                            .with_remaining_region(|work| {
                                let result = call(work, || {
                                    let _input = enter();
                                    value(InputSite::Query(InputQuery::Scalar), work, || {
                                        panic!("completed input must not execute again")
                                    })
                                })
                                .unwrap();
                                assert_eq!(result, Datum::Int4(7));
                            })
                            .unwrap();
                        assert_eq!(statement.used(), retained);
                    }
                });
            },
        )
    }

    #[test]
    fn saved_input_capacity_and_foreign_selection_errors_preserve_scope_authority() {
        let mut budget = Budget::new(8193);
        let source = Arena::new(&mut budget, "selected input", 4096).unwrap();
        let other = Arena::new(&mut budget, "other input", 4096).unwrap();
        let tiny = Arena::new(&mut budget, "tiny input", 1).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let datum = Datum::Text("cannot fit");
            assert_eq!(
                saved::OwnedInputFrame::capture(
                    InputFrame {
                        site: InputSite::Expression(InputNode::Value),
                        state: InputState::Value {
                            value: InputValue::Evaluated(&datum),
                            next_input: 1,
                            next_invocation: 0,
                            next_call: 0,
                            replay: None
                        },
                    },
                    &tiny
                )
                .err()
                .unwrap()
                .sqlstate,
                "54000"
            );
            let frame = source
                .retain(
                    saved::OwnedInputFrame::capture(
                        InputFrame {
                            site: InputSite::Expression(InputNode::Value),
                            state: InputState::Pending,
                        },
                        &source,
                    )
                    .unwrap(),
                )
                .unwrap();
            let invocations = RoutineInvocationState::new(&other);
            let mut frames = ArenaList::new(&other);
            frames.push(AttemptFrame::Saved(frame)).unwrap();
            let attempt = InputAttempt {
                registry: &invocations.inputs,
                owner: &other,
                invocation: &invocations.next,
                arena: &other,
                next: Cell::new(0),
                frames: RefCell::new(frames),
            };
            let before = AVAILABLE_INPUT.with(Cell::get);
            {
                let _available = AvailableScope(AVAILABLE_INPUT.with(|slot| {
                    slot.replace(Some(&attempt as *const InputAttempt<'_, '_> as *const ()))
                }));
                assert_eq!(selected_target().unwrap_err().sqlstate, "40001");
            }
            assert_eq!(AVAILABLE_INPUT.with(Cell::get), before);
        });
    }

    #[test]
    fn retained_input_values_advance_typed_journals_past_arguments_and_completed_bodies() {
        use crate::sql::sequence;
        let mut budget = Budget::new(64 << 10);
        let statement = Arena::new(&mut budget, "input effect replay", 32 << 10).unwrap();
        let mut work = Arena::new(&mut budget, "input effect scratch", 16 << 10).unwrap();
        let guc = crate::sql::guc::GucState::new();
        let invocations = RoutineInvocationState::new(&statement);
        let actions = Cell::new(0);
        crate::mem::guard::forbid_alloc(|| {
            let scope = sequence::enter_statement(&statement, guc.seq_session(), false, None);
            let start = sequence::begin_replay_cursor().unwrap();
            with_routine_invocation_scope(
                Some(RoutineInvocationContext::new(&invocations, &statement)),
                || {
                    let effect = |tag| {
                        let values = sequence::statement_effect(
                            sequence::StatementEffectKind::StatisticsVoid,
                            &[Datum::Int4(tag)],
                            sequence::StatementEffectRetry::Restart,
                            &statement,
                            || {
                                actions.set(actions.get() + 1);
                                sequence::StatementEffectAttempt {
                                    result: Ok(statement
                                        .alloc_slice_copy(&[Datum::Int4(tag)])
                                        .unwrap()),
                                    rewound_state: false,
                                }
                            },
                        )?;
                        Ok::<_, SqlError>(values[0])
                    };
                    for attempt in 0..2 {
                        sequence::rewind_replay_cursor(start).unwrap();
                        work.reset();
                        invocations.begin_attempt();
                        let result = call(&work, || {
                            let _inputs = enter();
                            let argument =
                                value(InputSite::Query(InputQuery::Scalar), &work, || effect(11))?;
                            crate::sql::type_input_plans::with_owner(
                                crate::sql::type_input_plans::Owner::Backend,
                                || {
                                    invocations.resolve_intrinsic(
                                        42,
                                        &[argument],
                                        &statement,
                                        &work,
                                    )
                                },
                            )
                        });
                        if attempt == 0 {
                            assert_eq!(
                                result.unwrap_err().sqlstate,
                                sqlstate::INTERNAL_ROUTINE_INVOCATION
                            );
                            assert!(invocations.take_pending().is_some());
                            let body = effect(22).unwrap();
                            invocations
                                .complete(
                                    OwnedRoutineResult::capture_scalar(body, &statement).unwrap(),
                                    &statement,
                                    &statement,
                                )
                                .unwrap();
                        } else {
                            assert_eq!(result.unwrap(), Datum::Int4(22));
                            assert_eq!(effect(33).unwrap(), Datum::Int4(33));
                        }
                    }
                    assert_eq!(actions.get(), 3);
                    scope.finish(guc.seq_session()).unwrap();
                },
            )
        });
    }

    fn captured_cast(function: i32) -> crate::sql::eval::BoundCast {
        crate::sql::eval::BoundCast {
            source_oid: 23,
            target_oid: 25,
            leaf_source_oid: 23,
            leaf_target_oid: 25,
            context: crate::sql::eval::DefinedCastContext::Explicit,
            strict: false,
            volatility: crate::storage::RoutineVolatility::Volatile,
            leakproof: false,
            definition: crate::storage::CastDef {
                created_at: 1,
                source: crate::storage::RoutineResult::builtin(crate::sql::types::ColType::Int4),
                target: crate::storage::RoutineResult::builtin(crate::sql::types::ColType::Text),
                method: crate::storage::CastMethod::Function(function),
                ddl_state: crate::storage::CatalogDdlState::Present,
                ..crate::storage::CastDef::EMPTY
            },
        }
    }

    #[test]
    fn assignment_sites_are_static_and_do_not_leak_into_nested_body_sources() {
        use crate::sql::type_inputs::{AssignmentNode, AssignmentSite};
        let mut budget = Budget::new(32 << 10);
        let arena = Arena::new(&mut budget, "assignment source sites", 32 << 10).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut parser =
                crate::sql::parser::Parser::new("INSERT INTO target VALUES(1,2),(3,4)", &arena)
                    .unwrap();
            let statement = parser.next_stmt().unwrap().unwrap();
            let mut sites = ArenaList::new(&arena);
            super::super::for_each_dml_assignment(&statement, &mut |_, _, _, site| {
                sites.push(site.unwrap()).map_err(|_| full())
            })
            .unwrap();
            assert_eq!(
                sites.borrowed_slice()[0],
                AssignmentSite {
                    statement_position: 0,
                    node: AssignmentNode::Values { row: 0, value: 0 }
                }
            );
            assert_eq!(
                sites.borrowed_slice()[3],
                AssignmentSite {
                    statement_position: 0,
                    node: AssignmentNode::Values { row: 1, value: 1 }
                }
            );
            let site = sites.borrowed_slice()[3];
            assert_eq!(invocation_site(), None);
            {
                let _assignment = enter_assignment(Some(site));
                assert_eq!(
                    invocation_site(),
                    Some(super::super::InvocationSite::Assignment(site))
                );
                {
                    let _body = enter_body();
                    assert_eq!(invocation_site(), None);
                    let expression =
                        crate::sql::parser::parse_expr("inner_call(1)", &arena).unwrap();
                    let _expression = enter_expression(expression).unwrap();
                    assert_eq!(
                        invocation_site(),
                        Some(super::super::InvocationSite::Expression(0))
                    );
                }
                assert_eq!(
                    invocation_site(),
                    Some(super::super::InvocationSite::Assignment(site))
                );
                {
                    let first = super::super::InvocationSite::ArrayElement {
                        position: 7,
                        element: 0,
                    };
                    let _element = enter_array_element(first);
                    assert_eq!(invocation_site(), Some(first));
                    {
                        let _second =
                            enter_array_element(super::super::InvocationSite::ArrayElement {
                                position: 7,
                                element: 1,
                            });
                        assert_ne!(invocation_site(), Some(first));
                        let expression =
                            crate::sql::parser::parse_expr("leaf_call(1)", &arena).unwrap();
                        let _expression = enter_expression(expression).unwrap();
                        assert_eq!(
                            invocation_site(),
                            Some(super::super::InvocationSite::Expression(0))
                        );
                    }
                    assert_eq!(invocation_site(), Some(first));
                    {
                        let _body = enter_body();
                        assert_eq!(invocation_site(), None);
                    }
                    assert_eq!(invocation_site(), Some(first));
                }
                assert_eq!(
                    invocation_site(),
                    Some(super::super::InvocationSite::Assignment(site))
                );
            }
            assert_eq!(invocation_site(), None);
        });
    }

    #[test]
    fn retained_input_ownership_keeps_cast_selections_and_builtin_absence_after_work_reset() {
        use crate::sql::eval::DefinedCastContext::Explicit;
        let mut budget = Budget::new(32768);
        let statement = Arena::new(&mut budget, "cast input statement", 16384).unwrap();
        let mut work = Arena::new(&mut budget, "cast input work", 8192).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        with_routine_invocation_scope(
            Some(RoutineInvocationContext::new(&invocations, &statement)),
            || {
                let selections = Cell::new(0);
                for attempt in 0..2 {
                    work.reset();
                    invocations.begin_attempt();
                    crate::mem::guard::forbid_alloc(|| {
                        let result = call(&work, || {
                            let _input = enter();
                            let selected = cast_definition(23, 25, Explicit, || {
                                selections.set(selections.get() + 1);
                                Ok(Some(captured_cast(if attempt == 0 { 111 } else { 222 })))
                            })?
                            .unwrap();
                            assert_eq!(
                                selected.definition.method,
                                crate::storage::CastMethod::Function(111)
                            );
                            assert_eq!(selected.definition.created_at, 1);
                            let absent = cast_definition(25, 23, Explicit, || {
                                selections.set(selections.get() + 1);
                                Ok(if attempt == 0 {
                                    None
                                } else {
                                    Some(captured_cast(333))
                                })
                            })?;
                            assert!(absent.is_none());
                            if attempt == 0 {
                                Err(sql_err!(
                                    sqlstate::INTERNAL_ROUTINE_INVOCATION,
                                    "test cast suspension"
                                ))
                            } else {
                                Ok(Datum::Int4(7))
                            }
                        });
                        if attempt == 0 {
                            assert_eq!(
                                result.unwrap_err().sqlstate,
                                sqlstate::INTERNAL_ROUTINE_INVOCATION
                            );
                        } else {
                            assert_eq!(result.unwrap(), Datum::Int4(7));
                        }
                        assert_eq!(selections.get(), 2);
                    });
                }
                assert!(ACTIVE_INPUT.with(Cell::get).is_none());
                assert!(AVAILABLE_INPUT.with(Cell::get).is_none());
            },
        )
    }

    #[test]
    fn retained_input_ownership_rejects_changed_cast_definition_keys() {
        use crate::sql::eval::DefinedCastContext::{Assignment, Explicit};
        for changed_key in [(24, 25, Explicit), (23, 26, Explicit), (23, 25, Assignment)] {
            let mut budget = Budget::new(32768);
            let statement = Arena::new(&mut budget, "cast key statement", 16384).unwrap();
            let mut work = Arena::new(&mut budget, "cast key work", 8192).unwrap();
            let invocations = RoutineInvocationState::new(&statement);
            with_routine_invocation_scope(
                Some(RoutineInvocationContext::new(&invocations, &statement)),
                || {
                    for attempt in 0..2 {
                        work.reset();
                        invocations.begin_attempt();
                        crate::mem::guard::forbid_alloc(|| {
                            let key = if attempt == 0 {
                                (23, 25, Explicit)
                            } else {
                                changed_key
                            };
                            let result: Result<(), SqlError> = call(&work, || {
                                let _input = enter();
                                cast_definition(key.0, key.1, key.2, || {
                                    assert_eq!(
                                        attempt, 0,
                                        "changed keys must fail before selecting"
                                    );
                                    Ok(Some(captured_cast(111)))
                                })?;
                                Err(sql_err!(
                                    sqlstate::INTERNAL_ROUTINE_INVOCATION,
                                    "test cast suspension"
                                ))
                            });
                            assert_eq!(
                                result.unwrap_err().sqlstate,
                                if attempt == 0 {
                                    crate::sql::eval::SqlState::known(
                                        sqlstate::INTERNAL_ROUTINE_INVOCATION,
                                    )
                                } else {
                                    crate::sql::eval::SqlState::known(
                                        sqlstate::SERIALIZATION_FAILURE,
                                    )
                                }
                            );
                        });
                    }
                },
            )
        }
    }

    #[test]
    fn retained_input_ownership_keeps_nested_values_and_ordinals_after_work_reset() {
        let mut budget = Budget::new(32768);
        let statement = Arena::new(&mut budget, "input statement", 16384).unwrap();
        let mut work = Arena::new(&mut budget, "input work", 8192).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        with_routine_invocation_scope(
            Some(RoutineInvocationContext::new(&invocations, &statement)),
            || {
                let effects = Cell::new(0);
                for attempt in 0..2 {
                    work.reset();
                    invocations.begin_attempt();
                    crate::mem::guard::forbid_alloc(|| {
                        let result = call(&work, || {
                            let _scope = enter();
                            value(InputSite::Query(InputQuery::Scalar), &work, || {
                                let first = value(
                                    InputSite::Call {
                                        name: "input_effect",
                                        kind: crate::sql::ast::CallKind::Routine,
                                        bound_oid: None,
                                        arity: 0,
                                        variadic: false,
                                    },
                                    &work,
                                    || {
                                        effects.set(effects.get() + 1);
                                        invocations.next.set(1);
                                        Ok(Datum::Text(work.alloc_str("retained value").unwrap()))
                                    },
                                )?;
                                assert_eq!(invocations.next.get(), 1);
                                assert!(matches!(first, Datum::Text("retained value")));
                                let second =
                                    value(InputSite::Query(InputQuery::Exists), &work, || {
                                        invocations.next.set(2);
                                        if attempt == 0 {
                                            Err(sql_err!(
                                                sqlstate::INTERNAL_ROUTINE_INVOCATION,
                                                "test suspension"
                                            ))
                                        } else {
                                            Ok(Datum::Int4(7))
                                        }
                                    })?;
                                assert!(matches!(second, Datum::Int4(7)));
                                Ok(first)
                            })
                        });
                        if attempt == 0 {
                            assert_eq!(
                                result.unwrap_err().sqlstate,
                                sqlstate::INTERNAL_ROUTINE_INVOCATION
                            );
                        } else {
                            assert!(matches!(result.unwrap(), Datum::Text("retained value")));
                        }
                        assert_eq!(effects.get(), 1);
                        assert_eq!(invocations.next.get(), 2);
                    });
                }
                assert!(ACTIVE_INPUT.with(Cell::get).is_none());
                assert!(AVAILABLE_INPUT.with(Cell::get).is_none());
            },
        )
    }

    #[test]
    fn retained_inputs_reject_a_syntax_node_replacing_an_ordinary_routine() {
        let mut budget = Budget::new(32768);
        let statement = Arena::new(&mut budget, "tagged input statement", 16384).unwrap();
        let mut work = Arena::new(&mut budget, "tagged input work", 8192).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        with_routine_invocation_scope(
            Some(RoutineInvocationContext::new(&invocations, &statement)),
            || {
                let effects = Cell::new(0);
                invocations.begin_attempt();
                crate::mem::guard::forbid_alloc(|| {
                    let suspended: Result<Datum<'_>, SqlError> = call(&work, || {
                        let _scope = enter();
                        value(
                            InputSite::Call {
                                name: "__json_object_json",
                                kind: crate::sql::ast::CallKind::Routine,
                                bound_oid: None,
                                arity: 0,
                                variadic: false,
                            },
                            &work,
                            || {
                                effects.set(effects.get() + 1);
                                invocations.next.set(1);
                                Ok(Datum::Int4(7))
                            },
                        )?;
                        Err(sql_err!(
                            sqlstate::INTERNAL_ROUTINE_INVOCATION,
                            "test suspension"
                        ))
                    });
                    assert_eq!(
                        suspended.unwrap_err().sqlstate,
                        sqlstate::INTERNAL_ROUTINE_INVOCATION
                    );
                });
                work.reset();
                invocations.begin_attempt();
                crate::mem::guard::forbid_alloc(|| {
                    let resumed = call(&work, || {
                        let _scope = enter();
                        value(
                            InputSite::Call {
                                name: "__json_object_json",
                                kind: crate::sql::ast::CallKind::SqlSyntax,
                                bound_oid: None,
                                arity: 0,
                                variadic: false,
                            },
                            &work,
                            || {
                                effects.set(effects.get() + 1);
                                Ok(Datum::Int4(9))
                            },
                        )
                    });
                    assert_eq!(
                        resumed.unwrap_err().sqlstate,
                        sqlstate::SERIALIZATION_FAILURE
                    );
                    assert_eq!(effects.get(), 1);
                });
            },
        )
    }

    #[test]
    fn nested_query_scopes_keep_parent_frames_outside_recycled_scratch() {
        let mut budget = Budget::new(8192);
        let statement = Arena::new(&mut budget, "nested input statement", 2048).unwrap();
        let work = Arena::new(&mut budget, "nested input scratch", 4096).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        with_routine_invocation_scope(
            Some(RoutineInvocationContext::new(&invocations, &statement)),
            || {
                crate::mem::guard::forbid_alloc(|| {
                    let output = call(&work, || {
                        let _inputs = enter();
                        let parent = ACTIVE_INPUT.with(Cell::get).unwrap();
                        let value = value(InputSite::Query(InputQuery::Scalar), &work, || {
                            let _body = enter_body();
                            let mark = work.mark();
                            for row in 0..64 {
                                let nested = call(&work, || {
                                    let _inputs = enter();
                                    value(InputSite::Expression(InputNode::Value), &work, || {
                                        Ok(Datum::Int4(row))
                                    })
                                })?;
                                assert!(matches!(nested, Datum::Int4(value) if value == row));
                                assert!(ACTIVE_INPUT.with(Cell::get).is_none());
                                // Only the copied integer escapes this nested row.
                                unsafe { work.rewind_to(mark) };
                            }
                            Ok(Datum::Int4(64))
                        })?;
                        assert_eq!(ACTIVE_INPUT.with(Cell::get), Some(parent));
                        let attempt = unsafe { &*(parent as *const InputAttempt<'_, '_>) };
                        assert_eq!(attempt.frames.borrow().len(), 1);
                        assert_eq!(attempt.next.get(), 1);
                        Ok(value)
                    })
                    .unwrap();
                    assert!(matches!(output, Datum::Int4(64)));
                    assert!(ACTIVE_INPUT.with(Cell::get).is_none());
                    assert!(AVAILABLE_INPUT.with(Cell::get).is_none());
                    assert_eq!(invocations.inputs.saved.borrow().len(), 0);
                    assert_eq!(statement.used(), 0);
                });
            },
        )
    }

    #[test]
    fn retained_input_ownership_successful_inputs_do_not_grow_statement_memory_per_row() {
        let mut budget = Budget::new(8192);
        let statement = Arena::new(&mut budget, "input statement", 1024).unwrap();
        let mut work = Arena::new(&mut budget, "input work", 4096).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        with_routine_invocation_scope(
            Some(RoutineInvocationContext::new(&invocations, &statement)),
            || {
                invocations.begin_attempt();
                let before = statement.used();
                for row in 0..100 {
                    work.reset();
                    crate::mem::guard::forbid_alloc(|| {
                        let result = call(&work, || {
                            let _scope = enter();
                            value(InputSite::Query(InputQuery::Scalar), &work, || {
                                Ok(Datum::Int4(row))
                            })
                        })
                        .unwrap();
                        assert!(matches!(result, Datum::Int4(value) if value == row));
                        assert_eq!(statement.used(), before);
                        assert_eq!(invocations.inputs.saved.borrow().len(), 0);
                    });
                }
            },
        )
    }

    #[test]
    fn retained_input_ownership_tracks_all_arguments_in_bounded_scratch() {
        let mut budget = Budget::new(40960);
        let statement = Arena::new(&mut budget, "input statement", 1024).unwrap();
        let work = Arena::new(&mut budget, "input work", 32768).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        with_routine_invocation_scope(
            Some(RoutineInvocationContext::new(&invocations, &statement)),
            || {
                crate::mem::guard::forbid_alloc(|| {
                    call(&work, || {
                        let _scope = enter();
                        for parameter in 0..crate::storage::MAX_ROUTINE_ARGUMENTS {
                            let result = value(
                                InputSite::Argument {
                                    routine: 42,
                                    parameter,
                                    written: Some(parameter),
                                    source: 23,
                                    target: 23,
                                },
                                &work,
                                || Ok(Datum::Int4(parameter as i32)),
                            )?;
                            assert!(
                                matches!(result, Datum::Int4(value) if value == parameter as i32)
                            );
                        }
                        Ok(())
                    })
                    .unwrap();
                    assert_eq!(statement.used(), 0);
                });
            },
        )
    }

    #[test]
    fn retained_input_ownership_borrows_wide_values_until_suspension() {
        let mut budget = Budget::new(4096);
        let statement = Arena::new(&mut budget, "input statement", 1024).unwrap();
        let work = Arena::new(&mut budget, "input work", 2048).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        with_routine_invocation_scope(
            Some(RoutineInvocationContext::new(&invocations, &statement)),
            || {
                crate::mem::guard::forbid_alloc(|| {
                    let raw = work.alloc_slice_copy(&[b'x'; 1024]).unwrap();
                    let text = core::str::from_utf8(raw).unwrap();
                    let result = call(&work, || {
                        let _scope = enter();
                        value(InputSite::Query(InputQuery::Scalar), &work, || {
                            Ok(Datum::Text(text))
                        })
                    })
                    .unwrap();
                    assert!(
                        matches!(result, Datum::Text(value) if core::ptr::eq(value.as_ptr(), text.as_ptr()))
                    );
                    assert_eq!(statement.used(), 0);
                });
            },
        )
    }

    #[test]
    fn retained_input_ownership_keeps_outer_planning_separate_from_new_bodies() {
        use super::super::super::eval::ExpressionEvaluation::{Immutable, Runtime};
        let mut budget = Budget::new(4096);
        let statement = Arena::new(&mut budget, "input planning", 4096).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        with_routine_invocation_scope(
            Some(RoutineInvocationContext::new(&invocations, &statement)),
            || {
                crate::mem::guard::forbid_alloc(|| {
                    assert!(evaluation(42, Runtime).unwrap() == Runtime);
                    let context = crate::sql::eval::DefinedCastContext::Explicit;
                    assert_eq!(
                        cast_selection(23, 25, context, || Ok(Some(captured_cast(42))))
                            .unwrap()
                            .unwrap()
                            .definition
                            .method,
                        crate::storage::CastMethod::Function(42)
                    );
                    assert!(
                        cast_selection(25, 23, context, || Ok(None))
                            .unwrap()
                            .is_none()
                    );
                    assert_eq!(invocations.inputs.next.get(), 0);
                    let before = statement.used();
                    invocations.begin_attempt();
                    assert!(evaluation(42, Immutable).unwrap() == Runtime);
                    assert_eq!(
                        cast_selection(23, 25, context, || panic!(
                            "outer planning must keep its cast"
                        ))
                        .unwrap()
                        .unwrap()
                        .definition
                        .method,
                        crate::storage::CastMethod::Function(42)
                    );
                    assert!(
                        cast_selection(25, 23, context, || panic!(
                            "outer planning must keep builtin absence"
                        ))
                        .unwrap()
                        .is_none()
                    );
                    {
                        let _body = enter_body();
                        assert!(evaluation(42, Immutable).unwrap() == Immutable);
                        assert_eq!(
                            cast_selection(23, 25, context, || Ok(Some(captured_cast(99))))
                                .unwrap()
                                .unwrap()
                                .definition
                                .method,
                            crate::storage::CastMethod::Function(99)
                        );
                    }
                    assert_eq!(
                        cast_selection(23, 25, context, || panic!(
                            "body must not replace outer planning"
                        ))
                        .unwrap()
                        .unwrap()
                        .definition
                        .method,
                        crate::storage::CastMethod::Function(42)
                    );
                    assert!(evaluation(42, Immutable).unwrap() == Runtime);
                    assert_eq!(statement.used(), before);
                    assert_eq!(invocations.inputs.next.get(), 0);
                });
            },
        )
    }

    #[test]
    fn retained_input_ownership_capacity_failure_restores_scopes() {
        let mut budget = Budget::new(8192);
        let statement = Arena::new(&mut budget, "input statement", 1).unwrap();
        let work = Arena::new(&mut budget, "input work", 4096).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        with_routine_invocation_scope(
            Some(RoutineInvocationContext::new(&invocations, &statement)),
            || {
                let error = crate::mem::guard::forbid_alloc(|| {
                    call(&work, || {
                        let _scope = enter();
                        cast_definition(
                            23,
                            25,
                            crate::sql::eval::DefinedCastContext::Explicit,
                            || Ok(None),
                        )?;
                        value(InputSite::Query(InputQuery::Scalar), &work, || {
                            Err(sql_err!(
                                sqlstate::INTERNAL_ROUTINE_INVOCATION,
                                "test suspension"
                            ))
                        })
                    })
                    .unwrap_err()
                });
                assert_eq!(error.sqlstate, "54000");
                assert_eq!(
                    error.message.as_str(),
                    "retained routine inputs exceed statement memory"
                );
                assert!(ACTIVE_INPUT.with(Cell::get).is_none());
                assert!(AVAILABLE_INPUT.with(Cell::get).is_none());
            },
        )
    }

    #[test]
    fn pending_calls_keep_the_lexical_plan_owner_after_the_source_scope_ends() {
        use crate::sql::type_input_plans::{self, Owner};
        let mut budget = Budget::new(16384);
        let mut statement = Arena::new(&mut budget, "pending input owner", 8192).unwrap();
        let mut work = Arena::new(&mut budget, "pending source work", 8192).unwrap();
        for owner in [
            Owner::ProtocolPrepared(2),
            Owner::SqlPrepared(3),
            Owner::Dynamic {
                epoch: 4,
                generation: 5,
            },
        ] {
            statement.reset();
            work.reset();
            let invocations = RoutineInvocationState::new(&statement);
            let pending = crate::mem::guard::forbid_alloc(|| {
                type_input_plans::with_owner(owner, || {
                    let expression =
                        crate::sql::parser::parse_expr("pending_call()", &work).unwrap();
                    let _source = enter_expression(expression).unwrap();
                    let value = work.alloc_str("retained argument").unwrap();
                    let error = invocations
                        .resolve_intrinsic(42, &[Datum::Text(value)], &statement, &work)
                        .unwrap_err();
                    assert_eq!(error.sqlstate, sqlstate::INTERNAL_ROUTINE_INVOCATION);
                    invocations.take_pending().unwrap()
                })
            });
            work.reset();
            crate::mem::guard::forbid_alloc(|| {
                assert_eq!(pending.plan_owner, owner);
                assert_eq!(
                    pending.source_position,
                    Some(super::super::InvocationSite::Expression(0))
                );
                let value = crate::sql::exec::decode_projected_col_record(
                    pending.arguments(&statement).unwrap(),
                    0,
                    &work,
                )
                .unwrap();
                assert!(matches!(value, Datum::Text("retained argument")));
                let outer = type_input_plans::owner().ok();
                type_input_plans::with_owner(pending.plan_owner, || {
                    assert_eq!(type_input_plans::owner().unwrap(), owner);
                });
                assert_eq!(type_input_plans::owner().ok(), outer);
            });
        }
    }
}
