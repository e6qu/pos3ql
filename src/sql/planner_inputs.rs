//! SQL inlining analyzes type inputs before deciding whether to inline a body.

pub(super) mod inline;

use crate::mem::arena::{Arena, ArenaList};
use crate::sql_err;
use crate::storage::{RoutineBodyKind, RoutineCallKind, RoutineCallSelection, Storage};

use super::{
    Engine, GucState, Parser, Responder, SqlPreparedPool, TxnState, TypeInputPlanSource, WireFull,
    ast::{CallKind, Expr, Stmt},
    cursor::CursorPool,
    eval::{ColumnLookup, SqlError, sqlstate},
    exec::{ColTypeResolver, infer_routine_argument_oid},
    query::{self, InvocationSite},
    type_input_plans::{self, InlineDisposition, InputImage, Key, Lookup, Owner},
    type_inputs::{self, Source},
};

#[derive(Clone, Copy)]
struct Parameters {
    values: *const (),
    known: *const bool,
    count: usize,
    source: ParameterSource,
}

#[derive(Clone, Copy)]
enum ParameterSource {
    Bound,
    Inline { depth: usize },
}

std::thread_local! {
    static PARAMETERS: core::cell::Cell<Option<Parameters>> = const { core::cell::Cell::new(None) };
}

