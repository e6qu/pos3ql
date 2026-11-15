//! postgres_fdw's remote query text (`deparse.c`) for a scan of one foreign
//! table: the columns it fetches, the conditions it ships and the order and
//! limit it pushes, as PostgreSQL 18.6's `deparseSelectStmtForRel` writes
//! them.
//!
//! A condition ships when `is_foreign_expr` would accept it: built-in
//! operators and constants over the table's own columns of built-in types,
//! whose collations come from those columns. This module models the shapes
//! listed in [`Deparser::shape`]; any other condition stays local and is
//! evaluated after the rows arrive, so results never depend on what ships.
//! Source: <https://github.com/postgres/postgres/blob/REL_18_6/contrib/postgres_fdw/deparse.c>
//! (`deparseConst`, `deparseOpExpr`, `deparseScalarArrayOpExpr`,
//! `appendOrderByClause`, `appendLimitClause`, `deparseLockingClause`).

use core::fmt::Write as _;

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::ast::{
    BinaryOp, Expr, LockStrength, OrderBy, RelationInheritance, Select, SelectItem, TableRef,
    UnaryOp, WindowSpec,
};
use crate::sql::eval::{SqlError, arena_full, sqlstate};
use crate::sql::types::{ColType, Datum, oid};
use crate::sql_err;
use crate::storage::foreign::ForeignTableDefinition;
use crate::storage::{ColumnSet, Storage, TableDef};
use crate::util::StackStr;

/// The longest remote query text a scan sends.
pub(crate) const REMOTE_QUERY_BYTES: usize = 16_384;

pub(crate) type RemoteText = StackStr<REMOTE_QUERY_BYTES>;

/// Why a subexpression does not ship.
#[expect(
    clippy::large_enum_variant,
    reason = "an error ends the walk once; boxing would allocate at run time"
)]
enum Stop {
    /// Outside the shapes postgres_fdw ships, or outside this module's model.
    Local,
    Error(SqlError),
}

impl From<crate::mem::arena::ArenaFull> for Stop {
    fn from(_: crate::mem::arena::ArenaFull) -> Self {
        Self::Error(arena_full())
    }
}

impl From<SqlError> for Stop {
    fn from(error: SqlError) -> Self {
        Self::Error(error)
    }
}

type Shipped<T> = Result<T, Stop>;

/// A constant before it is coerced to the type its consumer gives it.
#[derive(Clone, Copy)]
enum Literal<'e> {
    /// A quoted literal of unknown type.
    Unknown(&'e str),
    Null,
    Bool(bool),
    /// An integer literal's value.
    Integer(i64),
    /// A decimal literal's text and whether it is negated.
    Decimal(&'e str, bool),
    /// A quoted literal cast to a built-in type.
    Typed(&'e str, i32),
    /// A constant computed by the plan, of its type: a custom plan's
    /// statement parameter (`PARAM_FLAG_CONST`), an expression the planner
    /// folded over one, or a number cast to a type.
    Bound(&'e Expr<'e>, i32),
}

/// What a shippable subexpression is, before it is written.
#[derive(Clone, Copy)]
enum Shape<'e> {
    Literal(Literal<'e>),
    /// A value of type `type_oid`; `plain` when it is a bare column, the
    /// operand whose constant partner postgres_fdw leaves unlabelled.
    Value {
        type_oid: i32,
        plain: bool,
    },
}

/// A constant coerced to its type, as `deparseConst` prints it.
struct Constant<'e> {
    literal: Literal<'e>,
    type_oid: i32,
    /// `deparseConst`'s `showtype`.
    show: i8,
}

fn integer_rank(type_oid: i32) -> Option<u8> {
    match type_oid {
        oid::INT2 => Some(1),
        oid::INT4 => Some(2),
        oid::INT8 => Some(3),
        _ => None,
    }
}

/// PostgreSQL's numeric category promotion between the types modeled here.
fn numeric_rank(type_oid: i32) -> Option<u8> {
    match type_oid {
        oid::INT2 | oid::INT4 | oid::INT8 => integer_rank(type_oid),
        oid::NUMERIC => Some(4),
        oid::FLOAT8 => Some(5),
        _ => None,
    }
}

fn wider_numeric(left: i32, right: i32) -> i32 {
    match (numeric_rank(left), numeric_rank(right)) {
        (Some(left_rank), Some(right_rank)) if right_rank > left_rank => right,
        _ => left,
    }
}

/// The type an operator sees a value as: `varchar` has no operators of its
/// own and is relabelled `text`.
fn operator_type(type_oid: i32) -> i32 {
    if type_oid == oid::VARCHAR {
        oid::TEXT
    } else {
        type_oid
    }
}

/// The built-in column types whose operators this module ships.
fn modeled_type(ctype: ColType) -> bool {
    matches!(
        ctype,
        ColType::Bool
            | ColType::Int2
            | ColType::Int4
            | ColType::Int8
            | ColType::Float4
            | ColType::Float8
            | ColType::Numeric
            | ColType::Text
            | ColType::Varchar
            | ColType::Bpchar
            | ColType::Date
            | ColType::Timestamp
            | ColType::Timestamptz
            | ColType::Time
            | ColType::Interval
            | ColType::Uuid
    )
}

/// `deparseStringLiteral`.
fn write_string_literal(output: &mut impl core::fmt::Write, value: &str) {
    if value.contains('\\') {
        let _ = output.write_char('E');
    }
    let _ = output.write_char('\'');
    for character in value.chars() {
        if character == '\'' || character == '\\' {
            let _ = output.write_char(character);
        }
        let _ = output.write_char(character);
    }
    let _ = output.write_char('\'');
}

/// `deparse_type_name` of a built-in type without a modifier.
fn write_type_name(output: &mut impl core::fmt::Write, type_oid: i32) -> Shipped<()> {
    let name = crate::sql::eval::funcs::system::format_builtin_type(type_oid, -1, true)
        .ok_or(Stop::Local)?;
    let _ = output.write_str(name.as_str());
    Ok(())
}

/// `array_out`'s element quoting.
fn write_array_element(output: &mut impl core::fmt::Write, value: &str) {
    let quote = value.is_empty()
        || value.eq_ignore_ascii_case("NULL")
        || value.chars().any(|character| {
            matches!(character, '{' | '}' | ',' | '"' | '\\') || character.is_ascii_whitespace()
        });
    if !quote {
        let _ = output.write_str(value);
        return;
    }
    let _ = output.write_char('"');
    for character in value.chars() {
        if character == '"' || character == '\\' {
            let _ = output.write_char('\\');
        }
        let _ = output.write_char(character);
    }
    let _ = output.write_char('"');
}

/// A foreign table's remote names and column types.
pub(crate) struct Deparser<'t> {
    pub(crate) table: &'t TableDef,
    pub(crate) foreign: &'t ForeignTableDefinition,
    /// The name the query gives the table, which qualified columns use.
    pub(crate) exposed: &'t str,
    pub(crate) arena: &'t Arena,
    /// Resolves the built-in functions a condition calls.
    pub(crate) storage: &'t Storage,
    pub(crate) txid: u32,
    /// Writing the grouped relation's expressions, where the built-in
    /// aggregates ship (`foreign_grouping_ok`).
    pub(crate) grouped: bool,
    /// The parameters a scan's remote query reads (`params_list`), when
    /// parameters and InitPlan values may ship.
    pub(crate) params: Option<&'t RemoteParams>,
}

/// The most parameters one remote query sends.
pub(crate) const MAX_REMOTE_PARAMS: usize = 64;

/// What a remote query parameter carries: a statement parameter `$n`
/// (`PARAM_EXTERN`) or the value of an uncorrelated scalar subquery the
/// local plan runs as an InitPlan (`PARAM_EXEC`), identified by its node.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ParamKey {
    External(u32),
    InitPlan(usize),
}

/// postgres_fdw's `params_list`: the values a remote query reads as `$1`,
/// `$2`, ... in the order its text first names them, each sent once.
pub(crate) struct RemoteParams {
    entries: [core::cell::Cell<Option<(ParamKey, i32)>>; MAX_REMOTE_PARAMS],
    count: core::cell::Cell<usize>,
}

impl RemoteParams {
    pub(crate) const fn new() -> Self {
        Self {
            entries: [const { core::cell::Cell::new(None) }; MAX_REMOTE_PARAMS],
            count: core::cell::Cell::new(0),
        }
    }

    /// How many parameters the text written so far names.
    pub(crate) fn mark(&self) -> usize {
        self.count.get()
    }

    /// Forgets the parameters named after `mark`, whose text was discarded.
    pub(crate) fn reset(&self, mark: usize) {
        self.count.set(mark.min(self.count.get()));
    }

    /// The 1-based number of `key`, added when the text names it first.
    fn number(&self, key: ParamKey, type_oid: i32) -> Shipped<usize> {
        let count = self.count.get();
        if let Some(at) = self.entries[..count]
            .iter()
            .position(|entry| entry.get().is_some_and(|(held, _)| held == key))
        {
            return Ok(at + 1);
        }
        if count == MAX_REMOTE_PARAMS {
            return Err(Stop::Error(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "a foreign query reads more than {} parameters",
                MAX_REMOTE_PARAMS
            )));
        }
        self.entries[count].set(Some((key, type_oid)));
        self.count.set(count + 1);
        Ok(count + 1)
    }

    /// The nodes that compute each parameter, in number order, found among
    /// the expressions the remote query was written from.
    fn nodes<'a>(
        &self,
        roots: &[&'a Expr<'a>],
        arena: &'a Arena,
    ) -> Result<&'a [&'a Expr<'a>], SqlError> {
        let count = self.count.get();
        let nodes = arena
            .alloc_slice_with(count, |_| &Expr::Null)
            .map_err(|_| arena_full())?;
        for (at, entry) in self.entries[..count].iter().enumerate() {
            let (key, _) = entry.get().expect("counted entries are set");
            let mut found = None;
            for &root in roots {
                crate::sql::query::walk_expression_tree(root, &mut |node| {
                    let matches = match (key, node) {
                        (ParamKey::External(index), Expr::Param(held)) => index == *held,
                        (ParamKey::InitPlan(address), Expr::Subquery(_)) => {
                            core::ptr::from_ref(node) as usize == address
                        }
                        _ => false,
                    };
                    if matches && found.is_none() {
                        found = Some(node);
                    }
                })?;
            }
            nodes[at] = found.ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "a foreign query parameter has no source expression"
                )
            })?;
        }
        Ok(nodes)
    }
}

impl<'t> Deparser<'t> {
    /// `deparseRelation`: `schema.table`, from the table's options when it
    /// has them.
    pub(crate) fn write_relation(&self, output: &mut impl core::fmt::Write) {
        let schema = self
            .foreign
            .options
            .get("schema_name")
            .unwrap_or(self.table.schema.as_str());
        let name = self
            .foreign
            .options
            .get("table_name")
            .unwrap_or(self.table.name.as_str());
        super::quote_identifier(output, schema);
        let _ = output.write_char('.');
        super::quote_identifier(output, name);
    }

    pub(crate) fn write_column(&self, output: &mut impl core::fmt::Write, column: usize) {
        super::quote_remote_column(output, self.foreign, column, &self.table.columns()[column]);
    }

    /// `deparseTargetList`: the fetched columns in table order, or `NULL`.
    pub(crate) fn write_target_list(
        &self,
        output: &mut impl core::fmt::Write,
        columns: ColumnSet,
    ) -> bool {
        let mut first = true;
        for column in (0..self.table.n_columns).filter(|&column| columns.contains(column)) {
            if !first {
                let _ = output.write_str(", ");
            }
            first = false;
            self.write_column(output, column);
        }
        if first {
            let _ = output.write_str("NULL");
        }
        !first
    }

    /// The table column `expression` names, if it names one.
    pub(crate) fn column(&self, expression: &Expr<'_>) -> Option<usize> {
        let Expr::Column { qualifier, name } = expression else {
            return None;
        };
        if qualifier.is_some_and(|qualifier| qualifier != self.exposed) {
            return None;
        }
        self.table
            .columns()
            .iter()
            .position(|column| column.name.as_str() == *name)
    }

    fn column_type(&self, column: usize) -> Shipped<i32> {
        let definition = &self.table.columns()[column];
        if definition.user_type.is_some() || !modeled_type(definition.ctype) {
            return Err(Stop::Local);
        }
        Ok(definition.ctype.oid())
    }

    fn literal<'e>(expression: &'e Expr<'e>) -> Option<Literal<'e>> {
        Some(match unbound(expression) {
            Expr::Str(text) => Literal::Unknown(text),
            Expr::Null => Literal::Null,
            Expr::Bool(value) => Literal::Bool(*value),
            Expr::Int(_) => return None,
            Expr::NumericLit(text) => Literal::Decimal(text, false),
            Expr::Cast {
                operand: Expr::Str(text),
                type_name,
                type_mod: -1,
                ..
            } => {
                let ctype = ColType::from_sql_name(type_name)?;
                if !modeled_type(ctype) {
                    return None;
                }
                Literal::Typed(text, ctype.oid())
            }
            // A number cast to a type is a constant of it (`5::bigint`).
            cast @ Expr::Cast {
                operand:
                    Expr::Int(_)
                    | Expr::NumericLit(_)
                    | Expr::Unary {
                        operator: UnaryOp::Neg,
                        operand: Expr::Int(_) | Expr::NumericLit(_),
                        ..
                    },
                type_name,
                type_mod: -1,
                ..
            } => {
                let ctype = ColType::from_sql_name(type_name)?;
                if !modeled_type(ctype) {
                    return None;
                }
                Literal::Bound(cast, ctype.oid())
            }
            _ => return None,
        })
    }

    /// Classifies `expression`, or reports that it does not ship. Shapes:
    /// table columns; untyped, integer, decimal, boolean, `NULL` and cast
    /// string literals; comparison, arithmetic and `||` operators;
    /// `AND`/`OR`/`NOT`; `IS [NOT] NULL`; `[NOT] IN` over constants;
    /// `[NOT] [I]LIKE` without `ESCAPE`; and the POSIX regex operators.
    fn shape<'e>(&self, expression: &'e Expr<'e>) -> Shipped<Shape<'e>> {
        if let Some(shape) = self.param_shape(expression)? {
            return Ok(shape);
        }
        let expression = unbound(expression);
        if let Some(column) = self.column(expression) {
            return Ok(Shape::Value {
                type_oid: self.column_type(column)?,
                plain: true,
            });
        }
        if let Some(literal) = Self::literal(expression) {
            return Ok(Shape::Literal(literal));
        }
        let boolean = Shape::Value {
            type_oid: oid::BOOL,
            plain: false,
        };
        match expression {
            Expr::Int(_) => Ok(Shape::Literal(self.integer(expression).ok_or(Stop::Local)?)),
            Expr::Unary {
                operator: UnaryOp::Neg,
                operand: Expr::Int(_) | Expr::NumericLit(_),
                ..
            } => Ok(Shape::Literal(self.integer(expression).ok_or(Stop::Local)?)),
            Expr::Unary {
                operator: UnaryOp::Not,
                operand,
                ..
            } => {
                self.boolean_operand(operand)?;
                Ok(boolean)
            }
            Expr::Binary {
                operator: BinaryOp::And | BinaryOp::Or,
                left,
                right,
                ..
            } => {
                self.boolean_operand(left)?;
                self.boolean_operand(right)?;
                Ok(boolean)
            }
            Expr::Binary {
                operator,
                left,
                right,
                ..
            } => {
                if self.boolean_equality(*operator, left, right)?.is_some() {
                    return Ok(boolean);
                }
                let (_, _, type_oid) = self.binary(*operator, left, right)?;
                Ok(Shape::Value {
                    type_oid,
                    plain: false,
                })
            }
            Expr::Call { .. } if self.grouped && expression.is_aggregate() => Ok(Shape::Value {
                type_oid: self.aggregate(expression)?,
                plain: false,
            }),
            Expr::Call { .. } => Ok(Shape::Value {
                type_oid: self.call(expression)?.result_oid,
                plain: false,
            }),
            Expr::IsNull { operand, .. } => match self.shape(operand)? {
                Shape::Value { .. } => Ok(boolean),
                Shape::Literal(_) => Err(Stop::Local),
            },
            Expr::InList { operand, list, .. } => {
                self.in_list(operand, list)?;
                Ok(boolean)
            }
            Expr::Like {
                operand,
                pattern,
                escape: None,
                ..
            }
            | Expr::Match {
                operand, pattern, ..
            } => {
                self.pattern(operand, pattern)?;
                Ok(boolean)
            }
            _ => Err(Stop::Local),
        }
    }

    /// A statement parameter or InitPlan value the remote query reads as a
    /// parameter (`is_foreign_param`), and its type: one of a modeled type
    /// without a modifier, which `printRemoteParam` writes as `$n::type`.
    fn remote_param(&self, expression: &Expr<'_>) -> Shipped<Option<(ParamKey, i32)>> {
        if self.params.is_none() || self.grouped {
            return Ok(None);
        }
        let (key, type_oid) = match unbound(expression) {
            Expr::Param(index) => (
                ParamKey::External(*index),
                crate::sql::exec::bound_parameter_type_oid(*index).ok_or(Stop::Local)?,
            ),
            node @ Expr::Subquery(_) => {
                if crate::sql::query::subquery_node_correlated(
                    node,
                    self.storage,
                    self.txid,
                    self.arena,
                )? {
                    return Err(Stop::Local);
                }
                let type_oid = crate::sql::query::subquery_expression_type(
                    node,
                    self.storage,
                    self.txid,
                    self.arena,
                    &crate::sql::eval::NoColumns,
                )?
                .ok_or(Stop::Local)?;
                // An InitPlan's value has its subquery's type modifier,
                // which these types may carry.
                if matches!(
                    type_oid,
                    oid::NUMERIC
                        | oid::VARCHAR
                        | oid::BPCHAR
                        | oid::TIMESTAMP
                        | oid::TIMESTAMPTZ
                        | oid::TIME
                        | oid::INTERVAL
                ) {
                    return Err(Stop::Local);
                }
                (
                    ParamKey::InitPlan(core::ptr::from_ref(node) as usize),
                    type_oid,
                )
            }
            _ => return Ok(None),
        };
        match crate::sql::exec::coltype_of_oid(type_oid) {
            Some(ctype) if modeled_type(ctype) => Ok(Some((key, type_oid))),
            _ => Err(Stop::Local),
        }
    }

