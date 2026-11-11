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
    /// The FROM items of a view, by exposed name. Column references are
    /// qualified when there is more than one, as PostgreSQL prints them.
    pub joined: &'s [(&'s str, &'s TableDef)],
    /// `PRETTYFLAG_PAREN`: parentheses only where precedence requires them.
    pub pretty: bool,
}

/// The node a pretty-printed child expression appears under.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Parent {
    Operator(&'static str),
    Connective(BinaryOp),
    Not,
    Cast,
    NullTest,
    ArrayComparison,
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
        if let Some((_, table)) = self.joined_source(qualifier, name) {
            let column = table
                .columns()
                .iter()
                .find(|column| column.name.as_str() == name)
                .expect("joined source has the column");
            return super::catalog_column_type_oid(self.storage, column, self.txid).map(Some);
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

    /// The one joined FROM item a column reference names.
    fn joined_source(&self, qualifier: Option<&str>, name: &str) -> Option<(&str, &TableDef)> {
        let mut found = None;
        for (exposed, table) in self.joined {
            if qualifier.is_some_and(|qualifier| qualifier != *exposed)
                || !table
                    .columns()
                    .iter()
                    .any(|column| column.name.as_str() == name)
            {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some((*exposed, *table));
        }
        found
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
            Expr::Call {
                name: "count",
                star: true,
                ..
            } => Some(oid::INT8),
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
                self.wrapped(expression, None, Parent::Cast, true, output)?;
                output.push("::")?;
                self.type_name(expected, output)
            }
            _ => self.bare(expression, output),
        }
    }

    fn open(&self, output: &mut Output<'_>) -> Result<(), SqlError> {
        if self.pretty {
            Ok(())
        } else {
            output.push("(")
        }
    }

    fn close(&self, output: &mut Output<'_>) -> Result<(), SqlError> {
        if self.pretty {
            Ok(())
        } else {
            output.push(")")
        }
    }

    /// A child expression: a cast operand is always parenthesized, and pretty
    /// printing parenthesizes where precedence requires (`isSimpleNode`).
    /// `expected` of `None` prints the child itself, without a coercion.
    fn wrapped(
        &self,
        expression: &Expr<'_>,
        expected: Option<Option<i32>>,
        parent: Parent,
        first: bool,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        let parenthesize = if self.pretty {
            !simple(expression, parent, first)
        } else {
            parent == Parent::Cast
        };
        if parenthesize {
            output.push("(")?;
        }
        match expected {
            Some(expected) => self.write(expression, expected, output)?,
            None => self.bare(expression, output)?,
        }
        if parenthesize {
            output.push(")")?;
        }
        Ok(())
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
                let mut identifier = crate::util::StackStr::<260>::new();
                if self.joined.len() > 1 {
                    let Some((exposed, _)) = self.joined_source(*qualifier, name) else {
                        return output.unsupported();
                    };
                    write_identifier(&mut identifier, exposed);
                    let _ = identifier.write_char('.');
                }
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
                _ if self.pretty => output.unsupported(),
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
                self.open(output)?;
                output.push("NOT ")?;
                self.wrapped(operand, Some(Some(oid::BOOL)), Parent::Not, true, output)?;
                self.close(output)
            }
            Expr::Binary {
                operator: operator @ (BinaryOp::And | BinaryOp::Or),
                ..
            } => {
                self.open(output)?;
                self.connective(expression, *operator, true, output)?;
                self.close(output)
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
                self.open(output)?;
                let parent = Parent::Operator(symbol);
                self.wrapped(left, Some(left_type), parent, true, output)?;
                output.push(" ")?;
                output.push(symbol)?;
                output.push(" ")?;
                self.wrapped(right, Some(right_type), parent, false, output)?;
                self.close(output)
            }
            Expr::IsNull { operand, negated } => {
                self.open(output)?;
                self.wrapped(operand, Some(None), Parent::NullTest, true, output)?;
                output.push(if *negated { " IS NOT NULL" } else { " IS NULL" })?;
                self.close(output)
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
                self.open(output)?;
                self.wrapped(
                    operand,
                    Some(element),
                    Parent::ArrayComparison,
                    true,
                    output,
                )?;
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
                output.push("])")?;
                self.close(output)
            }
            Expr::Like {
                operand,
                pattern,
                negated,
                case_insensitive,
                escape: None,
            } => {
                let symbol = match (*negated, *case_insensitive) {
                    (false, false) => "~~",
                    (false, true) => "~~*",
                    (true, false) => "!~~",
                    (true, true) => "!~~*",
                };
                self.operator_pair(operand, pattern, symbol, output)
            }
            Expr::Match {
                operand,
                pattern,
                negated,
                case_insensitive,
            } => {
                let symbol = match (*negated, *case_insensitive) {
                    (false, false) => "~",
                    (false, true) => "~*",
                    (true, false) => "!~",
                    (true, true) => "!~*",
                };
                self.operator_pair(operand, pattern, symbol, output)
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
                        self.wrapped(operand, Some(None), Parent::Cast, true, output)?;
                        output.push("::")?;
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
            Expr::Call {
                name: "count",
                star: true,
                args: [],
                distinct: false,
                filter: None,
                over: None,
                ..
            } => output.push("count(*)"),
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

    /// A searched CASE in PostgreSQL's indented layout.
    fn case_block(
        &self,
        whens: &[(&Expr<'_>, &Expr<'_>)],
        otherwise: Option<&Expr<'_>>,
        indent: usize,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        const SPACES: &str = "                                ";
        let (Some(outer), Some(inner)) = (SPACES.get(..indent), SPACES.get(..indent + 4)) else {
            return output.unsupported();
        };
        let Some((_, first)) = whens.first() else {
            return output.unsupported();
        };
        let result = Some(self.type_of(first)?.unwrap_or(oid::TEXT));
        output.push("\n")?;
        output.push(outer)?;
        output.push("CASE")?;
        for (condition, value) in whens {
            output.push("\n")?;
            output.push(inner)?;
            output.push("WHEN ")?;
            self.write(condition, Some(oid::BOOL), output)?;
            output.push(" THEN ")?;
            self.write(value, result, output)?;
        }
        if let Some(otherwise) = otherwise {
            output.push("\n")?;
            output.push(inner)?;
            output.push("ELSE ")?;
            self.write(otherwise, result, output)?;
        }
        output.push("\n")?;
        output.push(outer)?;
        output.push("END")
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
            self.wrapped(
                right,
                Some(Some(oid::BOOL)),
                Parent::Connective(operator),
                false,
                output,
            )?;
            return Ok(first);
        }
        if !first {
            output.push(if operator == BinaryOp::And {
                " AND "
            } else {
                " OR "
            })?;
        }
        self.wrapped(
            expression,
            Some(Some(oid::BOOL)),
            Parent::Connective(operator),
            first,
            output,
        )?;
        Ok(false)
    }

    /// A text pattern operator (`~~`, `~`, and their variants).
    fn operator_pair(
        &self,
        left: &Expr<'_>,
        right: &Expr<'_>,
        symbol: &'static str,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        self.open(output)?;
        let parent = Parent::Operator(symbol);
        self.wrapped(left, Some(Some(oid::TEXT)), parent, true, output)?;
        output.push(" ")?;
        output.push(symbol)?;
        output.push(" ")?;
        self.wrapped(right, Some(Some(oid::TEXT)), parent, false, output)?;
        self.close(output)
    }
}

