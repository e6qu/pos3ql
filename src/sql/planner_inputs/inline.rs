//! PostgreSQL scalar SQL inlining gates, after analysis and before execution.
//! https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/util/clauses.c

use crate::mem::arena::Arena;
use crate::sql::{
    ast::{self, BinaryOp, CallKind, CastBinding, Expr, SelectItem, Stmt, UnaryOp},
    catalog,
    eval::{SqlError, sqlstate},
    exec::{ColTypeResolver, infer_routine_argument_oid},
    types::{ColType, oid},
};
use crate::sql_err;
use crate::storage::{RoutineDef, RoutineKind, RoutineLanguage, RoutineVolatility, Storage};

struct MetadataColumns<'a> {
    columns: &'a dyn ColTypeResolver,
    storage: &'a Storage,
    txid: u32,
}

impl<'a> crate::sql::eval::ColumnLookup<'a> for MetadataColumns<'_> {
    fn whole_row_field_metadata(
        &self,
        table: &str,
        index: usize,
        _catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
    ) -> Option<(crate::util::StackStr<64>, crate::sql::exec::StaticTypeMeta)> {
        self.columns.whole_row_field(table, index)
    }

    fn column_type_mod(&self, qualifier: Option<&str>, name: &str) -> i32 {
        self.columns
            .column_meta(qualifier, name)
            .map_or(-1, |column| column.type_mod)
    }

    fn lookup(
        &self,
        _qualifier: Option<&str>,
        _name: &str,
    ) -> Result<crate::sql::types::Datum<'a>, SqlError> {
        Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "planner metadata cannot read row values"
        ))
    }
    fn resolve_column_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<ColType, SqlError> {
        self.columns.resolve(qualifier, name)
    }
    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        self.columns
            .column_meta(qualifier, name)
            .map(|metadata| metadata.ctype)
    }
    fn column_user_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Option<crate::storage::UserTypeName> {
        let metadata = self.columns.column_meta(qualifier, name)?;
        self.storage
            .routine_result_for_oid(metadata.type_oid, self.txid)?
            .user_type
    }
    fn collation(&self, qualifier: Option<&str>, name: &str) -> ast::Collation {
        self.columns
            .column_meta(qualifier, name)
            .map_or(ast::Collation::None, |metadata| metadata.collation)
    }
    fn record_field_collation(&self, base: &Expr<'a>, field: &str) -> ast::Collation {
        crate::sql::exec::record_field_metadata(base, field, self.columns)
            .map_or(ast::Collation::None, |metadata| metadata.collation)
    }
    fn whole_row_type(&self, name: &str) -> Option<ColType> {
        self.columns
            .whole_row_scalar_type(name)
            .or_else(|| self.columns.is_whole_row(name).then_some(ColType::Record))
    }
    fn whole_row_is_scalar(&self, name: &str) -> bool {
        self.columns.whole_row_scalar_type(name).is_some()
    }
    fn is_empty_scope(&self) -> bool {
        self.columns.is_empty_scope()
    }
}