    /// A parameter's shape: a custom plan's bound value is a constant
    /// (`PARAM_FLAG_CONST`), any other remote parameter a value.
    fn param_shape<'e>(&self, expression: &'e Expr<'e>) -> Shipped<Option<Shape<'e>>> {
        if self.params.is_none() || self.grouped {
            return Ok(None);
        }
        let constant = match expression {
            Expr::Param(index) if crate::sql::eval::custom_plan_binds(*index) => Some((
                crate::sql::exec::bound_parameter_type_oid(*index).ok_or(Stop::Local)?,
                -1,
            )),
            Expr::BoundLiteral {
                value: crate::sql::ast::BoundLiteralValue::Folded(_),
                type_oid,
                type_mod,
                source,
                ..
            } if reads_custom_parameter(source)? => Some((*type_oid, *type_mod)),
            _ => None,
        };
        if let Some((type_oid, type_mod)) = constant {
            if type_mod >= 0
                || !crate::sql::exec::coltype_of_oid(type_oid).is_some_and(modeled_type)
            {
                return Err(Stop::Local);
            }
            return Ok(Some(Shape::Literal(
                match self.bound_value(expression)? {
                    Datum::Null => Literal::Null,
                    _ => Literal::Bound(expression, type_oid),
                },
            )));
        }
        Ok(self
            .remote_param(expression)?
            .map(|(_, type_oid)| Shape::Value {
                type_oid,
                plain: false,
            }))
    }