struct ParameterScope {
    prior: Option<Parameters>,
    _thread: core::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for ParameterScope {
    fn drop(&mut self) {
        PARAMETERS.with(|slot| slot.set(self.prior));
    }
}

/// A custom plan sees its already decoded values; a generic plan sees none.
/// The private scope cannot outlive the borrowed values or move to a new thread.
pub(crate) fn with_parameters<R>(
    parameters: Option<&[super::types::Datum<'_>]>,
    run: impl FnOnce() -> R,
) -> R {
    let current = parameters.map(|values| Parameters {
        values: values.as_ptr().cast(),
        known: core::ptr::null(),
        count: values.len(),
        source: ParameterSource::Bound,
    });
    let _scope = ParameterScope {
        prior: PARAMETERS.with(|slot| slot.replace(current)),
        _thread: core::marker::PhantomData,
    };
    run()
}

fn with_inline_parameters<R>(
    values: &[super::types::Datum<'_>],
    known: &[bool],
    run: impl FnOnce() -> R,
) -> R {
    assert_eq!(values.len(), known.len());
    let current = Parameters {
        values: values.as_ptr().cast(),
        known: known.as_ptr(),
        count: values.len(),
        source: ParameterSource::Inline {
            depth: super::exec::routine_parameter_depth(),
        },
    };
    let _scope = ParameterScope {
        prior: PARAMETERS.with(|slot| slot.replace(Some(current))),
        _thread: core::marker::PhantomData,
    };
    run()
}

fn parameter_known(index: u32) -> bool {
    let Some(index) = index.checked_sub(1).map(|index| index as usize) else {
        return false;
    };
    inspect_parameters(|values| {
        if !values.is_some_and(|values| index < values.len()) {
            return false;
        }
        let current = PARAMETERS
            .with(core::cell::Cell::get)
            .expect("visible parameter scope exists");
        // SAFETY: with_inline_parameters installs one initialized mask entry
        // per value and keeps both borrowed slices live through inspection.
        current.known.is_null() || unsafe { *current.known.add(index) }
    })
}

/// Only copied facts may leave inspection; borrowed text/array values cannot.
fn inspect_parameters<R>(
    inspect: impl for<'parameters> FnOnce(Option<&'parameters [super::types::Datum<'parameters>]>) -> R,
) -> R {
    let current = PARAMETERS.with(core::cell::Cell::get);
    let current = current.filter(|current| match current.source {
        ParameterSource::Bound => super::exec::bound_parameter_context_active(),
        ParameterSource::Inline { depth } => {
            !super::exec::bound_parameter_context_active()
                && depth == super::exec::routine_parameter_depth()
        }
    });
    let values = current.map(|current| {
        // SAFETY: with_parameters keeps the immutable slice and its values live
        // until its private scope restores the previous pointer. Inspection is
        // confined to that thread and cannot return a reference into the slice.
        unsafe {
            core::slice::from_raw_parts(
                current.values.cast::<super::types::Datum<'_>>(),
                current.count,
            )
        }
    });
    inspect(values)
}

#[derive(Clone, Copy)]
struct Candidate<'a> {
    oid: i32,
    version: u64,
    site: InvocationSite,
    input_collation: super::ast::Collation,
    arguments: &'a [inline::ArgumentFacts],
}

impl<'a> Candidate<'a> {
    fn detach(
        oid: i32,
        version: u64,
        site: InvocationSite,
        input_collation: super::ast::Collation,
        arguments: &[inline::ArgumentFacts],
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let arguments = arena
            .alloc_persistent_slice_with(arguments.len(), |at| arguments[at])
            .map_err(|_| super::eval::arena_full())?;
        Ok(Self {
            oid,
            version,
            site,
            input_collation,
            arguments,
        })
    }
}

pub(crate) struct CapturedPlannerInputs<'a> {
    candidates: &'a [Candidate<'a>],
}

/// Only detached type/effect facts escape the source plan's schema scope.
pub(crate) fn capture_planner_inputs<'a, 'source>(
    statement: &Stmt<'source>,
    outer: Option<&dyn ColumnLookup<'source>>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<CapturedPlannerInputs<'a>, SqlError> {
    let mut candidates = ArenaList::new_persistent(arena);
    let mark = arena.mark();
    let captured = query::visit_planner_calls(
        statement,
        storage,
        txid,
        arena,
        outer,
        &mut |expression, columns, assignment| {
            if let Some(assignment) = assignment {
                if let Some(candidate) =
                    capture_assignment(expression, columns, assignment, storage, txid, arena)?
                {
                    candidates
                        .push(candidate)
                        .map_err(|_| super::eval::arena_full())?;
                }
                Ok(())
            } else {
                capture_expression_into(expression, columns, storage, txid, arena, &mut candidates)
            }
        },
    );
    // Schema and argument scratch is temporary; candidate facts own only
    // persistent allocations and checked handles into the persistent tail.
    unsafe { arena.rewind_to(mark) };
    captured?;
    Ok(CapturedPlannerInputs {
        candidates: candidates.as_slice(),
    })
}

fn missing_contract() -> SqlError {
    sql_err!(
        sqlstate::INTERNAL_ERROR,
        "selected planner input contract is unavailable"
    )
}

fn capture_expression_into<'a>(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    candidates: &mut ArenaList<'a, Candidate<'a>>,
) -> Result<(), SqlError> {
    if let Expr::Cast {
        operand: Expr::Array(items),
        binding: super::ast::CastBinding::Resolve,
        source_position,
        type_mod,
        ..
    } = expression
        && let Some(position) = source_position.get()
        && let Some(super::types::ColType::Array(element)) = storage
            .routine_result_for_oid(infer_routine_argument_oid(expression, columns)?, txid)
            .map(|result| result.ctype)
    {
        return capture_array_constructor(
            items,
            columns,
            (position, *type_mod, element),
            storage,
            txid,
            arena,
            &mut 0,
            candidates,
        );
    }
    if let Some(candidate) = capture(expression, columns, storage, txid, arena)? {
        candidates
            .push(candidate)
            .map_err(|_| super::eval::arena_full())?;
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "constructor leaves preserve the cast source, target type, and shared lexical ordinal"
)]
fn capture_array_constructor<'a>(
    items: &[&Expr<'_>],
    columns: &dyn ColTypeResolver,
    context: (usize, i32, super::types::ArrElem),
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    next: &mut usize,
    candidates: &mut ArenaList<'a, Candidate<'a>>,
) -> Result<(), SqlError> {
    let (position, type_mod, element) = context;
    let types = super::eval::array_constructor_type(
        items,
        Some(element),
        &|expression| infer_routine_argument_oid(expression, columns),
        &|oid| storage.domain_base_type_for_oid(oid, txid),
    )?;
    let target = if types.multidimensional {
        element.array_oid()
    } else {
        element.element_oid()
    };
    let catalog = query::storage_catalog(storage, arena, txid);
    for expression in items {
        if let Expr::Array(children) = expression {
            capture_array_constructor(
                children, columns, context, storage, txid, arena, next, candidates,
            )?;
            continue;
        }
        let element = *next;
        *next = next.checked_add(1).ok_or_else(super::eval::arena_full)?;
        let source = infer_routine_argument_oid(expression, columns)?;
        if source == target || source == super::types::oid::UNKNOWN {
            continue;
        }
        if let Some(cast) = super::eval::CatalogAccess::bind_defined_cast(
            &catalog,
            source,
            target,
            super::eval::DefinedCastContext::Explicit,
        )? && let Some(candidate) = capture_cast_function(
            expression,
            columns,
            &cast,
            (InvocationSite::ArrayElement { position, element }, type_mod),
            storage,
            txid,
            arena,
        )? {
            candidates
                .push(candidate)
                .map_err(|_| super::eval::arena_full())?;
        }
    }
    Ok(())
}

fn capture<'a>(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<Candidate<'a>>, SqlError> {
    match expression {
        Expr::Cast { .. } => {
            return capture_explicit_cast(expression, columns, storage, txid, arena);
        }
        Expr::Binary { .. } | Expr::Unary { .. } => {
            return capture_operator(expression, columns, storage, txid, arena);
        }
        Expr::Call {
            kind: CallKind::SqlSyntax,
            name,
            ..
        } if super::ast::catalog_operator_call(name).is_some() => {
            return capture_operator(expression, columns, storage, txid, arena);
        }
        _ => {}
    }
    let Expr::Call {
        kind: CallKind::Routine,
        name,
        bound_oid,
        source_position,
        args,
        argument_names,
        variadic,
        ..
    } = expression
    else {
        return Ok(None);
    };
    let Some(position) = source_position.get() else {
        return Ok(None);
    };
    let has_stored_name = (0..storage.routine_count()).any(|slot| {
        let Some(identity) = storage.routine_identity_for(slot, txid) else {
            return false;
        };
        if let Some(oid) = bound_oid {
            return identity.oid == *oid;
        }
        *name == identity.name.as_str()
            || name
                .strip_suffix(identity.name.as_str())
                .is_some_and(|prefix| prefix.ends_with('.'))
    });
    if !has_stored_name {
        return Ok(None);
    }
    let mut oids = [super::types::oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
    if args.len() > oids.len() {
        return Err(missing_contract());
    }
    for (oid, expression) in oids.iter_mut().zip(*args) {
        *oid = infer_routine_argument_oid(expression, columns)?;
    }
    let selection = match bound_oid {
        Some(oid) => storage
            .routine_slot_by_oid(*oid, txid)
            .map(RoutineCallSelection::Stored),
        None => Some(storage.select_routine_call(
            name,
            argument_names,
            &oids[..args.len()],
            *variadic,
            txid,
            RoutineCallKind::Function,
        )),
    };
    let Some(RoutineCallSelection::Stored(slot)) = selection else {
        return Ok(None);
    };
    let declared = storage.routine_for(slot, txid);
    if !matches!(declared.kind, crate::storage::RoutineKind::Function { .. }) {
        return Ok(None);
    }
    let Some(routine) = storage.function_for_captured_call(
        crate::storage::routine_oid(&declared),
        argument_names,
        &oids[..args.len()],
        *variadic,
        txid,
    ) else {
        return Err(missing_contract());
    };
    if routine.body_kind != RoutineBodyKind::String
        || !inline::declaration(
            &routine,
            routine.argument_count,
            false,
            storage.require_routine_execute(slot, txid).is_ok(),
        )
    {
        return Ok(None);
    }
    let (_, mapping) = storage
        .routine_call_mapping(slot, argument_names, args.len(), *variadic, txid)
        .ok_or_else(missing_contract)?;
    let written = inline::argument_facts(args, columns, storage, txid, arena)?;
    let input_collation = inline::call_collation(
        args,
        &mapping[..args.len()],
        &routine,
        columns,
        storage,
        txid,
        arena,
    )?;
    let mut normalized = ArenaList::new(arena);
    for input in 0..routine.argument_count {
        let parameter = routine
            .parameter_for_input(input)
            .ok_or_else(missing_contract)?;
        let target = storage
            .routine_type_oid(
                routine.arguments()[input].ctype,
                routine.arguments()[input].user_type,
                txid,
            )
            .ok_or_else(missing_contract)?;
        let mut elements = ArenaList::new(arena);
        for (at, mapped) in mapping[..args.len()].iter().enumerate() {
            if usize::from(*mapped) == input {
                elements
                    .push(written[at])
                    .map_err(|_| super::eval::arena_full())?;
            }
        }
        let fact = if elements.is_empty() {
            let source = parameter.mode.default().ok_or_else(missing_contract)?;
            let parameter_index = routine
                .parameters()
                .iter()
                .enumerate()
                .filter(|(_, parameter)| parameter.mode.is_input())
                .nth(input)
                .ok_or_else(missing_contract)?
                .0;
            let dependencies = storage.snapshot_routine_dependencies_for(slot, txid, arena)?;
            let expression = query::bind_routine_parameter_default(
                source.as_str(),
                parameter_index,
                storage,
                txid,
                dependencies.view(),
                arena,
            )?;
            inline::argument_facts(&[expression], columns, storage, txid, arena)?[0]
        } else if matches!(
            parameter.mode,
            crate::storage::RoutineParameterMode::Variadic { .. }
        ) && !*variadic
        {
            let super::types::ColType::Array(element) = routine.arguments()[input].ctype else {
                return Err(missing_contract());
            };
            let mut converted = ArenaList::new(arena);
            for element_fact in elements.as_slice() {
                converted
                    .push(inline::convert_argument(
                        *element_fact,
                        element.element_oid(),
                        storage,
                        txid,
                        arena,
                    )?)
                    .map_err(|_| super::eval::arena_full())?;
            }
            inline::array_argument_facts(converted.as_slice(), target, element, arena)?
        } else {
            if elements.len() != 1 {
                return Err(missing_contract());
            }
            elements.as_slice()[0]
        };
        normalized
            .push(inline::convert_argument(
                fact, target, storage, txid, arena,
            )?)
            .map_err(|_| super::eval::arena_full())?;
    }
    if routine.attributes.strict
        && normalized
            .as_slice()
            .iter()
            .any(|argument| argument.known_null)
    {
        return Ok(None);
    }
    Candidate::detach(
        crate::storage::routine_oid(&routine),
        routine.version,
        InvocationSite::Expression(position),
        input_collation,
        normalized.as_slice(),
        arena,
    )
    .map(Some)
}

fn capture_assignment<'a>(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    assignment: (i32, i32, super::type_inputs::AssignmentSite),
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<Candidate<'a>>, SqlError> {
    let (target, type_mod, site) = assignment;
    let source = infer_routine_argument_oid(expression, columns)?;
    if source == target || source == super::types::oid::UNKNOWN {
        return Ok(None);
    }
    let catalog = query::storage_catalog(storage, arena, txid);
    let Some(cast) = super::eval::CatalogAccess::bind_defined_cast(
        &catalog,
        source,
        target,
        super::eval::DefinedCastContext::Assignment,
    )?
    else {
        return Ok(None);
    };
    capture_cast_function(
        expression,
        columns,
        &cast,
        (InvocationSite::Assignment(site), type_mod),
        storage,
        txid,
        arena,
    )
}

fn capture_explicit_cast<'a>(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<Candidate<'a>>, SqlError> {
    let Expr::Cast {
        source_position,
        operand,
        type_mod,
        binding,
        ..
    } = expression
    else {
        return Ok(None);
    };
    if *binding != super::ast::CastBinding::Resolve {
        return Ok(None);
    }
    let Some(position) = source_position.get() else {
        return Ok(None);
    };
    let target = infer_routine_argument_oid(expression, columns)?;
    let source = inline::cast_source_oid(operand, target, columns, storage, txid)?;
    if source == target || source == super::types::oid::UNKNOWN {
        return Ok(None);
    }
    let catalog = query::storage_catalog(storage, arena, txid);
    let Some(cast) = super::eval::CatalogAccess::bind_defined_cast(
        &catalog,
        source,
        target,
        super::eval::DefinedCastContext::Explicit,
    )?
    else {
        return Ok(None);
    };
    capture_cast_function(
        operand,
        columns,
        &cast,
        (InvocationSite::Expression(position), *type_mod),
        storage,
        txid,
        arena,
    )
}

fn capture_cast_function<'a>(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    cast: &super::eval::BoundCast,
    site: (InvocationSite, i32),
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<Candidate<'a>>, SqlError> {
    let (site, type_mod) = site;
    let crate::storage::CastMethod::Function(oid) = cast.definition.method else {
        return Ok(None);
    };
    let Some(slot) = storage.routine_slot_by_oid(oid, txid) else {
        return Ok(None);
    };
    let mut routine = storage.routine_for(slot, txid);
    routine.attributes.strict = cast.strict;
    if routine.body_kind != RoutineBodyKind::String
        || !inline::declaration(
            &routine,
            routine.argument_count,
            false,
            storage.require_routine_execute(slot, txid).is_ok(),
        )
    {
        return Ok(None);
    }
    let source_facts = inline::cast_source_argument(
        inline::argument_facts(&[expression], columns, storage, txid, arena)?[0],
        cast.leaf_source_oid,
        storage
            .routine_result_for_oid(cast.source_oid, txid)
            .is_some_and(|source| {
                matches!(source.ctype, super::types::ColType::Array(element)
                    if element.array_oid() != cast.leaf_source_oid)
            }),
    );
    if routine.attributes.strict && source_facts.known_null {
        return Ok(None);
    }
    let mut arguments = ArenaList::new(arena);
    arguments
        .push(source_facts)
        .map_err(|_| super::eval::arena_full())?;
    if routine.argument_count >= 2 {
        arguments
            .push(inline::literal_argument(
                super::types::Datum::Int4(type_mod),
                super::types::oid::INT4,
                arena,
            )?)
            .map_err(|_| super::eval::arena_full())?;
    }
    if routine.argument_count == 3 {
        arguments
            .push(inline::literal_argument(
                super::types::Datum::Bool(matches!(
                    cast.context,
                    super::eval::DefinedCastContext::Explicit
                )),
                super::types::oid::BOOL,
                arena,
            )?)
            .map_err(|_| super::eval::arena_full())?;
    }
    Candidate::detach(
        oid,
        routine.version,
        site,
        inline::cast_collation(expression, columns, storage, txid, arena)?,
        arguments.as_slice(),
        arena,
    )
    .map(Some)
}

