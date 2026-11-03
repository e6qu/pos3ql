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
struct Candidate<'a> {
    oid: i32,
    version: u64,
    site: InvocationSite,
    arguments: &'a [inline::ArgumentFacts],
}

impl<'a> Candidate<'a> {
    fn detach(
        oid: i32,
        version: u64,
        site: InvocationSite,
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
    query::visit_planner_calls(
        statement,
        storage,
        txid,
        arena,
        outer,
        &mut |expression, columns, assignment| {
            let candidate = match assignment {
                Some((target, site)) => {
                    capture_assignment(expression, columns, target, site, storage, txid, arena)?
                }
                None => capture(expression, columns, storage, txid, arena)?,
            };
            if let Some(candidate) = candidate {
                candidates
                    .push(candidate)
                    .map_err(|_| super::eval::arena_full())?;
            }
            Ok(())
        },
    )?;
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

fn capture<'a>(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<Candidate<'a>>, SqlError> {
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
    let Some(routine) = storage.function_for_captured_call(
        crate::storage::routine_oid(&storage.routine_for(slot, txid)),
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
                    )?)
                    .map_err(|_| super::eval::arena_full())?;
            }
            inline::array_argument_facts(converted.as_slice(), target)
        } else {
            if elements.len() != 1 {
                return Err(missing_contract());
            }
            elements.as_slice()[0]
        };
        normalized
            .push(inline::convert_argument(fact, target, storage, txid)?)
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
        normalized.as_slice(),
        arena,
    )
    .map(Some)
}

fn capture_assignment<'a>(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    target: i32,
    site: super::type_inputs::AssignmentSite,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<Candidate<'a>>, SqlError> {
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
    let crate::storage::CastMethod::Function(oid) = cast.definition.method else {
        return Ok(None);
    };
    let slot = storage
        .routine_slot_by_oid(oid, txid)
        .ok_or_else(missing_contract)?;
    let routine = storage.routine_for(slot, txid);
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
    let source_facts = inline::argument_facts(&[expression], columns, storage, txid, arena)?[0];
    if routine.attributes.strict && source_facts.known_null {
        return Ok(None);
    }
    let mut arguments = ArenaList::new(arena);
    arguments
        .push(inline::cast_source_argument(
            source_facts,
            cast.leaf_source_oid,
        ))
        .map_err(|_| super::eval::arena_full())?;
    if routine.argument_count >= 2 {
        arguments
            .push(inline::literal_argument(super::types::oid::INT4))
            .map_err(|_| super::eval::arena_full())?;
    }
    if routine.argument_count == 3 {
        arguments
            .push(inline::literal_argument(super::types::oid::BOOL))
            .map_err(|_| super::eval::arena_full())?;
    }
    Candidate::detach(
        oid,
        routine.version,
        InvocationSite::Assignment(site),
        arguments.as_slice(),
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
                query::visit_planner_expression(
                    expression,
                    &self.storage,
                    txn.txid,
                    arena,
                    row,
                    &mut |expression, columns, _assignment| {
                        if let Some(candidate) =
                            capture(expression, columns, &self.storage, txn.txid, arena)?
                        {
                            candidates
                                .push(candidate)
                                .map_err(|_| super::eval::arena_full())?;
                        }
                        Ok(())
                    },
                )?;
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
        let existing = match sqlprep.routine_inputs.find(key) {
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
            let _parameters =
                super::exec::enter_routine_parameter_types(routine.arguments(), Some(&catalog))?;
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
                super::exec::enter_routine_parameter_types(routine.arguments(), Some(&catalog))
            };
            let _parameters = match parameters {
                Ok(parameters) => parameters,
                Err(error) => return Ok(Err(error)),
            };
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
        }
        Ok(Ok(()))
    }
}