    /// The value of a constant the plan computes.
    fn bound_value<'v>(&'v self, node: &'v Expr<'v>) -> Shipped<Datum<'v>> {
        match node {
            Expr::Param(index) => {
                Ok(crate::sql::eval::custom_plan_parameter(*index).ok_or(Stop::Local)?)
            }
            node => Ok(crate::sql::eval::eval_full(
                node,
                self.arena,
                &[],
                &crate::sql::eval::NoColumns,
                &crate::sql::eval::NO_HOOKS,
            )?),
        }
    }

    /// Whether `expression` is a value the remote query reads as a
    /// parameter, which the planner treats as a constant.
    fn is_param(&self, expression: &Expr<'_>) -> bool {
        matches!(self.param_shape(expression), Ok(Some(_)))
    }

    /// Whether `expression` reads a column of the table.
    fn reads_column(&self, expression: &Expr<'_>) -> Result<bool, SqlError> {
        let mut reads = false;
        crate::sql::query::walk_expression_tree(expression, &mut |node| {
            reads |= self.column(node).is_some();
        })?;
        Ok(reads)
    }

    /// An integer or decimal literal, negated or not.
    fn integer<'e>(&self, expression: &'e Expr<'e>) -> Option<Literal<'e>> {
        let (negative, operand) = match unbound(expression) {
            Expr::Unary {
                operator: UnaryOp::Neg,
                operand,
                ..
            } => (true, *operand),
            other => (false, other),
        };
        match operand {
            Expr::Int(value) => Some(Literal::Integer(if negative {
                value.checked_neg()?
            } else {
                *value
            })),
            Expr::NumericLit(text) => Some(Literal::Decimal(text, negative)),
            _ => None,
        }
    }

    /// `simplify_boolean_equality`: `column = true` is the column and
    /// `column = false` its negation.
    fn boolean_equality<'e>(
        &self,
        operator: BinaryOp,
        left: &'e Expr<'e>,
        right: &'e Expr<'e>,
    ) -> Shipped<Option<(&'e Expr<'e>, bool)>> {
        if !matches!(operator, BinaryOp::Eq | BinaryOp::NotEq) {
            return Ok(None);
        }
        let (operand, value) = match (left, right) {
            (operand, Expr::Bool(value)) | (Expr::Bool(value), operand) => (operand, *value),
            _ => return Ok(None),
        };
        match self.column(operand) {
            Some(column) if self.column_type(column)? == oid::BOOL => {
                Ok(Some((operand, value != (operator == BinaryOp::Eq))))
            }
            _ => Ok(None),
        }
    }

    /// The type a literal has before a function argument coerces it.
    fn literal_type(literal: Literal<'_>) -> i32 {
        match literal {
            Literal::Unknown(_) | Literal::Null => oid::UNKNOWN,
            Literal::Bool(_) => oid::BOOL,
            Literal::Integer(value) => {
                if i32::try_from(value).is_ok() {
                    oid::INT4
                } else {
                    oid::INT8
                }
            }
            Literal::Decimal(..) => oid::NUMERIC,
            Literal::Typed(_, type_oid) | Literal::Bound(_, type_oid) => type_oid,
        }
    }

    /// A call of a built-in immutable function over modeled types, which
    /// `is_foreign_expr` ships.
    fn call(
        &self,
        expression: &Expr<'_>,
    ) -> Shipped<&'static crate::sql::catalog::BuiltinCallableSignature> {
        let Expr::Call {
            kind: crate::sql::ast::CallKind::Routine,
            name,
            args,
            argument_names,
            variadic: false,
            star: false,
            distinct: false,
            order_by: [],
            over: None,
            filter: None,
            ..
        } = expression
        else {
            return Err(Stop::Local);
        };
        if args.len() > crate::storage::MAX_ROUTINE_ARGUMENTS || !argument_names.is_empty() {
            return Err(Stop::Local);
        }
        let mut types = [oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
        for (target, argument) in types.iter_mut().zip(*args) {
            *target = match self.shape(argument)? {
                Shape::Value { type_oid, .. } => type_oid,
                Shape::Literal(literal) => Self::literal_type(literal),
            };
        }
        let crate::sql::eval::RoutineCallSelection::Builtin { oid: routine, .. } =
            self.storage.select_routine_call(
                name,
                &[],
                &types[..args.len()],
                false,
                self.txid,
                crate::storage::RoutineCallKind::Function,
            )
        else {
            return Err(Stop::Local);
        };
        let signature = crate::sql::catalog::builtin_callable_by_oid(routine).ok_or(Stop::Local)?;
        let modeled =
            |type_oid: i32| crate::sql::exec::coltype_of_oid(type_oid).is_some_and(modeled_type);
        if signature.kind != b'f'
            || signature.returns_set
            || signature.catalog.volatility != "i"
            || signature.arguments.len() != args.len()
            || !modeled(signature.result_oid)
            || !signature
                .arguments
                .iter()
                .all(|&parameter| modeled(parameter))
        {
            return Err(Stop::Local);
        }
        for (&argument, &parameter) in types[..args.len()].iter().zip(signature.arguments) {
            let argument = operator_type(argument);
            let parameter = operator_type(parameter);
            let coerced = argument == parameter
                || argument == oid::UNKNOWN
                || matches!(
                    (numeric_rank(argument), numeric_rank(parameter)),
                    (Some(from), Some(to)) if from < to
                );
            if !coerced {
                return Err(Stop::Local);
            }
        }
        Ok(signature)
    }

    /// `cost_qual_eval`'s cost of a shippable expression, in halves of
    /// `cpu_operator_cost`: each operator or function its procedure cost,
    /// each implicit cast of a value one, and an array comparison half its
    /// elements.
    fn cost(&self, expression: &Expr<'_>) -> Shipped<u32> {
        if self.column(expression).is_some()
            || Self::literal(expression).is_some()
            || self.is_param(expression)
        {
            return Ok(0);
        }
        let value_cast = |value: &Expr<'_>, target: i32| -> Shipped<u32> {
            Ok(match self.shape(value)? {
                Shape::Value { type_oid, .. }
                    if numeric_rank(type_oid).is_some()
                        && wider_numeric(type_oid, target) == target
                        && type_oid != target
                        && (integer_rank(type_oid).is_none() || integer_rank(target).is_none()) =>
                {
                    2
                }
                _ => 0,
            })
        };
        Ok(match expression {
            Expr::Int(_)
            | Expr::Unary {
                operator: UnaryOp::Neg,
                ..
            } => 0,
            Expr::Unary { operand, .. } | Expr::IsNull { operand, .. } => self.cost(operand)?,
            Expr::Binary {
                operator: BinaryOp::And | BinaryOp::Or,
                left,
                right,
                ..
            } => self.cost(left)? + self.cost(right)?,
            Expr::Binary {
                operator,
                left,
                right,
                ..
            } => match self.boolean_equality(*operator, left, right)? {
                Some(_) => 0,
                None => {
                    let (_, _, result) = self.binary(*operator, left, right)?;
                    let operand_type = match (self.shape(left)?, self.shape(right)?) {
                        (Shape::Value { type_oid: l, .. }, Shape::Value { type_oid: r, .. }) => {
                            wider_numeric(l, r)
                        }
                        (Shape::Value { type_oid, .. }, Shape::Literal(literal))
                        | (Shape::Literal(literal), Shape::Value { type_oid, .. }) => {
                            wider_numeric(type_oid, Self::constant_type(literal, type_oid)?)
                        }
                        _ => result,
                    };
                    2 + self.cost(left)?
                        + self.cost(right)?
                        + value_cast(left, operand_type)?
                        + value_cast(right, operand_type)?
                }
            },
            Expr::InList { operand, list, .. } => {
                let element = self.in_list(operand, list)?;
                let compared = if list.len() == 1 {
                    2
                } else {
                    list.len() as u32
                };
                compared + self.cost(operand)? + value_cast(operand, element)?
            }
            Expr::Like {
                operand, pattern, ..
            }
            | Expr::Match {
                operand, pattern, ..
            } => 2 + self.cost(operand)? + self.cost(pattern)?,
            // The aggregate node computes an aggregate; a qualification
            // reads it as a value.
            Expr::Call { .. } if self.grouped && expression.is_aggregate() => {
                self.aggregate(expression)?;
                0
            }
            Expr::Call { args, .. } => {
                let signature = self.call(expression)?;
                let mut total = (signature.catalog.cost * 2.0).round() as u32;
                for (argument, &parameter) in args.iter().zip(signature.arguments) {
                    total += self.cost(argument)? + value_cast(argument, parameter)?;
                }
                total
            }
            _ => return Err(Stop::Local),
        })
    }

    /// Whether a shipped condition is a mergejoinable equality the planner
    /// turns into an equivalence class, whose conditions it lists after the
    /// others; and the condition as it regenerates it, value before constant.
    fn equivalence<'e>(
        &self,
        condition: &'e Expr<'e>,
        arena: &'e Arena,
    ) -> Result<Option<&'e Expr<'e>>, SqlError> {
        let (left, right) = match condition {
            Expr::Binary {
                operator: BinaryOp::Eq,
                left,
                right,
                ..
            } => (*left, *right),
            Expr::InList {
                operand,
                list: [item],
                negated: false,
            } => (*operand, *item),
            _ => return Ok(None),
        };
        if matches!(
            self.boolean_equality(BinaryOp::Eq, left, right),
            Ok(Some(_))
        ) {
            return Ok(None);
        }
        Ok(match (self.shape(left), self.shape(right)) {
            (Ok(Shape::Literal(Literal::Null)), _) | (_, Ok(Shape::Literal(Literal::Null))) => None,
            // A parameter is the class's constant, which the regenerated
            // equality reads second.
            (Ok(Shape::Value { .. }), Ok(Shape::Value { .. }))
                if self.is_param(left) && !self.is_param(right) =>
            {
                Some(
                    arena
                        .alloc(Expr::Binary {
                            source_position: condition.source_position(),
                            operator: BinaryOp::Eq,
                            left: right,
                            right: left,
                        })
                        .map_err(|_| arena_full())?,
                )
            }
            (Ok(Shape::Value { .. }), Ok(Shape::Value { .. }))
            | (Ok(Shape::Value { .. }), Ok(Shape::Literal(_))) => Some(condition),
            (Ok(Shape::Literal(_)), Ok(Shape::Value { .. })) => Some(
                arena
                    .alloc(Expr::Binary {
                        source_position: condition.source_position(),
                        operator: BinaryOp::Eq,
                        left: right,
                        right: left,
                    })
                    .map_err(|_| arena_full())?,
            ),
            _ => None,
        })
    }

    /// A built-in aggregate `is_foreign_expr` ships, and its result type:
    /// shippable arguments, `DISTINCT`, `ORDER BY` keys and `FILTER`.
    fn aggregate(&self, expression: &Expr<'_>) -> Shipped<i32> {
        let Expr::Call {
            kind: crate::sql::ast::CallKind::Routine,
            name,
            args,
            argument_names: [],
            variadic: false,
            star,
            order_by,
            over: None,
            filter,
            ..
        } = expression
        else {
            return Err(Stop::Local);
        };
        let name = name.strip_prefix("pg_catalog.").unwrap_or(name);
        let mut types = [oid::UNKNOWN; 2];
        if args.len() > types.len() {
            return Err(Stop::Local);
        }
        for (target, argument) in types.iter_mut().zip(*args) {
            *target = match self.shape(argument)? {
                Shape::Value { type_oid, .. } => operator_type(type_oid),
                // Only `string_agg`'s delimiter may be a constant.
                Shape::Literal(Literal::Unknown(_)) if name == "string_agg" => oid::UNKNOWN,
                Shape::Literal(_) => return Err(Stop::Local),
            };
        }
        let result = match (name, *star, &types[..args.len()]) {
            ("count", true, []) | ("count", false, [_]) => oid::INT8,
            ("sum", false, [oid::INT2 | oid::INT4]) => oid::INT8,
            ("sum", false, [oid::INT8 | oid::NUMERIC]) => oid::NUMERIC,
            ("sum", false, [argument @ (oid::FLOAT4 | oid::FLOAT8)]) => *argument,
            ("avg", false, [oid::INT2 | oid::INT4 | oid::INT8 | oid::NUMERIC]) => oid::NUMERIC,
            ("avg", false, [oid::FLOAT4 | oid::FLOAT8]) => oid::FLOAT8,
            (
                "min" | "max",
                false,
                [
                    argument @ (oid::INT2
                    | oid::INT4
                    | oid::INT8
                    | oid::NUMERIC
                    | oid::FLOAT4
                    | oid::FLOAT8
                    | oid::TEXT
                    | oid::BPCHAR
                    | oid::DATE
                    | oid::TIMESTAMP
                    | oid::TIMESTAMPTZ),
                ],
            ) => *argument,
            ("bool_and" | "bool_or" | "every", false, [oid::BOOL]) => oid::BOOL,
            ("string_agg", false, [oid::TEXT, oid::TEXT | oid::UNKNOWN]) => oid::TEXT,
            _ => return Err(Stop::Local),
        };
        for key in *order_by {
            match self.shape(key.expression)? {
                Shape::Value { .. } => {}
                Shape::Literal(_) => return Err(Stop::Local),
            }
        }
        if let Some(filter) = filter {
            self.boolean_operand(filter)?;
        }
        Ok(result)
    }

    /// `deparseAggref`: `name([DISTINCT] arguments [ORDER BY ...])` and its
    /// `FILTER (WHERE ...)`.
    fn write_aggregate(
        &self,
        output: &mut impl core::fmt::Write,
        expression: &Expr<'_>,
    ) -> Shipped<()> {
        self.aggregate(expression)?;
        let Expr::Call {
            name,
            args,
            star,
            distinct,
            order_by,
            filter,
            ..
        } = expression
        else {
            return Err(Stop::Local);
        };
        let _ = write!(
            output,
            "{}(",
            name.strip_prefix("pg_catalog.").unwrap_or(name)
        );
        if *distinct {
            let _ = output.write_str("DISTINCT ");
        }
        if *star {
            let _ = output.write_char('*');
        }
        for (index, argument) in args.iter().enumerate() {
            if index != 0 {
                let _ = output.write_str(", ");
            }
            match self.shape(argument)? {
                Shape::Literal(literal) => self.write_constant(
                    output,
                    &Constant {
                        literal,
                        type_oid: oid::TEXT,
                        show: 0,
                    },
                )?,
                Shape::Value { .. } => self.write(output, argument)?,
            }
        }
        for (index, key) in order_by.iter().enumerate() {
            let _ = output.write_str(if index == 0 { " ORDER BY " } else { ", " });
            self.write(output, key.expression)?;
            write_direction(output, key);
        }
        if let Some(filter) = filter {
            let _ = output.write_str(") FILTER (WHERE ");
            self.write(output, filter)?;
        }
        let _ = output.write_char(')');
        Ok(())
    }

    fn boolean_operand(&self, expression: &Expr<'_>) -> Shipped<()> {
        match self.shape(expression)? {
            Shape::Value {
                type_oid: oid::BOOL,
                ..
            } => Ok(()),
            _ => Err(Stop::Local),
        }
    }

    /// The type a constant takes beside a value of `partner`'s type, as
    /// PostgreSQL's operator resolution coerces it.
    fn constant_type(literal: Literal<'_>, partner: i32) -> Shipped<i32> {
        let partner = operator_type(partner);
        match literal {
            Literal::Unknown(_) | Literal::Null => Ok(partner),
            Literal::Bool(_) if partner == oid::BOOL => Ok(oid::BOOL),
            Literal::Integer(value) => {
                let natural = if i32::try_from(value).is_ok() {
                    oid::INT4
                } else {
                    oid::INT8
                };
                match partner {
                    oid::INT2 | oid::INT4 | oid::INT8 => Ok(natural),
                    oid::NUMERIC | oid::FLOAT8 => Ok(partner),
                    _ => Err(Stop::Local),
                }
            }
            Literal::Decimal(..) => match partner {
                oid::INT2 | oid::INT4 | oid::INT8 | oid::NUMERIC => Ok(oid::NUMERIC),
                oid::FLOAT8 => Ok(oid::FLOAT8),
                _ => Err(Stop::Local),
            },
            Literal::Typed(_, type_oid) | Literal::Bound(_, type_oid)
                if type_oid == partner
                    || integer_rank(type_oid).is_some() && integer_rank(partner).is_some() =>
            {
                Ok(type_oid)
            }
            Literal::Bool(_) | Literal::Typed(..) | Literal::Bound(..) => Err(Stop::Local),
        }
    }

    /// Resolves a binary operator: its operands as printed, and its result.
    fn binary<'e>(
        &self,
        operator: BinaryOp,
        left: &'e Expr<'e>,
        right: &'e Expr<'e>,
    ) -> Shipped<(Operand<'e>, Operand<'e>, i32)> {
        let comparison = matches!(
            operator,
            BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq
        );
        let arithmetic = matches!(
            operator,
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod
        );
        let concat = operator == BinaryOp::Concat;
        if !comparison && !arithmetic && !concat {
            return Err(Stop::Local);
        }
        let result = |left_type: i32, right_type: i32| -> Shipped<i32> {
            let (left_type, right_type) = (operator_type(left_type), operator_type(right_type));
            if concat {
                return if left_type == oid::TEXT && right_type == oid::TEXT {
                    Ok(oid::TEXT)
                } else {
                    Err(Stop::Local)
                };
            }
            let compatible = left_type == right_type
                || numeric_rank(left_type).is_some() && numeric_rank(right_type).is_some();
            if !compatible {
                return Err(Stop::Local);
            }
            if arithmetic {
                let wider = wider_numeric(left_type, right_type);
                if numeric_rank(wider).is_none()
                    || operator == BinaryOp::Mod && wider == oid::FLOAT8
                {
                    return Err(Stop::Local);
                }
                return Ok(wider);
            }
            Ok(oid::BOOL)
        };
        match (self.shape(left)?, self.shape(right)?) {
            (
                Shape::Value {
                    type_oid: left_type,
                    ..
                },
                Shape::Value {
                    type_oid: right_type,
                    ..
                },
            ) => Ok((
                Operand::Expression(left),
                Operand::Expression(right),
                result(left_type, right_type)?,
            )),
            (Shape::Value { type_oid, plain }, Shape::Literal(literal)) => {
                let constant = Self::constant_type(literal, type_oid)?;
                let ty = result(type_oid, constant)?;
                Ok((
                    Operand::Expression(left),
                    Operand::Constant(Constant {
                        literal,
                        type_oid: constant,
                        show: if plain && constant == operator_type(type_oid) {
                            -2
                        } else {
                            0
                        },
                    }),
                    ty,
                ))
            }
            (Shape::Literal(literal), Shape::Value { type_oid, plain }) => {
                let constant = Self::constant_type(literal, type_oid)?;
                let ty = result(constant, type_oid)?;
                Ok((
                    Operand::Constant(Constant {
                        literal,
                        type_oid: constant,
                        show: if plain && constant == operator_type(type_oid) {
                            -2
                        } else {
                            0
                        },
                    }),
                    Operand::Expression(right),
                    ty,
                ))
            }
            (Shape::Literal(_), Shape::Literal(_)) => Err(Stop::Local),
        }
    }

    /// `[NOT] IN` over constants: the array element type.
    fn in_list<'e>(&self, operand: &'e Expr<'e>, list: &'e [&'e Expr<'e>]) -> Shipped<i32> {
        let Shape::Value { type_oid, .. } = self.shape(operand)? else {
            return Err(Stop::Local);
        };
        if matches!(type_oid, oid::VARCHAR | oid::BPCHAR | oid::FLOAT4) || list.is_empty() {
            return Err(Stop::Local);
        }
        // A list of parameters is an `ARRAY[...]` of them, which ships when
        // each has the operand's type.
        let value_param = |item: &Expr<'_>| {
            self.is_param(item) && matches!(self.shape(item), Ok(Shape::Value { .. }))
        };
        if list.len() > 1 && list.iter().any(|item| value_param(item)) {
            for item in list {
                match self.shape(item)? {
                    Shape::Value { type_oid: held, .. }
                        if value_param(item) && held == type_oid => {}
                    _ => return Err(Stop::Local),
                }
            }
            return Ok(type_oid);
        }
        let mut element = type_oid;
        for item in list {
            let Shape::Literal(literal) = self.shape(item)? else {
                return Err(Stop::Local);
            };
            let constant = Self::constant_type(literal, type_oid)?;
            element = wider_numeric(element, constant);
        }
        Ok(element)
    }

    fn pattern(&self, operand: &Expr<'_>, pattern: &Expr<'_>) -> Shipped<()> {
        let Shape::Value { type_oid, .. } = self.shape(operand)? else {
            return Err(Stop::Local);
        };
        if operator_type(type_oid) != oid::TEXT {
            return Err(Stop::Local);
        }
        match self.shape(pattern)? {
            Shape::Literal(Literal::Unknown(_)) => Ok(()),
            Shape::Literal(Literal::Typed(_, oid::TEXT) | Literal::Bound(_, oid::TEXT)) => Ok(()),
            Shape::Value { type_oid, .. } if operator_type(type_oid) == oid::TEXT => Ok(()),
            _ => Err(Stop::Local),
        }
    }

    /// `deparseConst`.
    fn write_constant(
        &self,
        output: &mut impl core::fmt::Write,
        constant: &Constant<'_>,
    ) -> Shipped<()> {
        let Constant {
            literal,
            type_oid,
            show,
        } = *constant;
        let source = match literal {
            Literal::Null => {
                let _ = output.write_str("NULL");
                if show >= 0 {
                    let _ = output.write_str("::");
                    write_type_name(output, type_oid)?;
                }
                return Ok(());
            }
            Literal::Bound(node, held) => {
                let ctype = crate::sql::exec::coltype_of_oid(type_oid).ok_or(Stop::Local)?;
                let mut value = self.bound_value(node)?;
                if held != type_oid {
                    value = crate::sql::eval::cast_to(value, ctype, self.arena)
                        .map_err(|_| Stop::Local)?;
                }
                let text = self
                    .arena
                    .alloc_str_display(value)
                    .map_err(|_| arena_full())?;
                return Self::write_constant_text(output, text, type_oid, show);
            }
            other => other,
        };
        let mut digits = StackStr::<128>::new();
        let source = source_text(source, &mut digits)?;
        let ctype = crate::sql::exec::coltype_of_oid(type_oid).ok_or(Stop::Local)?;
        // The constant prints as its type's output function writes it.
        let value = match crate::sql::eval::cast_to(Datum::Text(source), ctype, self.arena) {
            Ok(value) => value,
            Err(_) => return Err(Stop::Local),
        };
        let text = self
            .arena
            .alloc_str_display(value)
            .map_err(|_| arena_full())?;
        Self::write_constant_text(output, text, type_oid, show)
    }

    fn write_constant_text(
        output: &mut impl core::fmt::Write,
        text: &str,
        type_oid: i32,
        show: i8,
    ) -> Shipped<()> {
        let mut float = false;
        let mut string = false;
        match type_oid {
            oid::INT2
            | oid::INT4
            | oid::INT8
            | oid::OID
            | oid::FLOAT4
            | oid::FLOAT8
            | oid::NUMERIC => {
                if !text.is_empty()
                    && text
                        .chars()
                        .all(|character| "0123456789+-eE.".contains(character))
                {
                    if text.starts_with(['+', '-']) {
                        let _ = write!(output, "({text})");
                    } else {
                        let _ = output.write_str(text);
                    }
                    float = text.contains(['e', 'E', '.']);
                } else {
                    let _ = write!(output, "'{text}'");
                }
            }
            oid::BOOL => {
                let _ = output.write_str(if text == "true" || text == "t" {
                    "true"
                } else {
                    "false"
                });
            }
            _ => {
                write_string_literal(output, text);
                string = true;
            }
        }
        if show == -1 {
            return Ok(());
        }
        let label = match type_oid {
            oid::BOOL | oid::INT4 | oid::UNKNOWN => false,
            oid::NUMERIC => !float,
            _ if show == -2 => !string,
            _ => true,
        };
        if label || show > 0 {
            let _ = output.write_str("::");
            write_type_name(output, type_oid)?;
        }
        Ok(())
    }

    fn write_operand(
        &self,
        output: &mut impl core::fmt::Write,
        operand: &Operand<'_>,
    ) -> Shipped<()> {
        match operand {
            Operand::Expression(expression) => self.write(output, expression),
            Operand::Constant(constant) => self.write_constant(output, constant),
        }
    }

    /// Writes a shippable expression as postgres_fdw's `deparseExpr` does.
    fn write(&self, output: &mut impl core::fmt::Write, expression: &Expr<'_>) -> Shipped<()> {
        if let Some(column) = self.column(expression) {
            self.column_type(column)?;
            self.write_column(output, column);
            return Ok(());
        }
        // `printRemoteParam`.
        if let (Some((key, type_oid)), Some(params)) = (self.remote_param(expression)?, self.params)
        {
            let number = params.number(key, type_oid)?;
            let _ = write!(output, "${number}::");
            return write_type_name(output, type_oid);
        }
        match expression {
            Expr::Unary {
                operator: UnaryOp::Not,
                operand,
                ..
            } => {
                self.boolean_operand(operand)?;
                let _ = output.write_str("(NOT ");
                self.write(output, operand)?;
                let _ = output.write_char(')');
                Ok(())
            }
            Expr::Binary {
                operator: operator @ (BinaryOp::And | BinaryOp::Or),
                ..
            } => {
                self.shape(expression)?;
                let _ = output.write_char('(');
                self.write_connective(output, *operator, expression, true)?;
                let _ = output.write_char(')');
                Ok(())
            }
            Expr::Binary {
                operator,
                left,
                right,
                ..
            } if self.boolean_equality(*operator, left, right)?.is_some() => {
                let Some((operand, negated)) = self.boolean_equality(*operator, left, right)?
                else {
                    unreachable!("checked by the guard")
                };
                if negated {
                    let _ = output.write_str("(NOT ");
                }
                self.write(output, operand)?;
                if negated {
                    let _ = output.write_char(')');
                }
                Ok(())
            }
            Expr::Call { .. } if self.grouped && expression.is_aggregate() => {
                self.write_aggregate(output, expression)
            }
            Expr::Call { args, .. } => {
                let signature = self.call(expression)?;
                if signature.schema != "pg_catalog" {
                    super::quote_identifier(output, signature.schema);
                    let _ = output.write_char('.');
                }
                super::quote_identifier(output, signature.name);
                let _ = output.write_char('(');
                for (index, (argument, &parameter)) in
                    args.iter().zip(signature.arguments).enumerate()
                {
                    if index != 0 {
                        let _ = output.write_str(", ");
                    }
                    match self.shape(argument)? {
                        Shape::Literal(literal) => self.write_constant(
                            output,
                            &Constant {
                                literal,
                                type_oid: parameter,
                                show: 0,
                            },
                        )?,
                        Shape::Value { .. } => self.write(output, argument)?,
                    }
                }
                let _ = output.write_char(')');
                Ok(())
            }
            Expr::Binary {
                operator,
                left,
                right,
                ..
            } => {
                let (left, right, _) = self.binary(*operator, left, right)?;
                let symbol = match operator {
                    BinaryOp::Eq => "=",
                    BinaryOp::NotEq => "<>",
                    BinaryOp::Lt => "<",
                    BinaryOp::LtEq => "<=",
                    BinaryOp::Gt => ">",
                    BinaryOp::GtEq => ">=",
                    BinaryOp::Add => "+",
                    BinaryOp::Sub => "-",
                    BinaryOp::Mul => "*",
                    BinaryOp::Div => "/",
                    BinaryOp::Mod => "%",
                    BinaryOp::Concat => "||",
                    _ => return Err(Stop::Local),
                };
                let _ = output.write_char('(');
                self.write_operand(output, &left)?;
                let _ = write!(output, " {symbol} ");
                self.write_operand(output, &right)?;
                let _ = output.write_char(')');
                Ok(())
            }
            Expr::IsNull { operand, negated } => {
                self.shape(expression)?;
                let _ = output.write_char('(');
                self.write(output, operand)?;
                let _ = output.write_str(if *negated {
                    " IS NOT NULL)"
                } else {
                    " IS NULL)"
                });
                Ok(())
            }
            Expr::InList {
                operand,
                list,
                negated,
            } => {
                let element = self.in_list(operand, list)?;
                // A one-item list is an operator, as PostgreSQL's parser
                // turns it into one.
                if let [item] = list {
                    let operator = if *negated {
                        BinaryOp::NotEq
                    } else {
                        BinaryOp::Eq
                    };
                    let (left, right, _) = self.binary(operator, operand, item)?;
                    let _ = output.write_char('(');
                    self.write_operand(output, &left)?;
                    let _ = output.write_str(if *negated { " <> " } else { " = " });
                    self.write_operand(output, &right)?;
                    let _ = output.write_char(')');
                    return Ok(());
                }
                // `deparseArrayExpr`.
                if self.is_param(list[0]) && matches!(self.shape(list[0]), Ok(Shape::Value { .. }))
                {
                    let _ = output.write_char('(');
                    self.write(output, operand)?;
                    let _ = output.write_str(if *negated {
                        " <> ALL (ARRAY["
                    } else {
                        " = ANY (ARRAY["
                    });
                    for (index, item) in list.iter().enumerate() {
                        if index != 0 {
                            let _ = output.write_str(", ");
                        }
                        self.write(output, item)?;
                    }
                    let _ = output.write_str("]))");
                    return Ok(());
                }
                let element_type = crate::sql::exec::coltype_of_oid(element).ok_or(Stop::Local)?;
                let mut array = StackStr::<{ REMOTE_QUERY_BYTES / 2 }>::new();
                let _ = array.write_char('{');
                for (index, item) in list.iter().enumerate() {
                    if index != 0 {
                        let _ = array.write_char(',');
                    }
                    let Shape::Literal(literal) = self.shape(item)? else {
                        return Err(Stop::Local);
                    };
                    let source = match literal {
                        Literal::Null => {
                            let _ = array.write_str("NULL");
                            continue;
                        }
                        other => other,
                    };
                    let mut digits = StackStr::<128>::new();
                    let value = match source {
                        Literal::Bound(node, _) => self.bound_value(node)?,
                        source => Datum::Text(source_text(source, &mut digits)?),
                    };
                    let value = crate::sql::eval::cast_to(value, element_type, self.arena)
                        .map_err(|_| Stop::Local)?;
                    let text = self
                        .arena
                        .alloc_str_display(value)
                        .map_err(|_| arena_full())?;
                    write_array_element(&mut array, text);
                }
                let _ = array.write_char('}');
                if array.is_truncated() {
                    return Err(Stop::Error(query_too_long()));
                }
                let _ = output.write_char('(');
                self.write(output, operand)?;
                let _ = output.write_str(if *negated { " <> ALL (" } else { " = ANY (" });
                write_string_literal(output, array.as_str());
                let _ = output.write_str("::");
                write_type_name(output, element)?;
                let _ = output.write_str("[]))");
                Ok(())
            }
            Expr::Like {
                operand,
                pattern,
                negated,
                case_insensitive,
                escape: None,
            }
            | Expr::Match {
                operand,
                pattern,
                negated,
                case_insensitive,
            } => {
                self.pattern(operand, pattern)?;
                let like = matches!(expression, Expr::Like { .. });
                let symbol = match (like, *negated, *case_insensitive) {
                    (true, false, false) => "~~",
                    (true, false, true) => "~~*",
                    (true, true, false) => "!~~",
                    (true, true, true) => "!~~*",
                    (false, false, false) => "~",
                    (false, false, true) => "~*",
                    (false, true, false) => "!~",
                    (false, true, true) => "!~*",
                };
                let plain = self.column(operand).is_some();
                let _ = output.write_char('(');
                self.write(output, operand)?;
                let _ = write!(output, " {symbol} ");
                match self.shape(pattern)? {
                    Shape::Literal(literal) => self.write_constant(
                        output,
                        &Constant {
                            literal,
                            type_oid: oid::TEXT,
                            show: if plain { -2 } else { 0 },
                        },
                    )?,
                    Shape::Value { .. } => self.write(output, pattern)?,
                }
                let _ = output.write_char(')');
                Ok(())
            }
            _ => Err(Stop::Local),
        }
    }

    /// The operands of an `AND` or `OR` chain, flattened as PostgreSQL's
    /// planner flattens nested connectives of the same kind.
    fn write_connective(
        &self,
        output: &mut impl core::fmt::Write,
        operator: BinaryOp,
        expression: &Expr<'_>,
        first: bool,
    ) -> Shipped<bool> {
        if let Expr::Binary {
            operator: inner,
            left,
            right,
            ..
        } = expression
            && *inner == operator
        {
            let first = self.write_connective(output, operator, left, first)?;
            return self.write_connective(output, operator, right, first);
        }
        if !first {
            let _ = output.write_str(if operator == BinaryOp::And {
                " AND "
            } else {
                " OR "
            });
        }
        self.write(output, expression)?;
        Ok(false)
    }

    /// Appends `expression` when it ships; leaves `output` unchanged
    /// otherwise.
    pub(crate) fn try_write<const N: usize>(
        &self,
        output: &mut StackStr<N>,
        expression: &Expr<'_>,
    ) -> Result<bool, SqlError> {
        let mark = output.len();
        let named = self.param_mark();
        let shipped = self.shape(expression).and_then(|shape| match shape {
            Shape::Value { .. } => self.write(output, expression),
            Shape::Literal(_) => Err(Stop::Local),
        });
        match shipped {
            Ok(()) => {
                if output.is_truncated() {
                    return Err(query_too_long());
                }
                Ok(true)
            }
            Err(Stop::Local) => {
                output.truncate(mark);
                self.param_reset(named);
                Ok(false)
            }
            Err(Stop::Error(error)) => Err(error),
        }
    }

    /// How many parameters the text written so far names.
    pub(crate) fn param_mark(&self) -> usize {
        self.params.map_or(0, RemoteParams::mark)
    }

    /// Forgets the parameters named by text written after `mark`.
    pub(crate) fn param_reset(&self, mark: usize) {
        if let Some(params) = self.params {
            params.reset(mark);
        }
    }

    /// Writes a `WHERE` condition when it ships, as `appendConditions`
    /// wraps each one.
    pub(crate) fn try_write_condition<const N: usize>(
        &self,
        output: &mut StackStr<N>,
        expression: &Expr<'_>,
        separator: &str,
    ) -> Result<bool, SqlError> {
        let mark = output.len();
        let _ = output.write_str(separator);
        let _ = output.write_char('(');
        if !matches!(
            self.shape(expression),
            Ok(Shape::Value {
                type_oid: oid::BOOL,
                ..
            })
        ) {
            output.truncate(mark);
            return Ok(false);
        }
        if !self.try_write(output, expression)? {
            output.truncate(mark);
            return Ok(false);
        }
        let _ = output.write_char(')');
        Ok(true)
    }

    /// Writes a value assigned to `column` when it ships: a constant coerced
    /// to the column's type, or an expression the assignment coerces
    /// implicitly.
    pub(crate) fn try_write_assigned<const N: usize>(
        &self,
        output: &mut StackStr<N>,
        value: &Expr<'_>,
        column: usize,
    ) -> Result<bool, SqlError> {
        let definition = &self.table.columns()[column];
        let target = match self.column_type(column) {
            Ok(target) => target,
            Err(Stop::Local) => return Ok(false),
            Err(Stop::Error(error)) => return Err(error),
        };
        let mark = output.len();
        let written = self.shape(value).and_then(|shape| match shape {
            // A typmod'd constant prints its modifier; leave it local.
            Shape::Literal(literal) if definition.type_mod < 0 => {
                if !matches!(literal, Literal::Unknown(_) | Literal::Null) {
                    Self::constant_type(literal, target)?;
                }
                self.write_constant(
                    output,
                    &Constant {
                        literal,
                        type_oid: target,
                        show: 0,
                    },
                )
            }
            Shape::Literal(_) => Err(Stop::Local),
            Shape::Value { type_oid, .. } => {
                let (from, to) = (operator_type(type_oid), operator_type(target));
                let coerced = from == to
                    || numeric_rank(from).is_some() && numeric_rank(to).is_some()
                    || from == oid::TEXT && to == oid::BPCHAR;
                if !coerced {
                    return Err(Stop::Local);
                }
                self.write(output, value)
            }
        });
        match written {
            Ok(()) if output.is_truncated() => Err(query_too_long()),
            Ok(()) => Ok(true),
            Err(Stop::Local) => {
                output.truncate(mark);
                Ok(false)
            }
            Err(Stop::Error(error)) => Err(error),
        }
    }

    /// Writes a constant `LIMIT` or `OFFSET` count as a `bigint`.
    /// The type a select-list constant resolves to: an untyped literal or
    /// `NULL` is `text` there.
    fn target_constant_type(literal: Literal<'_>) -> i32 {
        match Self::literal_type(literal) {
            oid::UNKNOWN => oid::TEXT,
            type_oid => type_oid,
        }
    }

    /// Writes a constant of a grouped query's target list as `deparseConst`
    /// writes one there; leaves `output` unchanged for anything else.
    fn try_write_target_constant<const N: usize>(
        &self,
        output: &mut StackStr<N>,
        expression: &Expr<'_>,
    ) -> Result<bool, SqlError> {
        let literal = match self.shape(expression) {
            Ok(Shape::Literal(literal)) => literal,
            Ok(Shape::Value { .. }) | Err(Stop::Local) => return Ok(false),
            Err(Stop::Error(error)) => return Err(error),
        };
        let mark = output.len();
        let constant = Constant {
            literal,
            type_oid: Self::target_constant_type(literal),
            show: 0,
        };
        match self.write_constant(output, &constant) {
            Ok(()) if output.is_truncated() => Err(query_too_long()),
            Ok(()) => Ok(true),
            Err(Stop::Local) => {
                output.truncate(mark);
                Ok(false)
            }
            Err(Stop::Error(error)) => Err(error),
        }
    }

    /// The type a shipped grouped target computes; `None` outside the
    /// modeled shapes.
    fn target_type(&self, expression: &Expr<'_>) -> Result<Option<i32>, SqlError> {
        match self.shape(expression) {
            Ok(Shape::Value { type_oid, .. }) => Ok(Some(type_oid)),
            Ok(Shape::Literal(literal)) => Ok(Some(Self::target_constant_type(literal))),
            Err(Stop::Local) => Ok(None),
            Err(Stop::Error(error)) => Err(error),
        }
    }

    fn try_write_count<const N: usize>(
        &self,
        output: &mut StackStr<N>,
        expression: &Expr<'_>,
    ) -> Shipped<()> {
        let literal = match expression {
            Expr::Int(_) | Expr::Unary { .. } => self.integer(expression).ok_or(Stop::Local)?,
            Expr::Null => Literal::Null,
            // A parameter count stays local (`add_foreign_final_paths`
            // ships only a constant one); a custom plan's is a constant.
            _ => match self.param_shape(expression)? {
                Some(Shape::Literal(literal @ (Literal::Null | Literal::Bound(..)))) => literal,
                _ => return Err(Stop::Local),
            },
        };
        if matches!(literal, Literal::Decimal(..)) {
            return Err(Stop::Local);
        }
        self.write_constant(
            output,
            &Constant {
                literal,
                type_oid: oid::INT8,
                show: 0,
            },
        )
    }
}