pub(super) fn call_collation(
    arguments: &[&Expr<'_>],
    mapping: &[u8],
    routine: &RoutineDef,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<ast::Collation, SqlError> {
    let row = MetadataColumns {
        columns,
        storage,
        txid,
    };
    let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
    let mut derived = None;
    // Only supplied arguments contribute; defaults are inserted afterwards.
    for (expression, mapped) in arguments.iter().zip(mapping) {
        if matches!(expression, Expr::BoundDefault { .. }) {
            continue;
        }
        let input = routine.arguments()[usize::from(*mapped)];
        let collation =
            crate::sql::eval::routine_argument_collation(expression, &row, Some(&catalog))?;
        derived = crate::sql::eval::merge_derived_collations(
            derived,
            crate::sql::eval::DerivedCollation::for_input_type(collation, input.ctype),
        )?;
    }
    Ok(derived.map_or(
        ast::Collation::None,
        crate::sql::eval::DerivedCollation::input_collation,
    ))
}

pub(super) fn cast_collation(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<ast::Collation, SqlError> {
    let row = MetadataColumns {
        columns,
        storage,
        txid,
    };
    let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
    crate::sql::eval::coercion_input_collation(expression, &row, Some(&catalog))
}

pub(crate) struct Context<'a> {
    pub storage: &'a Storage,
    pub txid: u32,
    pub arena: &'a Arena,
    pub body_columns: &'a dyn ColTypeResolver,
}

/// Fold only operations whose selected implementation cannot enter a stored
/// routine. Planner discovery must not execute a function body to find inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConstantValue {
    Null,
    Boolean(bool),
    NonNull,
}

pub(crate) fn constant_value(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<Option<ConstantValue>, SqlError> {
    let mark = arena.mark();
    let result = constant_value_in(expression, columns, storage, txid, arena);
    // The resolver can describe subqueries in this same arena, so it must
    // remain available during inference. Only a copied classification escapes;
    // every schema owner and folded value has ended before releasing scratch.
    unsafe { arena.rewind_to(mark) };
    result
}

fn constant_value_in(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<Option<ConstantValue>, SqlError> {
    use crate::sql::types::Datum;
    if let Expr::Binary {
        operator: operator @ (BinaryOp::And | BinaryOp::Or),
        left,
        right,
        ..
    } = expression
    {
        let settles = *operator == BinaryOp::Or;
        let left = constant_value_in(left, columns, storage, txid, arena)?;
        if left == Some(ConstantValue::Boolean(settles)) {
            return Ok(Some(ConstantValue::Boolean(settles)));
        }
        let right = constant_value_in(right, columns, storage, txid, arena)?;
        if right == Some(ConstantValue::Boolean(settles)) {
            return Ok(Some(ConstantValue::Boolean(settles)));
        }
        return Ok(match (left, right) {
            (Some(ConstantValue::Boolean(_)), Some(ConstantValue::Boolean(_))) => {
                Some(ConstantValue::Boolean(!settles))
            }
            (Some(ConstantValue::Null), Some(ConstantValue::Boolean(_) | ConstantValue::Null))
            | (Some(ConstantValue::Boolean(_)), Some(ConstantValue::Null)) => {
                Some(ConstantValue::Null)
            }
            _ => None,
        });
    }
    if let Expr::Unary {
        operator: UnaryOp::Not,
        operand,
        ..
    } = expression
    {
        return Ok(
            match constant_value_in(operand, columns, storage, txid, arena)? {
                Some(ConstantValue::Boolean(value)) => Some(ConstantValue::Boolean(!value)),
                Some(ConstantValue::Null) => Some(ConstantValue::Null),
                _ => None,
            },
        );
    }
    if !builtin_constant(expression, columns, storage, txid)? {
        return Ok(None);
    }
    let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
    let hooks = crate::sql::eval::EvalHooks {
        catalog: Some(&catalog),
        ..crate::sql::eval::NO_HOOKS
    };
    super::inspect_parameters(|parameters| {
        crate::sql::eval::eval_full(
            expression,
            arena,
            parameters.unwrap_or(crate::sql::eval::NO_PARAMS),
            &crate::sql::eval::NoColumns,
            &hooks,
        )
        .map(|value| {
            Some(match value {
                Datum::Null => ConstantValue::Null,
                Datum::Bool(value) => ConstantValue::Boolean(value),
                _ => ConstantValue::NonNull,
            })
        })
    })
}

fn builtin_constant(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
) -> Result<bool, SqlError> {
    if let Some(index) = parameter_index(expression, columns) {
        return Ok(super::parameter_known(index));
    }
    let safe = match expression {
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::NumericLit(_)
        | Expr::Str(_)
        | Expr::BitLit(_)
        | Expr::BoundLiteral { .. } => true,
        Expr::IsNull { .. } | Expr::Collate { .. } | Expr::Array(_) | Expr::Subscript { .. } => {
            true
        }
        Expr::Like {
            operand,
            pattern,
            case_insensitive,
            ..
        } => {
            let left = infer_routine_argument_oid(operand, columns)?;
            let right = infer_routine_argument_oid(pattern, columns)?;
            let properties = operator_properties(
                storage,
                txid,
                Some("pg_catalog"),
                if *case_insensitive { "~~*" } else { "~~" },
                Some(left),
                Some(right),
            )?;
            properties.known && properties.volatility == 0
        }
        Expr::Unary {
            operator: UnaryOp::Not,
            ..
        }
        | Expr::Binary {
            operator: BinaryOp::And | BinaryOp::Or,
            ..
        } => true,
        Expr::Unary {
            operator: UnaryOp::Neg,
            operand,
            ..
        } => {
            let source = infer_routine_argument_oid(operand, columns)?;
            if storage
                .operator_slot_for_oids(None, "-", None, Some(source), txid)?
                .is_some()
            {
                return Ok(false);
            }
            let properties = operator_properties(storage, txid, None, "-", None, Some(source))?;
            properties.known && properties.volatility == 0
        }
        Expr::Binary {
            operator,
            left,
            right,
            ..
        } => {
            let Some(name) = operator.operator_name() else {
                return Ok(false);
            };
            let left = infer_routine_argument_oid(left, columns)?;
            let right = infer_routine_argument_oid(right, columns)?;
            if storage
                .operator_slot_for_oids(None, name, Some(left), Some(right), txid)?
                .is_some()
            {
                return Ok(false);
            }
            let Some((expected_left, expected_right)) =
                storage.builtin_operator_input_oids(name, Some(left), Some(right), txid)
            else {
                return Ok(false);
            };
            if [left, right]
                .into_iter()
                .zip([expected_left, expected_right])
                .any(|(source, target)| {
                    target.is_some_and(|target| {
                        source != target
                            && catalog::defined_cast_path(storage, txid, source, target).is_some()
                    })
                })
            {
                return Ok(false);
            }
            let properties =
                operator_properties(storage, txid, None, name, Some(left), Some(right))?;
            properties.known && properties.volatility == 0
        }
        Expr::Cast {
            operand,
            type_name,
            binding,
            ..
        } => {
            let Some(target) = ColType::from_sql_name(type_name) else {
                return Ok(false);
            };
            let source = cast_source_oid(operand, target.oid(), columns, storage, txid)?;
            if *binding == CastBinding::Resolve
                && source != target.oid()
                && catalog::defined_cast_path(storage, txid, source, target.oid()).is_some()
            {
                return Ok(false);
            }
            let properties = builtin_cast_properties(storage, txid, source, target.oid());
            properties.known && properties.volatility == 0
        }
        Expr::Call {
            kind: CallKind::SqlSyntax,
            name: "coalesce",
            ..
        } => true,
        Expr::Call {
            kind: CallKind::Routine,
            name,
            bound_oid,
            args,
            argument_names,
            variadic,
            over: None,
            filter: None,
            distinct: false,
            order_by: [],
            star: false,
            ..
        } => {
            let mut inputs = [oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
            if args.len() > inputs.len() {
                return Ok(false);
            }
            for (input, argument) in inputs.iter_mut().zip(*args) {
                *input = infer_routine_argument_oid(argument, columns)?;
            }
            let inputs = &inputs[..args.len()];
            let selected = match bound_oid {
                Some(oid) if storage.routine_slot_by_oid(*oid, txid).is_none() => {
                    crate::storage::RoutineCallSelection::Builtin {
                        oid: *oid,
                        schema: "pg_catalog",
                    }
                }
                Some(_) => return Ok(false),
                None => storage.select_routine_call(
                    name,
                    argument_names,
                    inputs,
                    *variadic,
                    txid,
                    crate::storage::RoutineCallKind::Function,
                ),
            };
            let crate::storage::RoutineCallSelection::Builtin { oid, .. } = selected else {
                return Ok(false);
            };
            let Some(signature) = catalog::builtin_callable_by_oid(oid) else {
                return Ok(false);
            };
            if signature.kind != b'f'
                || signature.returns_set
                || signature.catalog.volatility != "i"
            {
                return Ok(false);
            }
            let Some(expected) =
                storage.builtin_call_input_oids(oid, argument_names, inputs, *variadic, txid)
            else {
                return Ok(false);
            };
            for (source, target) in inputs.iter().copied().zip(expected) {
                if source != target
                    && catalog::defined_cast_path(storage, txid, source, target).is_some()
                {
                    return Ok(false);
                }
                let properties = builtin_cast_properties(storage, txid, source, target);
                if !properties.known || properties.volatility != 0 {
                    return Ok(false);
                }
            }
            true
        }
        _ => false,
    };
    if !safe {
        return Ok(false);
    }
    let mut children_safe = true;
    crate::sql::query::walk_children(expression, &mut |child| {
        children_safe &= builtin_constant(child, columns, storage, txid)?;
        Ok(())
    })?;
    Ok(children_safe)
}

pub(super) fn cast_source_oid(
    operand: &Expr<'_>,
    target: i32,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
) -> Result<i32, SqlError> {
    if let Expr::Array(items) = operand
        && let Some(ColType::Array(element)) = storage
            .routine_result_for_oid(target, txid)
            .map(|result| result.ctype)
    {
        return Ok(crate::sql::eval::array_constructor_type(
            items,
            Some(element),
            &|item| infer_routine_argument_oid(item, columns),
            &|oid| storage.domain_base_type_for_oid(oid, txid),
        )?
        .element
        .array_oid());
    }
    infer_routine_argument_oid(operand, columns)
}

fn parameter_index(expression: &Expr<'_>, columns: &dyn ColTypeResolver) -> Option<u32> {
    match expression {
        Expr::Param(index) => Some(*index),
        Expr::RoutineParam {
            qualifier,
            name,
            index,
            source: ast::ParameterSource::SqlRoutine,
        } if columns.resolve(*qualifier, name).is_err() => Some(*index),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct Properties {
    volatility: u8,
    strict: bool,
    cost: f64,
    constant: bool,
    subplan: bool,
    context_dependent: bool,
    forbidden: bool,
    known: bool,
}

impl Properties {
    const EMPTY: Self = Self {
        volatility: 0,
        strict: true,
        cost: 0.0,
        constant: true,
        subplan: false,
        context_dependent: false,
        forbidden: false,
        known: true,
    };
    const UNKNOWN: Self = Self {
        volatility: 2,
        strict: false,
        cost: f64::INFINITY,
        constant: false,
        known: false,
        ..Self::EMPTY
    };

    fn include(&mut self, other: Self) {
        self.volatility = self.volatility.max(other.volatility);
        self.strict &= other.strict;
        self.cost += other.cost;
        self.constant &= other.constant;
        self.subplan |= other.subplan;
        self.context_dependent |= other.context_dependent;
        self.forbidden |= other.forbidden;
        self.known &= other.known;
    }

    fn folded(mut self) -> Self {
        if self.constant && self.volatility == 0 && !self.forbidden && self.known {
            self.cost = 0.0;
        }
        self
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ArgumentFacts {
    pub type_oid: i32,
    pub known_null: bool,
    constant: Option<crate::mem::arena::ArenaSliceHandle<u8>>,
    properties: Properties,
}

impl ArgumentFacts {
    pub(super) fn constant<'a>(
        self,
        arena: &'a Arena,
    ) -> Result<Option<crate::sql::types::Datum<'a>>, SqlError> {
        let Some(handle) = self.constant else {
            return Ok(None);
        };
        let bytes = {
            let retained = arena.retained_slice_mut(handle).map_err(|_| {
                sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "planner constant value no longer belongs to this workspace"
                )
            })?;
            arena
                .alloc_slice_copy(&retained)
                .map_err(|_| crate::sql::eval::arena_full())?
        };
        let mut value = [crate::sql::types::Datum::Null];
        if crate::sql::exec::decode_projected_row_into(bytes, arena, &mut value)? != 1 {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "planner constant row has invalid width"
            ));
        }
        Ok(Some(value[0]))
    }
}

fn retain_constant(
    value: crate::sql::types::Datum<'_>,
    arena: &Arena,
) -> Result<crate::mem::arena::ArenaSliceHandle<u8>, SqlError> {
    let mark = arena.mark();
    let retained = crate::sql::exec::encode_projected_pub(&[value], arena).and_then(|row| {
        arena
            .retain_slice_with(row.len(), |index| row[index])
            .map_err(|_| crate::sql::eval::arena_full())
    });
    unsafe { arena.rewind_to(mark) };
    retained
}

fn capture_constant(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<Option<crate::mem::arena::ArenaSliceHandle<u8>>, SqlError> {
    if !builtin_constant(expression, columns, storage, txid)? {
        return Ok(None);
    }
    let mark = arena.mark();
    let captured = arena
        .with_remaining_bytes(|scratch| {
            let catalog = crate::sql::query::storage_catalog(storage, scratch, txid);
            let hooks = crate::sql::eval::EvalHooks {
                catalog: Some(&catalog),
                ..crate::sql::eval::NO_HOOKS
            };
            super::inspect_parameters(|parameters| {
                let value = crate::sql::eval::eval_full(
                    expression,
                    scratch,
                    parameters.unwrap_or(crate::sql::eval::NO_PARAMS),
                    &crate::sql::eval::NoColumns,
                    &hooks,
                )?;
                crate::sql::exec::encode_projected_pub(&[value], scratch)
            })
        })
        .map_err(|_| crate::sql::eval::arena_full())
        .and_then(|value| value)
        .and_then(|row| {
            arena
                .retain_slice_with(row.len(), |index| row[index])
                .map_err(|_| crate::sql::eval::arena_full())
        });
    // The checked handle owns tail bytes; no expression or schema scratch escapes.
    unsafe { arena.rewind_to(mark) };
    captured.map(Some)
}

pub(crate) fn declaration(
    routine: &RoutineDef,
    normalized_arity: usize,
    recursive: bool,
    execute_allowed: bool,
) -> bool {
    routine.language == RoutineLanguage::Sql
        && matches!(routine.kind, RoutineKind::Function { result } if result.ctype != ColType::Record)
        && !routine.attributes.security_definer
        && routine.configs().is_empty()
        && routine.argument_count == normalized_arity
        && !recursive
        && execute_allowed
}

pub(crate) fn argument_facts<'a>(
    arguments: &[&Expr<'_>],
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a [ArgumentFacts], SqlError> {
    let facts = arena
        .alloc_slice_with(arguments.len(), |_| ArgumentFacts {
            type_oid: oid::UNKNOWN,
            known_null: false,
            constant: None,
            properties: Properties::UNKNOWN,
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    for (output, argument) in facts.iter_mut().zip(arguments) {
        *output = ArgumentFacts {
            type_oid: infer_routine_argument_oid(argument, columns)?,
            known_null: known_null(argument, columns, storage, txid, arena, 0)?,
            constant: capture_constant(argument, columns, storage, txid, arena)?,
            properties: expression_properties(argument, columns, storage, txid, arena, 0)?,
        };
    }
    Ok(facts)
}

pub(crate) fn literal_argument(
    value: crate::sql::types::Datum<'_>,
    type_oid: i32,
    arena: &Arena,
) -> Result<ArgumentFacts, SqlError> {
    Ok(ArgumentFacts {
        type_oid,
        known_null: value.is_null(),
        constant: Some(retain_constant(value, arena)?),
        properties: Properties::EMPTY,
    })
}

pub(crate) fn cast_source_argument(
    mut argument: ArgumentFacts,
    leaf_source: i32,
    array_element: bool,
) -> ArgumentFacts {
    if argument.type_oid != leaf_source {
        argument.constant = None;
    }
    if array_element {
        // Array coercion supplies a context-bound element placeholder, not
        // the original array value. It prevents SQL-body substitution.
        argument.constant = None;
        argument.known_null = false;
        argument.properties = Properties {
            context_dependent: true,
            ..Properties::EMPTY
        };
    }
    argument.type_oid = leaf_source;
    argument
}

pub(crate) fn convert_argument(
    mut facts: ArgumentFacts,
    expected_oid: i32,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<ArgumentFacts, SqlError> {
    if facts.type_oid != expected_oid {
        let source = facts.type_oid;
        let cast = cast_properties(
            storage,
            txid,
            facts.type_oid,
            expected_oid,
            CastBinding::Resolve,
        )?;
        facts.known_null &= cast.strict
            && storage
                .domain_base_type_for_oid(expected_oid, txid)
                .is_none();
        facts.properties.include(cast);
        facts.properties = facts.properties.folded();
        facts.constant = if facts.constant.is_some() {
            let target = storage
                .routine_result_for_oid(expected_oid, txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "planner argument target type is unavailable"
                    )
                })?;
            let named = crate::storage::NamedTypeKind::from_oid(expected_oid);
            let source_base = storage
                .domain_base_type_for_oid(source, txid)
                .map_or(source, ColType::oid);
            let builtin = builtin_cast_properties(storage, txid, source_base, expected_oid);
            let safe = catalog::defined_cast_path(storage, txid, source, expected_oid).is_none()
                && catalog::defined_cast_path(storage, txid, source_base, expected_oid).is_none()
                && match named {
                    Some((crate::storage::NamedTypeKind::Enum, _, _)) => source == oid::UNKNOWN,
                    Some(_) => false,
                    None => builtin.known && builtin.volatility == 0,
                };
            if safe {
                let mark = arena.mark();
                let converted = (|| {
                    let value = facts.constant(arena)?.expect("constant handle is present");
                    let argument = crate::storage::RoutineArgumentDef {
                        name: crate::storage::SqlName::EMPTY,
                        ctype: target.ctype,
                        user_type: target.user_type,
                    };
                    let value = crate::sql::exec::coerce_routine_argument(
                        value, argument, storage, txid, arena, None,
                    )?;
                    retain_constant(value, arena)
                })();
                unsafe { arena.rewind_to(mark) };
                Some(converted?)
            } else {
                None
            }
        } else {
            None
        };
        facts.type_oid = expected_oid;
    }
    Ok(facts)
}

pub(crate) fn array_argument_facts(
    elements: &[ArgumentFacts],
    array_oid: i32,
    element: crate::sql::types::ArrElem,
    arena: &Arena,
) -> Result<ArgumentFacts, SqlError> {
    let mut properties = Properties {
        strict: false,
        ..Properties::EMPTY
    };
    for element in elements {
        properties.include(element.properties);
    }
    let constant = if elements.iter().all(|element| element.constant.is_some()) {
        let mark = arena.mark();
        let captured = (|| {
            let values = arena
                .alloc_slice_with(elements.len(), |_| crate::sql::types::Datum::Null)
                .map_err(|_| crate::sql::eval::arena_full())?;
            for (value, facts) in values.iter_mut().zip(elements) {
                *value = facts
                    .constant(arena)?
                    .expect("all array elements are constants");
            }
            let raw = crate::sql::array::build(values, arena)?;
            retain_constant(crate::sql::types::Datum::Array { element, raw }, arena)
        })();
        unsafe { arena.rewind_to(mark) };
        Some(captured?)
    } else {
        None
    };
    Ok(ArgumentFacts {
        type_oid: array_oid,
        known_null: false,
        constant,
        properties: properties.folded(),
    })
}

fn known_null(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    depth: usize,
) -> Result<bool, SqlError> {
    if depth >= 512 {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "SQL inlining expression nesting exceeds 512 levels"
        ));
    }
    if let Some(index) = parameter_index(expression, columns) {
        return Ok(super::parameter_known(index)
            && super::inspect_parameters(|parameters| {
                parameters
                    .and_then(|parameters| parameters.get(index as usize - 1))
                    .is_some_and(|value| value.is_null())
            }));
    }
    Ok(match expression {
        Expr::Null => true,
        Expr::BoundLiteral { value, .. } => {
            crate::sql::eval::bound_literal_value(*value, arena)?.is_null()
        }
        Expr::Collate { operand, .. } => {
            known_null(operand, columns, storage, txid, arena, depth + 1)?
        }
        Expr::Cast {
            operand, binding, ..
        } => {
            known_null(operand, columns, storage, txid, arena, depth + 1)?
                && storage
                    .domain_base_type_for_oid(
                        infer_routine_argument_oid(expression, columns)?,
                        txid,
                    )
                    .is_none()
                && cast_properties(
                    storage,
                    txid,
                    infer_routine_argument_oid(operand, columns)?,
                    infer_routine_argument_oid(expression, columns)?,
                    *binding,
                )?
                .strict
        }
        _ => false,
    })
}

fn simple_expression<'a>(statements: &'a [Stmt<'a>]) -> Option<&'a Expr<'a>> {
    let [Stmt::Select(select)] = statements else {
        return None;
    };
    if select.from.is_some()
        || select.where_clause.is_some()
        || !select.with.is_empty()
        || select.distinct
        || !select.distinct_on.is_empty()
        || !select.group_by.is_empty()
        || !select.grouping_sets.is_empty()
        || select.having.is_some()
        || !select.order_by.is_empty()
        || select.limit.is_some()
        || select.offset.is_some()
        || select.set_body.is_some()
        || !select.locking.is_empty()
    {
        return None;
    }
    let [SelectItem::Expr { expression, .. }] = select.items else {
        return None;
    };
    Some(expression)
}

pub(crate) fn accepted(
    routine: &RoutineDef,
    statements: &[Stmt<'_>],
    arguments: &[ArgumentFacts],
    context: &Context<'_>,
) -> Result<bool, SqlError> {
    let Some(expression) = simple_expression(statements) else {
        return Ok(false);
    };
    let RoutineKind::Function { result } = routine.kind else {
        return Ok(false);
    };
    if arguments.len() != routine.argument_count {
        return Ok(false);
    }
    let mut properties = expression_properties(
        expression,
        context.body_columns,
        context.storage,
        context.txid,
        context.arena,
        0,
    )?;
    let source = infer_routine_argument_oid(expression, context.body_columns)?;
    let target = context
        .storage
        .routine_type_oid(result.ctype, result.user_type, context.txid)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "SQL function result type is unavailable during inlining"
            )
        })?;
    if source != target {
        let permitted = source == oid::UNKNOWN
            || context
                .storage
                .routine_implicit_cast(source, target, context.txid)
            || catalog::builtin_cast_context(source, target)
                .is_some_and(|kind| kind.permits_assignment())
            || catalog::defined_cast_path(context.storage, context.txid, source, target)
                .is_some_and(|(_, cast)| cast.context.permits_assignment())
            || context
                .storage
                .routine_result_for_oid(target, context.txid)
                .is_some_and(|result| crate::sql::types::type_category(result.ctype) == "S");
        if !permitted {
            return Ok(false);
        }
        properties.include(cast_properties(
            context.storage,
            context.txid,
            source,
            target,
            CastBinding::Resolve,
        )?);
    }
    let uses = context
        .arena
        .alloc_slice_with(arguments.len(), |_| 0usize)
        .map_err(|_| crate::sql::eval::arena_full())?;
    count_parameters(expression, uses, 0)?;
    Ok(post_analysis(
        routine.attributes.volatility,
        routine.attributes.strict,
        properties,
        uses,
        arguments,
    ))
}

fn post_analysis(
    volatility: RoutineVolatility,
    strict: bool,
    body: Properties,
    uses: &[usize],
    arguments: &[ArgumentFacts],
) -> bool {
    let allowed = match volatility {
        RoutineVolatility::Immutable => 0,
        RoutineVolatility::Stable => 1,
        RoutineVolatility::Volatile => 2,
    };
    if !body.known
        || body.forbidden
        || body.subplan
        || body.context_dependent
        || body.volatility > allowed
        || strict && !body.strict
    {
        return false;
    }
    uses.iter().zip(arguments).all(|(&used, argument)| {
        let properties = argument.properties;
        properties.known
            && !properties.context_dependent
            && (!strict || used != 0)
            && (used <= 1
                || !properties.subplan && properties.cost <= 10.0 && properties.volatility < 2)
    })
}

fn count_parameters(
    expression: &Expr<'_>,
    uses: &mut [usize],
    depth: usize,
) -> Result<(), SqlError> {
    if depth >= 512 {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "SQL inlining expression nesting exceeds 512 levels"
        ));
    }
    if let Expr::Param(index)
    | Expr::RoutineParam {
        index,
        source: ast::ParameterSource::SqlRoutine | ast::ParameterSource::SqlRoutineField,
        ..
    } = expression
    {
        let slot = index
            .checked_sub(1)
            .and_then(|index| uses.get_mut(index as usize))
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "SQL function parameter is outside its declared signature"
                )
            })?;
        *slot = slot.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "SQL function parameter use count exhausted"
            )
        })?;
        return Ok(());
    }
    crate::sql::query::walk_children(expression, &mut |child| {
        count_parameters(child, uses, depth + 1)
    })
}