fn capture_operator<'a>(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<Candidate<'a>>, SqlError> {
    let Some(position) = expression.source_position().get() else {
        return Ok(None);
    };
    let pair;
    let (schema, symbol, arguments): (_, _, &[&Expr<'_>]) = match expression {
        Expr::Binary {
            operator,
            left,
            right,
            ..
        } => {
            let Some(symbol) = operator.operator_name() else {
                return Ok(None);
            };
            pair = [*left, *right];
            (None, symbol, &pair)
        }
        Expr::Unary {
            operator, operand, ..
        } => {
            let symbol = match operator {
                super::ast::UnaryOp::Neg => "-",
                super::ast::UnaryOp::BitNot => "~",
                super::ast::UnaryOp::SquareRoot => "|/",
                super::ast::UnaryOp::CubeRoot => "||/",
                super::ast::UnaryOp::AbsoluteValue => "@",
                super::ast::UnaryOp::TextSearchNot => "!!",
                super::ast::UnaryOp::Not => return Ok(None),
            };
            pair = [*operand, *operand];
            (None, symbol, &pair[..1])
        }
        Expr::Call {
            kind: CallKind::SqlSyntax,
            name,
            args,
            ..
        } => {
            let Some((schema, symbol)) = super::ast::catalog_operator_call(name) else {
                return Ok(None);
            };
            (schema, symbol, *args)
        }
        _ => return Ok(None),
    };
    if !(1..=2).contains(&arguments.len()) {
        return Err(missing_contract());
    }
    if !storage.operators_visible_to(txid).any(|(_, operator)| {
        operator.name.as_str() == symbol
            && match schema {
                Some(schema) => operator.schema.as_str() == schema,
                None => storage.schema_is_on_path(operator.schema, txid),
            }
    }) {
        return Ok(None);
    }
    let mut actual = [super::types::oid::UNKNOWN; 2];
    for (oid, expression) in actual.iter_mut().zip(arguments) {
        *oid = infer_routine_argument_oid(expression, columns)?;
    }
    let (left, right) = if arguments.len() == 1 {
        (None, Some(actual[0]))
    } else {
        (Some(actual[0]), Some(actual[1]))
    };
    let Some(slot) = storage.operator_slot_for_oids(schema, symbol, left, right, txid)? else {
        return Ok(None);
    };
    let operator = storage.operator_for(slot, txid);
    let Some(oid) = operator.implementation.routine() else {
        return Ok(None);
    };
    let Some(routine_slot) = storage.routine_slot_by_oid(oid, txid) else {
        return Ok(None);
    };
    let mut expected = [super::types::oid::UNKNOWN; 2];
    for (expected, parameter) in expected.iter_mut().zip(
        [operator.signature.left, operator.signature.right]
            .into_iter()
            .flatten(),
    ) {
        *expected = storage
            .routine_type_oid(parameter.ctype, parameter.user_type, txid)
            .ok_or_else(missing_contract)?;
    }
    let names = [None; 2];
    let routine = storage
        .function_for_captured_call(
            oid,
            &names[..arguments.len()],
            &expected[..arguments.len()],
            false,
            txid,
        )
        .ok_or_else(missing_contract)?;
    if routine.body_kind != RoutineBodyKind::String
        || !inline::declaration(
            &routine,
            routine.argument_count,
            false,
            storage.require_routine_execute(routine_slot, txid).is_ok(),
        )
    {
        return Ok(None);
    }
    if routine.argument_count != arguments.len() {
        return Err(missing_contract());
    }
    let written = inline::argument_facts(arguments, columns, storage, txid, arena)?;
    let mut normalized = ArenaList::new(arena);
    for (facts, expected) in written.iter().zip(expected) {
        normalized
            .push(inline::convert_argument(
                *facts, expected, storage, txid, arena,
            )?)
            .map_err(|_| super::eval::arena_full())?;
    }
    if routine.attributes.strict
        && normalized
            .as_slice()
            .iter()
            .any(|argument| argument.known_null)
    {
        return Ok(None);
    }
    let mapping = [0, 1];
    Candidate::detach(
        oid,
        routine.version,
        InvocationSite::Expression(position),
        inline::call_collation(
            arguments,
            &mapping[..arguments.len()],
            &routine,
            columns,
            storage,
            txid,
            arena,
        )?,
        normalized.as_slice(),
        arena,
    )
    .map(Some)
}

impl Engine {
    #[expect(
        clippy::too_many_arguments,
        reason = "PL planning uses the restored lexical source, local types and existing execution context"
    )]
    pub(super) fn prepare_plpgsql_planner_inputs<'a>(
        &mut self,
        site: super::plpgsql_inputs::Site<'a>,
        row: &dyn ColumnLookup<'a>,
        prepared: super::PreparedTypeInputs<'a>,
        arena: &'a Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut CursorPool,
        guc: &GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let captured = (|| {
            let mut parser =
                Parser::new(site.text, arena).map_err(|error| super::parse_error_to_sql(&error))?;
            parser.set_type_inputs(prepared.bindings);
            let mut candidates = ArenaList::new_persistent(arena);
            if site.kind == super::plpgsql_inputs::Kind::Expression {
                let expression = parser
                    .expression(0)
                    .map_err(|error| super::parse_error_to_sql(&error))?;
                let mark = arena.mark();
                let captured = query::visit_planner_expression(
                    expression,
                    &self.storage,
                    txn.txid,
                    arena,
                    row,
                    &mut |expression, columns, _assignment| {
                        capture_expression_into(
                            expression,
                            columns,
                            &self.storage,
                            txn.txid,
                            arena,
                            &mut candidates,
                        )
                    },
                );
                unsafe { arena.rewind_to(mark) };
                captured?;
            } else {
                while let Some(statement) = parser
                    .next_stmt()
                    .map_err(|error| super::parse_error_to_sql(&error))?
                {
                    let captured = capture_planner_inputs(
                        &statement,
                        Some(row),
                        &self.storage,
                        txn.txid,
                        arena,
                    )?;
                    for candidate in captured.candidates {
                        candidates
                            .push(*candidate)
                            .map_err(|_| super::eval::arena_full())?;
                    }
                }
            }
            type_inputs::validate_restored(&parser, &self.storage, txn.txid)?;
            Ok(CapturedPlannerInputs {
                candidates: candidates.as_slice(),
            })
        })();
        let captured = match captured {
            Ok(value) => value,
            Err(error) => return Ok(Err(error)),
        };
        type_input_plans::with_owner(prepared.owner, || {
            self.prepare_captured_planner_inputs(
                captured, arena, txn, sqlprep, cursors, guc, responder,
            )
        })
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "detached planner facts are admitted after releasing the cursor's immutable schema borrow"
    )]
    pub(crate) fn prepare_captured_planner_inputs<'a>(
        &mut self,
        captured: CapturedPlannerInputs<'a>,
        arena: &'a Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut CursorPool,
        guc: &GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        self.prepare_captured_planner_inputs_under(
            captured,
            arena,
            txn,
            sqlprep,
            cursors,
            guc,
            responder,
            &[],
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "planner admission shares the statement transaction, session, bounded arena and response sink"
    )]
    pub(crate) fn prepare_planner_inputs<'source>(
        &mut self,
        statement: &Stmt<'source>,
        outer: Option<&dyn ColumnLookup<'source>>,
        arena: &Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut CursorPool,
        guc: &GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        self.prepare_planner_inputs_under(
            statement,
            outer,
            arena,
            txn,
            sqlprep,
            cursors,
            guc,
            responder,
            &[],
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "recursive planner admission retains explicit routine ancestry and statement execution context"
    )]
    fn prepare_planner_inputs_under<'source>(
        &mut self,
        statement: &Stmt<'source>,
        outer: Option<&dyn ColumnLookup<'source>>,
        arena: &Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut CursorPool,
        guc: &GucState,
        responder: &mut Responder,
        ancestors: &[i32],
    ) -> Result<Result<(), SqlError>, WireFull> {
        let captured =
            match capture_planner_inputs(statement, outer, &self.storage, txn.txid, arena) {
                Ok(captured) => captured,
                Err(error) => return Ok(Err(error)),
            };
        self.prepare_captured_planner_inputs_under(
            captured, arena, txn, sqlprep, cursors, guc, responder, ancestors,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "detached planning retains the statement execution context and explicit routine ancestry"
    )]
    fn prepare_captured_planner_inputs_under<'a>(
        &mut self,
        captured: CapturedPlannerInputs<'a>,
        arena: &'a Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut CursorPool,
        guc: &GucState,
        responder: &mut Responder,
        ancestors: &[i32],
    ) -> Result<Result<(), SqlError>, WireFull> {
        for candidate in captured.candidates {
            if ancestors.contains(&candidate.oid) {
                continue;
            }
            let result = self.prepare_inline_attempt(
                *candidate, arena, txn, sqlprep, cursors, guc, responder, ancestors,
            )?;
            if result.is_err() {
                return Ok(result);
            }
        }
        Ok(Ok(()))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "an inline attempt must retain its transaction, lexical owner and routine ancestry across checked-input waits"
    )]
    fn prepare_inline_attempt<'a>(
        &mut self,
        candidate: Candidate<'a>,
        arena: &'a Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut CursorPool,
        guc: &GucState,
        responder: &mut Responder,
        ancestors: &[i32],
    ) -> Result<Result<(), SqlError>, WireFull> {
        let result = (|| {
            let slot = self
                .storage
                .routine_slot_by_oid(candidate.oid, txn.txid)
                .ok_or_else(missing_contract)?;
            let declared = self.storage.routine_for(slot, txn.txid);
            let mut inputs = [super::types::oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
            for (output, input) in inputs.iter_mut().zip(candidate.arguments) {
                *output = input.type_oid;
            }
            let variadic = declared.parameters().iter().any(|parameter| {
                matches!(
                    parameter.mode,
                    crate::storage::RoutineParameterMode::Variadic { .. }
                )
            });
            let routine = self
                .storage
                .function_for_captured_call(
                    candidate.oid,
                    &[],
                    &inputs[..candidate.arguments.len()],
                    variadic,
                    txn.txid,
                )
                .ok_or_else(missing_contract)?;
            if routine.version != candidate.version {
                return Err(missing_contract());
            }
            let key = Key {
                owner: type_input_plans::owner()?,
                source: Source::SqlInlineAttempt {
                    routine_oid: candidate.oid,
                    definition_version: candidate.version,
                    site: candidate.site,
                },
            };
            if key.owner == Owner::Unlocated {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "SQL planner inputs require a retained source owner"
                ));
            }
            let body = arena
                .alloc_str(routine.body.as_str())
                .map_err(|_| super::eval::arena_full())?;
            Ok((routine, key, body))
        })();
        let (routine, key, body) = match result {
            Ok(value) => value,
            Err(error) => return Ok(Err(error)),
        };
        let existing =
            match super::sequence::replayed_input_plan().and_then(|replayed| match replayed {
                Some(plan) => sqlprep.routine_inputs.lookup_replayed(plan, key).map(Some),
                None => sqlprep.routine_inputs.find(key),
            }) {
                Ok(value) => value,
                Err(error) => return Ok(Err(error)),
            };
        let rejected = matches!(
            &existing,
            Some(Lookup::Ready(
                _,
                InputImage::InlineAttempt {
                    disposition: InlineDisposition::Rejected,
                    ..
                }
            ))
        );
        let parse = |bindings: &'a [type_inputs::Binding<'a>]| -> Result<_, SqlError> {
            let name = arena
                .alloc_str(routine.name.as_str())
                .map_err(|_| super::eval::arena_full())?;
            let mut parser = Parser::new(body, arena)
                .and_then(|parser| parser.with_routine_parameters(name, routine.arguments()))
                .map_err(|error| super::parse_error_to_sql(&error))?;
            parser.capture_type_inputs();
            parser.set_type_inputs(bindings);
            let mut statements = ArenaList::new(arena);
            while let Some(statement) = parser
                .next_stmt()
                .map_err(|error| super::parse_error_to_sql(&error))?
            {
                statements
                    .push(statement)
                    .map_err(|_| super::eval::arena_full())?;
            }
            Ok((parser, statements.as_slice()))
        };
        let raw = if existing.is_none() || matches!(&existing, Some(Lookup::Waiting(_))) {
            match parse(&[]) {
                Ok(value) => Some(value),
                Err(error) => return Ok(Err(error)),
            }
        } else {
            None
        };
        let discovery = (|| {
            if let Some((parser, statements)) = &raw
                && statements.len() == 1
            {
                let catalog = query::storage_catalog(&self.storage, arena, txn.txid);
                let _parameters = super::exec::enter_routine_parameter_types(
                    routine.arguments(),
                    Some(&catalog),
                    candidate.input_collation,
                )?;
                query::capture_contextual_type_inputs(
                    parser,
                    &statements[0],
                    &self.storage,
                    txn.txid,
                    arena,
                )?;
            }
            Ok(())
        })();
        if let Err(error) = discovery {
            return Ok(Err(error));
        }
        let candidates = raw
            .as_ref()
            .filter(|(_, statements)| statements.len() == 1)
            .map_or(&[][..], |(parser, _)| parser.type_input_candidates());
        let prepared = match self.prepare_type_input_plan(
            TypeInputPlanSource {
                key: Some(key),
                reserved: None,
                source: key.source,
                text: body,
                candidates,
                captured: None,
            },
            arena,
            txn,
            sqlprep,
            cursors,
            guc,
            responder,
        )? {
            Ok(prepared) => prepared,
            Err(error) => return Ok(Err(error)),
        };
        if rejected {
            return Ok(Ok(()));
        }
        let classified = (|| {
            let (parser, statements) = parse(prepared.bindings)?;
            if statements.len() != 1 {
                let Owner::Nested(plan) = prepared.owner else {
                    return Err(missing_contract());
                };
                sqlprep
                    .routine_inputs
                    .finish_inline_attempt(plan, InlineDisposition::Rejected)?;
                return Ok((false, statements));
            }
            type_inputs::validate_restored(&parser, &self.storage, txn.txid)?;
            let catalog = query::storage_catalog(&self.storage, arena, txn.txid);
            let _parameters = super::exec::enter_routine_parameter_types(
                routine.arguments(),
                Some(&catalog),
                candidate.input_collation,
            )?;
            let columns = super::exec::CatalogCols {
                definition: None,
                alias: None,
                output_aliases: &[],
                storage: &self.storage,
                txid: txn.txid,
                arena,
            };
            let accepted = inline::accepted(
                &routine,
                statements,
                candidate.arguments,
                &inline::Context {
                    storage: &self.storage,
                    txid: txn.txid,
                    arena,
                    body_columns: &columns,
                },
            )?;
            let Owner::Nested(plan) = prepared.owner else {
                return Err(missing_contract());
            };
            sqlprep.routine_inputs.finish_inline_attempt(
                plan,
                if accepted {
                    InlineDisposition::Accepted
                } else {
                    InlineDisposition::Rejected
                },
            )?;
            Ok((accepted, statements))
        })();
        let (accepted, statements) = match classified {
            Ok(value) => value,
            Err(error) => return Ok(Err(error)),
        };
        if accepted {
            let parameters = {
                let catalog = query::storage_catalog(&self.storage, arena, txn.txid);
                super::exec::enter_routine_parameter_types(
                    routine.arguments(),
                    Some(&catalog),
                    candidate.input_collation,
                )
            };
            let _parameters = match parameters {
                Ok(parameters) => parameters,
                Err(error) => return Ok(Err(error)),
            };
            let values = match arena
                .alloc_slice_with(candidate.arguments.len(), |_| super::types::Datum::Null)
            {
                Ok(values) => values,
                Err(_) => return Ok(Err(super::eval::arena_full())),
            };
            let known = match arena.alloc_slice_with(candidate.arguments.len(), |_| false) {
                Ok(known) => known,
                Err(_) => return Ok(Err(super::eval::arena_full())),
            };
            for ((value, known), argument) in values
                .iter_mut()
                .zip(known.iter_mut())
                .zip(candidate.arguments)
            {
                match argument.constant(arena) {
                    Ok(Some(constant)) => {
                        *value = constant;
                        *known = true;
                    }
                    Ok(None) => {}
                    Err(error) => return Ok(Err(error)),
                }
            }
            let ancestry = match arena.alloc_slice_with(ancestors.len() + 1, |at| {
                if at < ancestors.len() {
                    ancestors[at]
                } else {
                    candidate.oid
                }
            }) {
                Ok(value) => value,
                Err(_) => return Ok(Err(super::eval::arena_full())),
            };
            return with_inline_parameters(values, known, || {
                for statement in statements {
                    let result = type_input_plans::with_owner(prepared.owner, || {
                        self.prepare_planner_inputs_under(
                            statement, None, arena, txn, sqlprep, cursors, guc, responder, ancestry,
                        )
                    })?;
                    if result.is_err() {
                        return Ok(result);
                    }
                }
                Ok(Ok(()))
            });
        }
        Ok(Ok(()))
    }
}