/// `appendOrderBySuffix` for a key with the type's default operators.
fn write_direction(output: &mut impl core::fmt::Write, key: &OrderBy<'_>) {
    let _ = output.write_str(if key.descending { " DESC" } else { " ASC" });
    let _ = output.write_str(if key.nulls_first {
        " NULLS FIRST"
    } else {
        " NULLS LAST"
    });
}

/// Whether a folded expression read a custom plan's parameter, whose value
/// PostgreSQL's plan holds instead of the expression.
fn reads_custom_parameter(expression: &Expr<'_>) -> Result<bool, SqlError> {
    let mut reads = false;
    crate::sql::query::walk_expression_tree(expression, &mut |node| {
        reads |= matches!(node, Expr::Param(index) if crate::sql::eval::custom_plan_binds(*index));
    })?;
    Ok(reads)
}

/// A constant the planner bound, as the literal it was written as.
fn unbound<'e>(expression: &'e Expr<'e>) -> &'e Expr<'e> {
    match expression {
        Expr::BoundLiteral { source, .. } => unbound(source),
        other => other,
    }
}

/// The text a non-null literal's type input function reads.
fn source_text<'s>(literal: Literal<'s>, digits: &'s mut StackStr<128>) -> Shipped<&'s str> {
    match literal {
        Literal::Null => Err(Stop::Local),
        Literal::Bool(value) => Ok(if value { "t" } else { "f" }),
        Literal::Unknown(text) | Literal::Typed(text, _) | Literal::Decimal(text, false) => {
            Ok(text)
        }
        Literal::Integer(value) => {
            let _ = write!(digits, "{value}");
            Ok(digits.as_str())
        }
        Literal::Decimal(text, true) => {
            let _ = write!(digits, "-{text}");
            if digits.is_truncated() {
                return Err(Stop::Local);
            }
            Ok(digits.as_str())
        }
        // Its callers print the value itself.
        Literal::Bound(..) => Err(Stop::Local),
    }
}

/// A printed operand of an operator.
enum Operand<'e> {
    Expression(&'e Expr<'e>),
    Constant(Constant<'e>),
}

fn query_too_long() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "foreign query exceeds its fixed buffer"
    )
}

/// The `AND`ed conditions of `expression`, flattened.
pub(crate) fn conjuncts<'e>(
    expression: Option<&'e Expr<'e>>,
    arena: &'e Arena,
) -> Result<&'e [&'e Expr<'e>], SqlError> {
    fn count(expression: &Expr<'_>) -> usize {
        match expression {
            Expr::Binary {
                operator: BinaryOp::And,
                left,
                right,
                ..
            } => count(left) + count(right),
            _ => 1,
        }
    }
    fn fill<'e>(expression: &'e Expr<'e>, target: &mut [&'e Expr<'e>], next: &mut usize) {
        match expression {
            Expr::Binary {
                operator: BinaryOp::And,
                left,
                right,
                ..
            } => {
                fill(left, target, next);
                fill(right, target, next);
            }
            other => {
                target[*next] = other;
                *next += 1;
            }
        }
    }
    let Some(expression) = expression else {
        return Ok(&[]);
    };
    let list = arena
        .alloc_slice_with(count(expression), |_| expression)
        .map_err(|_| arena_full())?;
    let mut next = 0;
    fill(expression, list, &mut next);
    Ok(list)
}

/// Rebuilds the `AND` of `kept` conditions, or `None` when there are none.
pub(crate) fn conjoin<'e>(
    kept: &[&'e Expr<'e>],
    arena: &'e Arena,
) -> Result<Option<&'e Expr<'e>>, SqlError> {
    let mut result: Option<&'e Expr<'e>> = None;
    for &condition in kept {
        result = Some(match result {
            None => condition,
            Some(left) => arena
                .alloc(Expr::Binary {
                    source_position: condition.source_position(),
                    operator: BinaryOp::And,
                    left,
                    right: condition,
                })
                .map_err(|_| arena_full())?,
        });
    }
    Ok(result)
}

/// The columns an expression reads, or `None` when it may read the whole
/// row or an enclosing query's columns (whole-row values and subqueries),
/// so every column is fetched.
fn read_columns(
    deparser: &Deparser<'_>,
    expression: &Expr<'_>,
    columns: &mut ColumnSet,
) -> Option<()> {
    let mut each = |child: &Expr<'_>| read_columns(deparser, child, columns);
    match expression {
        Expr::Column { .. } => {
            // A name that is no column of the table is an output name, which
            // its select-list item covers, or an error the query reports.
            if let Some(column) = deparser.column(expression) {
                columns.insert(column);
            }
            Some(())
        }
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::NumericLit(_)
        | Expr::Str(_)
        | Expr::BitLit(_)
        | Expr::BoundLiteral { .. }
        | Expr::Param(_)
        | Expr::RoutineParam { .. }
        | Expr::DefaultMarker
        | Expr::RecursiveState { .. } => Some(()),
        Expr::Subquery(_) if deparser.is_param(expression) => Some(()),
        Expr::WholeRow(_)
        | Expr::SchemaColumn { .. }
        | Expr::Subquery(_)
        | Expr::RowSubquery { .. }
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. }
        | Expr::Exists(_)
        | Expr::ArraySubquery(_)
        | Expr::OuterAggregate { .. } => None,
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::BoundDefault {
            expression: operand,
            ..
        }
        | Expr::Collate { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Field { base: operand, .. }
        | Expr::RecordFieldIndex { base: operand, .. } => each(operand),
        Expr::Slice { base, lower, upper } => {
            each(base)?;
            lower.map_or(Some(()), &mut each)?;
            upper.map_or(Some(()), each)
        }
        Expr::Binary { left, right, .. }
        | Expr::Subscript {
            base: left,
            index: right,
        }
        | Expr::AnyAll {
            operand: left,
            array: right,
            ..
        } => {
            each(left)?;
            each(right)
        }
        Expr::Call {
            args,
            order_by,
            over,
            filter,
            ..
        } => {
            for argument in *args {
                each(argument)?;
            }
            for key in *order_by {
                each(key.expression)?;
            }
            if let Some(filter) = filter {
                each(filter)?;
            }
            if let Some(window) = over {
                for expression in window.partition_by {
                    each(expression)?;
                }
                for key in window.order_by {
                    each(key.expression)?;
                }
            }
            Some(())
        }
        Expr::InList { operand, list, .. } => {
            each(operand)?;
            for item in *list {
                each(item)?;
            }
            Some(())
        }
        Expr::Like {
            operand,
            pattern,
            escape,
            ..
        } => {
            each(operand)?;
            each(pattern)?;
            escape.map_or(Some(()), each)
        }
        Expr::Match {
            operand, pattern, ..
        } => {
            each(operand)?;
            each(pattern)
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
            ..
        } => {
            operand.map_or(Some(()), &mut each)?;
            for (condition, result) in *whens {
                each(condition)?;
                each(result)?;
            }
            otherwise.map_or(Some(()), each)
        }
        Expr::Array(items) => {
            for item in *items {
                each(item)?;
            }
            Some(())
        }
    }
}

/// Whether an expression calls a routine or reads a subquery, either of
/// which may make the query grouped or set-returning; a subquery the plan
/// runs as an InitPlan reads one value and does neither.
fn calls_beyond_params(deparser: &Deparser<'_>, expression: &Expr<'_>) -> Result<bool, SqlError> {
    if expression.find_function(&|_, _| true).is_some() {
        return Ok(true);
    }
    let mut other = false;
    crate::sql::query::walk_expression_tree(expression, &mut |node| {
        other |= matches!(
            node,
            Expr::Subquery(_)
                | Expr::RowSubquery { .. }
                | Expr::InSubquery { .. }
                | Expr::QuantifiedSubquery { .. }
                | Expr::Exists(_)
                | Expr::ArraySubquery(_)
        ) && !deparser.is_param(node);
    })?;
    Ok(other)
}

/// The remote scan of a `SELECT` reading one foreign table.
pub(crate) struct ForeignSelectPlan<'a> {
    pub table_slot: usize,
    /// The columns the remote query returns, in table order.
    pub fetched: ColumnSet,
    /// The `ORDER BY` keys, positions and output names resolved, which the
    /// scan returns for the sort whether or not the remote query applies it.
    pub sort_keys: &'a [OrderBy<'a>],
    /// The order the remote rows arrive in for DISTINCT or a window, which
    /// the local plan reads without sorting.
    pub presorted: &'a [OrderBy<'a>],
    /// The conditions evaluated locally after the rows arrive.
    pub local_where: Option<&'a Expr<'a>>,
    pub order_pushed: bool,
    pub limit_pushed: bool,
    pub sql: &'a str,
    /// The expressions whose values the remote query reads as `$1`, `$2`,
    /// ...: statement parameters and InitPlan subqueries.
    pub params: &'a [&'a Expr<'a>],
    /// The grouped relation the remote query computes, when the query's
    /// grouping and aggregates ship.
    pub upper: Option<ForeignUpper<'a>>,
}

/// postgres_fdw's grouped relation (`Aggregate on (...)`), built as
/// `foreign_grouping_ok` builds it: the remote query groups and aggregates,
/// and its target list (`fdw_scan_tlist`) fills one local column each.
pub(crate) struct ForeignUpper<'a> {
    /// The grouping target the scan returns: the select list, then the
    /// `ORDER BY` and `GROUP BY` keys it lacks.
    pub output: &'a [&'a Expr<'a>],
    /// The remote target list (`fdw_scan_tlist`), which nodes above the
    /// scan read by reference.
    pub targets: &'a [&'a Expr<'a>],
    /// The relation the remote rows fill, one column per target: a grouped
    /// table column keeps its definition, any other target is named
    /// `?targetN?` with the type it computes.
    pub columns: &'a [crate::storage::ColumnMeta],
    /// The `HAVING` conditions evaluated over the remote rows.
    pub local_having: Option<&'a Expr<'a>>,
    /// The `ORDER BY` keys, resolved, when the rows are sorted here.
    pub local_order: &'a [OrderBy<'a>],
    /// The statement the executor runs over the remote rows: the select
    /// list read from those columns or computed from them, the conditions,
    /// order and `LIMIT` the remote query leaves.
    pub local: &'a Select<'a>,
}

impl<'a> ForeignSelectPlan<'a> {
    /// What the remote query returns.
    pub(crate) fn remote_columns(&self) -> super::RemoteColumns<'a> {
        match &self.upper {
            Some(upper) => super::RemoteColumns::Upper(upper.columns),
            None => super::RemoteColumns::Table(self.fetched),
        }
    }

    /// The statement the local executor runs over the remote rows: without
    /// the conditions, order and limit the remote query applies.
    pub(crate) fn local_statement(
        &self,
        statement: &'a Select<'a>,
        arena: &'a Arena,
    ) -> Result<&'a Select<'a>, SqlError> {
        if let Some(upper) = &self.upper {
            return Ok(upper.local);
        }
        let mut local = *statement;
        local.where_clause = self.local_where;
        // `WITH TIES` reads the order to find the ties it keeps.
        if self.order_pushed && !statement.with_ties {
            local.order_by = &[];
        }
        if self.limit_pushed {
            local.limit = None;
            local.offset = None;
        }
        arena
            .alloc(local)
            .map(|local| &*local)
            .map_err(|_| arena_full())
    }
}

