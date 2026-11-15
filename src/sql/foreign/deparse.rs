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

use crate::mem::arena::Arena;
use crate::sql::ast::{
    BinaryOp, Expr, LockStrength, OrderBy, RelationInheritance, Select, SelectItem, UnaryOp,
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
            _ => return None,
        })
    }

    /// Classifies `expression`, or reports that it does not ship. Shapes:
    /// table columns; untyped, integer, decimal, boolean, `NULL` and cast
    /// string literals; comparison, arithmetic and `||` operators;
    /// `AND`/`OR`/`NOT`; `IS [NOT] NULL`; `[NOT] IN` over constants;
    /// `[NOT] [I]LIKE` without `ESCAPE`; and the POSIX regex operators.
    fn shape<'e>(&self, expression: &'e Expr<'e>) -> Shipped<Shape<'e>> {
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
            Literal::Typed(_, type_oid) => type_oid,
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
        if self.column(expression).is_some() || Self::literal(expression).is_some() {
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
            Literal::Typed(_, type_oid)
                if type_oid == partner
                    || integer_rank(type_oid).is_some() && integer_rank(partner).is_some() =>
            {
                Ok(type_oid)
            }
            Literal::Bool(_) | Literal::Typed(..) => Err(Stop::Local),
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
            Shape::Literal(Literal::Typed(_, oid::TEXT)) => Ok(()),
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
                    let source = source_text(source, &mut digits)?;
                    let value =
                        crate::sql::eval::cast_to(Datum::Text(source), element_type, self.arena)
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
                Ok(false)
            }
            Err(Stop::Error(error)) => Err(error),
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
    fn try_write_count<const N: usize>(
        &self,
        output: &mut StackStr<N>,
        expression: &Expr<'_>,
    ) -> Shipped<()> {
        let literal = match expression {
            Expr::Int(_) | Expr::Unary { .. } => self.integer(expression).ok_or(Stop::Local)?,
            Expr::Null => Literal::Null,
            _ => return Err(Stop::Local),
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
/// which may make the query grouped or set-returning.
fn calls(expression: &Expr<'_>) -> bool {
    expression.find_function(&|_, _| true).is_some() || expression.contains_subquery()
}

/// The remote scan of a `SELECT` reading one foreign table.
pub(crate) struct ForeignSelectPlan<'a> {
    pub table_slot: usize,
    /// The columns the remote query returns, in table order.
    pub fetched: ColumnSet,
    /// The table columns `ORDER BY` keys name, in key order, which the scan
    /// returns for the sort whether or not the remote query applies it.
    pub sort_columns: &'a [usize],
    /// The conditions evaluated locally after the rows arrive.
    pub local_where: Option<&'a Expr<'a>>,
    pub order_pushed: bool,
    pub limit_pushed: bool,
    pub sql: &'a str,
}

impl<'a> ForeignSelectPlan<'a> {
    /// The statement the local executor runs over the remote rows: without
    /// the conditions, order and limit the remote query applies.
    pub(crate) fn local_statement(
        &self,
        statement: &'a Select<'a>,
        arena: &'a Arena,
    ) -> Result<&'a Select<'a>, SqlError> {
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
        if deparser.try_write_condition(&mut probe, condition, "")? {
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

/// The ORDER BY key's table column, through an output position or name.
fn order_column(
    deparser: &Deparser<'_>,
    statement: &Select<'_>,
    key: &OrderBy<'_>,
) -> Option<usize> {
    let item = |index: usize| match statement.items.get(index) {
        Some(SelectItem::Expr { expression, .. }) => deparser.column(expression),
        _ => None,
    };
    match key.expression {
        Expr::Int(position) => usize::try_from(*position)
            .ok()?
            .checked_sub(1)
            .and_then(item),
        Expr::Column {
            qualifier: None,
            name,
        } => {
            let named = statement.items.iter().position(|candidate| {
                matches!(candidate, SelectItem::Expr { alias: Some(alias), .. } if alias == name)
            });
            match named {
                Some(index) => item(index),
                None => deparser.column(key.expression),
            }
        }
        expression => deparser.column(expression),
    }
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
    let deparser = Deparser {
        table: &table,
        foreign: &foreign,
        exposed: base.alias.unwrap_or(base.table),
        arena,
        storage,
        txid,
    };
    plan_with(&deparser, slot, statement, arena).map(Some)
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
                simple_items &= !calls(expression);
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
    for window in statement.windows {
        for expression in window.partition_by {
            read!(expression);
        }
        for key in window.order_by {
            read!(key.expression);
        }
    }
    let grouped = !statement.group_by.is_empty()
        || !statement.grouping_sets.is_empty()
        || statement.having.is_some()
        || !statement.windows.is_empty()
        || statement.distinct
        || !statement.distinct_on.is_empty();
    // ORDER BY ships when every key is a column of the table.
    let mut ordered = ColumnSet::from_low_word(0);
    let sort_columns = arena
        .alloc_slice_with(statement.order_by.len(), |_| 0usize)
        .map_err(|_| arena_full())?;
    let mut sort_count = 0;
    let mut order_text = RemoteText::new();
    let mut order_pushed = !statement.order_by.is_empty() && !grouped && simple_items;
    for (index, key) in statement.order_by.iter().enumerate() {
        let column = order_column(deparser, statement, key);
        if let Some(column) = column
            && !sort_columns[..sort_count].contains(&column)
        {
            sort_columns[sort_count] = column;
            sort_count += 1;
        }
        match column {
            Some(column) if order_pushed && deparser.column_type(column).is_ok() => {
                ordered.insert(column);
                let _ = order_text.write_str(if index == 0 { " ORDER BY " } else { ", " });
                deparser.write_column(&mut order_text, column);
                let _ = order_text.write_str(if key.descending { " DESC" } else { " ASC" });
                let _ = order_text.write_str(if key.nulls_first {
                    " NULLS FIRST"
                } else {
                    " NULLS LAST"
                });
            }
            Some(column) => {
                order_pushed = false;
                fetched.insert(column);
            }
            None => {
                order_pushed = false;
                read!(key.expression);
            }
        }
    }
    if !order_pushed {
        order_text.clear();
    }
    for column in (0..deparser.table.n_columns).filter(|&column| ordered.contains(column)) {
        fetched.insert(column);
    }
    // A locked row is copied whole, as a foreign table's row mark is.
    if whole || !statement.locking.is_empty() {
        fetched = all;
    }
    // LIMIT and OFFSET ship when the remote rows are the final rows.
    let mut limit_text = RemoteText::new();
    let limit_pushed = (statement.limit.is_some() || statement.offset.is_some())
        && kept.is_empty()
        && !grouped
        && simple_items
        && !statement.with_ties
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
        sort_columns: &sort_columns[..sort_count],
        local_where: conjoin(kept, arena)?,
        order_pushed,
        limit_pushed,
        sql: arena.alloc_str(sql.as_str()).map_err(|_| arena_full())?,
    })
}

/// postgres_fdw's direct modification (`postgresPlanDirectModify`): an
/// `UPDATE` (with `assignments`) or `DELETE` of one foreign table sent as one
/// remote statement, when every condition and assigned value ships and no
/// local trigger or generated column needs the rows. `None` when it does not
/// apply.
pub(crate) fn direct_modify_sql<'a>(
    deparser: &Deparser<'_>,
    assignments: Option<&[(&str, &'a Expr<'a>)]>,
    predicate: Option<&'a Expr<'a>>,
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
    /// The columns the command returns, in table order.
    pub returned: ColumnSet,
}

/// Plans the row-by-row form. `updated` lists an UPDATE's assigned columns
/// (`None` for a DELETE); `returned` the columns the statement needs back;
/// `whole_old` whether local work needs every column of the old row.
pub(crate) fn plan_modify_scan<'a>(
    deparser: &Deparser<'_>,
    updated: Option<ColumnSet>,
    predicate: Option<&'a Expr<'a>>,
    returned: ColumnSet,
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
            let assigned = (0..deparser.table.n_columns).filter(|&column| columns.contains(column));
            for (parameter, column) in (2..).zip(assigned) {
                if parameter != 2 {
                    let _ = modify.write_str(", ");
                }
                deparser.write_column(&mut modify, column);
                let _ = write!(modify, " = ${parameter}");
            }
        }
        None => {
            let _ = modify.write_str("DELETE FROM ");
            deparser.write_relation(&mut modify);
        }
    }
    let _ = modify.write_str(" WHERE ctid = $1");
    if !returned.is_empty() {
        let _ = modify.write_str(" RETURNING ");
        deparser.write_target_list(&mut modify, returned);
    }
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