#[cfg(test)]
mod parameter_tests {
    use super::*;
    use crate::{mem::budget::Budget, sql::types::Datum};

    #[test]
    fn planner_parameters_confine_borrows_and_restore_nested_and_formal_scopes() {
        let mut budget = Budget::new(4096);
        let mut arena = Arena::new(&mut budget, "planner parameter ownership", 4096).unwrap();
        let text = arena.alloc_str("client value").unwrap();
        let outer = [Datum::Text(text), Datum::Null, Datum::Int4(9)];
        crate::mem::guard::forbid_alloc(|| {
            assert!(inspect_parameters(|values| values.is_none()));
            with_parameters(Some(&outer), || {
                assert!(inspect_parameters(|values| matches!(
                    values,
                    Some([Datum::Text("client value"), Datum::Null, Datum::Int4(9)])
                )));
                let inner = [Datum::Text("nested value")];
                let failed = with_parameters(Some(&inner), || {
                    assert!(inspect_parameters(|values| matches!(
                        values,
                        Some([Datum::Text("nested value")])
                    )));
                    Err::<(), _>(())
                });
                assert!(failed.is_err());
                with_parameters(None, || {
                    assert!(inspect_parameters(|values| values.is_none()))
                });
                with_parameters(Some(&[]), || {
                    assert!(inspect_parameters(|values| matches!(values, Some([]))))
                });
                let formal = super::super::exec::enter_routine_parameter_types(
                    &[],
                    None,
                    crate::sql::ast::Collation::None,
                )
                .unwrap();
                assert!(inspect_parameters(|values| values.is_none()));
                let values = [Datum::Null, Datum::Null, Datum::Int4(9)];
                with_inline_parameters(&values, &[false, true, true], || {
                    assert!(!parameter_known(1));
                    assert!(parameter_known(2));
                    assert!(parameter_known(3));
                    let nested = super::super::exec::enter_routine_parameter_types(
                        &[],
                        None,
                        crate::sql::ast::Collation::None,
                    )
                    .unwrap();
                    assert!(inspect_parameters(|values| values.is_none()));
                    assert!(!parameter_known(3));
                    drop(nested);
                    assert!(parameter_known(3));
                    assert!(inspect_parameters(|values| matches!(
                        values,
                        Some([Datum::Null, Datum::Null, Datum::Int4(9)])
                    )));
                });
                assert!(inspect_parameters(|values| values.is_none()));
                drop(formal);
                assert!(inspect_parameters(|values| matches!(
                    values,
                    Some([Datum::Text("client value"), Datum::Null, Datum::Int4(9)])
                )));
            });
            assert!(inspect_parameters(|values| values.is_none()));
        });
        arena.reset();
        let replacement = arena.alloc_str("reused storage").unwrap();
        let values = [Datum::Text(replacement)];
        crate::mem::guard::forbid_alloc(|| {
            assert!(inspect_parameters(|values| values.is_none()));
            with_parameters(Some(&values), || {
                assert!(inspect_parameters(|values| matches!(
                    values,
                    Some([Datum::Text("reused storage")])
                )));
            });
            assert!(inspect_parameters(|values| values.is_none()));
        });
    }
}