fn procedure_properties(oid: i32, storage: &Storage, txid: u32) -> Properties {
    if let Some(slot) = storage.routine_slot_by_oid(oid, txid) {
        let routine = storage.routine_for(slot, txid);
        return Properties {
            volatility: routine.attributes.volatility.code(),
            strict: routine.attributes.strict,
            cost: routine
                .attributes
                .cost_bits
                .map(f64::from_bits)
                .unwrap_or(100.0),
            forbidden: !matches!(routine.kind, RoutineKind::Function { .. }),
            ..Properties::EMPTY
        };
    }
    let Some(routine) = catalog::builtin_planner_procedure(oid) else {
        return Properties::UNKNOWN;
    };
    Properties {
        volatility: routine.volatility,
        strict: routine.strict,
        cost: routine.cost,
        forbidden: routine.forbidden,
        ..Properties::EMPTY
    }
}

fn cast_properties(
    storage: &Storage,
    txid: u32,
    source: i32,
    target: i32,
    binding: CastBinding,
) -> Result<Properties, SqlError> {
    if source == target {
        return Ok(Properties::EMPTY);
    }
    let source = storage
        .domain_base_type_for_oid(source, txid)
        .map_or(source, ColType::oid);
    let target = storage
        .domain_base_type_for_oid(target, txid)
        .map_or(target, ColType::oid);
    if let (Some(ColType::Array(left)), Some(ColType::Array(right))) = (
        storage
            .routine_result_for_oid(source, txid)
            .map(|result| result.ctype),
        storage
            .routine_result_for_oid(target, txid)
            .map(|result| result.ctype),
    ) {
        let mut properties = cast_properties(
            storage,
            txid,
            left.element_oid(),
            right.element_oid(),
            binding,
        )?;
        properties.strict = true;
        return Ok(properties);
    }
    if binding == CastBinding::Resolve
        && let Some((_, cast)) = catalog::defined_cast_path(storage, txid, source, target)
    {
        return Ok(match cast.method {
            crate::storage::CastMethod::Function(oid) => procedure_properties(oid, storage, txid),
            crate::storage::CastMethod::Binary => Properties::EMPTY,
            crate::storage::CastMethod::InOut => {
                let mut properties = Properties::EMPTY;
                if catalog::builtin_inout_cast_procedures(source, target, |oid| {
                    properties.include(procedure_properties(oid, storage, txid))
                })
                .is_none()
                {
                    properties = Properties::UNKNOWN;
                }
                properties.strict = true;
                properties
            }
        });
    }
    Ok(builtin_cast_properties(storage, txid, source, target))
}