/// `isSimpleNode` with `PRETTYFLAG_PAREN`: whether a child needs no
/// parentheses under its parent.
fn simple(expression: &Expr<'_>, parent: Parent, first: bool) -> bool {
    let connective = |kind: Option<BinaryOp>| match parent {
        Parent::Connective(outer) => match kind {
            None | Some(BinaryOp::And) => matches!(outer, BinaryOp::And | BinaryOp::Or),
            Some(_) => outer == BinaryOp::Or,
        },
        Parent::Not => matches!(kind, None | Some(BinaryOp::And)),
        _ => false,
    };
    let operator = |symbol: &str| match parent {
        Parent::Operator(outer) => {
            let low = |symbol: &str| matches!(symbol, "+" | "-");
            let high = |symbol: &str| matches!(symbol, "*" | "/" | "%");
            if !(low(symbol) || high(symbol)) || !(low(outer) || high(outer)) {
                false
            } else if high(symbol) && low(outer) {
                true
            } else if low(symbol) && high(outer) {
                false
            } else {
                first
            }
        }
        Parent::Connective(_) | Parent::Not => true,
        _ => false,
    };
    match expression {
        Expr::Column { .. }
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::NumericLit(_)
        | Expr::Bool(_)
        | Expr::Str(_)
        | Expr::Null
        | Expr::Call { .. }
        | Expr::Cast { .. } => true,
        Expr::Unary {
            operator: UnaryOp::Neg,
            operand,
            ..
        } => matches!(operand, Expr::Int(_) | Expr::NumericLit(_)),
        Expr::Unary {
            operator: UnaryOp::Not,
            ..
        } => connective(None),
        Expr::Binary {
            operator: kind @ (BinaryOp::And | BinaryOp::Or),
            ..
        } => connective(Some(*kind)),
        Expr::Binary { operator: kind, .. } => operator_symbol(*kind).is_some_and(operator),
        Expr::Like { .. } | Expr::Match { .. } => operator("~~"),
        Expr::IsNull { .. } => matches!(parent, Parent::Connective(_) | Parent::Not),
        _ => false,
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

/// `pg_get_viewdef` formatting: the pretty flag (`PRETTYFLAG_PAREN`) and the
/// target-list wrap column. Every form indents; PostgreSQL's default wrap
/// column 0 starts each target on its own line, and a negative one never
/// wraps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ViewLayout {
    pub pretty: bool,
    pub wrap_column: i32,
}

impl ViewLayout {
    pub(crate) const PLAIN: Self = Self {
        pretty: false,
        wrap_column: 0,
    };
}

/// The most FROM items a deparsed view may join.
const MAX_JOINED: usize = 8;

/// A plain table reference, with no function, subquery or sample.
fn plain_table<'r>(
    source: &'r crate::sql::ast::TableRef<'_>,
) -> Option<&'r crate::sql::ast::TableRef<'r>> {
    (source.subquery.is_none()
        && source.func_args.is_none()
        && source.rows_from.is_none()
        && source.col_alias.is_none())
    .then_some(source)
}

