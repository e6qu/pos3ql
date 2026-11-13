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
    /// A value of its own type, such as a select-list item.
    Value,
}

pub(crate) struct Context<'s> {
    pub storage: &'s Storage,
    pub txid: u32,
    pub table: Option<&'s TableDef>,
    /// Named parameters of the routine whose body or default is printed.
    pub parameters: &'s [crate::storage::RoutineParameterDef],
    /// The FROM items of a view, by exposed name. Column references are
    /// qualified when there is more than one, as PostgreSQL prints them.
    pub joined: &'s [Joined<'s>],
    /// `PRETTYFLAG_PAREN`: parentheses only where precedence requires them.
    pub pretty: bool,
    /// Qualify every column, as PostgreSQL does when the query's range table
    /// holds more than the FROM items (a rule's `OLD` and `NEW`).
    pub qualify: bool,
    /// The FROM item whose columns print unqualified while others are
    /// qualified, as PostgreSQL prints a scan's own conditions.
    pub bare: Option<&'s str>,
}

/// A FROM item: the name stored references qualify columns with, the name
/// printed for it (a renamed table's current name unless it has an alias)
/// and its definition.
#[derive(Clone, Copy)]
pub(crate) struct Joined<'s> {
    pub name: &'s str,
    pub printed: &'s str,
    pub table: &'s TableDef,
    /// Whether an unqualified column can name this item; a rule's `OLD` and
    /// `NEW` are reached only by their names.
    pub unqualified: bool,
}

impl<'s> Joined<'s> {
    pub(crate) fn new(name: &'s str, table: &'s TableDef) -> Self {
        Self {
            name,
            printed: name,
            table,
            unqualified: true,
        }
    }

    /// A rule's `OLD` or `NEW` row in a rule action.
    pub(crate) fn rule_row(name: &'s str, table: &'s TableDef) -> Self {
        Self {
            unqualified: false,
            ..Self::new(name, table)
        }
    }
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
    deparse_expression(context, expression, destination, arena)
}