/// Splits `predicate` into the conditions that ship, written to `output` as
/// postgres_fdw's `appendConditions` writes them, and those kept local.
/// The shipped ones follow `order_qual_clauses`: by evaluation cost, the
/// equivalence-class equalities the planner regenerates after the rest.
pub(crate) fn remote_conditions<'a, const N: usize>(
    deparser: &Deparser<'_>,
    predicate: Option<&'a Expr<'a>>,
    output: &mut StackStr<N>,
    arena: &'a Arena,
) -> Result<&'a [&'a Expr<'a>], SqlError> {
    let conditions = conjuncts(predicate, arena)?;
    let kept = arena
        .alloc_slice_with(conditions.len(), |index| conditions[index])
        .map_err(|_| arena_full())?;
    // (condition as written remotely, regenerated equality, cost)
    let shipped = arena
        .alloc_slice_with(conditions.len(), |index| (conditions[index], false, 0u32))
        .map_err(|_| arena_full())?;
    let (mut kept_count, mut shipped_count) = (0, 0);
    let mut probe = RemoteText::new();
    for &condition in conditions {
        probe.clear();
        // A condition that reads no column of the table is a pseudoconstant
        // the plan evaluates once, not a scan qualification.
        let named = deparser.param_mark();
        let ships = deparser.try_write_condition(&mut probe, condition, "")?
            && (named == deparser.param_mark() || deparser.reads_column(condition)?);
        deparser.param_reset(named);
        if ships {
            let (written, equivalence) = match deparser.equivalence(condition, arena)? {
                Some(regenerated) => (regenerated, true),
                None => (condition, false),
            };
            let cost = match deparser.cost(written) {
                Ok(cost) => cost,
                Err(Stop::Local) => {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "a shippable foreign condition has no modeled cost"
                    ));
                }
                Err(Stop::Error(error)) => return Err(error),
            };
            shipped[shipped_count] = (written, equivalence, cost);
            shipped_count += 1;
        } else {
            kept[kept_count] = condition;
            kept_count += 1;
        }
    }
    let shipped = &mut shipped[..shipped_count];
    // By cost, equalities after the rest at equal cost, otherwise as
    // written: an insertion sort, which needs no buffer.
    for next in 1..shipped.len() {
        let mut at = next;
        while at > 0 && (shipped[at - 1].2, shipped[at - 1].1) > (shipped[at].2, shipped[at].1) {
            shipped.swap(at - 1, at);
            at -= 1;
        }
    }
    for (index, &(condition, _, _)) in shipped.iter().enumerate() {
        let separator = if index == 0 { " WHERE " } else { " AND " };
        if !deparser.try_write_condition(output, condition, separator)? {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "a regenerated foreign condition no longer ships"
            ));
        }
    }
    Ok(&kept[..kept_count])
}

/// The table columns the top-level `WHERE` conditions equate with a
/// constant or parameter, which PostgreSQL's equivalence classes fix.
fn constant_columns(
    deparser: &Deparser<'_>,
    predicate: Option<&Expr<'_>>,
    arena: &Arena,
) -> Result<ColumnSet, SqlError> {
    let mut fixed = ColumnSet::from_low_word(0);
    let Some(predicate) = predicate else {
        return Ok(fixed);
    };
    for &condition in conjuncts(Some(predicate), arena)? {
        let (left, right) = match condition {
            Expr::Binary {
                operator: BinaryOp::Eq,
                left,
                right,
                ..
            } => (*left, *right),
            Expr::InList {
                operand,
                list: [item],
                negated: false,
            } => (*operand, *item),
            _ => continue,
        };
        let constant = |side: &Expr<'_>| {
            !matches!(unbound(side), Expr::Null | Expr::Bool(_))
                && (matches!(deparser.shape(side), Ok(Shape::Literal(_)))
                    || deparser.is_param(side))
        };
        for (column, other) in [(left, right), (right, left)] {
            if let Some(column) = deparser.column(column)
                && constant(other)
                && deparser
                    .column_type(column)
                    .is_ok_and(|type_oid| type_oid != oid::BOOL)
            {
                fixed.insert(column);
            }
        }
    }
    Ok(fixed)
}

/// The expression an ORDER BY key sorts by: a select-list item through its
/// position or output name, else the key itself.
fn order_expression<'a>(statement: &Select<'a>, key: &'a Expr<'a>) -> &'a Expr<'a> {
    let item = |index: usize| match statement.items.get(index) {
        Some(SelectItem::Expr { expression, .. }) => Some(*expression),
        _ => None,
    };
    let named = match key {
        Expr::Int(position) => usize::try_from(*position)
            .ok()
            .and_then(|position| position.checked_sub(1))
            .and_then(item),
        Expr::Column {
            qualifier: None,
            name,
        } => statement
            .items
            .iter()
            .position(|candidate| {
                matches!(candidate, SelectItem::Expr { alias: Some(alias), .. } if alias == name)
            })
            .and_then(item),
        _ => None,
    };
    named.unwrap_or(key)
}

/// The query's ORDER BY keys with their expressions resolved.
fn resolved_order<'a>(
    statement: &Select<'a>,
    arena: &'a Arena,
) -> Result<&'a [OrderBy<'a>], SqlError> {
    arena
        .alloc_slice_with(statement.order_by.len(), |index| {
            let key = statement.order_by[index];
            OrderBy {
                expression: order_expression(statement, key.expression),
                ..key
            }
        })
        .map(|keys| &*keys)
        .map_err(|_| arena_full())
}

/// The order a DISTINCT query reads its rows in: its ORDER BY keys, then
/// the remaining distinct expressions ascending. `None` when a star leaves
/// the distinct expressions unknown here.
fn distinct_keys<'a>(
    deparser: &Deparser<'_>,
    statement: &Select<'a>,
    arena: &'a Arena,
) -> Result<Option<&'a [OrderBy<'a>]>, SqlError> {
    let ordered = resolved_order(statement, arena)?;
    let mut keys = ArenaList::new(arena);
    for &key in ordered {
        keys.push(key).map_err(|_| arena_full())?;
    }
    let mut distinct = ArenaList::new(arena);
    if statement.distinct_on.is_empty() {
        for item in statement.items {
            match item {
                SelectItem::Expr { expression, .. } => {
                    distinct.push(*expression).map_err(|_| arena_full())?
                }
                _ => return Ok(None),
            }
        }
    } else {
        for &key in statement.distinct_on {
            distinct
                .push(order_expression(statement, key))
                .map_err(|_| arena_full())?;
        }
    }
    for &expression in distinct.as_slice() {
        let present = keys.as_slice().iter().any(|key: &OrderBy<'_>| {
            *key.expression == *expression
                || deparser.column(key.expression).is_some()
                    && deparser.column(key.expression) == deparser.column(expression)
        });
        if !present {
            keys.push(OrderBy {
                expression,
                descending: false,
                nulls_first: false,
            })
            .map_err(|_| arena_full())?;
        }
    }
    Ok(Some(keys.as_slice()))
}

/// Plans the remote scan of `statement` when it reads exactly one foreign
/// table, as postgres_fdw's `postgresGetForeignRelSize`,
/// `postgresGetForeignPaths` and `add_foreign_final_paths` do for a base
/// relation: shippable conditions, then `ORDER BY` when every key ships,
/// then `LIMIT`/`OFFSET` when nothing is left to filter locally.
pub(crate) fn plan_select<'a>(
    storage: &Storage,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
) -> Result<Option<ForeignSelectPlan<'a>>, SqlError> {
    let Some(from) = statement.from.as_ref() else {
        return Ok(None);
    };
    let base = &from.base;
    if !from.joins.is_empty()
        || statement.set_body.is_some()
        || base.subquery.is_some()
        || base.is_function_source()
        || base.cte.is_some()
        || base.sample.is_some()
        || base.col_alias.is_some()
        || base.view_access.is_some()
        || base.lateral
    {
        return Ok(None);
    }
    let Some(crate::storage::ResolvedRelation::Table(slot)) =
        storage.resolve_relation(base.schema, base.table, txid)
    else {
        return Ok(None);
    };
    let table = storage.table_def(slot, txid);
    if table.kind != crate::storage::TableKind::Foreign
        || base.inheritance == RelationInheritance::Descendants
            && storage.relation_has_descendants(slot, txid)
    {
        return Ok(None);
    }
    let Some((_, foreign)) = storage.foreign_table(slot as u16, txid) else {
        return Ok(None);
    };
    let params = RemoteParams::new();
    let deparser = Deparser {
        table: &table,
        grouped: false,
        foreign: &foreign,
        exposed: base.alias.unwrap_or(base.table),
        arena,
        storage,
        txid,
        params: Some(&params),
    };
    if let Some(upper) = plan_upper(&deparser, slot, statement, arena)? {
        return Ok(Some(upper));
    }
    plan_with(&deparser, slot, statement, arena).map(Some)
}

/// The remote scan of `leaf`, a foreign member of the FROM item `item`
/// exposed as `exposed`, under `conditions`, the item's own restrictions:
/// postgres_fdw plans each foreign partition or child as a foreign table
/// scanned on its own, shipping the conditions it can (`classifyConditions`)
/// and keeping the rest as the member's filter.
pub(crate) fn member_scan<'a>(
    storage: &Storage,
    txid: u32,
    item: &TableRef<'a>,
    leaf: usize,
    exposed: &'a str,
    conditions: &[&'a Expr<'a>],
    arena: &'a Arena,
) -> Result<Option<ForeignSelectPlan<'a>>, SqlError> {
    let definition = storage.table_def(leaf, txid);
    let table = arena
        .alloc_str(definition.name.as_str())
        .map_err(|_| arena_full())?;
    let schema = arena
        .alloc_str(definition.schema.as_str())
        .map_err(|_| arena_full())?;
    let items = arena
        .alloc_slice_copy(&[SelectItem::Wildcard(
            crate::sql::ast::SourcePosition::UNKNOWN,
        )])
        .map_err(|_| arena_full())?;
    let statement = arena
        .alloc(Select {
            items,
            from: Some(crate::sql::ast::FromClause {
                base: TableRef {
                    schema: Some(schema),
                    table,
                    alias: Some(exposed),
                    inheritance: RelationInheritance::Only,
                    bound_table: None,
                    sample: None,
                    lateral: false,
                    ..*item
                },
                joins: &[],
            }),
            where_clause: conjoin(conditions, arena)?,
            ..Select::EMPTY
        })
        .map_err(|_| arena_full())?;
    plan_select(storage, txid, statement, arena)
}

/// `preprocess_minmax_aggregates` over a query of one foreign table: a
/// select list whose one aggregate is `min` or `max` of a shippable
/// argument, without grouping, reads that aggregate as an InitPlan, the
/// first row of the remote scan ordered by the argument with its nulls
/// excluded and the query's conditions applied. The result is the
/// FROM-less query reading that InitPlan. PostgreSQL's costs prefer that ordered `LIMIT 1` scan to a
/// remote aggregate only for one such aggregate: each further one adds
/// another remote query, and two or more ship as one grouped scan. `None`
/// when the query does not take that path.
pub(crate) fn minmax_subquery<'a>(
    storage: &Storage,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
) -> Result<Option<&'a Select<'a>>, SqlError> {
    let Some(from) = statement.from.as_ref() else {
        return Ok(None);
    };
    let base = &from.base;
    if !from.joins.is_empty()
        || statement.set_body.is_some()
        || !statement.with.is_empty()
        || base.subquery.is_some()
        || base.is_function_source()
        || base.cte.is_some()
        || base.sample.is_some()
        || base.col_alias.is_some()
        || base.view_access.is_some()
        || base.lateral
        || !statement.group_by.is_empty()
        || !statement.grouping_sets.is_empty()
        || statement.having.is_some()
        || !statement.windows.is_empty()
        || statement.distinct
        || !statement.distinct_on.is_empty()
        || !statement.locking.is_empty()
        || !statement.order_by.is_empty()
        || statement.where_clause.is_some_and(Expr::contains_subquery)
    {
        return Ok(None);
    }
    let Some(crate::storage::ResolvedRelation::Table(slot)) =
        storage.resolve_relation(base.schema, base.table, txid)
    else {
        return Ok(None);
    };
    let table = storage.table_def(slot, txid);
    if table.kind != crate::storage::TableKind::Foreign
        || base.inheritance == RelationInheritance::Descendants
            && storage.relation_has_descendants(slot, txid)
    {
        return Ok(None);
    }
    let Some((_, foreign)) = storage.foreign_table(slot as u16, txid) else {
        return Ok(None);
    };
    let deparser = Deparser {
        table: &table,
        grouped: false,
        foreign: &foreign,
        exposed: base.alias.unwrap_or(base.table),
        arena,
        storage,
        txid,
        params: None,
    };
    // The one aggregate, as many times as the select list repeats it.
    let mut found: Option<&'a Expr<'a>> = None;
    let mut eligible = true;
    for item in statement.items {
        let SelectItem::Expr { expression, .. } = item else {
            return Ok(None);
        };
        if expression.contains_subquery() || has_window_call(expression)? {
            return Ok(None);
        }
        crate::sql::query::walk_expression_tree(expression, &mut |node| {
            if !node.is_aggregate_use() {
                return;
            }
            let minmax = matches!(
                node,
                Expr::Call {
                    name: "min" | "max" | "pg_catalog.min" | "pg_catalog.max",
                    args: [_],
                    distinct: false,
                    order_by: [],
                    filter: None,
                    ..
                }
            );
            match found {
                _ if !minmax => eligible = false,
                Some(first) if *first != *node => eligible = false,
                _ => found = Some(node),
            }
        })?;
    }
    let (Some(aggregate), true) = (found, eligible) else {
        return Ok(None);
    };
    let Expr::Call {
        name,
        args: [argument],
        ..
    } = aggregate
    else {
        unreachable!("matched above")
    };
    let mut probe = RemoteText::new();
    if !deparser.try_write(&mut probe, argument)? {
        return Ok(None);
    }
    let descending = name.ends_with("max");
    let not_null = arena
        .alloc(Expr::IsNull {
            operand: argument,
            negated: true,
        })
        .map_err(|_| arena_full())?;
    let where_clause = match statement.where_clause {
        Some(condition) => &*arena
            .alloc(Expr::Binary {
                operator: BinaryOp::And,
                left: not_null,
                right: condition,
                source_position: crate::sql::ast::SourcePosition::UNKNOWN,
            })
            .map_err(|_| arena_full())?,
        None => not_null,
    };
    let items = arena
        .alloc_slice_copy(&[SelectItem::Expr {
            expression: argument,
            alias: None,
        }])
        .map_err(|_| arena_full())?;
    let order_by = arena
        .alloc_slice_copy(&[OrderBy {
            expression: argument,
            descending,
            nulls_first: descending,
        }])
        .map_err(|_| arena_full())?;
    let one = arena.alloc(Expr::Int(1)).map_err(|_| arena_full())?;
    let subquery = arena
        .alloc(Select {
            items,
            where_clause: Some(where_clause),
            order_by,
            limit: Some(one),
            offset: None,
            minmax_subquery: true,
            ..*statement
        })
        .map_err(|_| arena_full())?;
    let read = &*arena
        .alloc(Expr::Subquery(subquery))
        .map_err(|_| arena_full())?;
    // The select list reads the subquery's value where it read the
    // aggregate, under the names it had.
    let rewritten = arena
        .alloc_slice_copy(statement.items)
        .map_err(|_| arena_full())?;
    for item in rewritten.iter_mut() {
        let SelectItem::Expr { expression, alias } = item else {
            unreachable!("checked above")
        };
        *alias = Some(alias.unwrap_or_else(|| crate::sql::exec::derived_name(expression)));
        *expression = crate::sql::ast::rewrite_expression(
            expression,
            arena,
            &mut |node: &'a Expr<'a>| -> Shipped<Option<&'a Expr<'a>>> {
                Ok((*node == *aggregate).then_some(read))
            },
        )
        .map_err(|stop| match stop {
            Stop::Error(error) => error,
            Stop::Local => unreachable!("the rewrite never declines"),
        })?;
    }
    let outer = arena
        .alloc(Select {
            items: rewritten,
            from: None,
            where_clause: None,
            ..*statement
        })
        .map_err(|_| arena_full())?;
    Ok(Some(outer))
}

/// Whether `expression` holds an aggregate call outside a subquery.
fn has_aggregate(expression: &Expr<'_>) -> Result<bool, SqlError> {
    let mut found = false;
    crate::sql::query::walk_expression_tree(expression, &mut |node| {
        found |= node.is_aggregate_use();
    })?;
    Ok(found)
}

/// Whether `expression` reads a window function, which runs after grouping.
fn has_window_call(expression: &Expr<'_>) -> Result<bool, SqlError> {
    let mut found = false;
    crate::sql::query::walk_expression_tree(expression, &mut |node| {
        found |= matches!(node, Expr::Call { over: Some(_), .. });
    })?;
    Ok(found)
}

