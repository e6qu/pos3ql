//! PostgreSQL's `ruleutils.c` spelling of stored scalar expressions.
//!
//! Column defaults, CHECK constraints and routine parameter defaults are stored
//! as their SQL source. PostgreSQL prints the analyzed form instead: literals
//! carry the type the parser resolved (`'x'::text`), operators and boolean
//! connectives are parenthesized, coercions that change representation are
//! written as casts and SQL value functions are spelled in upper case. This
//! module reproduces that for the expression shapes it can type; any other
//! shape is reported as unsupported so callers keep the stored spelling.
//! Source: <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/ruleutils.c>
//! (`get_rule_expr`, `get_const_expr`, `get_oper_expr`, `get_func_expr`).

use super::{Arena, SqlError, Storage, arena_full, write_identifier};
use crate::mem::arena::ArenaString;
use crate::sql::ast::{BinaryOp, CallKind, Expr, UnaryOp};
use crate::sql::types::{ColType, oid};
use crate::storage::TableDef;
use core::fmt::Write;

/// Where an expression's value is delivered.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Destination {
    /// Coerced to this type by assignment (column and parameter defaults);
    /// PostgreSQL does not print that final coercion.
    Assignment(i32),
    /// A boolean condition (CHECK, policy and trigger qualifications).
    Condition,
}

pub(crate) struct Context<'s> {
    pub storage: &'s Storage,
    pub txid: u32,
    pub table: Option<&'s TableDef>,
    /// Named parameters of the routine whose body or default is printed.
    pub parameters: &'s [crate::storage::RoutineParameterDef],
}

/// Deparses `source`, or returns `None` when its shape is outside this
/// module's typed coverage.
pub(crate) fn deparse<'a>(
    context: &Context<'_>,
    source: &str,
    destination: Destination,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let Ok(expression) = crate::sql::parser::parse_expr(source, arena) else {
        return Ok(None);
    };
    let mut output = Output {
        arena,
        text: ArenaString::new(arena),
        supported: true,
    };
    match destination {
        Destination::Assignment(target) => context.assigned(expression, target, &mut output)?,
        Destination::Condition => context.write(expression, Some(oid::BOOL), &mut output)?,
    }
    Ok(output.supported.then(|| output.text.as_str()))
}

struct Output<'a> {
    arena: &'a Arena,
    text: ArenaString<'a>,
    supported: bool,
}

impl Output<'_> {
    fn push(&mut self, text: &str) -> Result<(), SqlError> {
        self.text.write_str(text).map_err(|_| arena_full())
    }

    fn unsupported(&mut self) -> Result<(), SqlError> {
        self.supported = false;
        Ok(())
    }
}

/// Numeric promotion order for operators without cross-type variants.
fn numeric_rank(type_oid: i32) -> Option<u8> {
    Some(match type_oid {
        oid::INT2 | oid::INT4 | oid::INT8 => 1,
        oid::NUMERIC => 2,
        oid::FLOAT4 | oid::FLOAT8 => 3,
        _ => return None,
    })
}

/// The type PostgreSQL's operators see: `varchar` relabels to `text`.
fn operator_type(type_oid: i32) -> i32 {
    if type_oid == oid::VARCHAR {
        oid::TEXT
    } else {
        type_oid
    }
}

