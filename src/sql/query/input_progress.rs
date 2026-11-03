//! Input continuations are temporary row scratch until a call actually suspends.

use super::*;
use crate::storage::{
    RoutineDef, RoutineParameterMode, StoredQueryDependency, StoredQueryDependencyView,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
                kind,
                bound_oid,
                arity,
                variadic,
            } => InputSite::Call {
                name: core::str::from_utf8(copy_bytes(name.as_bytes(), arena)?)
                    .expect("copied SQL identifier is UTF-8"),
                kind,
                bound_oid,
                arity,
                variadic,
            },
            Self::Query(kind) => InputSite::Query(kind),
            Self::Expression(kind) => InputSite::Expression(kind),
            Self::Definition(oid) => InputSite::Definition(oid),
            Self::CastDefinition {
                source,
                target,
                context,
            } => InputSite::CastDefinition {
                source,
                target,
                context,
            },
            Self::Classification {
                routine,
                parameter,
                written,
            } => InputSite::Classification {
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
enum InputValue<'a> {
    Evaluated(&'a Datum<'a>),
    Encoded(&'a [u8]),
}

#[derive(Clone, Copy)]
enum InputState<'a> {
    Pending,
    Value {
        value: InputValue<'a>,
        next_input: usize,
        next_invocation: usize,
        next_call: usize,
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
enum AttemptFrame<'statement, 'query> {
    // retain is the only constructor: saved values are encoded statement bytes.
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

#[derive(Clone, Copy)]
struct CastSelection<'a> {
    source: i32,
    target: i32,
    context: super::super::eval::DefinedCastContext,
    cast: Option<&'a super::super::eval::BoundCast>,
}

impl CastSelection<'_> {
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

pub(super) struct RoutineInputs<'a> {
    arena: &'a Arena,
    next: Cell<usize>,
    saved: RefCell<ArenaList<'a, SavedInputs<'a>>>,
    evaluations: RefCell<ArenaList<'a, (i32, super::super::eval::ExpressionEvaluation)>>,
    cast_selections: RefCell<ArenaList<'a, CastSelection<'a>>>,
}

impl<'a> RoutineInputs<'a> {
    pub(super) fn new(arena: &'a Arena) -> Self {
        Self {
            arena,
            next: Cell::new(0),
            saved: RefCell::new(ArenaList::new_persistent(arena)),
            evaluations: RefCell::new(ArenaList::new_persistent(arena)),
            cast_selections: RefCell::new(ArenaList::new_persistent(arena)),
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
                .push(AttemptFrame::Temporary(InputFrame {
                    site,
                    state: InputState::Pending,
                }))
                .map_err(|_| full())?;
        }
        let frame = frames
            .borrowed_slice()
            .get(ordinal)
            .copied()
            .ok_or_else(changed)?
            .frame();
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
            } => {
                self.next.set(next_input);
                self.invocation.set(next_invocation);
                self.registry.next.set(next_call);
                return match value {
                    InputValue::Encoded(encoded) => {
                        let encoded = output.alloc_slice_copy(encoded).map_err(|_| full())?;
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
        let result = evaluate()?;
        frame.state = InputState::Value {
            value: InputValue::Evaluated(self.arena.alloc(result).map_err(|_| full())?),
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
                let definition: &'query InputDefinition<'query> = self
                    .arena
                    .alloc(InputDefinition::capture(
                        routine, slot, storage, txid, self.arena,
                    )?)
                    .map_err(|_| full())?;
                frame.state = InputState::Definition(definition);
                self.frames
                    .borrow_mut()
                    .as_mut_slice()
                    .map_err(|_| full())?[ordinal] = AttemptFrame::Temporary(frame);
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
                self.frames
                    .borrow_mut()
                    .as_mut_slice()
                    .map_err(|_| full())?[ordinal] = AttemptFrame::Temporary(frame);
                Ok(selected)
            }
            InputState::CastDefinition(selected) => Ok(selected.copied()),
            InputState::Value { .. } | InputState::Definition(_) => Err(changed()),
        }
    }

    fn retain(&self, ordinal: usize) -> Result<(), SqlError> {
        let arena = self.registry.arena;
        let mut copied = ArenaList::new(arena);
        for frame in self.frames.borrow().borrowed_slice() {
            let saved = match *frame {
                AttemptFrame::Saved(frame) => frame,
                AttemptFrame::Temporary(frame) => {
                    let state = match frame.state {
                        InputState::Pending => InputState::Pending,
                        InputState::Value {
                            value,
                            next_input,
                            next_invocation,
                            next_call,
                        } => InputState::Value {
                            value: InputValue::Encoded(match value {
                                InputValue::Evaluated(value) => {
                                    super::super::exec::encode_projected_persistent_pub(
                                        &[*value],
                                        arena,
                                    )?
                                }
                                InputValue::Encoded(encoded) => copy_bytes(encoded, arena)?,
                            }),
                            next_input,
                            next_invocation,
                            next_call,
                        },
                        InputState::Definition(definition) => {
                            let definition = definition.retain(arena)?;
                            InputState::Definition(
                                &arena
                                    .alloc_persistent_slice_with(1, |_| definition)
                                    .map_err(|_| full())?[0],
                            )
                        }
                        InputState::CastDefinition(definition) => {
                            InputState::CastDefinition(match definition {
                                Some(definition) => Some(
                                    &arena
                                        .alloc_persistent_slice_with(1, |_| *definition)
                                        .map_err(|_| full())?[0],
                                ),
                                None => None,
                            })
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
            .alloc_persistent_slice_with(copied.len(), |index| copied.borrowed_slice()[index])
            .map_err(|_| full())?;
        let saved = SavedInputs { ordinal, frames };
        let mut entries = self.registry.saved.borrow_mut();
        match entries
            .borrowed_slice()
            .binary_search_by_key(&ordinal, |entry| entry.ordinal)
        {
            Ok(index) => entries.as_mut_slice().map_err(|_| full())?[index] = saved,
            Err(index) => {
                entries.push(saved).map_err(|_| full())?;
                entries.as_mut_slice().map_err(|_| full())?[index..].rotate_right(1);
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
    let Some((invocations, _)) = active_routine_invocations() else {
        return Ok(current);
    };
    let mut entries = invocations.inputs.evaluations.borrow_mut();
    match entries
        .borrowed_slice()
        .binary_search_by_key(&oid, |entry| entry.0)
    {
        Ok(index) => Ok(entries.borrowed_slice()[index].1),
        Err(index) => {
            entries.push((oid, current)).map_err(|_| full())?;
            entries.as_mut_slice().map_err(|_| full())?[index..].rotate_right(1);
            Ok(current)
        }
    }
}

pub(crate) struct ExpressionPosition {
    previous: Option<usize>,
    pub(crate) target: Option<i32>,
    _thread: core::marker::PhantomData<*const ()>,
}
impl Drop for ExpressionPosition {
    fn drop(&mut self) {
        CURRENT_CALL.with(|current| current.set(self.previous));
    }
}

fn definition_oid(frame: InputFrame<'_>) -> Option<i32> {
    match frame.state {
        InputState::Definition(definition) => Some(definition.signature.oid),
        _ => None,
    }
}

pub(super) fn selected_target() -> Option<i32> {
    let active = AVAILABLE_INPUT.with(Cell::get)?;
    // AvailableScope keeps the stack-owned attempt live during this lookup.
    let attempt = unsafe { &*(active as *const InputAttempt<'_, '_>) };
    attempt
        .frames
        .borrow()
        .borrowed_slice()
        .get(attempt.next.get())
        .and_then(|frame| definition_oid(frame.frame()))
}

pub(crate) fn enter_expression(expression: &Expr<'_>) -> Result<ExpressionPosition, SqlError> {
    let previous = CURRENT_CALL.with(Cell::get);
    if !matches!(expression, Expr::Call { .. }) {
        return Ok(ExpressionPosition {
            previous,
            target: None,
            _thread: core::marker::PhantomData,
        });
    }
    let Some((invocations, _)) = active_routine_invocations() else {
        return Ok(ExpressionPosition {
            previous,
            target: None,
            _thread: core::marker::PhantomData,
        });
    };
    let registry = &invocations.inputs;
    let ordinal = registry.next.get();
    registry.next.set(ordinal.checked_add(1).ok_or_else(full)?);
    CURRENT_CALL.with(|current| current.set(Some(ordinal)));
    let target = if ACTIVE_INPUT.with(Cell::get).is_some() {
        selected_target()
    } else {
        let saved = registry.saved.borrow();
        saved
            .borrowed_slice()
            .binary_search_by_key(&ordinal, |entry| entry.ordinal)
            .ok()
            .and_then(|index| saved.borrowed_slice()[index].frames.first())
            .and_then(|frame| definition_oid(**frame))
    };
    Ok(ExpressionPosition {
        previous,
        target,
        _thread: core::marker::PhantomData,
    })
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
    in_body: bool,
}
impl Drop for BodyScope {
    fn drop(&mut self) {
        ACTIVE_INPUT.with(|active| active.set(self.active));
        AVAILABLE_INPUT.with(|active| active.set(self.available));
        CURRENT_CALL.with(|current| current.set(self.expression));
        IN_BODY.with(|current| current.set(self.in_body));
    }
}
pub(crate) fn enter_body() -> BodyScope {
    BodyScope {
        active: ACTIVE_INPUT.with(|active| active.replace(None)),
        available: AVAILABLE_INPUT.with(|active| active.replace(None)),
        expression: CURRENT_CALL.with(|current| current.replace(None)),
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
    let Some((invocations, _)) = active_routine_invocations() else {
        return execute();
    };
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
    {
        let saved = registry.saved.borrow();
        if let Ok(index) = saved
            .borrowed_slice()
            .binary_search_by_key(&ordinal, |entry| entry.ordinal)
        {
            for frame in saved.borrowed_slice()[index].frames {
                frames
                    .push(AttemptFrame::Saved(frame))
                    .map_err(|_| full())?;
            }
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
    let Some((invocations, arena)) = active_routine_invocations() else {
        return select();
    };
    let key = CastSelection {
        source,
        target,
        context,
        cast: None,
    }
    .key();
    {
        let entries = invocations.inputs.cast_selections.borrow();
        if let Ok(index) = entries
            .borrowed_slice()
            .binary_search_by_key(&key, |entry| entry.key())
        {
            return Ok(entries.borrowed_slice()[index].cast.copied());
        }
    }
    let selected = select()?;
    let cast = selected
        .map(|cast| {
            arena
                .alloc_persistent_slice_with(1, |_| cast)
                .map(|cast| &cast[0])
                .map_err(|_| full())
        })
        .transpose()?;
    let mut entries = invocations.inputs.cast_selections.borrow_mut();
    let index = match entries
        .borrowed_slice()
        .binary_search_by_key(&key, |entry| entry.key())
    {
        Ok(index) => return Ok(entries.borrowed_slice()[index].cast.copied()),
        Err(index) => index,
    };
    entries
        .push(CastSelection {
            source,
            target,
            context,
            cast,
        })
        .map_err(|_| full())?;
    entries.as_mut_slice().map_err(|_| full())?[index..].rotate_right(1);
    Ok(selected)
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
    fn retained_input_ownership_keeps_cast_selections_and_builtin_absence_after_work_reset() {
        use crate::sql::eval::DefinedCastContext::Explicit;
        let mut budget = Budget::new(32768);
        let statement = Arena::new(&mut budget, "cast input statement", 16384).unwrap();
        let mut work = Arena::new(&mut budget, "cast input work", 8192).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        let _scope = enter_routine_invocation_scope(Some(RoutineInvocationContext::new(
            &invocations,
            &statement,
        )));
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
    }

    #[test]
    fn retained_input_ownership_rejects_changed_cast_definition_keys() {
        use crate::sql::eval::DefinedCastContext::{Assignment, Explicit};
        for changed_key in [(24, 25, Explicit), (23, 26, Explicit), (23, 25, Assignment)] {
            let mut budget = Budget::new(32768);
            let statement = Arena::new(&mut budget, "cast key statement", 16384).unwrap();
            let mut work = Arena::new(&mut budget, "cast key work", 8192).unwrap();
            let invocations = RoutineInvocationState::new(&statement);
            let _scope = enter_routine_invocation_scope(Some(RoutineInvocationContext::new(
                &invocations,
                &statement,
            )));
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
                            assert_eq!(attempt, 0, "changed keys must fail before selecting");
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
                            sqlstate::INTERNAL_ROUTINE_INVOCATION
                        } else {
                            sqlstate::SERIALIZATION_FAILURE
                        }
                    );
                });
            }
        }
    }

    #[test]
    fn retained_input_ownership_keeps_nested_values_and_ordinals_after_work_reset() {
        let mut budget = Budget::new(32768);
        let statement = Arena::new(&mut budget, "input statement", 16384).unwrap();
        let mut work = Arena::new(&mut budget, "input work", 8192).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        let _scope = enter_routine_invocation_scope(Some(RoutineInvocationContext::new(
            &invocations,
            &statement,
        )));
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
                        let second = value(InputSite::Query(InputQuery::Exists), &work, || {
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
    }

    #[test]
    fn retained_inputs_reject_a_syntax_node_replacing_an_ordinary_routine() {
        let mut budget = Budget::new(32768);
        let statement = Arena::new(&mut budget, "tagged input statement", 16384).unwrap();
        let mut work = Arena::new(&mut budget, "tagged input work", 8192).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        let _scope = enter_routine_invocation_scope(Some(RoutineInvocationContext::new(
            &invocations,
            &statement,
        )));
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
    }

    #[test]
    fn retained_input_ownership_successful_inputs_do_not_grow_statement_memory_per_row() {
        let mut budget = Budget::new(8192);
        let statement = Arena::new(&mut budget, "input statement", 1024).unwrap();
        let mut work = Arena::new(&mut budget, "input work", 4096).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        let _scope = enter_routine_invocation_scope(Some(RoutineInvocationContext::new(
            &invocations,
            &statement,
        )));
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
    }

    #[test]
    fn retained_input_ownership_tracks_all_arguments_in_bounded_scratch() {
        let mut budget = Budget::new(40960);
        let statement = Arena::new(&mut budget, "input statement", 1024).unwrap();
        let work = Arena::new(&mut budget, "input work", 32768).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        let _scope = enter_routine_invocation_scope(Some(RoutineInvocationContext::new(
            &invocations,
            &statement,
        )));
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
                    assert!(matches!(result, Datum::Int4(value) if value == parameter as i32));
                }
                Ok(())
            })
            .unwrap();
            assert_eq!(statement.used(), 0);
        });
    }

    #[test]
    fn retained_input_ownership_borrows_wide_values_until_suspension() {
        let mut budget = Budget::new(4096);
        let statement = Arena::new(&mut budget, "input statement", 1024).unwrap();
        let work = Arena::new(&mut budget, "input work", 2048).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        let _scope = enter_routine_invocation_scope(Some(RoutineInvocationContext::new(
            &invocations,
            &statement,
        )));
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
    }

    #[test]
    fn retained_input_ownership_keeps_outer_planning_separate_from_new_bodies() {
        use super::super::super::eval::ExpressionEvaluation::{Immutable, Runtime};
        let mut budget = Budget::new(4096);
        let statement = Arena::new(&mut budget, "input planning", 4096).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        let _scope = enter_routine_invocation_scope(Some(RoutineInvocationContext::new(
            &invocations,
            &statement,
        )));
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
    }

    #[test]
    fn retained_input_ownership_capacity_failure_restores_scopes() {
        let mut budget = Budget::new(8192);
        let statement = Arena::new(&mut budget, "input statement", 1).unwrap();
        let work = Arena::new(&mut budget, "input work", 4096).unwrap();
        let invocations = RoutineInvocationState::new(&statement);
        let _scope = enter_routine_invocation_scope(Some(RoutineInvocationContext::new(
            &invocations,
            &statement,
        )));
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
    }
}