/// A grouped query's select list with each star expanded into the table's
/// columns, as parse analysis expands it. `None` for a whole-row reference.
fn expanded_items<'a>(
    deparser: &Deparser<'_>,
    statement: &'a Select<'a>,
    arena: &'a Arena,
) -> Result<Option<&'a Select<'a>>, SqlError> {
    let starred = |item: &SelectItem<'_>| {
        matches!(item, SelectItem::Wildcard(_) | SelectItem::TableWildcard(_))
    };
    if !statement.items.iter().any(starred) {
        return Ok(Some(statement));
    }
    let columns = deparser.table.columns();
    let mut items = ArenaList::new(arena);
    for item in statement.items {
        match item {
            SelectItem::Wildcard(_) => {}
            SelectItem::TableWildcard(qualifier) if *qualifier == deparser.exposed => {}
            SelectItem::Expr { .. } => {
                items.push(*item).map_err(|_| arena_full())?;
                continue;
            }
            SelectItem::TableWildcard(_) | SelectItem::RecordStar(_) => return Ok(None),
        }
        for column in columns {
            let name: &'a str = arena
                .alloc_str(column.name.as_str())
                .map_err(|_| arena_full())?;
            let expression = arena
                .alloc(Expr::Column {
                    qualifier: None,
                    name,
                })
                .map_err(|_| arena_full())?;
            items
                .push(SelectItem::Expr {
                    expression,
                    alias: Some(name),
                })
                .map_err(|_| arena_full())?;
        }
    }
    let expanded = arena
        .alloc(Select {
            items: items.as_slice(),
            ..*statement
        })
        .map_err(|_| arena_full())?;
    Ok(Some(expanded))
}

/// Whether `expression` is a literal the planner folds into a `Const`: a
/// quoted, numeric, boolean or `NULL` literal, a negated number, or a
/// quoted literal cast to a type.
pub(crate) fn is_constant(expression: &Expr<'_>) -> bool {
    matches!(
        unbound(expression),
        Expr::Int(_)
            | Expr::NumericLit(_)
            | Expr::Str(_)
            | Expr::Bool(_)
            | Expr::Null
            | Expr::Unary {
                operator: UnaryOp::Neg,
                operand: Expr::Int(_) | Expr::NumericLit(_),
                ..
            }
            | Expr::Cast {
                operand: Expr::Str(_),
                ..
            }
    )
}

/// Whether `node` is one of a grouped scan's remote `targets`, which a node
/// above it reads by reference. A column or a constant is never one: the
/// first prints as itself, and `fix_upper_expr` keeps the second.
pub(crate) fn reads_remote_target(targets: &[&Expr<'_>], node: &Expr<'_>) -> bool {
    !matches!(unbound(node), Expr::Column { .. })
        && !is_constant(node)
        && targets
            .iter()
            .any(|target| unbound(target) == unbound(node))
}

/// The grouped remote scan of a query of one foreign table
/// (`foreign_grouping_ok` for `add_foreign_grouping_paths`, then
/// `add_foreign_ordered_paths` and `add_foreign_final_paths`). Every
/// `GROUP BY` key ships, and the `WHERE` ships whole. The scan returns the
/// grouping target: the select list, then the `ORDER BY` and `GROUP BY` keys
/// it lacks. A target that ships is sent whole; any other is computed here
/// from the aggregates it reads, which must ship. A `HAVING` condition that
/// does not ship filters the remote rows here. `None` leaves grouping local.
///
/// With a `LIMIT` over `GROUP BY` PostgreSQL's costs prefer grouping the
/// remotely sorted rows here unless the order is by an aggregate or by an
/// expression only this side computes, and an aggregate without `GROUP BY`
/// keeps its `LIMIT` local; groups filtered here are grouped remotely.
fn plan_upper<'a>(
    table_deparser: &Deparser<'_>,
    slot: usize,
    statement: &'a Select<'a>,
    arena: &'a Arena,
) -> Result<Option<ForeignSelectPlan<'a>>, SqlError> {
    let deparser = Deparser {
        grouped: true,
        params: None,
        ..*table_deparser
    };
    let deparser = &deparser;
    let Some(statement) = expanded_items(deparser, statement, arena)? else {
        return Ok(None);
    };
    let mut aggregated = !statement.group_by.is_empty() || statement.having.is_some();
    for item in statement.items {
        let SelectItem::Expr { expression, .. } = item else {
            unreachable!("stars expanded above")
        };
        if expression.contains_subquery() || has_window_call(expression)? {
            return Ok(None);
        }
        aggregated |= has_aggregate(expression)?;
    }
    for key in statement.order_by {
        if key.expression.contains_subquery() || has_window_call(key.expression)? {
            return Ok(None);
        }
        aggregated |= has_aggregate(key.expression)?;
    }
    if !aggregated
        || !statement.grouping_sets.is_empty()
        || !statement.windows.is_empty()
        || statement.distinct
        || !statement.distinct_on.is_empty()
        || !statement.locking.is_empty()
        || statement.with_ties
        || statement.where_clause.is_some_and(Expr::contains_subquery)
        || statement.having.is_some_and(Expr::contains_subquery)
        || statement.group_by.iter().any(|key| key.contains_subquery())
    {
        return Ok(None);
    }
    // The planner moves a HAVING condition without aggregates or volatile
    // calls into WHERE when the query has GROUP BY.
    let having = conjuncts(statement.having, arena)?;
    let mut moved = statement.where_clause;
    let mut kept_having = ArenaList::new(arena);
    for &condition in having {
        if !statement.group_by.is_empty()
            && !has_aggregate(condition)?
            && condition.contains_volatile_function().is_none()
        {
            moved = Some(match moved {
                Some(earlier) => arena
                    .alloc(Expr::Binary {
                        operator: BinaryOp::And,
                        left: earlier,
                        right: condition,
                        source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                    })
                    .map_err(|_| arena_full())?,
                None => condition,
            });
        } else {
            kept_having.push(condition).map_err(|_| arena_full())?;
        }
    }
    let mut remote_where = RemoteText::new();
    if !remote_conditions(table_deparser, moved, &mut remote_where, arena)?.is_empty() {
        return Ok(None);
    }
    // The same expression, a table column however it is qualified.
    let same =
        |left: &Expr<'_>, right: &Expr<'_>| match (deparser.column(left), deparser.column(right)) {
            (Some(left), Some(right)) => left == right,
            (None, None) => unbound(left) == unbound(right),
            _ => false,
        };
    // A select-list position or output name, as GROUP BY and ORDER BY
    // resolve a bare key.
    let item_expression = |key: &'a Expr<'a>| -> &'a Expr<'a> {
        let item = |index: usize| match statement.items.get(index) {
            Some(SelectItem::Expr { expression, .. }) => Some(*expression),
            _ => None,
        };
        let named = match key {
            Expr::Int(position) => usize::try_from(*position)
                .ok()
                .and_then(|position| position.checked_sub(1))
                .and_then(item),
            Expr::Column {
                qualifier: None,
                name,
            } if deparser.column(key).is_none() => statement
                .items
                .iter()
                .position(|candidate| {
                    matches!(candidate, SelectItem::Expr { alias: Some(alias), .. } if alias == name)
                })
                .and_then(item),
            _ => None,
        };
        named.unwrap_or(key)
    };
    let mut probe = RemoteText::new();
    let mut ships = |expression: &Expr<'_>| -> Result<bool, SqlError> {
        probe.clear();
        if deparser.try_write(&mut probe, expression)? {
            return Ok(true);
        }
        deparser.try_write_target_constant(&mut probe, expression)
    };
    // The remote target list (`fdw_scan_tlist`) and the grouping target the
    // scan returns.
    let mut targets: ArenaList<'a, &'a Expr<'a>> = ArenaList::new(arena);
    let mut output: ArenaList<'a, &'a Expr<'a>> = ArenaList::new(arena);
    // The target each GROUP BY key names (`ressortgroupref`).
    let group_targets = arena
        .alloc_slice_with(statement.group_by.len(), |_| usize::MAX)
        .map_err(|_| arena_full())?;
    // `add_to_flat_tlist`: a target already listed is not repeated.
    fn flat<'a>(
        targets: &mut ArenaList<'a, &'a Expr<'a>>,
        expression: &'a Expr<'a>,
        same: &dyn Fn(&Expr<'_>, &Expr<'_>) -> bool,
    ) -> Result<usize, SqlError> {
        if let Some(at) = targets
            .as_slice()
            .iter()
            .position(|target| same(target, expression))
        {
            return Ok(at);
        }
        targets.push(expression).map_err(|_| arena_full())?;
        Ok(targets.len() - 1)
    }
    // A target that does not ship whole: the aggregates it reads must, and
    // are sent instead (`pull_var_clause` with `PVC_INCLUDE_AGGREGATES`).
    let pull = |targets: &mut ArenaList<'a, &'a Expr<'a>>,
                expression: &'a Expr<'a>,
                ships: &mut dyn FnMut(&Expr<'_>) -> Result<bool, SqlError>|
     -> Result<bool, SqlError> {
        let mut aggregates = ArenaList::new(arena);
        let mut full = false;
        crate::sql::query::walk_expression_tree(expression, &mut |node| {
            if node.is_aggregate_use() {
                full |= aggregates.push(node).is_err();
            }
        })?;
        if full {
            return Err(arena_full());
        }
        for &aggregate in aggregates.as_slice() {
            if !ships(aggregate)? {
                return Ok(false);
            }
            flat(targets, aggregate, &same)?;
        }
        Ok(true)
    };
    // One entry of the grouping target: a GROUP BY key is a target of its
    // own; anything else ships whole or through its aggregates.
    let entry = |targets: &mut ArenaList<'a, &'a Expr<'a>>,
                 group_targets: &mut [usize],
                 expression: &'a Expr<'a>,
                 grouping: Option<usize>,
                 ships: &mut dyn FnMut(&Expr<'_>) -> Result<bool, SqlError>|
     -> Result<bool, SqlError> {
        if let Some(at) = grouping {
            if !ships(expression)? {
                return Ok(false);
            }
            let target = targets.len();
            group_targets[at] = target;
            // Every key equal to this one names the same entry.
            for (other, key) in statement.group_by.iter().enumerate() {
                if group_targets[other] == usize::MAX && same(item_expression(key), expression) {
                    group_targets[other] = target;
                }
            }
            targets.push(expression).map_err(|_| arena_full())?;
            return Ok(true);
        }
        if ships(expression)? {
            flat(targets, expression, &same)?;
            return Ok(true);
        }
        pull(targets, expression, ships)
    };
    let grouping_key = |expression: &Expr<'_>, group_targets: &[usize]| {
        statement
            .group_by
            .iter()
            .enumerate()
            .filter(|(at, _)| group_targets[*at] == usize::MAX)
            .find(|(_, key)| same(item_expression(key), expression))
            .map(|(at, _)| at)
    };
    // `make_sort_input_target`: under ORDER BY, a volatile select-list
    // expression that is no sort or grouping key is computed after the
    // sort, and the grouping reads only the aggregates it needs, last.
    let mut postponed = ArenaList::new(arena);
    for item in statement.items {
        let SelectItem::Expr { expression, .. } = item else {
            unreachable!("stars expanded above")
        };
        output.push(*expression).map_err(|_| arena_full())?;
        let grouping = grouping_key(expression, group_targets);
        if grouping.is_none()
            && !statement.order_by.is_empty()
            && expression.contains_volatile_function().is_some()
            && !statement
                .order_by
                .iter()
                .any(|key| same(item_expression(key.expression), expression))
        {
            postponed.push(*expression).map_err(|_| arena_full())?;
            continue;
        }
        if !entry(
            &mut targets,
            group_targets,
            expression,
            grouping,
            &mut ships,
        )? {
            return Ok(None);
        }
    }
    for key in statement.order_by {
        let expression = item_expression(key.expression);
        if output
            .as_slice()
            .iter()
            .any(|listed| same(listed, expression))
        {
            continue;
        }
        let grouping = grouping_key(expression, group_targets);
        if !entry(
            &mut targets,
            group_targets,
            expression,
            grouping,
            &mut ships,
        )? {
            return Ok(None);
        }
        output.push(expression).map_err(|_| arena_full())?;
    }
    for (at, key) in statement.group_by.iter().enumerate() {
        if group_targets[at] != usize::MAX {
            continue;
        }
        let expression = item_expression(key);
        match targets
            .as_slice()
            .iter()
            .position(|target| same(target, expression))
        {
            Some(found) => group_targets[at] = found,
            None => {
                if !entry(
                    &mut targets,
                    group_targets,
                    expression,
                    Some(at),
                    &mut ships,
                )? {
                    return Ok(None);
                }
            }
        }
        output.push(expression).map_err(|_| arena_full())?;
    }
    for &expression in postponed.as_slice() {
        if !pull(&mut targets, expression, &mut ships)? {
            return Ok(None);
        }
    }
    // HAVING conditions ship, or filter the remote rows here.
    let mut remote_having = RemoteText::new();
    let mut local_having = ArenaList::new(arena);
    for &condition in kept_having.as_slice() {
        let separator = if remote_having.is_empty() {
            " HAVING "
        } else {
            " AND "
        };
        if deparser.try_write_condition(&mut remote_having, condition, separator)? {
            continue;
        }
        if !pull(&mut targets, condition, &mut ships)? {
            return Ok(None);
        }
        local_having.push(condition).map_err(|_| arena_full())?;
    }
    let targets = targets.as_slice();
    let filtered = !local_having.is_empty();
    // ORDER BY ships when every key is a remote target and nothing filters
    // the groups here.
    let mut order_targets = ArenaList::new(arena);
    for key in statement.order_by {
        let expression = item_expression(key.expression);
        if let Some(at) = targets.iter().position(|target| same(target, expression))
            && ships(expression)?
        {
            order_targets.push(at).map_err(|_| arena_full())?;
        }
    }
    // `adjust_foreign_grouping_path_cost`: a remote order by grouping keys
    // costs a small multiple of the grouping, any other a sort of every
    // group, which a local sort of the groups the filter keeps undercuts
    // unless a LIMIT reads only the first rows.
    let group_order = order_targets
        .as_slice()
        .iter()
        .all(|at| group_targets.contains(at));
    let order_pushed = !statement.order_by.is_empty()
        && order_targets.len() == statement.order_by.len()
        && (!filtered || group_order || statement.limit.is_some());
    // A postponed expression over rows sorted here needs a projection
    // above the sort, which this plan does not model.
    if !order_pushed && !postponed.is_empty() {
        return Ok(None);
    }
    // LIMIT and OFFSET: grouped rows with an order by aggregate ship them;
    // the order by grouping keys or none groups sorted rows here instead.
    let limited = statement.limit.is_some() || statement.offset.is_some();
    let mut limit_text = RemoteText::new();
    let mut limit_pushed = false;
    if limited && !statement.group_by.is_empty() {
        let by_aggregate = {
            let mut any = false;
            for &at in order_targets.as_slice() {
                any |= has_aggregate(targets[at])?;
            }
            any
        };
        // An order by an expression only this side computes cannot read
        // presorted groups.
        let computed_here = order_targets.len() != statement.order_by.len();
        if !by_aggregate && !computed_here && !filtered {
            return Ok(None);
        }
        // Rows filtered here keep their LIMIT here (`add_foreign_final_paths`).
        if (order_pushed || statement.order_by.is_empty()) && !filtered {
            limit_pushed = true;
            for (keyword, count) in [(" LIMIT ", statement.limit), (" OFFSET ", statement.offset)] {
                if let Some(count) = count {
                    let _ = limit_text.write_str(keyword);
                    match deparser.try_write_count(&mut limit_text, count) {
                        Ok(()) => {}
                        Err(Stop::Local) => return Ok(None),
                        Err(Stop::Error(error)) => return Err(error),
                    }
                }
            }
        }
    }
    let mut sql = RemoteText::new();
    let _ = sql.write_str("SELECT ");
    for (index, target) in targets.iter().enumerate() {
        if index != 0 {
            let _ = sql.write_str(", ");
        }
        if !ships(target)? {
            return Ok(None);
        }
        let _ = sql.write_str(probe_text(deparser, target)?.as_str());
    }
    let _ = sql.write_str(" FROM ");
    deparser.write_relation(&mut sql);
    let _ = sql.write_str(remote_where.as_str());
    for (index, &at) in group_targets.iter().enumerate() {
        let _ = write!(
            sql,
            "{}{}",
            if index == 0 { " GROUP BY " } else { ", " },
            at + 1
        );
    }
    let _ = sql.write_str(remote_having.as_str());
    if order_pushed {
        for (index, (&at, key)) in order_targets
            .as_slice()
            .iter()
            .zip(statement.order_by)
            .enumerate()
        {
            let _ = sql.write_str(if index == 0 { " ORDER BY " } else { ", " });
            if !deparser.try_write(&mut sql, targets[at])? {
                return Ok(None);
            }
            write_direction(&mut sql, key);
        }
    }
    let _ = sql.write_str(limit_text.as_str());
    if sql.is_truncated() || remote_where.is_truncated() || remote_having.is_truncated() {
        return Err(query_too_long());
    }
    // The local relation: one column per target.
    let mut columns = ArenaList::new(arena);
    for (index, target) in targets.iter().enumerate() {
        columns
            .push(match deparser.column(target) {
                Some(column) => table_deparser.table.columns()[column],
                None => {
                    let type_oid = match deparser.target_type(target)? {
                        Some(type_oid) => type_oid,
                        None => return Ok(None),
                    };
                    let ctype = crate::sql::exec::coltype_of_oid(type_oid).ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "a shipped grouped target has no modeled type"
                        )
                    })?;
                    let mut name = StackStr::<24>::new();
                    let _ = write!(name, "?target{}?", index + 1);
                    crate::storage::ColumnMeta {
                        name: crate::storage::SqlName::parse(name.as_str())?,
                        ctype,
                        type_mod: -1,
                        ..crate::storage::ColumnMeta::EMPTY
                    }
                }
            })
            .map_err(|_| arena_full())?;
    }
    let columns = columns.as_slice();
    // What the executor reads over the remote rows: each target through its
    // column, everything else computed here.
    let exposed: &'a str = arena
        .alloc_str(deparser.exposed)
        .map_err(|_| arena_full())?;
    let mut unbound_column = false;
    let mut local = |expression: &'a Expr<'a>| -> Result<&'a Expr<'a>, SqlError> {
        let rewritten = crate::sql::ast::rewrite_expression(
            expression,
            arena,
            &mut |node: &'a Expr<'a>| -> Shipped<Option<&'a Expr<'a>>> {
                if let Some(at) = targets.iter().position(|target| same(target, node)) {
                    let name: &'a str = arena
                        .alloc_str(columns[at].name.as_str())
                        .map_err(|_| arena_full())?;
                    let reference = arena
                        .alloc(Expr::Column {
                            qualifier: Some(exposed),
                            name,
                        })
                        .map_err(|_| arena_full())?;
                    return Ok(Some(&*reference));
                }
                // A column outside the grouping keys is the error local
                // grouping reports.
                unbound_column |= deparser.column(node).is_some();
                Ok(None)
            },
        );
        match rewritten {
            Ok(rewritten) => Ok(rewritten),
            Err(Stop::Error(error)) => Err(error),
            Err(Stop::Local) => unreachable!("the rewrite never declines"),
        }
    };
    let items = arena
        .alloc_slice_with(statement.items.len(), |_| statement.items[0])
        .map_err(|_| arena_full())?;
    for (index, item) in statement.items.iter().enumerate() {
        let SelectItem::Expr { expression, alias } = item else {
            unreachable!("stars expanded above")
        };
        items[index] = SelectItem::Expr {
            expression: local(expression)?,
            alias: Some(alias.unwrap_or_else(|| crate::sql::exec::derived_name(expression))),
        };
    }
    let mut local_where = None;
    for &condition in local_having.as_slice() {
        let condition = local(condition)?;
        local_where = Some(match local_where {
            Some(earlier) => &*arena
                .alloc(Expr::Binary {
                    operator: BinaryOp::And,
                    left: earlier,
                    right: condition,
                    source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                })
                .map_err(|_| arena_full())?,
            None => condition,
        });
    }
    let local_order: &'a [OrderBy<'a>] = if order_pushed {
        &[]
    } else {
        let keys = arena
            .alloc_slice_copy(statement.order_by)
            .map_err(|_| arena_full())?;
        for key in keys.iter_mut() {
            if !matches!(key.expression, Expr::Int(_)) {
                key.expression = local(item_expression(key.expression))?;
            }
        }
        keys
    };
    if unbound_column {
        return Ok(None);
    }
    let local_statement = arena
        .alloc(Select {
            items,
            where_clause: local_where,
            group_by: &[],
            having: None,
            order_by: local_order,
            limit: if limit_pushed { None } else { statement.limit },
            offset: if limit_pushed { None } else { statement.offset },
            ..*statement
        })
        .map_err(|_| arena_full())?;
    Ok(Some(ForeignSelectPlan {
        table_slot: slot,
        fetched: ColumnSet::from_low_word(0),
        sort_keys: &[],
        presorted: &[],
        local_where: None,
        order_pushed,
        limit_pushed,
        sql: arena.alloc_str(sql.as_str()).map_err(|_| arena_full())?,
        params: &[],
        upper: Some(ForeignUpper {
            output: output.as_slice(),
            targets,
            columns,
            local_having: conjoin(local_having.as_slice(), arena)?,
            local_order: if order_pushed {
                &[]
            } else {
                resolved_order(statement, arena)?
            },
            local: local_statement,
        }),
    }))
}