fn builtin_cast_properties(storage: &Storage, txid: u32, source: i32, target: i32) -> Properties {
    let mut properties = Properties::EMPTY;
    let mut procedures = 0;
    if catalog::builtin_cast_procedures(source, target, |oid| {
        procedures += 1;
        properties.include(procedure_properties(oid, storage, txid));
    })
    .is_none()
    {
        return Properties::UNKNOWN;
    }
    // CoerceViaIO is strict even when a participating I/O procedure is not.
    if procedures == 2 {
        properties.strict = true;
    }
    properties
}

fn operator_properties(
    storage: &Storage,
    txid: u32,
    schema: Option<&str>,
    name: &str,
    left: Option<i32>,
    right: Option<i32>,
) -> Result<Properties, SqlError> {
    if schema != Some("pg_catalog")
        && let Some(slot) = storage.operator_slot_for_oids(schema, name, left, right, txid)?
    {
        let operator = storage.operator_for(slot, txid);
        let Some(oid) = operator.implementation.routine() else {
            return Ok(Properties::UNKNOWN);
        };
        let mut properties = procedure_properties(oid, storage, txid);
        for (actual, expected) in [
            (left, operator.signature.left),
            (right, operator.signature.right),
        ] {
            if let (Some(actual), Some(expected)) = (actual, expected) {
                let expected = storage
                    .routine_type_oid(expected.ctype, expected.user_type, txid)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "operator input type is unavailable"
                        )
                    })?;
                properties.include(cast_properties(
                    storage,
                    txid,
                    actual,
                    expected,
                    CastBinding::Resolve,
                )?);
            }
        }
        return Ok(properties);
    }
    if schema.is_some_and(|schema| schema != "pg_catalog") {
        return Ok(Properties::UNKNOWN);
    }
    let Some(oid) = catalog::builtin_operator_procedure(name, left, right) else {
        return Ok(Properties::UNKNOWN);
    };
    let mut properties = procedure_properties(oid, storage, txid);
    let Some((expected_left, expected_right)) =
        storage.builtin_operator_input_oids(name, left, right, txid)
    else {
        return Ok(Properties::UNKNOWN);
    };
    for (actual, expected) in [(left, expected_left), (right, expected_right)] {
        if let (Some(actual), Some(expected)) = (actual, expected) {
            properties.include(cast_properties(
                storage,
                txid,
                actual,
                expected,
                CastBinding::Resolve,
            )?);
        }
    }
    Ok(properties)
}