/// `pg_get_viewdef(view)` for a view over one table: PostgreSQL's indented
/// layout, one target per line, with analyzed expressions. Other query shapes
/// are reported as unsupported.
pub(crate) fn view_definition<'a>(
    storage: &Storage,
    txid: u32,
    query: &str,
    column_names: &[&str],
    layout: ViewLayout,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let pretty = layout.pretty;
    let Ok(select) = crate::sql::parser::parse_query(query, arena) else {
        return Ok(None);
    };
    let Some(from) = select.from else {
        return Ok(None);
    };
    if select.distinct
        || !select.distinct_on.is_empty()
        || from.joins.len() >= MAX_JOINED
        || from.joins.iter().any(|join| {
            join.on.is_none()
                || join.using.is_some()
                || join.natural
                || join.kind == crate::sql::ast::JoinKind::Cross
                || plain_table(&join.table).is_none()
        })
        || !select.with.is_empty()
        || select.set_body.is_some()
        || !select.locking.is_empty()
        || !select.grouping_sets.is_empty()
        || select.having.is_some()
        || select.with_ties
        || plain_table(&from.base).is_none()
        || (!from.joins.is_empty()
            && matches!(select.items, [crate::sql::ast::SelectItem::Wildcard(_)]))
    {
        return Ok(None);
    }
    let wildcard = matches!(select.items, [crate::sql::ast::SelectItem::Wildcard(_)]);
    if !wildcard && select.items.len() != column_names.len() {
        return Ok(None);
    }
    let sources = core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table));
    let mut reads: [Option<crate::storage::TableDefinitionRead<'_>>; MAX_JOINED] =
        core::array::from_fn(|_| None);
    for (read, source) in reads.iter_mut().zip(sources.clone()) {
        let Some(crate::storage::ResolvedRelation::Table(slot)) =
            storage.resolve_relation(source.schema, source.table, txid)
        else {
            return Ok(None);
        };
        *read = Some(storage.table_def(slot, txid));
    }
    let table = reads[0].as_deref().expect("resolved base relation");
    let mut joined: [(&str, &TableDef); MAX_JOINED] = [("", table); MAX_JOINED];
    let count = from.joins.len() + 1;
    for ((target, read), source) in joined.iter_mut().zip(&reads).zip(sources) {
        *target = (
            source.alias.unwrap_or(source.table),
            read.as_deref().expect("resolved source"),
        );
    }
    let context = Context {
        storage,
        txid,
        table: Some(table),
        parameters: &[],
        joined: &joined[..count],
        pretty,
    };
    let mut output = Output {
        arena,
        text: ArenaString::new(arena),
        supported: true,
    };
    if wildcard && table.columns().len() < column_names.len() {
        return Ok(None);
    }
    let mut last_multiline = false;
    for (index, name) in column_names.iter().enumerate() {
        let mut target = Output {
            arena,
            text: ArenaString::new(arena),
            supported: true,
        };
        // `*` was expanded to the table's columns when the view was created.
        let expanded;
        let expression = if wildcard {
            expanded = Expr::Column {
                qualifier: None,
                name: table.columns()[index].name.as_str(),
            };
            &expanded
        } else {
            let crate::sql::ast::SelectItem::Expr { expression, .. } = &select.items[index] else {
                return Ok(None);
            };
            *expression
        };
        if let Expr::Case {
            operand: None,
            whens,
            otherwise,
            synthetic: false,
        } = expression
        {
            // The indented layout starts a searched CASE on its own line.
            context.case_block(whens, *otherwise, 8, &mut target)?;
        } else {
            let natural = context.type_of(expression)?;
            context.write(expression, natural, &mut target)?;
        }
        let bare_column = matches!(expression, Expr::Column { name: column, .. } if column == name);
        if !bare_column {
            target.push(" AS ")?;
            let mut identifier = crate::util::StackStr::<130>::new();
            write_identifier(&mut identifier, name);
            target.push(identifier.as_str())?;
        }
        output.supported &= target.supported;
        let text = target.text.as_str();
        // `get_target_list`: a target that starts a new line drops the
        // separator's trailing space; another wraps when it would pass the
        // wrap column or follows a multi-line target.
        let leading_newline = text.starts_with('\n');
        if index == 0 {
            output.push(if leading_newline {
                " SELECT"
            } else {
                " SELECT "
            })?;
        } else if leading_newline || layout.wrap_column < 0 {
            output.push(if leading_newline { "," } else { ", " })?;
        } else {
            let current = output.text.as_str();
            let line = current
                .rfind('\n')
                .map_or(current.len(), |at| current.len() - at - 1);
            let wraps = last_multiline || line + 2 + text.len() > layout.wrap_column as usize;
            output.push(if wraps { ",\n    " } else { ", " })?;
        }
        output.push(text)?;
        last_multiline = text[usize::from(leading_newline)..].contains('\n');
    }
    output.push("\n   FROM ")?;
    if !pretty {
        for _ in from.joins {
            output.push("(")?;
        }
    }
    let relation = |table: &TableDef, source: &crate::sql::ast::TableRef<'_>| {
        let mut relation = crate::util::StackStr::<400>::new();
        if !storage.schema_is_on_path(table.schema, txid) {
            write_identifier(&mut relation, table.schema.as_str());
            let _ = relation.write_char('.');
        }
        write_identifier(&mut relation, table.name.as_str());
        if let Some(alias) = source.alias.filter(|alias| *alias != table.name.as_str()) {
            let _ = relation.write_char(' ');
            write_identifier(&mut relation, alias);
        }
        relation
    };
    output.push(relation(table, &from.base).as_str())?;
    for (join, (_, joined_table)) in from.joins.iter().zip(&joined[1..]) {
        output.push(match join.kind {
            crate::sql::ast::JoinKind::Left => "\n     LEFT JOIN ",
            crate::sql::ast::JoinKind::Right => "\n     RIGHT JOIN ",
            crate::sql::ast::JoinKind::Full => "\n     FULL JOIN ",
            _ => "\n     JOIN ",
        })?;
        output.push(relation(joined_table, &join.table).as_str())?;
        output.push(if pretty { " ON " } else { " ON (" })?;
        context.write(
            join.on.expect("checked join condition"),
            Some(oid::BOOL),
            &mut output,
        )?;
        if !pretty {
            output.push("))")?;
        }
    }
    if let Some(condition) = select.where_clause {
        output.push("\n  WHERE ")?;
        context.write(condition, Some(oid::BOOL), &mut output)?;
    }
    if !select.group_by.is_empty() {
        output.push("\n  GROUP BY ")?;
        for (index, expression) in select.group_by.iter().enumerate() {
            if index != 0 {
                output.push(", ")?;
            }
            let natural = context.type_of(expression)?;
            context.write(expression, natural, &mut output)?;
        }
    }
    if !select.order_by.is_empty() {
        output.push("\n  ORDER BY ")?;
        for (index, order) in select.order_by.iter().enumerate() {
            if index != 0 {
                output.push(", ")?;
            }
            let natural = context.type_of(order.expression)?;
            context.write(order.expression, natural, &mut output)?;
            if order.descending {
                output.push(" DESC")?;
            }
            if order.nulls_first != order.descending {
                output.push(if order.nulls_first {
                    " NULLS FIRST"
                } else {
                    " NULLS LAST"
                })?;
            }
        }
    }
    for (keyword, value) in [(" OFFSET ", select.offset), (" LIMIT ", select.limit)] {
        if let Some(value) = value {
            output.push("\n")?;
            output.push(keyword)?;
            let natural = context.type_of(value)?;
            context.write(value, natural, &mut output)?;
        }
    }
    output.push(";")?;
    Ok(output.supported.then(|| output.text.as_str()))
}