impl Context<'_> {
    fn column_type(&self, qualifier: Option<&str>, name: &str) -> Result<Option<i32>, SqlError> {
        if qualifier.is_none()
            && let Some(parameter) = self
                .parameters
                .iter()
                .find(|parameter| parameter.name.as_str() == name)
        {
            return Ok(self.storage.routine_type_oid(
                parameter.ctype,
                parameter.user_type,
                self.txid,
            ));
        }
        let Some(table) = self.table else {
            return Ok(None);
        };
        if qualifier.is_some_and(|qualifier| qualifier != table.name.as_str()) {
            return Ok(None);
        }
        let Some(column) = table
            .columns()
            .iter()
            .find(|column| column.name.as_str() == name)
        else {
            return Ok(None);
        };
        super::catalog_column_type_oid(self.storage, column, self.txid).map(Some)
    }

    fn named_type(&self, type_name: &str) -> Option<i32> {
        ColType::from_sql_name(type_name)
            .map(ColType::oid)
            .or_else(|| super::user_type_oid(self.storage, self.txid, type_name))
    }

    /// The type an expression has before its consumer coerces it, or `None`
    /// for an unknown-typed literal.
    fn type_of(&self, expression: &Expr<'_>) -> Result<Option<i32>, SqlError> {
        Ok(match expression {
            Expr::Int(value) => Some(if i32::try_from(*value).is_ok() {
                oid::INT4
            } else {
                oid::INT8
            }),
            Expr::Float(_) | Expr::NumericLit(_) => Some(oid::NUMERIC),
            Expr::Bool(_)
            | Expr::IsNull { .. }
            | Expr::InList { .. }
            | Expr::Like { .. }
            | Expr::Match { .. } => Some(oid::BOOL),
            Expr::Str(_) | Expr::Null => None,
            Expr::Column { qualifier, name } => self.column_type(*qualifier, name)?,
            Expr::Cast { type_name, .. } => self.named_type(type_name),
            Expr::Unary {
                operator: UnaryOp::Not,
                ..
            } => Some(oid::BOOL),
            Expr::Unary { operand, .. } => self.type_of(operand)?,
            Expr::Binary {
                operator,
                left,
                right,
                ..
            } => match operator {
                BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq
                | BinaryOp::And
                | BinaryOp::Or => Some(oid::BOOL),
                BinaryOp::Concat => Some(oid::TEXT),
                BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
                    let (left, right) = self.operand_types(left, right)?;
                    left.or(right)
                }
                _ => None,
            },
            Expr::Call {
                kind: CallKind::SqlSyntax,
                name,
                args,
                ..
            } => match sql_value_function(name) {
                Some((_, result)) => Some(result),
                None if is_conditional(name) => {
                    let mut found = None;
                    for argument in *args {
                        if let Some(argument) = self.type_of(argument)? {
                            found = Some(argument);
                            break;
                        }
                    }
                    found
                }
                None => None,
            },
            Expr::Call { .. } => self
                .selected_builtin(expression)?
                .map(|routine| routine.result_oid),
            _ => None,
        })
    }

    /// Operand types after PostgreSQL's operator resolution, `None` only when
    /// both sides are unknown literals.
    fn operand_types(
        &self,
        left: &Expr<'_>,
        right: &Expr<'_>,
    ) -> Result<(Option<i32>, Option<i32>), SqlError> {
        let (left, right) = (self.type_of(left)?, self.type_of(right)?);
        Ok(match (left.map(operator_type), right.map(operator_type)) {
            (Some(left), None) => (Some(left), Some(left)),
            (None, Some(right)) => (Some(right), Some(right)),
            (None, None) => (Some(oid::TEXT), Some(oid::TEXT)),
            (Some(left), Some(right)) => {
                match (numeric_rank(left), numeric_rank(right)) {
                    // Integers and floats have cross-type operators within
                    // their family; between families the lower one is cast.
                    (Some(left_rank), Some(right_rank)) if left_rank < right_rank => {
                        (Some(right), Some(right))
                    }
                    (Some(left_rank), Some(right_rank)) if left_rank > right_rank => {
                        (Some(left), Some(left))
                    }
                    _ => (Some(left), Some(right)),
                }
            }
        })
    }

    fn selected_builtin(
        &self,
        expression: &Expr<'_>,
    ) -> Result<Option<&'static super::BuiltinCallableSignature>, SqlError> {
        let Expr::Call {
            kind: CallKind::Routine,
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
            return Ok(None);
        };
        if args.len() > crate::storage::MAX_ROUTINE_ARGUMENTS
            || argument_names.iter().any(Option::is_some)
        {
            return Ok(None);
        }
        let mut argument_oids = [oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
        for (target, argument) in argument_oids.iter_mut().zip(*args) {
            *target = self.type_of(argument)?.unwrap_or(oid::UNKNOWN);
        }
        Ok(
            match self.storage.select_routine_call(
                name,
                &[],
                &argument_oids[..args.len()],
                false,
                self.txid,
                crate::storage::RoutineCallKind::Function,
            ) {
                crate::storage::RoutineCallSelection::Builtin { oid, .. } => {
                    super::builtin_callable_by_oid(oid)
                }
                _ => None,
            },
        )
    }

    fn type_name(&self, type_oid: i32, output: &mut Output<'_>) -> Result<(), SqlError> {
        // `format_type_with_typemod(type, -1)`: an unconstrained character
        // type is spelled `bpchar`.
        if type_oid == oid::BPCHAR {
            return output.push("bpchar");
        }
        if let Some(ctype) = ColType::from_oid(type_oid) {
            return output.push(ctype.name());
        }
        match super::user_type_name_fixed(self.storage, self.txid, type_oid)? {
            Some(name) => output.push(name.as_str()),
            None => output.unsupported(),
        }
    }

    /// An expression assigned to `target`: the final coercion is not printed.
    fn assigned(
        &self,
        expression: &Expr<'_>,
        target: i32,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        match expression {
            Expr::Str(_) | Expr::Null => self.literal(expression, target, output),
            _ => {
                let natural = self.type_of(expression)?;
                self.write(expression, natural, output)
            }
        }
    }

    /// Writes `expression` where its consumer expects `expected`, printing a
    /// representation-changing coercion as PostgreSQL does.
    fn write(
        &self,
        expression: &Expr<'_>,
        expected: Option<i32>,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        if matches!(expression, Expr::Str(_) | Expr::Null) {
            return self.literal(expression, expected.unwrap_or(oid::TEXT), output);
        }
        let natural = self.type_of(expression)?;
        match (natural, expected) {
            (Some(natural), Some(expected))
                if natural != expected
                    && !(numeric_rank(natural) == Some(1) && numeric_rank(expected) == Some(1))
                    && !(numeric_rank(natural) == Some(3) && numeric_rank(expected) == Some(3)) =>
            {
                output.push("(")?;
                self.bare(expression, output)?;
                output.push(")::")?;
                self.type_name(expected, output)
            }
            _ => self.bare(expression, output),
        }
    }

    fn literal(
        &self,
        expression: &Expr<'_>,
        target: i32,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        let text = match expression {
            Expr::Str(text) => *text,
            Expr::Null => {
                output.push("NULL::")?;
                return self.type_name(target, output);
            }
            _ => return output.unsupported(),
        };
        // The parser converts an unknown literal with the type's input
        // function, so the printed value is that type's canonical output.
        let value = match ColType::from_oid(target) {
            Some(ctype) if ctype.is_reg_object() => {
                let catalog =
                    crate::sql::query::storage_catalog(self.storage, output.arena, self.txid);
                match crate::sql::eval::regobject_cast(
                    crate::sql::types::Datum::Text(text),
                    ctype,
                    Some(&catalog),
                    output.arena,
                ) {
                    Ok(value) => output
                        .arena
                        .alloc_str_display(value)
                        .map_err(|_| arena_full())?,
                    Err(_) => return output.unsupported(),
                }
            }
            Some(ctype) if !ctype.is_collatable() || ctype == ColType::Name => {
                match crate::sql::eval::cast_to(
                    crate::sql::types::Datum::Text(text),
                    ctype,
                    output.arena,
                ) {
                    Ok(value) => output
                        .arena
                        .alloc_str_display(value)
                        .map_err(|_| arena_full())?,
                    Err(_) => return output.unsupported(),
                }
            }
            Some(_) => text,
            None => text,
        };
        self.constant(value, target, output)
    }

    /// `get_const_expr`: integer and decimal constants are bare, booleans are
    /// keywords and everything else is a labeled string.
    fn constant(
        &self,
        value: &str,
        type_oid: i32,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        match type_oid {
            oid::INT4 if !value.starts_with('-') => output.push(value),
            oid::NUMERIC
                if value.starts_with(|first: char| first.is_ascii_digit())
                    && value.contains(['.', 'e', 'E']) =>
            {
                output.push(value)
            }
            oid::BOOL => output.push(if value.starts_with('t') {
                "true"
            } else {
                "false"
            }),
            _ => {
                output.push("'")?;
                for (index, part) in value.split('\'').enumerate() {
                    if index != 0 {
                        output.push("''")?;
                    }
                    output.push(part)?;
                }
                output.push("'::")?;
                self.type_name(type_oid, output)
            }
        }
    }

    fn bare(&self, expression: &Expr<'_>, output: &mut Output<'_>) -> Result<(), SqlError> {
        match expression {
            Expr::Int(value) => {
                let text = crate::stack_format!(24, "{}", value);
                self.constant(
                    text.as_str(),
                    self.type_of(expression)?.unwrap_or(oid::INT4),
                    output,
                )
            }
            Expr::Float(_) => output.unsupported(),
            Expr::NumericLit(text) => self.constant(text, oid::NUMERIC, output),
            Expr::Bool(value) => output.push(if *value { "true" } else { "false" }),
            Expr::Column { qualifier, name } => {
                if self.column_type(*qualifier, name)?.is_none() {
                    return output.unsupported();
                }
                let mut identifier = crate::util::StackStr::<130>::new();
                write_identifier(&mut identifier, name);
                output.push(identifier.as_str())
            }
            Expr::Unary {
                operator: UnaryOp::Neg,
                operand,
                ..
            } => match operand {
                // The parser folds a negated numeric literal into a constant.
                Expr::Int(value) => {
                    let text = crate::stack_format!(24, "{}", &-value);
                    self.constant(
                        text.as_str(),
                        self.type_of(operand)?.unwrap_or(oid::INT4),
                        output,
                    )
                }
                Expr::NumericLit(text) => {
                    let negated = crate::stack_format!(160, "-{}", text);
                    self.constant(negated.as_str(), oid::NUMERIC, output)
                }
                _ => {
                    output.push("(- ")?;
                    self.write(operand, None, output)?;
                    output.push(")")
                }
            },
            Expr::Unary {
                operator: UnaryOp::Not,
                operand,
                ..
            } => {
                output.push("(NOT ")?;
                self.write(operand, Some(oid::BOOL), output)?;
                output.push(")")
            }
            Expr::Binary {
                operator: operator @ (BinaryOp::And | BinaryOp::Or),
                ..
            } => {
                output.push("(")?;
                self.connective(expression, *operator, true, output)?;
                output.push(")")
            }
            Expr::Binary {
                operator,
                left,
                right,
                ..
            } => {
                let Some(symbol) = operator_symbol(*operator) else {
                    return output.unsupported();
                };
                let (left_type, right_type) = if *operator == BinaryOp::Concat {
                    let text = |expression: &Expr<'_>| -> Result<Option<i32>, SqlError> {
                        Ok(match self.type_of(expression)? {
                            None => Some(oid::TEXT),
                            Some(found) => Some(operator_type(found)),
                        })
                    };
                    (text(left)?, text(right)?)
                } else {
                    self.operand_types(left, right)?
                };
                output.push("(")?;
                self.write(left, left_type, output)?;
                output.push(" ")?;
                output.push(symbol)?;
                output.push(" ")?;
                self.write(right, right_type, output)?;
                output.push(")")
            }
            Expr::IsNull { operand, negated } => {
                output.push("(")?;
                self.write(operand, None, output)?;
                output.push(if *negated {
                    " IS NOT NULL)"
                } else {
                    " IS NULL)"
                })
            }
            Expr::InList {
                operand,
                list,
                negated,
            } => {
                let element = match self.type_of(operand)? {
                    Some(found) => Some(operator_type(found)),
                    None => Some(oid::TEXT),
                };
                output.push("(")?;
                self.write(operand, element, output)?;
                output.push(if *negated {
                    " <> ALL (ARRAY["
                } else {
                    " = ANY (ARRAY["
                })?;
                for (index, item) in list.iter().enumerate() {
                    if index != 0 {
                        output.push(", ")?;
                    }
                    self.write(item, element, output)?;
                }
                output.push("]))")
            }
            Expr::Like {
                operand,
                pattern,
                negated,
                case_insensitive,
                escape: None,
            } => {
                output.push("(")?;
                self.write(operand, Some(oid::TEXT), output)?;
                output.push(match (*negated, *case_insensitive) {
                    (false, false) => " ~~ ",
                    (false, true) => " ~~* ",
                    (true, false) => " !~~ ",
                    (true, true) => " !~~* ",
                })?;
                self.write(pattern, Some(oid::TEXT), output)?;
                output.push(")")
            }
            Expr::Match {
                operand,
                pattern,
                negated,
                case_insensitive,
            } => {
                output.push("(")?;
                self.write(operand, Some(oid::TEXT), output)?;
                output.push(match (*negated, *case_insensitive) {
                    (false, false) => " ~ ",
                    (false, true) => " ~* ",
                    (true, false) => " !~ ",
                    (true, true) => " !~* ",
                })?;
                self.write(pattern, Some(oid::TEXT), output)?;
                output.push(")")
            }
            Expr::Cast {
                operand, type_name, ..
            } => {
                let Some(target) = self.named_type(type_name) else {
                    return output.unsupported();
                };
                match operand {
                    // A cast literal is converted while parsing.
                    Expr::Str(_) | Expr::Null => self.literal(operand, target, output),
                    _ => {
                        output.push("(")?;
                        self.write(operand, None, output)?;
                        output.push(")::")?;
                        self.type_name(target, output)
                    }
                }
            }
            Expr::Call {
                kind: CallKind::SqlSyntax,
                name,
                args,
                ..
            } => {
                if let Some((spelling, _)) = sql_value_function(name) {
                    output.push(spelling)?;
                    match args {
                        [] => Ok(()),
                        [Expr::Int(precision)] => {
                            let precision = crate::stack_format!(24, "{}", precision);
                            output.push("(")?;
                            output.push(precision.as_str())?;
                            output.push(")")
                        }
                        _ => output.unsupported(),
                    }
                } else if is_conditional(name) {
                    let common = self.type_of(expression)?;
                    output.push(upper_ascii(name).as_str())?;
                    output.push("(")?;
                    for (index, argument) in args.iter().enumerate() {
                        if index != 0 {
                            output.push(", ")?;
                        }
                        self.write(argument, common, output)?;
                    }
                    output.push(")")
                } else {
                    output.unsupported()
                }
            }
            Expr::Call { name, args, .. } => {
                let Some(routine) = self.selected_builtin(expression)? else {
                    return output.unsupported();
                };
                if routine.arguments.len() != args.len() || !routine.schema.eq("pg_catalog") {
                    return output.unsupported();
                }
                let mut identifier = crate::util::StackStr::<130>::new();
                write_identifier(&mut identifier, name);
                output.push(identifier.as_str())?;
                output.push("(")?;
                for (index, (argument, parameter)) in args.iter().zip(routine.arguments).enumerate()
                {
                    if index != 0 {
                        output.push(", ")?;
                    }
                    let expected = (crate::storage::PolymorphicType::from_oid(*parameter)
                        .is_none())
                    .then_some(*parameter);
                    self.write(argument, expected, output)?;
                }
                output.push(")")
            }
            Expr::Case {
                operand: None,
                whens,
                otherwise,
                synthetic: false,
            } => {
                let Some((_, first)) = whens.first() else {
                    return output.unsupported();
                };
                let result = match self.type_of(first)? {
                    Some(found) => Some(found),
                    None => Some(oid::TEXT),
                };
                output.push("CASE")?;
                for (condition, value) in *whens {
                    output.push(" WHEN ")?;
                    self.write(condition, Some(oid::BOOL), output)?;
                    output.push(" THEN ")?;
                    self.write(value, result, output)?;
                }
                if let Some(otherwise) = otherwise {
                    output.push(" ELSE ")?;
                    self.write(otherwise, result, output)?;
                }
                output.push(" END")
            }
            _ => output.unsupported(),
        }
    }

    /// PostgreSQL's grammar flattens a left-nested `AND`/`OR` chain into one
    /// list; a right operand of the same connective stays a nested list.
    fn connective(
        &self,
        expression: &Expr<'_>,
        operator: BinaryOp,
        first: bool,
        output: &mut Output<'_>,
    ) -> Result<bool, SqlError> {
        if let Expr::Binary {
            operator: found,
            left,
            right,
            ..
        } = expression
            && *found == operator
        {
            let first = self.connective(left, operator, first, output)?;
            output.push(if operator == BinaryOp::And {
                " AND "
            } else {
                " OR "
            })?;
            self.write(right, Some(oid::BOOL), output)?;
            return Ok(first);
        }
        if !first {
            output.push(if operator == BinaryOp::And {
                " AND "
            } else {
                " OR "
            })?;
        }
        self.write(expression, Some(oid::BOOL), output)?;
        Ok(false)
    }
}