fn expression_properties(
    expression: &Expr<'_>,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    depth: usize,
) -> Result<Properties, SqlError> {
    if depth >= 512 {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "SQL inlining expression nesting exceeds 512 levels"
        ));
    }
    let mut properties = Properties::EMPTY;
    crate::sql::query::walk_children(expression, &mut |child| {
        properties.include(expression_properties(
            child,
            columns,
            storage,
            txid,
            arena,
            depth + 1,
        )?);
        Ok(())
    })?;
    let type_of = |expression: &Expr<'_>| infer_routine_argument_oid(expression, columns);
    let own = match expression {
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::NumericLit(_)
        | Expr::Str(_)
        | Expr::BitLit(_)
        | Expr::BoundLiteral { .. } => Properties::EMPTY,
        Expr::Param(_) | Expr::RoutineParam { .. }
            if parameter_index(expression, columns).is_some_and(super::parameter_known) =>
        {
            Properties::EMPTY
        }
        Expr::Column { .. }
        | Expr::SchemaColumn { .. }
        | Expr::WholeRow(_)
        | Expr::OuterAggregate { .. }
        | Expr::RoutineParam { .. }
        | Expr::Param(_) => Properties {
            constant: false,
            ..Properties::EMPTY
        },
        Expr::RecursiveState { .. } | Expr::DefaultMarker => Properties {
            context_dependent: true,
            constant: false,
            ..Properties::EMPTY
        },
        Expr::Subquery(_)
        | Expr::RowSubquery { .. }
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. }
        | Expr::Exists(_)
        | Expr::ArraySubquery(_) => Properties {
            subplan: true,
            constant: false,
            ..Properties::EMPTY
        },
        Expr::Unary {
            operator: UnaryOp::Not,
            ..
        }
        | Expr::Collate { .. }
        | Expr::BoundDefault { .. }
        | Expr::Field { .. }
        | Expr::RecordFieldIndex { .. }
        | Expr::Subscript { .. }
        | Expr::Slice { .. } => Properties::EMPTY,
        Expr::Unary {
            operator, operand, ..
        } => operator_properties(
            storage,
            txid,
            None,
            match operator {
                UnaryOp::Neg => "-",
                UnaryOp::BitNot => "~",
                UnaryOp::SquareRoot => "|/",
                UnaryOp::CubeRoot => "||/",
                UnaryOp::AbsoluteValue => "@",
                UnaryOp::TextSearchNot => "!!",
                UnaryOp::Not => unreachable!(),
            },
            None,
            Some(type_of(operand)?),
        )?,
        Expr::Binary {
            operator: BinaryOp::And | BinaryOp::Or,
            ..
        }
        | Expr::IsNull { .. }
        | Expr::Case { .. }
        | Expr::Array(_) => Properties {
            strict: false,
            ..Properties::EMPTY
        },
        Expr::Binary {
            operator,
            left,
            right,
            ..
        } => match operator.operator_name() {
            Some(name) => operator_properties(
                storage,
                txid,
                None,
                name,
                Some(type_of(left)?),
                Some(type_of(right)?),
            )?,
            None => Properties::UNKNOWN,
        },
        Expr::Cast {
            operand, binding, ..
        } => cast_properties(
            storage,
            txid,
            cast_source_oid(operand, type_of(expression)?, columns, storage, txid)?,
            type_of(expression)?,
            *binding,
        )?,
        Expr::Like {
            operand,
            pattern,
            case_insensitive,
            ..
        } => operator_properties(
            storage,
            txid,
            Some("pg_catalog"),
            if *case_insensitive { "~~*" } else { "~~" },
            Some(type_of(operand)?),
            Some(type_of(pattern)?),
        )?,
        Expr::Match {
            operand,
            pattern,
            case_insensitive,
            ..
        } => operator_properties(
            storage,
            txid,
            Some("pg_catalog"),
            if *case_insensitive { "~*" } else { "~" },
            Some(type_of(operand)?),
            Some(type_of(pattern)?),
        )?,
        Expr::InList { operand, list, .. } => {
            let mut comparisons = Properties::EMPTY;
            for item in *list {
                comparisons.include(operator_properties(
                    storage,
                    txid,
                    None,
                    "=",
                    Some(type_of(operand)?),
                    Some(type_of(item)?),
                )?);
            }
            comparisons.cost *= 0.5;
            comparisons.strict &= !list.is_empty();
            comparisons
        }
        Expr::AnyAll {
            operand,
            operator,
            array,
            ..
        } => {
            let Some(ColType::Array(element)) = storage
                .routine_result_for_oid(type_of(array)?, txid)
                .map(|result| result.ctype)
            else {
                return Ok(Properties::UNKNOWN);
            };
            let Some(name) = operator.operator_name() else {
                return Ok(Properties::UNKNOWN);
            };
            let mut comparison = operator_properties(
                storage,
                txid,
                None,
                name,
                Some(type_of(operand)?),
                Some(element.element_oid()),
            )?;
            comparison.strict &= matches!(array, Expr::Array(items) if !items.is_empty());
            comparison
        }
        Expr::Call {
            kind,
            name,
            bound_oid,
            args,
            argument_names,
            variadic,
            over,
            ..
        } => {
            let mut oids = [0; crate::storage::MAX_ROUTINE_ARGUMENTS];
            if args.len() > oids.len() {
                return Ok(Properties::UNKNOWN);
            }
            for (output, argument) in oids.iter_mut().zip(*args) {
                *output = type_of(argument)?;
            }
            let oids = &oids[..args.len()];
            if let Some((schema, name)) = (*kind == CallKind::SqlSyntax)
                .then(|| ast::catalog_operator_call(name))
                .flatten()
            {
                match oids {
                    [right] => {
                        operator_properties(storage, txid, schema, name, None, Some(*right))?
                    }
                    [left, right] => {
                        operator_properties(storage, txid, schema, name, Some(*left), Some(*right))?
                    }
                    _ => Properties::UNKNOWN,
                }
            } else if *kind == CallKind::SqlSyntax {
                syntax_properties(expression, name, args, columns, storage, txid)?
            } else {
                let selected = bound_oid.map_or_else(
                    || {
                        storage.select_routine_call(
                            name,
                            argument_names,
                            oids,
                            *variadic,
                            txid,
                            crate::storage::RoutineCallKind::Function,
                        )
                    },
                    |oid| {
                        storage.routine_slot_by_oid(oid, txid).map_or(
                            crate::storage::RoutineCallSelection::Builtin {
                                oid,
                                schema: "pg_catalog",
                            },
                            crate::storage::RoutineCallSelection::Stored,
                        )
                    },
                );
                let mut selected_properties = match selected {
                    crate::storage::RoutineCallSelection::Stored(slot) => {
                        let oid = crate::storage::routine_oid(&storage.routine_for(slot, txid));
                        let mut result = procedure_properties(oid, storage, txid);
                        if let Some(routine) = storage.function_for_captured_call(
                            oid,
                            argument_names,
                            oids,
                            *variadic,
                            txid,
                        ) {
                            let Some(mapping) =
                                routine.call_input_mapping(argument_names, args.len(), *variadic)
                            else {
                                return Ok(Properties::UNKNOWN);
                            };
                            for (actual, input) in oids.iter().zip(mapping) {
                                let input = usize::from(input);
                                let argument = routine.arguments()[input];
                                let expanded = !*variadic && routine.parameter_for_input(input).is_some_and(|parameter| matches!(parameter.mode, crate::storage::RoutineParameterMode::Variadic { .. }));
                                let target = if expanded {
                                    let ColType::Array(element) = argument.ctype else {
                                        return Ok(Properties::UNKNOWN);
                                    };
                                    element.element_oid()
                                } else {
                                    let Some(target) = storage.routine_type_oid(
                                        argument.ctype,
                                        argument.user_type,
                                        txid,
                                    ) else {
                                        return Ok(Properties::UNKNOWN);
                                    };
                                    target
                                };
                                result.include(cast_properties(
                                    storage,
                                    txid,
                                    *actual,
                                    target,
                                    CastBinding::Resolve,
                                )?);
                            }
                            for input in 0..routine.argument_count {
                                if mapping[..args.len()]
                                    .iter()
                                    .any(|mapped| usize::from(*mapped) == input)
                                {
                                    continue;
                                }
                                let Some((parameter_index, parameter)) = routine
                                    .parameters()
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, parameter)| parameter.mode.is_input())
                                    .nth(input)
                                else {
                                    return Ok(Properties::UNKNOWN);
                                };
                                let Some(source) = parameter.mode.default_sql() else {
                                    return Ok(Properties::UNKNOWN);
                                };
                                let dependencies =
                                    storage.snapshot_routine_dependencies_for(slot, txid, arena)?;
                                let default = crate::sql::query::bind_routine_parameter_default(
                                    source,
                                    parameter_index,
                                    storage,
                                    txid,
                                    dependencies.view(),
                                    arena,
                                )?;
                                let argument = routine.arguments()[input];
                                let Some(target) = storage.routine_type_oid(
                                    argument.ctype,
                                    argument.user_type,
                                    txid,
                                ) else {
                                    return Ok(Properties::UNKNOWN);
                                };
                                result.include(expression_properties(
                                    default,
                                    columns,
                                    storage,
                                    txid,
                                    arena,
                                    depth + 1,
                                )?);
                                result.include(cast_properties(
                                    storage,
                                    txid,
                                    type_of(default)?,
                                    target,
                                    CastBinding::Resolve,
                                )?);
                            }
                        } else {
                            result.known = false;
                        }
                        result
                    }
                    crate::storage::RoutineCallSelection::Builtin { oid, .. } => {
                        let mut result = procedure_properties(oid, storage, txid);
                        if let Some(expected) = storage.builtin_call_input_oids(
                            oid,
                            argument_names,
                            oids,
                            *variadic,
                            txid,
                        ) {
                            for (source, target) in oids.iter().zip(expected) {
                                result.include(cast_properties(
                                    storage,
                                    txid,
                                    *source,
                                    target,
                                    CastBinding::Resolve,
                                )?);
                            }
                        } else {
                            result.known = false;
                        }
                        if let Some(declaration) = catalog::builtin_callable_by_oid(oid) {
                            let Some(mapping) = Storage::builtin_call_mapping(
                                oid,
                                argument_names,
                                args.len(),
                                *variadic,
                            ) else {
                                return Ok(Properties::UNKNOWN);
                            };
                            if let Some(defaults) = declaration.defaults {
                                let defaults =
                                    crate::sql::parser::parse_expression_list(defaults, arena)?;
                                for (offset, default) in defaults.iter().enumerate() {
                                    let input = declaration.required_arguments + offset;
                                    if mapping[..args.len()]
                                        .iter()
                                        .any(|mapped| usize::from(*mapped) == input)
                                    {
                                        continue;
                                    }
                                    result.include(expression_properties(
                                        default,
                                        columns,
                                        storage,
                                        txid,
                                        arena,
                                        depth + 1,
                                    )?);
                                    result.include(cast_properties(
                                        storage,
                                        txid,
                                        type_of(default)?,
                                        declaration.arguments[input],
                                        CastBinding::Resolve,
                                    )?);
                                }
                            }
                        }
                        result
                    }
                    _ => Properties::UNKNOWN,
                };
                selected_properties.forbidden |= over.is_some();
                selected_properties
            }
        }
    };
    properties.include(own);
    Ok(properties.folded())
}