/// Deparses a parsed expression, such as one whose names were bound to the
/// catalog identities they captured at CREATE.
pub(crate) fn deparse_expression<'a>(
    context: &Context<'_>,
    expression: &Expr<'_>,
    destination: Destination,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let mut output = Output {
        arena,
        text: ArenaString::new(arena),
        supported: true,
    };
    match destination {
        Destination::Assignment(target) => context.assigned(expression, target, &mut output)?,
        Destination::Condition => context.write(expression, Some(oid::BOOL), &mut output)?,
        Destination::Value => context.write(expression, None, &mut output)?,
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

    fn push_spaces(&mut self, count: usize) -> Result<(), SqlError> {
        for _ in 0..count {
            self.push(" ")?;
        }
        Ok(())
    }

    /// Pushes `text`, which starts a new line, after dropping the trailing
    /// spaces before it, as `appendContextKeyword` does.
    fn push_trimmed(&mut self, text: &str) -> Result<(), SqlError> {
        let kept = self.text.as_str().trim_end_matches(' ').len();
        self.text.truncate(kept);
        self.push(text)
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
        for joined in self.joined {
            if qualifier.is_some_and(|qualifier| qualifier != joined.name)
                || qualifier.is_none() && !joined.unqualified
                || !joined
                    .table
                    .columns()
                    .iter()
                    .any(|column| column.name.as_str() == name)
            {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some((joined.printed, joined.table));
        }
        found
    }

    /// The relation a whole-row reference names, with the name it is exposed
    /// under: `t.*`, or a bare `t` that is neither a column nor a parameter.
    fn whole_row(&self, expression: &Expr<'_>) -> Option<(&str, &TableDef)> {
        let (name, bare) = match expression {
            Expr::WholeRow(name) => (*name, false),
            Expr::Column {
                qualifier: None,
                name,
            } if !self
                .parameters
                .iter()
                .any(|parameter| parameter.name.as_str() == *name) =>
            {
                (*name, true)
            }
            _ => return None,
        };
        if self.joined.is_empty() {
            let table = self.table?;
            return (name == table.name.as_str() && !(bare && table.column_index(name).is_some()))
                .then_some((table.name.as_str(), table));
        }
        if bare
            && self
                .joined
                .iter()
                .any(|joined| joined.table.column_index(name).is_some())
        {
            return None;
        }
        self.joined
            .iter()
            .find(|joined| joined.name == name)
            .map(|joined| (joined.printed, joined.table))
    }

    fn row_type(&self, table: &TableDef) -> Option<i32> {
        self.storage
            .find_visible(table.schema.as_str(), table.name.as_str(), self.txid)
            .map(|slot| super::table_row_type_oid(self.storage, slot, false))
    }

    /// `(t).column`, which PostgreSQL resolves to the column itself.
    fn row_field<'e>(&self, expression: &Expr<'e>) -> Option<&'e str> {
        match expression {
            Expr::Field { base, field } if *field != "*" && self.whole_row(base).is_some() => {
                Some(field)
            }
            _ => None,
        }
    }

    /// A user-defined function selected for a positional call.
    fn selected_routine(
        &self,
        expression: &Expr<'_>,
    ) -> Result<Option<(usize, crate::storage::RoutineDef)>, SqlError> {
        let Expr::Call {
            kind: CallKind::Routine,
            name,
            args,
            argument_names,
            bound_oid,
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
        // A bound call keeps the routine its stored body selected at CREATE.
        if let Some(slot) =
            bound_oid.and_then(|oid| self.storage.routine_slot_by_oid(oid, self.txid))
        {
            let routine = self.storage.routine_for(slot, self.txid);
            return Ok((routine.argument_count == args.len()).then_some((slot, routine)));
        }
        let mut argument_oids = [oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
        for (target, argument) in argument_oids.iter_mut().zip(*args) {
            *target = self.type_of(argument)?.unwrap_or(oid::UNKNOWN);
        }
        let crate::storage::RoutineCallSelection::Stored(slot) = self.storage.select_routine_call(
            name,
            &[],
            &argument_oids[..args.len()],
            false,
            self.txid,
            crate::storage::RoutineCallKind::Function,
        ) else {
            return Ok(None);
        };
        let routine = self.storage.routine_for(slot, self.txid);
        Ok((routine.argument_count == args.len()).then_some((slot, routine)))
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
            | Expr::AnyAll { .. }
            | Expr::Like { .. }
            | Expr::Match { .. } => Some(oid::BOOL),
            Expr::Str(_) | Expr::Null => None,
            _ if self.whole_row(expression).is_some() => self
                .whole_row(expression)
                .and_then(|(_, table)| self.row_type(table)),
            Expr::Column { qualifier, name } => self.column_type(*qualifier, name)?,
            Expr::Field { .. } => match self.row_field(expression) {
                Some(field) => self.column_type(None, field)?,
                None => None,
            },
            Expr::Call {
                kind: CallKind::SqlSyntax,
                name,
                ..
            } if name.eq_ignore_ascii_case("row") => Some(oid::RECORD),
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
            Expr::Call { .. } => match self.selected_builtin(expression)? {
                Some(routine) => Some(routine.result_oid),
                None => self.selected_routine(expression)?.and_then(|(_, routine)| {
                    self.storage
                        .routine_function_result_oid(&routine, self.txid)
                }),
            },
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
        if let Some(ctype) = ColType::from_oid(type_oid)
            && crate::storage::NamedTypeKind::from_oid(type_oid).is_none()
        {
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
            // `enum_out(enum_in(label))` is the label itself.
            Some(ColType::Enum(_)) => text,
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
            // `get_variable` prints a whole-row variable as `t.*`.
            _ if self.whole_row(expression).is_some() => {
                let exposed = self
                    .whole_row(expression)
                    .map_or("", |(exposed, _)| exposed);
                let mut identifier = crate::util::StackStr::<260>::new();
                write_identifier(&mut identifier, exposed);
                let _ = identifier.write_str(".*");
                output.push(identifier.as_str())
            }
            Expr::Field { .. } if self.row_field(expression).is_some() => {
                let field = self.row_field(expression).unwrap_or_default();
                self.bare(
                    &Expr::Column {
                        qualifier: None,
                        name: field,
                    },
                    output,
                )
            }
            Expr::Call {
                kind: CallKind::SqlSyntax,
                name,
                args,
                ..
            } if name.eq_ignore_ascii_case("row") => {
                // The parser expands `ROW(t.*)` into the relation's columns.
                output.push("ROW(")?;
                let mut first = true;
                for argument in *args {
                    let expansion = match argument {
                        Expr::WholeRow(_) => self.whole_row(argument),
                        Expr::Field { base, field: "*" } => self.whole_row(base),
                        _ => None,
                    };
                    let columns = match expansion {
                        Some((_, table)) => table.columns(),
                        None => &[],
                    };
                    for column in columns {
                        if !first {
                            output.push(", ")?;
                        }
                        first = false;
                        let mut identifier = crate::util::StackStr::<260>::new();
                        write_identifier(&mut identifier, column.name.as_str());
                        output.push(identifier.as_str())?;
                    }
                    if expansion.is_none() {
                        if !first {
                            output.push(", ")?;
                        }
                        first = false;
                        self.write(argument, None, output)?;
                    }
                }
                output.push(")")
            }
            Expr::Column { qualifier, name } => {
                if self.column_type(*qualifier, name)?.is_none() {
                    return output.unsupported();
                }
                let mut identifier = crate::util::StackStr::<260>::new();
                if self.joined.len() > 1 || self.qualify {
                    let Some((exposed, _)) = self.joined_source(*qualifier, name) else {
                        return output.unsupported();
                    };
                    if self.bare != Some(exposed) {
                        write_identifier(&mut identifier, exposed);
                        let _ = identifier.write_char('.');
                    }
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
            Expr::Array(items) => {
                let mut element = None;
                for item in *items {
                    if let Some(found) = self.type_of(item)? {
                        element = Some(found);
                        break;
                    }
                }
                let element = element.unwrap_or(oid::TEXT);
                output.push("ARRAY[")?;
                for (index, item) in items.iter().enumerate() {
                    if index != 0 {
                        output.push(", ")?;
                    }
                    self.write(item, Some(element), output)?;
                }
                output.push("]")
            }
            Expr::AnyAll {
                operand,
                operator,
                array,
                all,
            } => {
                let Some(symbol) = operator.operator_name() else {
                    return output.unsupported();
                };
                let element = match self.type_of(operand)? {
                    Some(found) => Some(operator_type(found)),
                    None => Some(oid::TEXT),
                };
                // An untyped array literal takes the operand's array type,
                // which this deparser has no spelling for.
                if matches!(array, Expr::Str(_)) {
                    return output.unsupported();
                }
                self.open(output)?;
                self.wrapped(
                    operand,
                    Some(element),
                    Parent::ArrayComparison,
                    true,
                    output,
                )?;
                output.push(" ")?;
                output.push(symbol)?;
                output.push(if *all { " ALL (" } else { " ANY (" })?;
                self.write(array, None, output)?;
                output.push(")")?;
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
            Expr::Call { args, .. } if self.selected_builtin(expression)?.is_none() => {
                let Some((slot, routine)) = self.selected_routine(expression)? else {
                    return output.unsupported();
                };
                let mut identifier = crate::util::StackStr::<260>::new();
                // `generate_function_name` qualifies a function the bare
                // name would not select.
                let argument_oids = |index: usize| {
                    let argument = routine.arguments[index];
                    self.storage
                        .routine_type_oid(argument.ctype, argument.user_type, self.txid)
                };
                let mut parameter_oids = [oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
                for (index, target) in parameter_oids.iter_mut().enumerate().take(args.len()) {
                    let Some(found) = argument_oids(index) else {
                        return output.unsupported();
                    };
                    *target = found;
                }
                if self.storage.select_routine_call(
                    routine.name.as_str(),
                    &[],
                    &parameter_oids[..args.len()],
                    false,
                    self.txid,
                    crate::storage::RoutineCallKind::Function,
                ) != crate::storage::RoutineCallSelection::Stored(slot)
                {
                    write_identifier(&mut identifier, routine.schema.as_str());
                    let _ = identifier.write_char('.');
                }
                write_identifier(&mut identifier, routine.name.as_str());
                output.push(identifier.as_str())?;
                output.push("(")?;
                for (index, argument) in args.iter().enumerate() {
                    if index != 0 {
                        output.push(", ")?;
                    }
                    self.write(argument, Some(parameter_oids[index]), output)?;
                }
                output.push(")")
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
                origin: crate::sql::ast::CaseOrigin::DistinctFrom { .. },
                ..
            } => {
                let Some((left, right, negated)) = expression.distinct_from_operands() else {
                    return output.unsupported();
                };
                // PostgreSQL parses `IS NOT DISTINCT FROM` as the negation of
                // a DistinctExpr.
                if negated {
                    self.open(output)?;
                    output.push("NOT ")?;
                }
                let (left_type, right_type) = self.operand_types(left, right)?;
                self.open(output)?;
                let parent = Parent::Operator("IS DISTINCT FROM");
                self.wrapped(left, Some(left_type), parent, true, output)?;
                output.push(" IS DISTINCT FROM ")?;
                self.wrapped(right, Some(right_type), parent, false, output)?;
                self.close(output)?;
                if negated {
                    self.close(output)?;
                }
                Ok(())
            }
            Expr::Case {
                origin: crate::sql::ast::CaseOrigin::BooleanTest { .. },
                ..
            } => {
                let Some((operand, value, negated)) = expression.boolean_test_operand() else {
                    return output.unsupported();
                };
                self.open(output)?;
                self.wrapped(
                    operand,
                    Some(Some(oid::BOOL)),
                    Parent::NullTest,
                    true,
                    output,
                )?;
                output.push(match (value, negated) {
                    (true, false) => " IS TRUE",
                    (true, true) => " IS NOT TRUE",
                    (false, false) => " IS FALSE",
                    (false, true) => " IS NOT FALSE",
                })?;
                self.close(output)
            }
            Expr::Case {
                operand: None,
                whens,
                otherwise,
                origin: crate::sql::ast::CaseOrigin::Written,
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
        | Expr::WholeRow(_)
        | Expr::Field { .. }
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
        Expr::IsNull { .. }
        | Expr::Case {
            origin:
                crate::sql::ast::CaseOrigin::BooleanTest { .. }
                | crate::sql::ast::CaseOrigin::DistinctFrom { negated: false },
            ..
        } => matches!(parent, Parent::Connective(_) | Parent::Not),
        Expr::Case {
            origin: crate::sql::ast::CaseOrigin::DistinctFrom { negated: true },
            ..
        } => connective(None),
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

/// `looks_like_function`: a call written in function syntax, which an index
/// definition prints without surrounding parentheses.
pub(crate) fn looks_like_function(expression: &Expr<'_>) -> bool {
    match expression {
        Expr::Call {
            kind: CallKind::Routine,
            ..
        } => true,
        Expr::Call {
            kind: CallKind::SqlSyntax,
            name,
            ..
        } => is_conditional(name) || sql_value_function(name).is_some(),
        _ => false,
    }
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
/// are reported as unsupported. A stored view's relations, routines and types
/// are bound to the identities they captured at CREATE and printed under
/// their current names.
#[expect(
    clippy::too_many_arguments,
    reason = "a view's query, captured names, columns and layout are explicit"
)]
pub(crate) fn view_definition<'a>(
    storage: &Storage,
    txid: u32,
    select: &'a crate::sql::ast::Select<'a>,
    dependencies: Option<crate::storage::StoredQueryDependencyView<'_>>,
    column_names: &[&str],
    layout: ViewLayout,
    origin: crate::storage::ViewQueryOrigin,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let pretty = layout.pretty;
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
                || join.kind.is_cross()
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
    // Binding substitutes captured names only; a subquery would expand the
    // views it reads, which this layout cannot print.
    let bind = |expression: &'a Expr<'a>| -> Result<Option<&'a Expr<'a>>, SqlError> {
        let Some(dependencies) = dependencies else {
            return Ok(Some(expression));
        };
        if expression.contains_subquery() {
            return Ok(None);
        }
        crate::sql::query::bind_stored_expression(expression, storage, txid, dependencies, arena)
            .map(Some)
    };
    let sources = core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table));
    let mut reads: [Option<crate::storage::TableDefinitionRead<'_>>; MAX_JOINED] =
        core::array::from_fn(|_| None);
    for (read, source) in reads.iter_mut().zip(sources.clone()) {
        let captured = dependencies
            .and_then(|dependencies| {
                dependencies.entries().iter().find(|dependency| {
                    matches!(
                        dependency.class,
                        crate::storage::DependencyClass::Table
                            | crate::storage::DependencyClass::View
                    ) && dependency.referenced_schema.as_str() == source.schema.unwrap_or("")
                        && dependency.referenced_name.as_str() == source.table
                })
            })
            .map(|dependency| storage.resolve_stored_query_relation(dependency, txid))
            .transpose()?;
        let resolved = match captured {
            Some(resolved) => Some(resolved),
            None => storage.resolve_relation(source.schema, source.table, txid),
        };
        let Some(crate::storage::ResolvedRelation::Table(slot)) = resolved else {
            return Ok(None);
        };
        *read = Some(storage.table_def(slot, txid));
    }
    let table = reads[0].as_deref().expect("resolved base relation");
    let mut joined = [Joined::new("", table); MAX_JOINED];
    let count = from.joins.len() + 1;
    for ((target, read), source) in joined.iter_mut().zip(&reads).zip(sources) {
        let read = read.as_deref().expect("resolved source");
        *target = Joined {
            name: source.alias.unwrap_or(source.table),
            printed: source.alias.unwrap_or(read.name.as_str()),
            table: read,
            unqualified: true,
        };
    }
    let context = Context {
        storage,
        txid,
        table: Some(table),
        parameters: &[],
        joined: &joined[..count],
        pretty,
        qualify: origin == crate::storage::ViewQueryOrigin::ReturnRule,
        bare: None,
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
            let Some(expression) = bind(expression)? else {
                return Ok(None);
            };
            expression
        };
        if let Expr::Case {
            operand: None,
            whens,
            otherwise,
            origin: crate::sql::ast::CaseOrigin::Written,
        } = expression
        {
            // The indented layout starts a searched CASE on its own line.
            context.case_block(whens, *otherwise, 8, &mut target)?;
        } else {
            let natural = context.type_of(expression)?;
            context.write(expression, natural, &mut target)?;
            // `get_variable` casts a top-level whole-row variable to its row
            // type so that it is not read back as a `*` expansion.
            if context.whole_row(expression).is_some() {
                match natural {
                    Some(row_type) => {
                        match super::user_type_name_fixed(context.storage, context.txid, row_type)?
                        {
                            Some(name) => {
                                target.push("::")?;
                                target.push(name.as_str())?;
                            }
                            None => target.unsupported()?,
                        }
                    }
                    None => target.unsupported()?,
                }
            }
        }
        let bare_column = context.whole_row(expression).is_none()
            && matches!(expression, Expr::Column { name: column, .. } if column == name);
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
    for (join, joined_table) in from
        .joins
        .iter()
        .zip(joined[1..].iter().map(|joined| joined.table))
    {
        output.push(match join.kind {
            crate::sql::ast::JoinKind::Left => "\n     LEFT JOIN ",
            crate::sql::ast::JoinKind::Right => "\n     RIGHT JOIN ",
            crate::sql::ast::JoinKind::Full => "\n     FULL JOIN ",
            _ => "\n     JOIN ",
        })?;
        output.push(relation(joined_table, &join.table).as_str())?;
        output.push(if pretty { " ON " } else { " ON (" })?;
        let Some(condition) = bind(join.on.expect("checked join condition"))? else {
            return Ok(None);
        };
        context.write(condition, Some(oid::BOOL), &mut output)?;
        if !pretty {
            output.push("))")?;
        }
    }
    if let Some(condition) = select.where_clause {
        let Some(condition) = bind(condition)? else {
            return Ok(None);
        };
        output.push("\n  WHERE ")?;
        context.write(condition, Some(oid::BOOL), &mut output)?;
    }
    if !select.group_by.is_empty() {
        output.push("\n  GROUP BY ")?;
        for (index, expression) in select.group_by.iter().enumerate() {
            if index != 0 {
                output.push(", ")?;
            }
            let Some(expression) = bind(expression)? else {
                return Ok(None);
            };
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
            let Some(expression) = bind(order.expression)? else {
                return Ok(None);
            };
            let natural = context.type_of(expression)?;
            context.write(expression, natural, &mut output)?;
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
            let Some(value) = bind(value)? else {
                return Ok(None);
            };
            output.push("\n")?;
            output.push(keyword)?;
            let natural = context.type_of(value)?;
            context.write(value, natural, &mut output)?;
        }
    }
    output.push(";")?;
    Ok(output.supported.then(|| output.text.as_str()))
}

/// One action of a rewrite rule as `make_ruledef` prints it with
/// `PRETTYFLAG_INDENT` (both of `pg_get_ruledef`'s modes): a leading space
/// or line break, then the statement without its terminating semicolon.
/// `rule_table` is the relation the rule is defined on, whose `OLD` and
/// `NEW` rows the action reads; every column is qualified, as the action's
/// range table always holds them. `None` when the action's shape is outside
/// this module's typed coverage.
pub(crate) fn rule_action_definition<'a>(
    storage: &Storage,
    txid: u32,
    rule_table: &TableDef,
    action: &'a str,
    pretty: bool,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    use crate::sql::ast::Stmt;
    let Ok(statement) = crate::sql::parser::parse_stored_statement(action, arena) else {
        return Ok(None);
    };
    let mut output = Output {
        arena,
        text: ArenaString::new(arena),
        supported: true,
    };
    if let Stmt::Notify { channel, payload } = statement {
        // `get_utility_query_def` starts the command on its own line.
        output.push("\n NOTIFY ")?;
        push_identifier(&mut output, channel)?;
        if let Some(payload) = payload {
            output.push(", '")?;
            for character in payload.chars() {
                if character == '\'' {
                    output.push("''")?;
                } else {
                    let mut encoded = [0u8; 4];
                    output.push(character.encode_utf8(&mut encoded))?;
                }
            }
            output.push("'")?;
        }
        return Ok(Some(output.text.as_str()));
    }
    // The target (if any), then the FROM, USING or SELECT items, each with
    // the alias it is exposed under.
    let mut sources: [Option<(&str, &str, Option<&str>)>; MAX_JOINED] = [None; MAX_JOINED];
    let mut source_count = 0;
    let mut push_source = |schema: Option<&'a str>, table: &'a str, alias: Option<&'a str>| {
        if source_count == MAX_JOINED {
            return false;
        }
        sources[source_count] = Some((schema.unwrap_or(""), table, alias));
        source_count += 1;
        true
    };
    // Plain tables, optionally aliased, listed with commas.
    let add_from = |from: Option<&'a crate::sql::ast::FromClause<'a>>,
                    push: &mut dyn FnMut(Option<&'a str>, &'a str, Option<&'a str>) -> bool|
     -> bool {
        let Some(from) = from else {
            return true;
        };
        core::iter::once(&from.base)
            .chain(from.joins.iter().map(|join| &join.table))
            .all(|item| plain_table(item).is_some() && item.sample.is_none() && !item.lateral)
            && from
                .joins
                .iter()
                .all(|join| join.kind == crate::sql::ast::JoinKind::Comma)
            && core::iter::once(&from.base)
                .chain(from.joins.iter().map(|join| &join.table))
                .all(|item| push(item.schema, item.table, item.alias))
    };
    let (target, returning) = match statement {
        Stmt::Insert(insert) => {
            if !push_source(insert.table.schema, insert.table.name, insert.alias) {
                return Ok(None);
            }
            if let Some(select) = insert.select
                && !add_from(select.from.as_ref(), &mut push_source)
            {
                return Ok(None);
            }
            (true, insert.returning)
        }
        Stmt::Update(update) => {
            if !push_source(update.table.schema, update.table.name, update.alias)
                || !add_from(update.from, &mut push_source)
            {
                return Ok(None);
            }
            (true, update.returning)
        }
        Stmt::Delete(delete) => {
            if !push_source(delete.table.schema, delete.table.name, delete.alias)
                || !add_from(delete.using, &mut push_source)
            {
                return Ok(None);
            }
            (true, delete.returning)
        }
        Stmt::Select(select) => {
            if !add_from(select.from.as_ref(), &mut push_source) {
                return Ok(None);
            }
            (false, crate::sql::ast::Returning::NONE)
        }
        _ => return Ok(None),
    };
    let mut reads: [Option<crate::storage::TableDefinitionRead<'_>>; MAX_JOINED] =
        core::array::from_fn(|_| None);
    for (read, source) in reads.iter_mut().zip(&sources[..source_count]) {
        let (schema, table, _) = source.expect("counted source");
        let schema = (!schema.is_empty()).then_some(schema);
        let Some(crate::storage::ResolvedRelation::Table(slot)) =
            storage.resolve_relation(schema, table, txid)
        else {
            return Ok(None);
        };
        *read = Some(storage.table_def(slot, txid));
    }
    // OLD, NEW and, for ON CONFLICT DO UPDATE, EXCLUDED, then the sources.
    let mut joined = [Joined::rule_row("old", rule_table); MAX_JOINED + 3];
    joined[1] = Joined::rule_row("new", rule_table);
    for (index, read) in reads[..source_count].iter().enumerate() {
        let definition = read.as_deref().expect("resolved source");
        let alias = sources[index].expect("counted source").2;
        joined[index + 2] = Joined::new(alias.unwrap_or(definition.name.as_str()), definition);
    }
    let excluded = match statement {
        Stmt::Insert(insert) if insert.on_conflict.is_some() => {
            joined[source_count + 2] =
                Joined::rule_row("excluded", reads[0].as_deref().expect("resolved target"));
            1
        }
        _ => 0,
    };
    let joined = &joined[..source_count + 2 + excluded];
    let sources = &joined[2..source_count + 2];
    let target_table = target.then(|| joined[2].table);
    let context = Context {
        storage,
        txid,
        table: target_table,
        parameters: &[],
        joined,
        pretty,
        qualify: true,
        bare: None,
    };
    // A source's relation, then its alias after `separator`.
    let relation = |source: &Joined<'_>, separator: &str| {
        let table = source.table;
        let mut relation = crate::util::StackStr::<400>::new();
        if !storage.schema_is_on_path(table.schema, txid) {
            write_identifier(&mut relation, table.schema.as_str());
            let _ = relation.write_char('.');
        }
        write_identifier(&mut relation, table.name.as_str());
        if source.name != table.name.as_str() {
            let _ = relation.write_str(separator);
            write_identifier(&mut relation, source.name);
        }
        relation
    };
    let column_type = |table: &TableDef, name: &str| -> Result<Option<i32>, SqlError> {
        match table
            .columns()
            .iter()
            .find(|column| column.name.as_str() == name)
        {
            Some(column) => super::catalog_column_type_oid(storage, column, txid).map(Some),
            None => Ok(None),
        }
    };
    let condition = |output: &mut Output<'_>, condition: Option<&Expr<'_>>| {
        if let Some(condition) = condition {
            output.push("\n  WHERE ")?;
            context.write(condition, Some(oid::BOOL), output)?;
        }
        Ok::<(), SqlError>(())
    };
    // `get_from_clause` with the target excluded: each later item wraps.
    let extra = |output: &mut Output<'_>, keyword: &str| {
        for (index, source) in sources[1..].iter().enumerate() {
            output.push(if index == 0 { keyword } else { ",\n    " })?;
            output.push(relation(source, " ").as_str())?;
        }
        Ok::<(), SqlError>(())
    };
    let assigned_value = |output: &mut Output<'_>, value: &Expr<'_>, type_oid: i32| {
        if matches!(value, Expr::DefaultMarker) {
            output.push("DEFAULT")
        } else {
            context.assigned(value, type_oid, output)
        }
    };
    match statement {
        Stmt::Insert(insert) => {
            let target = joined[2].table;
            let width = match (insert.rows.first(), insert.select) {
                (Some(row), None) => row.len(),
                (None, Some(select)) => select.items.len(),
                _ => return Ok(None),
            };
            if width > target.columns().len()
                || (!insert.columns.is_empty() && insert.columns.len() != width)
                || insert.overriding != crate::sql::ast::Overriding::None
            {
                return Ok(None);
            }
            let name_at = |index: usize| match insert.columns.get(index) {
                Some(name) => *name,
                None => target.columns()[index].name.as_str(),
            };
            output.push(" INSERT INTO ")?;
            output.push(relation(&joined[2], " AS ").as_str())?;
            output.push(" (")?;
            for index in 0..width {
                if index != 0 {
                    output.push(", ")?;
                }
                push_identifier(&mut output, name_at(index))?;
            }
            output.push(") ")?;
            match (insert.rows, insert.select) {
                ([row], None) => {
                    // `appendContextKeyword` drops the trailing space.
                    output.push_trimmed("\n  VALUES (")?;
                    for (index, value) in row.iter().enumerate() {
                        let Some(type_oid) = column_type(target, name_at(index))? else {
                            return Ok(None);
                        };
                        if index != 0 {
                            output.push(", ")?;
                        }
                        assigned_value(&mut output, value, type_oid)?;
                    }
                    output.push(")")?;
                }
                (rows, None) => {
                    // `get_values_def`: the VALUES list's own column types.
                    output.push("VALUES ")?;
                    for (row_index, row) in rows.iter().enumerate() {
                        if row.len() != width {
                            return Ok(None);
                        }
                        output.push(if row_index == 0 { "(" } else { ", (" })?;
                        for (index, value) in row.iter().enumerate() {
                            if index != 0 {
                                output.push(",")?;
                            }
                            let column = rows
                                .iter()
                                .find_map(|other| {
                                    (!matches!(other[index], Expr::Str(_) | Expr::Null))
                                        .then(|| context.type_of(other[index]))
                                })
                                .transpose()?
                                .flatten();
                            let Some(column) = column.or(column_type(target, name_at(index))?)
                            else {
                                return Ok(None);
                            };
                            if matches!(value, Expr::DefaultMarker) {
                                output.push("DEFAULT")?;
                            } else {
                                context.write(value, Some(column), &mut output)?;
                            }
                        }
                        output.push(")")?;
                    }
                }
                (_, Some(select)) => {
                    // The source query reads its own FROM items and the
                    // rule's rows, not the target, two levels past the action.
                    let mut inner = [joined[0]; MAX_JOINED + 2];
                    inner[1] = joined[1];
                    inner[2..source_count + 1].copy_from_slice(&sources[1..]);
                    let inner_context = Context {
                        table: None,
                        joined: &inner[..source_count + 1],
                        ..context
                    };
                    if !write_rule_select(&inner_context, select, 8, &mut output)? {
                        return Ok(None);
                    }
                }
            }
            if let Some(conflict) = insert.on_conflict {
                // `get_insert_query_def`: the arbiter columns unqualified.
                output.push(" ON CONFLICT")?;
                if !conflict.target.is_empty() {
                    output.push("(")?;
                    for (index, element) in conflict.target.iter().enumerate() {
                        let Some(column) = element.column else {
                            return Ok(None);
                        };
                        if index != 0 {
                            output.push(", ")?;
                        }
                        push_identifier(&mut output, column)?;
                    }
                    output.push(")")?;
                } else if let Some(constraint) = conflict.constraint {
                    output.push(" ON CONSTRAINT ")?;
                    push_identifier(&mut output, constraint)?;
                }
                match conflict.update {
                    None => output.push(" DO NOTHING")?,
                    Some(assignments) => {
                        output.push(" DO UPDATE SET ")?;
                        for (index, (name, value)) in assignments.iter().enumerate() {
                            let Some(type_oid) = column_type(target, name)? else {
                                return Ok(None);
                            };
                            if index != 0 {
                                output.push(", ")?;
                            }
                            push_identifier(&mut output, name)?;
                            output.push(" = ")?;
                            assigned_value(&mut output, value, type_oid)?;
                        }
                        condition(&mut output, conflict.update_where)?;
                    }
                }
            }
        }
        Stmt::Update(update) => {
            let target = joined[2].table;
            output.push(" UPDATE ")?;
            output.push(relation(&joined[2], " ").as_str())?;
            output.push(" SET ")?;
            for (index, (name, value)) in update.assignments.iter().enumerate() {
                let Some(type_oid) = column_type(target, name)? else {
                    return Ok(None);
                };
                if index != 0 {
                    output.push(", ")?;
                }
                push_identifier(&mut output, name)?;
                output.push(" = ")?;
                assigned_value(&mut output, value, type_oid)?;
            }
            extra(&mut output, "\n   FROM ")?;
            condition(&mut output, update.where_clause)?;
        }
        Stmt::Delete(delete) => {
            output.push(" DELETE FROM ")?;
            output.push(relation(&joined[2], " ").as_str())?;
            extra(&mut output, "\n   USING ")?;
            condition(&mut output, delete.where_clause)?;
        }
        Stmt::Select(select) => {
            if !write_rule_select(&context, select, 0, &mut output)? {
                return Ok(None);
            }
        }
        _ => return Ok(None),
    }
    if !returning.is_empty() {
        output.push("\n  RETURNING")?;
        if !write_targets(&context, returning.items, 0, &mut output)? {
            return Ok(None);
        }
    }
    Ok(output.supported.then(|| output.text.as_str()))
}

fn push_identifier(output: &mut Output<'_>, name: &str) -> Result<(), SqlError> {
    let mut identifier = crate::util::StackStr::<130>::new();
    write_identifier(&mut identifier, name);
    output.push(identifier.as_str())
}

/// `get_select_query_def` for a rule's query over plain tables listed
/// with commas, starting `indent` columns in (the enclosing statement's
/// indentation). Each clause starts its own line as `appendContextKeyword`
/// places it. False when the query's shape is outside this module's
/// coverage.
fn write_rule_select<'s>(
    context: &Context<'_>,
    select: &'s crate::sql::ast::Select<'s>,
    indent: usize,
    output: &mut Output<'_>,
) -> Result<bool, SqlError> {
    if !select.distinct_on.is_empty()
        || !select.windows.is_empty()
        || !select.grouping_sets.is_empty()
        || select.with_ties
        || !select.with.is_empty()
        || select.set_body.is_some()
        || !select.locking.is_empty()
    {
        return Ok(false);
    }
    let clause = |output: &mut Output<'_>, plus: usize, keyword: &str| {
        output.push("\n")?;
        output.push_spaces(indent + plus)?;
        output.push(keyword)
    };
    output.push(if select.distinct {
        " SELECT DISTINCT"
    } else {
        " SELECT"
    })?;
    if !write_targets(context, select.items, indent, output)? {
        return Ok(false);
    }
    if let Some(from) = &select.from {
        for (index, item) in core::iter::once(&from.base)
            .chain(from.joins.iter().map(|join| &join.table))
            .enumerate()
        {
            let exposed = item.alias.unwrap_or(item.table);
            let Some(table) = context
                .joined
                .iter()
                .find(|joined| joined.unqualified && joined.name == exposed)
            else {
                return Ok(false);
            };
            if index == 0 {
                clause(output, 2, " FROM ")?;
            } else {
                output.push(",\n")?;
                output.push_spaces(indent + 4)?;
            }
            if !context
                .storage
                .schema_is_on_path(table.table.schema, context.txid)
            {
                push_identifier(output, table.table.schema.as_str())?;
                output.push(".")?;
            }
            push_identifier(output, table.table.name.as_str())?;
            if table.name != table.table.name.as_str() {
                output.push(" ")?;
                push_identifier(output, table.name)?;
            }
        }
    }
    if let Some(condition) = select.where_clause {
        clause(output, 1, " WHERE ")?;
        context.write(condition, Some(oid::BOOL), output)?;
    }
    // `ORDER BY 1` and `GROUP BY 1` name a target by position.
    let resolve = |expression: &'s Expr<'s>| -> Option<&'s Expr<'s>> {
        match expression {
            Expr::Int(position) if *position >= 1 => {
                match select.items.get((*position - 1) as usize) {
                    Some(crate::sql::ast::SelectItem::Expr { expression, .. }) => Some(expression),
                    _ => None,
                }
            }
            expression => Some(expression),
        }
    };
    if !select.group_by.is_empty() {
        clause(output, 1, " GROUP BY ")?;
        for (index, expression) in select.group_by.iter().enumerate() {
            if index != 0 {
                output.push(", ")?;
            }
            let Some(expression) = resolve(expression) else {
                return Ok(false);
            };
            let natural = context.type_of(expression)?;
            context.write(expression, natural, output)?;
        }
    }
    if let Some(having) = select.having {
        clause(output, 0, " HAVING ")?;
        context.write(having, Some(oid::BOOL), output)?;
    }
    if !select.order_by.is_empty() {
        clause(output, 1, " ORDER BY ")?;
        for (index, order) in select.order_by.iter().enumerate() {
            if index != 0 {
                output.push(", ")?;
            }
            let Some(expression) = resolve(order.expression) else {
                return Ok(false);
            };
            let natural = context.type_of(expression)?;
            context.write(expression, natural, output)?;
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
            clause(output, 0, keyword)?;
            let natural = context.type_of(value)?;
            context.write(value, natural, output)?;
        }
    }
    Ok(true)
}

/// `get_target_list` with the default wrap column: each target after the
/// first starts its own line `indent + 4` columns in. A plain column keeps
/// its name; another expression needs its written name. `*` expands to the
/// statement's target table.
fn write_targets(
    context: &Context<'_>,
    items: &[crate::sql::ast::SelectItem<'_>],
    indent: usize,
    output: &mut Output<'_>,
) -> Result<bool, SqlError> {
    let mut index = 0;
    let mut target = |output: &mut Output<'_>, expression: &Expr<'_>, alias: Option<&str>| {
        if index == 0 {
            output.push(" ")?;
        } else {
            output.push(",\n")?;
            output.push_spaces(indent + 4)?;
        }
        index += 1;
        let natural = context.type_of(expression)?;
        context.write(expression, natural, output)?;
        match (expression, alias) {
            (Expr::Column { name, .. }, Some(alias)) if *name == alias => {}
            (_, Some(alias)) => {
                output.push(" AS ")?;
                push_identifier(output, alias)?;
            }
            (Expr::Column { .. }, None) => {}
            (_, None) => {
                output.push(" AS ")?;
                push_identifier(output, crate::sql::exec::derived_name(expression))?;
            }
        }
        Ok::<(), SqlError>(())
    };
    for item in items {
        match item {
            crate::sql::ast::SelectItem::Expr { expression, alias } => {
                target(output, expression, *alias)?;
            }
            crate::sql::ast::SelectItem::Wildcard(_) => {
                let Some(table) = context.table else {
                    return Ok(false);
                };
                for column in table.columns() {
                    let column = Expr::Column {
                        qualifier: Some(table.name.as_str()),
                        name: column.name.as_str(),
                    };
                    target(output, &column, None)?;
                }
            }
            _ => return Ok(false),
        }
    }
    Ok(true)
}