/// A remote target as the remote query writes it.
fn probe_text(deparser: &Deparser<'_>, target: &Expr<'_>) -> Result<RemoteText, SqlError> {
    let mut text = RemoteText::new();
    if deparser.try_write(&mut text, target)?
        || deparser.try_write_target_constant(&mut text, target)?
    {
        return Ok(text);
    }
    Err(sql_err!(
        sqlstate::INTERNAL_ERROR,
        "a shipped grouped target no longer ships"
    ))
}

fn plan_with<'a>(
    deparser: &Deparser<'_>,
    slot: usize,
    statement: &'a Select<'a>,
    arena: &'a Arena,
) -> Result<ForeignSelectPlan<'a>, SqlError> {
    let all = ColumnSet::all(deparser.table.n_columns);
    let mut remote_where = RemoteText::new();
    let kept = remote_conditions(deparser, statement.where_clause, &mut remote_where, arena)?;
    // The columns the local plan reads: its output, local conditions and
    // the clauses evaluated over the rows.
    let mut fetched = ColumnSet::from_low_word(0);
    let mut whole = false;
    let mut simple_items = true;
    for item in statement.items {
        match item {
            SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => whole = true,
            SelectItem::RecordStar(expression) | SelectItem::Expr { expression, .. } => {
                simple_items &= !calls_beyond_params(deparser, expression)?;
                whole |= read_columns(deparser, expression, &mut fetched).is_none();
            }
        }
    }
    macro_rules! read {
        ($expression:expr) => {
            if read_columns(deparser, $expression, &mut fetched).is_none() {
                whole = true;
            }
        };
    }
    for &condition in kept {
        read!(condition);
    }
    for expression in statement
        .group_by
        .iter()
        .chain(statement.distinct_on)
        .chain(statement.having.iter())
    {
        read!(expression);
    }
    // Every `WINDOW` definition's expressions are resjunk targets, used or
    // not.
    for window in statement.windows {
        for expression in window.partition_by {
            read!(expression);
        }
        for key in window.order_by {
            read!(key.expression);
        }
    }
    let mut aggregated = false;
    let mut window: Option<&WindowSpec<'a>> = None;
    let mut windows_differ = false;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
            aggregated |= has_aggregate(expression)?;
            crate::sql::query::walk_expression_tree(expression, &mut |node| {
                if let Expr::Call {
                    over: Some(spec), ..
                } = node
                {
                    match window {
                        Some(first) => windows_differ |= first != *spec,
                        None => window = Some(spec),
                    }
                }
            })?;
        }
    }
    let distinct = statement.distinct || !statement.distinct_on.is_empty();
    let grouped = !statement.group_by.is_empty()
        || !statement.grouping_sets.is_empty()
        || statement.having.is_some()
        || window.is_some()
        || distinct;
    // The order the remote rows arrive in (`query_pathkeys`): the query's
    // ORDER BY when the rows are final, else the order DISTINCT or the one
    // window clause reads its rows in, which the local plan then needs no
    // sort for. DISTINCT without ORDER BY and with a WHERE clause is hashed
    // instead, as PostgreSQL's costs choose.
    let mut order_redundant = false;
    let keys: Option<(&'a [OrderBy<'a>], bool)> = if aggregated
        || !statement.group_by.is_empty()
        || !statement.grouping_sets.is_empty()
        || statement.having.is_some()
        || windows_differ
    {
        None
    } else if let Some(spec) = window {
        (statement.order_by.is_empty() && !distinct && spec.base.is_none())
            .then(|| crate::sql::query::window_sort_keys(spec, arena).map(|keys| (keys, false)))
            .transpose()?
    } else if distinct {
        if statement.order_by.is_empty() && statement.where_clause.is_some() {
            None
        } else {
            distinct_keys(deparser, statement, arena)?.map(|keys| (keys, false))
        }
    } else if !statement.order_by.is_empty() && simple_items {
        // A key a `WHERE` equality fixes to a constant orders nothing (its
        // pathkey is redundant); with none left the rows need no order, and
        // the `LIMIT` stays local (`qp_is_pushdown_safe`).
        let fixed = constant_columns(deparser, statement.where_clause, arena)?;
        let ordered = resolved_order(statement, arena)?;
        let mut kept = ArenaList::new(arena);
        for &key in ordered {
            if !deparser
                .column(key.expression)
                .is_some_and(|column| fixed.contains(column))
            {
                kept.push(key).map_err(|_| arena_full())?;
            }
        }
        if kept.as_slice().is_empty() && !statement.with_ties {
            order_redundant = true;
            None
        } else {
            Some((kept.as_slice(), true))
        }
    } else {
        None
    };
    // The ORDER BY keys, which the scan returns for the sort whether or not
    // the remote query applies it.
    let sort_keys = resolved_order(statement, arena)?;
    for key in sort_keys {
        read!(key.expression);
    }
    let mut order_text = RemoteText::new();
    let mut order_shipped = keys.is_some();
    let order_params = deparser.param_mark();
    if let Some((keys, _)) = keys {
        for (index, key) in keys.iter().enumerate() {
            let _ = order_text.write_str(if index == 0 { " ORDER BY " } else { ", " });
            if !deparser.try_write(&mut order_text, key.expression)? {
                order_shipped = false;
                break;
            }
            write_direction(&mut order_text, key);
            read!(key.expression);
        }
    }
    // Without a LIMIT, the remote order is a path whose cost competes with
    // a local Sort of the unordered rows (`add_path`).
    if order_shipped
        && keys.is_some_and(|(_, final_order)| final_order)
        && statement.limit.is_none()
        && statement.offset.is_none()
        && local_sort_cheaper(deparser, statement, kept, sort_keys, arena)?
    {
        order_shipped = false;
    }
    if !order_shipped {
        order_text.clear();
        deparser.param_reset(order_params);
    }
    let order_pushed =
        order_redundant || order_shipped && keys.is_some_and(|(_, final_order)| final_order);
    let presorted: &'a [OrderBy<'a>] = match keys {
        Some((keys, false)) if order_shipped => keys,
        _ => &[],
    };
    // A locked row is copied whole, as a foreign table's row mark is.
    if whole || !statement.locking.is_empty() {
        fetched = all;
    }
    // LIMIT and OFFSET ship when the remote rows are the final rows.
    let mut limit_text = RemoteText::new();
    let limit_params = deparser.param_mark();
    let limit_pushed = (statement.limit.is_some() || statement.offset.is_some())
        && !statement.minmax_subquery
        && kept.is_empty()
        && !grouped
        && simple_items
        && !statement.with_ties
        && !order_redundant
        && (statement.order_by.is_empty() || order_pushed)
        && {
            let mut ships = true;
            for (keyword, count) in [(" LIMIT ", statement.limit), (" OFFSET ", statement.offset)] {
                if let Some(count) = count {
                    let _ = limit_text.write_str(keyword);
                    match deparser.try_write_count(&mut limit_text, count) {
                        Ok(()) => {}
                        Err(Stop::Local) => ships = false,
                        Err(Stop::Error(error)) => return Err(error),
                    }
                }
            }
            ships
        };
    if !limit_pushed {
        limit_text.clear();
        deparser.param_reset(limit_params);
    }
    let mut sql = RemoteText::new();
    let _ = sql.write_str("SELECT ");
    deparser.write_target_list(&mut sql, fetched);
    let _ = sql.write_str(" FROM ");
    deparser.write_relation(&mut sql);
    let _ = sql.write_str(remote_where.as_str());
    let _ = sql.write_str(order_text.as_str());
    let _ = sql.write_str(limit_text.as_str());
    if let Some(lock) = statement.locking.first() {
        let _ = sql.write_str(match lock.strength {
            LockStrength::Update | LockStrength::NoKeyUpdate => " FOR UPDATE",
            LockStrength::Share | LockStrength::KeyShare => " FOR SHARE",
        });
    }
    if sql.is_truncated() || remote_where.is_truncated() || order_text.is_truncated() {
        return Err(query_too_long());
    }
    Ok(ForeignSelectPlan {
        table_slot: slot,
        fetched,
        sort_keys,
        presorted,
        local_where: conjoin(kept, arena)?,
        order_pushed,
        limit_pushed,
        sql: arena.alloc_str(sql.as_str()).map_err(|_| arena_full())?,
        params: match deparser.params {
            Some(params) => {
                let mut roots = ArenaList::new(arena);
                for root in statement
                    .where_clause
                    .into_iter()
                    .chain(statement.limit)
                    .chain(statement.offset)
                    .chain(
                        keys.into_iter()
                            .flat_map(|(keys, _)| keys.iter().map(|key| key.expression)),
                    )
                {
                    roots.push(root).map_err(|_| arena_full())?;
                }
                params.nodes(roots.as_slice(), arena)?
            }
            None => &[],
        },
        upper: None,
    })
}

/// PostgreSQL's planner and postgres_fdw cost constants (`costsize.h`,
/// `postgres_fdw.c`), at their defaults.
const SEQ_PAGE_COST: f64 = 1.0;
const CPU_TUPLE_COST: f64 = 0.01;
const CPU_OPERATOR_COST: f64 = 0.0025;
const FDW_STARTUP_COST: f64 = 100.0;
const FDW_TUPLE_COST: f64 = 0.2;
const DEFAULT_FDW_SORT_MULTIPLIER: f64 = 1.2;
const STD_FUZZ_FACTOR: f64 = 1.01;
const DEFAULT_EQ_SEL: f64 = 0.005;
const DEFAULT_INEQ_SEL: f64 = 1.0 / 3.0;
const DEFAULT_RANGE_INEQ_SEL: f64 = 0.005;
const DEFAULT_UNK_SEL: f64 = 0.005;
const DEFAULT_NOT_UNK_SEL: f64 = 1.0 - DEFAULT_UNK_SEL;
/// `work_mem`'s default, in bytes: a larger sort spills, which this model
/// does not cost.
const WORK_MEM_BYTES: f64 = 4.0 * 1024.0 * 1024.0;

/// `get_typavgwidth` of a column.
fn average_width(column: &crate::storage::ColumnMeta) -> Option<f64> {
    let maximum = |type_mod: i32| -> Option<i32> {
        match column.ctype {
            ColType::Varchar | ColType::Bpchar if type_mod > 4 => Some((type_mod - 4) * 4 + 4),
            ColType::Numeric if type_mod >= 4 => {
                let precision = ((type_mod - 4) >> 16) & 0xffff;
                Some(8 + (precision + 6) / 4 * 2)
            }
            _ => None,
        }
    };
    Some(f64::from(match column.ctype {
        ColType::Bool => 1,
        ColType::Int2 => 2,
        ColType::Int4 | ColType::Float4 | ColType::Date => 4,
        ColType::Int8
        | ColType::Float8
        | ColType::Timestamp
        | ColType::Timestamptz
        | ColType::Time => 8,
        ColType::Interval | ColType::Uuid => 16,
        ColType::Text | ColType::Varchar | ColType::Bpchar | ColType::Numeric => {
            match maximum(column.type_mod) {
                Some(width) if column.ctype == ColType::Bpchar => width,
                Some(width) if width <= 32 => width,
                Some(width) if width < 1000 => 32 + (width - 32) / 2,
                Some(_) => 32 + (1000 - 32) / 2,
                None => 32,
            }
        }
        _ => return None,
    }))
}

/// `clamp_row_est`.
fn clamp_rows(rows: f64) -> f64 {
    if rows <= 1.0 { 1.0 } else { rows.round() }
}