fn syntax_properties(
    expression: &Expr<'_>,
    name: &str,
    args: &[&Expr<'_>],
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
) -> Result<Properties, SqlError> {
    if matches!(
        name,
        "current_date"
            | "current_timestamp"
            | "current_time"
            | "localtimestamp"
            | "localtime"
            | "current_user"
            | "current_role"
            | "session_user"
            | "user"
            | "current_catalog"
            | "current_schema"
    ) {
        return Ok(Properties {
            volatility: 1,
            ..Properties::EMPTY
        });
    }
    if expression.is_aggregate() {
        return Ok(Properties {
            forbidden: true,
            ..Properties::EMPTY
        });
    }
    let mut result = Properties {
        strict: false,
        ..Properties::EMPTY
    };
    if matches!(name, "coalesce" | "greatest" | "least") {
        let target = infer_routine_argument_oid(expression, columns)?;
        for argument in args {
            result.include(cast_properties(
                storage,
                txid,
                infer_routine_argument_oid(argument, columns)?,
                target,
                CastBinding::Resolve,
            )?);
        }
        if name != "coalesce" {
            result.include(operator_properties(
                storage,
                txid,
                Some("pg_catalog"),
                if name == "greatest" { ">" } else { "<" },
                Some(target),
                Some(target),
            )?);
        }
    } else if name == "nullif" {
        if let [left, right] = args {
            result.include(operator_properties(
                storage,
                txid,
                None,
                "=",
                Some(infer_routine_argument_oid(left, columns)?),
                Some(infer_routine_argument_oid(right, columns)?),
            )?);
        } else {
            result.known = false;
        }
    } else if name == "grouping" || name == "merge_action" {
        result.forbidden = true;
    } else if !crate::sql::eval::sql_syntax_call_name(name) {
        result.known = false;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;

    #[test]
    fn detached_dml_input_facts_survive_schema_front_rewind_and_reuse() {
        let mut budget = Budget::new(8192);
        let arena = Arena::new(&mut budget, "detached planner input facts", 8192).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut candidates = crate::mem::arena::ArenaList::new_persistent(&arena);
            let mark = arena.mark();
            let constants = [
                literal_argument(crate::sql::types::Datum::Int4(17), oid::INT4, &arena).unwrap(),
                literal_argument(crate::sql::types::Datum::Null, oid::TEXT, &arena).unwrap(),
            ];
            let arguments = arena
                .alloc_slice_with(2, |at| ArgumentFacts {
                    type_oid: if at == 0 { oid::INT4 } else { oid::TEXT },
                    known_null: at == 1,
                    constant: constants[at].constant,
                    properties: Properties {
                        cost: at as f64 + 3.0,
                        ..Properties::EMPTY
                    },
                })
                .unwrap();
            candidates
                .push(
                    super::super::Candidate::detach(
                        42,
                        7,
                        crate::sql::query::InvocationSite::Expression(19),
                        ast::Collation::None,
                        arguments,
                        &arena,
                    )
                    .unwrap(),
                )
                .unwrap();
            let captured = super::super::CapturedPlannerInputs {
                candidates: candidates.as_slice(),
            };
            // DML schema visitors release their front scratch after callbacks.
            // The captured graph keeps persistent facts and checked handles
            // to their encoded constants, independently of schema scratch.
            unsafe { arena.rewind_to(mark) };
            let _replacement = arena.alloc_slice_with(4096, |_| 0xa5u8).unwrap();
            let [candidate] = captured.candidates else {
                panic!("one call");
            };
            assert_eq!(
                (candidate.oid, candidate.version, candidate.site),
                (42, 7, crate::sql::query::InvocationSite::Expression(19))
            );
            assert_eq!(candidate.arguments[0].type_oid, oid::INT4);
            assert_eq!(candidate.arguments[0].properties.cost, 3.0);
            assert!(!candidate.arguments[0].known_null);
            assert_eq!(candidate.arguments[1].type_oid, oid::TEXT);
            assert_eq!(candidate.arguments[1].properties.cost, 4.0);
            assert!(candidate.arguments[1].known_null);
            assert_eq!(
                candidate.arguments[0].constant(&arena).unwrap(),
                Some(crate::sql::types::Datum::Int4(17))
            );
            assert_eq!(
                candidate.arguments[1].constant(&arena).unwrap(),
                Some(crate::sql::types::Datum::Null)
            );
            let borrowed = arena
                .retained_slice_mut(candidate.arguments[0].constant.unwrap())
                .unwrap();
            assert_eq!(
                candidate.arguments[0]
                    .constant(&arena)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE
            );
            drop(borrowed);
            assert_eq!(
                candidate.arguments[0].constant(&arena).unwrap(),
                Some(crate::sql::types::Datum::Int4(17))
            );
        });
    }

    #[test]
    fn scalar_inline_shape_and_parameter_usage_match_analyzed_select_boundaries() {
        let mut budget = Budget::new(16 * 1024);
        let mut arena = Arena::new(&mut budget, "SQL inline analysis", 16 * 1024).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for (sql, accepted) in [
                ("SELECT $1 + $1", true),
                ("SELECT $1 WHERE true", false),
                ("SELECT $1 ORDER BY 1", false),
                ("SELECT $1 LIMIT 1", false),
                ("SELECT DISTINCT $1", false),
                ("SELECT $1 FROM source", false),
                ("SELECT $1, $1", false),
                ("WITH source AS (SELECT 1) SELECT $1", false),
                ("SELECT $1 UNION ALL SELECT $1", false),
            ] {
                arena.reset();
                let select = crate::sql::parser::parse_query(sql, &arena).unwrap();
                let statements = [Stmt::Select(*select)];
                let expression = simple_expression(&statements);
                assert_eq!(expression.is_some(), accepted, "{sql}");
                if let Some(expression) = expression {
                    let mut uses = [0];
                    count_parameters(expression, &mut uses, 0).unwrap();
                    assert_eq!(uses, [2]);
                }
            }
            let mut uses = [0];
            let error = count_parameters(&Expr::Param(2), &mut uses, 0).unwrap_err();
            assert_eq!(error.sqlstate.as_str(), sqlstate::DATA_EXCEPTION);
            let packed = array_argument_facts(
                &[ArgumentFacts {
                    type_oid: oid::INT4,
                    known_null: true,
                    constant: None,
                    properties: Properties {
                        volatility: 1,
                        cost: 7.0,
                        constant: false,
                        ..Properties::EMPTY
                    },
                }],
                crate::sql::types::ArrElem::Int4.array_oid(),
                crate::sql::types::ArrElem::Int4,
                &arena,
            )
            .unwrap();
            assert!(!packed.known_null);
            assert!(!packed.properties.strict);
            assert_eq!(packed.properties.volatility, 1);
            assert_eq!(packed.properties.cost, 7.0);
        });
    }

    #[test]
    fn scalar_inline_declaration_rejects_opaque_or_unavailable_function_contracts() {
        let mut routine = RoutineDef::EMPTY;
        assert!(declaration(&routine, 0, false, true));
        assert!(!declaration(&routine, 1, false, true));
        assert!(!declaration(&routine, 0, true, true));
        assert!(!declaration(&routine, 0, false, false));
        routine.attributes.security_definer = true;
        assert!(!declaration(&routine, 0, false, true));
        routine.attributes.security_definer = false;
        routine.config_count = 1;
        assert!(!declaration(&routine, 0, false, true));
        routine.config_count = 0;
        routine.language = RoutineLanguage::PlPgSql;
        assert!(!declaration(&routine, 0, false, true));
        routine.language = RoutineLanguage::Sql;
        routine.kind = RoutineKind::Function {
            result: crate::storage::RoutineResult {
                ctype: ColType::Record,
                user_type: None,
            },
        };
        assert!(!declaration(&routine, 0, false, true));
    }

    #[test]
    fn scalar_inline_post_analysis_preserves_strictness_volatility_and_duplication_cost() {
        let argument = ArgumentFacts {
            type_oid: oid::INT4,
            known_null: false,
            constant: None,
            properties: Properties {
                constant: false,
                ..Properties::EMPTY
            },
        };
        crate::mem::guard::forbid_alloc(|| {
            let array = ArgumentFacts {
                type_oid: crate::sql::types::ArrElem::Int4.array_oid(),
                known_null: true,
                ..argument
            };
            let element = cast_source_argument(array, oid::INT4, true);
            assert!(!element.known_null);
            assert!(element.properties.context_dependent);
            assert!(!post_analysis(
                RoutineVolatility::Volatile,
                false,
                Properties::EMPTY,
                &[1],
                &[element]
            ));
            let scalar = cast_source_argument(argument, oid::INT4, false);
            assert!(post_analysis(
                RoutineVolatility::Volatile,
                false,
                Properties::EMPTY,
                &[1],
                &[scalar]
            ));
            assert!(post_analysis(
                RoutineVolatility::Volatile,
                false,
                Properties::EMPTY,
                &[0],
                &[argument]
            ));
            assert!(!post_analysis(
                RoutineVolatility::Volatile,
                true,
                Properties::EMPTY,
                &[0],
                &[argument]
            ));
            assert!(!post_analysis(
                RoutineVolatility::Volatile,
                true,
                Properties {
                    strict: false,
                    ..Properties::EMPTY
                },
                &[1],
                &[argument]
            ));
            assert!(!post_analysis(
                RoutineVolatility::Immutable,
                false,
                Properties {
                    volatility: 1,
                    ..Properties::EMPTY
                },
                &[1],
                &[argument]
            ));
            assert!(post_analysis(
                RoutineVolatility::Stable,
                false,
                Properties {
                    volatility: 1,
                    ..Properties::EMPTY
                },
                &[1],
                &[argument]
            ));
            for (cost, accepted) in [(10.0, true), (10.5, false)] {
                let argument = ArgumentFacts {
                    properties: Properties {
                        cost,
                        ..argument.properties
                    },
                    ..argument
                };
                assert_eq!(
                    post_analysis(
                        RoutineVolatility::Volatile,
                        false,
                        Properties::EMPTY,
                        &[2],
                        &[argument]
                    ),
                    accepted
                );
            }
            for properties in [
                Properties {
                    volatility: 2,
                    ..argument.properties
                },
                Properties {
                    subplan: true,
                    ..argument.properties
                },
            ] {
                let argument = ArgumentFacts {
                    properties,
                    ..argument
                };
                assert!(post_analysis(
                    RoutineVolatility::Volatile,
                    false,
                    Properties::EMPTY,
                    &[1],
                    &[argument]
                ));
                assert!(!post_analysis(
                    RoutineVolatility::Volatile,
                    false,
                    Properties::EMPTY,
                    &[2],
                    &[argument]
                ));
            }
        });
    }
}