fn operator_symbol(operator: BinaryOp) -> Option<&'static str> {
    Some(match operator {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Mod => "%",
        BinaryOp::Eq => "=",
        BinaryOp::NotEq => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::LtEq => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::GtEq => ">=",
        BinaryOp::Concat => "||",
        _ => return None,
    })
}

/// SQL value functions and their result types.
fn sql_value_function(name: &str) -> Option<(&'static str, i32)> {
    Some(match name {
        "current_date" => ("CURRENT_DATE", oid::DATE),
        "current_time" => ("CURRENT_TIME", oid::TIMETZ),
        "current_timestamp" => ("CURRENT_TIMESTAMP", oid::TIMESTAMPTZ),
        "localtime" => ("LOCALTIME", oid::TIME),
        "localtimestamp" => ("LOCALTIMESTAMP", oid::TIMESTAMP),
        "current_user" => ("CURRENT_USER", oid::NAME),
        "current_role" => ("CURRENT_ROLE", oid::NAME),
        "session_user" => ("SESSION_USER", oid::NAME),
        "user" => ("USER", oid::NAME),
        "current_catalog" => ("CURRENT_CATALOG", oid::NAME),
        "current_schema" => ("CURRENT_SCHEMA", oid::NAME),
        _ => return None,
    })
}

fn is_conditional(name: &str) -> bool {
    matches!(name, "coalesce" | "nullif" | "greatest" | "least")
}

fn upper_ascii(name: &str) -> crate::util::StackStr<16> {
    let mut upper = crate::util::StackStr::<16>::new();
    for character in name.chars() {
        let _ = upper.write_char(character.to_ascii_uppercase());
    }
    upper
}