impl Deparser<'_> {
    /// `clause_selectivity` of a condition over a never-analyzed table, with
    /// the default estimates `selfuncs.c` uses without statistics; `None`
    /// for a shape this model does not cover.
    fn selectivity(&self, condition: &Expr<'_>, arena: &Arena) -> Result<Option<f64>, SqlError> {
        let boolean = |expression: &Expr<'_>| {
            matches!(
                self.shape(expression),
                Ok(Shape::Value {
                    type_oid: oid::BOOL,
                    ..
                })
            )
        };
        let null = |expression: &Expr<'_>| matches!(unbound(expression), Expr::Null);
        Ok(match unbound(condition) {
            Expr::Binary {
                operator: BinaryOp::And,
                ..
            } => return self.conjunction_selectivity(conjuncts(Some(condition), arena)?, arena),
            Expr::Binary {
                operator: BinaryOp::Or,
                left,
                right,
                ..
            } => match (
                self.selectivity(left, arena)?,
                self.selectivity(right, arena)?,
            ) {
                (Some(left), Some(right)) => Some(left + right - left * right),
                _ => None,
            },
            Expr::Unary {
                operator: UnaryOp::Not,
                operand,
                ..
            } => self
                .selectivity(operand, arena)?
                .map(|selectivity| 1.0 - selectivity),
            Expr::Binary {
                operator: BinaryOp::Eq | BinaryOp::NotEq,
                left,
                right,
                ..
            } if null(left) || null(right) => None,
            Expr::Binary {
                operator: operator @ (BinaryOp::Eq | BinaryOp::NotEq),
                left,
                right,
                ..
            } => {
                // A boolean has two distinct values.
                let equal = if boolean(left) || boolean(right) {
                    0.5
                } else {
                    DEFAULT_EQ_SEL
                };
                Some(if *operator == BinaryOp::Eq {
                    equal
                } else {
                    1.0 - equal
                })
            }
            Expr::Binary {
                operator: BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq,
                ..
            } => Some(DEFAULT_INEQ_SEL),
            Expr::IsNull { negated, .. } => Some(if *negated {
                DEFAULT_NOT_UNK_SEL
            } else {
                DEFAULT_UNK_SEL
            }),
            Expr::InList {
                operand,
                list,
                negated,
            } => {
                let equal = if boolean(operand) {
                    0.5
                } else {
                    DEFAULT_EQ_SEL
                };
                // `scalararraysel`: a NULL element matches nothing.
                let items = list.iter().filter(|item| !null(item)).count() as f64;
                if list.len() == 1 {
                    Some(match (*negated, items == 1.0) {
                        (false, true) => equal,
                        (true, true) => 1.0 - equal,
                        _ => return Ok(None),
                    })
                } else if *negated {
                    if items < list.len() as f64 {
                        Some(0.0)
                    } else {
                        let disjoint = 1.0 - items * equal;
                        Some(if (0.0..=1.0).contains(&disjoint) {
                            disjoint
                        } else {
                            (1.0 - equal).powf(items)
                        })
                    }
                } else {
                    let disjoint = items * equal;
                    Some(if (0.0..=1.0).contains(&disjoint) {
                        disjoint
                    } else {
                        1.0 - (1.0 - equal).powf(items)
                    })
                }
            }
            expression if self.column(expression).is_some() && boolean(expression) => Some(0.5),
            _ => None,
        })
    }

    /// `clauselist_selectivity` of AND-ed conditions: the product, with an
    /// upper and a lower bound on the same expression paired as a range
    /// (`addRangeClause`), which defaults to `DEFAULT_RANGE_INEQ_SEL`.
    fn conjunction_selectivity(
        &self,
        conditions: &[&Expr<'_>],
        arena: &Arena,
    ) -> Result<Option<f64>, SqlError> {
        let constant = |expression: &Expr<'_>| {
            matches!(self.shape(expression), Ok(Shape::Literal(_))) || self.is_param(expression)
        };
        // (bounded expression, has lower, has upper)
        let mut ranges: [(Option<&Expr<'_>>, bool, bool); 32] = [(None, false, false); 32];
        let mut range_count = 0;
        let mut product = 1.0;
        for &condition in conditions {
            let bound = match unbound(condition) {
                Expr::Binary {
                    operator:
                        operator @ (BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq),
                    left,
                    right,
                    ..
                } if constant(right) && !constant(left) => {
                    Some((*left, matches!(operator, BinaryOp::Gt | BinaryOp::GtEq)))
                }
                Expr::Binary {
                    operator:
                        operator @ (BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq),
                    left,
                    right,
                    ..
                } if constant(left) && !constant(right) => {
                    Some((*right, matches!(operator, BinaryOp::Lt | BinaryOp::LtEq)))
                }
                _ => None,
            };
            if let Some((bounded, lower)) = bound {
                let at = match ranges[..range_count]
                    .iter()
                    .position(|(held, ..)| held.is_some_and(|held| *held == *bounded))
                {
                    Some(at) => at,
                    None if range_count < ranges.len() => {
                        ranges[range_count].0 = Some(bounded);
                        range_count += 1;
                        range_count - 1
                    }
                    None => return Ok(None),
                };
                if lower {
                    ranges[at].1 = true;
                } else {
                    ranges[at].2 = true;
                }
                continue;
            }
            match self.selectivity(condition, arena)? {
                Some(selectivity) => product *= selectivity,
                None => return Ok(None),
            }
        }
        for &(_, lower, upper) in &ranges[..range_count] {
            product *= if lower && upper {
                DEFAULT_RANGE_INEQ_SEL
            } else {
                DEFAULT_INEQ_SEL
            };
        }
        Ok(Some(product))
    }
}

/// Whether PostgreSQL's planner sorts a foreign scan's rows locally rather
/// than taking postgres_fdw's ordered path, for a query without `LIMIT`:
/// `estimate_path_cost_size` without remote estimates prices a remote sort
/// at 1.2 times the scan's run cost, `cost_sort` the local one, and
/// `add_path` keeps the ordered path unless it costs more than the sorted
/// one by its fuzz factor. A never-analyzed table is ten pages of rows as
/// wide as the columns the query reads. `false` when a condition's
/// selectivity is outside this model, which keeps the remote order.
fn local_sort_cheaper(
    deparser: &Deparser<'_>,
    statement: &Select<'_>,
    kept: &[&Expr<'_>],
    sort_keys: &[OrderBy<'_>],
    arena: &Arena,
) -> Result<bool, SqlError> {
    // The columns the scan returns above it.
    let mut needed = ColumnSet::from_low_word(0);
    for item in statement.items {
        match item {
            SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => {
                needed = ColumnSet::all(deparser.table.n_columns)
            }
            SelectItem::RecordStar(expression) | SelectItem::Expr { expression, .. } => {
                if read_columns(deparser, expression, &mut needed).is_none() {
                    return Ok(false);
                }
            }
        }
    }
    for key in sort_keys {
        if read_columns(deparser, key.expression, &mut needed).is_none() {
            return Ok(false);
        }
    }
    let mut width = 0.0;
    for (column, definition) in deparser.table.columns().iter().enumerate() {
        if needed.contains(column) {
            match average_width(definition) {
                Some(column_width) => width += column_width,
                None => return Ok(false),
            }
        }
    }
    // `10 * BLCKSZ / (width + MAXALIGN(SizeofHeapTupleHeader))`.
    let tuples = (81_920.0 / (width + 24.0)).floor();
    // Pseudoconstant conditions gate the scan and do not restrict it.
    let all = conjuncts(statement.where_clause, arena)?;
    let mut restricting = ArenaList::new(arena);
    let mut cost_units = 0.0;
    let columns = crate::sql::exec::AliasedDefCols {
        definition: crate::storage::RelationDefinition::from_definition(deparser.table),
        alias: (deparser.exposed != deparser.table.name.as_str()).then_some(deparser.exposed),
        output_aliases: &[],
        remote_ctid: false,
    };
    for &condition in all {
        if deparser.reads_column(condition)? {
            restricting.push(condition).map_err(|_| arena_full())?;
            cost_units += crate::sql::query::qual_cost(
                condition,
                &columns,
                deparser.storage,
                deparser.txid,
                arena,
            );
        }
    }
    let (Some(selectivity), Some(local)) = (
        deparser.conjunction_selectivity(restricting.as_slice(), arena)?,
        deparser.conjunction_selectivity(kept, arena)?,
    ) else {
        return Ok(false);
    };
    let rows = clamp_rows(tuples * selectivity);
    let retrieved = clamp_rows(rows / local).min(tuples);
    let run =
        SEQ_PAGE_COST * 10.0 + (CPU_TUPLE_COST + cost_units * CPU_OPERATOR_COST / 2.0) * tuples;
    let transfer = FDW_STARTUP_COST + (FDW_TUPLE_COST + CPU_TUPLE_COST) * retrieved;
    let ordered = DEFAULT_FDW_SORT_MULTIPLIER * run + transfer;
    let sort_rows = rows.max(2.0);
    // `relation_byte_size`: MAXALIGN(width) plus the tuple header.
    if sort_rows * ((width / 8.0).ceil() * 8.0 + 24.0) > WORK_MEM_BYTES {
        return Ok(false);
    }
    let sorted = run
        + transfer
        + 2.0 * CPU_OPERATOR_COST * sort_rows * sort_rows.log2()
        + CPU_OPERATOR_COST * sort_rows;
    Ok(ordered > sorted * STD_FUZZ_FACTOR)
}

/// What a foreign write's `RETURNING` list returns
/// (`deparseReturningList`): the columns the local statement reads, in table
/// order, then the remote row's `ctid` when the statement reads it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteReturning {
    pub columns: ColumnSet,
    pub ctid: bool,
}

impl RemoteReturning {
    pub(crate) const NONE: Self = Self {
        columns: ColumnSet::EMPTY,
        ctid: false,
    };

    pub(crate) fn is_empty(self) -> bool {
        self.columns.is_empty() && !self.ctid
    }

    /// The fields each returned row carries.
    pub(crate) fn fields(self) -> usize {
        self.columns.count_ones() as usize + usize::from(self.ctid)
    }

    /// ` RETURNING col, ..., ctid`, nothing when the list is empty.
    pub(crate) fn write(self, deparser: &Deparser<'_>, output: &mut impl core::fmt::Write) {
        if self.is_empty() {
            return;
        }
        let _ = output.write_str(" RETURNING ");
        if self.columns.is_empty() {
            let _ = output.write_str("ctid");
            return;
        }
        deparser.write_target_list(output, self.columns);
        if self.ctid {
            let _ = output.write_str(", ctid");
        }
    }
}

/// postgres_fdw's direct modification (`postgresPlanDirectModify`): an
/// `UPDATE` (with `assignments`) or `DELETE` of one foreign table sent as one
/// remote statement, when every condition and assigned value ships and no
/// local trigger or generated column needs the rows. `returned` lists the
/// columns the local `RETURNING` list reads, which `deparseReturningList`
/// appends. `None` when it does not apply.
pub(crate) fn direct_modify_sql<'a>(
    deparser: &Deparser<'_>,
    assignments: Option<&[(&str, &'a Expr<'a>)]>,
    predicate: Option<&'a Expr<'a>>,
    returned: RemoteReturning,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let mut conditions = RemoteText::new();
    if !remote_conditions(deparser, predicate, &mut conditions, arena)?.is_empty() {
        return Ok(None);
    }
    let mut sql = RemoteText::new();
    match assignments {
        Some(assignments) => {
            let _ = sql.write_str("UPDATE ");
            deparser.write_relation(&mut sql);
            let _ = sql.write_str(" SET ");
            let columns = deparser.table.columns();
            if columns.iter().any(|column| {
                matches!(
                    column.default,
                    crate::storage::ColumnDefault::Generated { .. }
                )
            }) || assignments
                .iter()
                .any(|(name, _)| !columns.iter().any(|column| column.name.as_str() == *name))
            {
                return Ok(None);
            }
            // `deparseDirectUpdateSql` lists the targets in column order.
            let mut first = true;
            for (column, definition) in columns.iter().enumerate() {
                let Some(&(_, value)) = assignments
                    .iter()
                    .find(|(name, _)| *name == definition.name.as_str())
                else {
                    continue;
                };
                if !first {
                    let _ = sql.write_str(", ");
                }
                first = false;
                deparser.write_column(&mut sql, column);
                let _ = sql.write_str(" = ");
                if !deparser.try_write_assigned(&mut sql, value, column)? {
                    return Ok(None);
                }
            }
        }
        None => {
            let _ = sql.write_str("DELETE FROM ");
            deparser.write_relation(&mut sql);
        }
    }
    let _ = sql.write_str(conditions.as_str());
    returned.write(deparser, &mut sql);
    if sql.is_truncated() || conditions.is_truncated() {
        return Err(query_too_long());
    }
    Ok(Some(
        arena.alloc_str(sql.as_str()).map_err(|_| arena_full())?,
    ))
}

/// The row-by-row form of a foreign `UPDATE` or `DELETE`
/// (`postgresPlanForeignModify`): a scan that locks the rows the shippable
/// conditions select, and the command each row's `ctid` then runs.
pub(crate) struct ForeignModifyScan<'a> {
    /// `SELECT ... ctid FROM ... FOR UPDATE`.
    pub scan_sql: &'a str,
    /// The columns the scan reads, before its `ctid`.
    pub fetched: ColumnSet,
    /// The conditions evaluated locally on each scanned row.
    pub local_where: Option<&'a Expr<'a>>,
    /// `UPDATE ... SET col = $2 ... WHERE ctid = $1` or
    /// `DELETE ... WHERE ctid = $1`, with its `RETURNING` list.
    pub modify_sql: &'a str,
    /// What the command returns.
    pub returned: RemoteReturning,
}

/// Plans the row-by-row form. `updated` lists an UPDATE's assigned columns
/// (`None` for a DELETE); `returned` the columns the statement needs back;
/// `whole_old` whether local work needs every column of the old row.
pub(crate) fn plan_modify_scan<'a>(
    deparser: &Deparser<'_>,
    updated: Option<ColumnSet>,
    predicate: Option<&'a Expr<'a>>,
    returned: RemoteReturning,
    whole_old: bool,
    arena: &'a Arena,
) -> Result<ForeignModifyScan<'a>, SqlError> {
    let all = ColumnSet::all(deparser.table.n_columns);
    let mut conditions = RemoteText::new();
    let kept = remote_conditions(deparser, predicate, &mut conditions, arena)?;
    // An UPDATE reads the whole old row to form the new one; a DELETE only
    // what its local conditions read.
    let mut fetched = ColumnSet::from_low_word(0);
    let mut whole = whole_old || updated.is_some();
    for condition in kept {
        whole |= read_columns(deparser, condition, &mut fetched).is_none();
    }
    if whole {
        fetched = all;
    }
    let mut scan = RemoteText::new();
    let _ = scan.write_str("SELECT ");
    if deparser.write_target_list(&mut scan, fetched) {
        let _ = scan.write_str(", ctid");
    } else {
        scan.clear();
        let _ = scan.write_str("SELECT ctid");
    }
    let _ = scan.write_str(" FROM ");
    deparser.write_relation(&mut scan);
    let _ = scan.write_str(conditions.as_str());
    let _ = scan.write_str(" FOR UPDATE");
    let mut modify = RemoteText::new();
    match updated {
        Some(columns) => {
            let _ = modify.write_str("UPDATE ");
            deparser.write_relation(&mut modify);
            let _ = modify.write_str(" SET ");
            // `deparseUpdateSql` over the updated columns and the stored
            // generated columns that depend on them, in column order; a
            // generated column is set to DEFAULT.
            let generated = dependent_generated_columns(deparser.table, columns, arena)?;
            let mut parameter = 2;
            let mut first = true;
            for column in (0..deparser.table.n_columns)
                .filter(|&column| columns.contains(column) || generated.contains(column))
            {
                if !first {
                    let _ = modify.write_str(", ");
                }
                first = false;
                deparser.write_column(&mut modify, column);
                if deparser.table.columns()[column].default.is_generated() {
                    let _ = modify.write_str(" = DEFAULT");
                } else {
                    let _ = write!(modify, " = ${parameter}");
                    parameter += 1;
                }
            }
        }
        None => {
            let _ = modify.write_str("DELETE FROM ");
            deparser.write_relation(&mut modify);
        }
    }
    let _ = modify.write_str(" WHERE ctid = $1");
    returned.write(deparser, &mut modify);
    if scan.is_truncated() || modify.is_truncated() || conditions.is_truncated() {
        return Err(query_too_long());
    }
    Ok(ForeignModifyScan {
        scan_sql: arena.alloc_str(scan.as_str()).map_err(|_| arena_full())?,
        fetched,
        local_where: conjoin(kept, arena)?,
        modify_sql: arena.alloc_str(modify.as_str()).map_err(|_| arena_full())?,
        returned,
    })
}

/// `get_dependent_generated_columns`: the stored generated columns whose
/// expressions read a column of `updated`.
fn dependent_generated_columns(
    table: &crate::storage::TableDef,
    updated: ColumnSet,
    arena: &Arena,
) -> Result<ColumnSet, SqlError> {
    let mut dependent = ColumnSet::EMPTY;
    for (index, column) in table.columns().iter().enumerate() {
        let crate::storage::ColumnDefault::Generated {
            expression,
            kind: crate::storage::Generation::Stored,
        } = &column.default
        else {
            continue;
        };
        let parsed = crate::sql::parser::parse_expr(expression.as_str(), arena).map_err(|_| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "stored generation expression does not parse"
            )
        })?;
        if expression_reads(parsed, table, updated)? {
            dependent.insert(index);
        }
    }
    Ok(dependent)
}

fn expression_reads(
    expression: &Expr<'_>,
    table: &crate::storage::TableDef,
    columns: ColumnSet,
) -> Result<bool, SqlError> {
    if let Expr::Column { name, .. } = expression
        && table
            .column_index(name)
            .is_some_and(|index| columns.contains(index))
    {
        return Ok(true);
    }
    let mut found = false;
    crate::sql::query::walk_children(expression, &mut |child| {
        if !found {
            found = expression_reads(child, table, columns)?;
        }
        Ok(())
    })?;
    Ok(found)
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

    fn constant(text: &str, type_oid: i32, show: i8) -> String {
        let mut output = String::new();
        assert!(Deparser::write_constant_text(&mut output, text, type_oid, show).is_ok());
        output
    }

    #[test]
    fn constants_follow_deparse_const() {
        assert_eq!(constant("50", oid::INT4, -2), "50");
        assert_eq!(constant("-5", oid::INT4, 0), "(-5)");
        assert_eq!(constant("3", oid::INT8, 0), "3::bigint");
        assert_eq!(constant("5", oid::NUMERIC, -2), "5::numeric");
        assert_eq!(constant("1.5", oid::NUMERIC, 0), "1.5");
        assert_eq!(constant("NaN", oid::NUMERIC, 0), "'NaN'::numeric");
        assert_eq!(constant("0.5", oid::FLOAT8, -2), "0.5::double precision");
        assert_eq!(constant("2024-01-05", oid::DATE, -2), "'2024-01-05'");
        assert_eq!(constant("2024-01-05", oid::DATE, 0), "'2024-01-05'::date");
        assert_eq!(constant("a'b\\c", oid::TEXT, -2), "E'a''b\\\\c'");
        assert_eq!(constant("true", oid::BOOL, 0), "true");
    }

    #[test]
    fn array_elements_quote_as_array_out() {
        let mut output = String::new();
        write_array_element(&mut output, "a b");
        write_array_element(&mut output, "null");
        write_array_element(&mut output, "x\"y");
        write_array_element(&mut output, "plain");
        assert_eq!(output, "\"a b\"\"null\"\"x\\\"y\"plain");
    }
}
